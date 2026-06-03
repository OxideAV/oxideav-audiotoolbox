//! Opus decoder backed by macOS AudioConverter (`kAudioFormatOpus`).
//!
//! Input:  one self-contained Opus packet per [`Packet`] — TOC byte
//!         (RFC 6716 §3.1) + the packet's framing for code 0..=3
//!         (RFC 6716 §3.2) + per-frame compressed bodies. The
//!         AudioConverter walks the framing internally; the bridge
//!         only validates the TOC byte parses.
//! Output: interleaved S16 PCM [`AudioFrame`]s at 48 kHz (RFC 7845 §5.1
//!         normative output rate). AT may vend the PCM in sub-frame
//!         blocks per `FillComplexBuffer` call, so the per-frame size
//!         invariant the caller can rely on is `samples × channels ×
//!         2 == data.len()`.
//!
//! ## Magic-cookie resolution
//!
//! Three accepted shapes for `CodecParameters::extradata`:
//!
//! 1. **Bare RFC 7845 §5.1 OpusHead body** (19+ bytes, no magic prefix).
//! 2. **Full OpusHead packet** (8-byte "OpusHead" ASCII + body).
//! 3. **AT-vended 28-byte compression cookie**
//!    (`kAudioConverterCompressionMagicCookie` output from the encoder
//!    side — see [`crate::opus`] for the layout). The decoder accepts
//!    these verbatim per AT's cross-direction cookie compatibility
//!    (encoder cookie → decoder converter validated empirically).
//! 4. **Empty extradata** — AT also accepts no cookie; the bridge skips
//!    the `SetProperty` call entirely and lets the converter infer
//!    stream parameters from the first packet's TOC byte. This matches
//!    the no-cookie behaviour observed empirically.
//!
//! ## Channel and rate latching
//!
//! Both the converter input ASBD and the output ASBD are configured at
//! `new()` time from the explicit `CodecParameters` (sample_rate +
//! channels, defaulting to 48 kHz stereo). Mid-stream changes to
//! sample rate or channel count would require tearing down and
//! rebuilding the converter; the bridge rejects them with typed
//! `Error::unsupported`. The configured frame size (`frames_per_packet`)
//! is the upper bound — Opus packets with smaller frame durations
//! (e.g. 2.5 ms CELT in a 20 ms-configured converter) are accepted;
//! the per-packet PTS still advances by the TOC-derived frame size at
//! 48 kHz.
//!
//! The decoder uses the same persistent input-queue + one-packet-of-
//! slack lookahead pattern as the MP3 / FLAC / AMR-NB / AMR-WB
//! bridges so AT never sees "0 packets" mid-stream (which would put
//! it into a permanent end-of-stream state).

use std::ffi::c_void;

use oxideav_core::Decoder;
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, Result, TimeBase};

use crate::opus::{self, OpusHead, Toc};
use crate::sys::{
    self, AudioBuffer, AudioBufferList1, AudioConverterRef, AudioStreamBasicDescription,
    AudioStreamPacketDescription, K_AUDIO_CONVERTER_DECOMPRESSION_MAGIC_COOKIE, NO_ERR,
};

/// Default per-packet duration: 20 ms — every Ogg-Opus fixture under
/// `docs/audio/opus/fixtures/` ships at this cadence and AT's own
/// encoder side defaults to it. The PCM-frame count is computed as
/// `sample_rate / 1000 * 20` and ranges from 160 (at 8 kHz) to 960
/// (at 48 kHz). AT's converter requires that `frames_per_packet`
/// scale with the configured rate — a 960-frame `frames_per_packet`
/// against an 8 kHz ASBD produces `kAudioConverterErr_FormatNotSupported`
/// (probed empirically).
pub const DEFAULT_FRAME_DURATION_MS: u32 = 20;

/// Compute the canonical `frames_per_packet` for a given output rate at
/// the 20 ms default duration.
fn default_frames_per_packet(sample_rate: u32) -> u32 {
    sample_rate / 1000 * DEFAULT_FRAME_DURATION_MS
}

/// State shared with the AudioConverter input callback.
struct InputContext {
    queue: Vec<Vec<u8>>,
    /// Frames handed off to AT during the current `FillComplexBuffer`
    /// call — kept alive so the converter can still reference their
    /// bytes after the callback returns.
    handed_off: Vec<Vec<u8>>,
    /// Per-call packet descriptor (rewritten on each invocation).
    packet_desc: AudioStreamPacketDescription,
}

