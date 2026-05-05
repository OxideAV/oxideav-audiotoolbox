//! AAC LC encoder backed by macOS AudioConverter.
//!
//! Input:  interleaved IEEE float-32 or S16 PCM in `AudioFrame`.
//! Output: ADTS-framed AAC packets (7-byte header synthesised from the
//!         configured ASBD + raw AAC payload from AudioConverter).
//!
//! Default bitrate: 128 kbit/s.  Overridable via `CodecParameters::bit_rate`.

use std::ffi::c_void;

use oxideav_core::Encoder;
use oxideav_core::{
    CodecId, CodecParameters, Error, Frame, Packet, Result, SampleFormat, TimeBase,
};

use crate::adts;
use crate::sys::{
    self, AudioBuffer, AudioBufferList1, AudioConverterRef, AudioStreamBasicDescription,
    AudioStreamPacketDescription, K_AUDIO_CONVERTER_ENCODE_BIT_RATE,
    K_AUDIO_CONVERTER_MAX_OUTPUT_PACKET_SIZE, NO_ERR,
};

/// Default AAC bitrate when the caller does not specify one.
const DEFAULT_BITRATE_BPS: u32 = 128_000;

/// Maximum raw AAC packet that AudioConverter can emit (before ADTS header).
/// AudioConverter will tell us the real maximum via a property query, but
/// this upper bound lets us allocate before querying.
const MAX_PACKET_BYTES: usize = 8192;

/// State handed to the AudioConverter input callback.
struct PcmContext {
    /// Pointer to the PCM interleaved byte buffer.
    data: *const u8,
    /// Total byte length of `data`.
    len: u32,
    /// Bytes per packet from the input ASBD (= bytes_per_frame for PCM).
    bytes_per_packet: u32,
    /// Already consumed — flag to return "no data" on second call.
    consumed: bool,
}

/// AudioConverter-backed AAC encoder.
pub struct AacAtEncoder {
    codec_id: CodecId,
    /// AudioConverter handle.
    converter: AudioConverterRef,
    /// Channels (1 or 2 typically).
    channels: u16,
    /// Bytes per PCM frame (= channels × bytes_per_sample).
    bytes_per_frame: u32,
    /// ADTS sampling-frequency index (0..=12).
    sf_index: u8,
    /// ADTS channel configuration (1..=7).
    channel_config: u8,
    /// Max raw-AAC bytes per packet from AudioConverter property query.
    max_packet_bytes: u32,
    /// Queued encoded packet ready to return from `receive_packet`.
    pending: Option<Packet>,
    /// Output codec parameters.
    out_params: CodecParameters,
    /// PTS counter (sample-level).
    pts: i64,
    /// TimeBase for PTS.
    time_base: TimeBase,
    /// Set after `flush()`.
    eof: bool,
}

// SAFETY: same justification as AacAtDecoder.
unsafe impl Send for AacAtEncoder {}

impl AacAtEncoder {
    fn new(params: &CodecParameters) -> Result<Self> {
        let fw =
            sys::framework().map_err(|e| Error::other(format!("AudioToolbox unavailable: {e}")))?;

        let sr = params.sample_rate.unwrap_or(48_000);
        let ch = params.channels.unwrap_or(2) as u32;
        let bitrate = params.bit_rate.unwrap_or(DEFAULT_BITRATE_BPS as u64) as u32;

        let sf_index = adts::sample_rate_index(sr).ok_or_else(|| {
            Error::unsupported(format!("AacAtEncoder: unsupported sample rate {sr}"))
        })?;

        // channel_config for ADTS: 1-6 direct, 7 = 8ch.
        let channel_config = match ch {
            1..=6 => ch as u8,
            8 => 7,
            _ => {
                return Err(Error::unsupported(format!(
                    "AacAtEncoder: unsupported channel count {ch}"
                )))
            }
        };

        let sample_format = params.sample_format.unwrap_or(SampleFormat::F32);

        let (in_asbd, bps) = match sample_format {
            SampleFormat::F32 => {
                let bps = 4u32 * ch;
                (AudioStreamBasicDescription::pcm_float32(sr as f64, ch), bps)
            }
            SampleFormat::S16 => {
                let bps = 2u32 * ch;
                (AudioStreamBasicDescription::pcm_s16(sr as f64, ch), bps)
            }
            _ => {
                return Err(Error::unsupported(
                    "AacAtEncoder: only F32 and S16 input supported",
                ))
            }
        };

        let out_asbd = AudioStreamBasicDescription::mpeg4_aac(sr as f64, ch);

        let mut converter: AudioConverterRef = std::ptr::null_mut();
        let status = unsafe { sys::audio_converter_new(fw, &in_asbd, &out_asbd, &mut converter) };
        if status != NO_ERR {
            return Err(Error::other(format!(
                "AudioConverterNew (encoder) failed: OSStatus {status}"
            )));
        }

        // Set target bitrate.
        let status = unsafe {
            sys::audio_converter_set_property(
                fw,
                converter,
                K_AUDIO_CONVERTER_ENCODE_BIT_RATE,
                std::mem::size_of::<u32>() as u32,
                &bitrate as *const u32 as *const c_void,
            )
        };
        if status != NO_ERR {
            // Non-fatal: Apple's software encoder may round the bitrate.
            eprintln!("audiotoolbox: AudioConverterSetProperty(bitrate) = {status} (ignoring)");
        }

        // Query max output packet size.
        let mut max_pkt: u32 = MAX_PACKET_BYTES as u32;
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

        let time_base = TimeBase::new(1, sr as i64);
        let out_params = CodecParameters::audio(params.codec_id.clone());

        Ok(Self {
            codec_id: params.codec_id.clone(),
            converter,
            channels: ch as u16,
            bytes_per_frame: bps,
            sf_index,
            channel_config,
            max_packet_bytes: max_pkt.max(256),
            pending: None,
            out_params,
            pts: 0,
            time_base,
            eof: false,
        })
    }

