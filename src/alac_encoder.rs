//! Apple Lossless (ALAC) encoder backed by macOS AudioConverter.
//!
//! Input:  interleaved S16 (or S32) PCM in `AudioFrame::data[0]`.
//! Output: one raw ALAC packet per encoded frame (no container framing).
//!         The encoder-vended **magic cookie** is exposed via
//!         `output_params.extradata` so a downstream muxer (mov / m4a /
//!         caf) can write a working ALAC track.
//!
//! Each output packet covers exactly `frame_length` (default 4096) PCM
//! frames; the encoder buffers partial frames internally so callers can
//! pass arbitrary chunk sizes.

use std::ffi::c_void;

use oxideav_core::Encoder;
use oxideav_core::{
    CodecId, CodecParameters, Error, Frame, Packet, Result, SampleFormat, TimeBase,
};

use crate::alac::{self, AlacSpecificConfig, DEFAULT_FRAME_LENGTH};
use crate::sys::{
    self, AudioBuffer, AudioBufferList1, AudioConverterRef, AudioStreamBasicDescription,
    AudioStreamPacketDescription, K_AUDIO_CONVERTER_COMPRESSION_MAGIC_COOKIE,
    K_AUDIO_CONVERTER_MAX_OUTPUT_PACKET_SIZE, NO_ERR,
};

/// State handed to the input callback.
struct PcmContext {
    data: *const u8,
    len: u32,
    bytes_per_packet: u32,
    consumed: bool,
}

/// AudioConverter-backed ALAC encoder.
pub struct AlacAtEncoder {
    codec_id: CodecId,
    converter: AudioConverterRef,
    channels: u16,
    #[allow(dead_code)]
    bit_depth: u8,
    bytes_per_frame: u32,
    /// Encoder packet size (typically 4096 PCM frames).
    frame_length: u32,
    /// Internal PCM staging buffer — we only emit a packet once we have
    /// `frame_length` × `bytes_per_frame` bytes pending.
    staging: Vec<u8>,
    /// Per-packet max raw ALAC bytes from AudioConverter property query.
    max_packet_bytes: u32,
    pending: Vec<Packet>,
    out_params: CodecParameters,
    pts: i64,
    time_base: TimeBase,
    eof: bool,
}

// SAFETY: same justification as AacAtEncoder — handle is used from a
// single thread per call.
unsafe impl Send for AlacAtEncoder {}

