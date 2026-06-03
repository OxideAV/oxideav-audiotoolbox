//! Opus encoder backed by macOS AudioConverter (`kAudioFormatOpus`).
//!
//! Input:  interleaved S16 PCM in [`AudioFrame::data\[0\]`]. Sample rate
//!         must be one of the RFC 6716 §2.1.1 input rates (8/12/16/24/
//!         48 kHz). 48 kHz is the canonical encoder rate.
//! Output: one raw Opus packet per encoded block — TOC byte + per-frame
//!         compressed bodies as defined by RFC 6716 §3. The packet
//!         length comes back through the per-packet
//!         `AudioStreamPacketDescription` AT writes via the output
//!         buffer list. Per RFC 6716 §3.2.1 a single Opus frame
//!         maxes out at 1275 bytes; AT reports up to 1276 via the
//!         `MaximumOutputPacketSize` property (one extra byte for
//!         alignment / framing slack).
//!
//! ## AT-side configuration
//!
//! AudioConverter accepts Opus output ASBDs at all five RFC-specified
//! sample rates and `frames_per_packet` values from 120 (2.5 ms @
//! 48 kHz) to 2880 (60 ms @ 48 kHz). 5760 (120 ms) is rejected with
//! `kAudioConverterErr_FormatNotSupported` — discovered by probing
//! every value in `{0, 120, 240, 480, 960, 1920, 2880, 5760}`.
//!
//! The default frame size is 20 ms (= 960 PCM frames at 48 kHz) —
//! libopus's default, matching every fixture in
//! `docs/audio/opus/fixtures/` and the canonical RTP packetisation
//! (RFC 7587 §4.5).
//!
//! ## Output magic cookie
//!
//! After the converter is built, the encoder reads back
//! `kAudioConverterCompressionMagicCookie` — AT vends a 28-byte
//! payload (see [`crate::opus::AtCompressionCookie`] for the field
//! layout) that downstream code can:
//!
//! * Hand back to a decoder converter as
//!   `kAudioConverterDecompressionMagicCookie` (validated empirically —
//!   round-trip via [`crate::opus_decoder::OpusAtDecoder::new`] accepts
//!   the 28-byte form).
//! * Use to populate a muxer-side OpusHead packet for Ogg / WebM /
//!   ISO BMFF carriage (the cookie supplies `sample_rate +
//!   channel_count + frames_per_packet` — a downstream layer maps
//!   them into the RFC 7845 §5.1 layout).
//!
//! If the property query fails (older macOS slot), the encoder
//! synthesises a body-form OpusHead via
//! [`crate::opus::OpusHead::to_body_bytes`] from
//! `(sample_rate, channels)`. That fallback is sufficient for the
//! decoder side to bootstrap.
//!
//! ## Persistent PCM feeder
//!
//! AT's Opus encoder uses the same callback-cadence shape as the FLAC
//! encoder slot: at least one frame-of-input request per FCB
//! invocation plus a possible look-ahead pull. We mirror the FLAC
//! solution — a `Box<PcmContext>` holding a persistent PCM byte
//! queue + read cursor + one-packet-of-slack discipline — so the
//! callback never returns zero mid-stream and the converter never
//! locks itself into the permanent EOF state observed on the FLAC
//! encoder slot.

use std::ffi::c_void;

use oxideav_core::Encoder;
use oxideav_core::{
    CodecId, CodecParameters, Error, Frame, Packet, Result, SampleFormat, TimeBase,
};

#[cfg(test)]
use crate::opus;
use crate::opus::OpusHead;
use crate::sys::{
    self, AudioBuffer, AudioBufferList1, AudioConverterRef, AudioStreamBasicDescription,
    AudioStreamPacketDescription, K_AUDIO_CONVERTER_COMPRESSION_MAGIC_COOKIE,
    K_AUDIO_CONVERTER_ENCODE_BIT_RATE, K_AUDIO_CONVERTER_MAX_OUTPUT_PACKET_SIZE, NO_ERR,
};

