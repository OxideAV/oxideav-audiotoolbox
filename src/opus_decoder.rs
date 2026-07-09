//! Opus decoder backed by macOS AudioConverter (`kAudioFormatOpus`).
//!
//! Input:  one raw Opus packet per `Packet`. Each packet starts with
//!         the RFC 6716 §3.1 TOC byte (config + stereo bit + code)
//!         followed by the per-code framing and the encoded Opus
//!         frames themselves. AT consumes the TOC byte directly for
//!         per-packet mode + bandwidth + frame-size decoding; the
//!         encoder-vended OpusHead (RFC 7845 §5.1) is forwarded as
//!         the decompression magic cookie.
//! Output: interleaved S16 PCM `AudioFrame`s at 48 kHz (RFC 7845
//!         §5.1 recommended player rate). The number of PCM frames
//!         AT vends per `FillComplexBuffer` call equals the TOC-
//!         derived per-packet sample count (one of 120 / 240 / 480 /
//!         960 / 1920 / 2880 at 48 kHz).
//!
//! The decoder uses the same persistent input-queue + one-packet-of-
//! slack lookahead pattern as the MP3 / AMR-NB / AMR-WB / FLAC
//! bridges so AT never sees "0 packets" mid-stream (which would put
//! it into a permanent end-of-stream state).
//!
//! ## Magic-cookie resolution
//!
//! 1. If `CodecParameters::extradata` parses as a valid OpusHead
//!    (RFC 7845 §5.1, at least 19 bytes starting with `OpusHead`),
//!    it is forwarded verbatim to AT via
//!    `AudioConverterSetProperty(kAudioConverterDecompressionMagicCookie,
//!    …)`.
//! 2. Otherwise the bridge synthesises a minimal family-0 OpusHead
//!    from the explicit `sample_rate` / `channels` parameters via
//!    [`crate::opus::OpusHead::family_0`]. This is the standalone-
//!    test path: the caller knows the geometry it is feeding.

use std::ffi::c_void;

use oxideav_core::Decoder;
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, Result, TimeBase};

use crate::opus::{OpusHead, DEFAULT_FRAMES_PER_PACKET_48K, HEAD_LEN_FAMILY_0};
use crate::status::status_error;
use crate::sys::{
    self, AudioBuffer, AudioBufferList1, AudioConverterRef, AudioStreamBasicDescription,
    AudioStreamPacketDescription, K_AUDIO_CONVERTER_DECOMPRESSION_MAGIC_COOKIE, NO_ERR,
};

/// State shared with the AudioConverter input callback.
struct InputContext {
    queue: Vec<Vec<u8>>,
    /// Packets handed off to AT during the current `FillComplexBuffer`
    /// call — retained so the converter can still reference their
    /// bytes after the callback returns.
    handed_off: Vec<Vec<u8>>,
    /// Per-call packet descriptor (rewritten on each invocation).
    packet_desc: AudioStreamPacketDescription,
}

/// AudioConverter-backed Opus decoder.
pub struct OpusAtDecoder {
    codec_id: CodecId,
    head: OpusHead,
    sample_rate: u32,
    converter: AudioConverterRef,
    /// Worst-case output buffer size (PCM frames × channels × 2 bytes
    /// for S16). Sized for the maximum Opus frame at the active
    /// output rate.
    max_pcm_bytes: usize,
    pending: Vec<Frame>,
    input_queue: Vec<Vec<u8>>,
    pts: i64,
    #[allow(dead_code)]
    time_base: TimeBase,
    eof: bool,
}

// SAFETY: AudioConverterRef is used single-threaded inside the Decoder
// impl. We never move the handle across threads during one call.
unsafe impl Send for OpusAtDecoder {}