/// AudioConverter-backed Opus decoder.
pub struct OpusAtDecoder {
    codec_id: CodecId,
    converter: AudioConverterRef,
    #[allow(dead_code)]
    sample_rate: u32,
    channels: u8,
    frames_per_packet: u32,
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
        let fw =
            sys::framework().map_err(|e| Error::other(format!("AudioToolbox unavailable: {e}")))?;

        // Resolve (sample_rate, channels) from extradata (cookie of any
        // accepted shape) or fall back to the explicit params.
        let (cookie_bytes, sample_rate, channels) = resolve_params(params)?;

        if !(1..=8).contains(&channels) {
            return Err(Error::unsupported(format!(
                "Opus decoder: channel count {channels} out of range (1..=8)"
            )));
        }
        if !is_supported_output_rate(sample_rate) {
            return Err(Error::unsupported(format!(
                "Opus decoder: output rate {sample_rate} Hz not supported (RFC 6716 §2.1.1: 8/12/16/24/48 kHz)"
            )));
        }

        let frames_per_packet = default_frames_per_packet(sample_rate);
        let in_asbd = AudioStreamBasicDescription::opus(
            sample_rate as f64,
            channels as u32,
            frames_per_packet,
        );
        let out_asbd = AudioStreamBasicDescription::pcm_s16(sample_rate as f64, channels as u32);

        let mut converter: AudioConverterRef = std::ptr::null_mut();
        let status = unsafe { sys::audio_converter_new(fw, &in_asbd, &out_asbd, &mut converter) };
        if status != NO_ERR {
            return Err(Error::other(format!(
                "AudioConverterNew (Opus dec) failed: OSStatus {status}"
            )));
        }

        // Forward the magic cookie to the converter when we have one.
        // Empty cookies skip SetProperty entirely — AT accepts the
        // no-cookie path for Opus (verified empirically) and infers
        // stream parameters from the first TOC byte.
        if !cookie_bytes.is_empty() {
            let status = unsafe {
                sys::audio_converter_set_property(
                    fw,
                    converter,
                    K_AUDIO_CONVERTER_DECOMPRESSION_MAGIC_COOKIE,
                    cookie_bytes.len() as u32,
                    cookie_bytes.as_ptr() as *const c_void,
                )
            };
            if status != NO_ERR {
                unsafe {
                    let _ = sys::audio_converter_dispose(fw, converter);
                }
                return Err(Error::other(format!(
                    "AudioConverterSetProperty(DecompressionMagicCookie / Opus) failed: OSStatus {status}"
                )));
            }
        }