/// Default per-packet duration: 20 ms. Matches libopus's default and
/// the canonical RTP packetisation. The PCM-frame count is computed as
/// `sample_rate / 1000 * 20` and ranges from 160 (at 8 kHz) to 960
/// (at 48 kHz). AT's converter requires that `frames_per_packet`
/// scale with the configured rate.
pub const DEFAULT_FRAME_DURATION_MS: u32 = 20;

/// Canonical `frames_per_packet` for the configured rate at 20 ms.
pub fn default_frames_per_packet(sample_rate: u32) -> u32 {
    sample_rate / 1000 * DEFAULT_FRAME_DURATION_MS
}

/// State handed to the AudioConverter PCM input callback.
///
/// Same shape as the FLAC encoder's `PcmContext` — a persistent PCM
/// byte queue + read cursor + per-frame byte-size helper. The queue is
/// held in a `Box` on the encoder so the address AT receives via the
/// callback user-data pointer stays stable across `Vec` mutations.
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
        // Periodic compaction: once the cursor crosses the queue's
        // midpoint, drop the consumed front so memory stays bounded.
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
    bytes_per_frame: u32,
    frames_per_packet: u32,
    feeder: Box<PcmContext>,
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
        let fw =
            sys::framework().map_err(|e| Error::other(format!("AudioToolbox unavailable: {e}")))?;

        let sr = params.sample_rate.unwrap_or(48_000);
        let ch = params.channels.unwrap_or(2);

        if !(1..=8).contains(&ch) {
            return Err(Error::unsupported(format!(
                "Opus encoder: unsupported channel count {ch}"
            )));
        }
        if !is_supported_input_rate(sr) {
            return Err(Error::unsupported(format!(
                "Opus encoder: input rate {sr} Hz not supported (RFC 6716 §2.1.1: 8/12/16/24/48 kHz)"
            )));
        }
        // S16 only. RFC 6716 doesn't restrict the input width, but AT's
        // Opus encoder slot specifically accepts S16 packed; the
        // simpler invariant matches every other AT bridge with no
        // visible quality loss for the lossy CELT/SILK path.
        match params.sample_format.unwrap_or(SampleFormat::S16) {
            SampleFormat::S16 => {}
            other => {
                return Err(Error::unsupported(format!(
                    "Opus encoder: only S16 input supported (got {other:?})"
                )))
            }
        }

        let bytes_per_frame = 2u32 * ch as u32;
        let in_asbd = AudioStreamBasicDescription::pcm_s16(sr as f64, ch as u32);
        let frames_per_packet = default_frames_per_packet(sr);
        let out_asbd = AudioStreamBasicDescription::opus(sr as f64, ch as u32, frames_per_packet);

        let mut converter: AudioConverterRef = std::ptr::null_mut();
        let status = unsafe { sys::audio_converter_new(fw, &in_asbd, &out_asbd, &mut converter) };
        if status != NO_ERR {
            return Err(Error::other(format!(
                "AudioConverterNew (Opus enc) failed: OSStatus {status}"
            )));
        }

        // Optional bitrate override via params.bit_rate. AT validates
        // the value against the codec's accepted range internally.
        if let Some(br) = params.bit_rate {
            if br > 0 {
                let v = br as u32;
                let _ = unsafe {
                    sys::audio_converter_set_property(
                        fw,
                        converter,
                        K_AUDIO_CONVERTER_ENCODE_BIT_RATE,
                        std::mem::size_of::<u32>() as u32,
                        &v as *const u32 as *const c_void,
                    )
                };
            }
        }

        // Query the maximum output packet size. Per RFC 6716 §3.2.1 an
        // individual Opus frame maxes at 1275 bytes; AT typically
        // reports 1276 (one byte of header / framing slack). Use that
        // as the per-packet output buffer floor, plus a generous safety
        // pad to cover code-3 packets that pack multiple frames.
        let mut max_pkt: u32 = 4096;
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

        // Read the encoder-vended magic cookie. AT returns a 28-byte
        // payload (see opus::AtCompressionCookie). On older slots that
        // fail the property query we fall back to a body-form OpusHead
        // from (sample_rate, channels).
        let cookie = read_compression_cookie(fw, converter).unwrap_or_else(|_| {
            let head = OpusHead {
                version: 1,
                channel_count: ch as u8,
                pre_skip: 0,
                input_sample_rate: sr,
                output_gain: 0,
                channel_mapping_family: 0,
                mapping_table: Vec::new(),
            };
            head.to_body_bytes()
        });

        let mut out_params = CodecParameters::audio(params.codec_id.clone());
        out_params.sample_rate = Some(sr);
        out_params.channels = Some(ch);
        out_params.sample_format = Some(SampleFormat::S16);
        out_params.extradata = cookie;
        if let Some(br) = params.bit_rate {
            out_params.bit_rate = Some(br);
        }

        Ok(Self {
            codec_id: params.codec_id.clone(),
            converter,
            channels: ch,
            bytes_per_frame,
            frames_per_packet,
            feeder: Box::new(PcmContext {
                queue: Vec::with_capacity((frames_per_packet * bytes_per_frame * 4) as usize),
                read_pos: 0,
                bytes_per_frame,
            }),
            max_packet_bytes: max_pkt.max(1280),
            pending: Vec::new(),
            out_params,
            pts: 0,
            time_base: TimeBase::new(1, sr as i64),
            eof: false,
        })
    }

    /// Drain whatever PCM is currently buffered, one
    /// `frames_per_packet`-sized packet at a time. `final_flush == true`
    /// drops the slack and accepts AT's EOF poison after the last
    /// packet.
    fn drain_pcm(&mut self, final_flush: bool) -> Result<()> {
        let packet_pcm_bytes = (self.frames_per_packet * self.bytes_per_frame) as usize;
        let slack = if final_flush { 0 } else { packet_pcm_bytes };

        while self.feeder.remaining() >= packet_pcm_bytes + slack {
            self.pump_one_packet()?;
        }
        if final_flush && self.feeder.remaining() > 0 {
            self.pump_one_packet()?;
        }
        Ok(())
    }

    /// Single `FillComplexBuffer` call asking for one Opus packet.
    fn pump_one_packet(&mut self) -> Result<()> {
        let fw =
            sys::framework().map_err(|e| Error::other(format!("AudioToolbox unavailable: {e}")))?;

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
            return Err(Error::other(format!(
                "AudioConverterFillComplexBuffer (Opus enc) failed: OSStatus {status}"
            )));
        }

        let raw_len = abl.buffers[0].data_byte_size as usize;
        if raw_len == 0 || output_packet_count == 0 {
            return Ok(());
        }
        out_buf.truncate(raw_len);

        let pkt = Packet::new(0, self.time_base, out_buf)
            .with_pts(self.pts)
            .with_keyframe(true);
        // Opus packets at the canonical 20 ms / 960-frame size advance
        // PTS by one packet's worth of PCM samples per emitted packet.
        self.pts += self.frames_per_packet as i64;
        self.pending.push(pkt);
        Ok(())
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