impl OpusAtDecoder {
    fn new(params: &CodecParameters) -> Result<Self> {
        let fw = sys::framework()
            .map_err(|e| Error::unsupported(format!("AudioToolbox unavailable: {e}")))?;

        let head = resolve_head(params)?;
        if head.mapping_family != 0 {
            return Err(Error::unsupported(format!(
                "Opus decoder: mapping family {} is not exposed via the AT bridge \
                 (multi-channel routing is the container layer's responsibility)",
                head.mapping_family
            )));
        }
        if !(1..=2).contains(&head.channels) {
            return Err(Error::unsupported(format!(
                "Opus decoder (mapping family 0): channel count {} out of range (1..=2)",
                head.channels
            )));
        }

        // RFC 6716 §2.1.1: decoder output rate is one of
        // 8 / 12 / 16 / 24 / 48 kHz. RFC 7845 §5.1 recommends 48 kHz
        // for playback. The bridge defaults to 48 kHz; an explicit
        // sample_rate request that lands on a different valid rate
        // is honoured so callers can target NB / MB / WB / SWB
        // pipelines that prefer lower output rates.
        let sample_rate = params.sample_rate.unwrap_or(48_000);
        validate_output_rate(sample_rate)?;

        let frames_per_packet = match sample_rate {
            8_000 => 160,
            12_000 => 240,
            16_000 => 320,
            24_000 => 480,
            48_000 => DEFAULT_FRAMES_PER_PACKET_48K,
            _ => unreachable!("validate_output_rate ensured this"),
        };

        let in_asbd = AudioStreamBasicDescription::opus(
            sample_rate as f64,
            head.channels as u32,
            frames_per_packet,
        );
        let out_asbd =
            AudioStreamBasicDescription::pcm_s16(sample_rate as f64, head.channels as u32);

        let mut converter: AudioConverterRef = std::ptr::null_mut();
        let status = unsafe { sys::audio_converter_new(fw, &in_asbd, &out_asbd, &mut converter) };
        if status != NO_ERR {
            return Err(status_error("AudioConverterNew (Opus dec)", status));
        }

        let cookie = head.to_bytes();
        let status = unsafe {
            sys::audio_converter_set_property(
                fw,
                converter,
                K_AUDIO_CONVERTER_DECOMPRESSION_MAGIC_COOKIE,
                cookie.len() as u32,
                cookie.as_ptr() as *const c_void,
            )
        };
        if status != NO_ERR {
            unsafe {
                let _ = sys::audio_converter_dispose(fw, converter);
            }
            return Err(status_error(
                "AudioConverterSetProperty(DecompressionMagicCookie / Opus)",
                status,
            ));
        }

        // Worst-case PCM block: 60 ms frame at the configured rate.
        // At 48 kHz that's 2880 frames × channels × 2 bytes.
        let max_frames = (sample_rate as usize * 60) / 1000;
        let max_pcm_bytes = max_frames * head.channels as usize * 2;

        let tb = TimeBase::new(1, sample_rate as i64);
        Ok(Self {
            codec_id: params.codec_id.clone(),
            head,
            sample_rate,
            converter,
            max_pcm_bytes,
            pending: Vec::new(),
            input_queue: Vec::new(),
            pts: 0,
            time_base: tb,
            eof: false,
        })
    }