        let tb = TimeBase::new(1, sample_rate as i64);
        Ok(Self {
            codec_id: params.codec_id.clone(),
            converter,
            sample_rate,
            channels,
            frames_per_packet,
            pending: Vec::new(),
            input_queue: Vec::new(),
            pts: 0,
            time_base: tb,
            eof: false,
        })
    }

    fn decode_packet(&mut self, data: &[u8]) -> Result<()> {
        // RFC 6716 §3.1: every Opus packet must carry at least the
        // TOC byte. (Some code-3 packets need additional bytes; AT
        // performs the deeper walk — we just validate the TOC parses.)
        if data.is_empty() {
            return Err(Error::invalid("Opus: empty packet"));
        }
        let toc = Toc::parse(data).ok_or_else(|| Error::invalid("Opus: TOC parse failed"))?;
        // Stereo-bit consistency: a stereo packet on a mono-configured
        // converter (or vice versa) would mean AT mis-counts PCM
        // samples. Reject it.
        if toc.channels() > self.channels {
            return Err(Error::unsupported(format!(
                "Opus decoder: TOC declares {} channels, converter configured for {}",
                toc.channels(),
                self.channels
            )));
        }
        self.input_queue.push(data.to_vec());
        self.drain_pcm()?;
        Ok(())
    }

    /// Drain PCM frames from AT until either the converter signals it
    /// needs more input, or the queue is down to a single packet
    /// (preserving the look-ahead tail).
    fn drain_pcm(&mut self) -> Result<()> {
        while self.input_queue.len() > 1 {
            if !self.pull_one_pcm_frame()? {
                break;
            }
        }
        Ok(())
    }

    /// Single `FillComplexBuffer` call asking for one packet's worth of
    /// PCM. Returns `true` if a frame was produced.
    fn pull_one_pcm_frame(&mut self) -> Result<bool> {
        let fw =
            sys::framework().map_err(|e| Error::other(format!("AudioToolbox unavailable: {e}")))?;

        let channels = self.channels as usize;
        let frames_per_packet = self.frames_per_packet as usize;
        let buf_size = frames_per_packet * channels * 2; // S16 interleaved
        let mut pcm_buf = vec![0u8; buf_size];

        let mut ctx = InputContext {
            queue: std::mem::take(&mut self.input_queue),
            handed_off: Vec::new(),
            packet_desc: AudioStreamPacketDescription::default(),
        };

        let mut output_packet_count: u32 = frames_per_packet as u32;
        let mut abl = AudioBufferList1 {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: self.channels as u32,
                data_byte_size: buf_size as u32,
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
            return Err(Error::other(format!(
                "AudioConverterFillComplexBuffer (Opus dec) failed: OSStatus {status}"
            )));
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
        self.decode_packet(&packet.data)
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

/// Resolve the `(cookie, sample_rate, channels)` triple from a
/// `CodecParameters`. Cookies are emitted to AT verbatim when at all
/// possible — the only synth path is when no cookie is present and
/// the explicit params provide the rate / channel count.
fn resolve_params(params: &CodecParameters) -> Result<(Vec<u8>, u32, u8)> {
    // AT's compression-side cookie (28 bytes) takes precedence — it's
    // the most precise form because it captures the exact converter
    // configuration the encoder used.
    if let Some(c) = opus::parse_at_compression_cookie(&params.extradata) {
        if c.sample_rate == 0 || c.channel_count == 0 {
            return Err(Error::invalid(
                "Opus: AT compression cookie carries invalid sample_rate / channels",
            ));
        }
        return Ok((
            params.extradata.clone(),
            c.sample_rate,
            c.channel_count as u8,
        ));
    }
    // RFC 7845 OpusHead — either with or without the 8-byte magic
    // prefix. AT's decompression cookie validator requires the
    // 8-byte "OpusHead" ASCII magic in front of the body (probed
    // empirically: a bare 11-byte body returns `'!dat'`, the
    // 19-byte packet-form succeeds). The bridge always forwards the
    // full packet shape so callers may supply either form.
    if !params.extradata.is_empty() {
        if let Some(head) = OpusHead::parse(&params.extradata) {
            let cookie = head.to_packet_bytes();
            // RFC 7845 §5.1: input_sample_rate is informational; the
            // decoder always emits 48 kHz output unless overridden
            // via the explicit params.
            let sr = params.sample_rate.unwrap_or(48_000);
            return Ok((cookie, sr, head.channel_count));
        }
        // Extradata present but unparseable — surface it rather than
        // silently fall through.
        return Err(Error::invalid(format!(
            "Opus: extradata ({} bytes) is not a recognised OpusHead or AT cookie",
            params.extradata.len()
        )));
    }

    // No cookie. Pull rate / channels from the explicit params, with
    // safe defaults (RFC 7845 §5.1 normative 48 kHz stereo).
    let sr = params.sample_rate.unwrap_or(48_000);
    let ch = params.channels.unwrap_or(2);
    Ok((Vec::new(), sr, ch as u8))
}

/// True when `sr` is one of the RFC 6716 §2.1.1 decoder output rates.
fn is_supported_output_rate(sr: u32) -> bool {
    matches!(sr, 8_000 | 12_000 | 16_000 | 24_000 | 48_000)
}

/// Input callback — supplies one compressed Opus packet per call from
/// the front of the persistent queue. The packet's byte count is
/// written into the descriptor so AT can read the variable size from
/// the right place.
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
        // Per-packet PCM frame count varies (depending on the TOC byte's
        // frame size) but AT walks that field internally — set to 0 to
        // let the converter parse it from the bitstream.
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
    fn make_decoder_succeeds_without_cookie() {
        let r = make_decoder(&params_opus_48k_stereo());
        assert!(r.is_ok(), "make_decoder failed: {:?}", r.err());
    }

    #[test]
    fn make_decoder_succeeds_with_opushead_body() {
        let head = OpusHead::stereo(48_000);
        let mut p = params_opus_48k_stereo();
        p.extradata = head.to_body_bytes();
        let r = make_decoder(&p);
        assert!(
            r.is_ok(),
            "make_decoder with OpusHead body failed: {:?}",
            r.err()
        );
    }

    #[test]
    fn make_decoder_succeeds_with_opushead_packet_prefix() {
        let head = OpusHead::stereo(48_000);
        let mut p = params_opus_48k_stereo();
        p.extradata = head.to_packet_bytes(); // 8-byte magic + 11-byte body
        let r = make_decoder(&p);
        assert!(
            r.is_ok(),
            "make_decoder with OpusHead packet prefix failed: {:?}",
            r.err()
        );
    }

    #[test]
    fn make_decoder_succeeds_with_mono_opushead() {
        let head = OpusHead::mono(48_000);
        let mut p = params_opus_48k_stereo();
        p.channels = Some(1);
        p.extradata = head.to_body_bytes();
        let r = make_decoder(&p);
        assert!(r.is_ok(), "mono OpusHead path failed: {:?}", r.err());
    }

    #[test]
    fn make_decoder_succeeds_with_at_compression_cookie() {
        // AT-vended 28-byte cookie for 48 kHz stereo 20 ms.
        let cookie = vec![
            0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0xBB, 0x80, 0x00, 0x00, 0x03, 0xC0, 0xFF, 0xFF,
            0xFC, 0x18, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let mut p = params_opus_48k_stereo();
        p.extradata = cookie;
        let r = make_decoder(&p);
        assert!(
            r.is_ok(),
            "make_decoder w/ AT compression cookie failed: {:?}",
            r.err()
        );
    }

    #[test]
    fn make_decoder_rejects_unparseable_extradata() {
        let mut p = params_opus_48k_stereo();
        // 15 bytes of garbage — too short for OpusHead, wrong size for
        // AT cookie, but non-empty so the synth path doesn't kick in.
        p.extradata = vec![0xAA; 15];
        let r = make_decoder(&p);
        assert!(r.is_err(), "garbage extradata must be rejected");
    }

    #[test]
    fn make_decoder_rejects_unsupported_rate() {
        let mut p = params_opus_48k_stereo();
        p.sample_rate = Some(44_100); // RFC 6716 §2.1.1: not a supported output rate
        let r = make_decoder(&p);
        assert!(r.is_err(), "44.1 kHz output rate must be rejected");
    }

    #[test]
    fn make_decoder_accepts_8khz_output_rate() {
        let mut p = params_opus_48k_stereo();
        p.sample_rate = Some(8_000);
        p.channels = Some(1);
        let r = make_decoder(&p);
        assert!(r.is_ok(), "8 kHz mono output should be accepted");
    }

    #[test]
    fn make_decoder_rejects_too_many_channels() {
        let mut p = params_opus_48k_stereo();
        p.channels = Some(16);
        let r = make_decoder(&p);
        assert!(r.is_err(), "16 channels must be rejected (cap is 8)");
    }

    #[test]
    fn send_packet_rejects_empty_packet() {
        let mut dec = make_decoder(&params_opus_48k_stereo()).expect("decoder");
        let pkt = Packet::new(0, TimeBase::new(1, 48_000), Vec::new());
        assert!(dec.send_packet(&pkt).is_err());
    }

    #[test]
    fn send_packet_rejects_stereo_on_mono_converter() {
        let mut p = params_opus_48k_stereo();
        p.channels = Some(1);
        let mut dec = make_decoder(&p).expect("decoder");
        // TOC: config=31 (CELT FB 20 ms) + stereo bit set + code=0
        let pkt = Packet::new(0, TimeBase::new(1, 48_000), vec![0xFC, 0x00]);
        assert!(
            dec.send_packet(&pkt).is_err(),
            "stereo packet on mono converter must error"
        );
    }

    #[test]
    fn round_trip_codec_id_preserved() {
        let dec = make_decoder(&params_opus_48k_stereo()).expect("decoder");
        assert_eq!(dec.codec_id().as_str(), "opus");
    }

    #[test]
    fn supported_output_rates_table() {
        for r in [8_000, 12_000, 16_000, 24_000, 48_000] {
            assert!(is_supported_output_rate(r), "{r} must be supported");
        }
        for r in [11_025, 22_050, 32_000, 44_100, 88_200, 96_000] {
            assert!(!is_supported_output_rate(r), "{r} must be rejected");
        }
    }
}