/// True when `sr` is one of the RFC 6716 §2.1.1 encoder input rates.
fn is_supported_input_rate(sr: u32) -> bool {
    matches!(sr, 8_000 | 12_000 | 16_000 | 24_000 | 48_000)
}

/// Read the encoder-vended cookie (size query + value fetch).
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
        return Err(Error::other(format!(
            "GetPropertyInfo(CompressionMagicCookie / Opus) failed: OSStatus {status}, size {size}"
        )));
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
        return Err(Error::other(format!(
            "GetProperty(CompressionMagicCookie / Opus) failed: OSStatus {status}"
        )));
    }
    buf.truncate(io_size as usize);
    Ok(buf)
}

/// PCM input callback for Opus encode.
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
    fn make_encoder_succeeds_stereo_48k() {
        let r = make_encoder(&params_opus_48k_stereo());
        assert!(r.is_ok(), "make_encoder failed: {:?}", r.err());
    }

    #[test]
    fn make_encoder_succeeds_mono_48k() {
        let mut p = params_opus_48k_stereo();
        p.channels = Some(1);
        let r = make_encoder(&p);
        assert!(r.is_ok(), "mono make_encoder failed: {:?}", r.err());
    }

    #[test]
    fn make_encoder_accepts_24khz_input() {
        let mut p = params_opus_48k_stereo();
        p.sample_rate = Some(24_000);
        let r = make_encoder(&p);
        assert!(r.is_ok(), "24 kHz input should be accepted");
    }

    #[test]
    fn make_encoder_rejects_44100() {
        let mut p = params_opus_48k_stereo();
        p.sample_rate = Some(44_100);
        let r = make_encoder(&p);
        assert!(r.is_err(), "44.1 kHz input must be rejected");
    }

    #[test]
    fn make_encoder_rejects_unsupported_sample_format() {
        let mut p = params_opus_48k_stereo();
        p.sample_format = Some(SampleFormat::F32);
        let r = make_encoder(&p);
        assert!(
            r.is_err(),
            "F32 input must be rejected (only S16 supported)"
        );
    }

    #[test]
    fn make_encoder_rejects_too_many_channels() {
        let mut p = params_opus_48k_stereo();
        p.channels = Some(16);
        let r = make_encoder(&p);
        assert!(r.is_err(), "16 channels must be rejected (cap is 8)");
    }

    #[test]
    fn encoder_publishes_at_compression_cookie() {
        let enc = make_encoder(&params_opus_48k_stereo()).expect("make_encoder");
        let cookie = &enc.output_params().extradata;
        assert!(!cookie.is_empty(), "cookie must not be empty");
        // Either the 28-byte AT cookie or the 11-byte body-form OpusHead
        // fallback should be present. Both round-trip through the
        // decoder side.
        let is_at_cookie = cookie.len() == opus::AT_COMPRESSION_COOKIE_LEN
            && opus::parse_at_compression_cookie(cookie).is_some();
        let is_opushead = OpusHead::parse(cookie).is_some();
        assert!(
            is_at_cookie || is_opushead,
            "cookie must be either AT compression cookie or OpusHead (got {} bytes)",
            cookie.len()
        );
    }

    #[test]
    fn at_cookie_carries_configured_rate_and_channels() {
        let enc = make_encoder(&params_opus_48k_stereo()).expect("make_encoder");
        let cookie = &enc.output_params().extradata;
        if let Some(c) = opus::parse_at_compression_cookie(cookie) {
            assert_eq!(c.sample_rate, 48_000);
            assert_eq!(c.channel_count, 2);
            assert_eq!(c.frames_per_packet, default_frames_per_packet(48_000));
        }
    }

    #[test]
    fn output_params_echo_input_format() {
        let enc = make_encoder(&params_opus_48k_stereo()).expect("make_encoder");
        let op = enc.output_params();
        assert_eq!(op.sample_rate, Some(48_000));
        assert_eq!(op.channels, Some(2));
        assert_eq!(op.sample_format, Some(SampleFormat::S16));
    }

    #[test]
    fn bit_rate_override_echoes_through_output_params() {
        let mut p = params_opus_48k_stereo();
        p.bit_rate = Some(96_000);
        let enc = make_encoder(&p).expect("make_encoder");
        assert_eq!(enc.output_params().bit_rate, Some(96_000));
    }

    #[test]
    fn supported_input_rates_table() {
        for r in [8_000, 12_000, 16_000, 24_000, 48_000] {
            assert!(is_supported_input_rate(r), "{r} must be supported");
        }
        for r in [11_025, 22_050, 32_000, 44_100, 88_200, 96_000] {
            assert!(!is_supported_input_rate(r), "{r} must be rejected");
        }
    }
}