    /// Queue a packet and drain whatever PCM AT will emit so far,
    /// leaving one packet of slack in the queue.
    fn enqueue_packet(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Err(Error::invalid("Opus: empty packet (no TOC byte)"));
        }
        // RFC 6716 §3.1: TOC byte is always present; everything else is
        // intentionally left for AT (we do not parse `config` here so
        // that the bridge stays agnostic to SILK vs Hybrid vs CELT).
        self.input_queue.push(data.to_vec());
        self.drain_pcm()
    }

    /// Drain PCM frames from AT.
    ///
    /// AT's Opus decoder may consume multiple compressed packets per
    /// `FillComplexBuffer` invocation (the callback fires repeatedly
    /// inside one FCB call until AT has enough Opus bytes or the
    /// queue runs out). Each FCB call emits up to
    /// `max_pcm_bytes / bytes_per_pcm_frame` decoded PCM frames.
    /// Therefore we keep pulling PCM as long as either:
    ///
    /// * compressed input is still queued (more to feed), OR
    /// * the previous FCB call produced output (AT may have queued
    ///   more PCM internally).
    ///
    /// We do NOT rely on the one-packet-of-slack invariant that the
    /// FLAC / iLBC / AMR-NB decoders use: AT's Opus slot does not
    /// EOF-poison the converter when the input callback signals "no
    /// more packets right now". An empty-queue callback simply
    /// causes FCB to return whatever PCM AT has already buffered and
    /// then exit; subsequent FCB calls with new input packets
    /// continue producing PCM normally.
    /// Drain PCM frames from AT.
    ///
    /// Strict slack discipline: never invoke FCB unless the queue
    /// has at least 2 packets so the input callback can satisfy the
    /// pull without an empty signal. AT's Opus slot **locks the
    /// converter** the moment the input callback returns 0 packets
    /// — subsequent FCB calls fail to fire the callback at all.
    /// Empirically a single empty-queue callback is sufficient to
    /// trigger this lockout, so we have to be conservative about
    /// when we call FCB.
    fn drain_pcm(&mut self) -> Result<()> {
        while self.input_queue.len() >= 2 {
            if !self.pull_one_pcm_frame()? {
                break;
            }
        }
        Ok(())
    }

    /// Single `FillComplexBuffer` call asking for one frame's worth of
    /// PCM. Returns `true` if a frame was produced.
    fn pull_one_pcm_frame(&mut self) -> Result<bool> {
        let fw = sys::framework()
            .map_err(|e| Error::unsupported(format!("AudioToolbox unavailable: {e}")))?;

        let channels = self.head.channels as usize;
        let mut pcm_buf = vec![0u8; self.max_pcm_bytes];

        let mut ctx = InputContext {
            queue: std::mem::take(&mut self.input_queue),
            handed_off: Vec::new(),
            packet_desc: AudioStreamPacketDescription::default(),
        };

        // Ask AT for one Opus packet's worth of PCM frames at most.
        // For a `pcm_s16` output ASBD (frames_per_packet = 1) this is
        // a PCM-frame upper bound; AT decides the actual returned
        // count from the input Opus packet's TOC byte (RFC 6716
        // §3.1). Asking for "one packet's worth" (default 20 ms =
        // 960 frames at 48 kHz) bounds AT's hunger so it consumes
        // one input packet per FCB call instead of draining the
        // whole queue in one shot — the latter triggers AT's Opus-
        // slot end-of-stream lockout the moment the callback signals
        // an empty queue.
        let mut output_packet_count: u32 =
            ((self.sample_rate as f64 * crate::opus::DEFAULT_FRAME_DURATION_MS) / 1000.0) as u32;
        let mut abl = AudioBufferList1 {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: self.head.channels as u32,
                data_byte_size: self.max_pcm_bytes as u32,
                data: pcm_buf.as_mut_ptr(),
            }],
        };

        let status = unsafe {
            sys::audio_converter_fill_complex_buffer(
                fw,
                self.converter,
                opus_input_callback,
                &mut ctx as *mut InputContext as *mut c_void,
                &mut output_packet_count,
                &mut abl,
                std::ptr::null_mut(),
            )
        };

        // Restore whatever's left of the queue.
        self.input_queue = std::mem::take(&mut ctx.queue);

        if status != NO_ERR && status != 1 {
            return Err(status_error(
                "AudioConverterFillComplexBuffer (Opus dec)",
                status,
            ));
        }

        let actual_bytes = abl.buffers[0].data_byte_size as usize;
        if actual_bytes == 0 || output_packet_count == 0 {
            return Ok(false);
        }
        let actual_samples = actual_bytes / (channels * 2);
        let frame = AudioFrame {
            samples: actual_samples as u32,
            pts: Some(self.pts),
            data: vec![pcm_buf[..actual_bytes].to_vec()],
        };
        self.pts += actual_samples as i64;
        self.pending.push(Frame::Audio(frame));
        Ok(true)
    }

    /// Drain every PCM frame still cached in AT — called from `flush`.
    fn drain_pcm_all(&mut self) -> Result<()> {
        for _ in 0..256 {
            if self.input_queue.is_empty() {
                break;
            }
            if !self.pull_one_pcm_frame()? {
                break;
            }
        }
        // Final pull to flush any internal look-ahead PCM.
        for _ in 0..2 {
            if !self.pull_one_pcm_frame()? {
                break;
            }
        }
        Ok(())
    }

    /// Observed decoder output sample rate (one of 8 / 12 / 16 / 24 /
    /// 48 kHz). Exposed for the standalone-test path.
    pub fn output_sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Latched OpusHead — useful for diagnostics or for forwarding to
    /// a downstream encoder that needs to inherit the channel count
    /// and pre-skip.
    pub fn opus_head(&self) -> &OpusHead {
        &self.head
    }
}

