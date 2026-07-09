//! FLAC encoder backed by macOS AudioConverter (`kAudioFormatFLAC`).
//!
//! Input:  interleaved S16 (or S32) PCM in `AudioFrame::data[0]`.
//! Output: one raw FLAC packet per encoded block (no container framing —
//!         the `fLaC` signature + metadata-block chain belongs at the
//!         file or `dfLa`-in-container level, not on every packet).
//!
//! AudioToolbox exposes `kAudioFormatFLAC` as a compression target on
//! macOS 13+. The encoder is the symmetric partner to
//! `FlacAtDecoder`: both sides use the Xiph "FLAC in ISOBMFF" `dfLa`
//! magic cookie convention (see [`crate::flac`] for the empirical
//! probe-derived rationale on the decode side; the encoder vends the
//! same shape).
//!
//! ## Bit depth
//!
//! Per Apple's `CoreAudioBaseTypes.h` enum comment for
//! `kAudioFormatFLAC`, the format's `mFormatFlags` slot carries the
//! same `kAppleLosslessFormatFlag_*BitSourceData` value that ALAC uses
//! — declaring the source PCM bit depth. So an S16 input maps to
//! `K_AF_APPLE_LOSSLESS_16_BIT` and an S32 input to
//! `K_AF_APPLE_LOSSLESS_32_BIT`. Other input widths (e.g. F32) are
//! rejected upstream so the encoder doesn't silently quantise.
//!
//! ## Frame length
//!
//! AudioConverter packetises FLAC at a fixed block size that matches
//! the `frames_per_packet` field of the output ASBD. We use the same
//! 4096-sample default as the decode side — RFC 9639 §9.1.2 Table 1
//! code 11, well within the AT-accepted range and matching what every
//! fixture in `docs/audio/flac/fixtures/` uses.
//!
//! ## Output cookie
//!
//! After the converter is built, the encoder reads back the
//! `kAudioConverterCompressionMagicCookie` property — AT vends a fully
//! formed `dfLa` box (or a near-equivalent FLAC-in-MP4 metadata
//! chunk) that a downstream muxer can paste directly into a `dfLa`
//! sample-entry box. The cookie is published through
//! `output_params.extradata`. If the property query fails for any
//! reason the encoder falls back to a synthesised cookie built from
//! `(sample_rate, channels, bit_depth)` via
//! [`crate::flac::build_magic_cookie`] so callers always get a usable
//! blob.

use std::ffi::c_void;

use oxideav_core::Encoder;
use oxideav_core::{
    CodecId, CodecParameters, Error, Frame, Packet, Result, SampleFormat, TimeBase,
};

use crate::flac::{self, StreamInfo};
use crate::status::status_error;
use crate::sys::{
    self, AudioBuffer, AudioBufferList1, AudioConverterRef, AudioStreamBasicDescription,
    AudioStreamPacketDescription, K_AUDIO_CONVERTER_COMPRESSION_MAGIC_COOKIE,
    K_AUDIO_CONVERTER_MAX_OUTPUT_PACKET_SIZE, NO_ERR,
};

/// Canonical FLAC block size used by every fixture in
/// `docs/audio/flac/fixtures/`. RFC 9639 §9.1.2 Table 1 code 11.
pub const DEFAULT_FRAME_LENGTH: u32 = 4096;

/// State handed to the AudioConverter PCM input callback.
///
/// AT's FLAC encoder slot calls back twice per `FillComplexBuffer`
/// invocation in steady state: once asking for one packet's worth of
/// PCM (`frame_length` frames), then once more asking for any
/// further input. If that second call returns zero bytes, AT
/// interprets it as end-of-stream and **refuses to fire the callback
/// on subsequent FCB calls** — the converter sits in a permanently
/// drained state that no amount of fresh PCM injected on later
/// invocations will unstick.
///
/// The fix is to keep a **persistent** PCM queue alive across the
/// encoder's whole lifetime: the callback always serves from the
/// queue, and the encoder only invokes FCB once enough PCM has been
/// buffered that the callback will satisfy AT's lookahead pull
/// without returning short. With this discipline the converter
/// stays "warm" and emits a steady stream of compressed packets.
struct PcmContext {
    /// Continuous PCM byte queue (interleaved S16 / S32 per ASBD).
    /// Owned by `FlacAtEncoder`; the raw pointer the callback exposes
    /// to AT is invalidated whenever this queue is mutated, so the
    /// encoder must take care to mutate it ONLY while AT is not
    /// inside FCB.
    queue: Vec<u8>,
    /// Read position into `queue` (front-of-queue advances during
    /// callbacks; trimmed periodically to avoid unbounded growth).
    read_pos: usize,
    /// Bytes per single PCM frame (= channels × bytes_per_sample).
    bytes_per_frame: u32,
}

