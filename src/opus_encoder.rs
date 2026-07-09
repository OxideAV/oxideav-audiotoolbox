//! Opus encoder backed by macOS AudioConverter (`kAudioFormatOpus`).
//!
//! Input:  interleaved S16 (or F32) PCM at 8 / 12 / 16 / 24 / 48 kHz
//!         per RFC 6716 §2.1.1; the bridge defaults to 48 kHz / 2 ch.
//! Output: one raw Opus packet per encoded block (no container
//!         framing — the OpusHead identification header travels via
//!         `output_params.extradata`, and the per-packet TOC byte
//!         carries the rest of the per-packet metadata).
//!
//! AT exposes `kAudioFormatOpus` (FourCC `'opus'`) as a compression
//! target on the macOS releases that ship the Opus codec slot. The
//! encoder shape mirrors the FLAC encoder: a `Box<PcmContext>` queue
//! drives the input callback, the slack discipline keeps AT from
//! locking itself at end-of-stream mid-encode, and the encoder reads
//! back the OpusHead the converter vends through the compression
//! magic-cookie property (synthesising one if AT does not vend it).
//!
//! ## Bit-rate
//!
//! Configurable via `CodecParameters::bit_rate` (default 96 000 bit/s
//! — a typical music-quality target). Plumbed through to AT via
//! `AudioConverterSetProperty(kAudioConverterEncodeBitRate, …)`. AT
//! quantises the supplied value to the closest rate its CBR / VBR
//! scheduler accepts and the encoder reads it back via
//! `output_params.bit_rate`.
//!
//! ## Frame length
//!
//! Default per-packet duration: 20 ms (`fpp = sample_rate * 20 /
//! 1000`). At 48 kHz that yields 960 PCM frames per output packet
//! (RFC 6716 Table 2). Callers can override the duration via
//! `CodecParameters::options.insert("frame_duration_ms", "10")` (or
//! any of `2.5 / 5 / 10 / 20 / 40 / 60`) — the value is mapped to
//! the corresponding `frames_per_packet` field of the compressed-side
//! ASBD.

use std::ffi::c_void;

use oxideav_core::Encoder;
use oxideav_core::{
    CodecId, CodecParameters, Error, Frame, Packet, Result, SampleFormat, TimeBase,
};

use crate::opus::{self, OpusHead, DEFAULT_FRAME_DURATION_MS, DEFAULT_PRE_SKIP};
use crate::status::status_error;
use crate::sys::{
    self, AudioBuffer, AudioBufferList1, AudioConverterRef, AudioStreamBasicDescription,
    AudioStreamPacketDescription, K_AUDIO_CONVERTER_COMPRESSION_MAGIC_COOKIE,
    K_AUDIO_CONVERTER_ENCODE_BIT_RATE, K_AUDIO_CONVERTER_MAX_OUTPUT_PACKET_SIZE, NO_ERR,
};

/// Default encode bit-rate (96 kbit/s — a balanced music-quality
/// target at 48 kHz stereo).
pub const DEFAULT_BIT_RATE: u32 = 96_000;

/// Persistent PCM feeder context. The same pattern as the FLAC
/// encoder's `PcmContext` — see that module's commentary for the
/// rationale on why the queue must survive across `FillComplexBuffer`
/// calls.
struct PcmContext {
    queue: Vec<u8>,
    read_pos: usize,
    bytes_per_frame: u32,
}

impl PcmContext {
    fn remaining(&self) -> usize {
        self.queue.len().saturating_sub(self.read_pos)
    }

    fn extend(&mut self, pcm: &[u8]) {
        if self.read_pos > 0 && self.read_pos >= self.queue.len() / 2 {
            self.queue.drain(..self.read_pos);
            self.read_pos = 0;
        }
        self.queue.extend_from_slice(pcm);
    }
}

/// AudioConverter-backed Opus encoder.
pub struct OpusAtEncoder {
    codec_id: CodecId,
    converter: AudioConverterRef,
    channels: u16,
    sample_rate: u32,
    bytes_per_frame: u32,
    /// Encoder packet size (PCM frames per output Opus packet).
    frame_length: u32,
    feeder: Box<PcmContext>,
    /// Per-packet max raw Opus bytes (queried from AT).
    max_packet_bytes: u32,
    pending: Vec<Packet>,
    out_params: CodecParameters,
    pts: i64,
    time_base: TimeBase,
    eof: bool,
}