impl Drop for OpusAtDecoder {
    fn drop(&mut self) {
        if !self.converter.is_null() {
            if let Ok(fw) = sys::framework() {
                unsafe {
                    let _ = sys::audio_converter_dispose(fw, self.converter);
                }
            }
            self.converter = std::ptr::null_mut();
        }
    }
}

impl Decoder for OpusAtDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if self.eof {
            return Err(Error::Eof);
        }
        self.enqueue_packet(&packet.data)
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if !self.pending.is_empty() {
            return Ok(self.pending.remove(0));
        }
        if self.eof {
            return Err(Error::Eof);
        }
        Err(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        self.drain_pcm_all()?;
        self.eof = true;
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.pending.clear();
        self.input_queue.clear();
        self.pts = 0;
        self.eof = false;
        if !self.converter.is_null() {
            if let Ok(fw) = sys::framework() {
                unsafe {
                    sys::audio_converter_reset(fw, self.converter);
                }
            }
        }
        Ok(())
    }
}

/// Decode the OpusHead from `params.extradata`, falling back to a
/// synthesised family-0 header built from `sample_rate` and
/// `channels` when no cookie is present.
fn resolve_head(params: &CodecParameters) -> Result<OpusHead> {
    if params.extradata.len() >= HEAD_LEN_FAMILY_0 {
        return OpusHead::from_bytes(&params.extradata).map_err(|e| Error::invalid(e.0));
    }
    let sr = params.sample_rate.unwrap_or(48_000);
    let ch = params.channels.unwrap_or(2) as u8;
    if ch == 0 {
        return Err(Error::invalid(
            "Opus decoder: channels must be 1 or 2 when no OpusHead cookie is supplied",
        ));
    }
    Ok(OpusHead::family_0(ch, sr))
}

fn validate_output_rate(rate: u32) -> Result<()> {
    match rate {
        8_000 | 12_000 | 16_000 | 24_000 | 48_000 => Ok(()),
        _ => Err(Error::unsupported(format!(
            "Opus decoder: output rate {rate} Hz is not one of 8000 / 12000 / 16000 / 24000 / 48000 \
             (RFC 6716 §2.1.1)"
        ))),
    }
}