impl PcmContext {
    fn remaining(&self) -> usize {
        self.queue.len().saturating_sub(self.read_pos)
    }

    fn extend(&mut self, pcm: &[u8]) {
        // Periodic compaction: once the read cursor crosses half the
        // queue length, shift the live tail down to the front. Keeps
        // memory usage bounded for long encode sessions.
        if self.read_pos > 0 && self.read_pos >= self.queue.len() / 2 {
            self.queue.drain(..self.read_pos);
            self.read_pos = 0;
        }
        self.queue.extend_from_slice(pcm);
    }
}

/// AudioConverter-backed FLAC encoder.
pub struct FlacAtEncoder {
    codec_id: CodecId,
    converter: AudioConverterRef,
    channels: u16,
    #[allow(dead_code)]
    bit_depth: u8,
    bytes_per_frame: u32,
    /// Encoder packet size (PCM frames per output FLAC packet).
    frame_length: u32,
    /// Persistent PCM feeder context. Held in a Box so the address
    /// AT receives via the callback's user-data pointer stays stable
    /// across mutations of the Vec inside.
    feeder: Box<PcmContext>,
    /// Per-packet max raw FLAC bytes (queried from AT).
    max_packet_bytes: u32,
    pending: Vec<Packet>,
    out_params: CodecParameters,
    pts: i64,
    time_base: TimeBase,
    eof: bool,
}

// SAFETY: same justification as AlacAtEncoder — the AudioConverterRef
// is used single-threaded inside the Encoder impl. We never share the
// raw handle across threads during one call.
unsafe impl Send for FlacAtEncoder {}

impl FlacAtEncoder {
    fn new(params: &CodecParameters) -> Result<Self> {
        let fw = sys::framework()
            .map_err(|e| Error::unsupported(format!("AudioToolbox unavailable: {e}")))?;

        let sr = params.sample_rate.unwrap_or(48_000);
        let ch = params.channels.unwrap_or(2);

        if !(1..=8).contains(&ch) {
            return Err(Error::unsupported(format!(
                "FLAC encoder: unsupported channel count {ch}"
            )));
        }

        // AT's FLAC encoder slot was empirically probed (round 218)
        // across every (input PCM width × source-data flag) combo. The
        // S32 → 32-bit FLAC combo is the only one AT rejects with
        // `kAudioConverterErr_FormatNotSupported` (`'fmt?'` =
        // 1718449215). S16 and S32 PCM input are both accepted, but
        // the compressed-side declared bit depth must not exceed 24
        // — AT does not appear to ship a 32-bit FLAC compression
        // path on the macOS slots tested. So we cap the output side
        // at 24-bit for S32 input. The PCM bytes that fit in 24 bits
        // round-trip cleanly through the 24-bit FLAC compressor;
        // callers that need bit-true 32-bit lossless should stick to
        // the ALAC path until AT's FLAC slot grows the 32-bit
        // compression tier.
        let sample_format = params.sample_format.unwrap_or(SampleFormat::S16);
        let (in_asbd, bit_depth, output_bit_depth, bytes_per_frame) = match sample_format {
            SampleFormat::S16 => (
                AudioStreamBasicDescription::pcm_s16(sr as f64, ch as u32),
                16u8,
                16u8,
                2u32 * ch as u32,
            ),
            SampleFormat::S32 => (
                AudioStreamBasicDescription::pcm_s32(sr as f64, ch as u32),
                32u8,
                24u8, // AT FLAC encoder caps compressed depth at 24
                4u32 * ch as u32,
            ),
            other => {
                return Err(Error::unsupported(format!(
                    "FLAC encoder: only S16 / S32 input supported (got {other:?})"
                )))
            }
        };

        let bit_depth_flag = flac::bit_depth_flag(output_bit_depth).ok_or_else(|| {
            Error::unsupported(format!(
                "FLAC encoder: unsupported output bit_depth {output_bit_depth}"
            ))
        })?;
        let out_asbd = AudioStreamBasicDescription::flac(
            sr as f64,
            ch as u32,
            bit_depth_flag,
            DEFAULT_FRAME_LENGTH,
        );

        let mut converter: AudioConverterRef = std::ptr::null_mut();
        let status = unsafe { sys::audio_converter_new(fw, &in_asbd, &out_asbd, &mut converter) };
        if status != NO_ERR {
            return Err(status_error("AudioConverterNew (FLAC enc)", status));
        }

        // Query maximum output packet size so the per-packet output
        // buffer is sized correctly. Worst case for a FLAC packet is a
        // verbatim (uncompressed) block: `frame_length × channels ×
        // bytes_per_sample` plus a few bytes of frame header / footer.
        // Use a 2× over-allocation as a generous safety margin.
        let mut max_pkt: u32 = DEFAULT_FRAME_LENGTH * bytes_per_frame * 2 + 1024;
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

        // Read back the encoder-vended magic cookie. AT returns a fully
        // formed `dfLa` box (or a closely related FLAC-in-MP4 metadata
        // chunk) that a downstream muxer can paste verbatim into a
        // sample-entry box. If the property query fails — e.g. on an
        // older macOS slot — synthesise a minimal `dfLa` cookie from
        // the configured (sample_rate, channels, bit_depth) so callers
        // always receive something a downstream decoder can use.
        let cookie = read_compression_cookie(fw, converter).unwrap_or_else(|_| {
            let info = StreamInfo {
                min_blocksize: DEFAULT_FRAME_LENGTH as u16,
                max_blocksize: DEFAULT_FRAME_LENGTH as u16,
                min_framesize: 0,
                max_framesize: 0,
                sample_rate: sr,
                channels: ch as u8,
                bits_per_sample: output_bit_depth,
                total_samples: 0,
                md5: [0u8; 16],
            };
            flac::build_magic_cookie(&info)
        });

        let mut out_params = CodecParameters::audio(params.codec_id.clone());
        out_params.sample_rate = Some(sr);
        out_params.channels = Some(ch);
        out_params.sample_format = Some(sample_format);
        out_params.extradata = cookie;

        Ok(Self {
            codec_id: params.codec_id.clone(),
            converter,
            channels: ch,
            bit_depth,
            bytes_per_frame,
            frame_length: DEFAULT_FRAME_LENGTH,
            feeder: Box::new(PcmContext {
                queue: Vec::with_capacity((DEFAULT_FRAME_LENGTH * bytes_per_frame * 4) as usize),
                read_pos: 0,
                bytes_per_frame,
            }),
            max_packet_bytes: max_pkt.max(256),
            pending: Vec::new(),
            out_params,
            pts: 0,
            time_base: TimeBase::new(1, sr as i64),
            eof: false,
        })
    }