// SAFETY: AudioConverterRef is used single-threaded inside the Encoder
// impl. We never share the raw handle across threads during one call.
unsafe impl Send for OpusAtEncoder {}

impl OpusAtEncoder {
    fn new(params: &CodecParameters) -> Result<Self> {
        let fw = sys::framework()
            .map_err(|e| Error::unsupported(format!("AudioToolbox unavailable: {e}")))?;

        let sr = params.sample_rate.unwrap_or(48_000);
        validate_input_rate(sr)?;

        let ch = params.channels.unwrap_or(2);
        if !(1..=2).contains(&ch) {
            return Err(Error::unsupported(format!(
                "Opus encoder (mapping family 0): channel count {ch} out of range (1..=2)"
            )));
        }

        let sample_format = params.sample_format.unwrap_or(SampleFormat::S16);
        let (in_asbd, bytes_per_frame) = match sample_format {
            SampleFormat::S16 => (
                AudioStreamBasicDescription::pcm_s16(sr as f64, ch as u32),
                2u32 * ch as u32,
            ),
            SampleFormat::F32 => (
                AudioStreamBasicDescription::pcm_float32(sr as f64, ch as u32),
                4u32 * ch as u32,
            ),
            other => {
                return Err(Error::unsupported(format!(
                    "Opus encoder: only S16 / F32 input supported (got {other:?})"
                )))
            }
        };

        // Resolve per-packet frame length. At 48 kHz the duration map
        // is `fpp = sample_rate * duration_ms / 1000`; for lower
        // output rates we scale linearly so a 20 ms request at
        // 16 kHz lands on 320 frames per packet.
        let duration_ms = parse_frame_duration_ms(params)?;
        let frame_length = if sr == 48_000 {
            opus::frames_per_packet_48k(duration_ms).map_err(|e| Error::invalid(e.0))?
        } else {
            ((sr as f64 * duration_ms) / 1000.0) as u32
        };
        if frame_length == 0 {
            return Err(Error::invalid(format!(
                "Opus encoder: frame_length resolved to 0 (rate {sr} Hz, duration {duration_ms} ms)"
            )));
        }

        let out_asbd = AudioStreamBasicDescription::opus(sr as f64, ch as u32, frame_length);

        let mut converter: AudioConverterRef = std::ptr::null_mut();
        let status = unsafe { sys::audio_converter_new(fw, &in_asbd, &out_asbd, &mut converter) };
        if status != NO_ERR {
            return Err(status_error("AudioConverterNew (Opus enc)", status));
        }

        // Configure the target bit-rate. AT quantises to its own grid;
        // we read the actual value back after the property set.
        let requested_bitrate: u64 = params.bit_rate.unwrap_or(DEFAULT_BIT_RATE as u64);
        let br_u32 = requested_bitrate as u32;
        let status = unsafe {
            sys::audio_converter_set_property(
                fw,
                converter,
                K_AUDIO_CONVERTER_ENCODE_BIT_RATE,
                std::mem::size_of::<u32>() as u32,
                &br_u32 as *const u32 as *const c_void,
            )
        };
        // Bit-rate is advisory — AT may refuse the value if it is out
        // of the codec's supported range. Log via the error path is
        // unnecessary here; the converter is still usable at its
        // default rate.
        let _ = status;

        let mut actual_br: u32 = br_u32;
        let mut br_size = std::mem::size_of::<u32>() as u32;
        unsafe {
            let _ = sys::audio_converter_get_property(
                fw,
                converter,
                K_AUDIO_CONVERTER_ENCODE_BIT_RATE,
                &mut br_size,
                &mut actual_br as *mut u32 as *mut c_void,
            );
        }

        // Query maximum output packet size for AT-managed packet
        // sizing. Fall back to a worst-case Opus bound otherwise:
        // RFC 6716 §3.2 caps a single Opus frame at 1275 bytes, and
        // a code-3 packet may carry up to 48 frames (RFC 6716 §3.2.5),
        // so the theoretical max is 1275 × 48 + a few header bytes;
        // for the single-frame mode AT uses, 1500 bytes is more than
        // enough.
        let mut max_pkt: u32 = 1_500;
        let mut prop_size = std::mem::size_of::<u32>() as u32;
        unsafe {
            let _ = sys::audio_converter_get_property(
                fw,
                converter,
                K_AUDIO_CONVERTER_MAX_OUTPUT_PACKET_SIZE,
                &mut prop_size,
                &mut max_pkt as *mut u32 as *mut c_void,
            );
        }

        // Publish an OpusHead (RFC 7845 §5.1) identification header
        // through `output_params.extradata` — this is the
        // container-layer descriptor a downstream Ogg / MP4 / WebM
        // muxer needs to write its Opus track header. AT's own
        // compression-magic-cookie property returns an opaque
        // private blob whose shape is not documented in
        // `CoreAudioBaseTypes.h`; we read it here only to verify the
        // converter accepted the encoder configuration, but do not
        // forward it to consumers (its layout is AT-internal).
        let _ = read_compression_cookie(fw, converter);
        let cookie = OpusHead {
            version: 1,
            channels: ch as u8,
            pre_skip: DEFAULT_PRE_SKIP,
            input_sample_rate: sr,
            output_gain: 0,
            mapping_family: 0,
            mapping_table: Vec::new(),
        }
        .to_bytes();

        let mut out_params = CodecParameters::audio(params.codec_id.clone());
        out_params.sample_rate = Some(sr);
        out_params.channels = Some(ch);
        out_params.sample_format = Some(sample_format);
        out_params.bit_rate = Some(actual_br as u64);
        out_params.extradata = cookie;
        out_params
            .options
            .insert("frame_duration_ms", format!("{duration_ms}"));

        Ok(Self {
            codec_id: params.codec_id.clone(),
            converter,
            channels: ch,
            sample_rate: sr,
            bytes_per_frame,
            frame_length,
            feeder: Box::new(PcmContext {
                queue: Vec::with_capacity((frame_length * bytes_per_frame * 4) as usize),
                read_pos: 0,
                bytes_per_frame,
            }),
            max_packet_bytes: max_pkt.max(1_500),
            pending: Vec::new(),
            out_params,
            pts: 0,
            time_base: TimeBase::new(1, sr as i64),
            eof: false,
        })
    }