/// Input callback — supplies one compressed Opus packet per call from
/// the front of the persistent queue. The packet's byte count is
/// written into the packet descriptor so AT can read the variable
/// size from the right place.
///
/// # Safety
/// `in_user_data` must point to a valid `InputContext` for the
/// duration of the call.
unsafe extern "C" fn opus_input_callback(
    _converter: AudioConverterRef,
    io_number_data_packets: *mut u32,
    io_data: *mut AudioBufferList1,
    out_packet_desc: *mut *mut AudioStreamPacketDescription,
    in_user_data: *mut c_void,
) -> sys::OSStatus {
    let ctx = &mut *(in_user_data as *mut InputContext);
    if ctx.queue.is_empty() {
        *io_number_data_packets = 0;
        (*io_data).buffers[0].data_byte_size = 0;
        (*io_data).buffers[0].data = std::ptr::null_mut();
        return 0;
    }
    let pkt = ctx.queue.remove(0);

    ctx.packet_desc = AudioStreamPacketDescription {
        start_offset: 0,
        // AT reads the per-packet frame count from the Opus TOC byte
        // itself — set this to 0 so the converter consults its own
        // parse rather than ours.
        variable_frames_in_packet: 0,
        data_byte_size: pkt.len() as u32,
    };

    *io_number_data_packets = 1;
    (*io_data).number_buffers = 1;
    (*io_data).buffers[0].data_byte_size = pkt.len() as u32;
    (*io_data).buffers[0].data = pkt.as_ptr() as *mut u8;
    (*io_data).buffers[0].number_channels = 0;

    if !out_packet_desc.is_null() {
        *out_packet_desc = &mut ctx.packet_desc;
    }

    // Stash the moved packet so its allocation outlives the callback
    // and AT can still read the bytes after we return.
    ctx.handed_off.push(pkt);
    0
}

/// Factory function registered with the codec registry.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(OpusAtDecoder::new(params)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{CodecId, CodecParameters};

    fn params_opus_48k_stereo() -> CodecParameters {
        let mut p = CodecParameters::audio(CodecId::new("opus"));
        p.sample_rate = Some(48_000);
        p.channels = Some(2);
        p
    }

    #[test]
    fn make_decoder_succeeds_48k_stereo_no_cookie() {
        let r = make_decoder(&params_opus_48k_stereo());
        assert!(r.is_ok(), "make_decoder failed: {:?}", r.err());
    }

    #[test]
    fn make_decoder_succeeds_48k_mono_no_cookie() {
        let mut p = params_opus_48k_stereo();
        p.channels = Some(1);
        assert!(make_decoder(&p).is_ok());
    }

    #[test]
    fn make_decoder_succeeds_with_cookie() {
        let mut p = params_opus_48k_stereo();
        p.extradata = OpusHead::family_0(2, 48_000).to_bytes();
        assert!(make_decoder(&p).is_ok());
    }

    #[test]
    fn make_decoder_rejects_mapping_family_1() {
        let mut p = params_opus_48k_stereo();
        let head = OpusHead {
            version: 1,
            channels: 6,
            pre_skip: 3_840,
            input_sample_rate: 48_000,
            output_gain: 0,
            mapping_family: 1,
            mapping_table: vec![4, 2, 0, 4, 1, 2, 3, 5],
        };
        p.extradata = head.to_bytes();
        p.channels = Some(6);
        let r = make_decoder(&p);
        assert!(
            r.is_err(),
            "mapping family 1 should be rejected by the AT bridge"
        );
    }

    #[test]
    fn make_decoder_rejects_zero_channels() {
        let mut p = params_opus_48k_stereo();
        p.channels = Some(0);
        assert!(make_decoder(&p).is_err());
    }

    #[test]
    fn make_decoder_rejects_invalid_output_rate() {
        let mut p = params_opus_48k_stereo();
        p.sample_rate = Some(44_100);
        let r = make_decoder(&p);
        assert!(
            r.is_err(),
            "44.1 kHz must be rejected (Opus output rates: 8/12/16/24/48 kHz)"
        );
    }

    #[test]
    fn opus_head_accessible_after_construct() {
        let dec = OpusAtDecoder::new(&params_opus_48k_stereo()).expect("decoder construct");
        assert_eq!(dec.opus_head().channels, 2);
        assert_eq!(dec.output_sample_rate(), 48_000);
    }
}