    /// Drain whatever PCM is currently buffered in the feeder, one
    /// `frame_length`-sized packet at a time.
    ///
    /// AT's FLAC encoder calls back at least twice per
    /// `FillComplexBuffer` invocation (one frame-of-input request +
    /// one look-ahead pull), so we keep at least **two packets'
    /// worth** of PCM available before pumping FCB. The trailing
    /// one-packet-of-slack also prevents the callback ever returning
    /// zero-length while there's still PCM to encode further on —
    /// the same poison condition that locks the converter at EOF.
    ///
    /// `final_flush == true` switches to "give whatever's left"
    /// mode: the slack is dropped, and the encoder is willing to
    /// hand AT a short tail and accept the EOF poison after the
    /// last packet.
    fn drain_pcm(&mut self, final_flush: bool) -> Result<()> {
        let packet_pcm_bytes = (self.frame_length * self.bytes_per_frame) as usize;
        // Keep one packet of slack in non-final mode so AT's
        // look-ahead pull is satisfied.
        let slack = if final_flush { 0 } else { packet_pcm_bytes };

        while self.feeder.remaining() >= packet_pcm_bytes + slack {
            self.pump_one_packet()?;
        }
        if final_flush && self.feeder.remaining() > 0 {
            // Short tail.
            self.pump_one_packet()?;
        }
        Ok(())
    }

    /// Single `FillComplexBuffer` call asking for one FLAC packet's
    /// worth of compressed bytes. Returns `Ok(())` whether or not the
    /// converter produced output (an empty pull just means the
    /// encoder is still buffering).
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
                "AudioConverterFillComplexBuffer (FLAC enc)",
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
}

impl Drop for FlacAtEncoder {
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

impl Encoder for FlacAtEncoder {
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
                "FlacAtEncoder only accepts Audio frames",
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
/// query + value fetch). Returns the cookie bytes verbatim — typically
/// a `dfLa` ISOBMFF box per the Xiph FLAC-in-ISOBMFF spec.
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
            &format!("GetPropertyInfo(CompressionMagicCookie / FLAC) (size {size})"),
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
            "GetProperty(CompressionMagicCookie / FLAC)",
            status,
        ));
    }
    buf.truncate(io_size as usize);
    Ok(buf)
}