impl AlacAtEncoder {
    fn new(params: &CodecParameters) -> Result<Self> {
        let fw =
            sys::framework().map_err(|e| Error::other(format!("AudioToolbox unavailable: {e}")))?;

        let sr = params.sample_rate.unwrap_or(48_000);
        let ch = params.channels.unwrap_or(2);

        if !(1..=8).contains(&ch) {
            return Err(Error::unsupported(format!(
                "ALAC encoder: unsupported channel count {ch}"
            )));
        }

        let sample_format = params.sample_format.unwrap_or(SampleFormat::S16);
        let (in_asbd, bit_depth, bps) = match sample_format {
            SampleFormat::S16 => (
                AudioStreamBasicDescription::pcm_s16(sr as f64, ch as u32),
                16u8,
                2u32 * ch as u32,
            ),
            SampleFormat::S32 => {
                // 32-bit PCM in, 32-bit ALAC out.
                let asbd = AudioStreamBasicDescription {
                    sample_rate: sr as f64,
                    format_id: sys::K_AUDIO_FORMAT_LINEAR_PCM,
                    format_flags: sys::K_AF_FLAG_IS_SIGNED_INTEGER | sys::K_AF_FLAG_IS_PACKED,
                    bytes_per_packet: 4 * ch as u32,
                    frames_per_packet: 1,
                    bytes_per_frame: 4 * ch as u32,
                    channels_per_frame: ch as u32,
                    bits_per_channel: 32,
                    reserved: 0,
                };
                (asbd, 32u8, 4u32 * ch as u32)
            }
            other => {
                return Err(Error::unsupported(format!(
                    "ALAC encoder: only S16 / S32 input supported (got {other:?})"
                )))
            }
        };

        let bit_depth_flag = alac::bit_depth_flag(bit_depth).ok_or_else(|| {
            Error::unsupported(format!("ALAC encoder: unsupported bit_depth {bit_depth}"))
        })?;
        let out_asbd = AudioStreamBasicDescription::apple_lossless(
            sr as f64,
            ch as u32,
            bit_depth_flag,
            DEFAULT_FRAME_LENGTH,
        );

        let mut converter: AudioConverterRef = std::ptr::null_mut();
        let status = unsafe { sys::audio_converter_new(fw, &in_asbd, &out_asbd, &mut converter) };
        if status != NO_ERR {
            return Err(Error::other(format!(
                "AudioConverterNew (ALAC enc) failed: OSStatus {status}"
            )));
        }

        // Query maximum output packet size so we can size our buffer correctly.
        let mut max_pkt: u32 = DEFAULT_FRAME_LENGTH * ch as u32 * 4; // generous upper bound
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

        // Read back the encoder-vended magic cookie. AT may return more
        // than 24 bytes (channel-layout-info appended or a wrapper
        // version); we forward the bytes verbatim so callers can mux it.
        let cookie = read_compression_cookie(fw, converter).unwrap_or_else(|_| {
            // Fall back to a synthesised cookie if the property query
            // fails — better to ship a working stream than to abort.
            AlacSpecificConfig::new(sr, ch as u8, bit_depth)
                .to_bytes()
                .to_vec()
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
            bytes_per_frame: bps,
            frame_length: DEFAULT_FRAME_LENGTH,
            staging: Vec::with_capacity((DEFAULT_FRAME_LENGTH * bps) as usize),
            max_packet_bytes: max_pkt.max(256),
            pending: Vec::new(),
            out_params,
            pts: 0,
            time_base: TimeBase::new(1, sr as i64),
            eof: false,
        })
    }

    /// Drain any complete `frame_length`-sized PCM chunks from the
    /// staging buffer, encoding one ALAC packet per chunk.
    fn drain_staging(&mut self) -> Result<()> {
        let packet_bytes = (self.frame_length * self.bytes_per_frame) as usize;
        while self.staging.len() >= packet_bytes {
            // Take the front packet's worth.
            let chunk: Vec<u8> = self.staging.drain(..packet_bytes).collect();
            self.encode_one_packet(&chunk, self.frame_length as i64)?;
        }
        Ok(())
    }

    /// Encode any leftover < `frame_length` chunk as a final short packet.
    /// Apple's ALAC encoder accepts a final smaller packet; the cookie
    /// declares the *typical* packet size and the per-packet descriptor
    /// carries `variable_frames_in_packet` for the short tail.
    fn drain_tail(&mut self) -> Result<()> {
        if self.staging.is_empty() {
            return Ok(());
        }
        let chunk: Vec<u8> = std::mem::take(&mut self.staging);
        let frames = (chunk.len() as u32 / self.bytes_per_frame) as i64;
        if frames > 0 {
            self.encode_one_packet(&chunk, frames)?;
        }
        Ok(())
    }