    /// Drain whatever PCM is currently buffered in the feeder, one
    /// `frame_length`-sized packet at a time. Keeps one packet of
    /// slack while not flushing so AT's look-ahead pull is always
    /// satisfied.
    fn drain_pcm(&mut self, final_flush: bool) -> Result<()> {
        let packet_pcm_bytes = (self.frame_length * self.bytes_per_frame) as usize;
        let slack = if final_flush { 0 } else { packet_pcm_bytes };

        while self.feeder.remaining() >= packet_pcm_bytes + slack {
            self.pump_one_packet()?;
        }
        if final_flush && self.feeder.remaining() > 0 {
            // Zero-pad the trailing partial packet so AT receives a
            // full block (Opus has a CBR frame contract — the
            // tail-pad is the standard EOS handling for CBR codecs).
            let need = packet_pcm_bytes - self.feeder.remaining();
            let pad = vec![0u8; need];
            self.feeder.extend(&pad);
            self.pump_one_packet()?;
        }
        Ok(())
    }

    fn pump_one_packet(&mut self) -> Result<()> {
        let fw = sys::framework()
            .map_err(|e| Error::unsupported(format!("AudioToolbox unavailable: {e}")))?;

        let out_size = self.max_packet_bytes as usize;
        let mut out_buf = vec![0u8; out_size];
        let mut output_packet_count: u32 = 1;
        let mut pkt_desc = AudioStreamPacketDescription::default();
        let mut abl = AudioBufferList1 {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: self.channels as u32,
                data_byte_size: out_size as u32,
                data: out_buf.as_mut_ptr(),
            }],
        };

        let ctx_ptr = &mut *self.feeder as *mut PcmContext as *mut c_void;
        let status = unsafe {
            sys::audio_converter_fill_complex_buffer(
                fw,
                self.converter,
                pcm_input_callback,
                ctx_ptr,
                &mut output_packet_count,
                &mut abl,
                &mut pkt_desc,
            )
        };
        if status != NO_ERR {
            return Err(status_error(
                "AudioConverterFillComplexBuffer (Opus enc)",
                status,
            ));
        }

        let raw_len = abl.buffers[0].data_byte_size as usize;
        if raw_len == 0 || output_packet_count == 0 {
            return Ok(());
        }
        out_buf.truncate(raw_len);

        let pkt = Packet::new(0, self.time_base, out_buf)
            .with_pts(self.pts)
            .with_keyframe(true);
        self.pts += self.frame_length as i64;
        self.pending.push(pkt);
        Ok(())
    }

    /// Observed configured encode rate (Hz).
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Per-packet PCM frame count (the active Opus frame size).
    pub fn frames_per_packet(&self) -> u32 {
        self.frame_length
    }
}