    /// Encode one interleaved PCM frame and store the ADTS packet.
    fn encode_frame_inner(&mut self, pcm: &[u8]) -> Result<()> {
        let fw =
            sys::framework().map_err(|e| Error::other(format!("AudioToolbox unavailable: {e}")))?;

        // Allocate output buffer.
        let out_size = self.max_packet_bytes as usize;
        let mut out_buf = vec![0u8; out_size + 7]; // +7 for ADTS header
        let raw_aac_ptr = out_buf[7..].as_mut_ptr(); // we'll fill bytes 0-6 after

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
                data: raw_aac_ptr,
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
                "AudioConverterFillComplexBuffer (encoder) failed: OSStatus {status}"
            )));
        }

        let raw_len = abl.buffers[0].data_byte_size as usize;
        if raw_len == 0 {
            // Converter buffered the input; no output yet.
            return Ok(());
        }

        // Synthesise ADTS header (7 bytes, no CRC, AAC-LC = profile 1).
        let hdr = adts::build_header(raw_len, self.sf_index, self.channel_config, 1);
        out_buf[..7].copy_from_slice(&hdr);
        let total = 7 + raw_len;
        out_buf.truncate(total);

        let samples = 1024i64; // AAC LC always 1024 samples per frame
        let pkt = Packet::new(0, self.time_base, out_buf)
            .with_pts(self.pts)
            .with_keyframe(true);
        self.pts += samples;
        self.pending = Some(pkt);
        Ok(())
    }
}

impl Drop for AacAtEncoder {
    fn drop(&mut self) {
        if !self.converter.is_null() {
            if let Ok(fw) = sys::framework() {
                unsafe {
                    let _ = sys::audio_converter_dispose(fw, self.converter);
                };
            }
            self.converter = std::ptr::null_mut();
        }
    }
}

impl Encoder for AacAtEncoder {
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
                // We expect interleaved PCM in a single plane.
                let pcm = &af.data[0];
                self.encode_frame_inner(pcm)
            }
            _ => Err(Error::unsupported("AacAtEncoder only accepts Audio frames")),
        }
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if let Some(p) = self.pending.take() {
            return Ok(p);
        }
        if self.eof {
            return Err(Error::Eof);
        }
        Err(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }
}

/// Input callback for the encoder: supplies interleaved PCM to AudioConverter.
///
/// # Safety
/// `in_user_data` must point to a valid `PcmContext` for the duration of the
/// `FillComplexBuffer` call.
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

    // Compute how many PCM packets (frames) we can supply.
    let n_packets = (ctx.len / ctx.bytes_per_packet).max(1);
    *io_number_data_packets = n_packets;
    (*io_data).number_buffers = 1;
    (*io_data).buffers[0].data_byte_size = n_packets * ctx.bytes_per_packet;
    (*io_data).buffers[0].data = ctx.data as *mut u8;

    ctx.consumed = true;
    0 // noErr
}

/// Factory function registered with the codec registry.
pub fn make_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    Ok(Box::new(AacAtEncoder::new(params)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{CodecId, CodecParameters};

    fn params_48k_stereo() -> CodecParameters {
        let mut p = CodecParameters::audio(CodecId::new("aac"));
        p.sample_rate = Some(48_000);
        p.channels = Some(2);
        p.sample_format = Some(SampleFormat::F32);
        p.bit_rate = Some(128_000);
        p
    }

    #[test]
    fn make_encoder_succeeds() {
        let r = make_encoder(&params_48k_stereo());
        assert!(r.is_ok(), "make_encoder failed: {:?}", r.err());
    }
}