    fn encode_one_packet(&mut self, pcm: &[u8], sample_count: i64) -> Result<()> {
        let fw =
            sys::framework().map_err(|e| Error::other(format!("AudioToolbox unavailable: {e}")))?;

        let out_size = self.max_packet_bytes as usize;
        let mut out_buf = vec![0u8; out_size];

        let mut ctx = PcmContext {
            data: pcm.as_ptr(),
            len: pcm.len() as u32,
            bytes_per_packet: self.bytes_per_frame,
            consumed: false,
        };

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

        let status = unsafe {
            sys::audio_converter_fill_complex_buffer(
                fw,
                self.converter,
                pcm_input_callback,
                &mut ctx as *mut PcmContext as *mut c_void,
                &mut output_packet_count,
                &mut abl,
                &mut pkt_desc,
            )
        };
        if status != NO_ERR {
            return Err(Error::other(format!(
                "AudioConverterFillComplexBuffer (ALAC enc) failed: OSStatus {status}"
            )));
        }

        let raw_len = abl.buffers[0].data_byte_size as usize;
        if raw_len == 0 {
            return Ok(());
        }
        out_buf.truncate(raw_len);

        let pkt = Packet::new(0, self.time_base, out_buf)
            .with_pts(self.pts)
            .with_keyframe(true);
        self.pts += sample_count;
        self.pending.push(pkt);
        Ok(())
    }
}

impl Drop for AlacAtEncoder {
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

impl Encoder for AlacAtEncoder {
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
                self.staging.extend_from_slice(&af.data[0]);
                self.drain_staging()
            }
            _ => Err(Error::unsupported(
                "AlacAtEncoder only accepts Audio frames",
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
        self.drain_tail()?;
        self.eof = true;
        Ok(())
    }
}

/// Read the encoder-vended magic cookie via two property calls (size
/// query + value fetch). Returns the cookie bytes verbatim — typically
/// 24 bytes (mandatory `ALACSpecificConfig` only) or 48 bytes
/// (`ALACSpecificConfig + ALACChannelLayoutInfo`).
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
            "GetPropertyInfo(CompressionMagicCookie) failed: OSStatus {status}, size {size}"
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
            "GetProperty(CompressionMagicCookie) failed: OSStatus {status}"
        )));
    }
    buf.truncate(io_size as usize);
    Ok(buf)
}

/// PCM input callback for ALAC encode.
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
    if ctx.consumed || ctx.len == 0 {
        *io_number_data_packets = 0;
        (*io_data).buffers[0].data_byte_size = 0;
        (*io_data).buffers[0].data = std::ptr::null_mut();
        return 0;
    }

    let n_packets = (ctx.len / ctx.bytes_per_packet).max(1);
    *io_number_data_packets = n_packets;
    (*io_data).number_buffers = 1;
    (*io_data).buffers[0].data_byte_size = n_packets * ctx.bytes_per_packet;
    (*io_data).buffers[0].data = ctx.data as *mut u8;

    ctx.consumed = true;
    0
}

/// Factory function registered with the codec registry.
pub fn make_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    Ok(Box::new(AlacAtEncoder::new(params)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{CodecId, CodecParameters};

    fn params_alac_48k_stereo() -> CodecParameters {
        let mut p = CodecParameters::audio(CodecId::new("alac"));
        p.sample_rate = Some(48_000);
        p.channels = Some(2);
        p.sample_format = Some(SampleFormat::S16);
        p
    }

    #[test]
    fn make_encoder_succeeds() {
        let r = make_encoder(&params_alac_48k_stereo());
        assert!(r.is_ok(), "make_encoder failed: {:?}", r.err());
    }

    #[test]
    fn encoder_publishes_magic_cookie() {
        let enc = make_encoder(&params_alac_48k_stereo()).expect("make_encoder");
        let cookie = &enc.output_params().extradata;
        assert!(
            cookie.len() >= alac::SPECIFIC_CONFIG_LEN,
            "cookie too short: {} bytes",
            cookie.len()
        );
        // First 4 bytes = frame_length, big-endian. Apple's encoder
        // defaults to 4096 — but the wrapper format some AT versions
        // use puts an ISO box header at the front. Either way, the
        // mandatory ALACSpecificConfig portion is present somewhere
        // in the blob. Spot-check that the cookie isn't all-zero.
        assert!(
            cookie.iter().any(|&b| b != 0),
            "cookie is all zero — encoder did not vend one"
        );
    }
}