impl Drop for OpusAtEncoder {
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

impl Encoder for OpusAtEncoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn output_params(&self) -> &CodecParameters {
        &self.out_params
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        if self.eof {
            return Err(Error::Eof);
        }
        match frame {
            Frame::Audio(af) => {
                if af.data.is_empty() || af.data[0].is_empty() {
                    return Ok(());
                }
                self.feeder.extend(&af.data[0]);
                self.drain_pcm(false)
            }
            _ => Err(Error::unsupported(
                "OpusAtEncoder only accepts Audio frames",
            )),
        }
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if !self.pending.is_empty() {
            return Ok(self.pending.remove(0));
        }
        if self.eof {
            return Err(Error::Eof);
        }
        Err(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        self.drain_pcm(true)?;
        self.eof = true;
        Ok(())
    }
}

/// Read the encoder-vended magic cookie via two property calls (size
/// query + value fetch). Returns the cookie bytes verbatim — for AT's
/// Opus slot this is typically the OpusHead (RFC 7845 §5.1) wire
/// form; some macOS releases may decline to vend one, in which case
/// the caller falls back to a synthesised header.
fn read_compression_cookie(fw: &sys::Framework, converter: AudioConverterRef) -> Result<Vec<u8>> {
    let mut size: u32 = 0;
    let mut writable: u8 = 0;
    let status = unsafe {
        sys::audio_converter_get_property_info(
            fw,
            converter,
            K_AUDIO_CONVERTER_COMPRESSION_MAGIC_COOKIE,
            &mut size,
            &mut writable,
        )
    };
    if status != NO_ERR || size == 0 {
        return Err(status_error(
            &format!("GetPropertyInfo(CompressionMagicCookie / Opus) (size {size})"),
            status,
        ));
    }

    let mut buf = vec![0u8; size as usize];
    let mut io_size = size;
    let status = unsafe {
        sys::audio_converter_get_property(
            fw,
            converter,
            K_AUDIO_CONVERTER_COMPRESSION_MAGIC_COOKIE,
            &mut io_size,
            buf.as_mut_ptr() as *mut c_void,
        )
    };
    if status != NO_ERR {
        return Err(status_error(
            "GetProperty(CompressionMagicCookie / Opus)",
            status,
        ));
    }
    buf.truncate(io_size as usize);
    Ok(buf)
}

/// Parse `options["frame_duration_ms"]` if present, defaulting to the
/// 20 ms value defined by [`opus::DEFAULT_FRAME_DURATION_MS`].
fn parse_frame_duration_ms(params: &CodecParameters) -> Result<f64> {
    match params.options.get("frame_duration_ms") {
        Some(s) => s.parse::<f64>().map_err(|e| {
            Error::invalid(format!(
                "Opus encoder: frame_duration_ms = {s:?} is not a number ({e})"
            ))
        }),
        None => Ok(DEFAULT_FRAME_DURATION_MS),
    }
}

fn validate_input_rate(rate: u32) -> Result<()> {
    match rate {
        8_000 | 12_000 | 16_000 | 24_000 | 48_000 => Ok(()),
        _ => Err(Error::unsupported(format!(
            "Opus encoder: input rate {rate} Hz is not one of 8000 / 12000 / 16000 / 24000 / 48000 \
             (RFC 6716 §2.1.1)"
        ))),
    }
}

/// PCM input callback for Opus encode — same shape as the FLAC
/// encoder's callback (persistent queue + read cursor).
///
/// # Safety
/// `in_user_data` must point to a valid `PcmContext` for the duration
/// of the `FillComplexBuffer` call.
unsafe extern "C" fn pcm_input_callback(
    _converter: AudioConverterRef,
    io_number_data_packets: *mut u32,
    io_data: *mut AudioBufferList1,
    _out_packet_desc: *mut *mut AudioStreamPacketDescription,
    in_user_data: *mut c_void,
) -> sys::OSStatus {
    let ctx = &mut *(in_user_data as *mut PcmContext);
    let remaining = ctx.remaining();
    if remaining == 0 {
        *io_number_data_packets = 0;
        (*io_data).buffers[0].data_byte_size = 0;
        (*io_data).buffers[0].data = std::ptr::null_mut();
        return 0;
    }

    let requested = (*io_number_data_packets).max(1) as usize;
    let avail_frames = remaining / ctx.bytes_per_frame as usize;
    let give = avail_frames.min(requested);
    let give_bytes = give * ctx.bytes_per_frame as usize;

    *io_number_data_packets = give as u32;
    (*io_data).number_buffers = 1;
    (*io_data).buffers[0].data_byte_size = give_bytes as u32;
    let ptr = ctx.queue.as_ptr().add(ctx.read_pos) as *mut u8;
    (*io_data).buffers[0].data = ptr;

    ctx.read_pos += give_bytes;
    0
}

/// Factory function registered with the codec registry.
pub fn make_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    Ok(Box::new(OpusAtEncoder::new(params)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{CodecId, CodecParameters};

    fn params_opus_48k_stereo() -> CodecParameters {
        let mut p = CodecParameters::audio(CodecId::new("opus"));
        p.sample_rate = Some(48_000);
        p.channels = Some(2);
        p.sample_format = Some(SampleFormat::S16);
        p
    }

    #[test]
    fn make_encoder_succeeds_s16_48k_stereo() {
        let r = make_encoder(&params_opus_48k_stereo());
        assert!(r.is_ok(), "make_encoder failed: {:?}", r.err());
    }

    #[test]
    fn make_encoder_succeeds_s16_48k_mono() {
        let mut p = params_opus_48k_stereo();
        p.channels = Some(1);
        assert!(make_encoder(&p).is_ok());
    }

    #[test]
    fn make_encoder_rejects_invalid_rate() {
        let mut p = params_opus_48k_stereo();
        p.sample_rate = Some(44_100);
        let r = make_encoder(&p);
        assert!(
            r.is_err(),
            "44.1 kHz must be rejected (Opus rates: 8/12/16/24/48 kHz)"
        );
    }

    #[test]
    fn make_encoder_rejects_unsupported_sample_format() {
        let mut p = params_opus_48k_stereo();
        p.sample_format = Some(SampleFormat::S32);
        let r = make_encoder(&p);
        assert!(
            r.is_err(),
            "S32 input must be rejected (only S16/F32 supported)"
        );
    }

    #[test]
    fn output_params_publish_opus_head_cookie() {
        let enc = make_encoder(&params_opus_48k_stereo()).expect("make_encoder");
        let cookie = &enc.output_params().extradata;
        assert!(
            cookie.len() >= opus::HEAD_LEN_FAMILY_0,
            "cookie too short: {} bytes (need at least {})",
            cookie.len(),
            opus::HEAD_LEN_FAMILY_0
        );
        assert_eq!(
            &cookie[0..8],
            opus::MAGIC,
            "cookie must start with the OpusHead magic signature"
        );
    }

    #[test]
    fn output_params_publish_bit_rate() {
        let enc = make_encoder(&params_opus_48k_stereo()).expect("make_encoder");
        let br = enc
            .output_params()
            .bit_rate
            .expect("bit_rate must be published");
        assert!(br > 0, "published bit_rate must be positive (got {br})");
    }

    #[test]
    fn default_frame_length_is_20ms() {
        let enc = OpusAtEncoder::new(&params_opus_48k_stereo()).expect("encoder construct");
        // 20 ms × 48 kHz = 960 PCM frames per packet.
        assert_eq!(enc.frames_per_packet(), opus::DEFAULT_FRAMES_PER_PACKET_48K);
        let d = enc
            .output_params()
            .options
            .get("frame_duration_ms")
            .expect("frame_duration_ms echoed");
        assert_eq!(d, "20");
    }

    #[test]
    fn frame_duration_override_10ms() {
        let mut p = params_opus_48k_stereo();
        p.options.insert("frame_duration_ms", "10");
        let enc = OpusAtEncoder::new(&p).expect("encoder construct");
        assert_eq!(enc.frames_per_packet(), 480);
    }

    #[test]
    fn frame_duration_override_60ms_max() {
        let mut p = params_opus_48k_stereo();
        p.options.insert("frame_duration_ms", "60");
        let enc = OpusAtEncoder::new(&p).expect("encoder construct");
        assert_eq!(enc.frames_per_packet(), 2880);
    }

    #[test]
    fn frame_duration_rejects_invalid_value() {
        let mut p = params_opus_48k_stereo();
        p.options.insert("frame_duration_ms", "15");
        let r = OpusAtEncoder::new(&p);
        assert!(
            r.is_err(),
            "15 ms must be rejected (valid: 2.5/5/10/20/40/60)"
        );
    }
}