/// PCM input callback for FLAC encode.
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
        // Signal "no more PCM right now". AT may interpret this as
        // end-of-stream; the encoder ensures it only happens when
        // we're actually flushing.
        *io_number_data_packets = 0;
        (*io_data).buffers[0].data_byte_size = 0;
        (*io_data).buffers[0].data = std::ptr::null_mut();
        return 0;
    }

    // AT requests a specific number of input PCM "packets" (= PCM
    // frames). Honour it up to whatever the queue holds.
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
    Ok(Box::new(FlacAtEncoder::new(params)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{CodecId, CodecParameters};

    fn params_flac_48k_stereo() -> CodecParameters {
        let mut p = CodecParameters::audio(CodecId::new("flac"));
        p.sample_rate = Some(48_000);
        p.channels = Some(2);
        p.sample_format = Some(SampleFormat::S16);
        p
    }

    #[test]
    fn make_encoder_succeeds_s16() {
        let r = make_encoder(&params_flac_48k_stereo());
        assert!(r.is_ok(), "make_encoder failed: {:?}", r.err());
    }

    #[test]
    fn make_encoder_succeeds_s32() {
        let mut p = params_flac_48k_stereo();
        p.sample_format = Some(SampleFormat::S32);
        let r = make_encoder(&p);
        assert!(r.is_ok(), "make_encoder (S32) failed: {:?}", r.err());
    }

    #[test]
    fn make_encoder_rejects_unsupported_sample_format() {
        let mut p = params_flac_48k_stereo();
        p.sample_format = Some(SampleFormat::F32);
        let r = make_encoder(&p);
        assert!(r.is_err(), "F32 input must be rejected (not S16/S32)");
    }

    #[test]
    fn make_encoder_rejects_too_many_channels() {
        let mut p = params_flac_48k_stereo();
        p.channels = Some(16);
        let r = make_encoder(&p);
        assert!(r.is_err(), "16 channels must be rejected (cap is 8)");
    }

    #[test]
    fn encoder_publishes_dfla_magic_cookie() {
        let enc = make_encoder(&params_flac_48k_stereo()).expect("make_encoder");
        let cookie = &enc.output_params().extradata;
        assert!(
            cookie.len() >= flac::MAGIC_COOKIE_MIN_LEN,
            "cookie too short: {} bytes (need at least {})",
            cookie.len(),
            flac::MAGIC_COOKIE_MIN_LEN
        );
        assert!(
            cookie.len() <= flac::MAGIC_COOKIE_MAX_LEN,
            "cookie exceeds AT max: {} bytes",
            cookie.len()
        );
        // Box type at offset 4..8 must be 'dfLa' (whether the cookie
        // came from AT itself or our synthesised fallback). The decoder
        // round-trips it via parse_magic_cookie which validates this
        // exact byte pattern.
        assert_eq!(
            &cookie[4..8],
            b"dfLa",
            "cookie box type must be 'dfLa' (Xiph FLAC-in-ISOBMFF specific box)"
        );
        // Cookie must round-trip through parse_magic_cookie back to a
        // StreamInfo with the configured sample rate and channel count.
        let info = flac::parse_magic_cookie(cookie).expect("cookie parses");
        assert_eq!(info.sample_rate, 48_000);
        assert_eq!(info.channels, 2);
        assert_eq!(info.bits_per_sample, 16);
    }

    #[test]
    fn output_params_echo_input_format() {
        let enc = make_encoder(&params_flac_48k_stereo()).expect("make_encoder");
        let op = enc.output_params();
        assert_eq!(op.sample_rate, Some(48_000));
        assert_eq!(op.channels, Some(2));
        assert_eq!(op.sample_format, Some(SampleFormat::S16));
    }

    #[test]
    fn s32_cookie_declares_24bit_compressed_depth() {
        // AT's FLAC encoder slot caps the compressed bit depth at 24
        // (see module docs / the empirical probe in
        // `new`). S32 PCM input therefore produces a cookie that
        // advertises bits_per_sample = 24 — the bytes the S32 word
        // carries within the 24-bit range round-trip cleanly.
        let mut p = params_flac_48k_stereo();
        p.sample_format = Some(SampleFormat::S32);
        let enc = make_encoder(&p).expect("make_encoder S32");
        let cookie = &enc.output_params().extradata;
        let info = flac::parse_magic_cookie(cookie).expect("cookie parses");
        assert_eq!(
            info.bits_per_sample, 24,
            "S32 input must produce a 24-bit STREAMINFO bit_depth (AT cap)"
        );
    }
}
