//! AAC LC decoder backed by macOS AudioConverter.
//!
//! Input:  ADTS-framed AAC packets (7-byte header + raw AAC payload).
//! Output: interleaved IEEE float-32 PCM, one `AudioFrame` per 1024 AAC
//!         samples.
//!
//! The converter is configured lazily on the first packet: the ADTS header
//! supplies the sample rate, channel count, and object type needed to build
//! the input `AudioStreamBasicDescription`.

use std::ffi::c_void;

use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, Result, TimeBase};
use oxideav_core::Decoder;

use crate::adts;
use crate::adts::SAMPLE_RATES;
use crate::sys::{
    self, AudioBuffer, AudioBufferList1, AudioStreamBasicDescription, AudioStreamPacketDescription,
    AudioConverterRef, NO_ERR,
};

/// State shared with the AudioConverter input callback.
struct InputContext {
    /// Borrowed slice for the current `send_packet` call's raw AAC payload.
    data: *const u8,
    len: u32,
    /// Descriptor for the single input packet (compressed data).
    packet_desc: AudioStreamPacketDescription,
    /// Set to true once the callback has been called once — subsequent calls
    /// within the same `FillComplexBuffer` request return "no data".
    consumed: bool,
}

/// AudioConverter-backed AAC decoder.
pub struct AacAtDecoder {
    codec_id: CodecId,
    /// Configured sample rate (from first ADTS packet).
    sample_rate: u32,
    /// Channel count.
    channels: u16,
    /// Opaque converter handle, null until the first packet arrives.
    converter: AudioConverterRef,
    /// Queued output frame ready to be returned by `receive_frame`.
    pending: Option<Frame>,
    /// Tracks how many PCM frames we have emitted (for PTS).
    pts: i64,
    /// TimeBase for PTS arithmetic.
    time_base: TimeBase,
    /// Set after `flush()`.
    eof: bool,
    /// Whether `converter` has been initialised.
    configured: bool,
}

// SAFETY: AudioConverterRef is used single-threaded inside the Decoder impl.
// The `*mut` inside it is the opaque handle Apple guarantees is usable from
// the thread it was created on; we never move the handle across threads.
unsafe impl Send for AacAtDecoder {}

impl AacAtDecoder {
    fn new(params: &CodecParameters) -> Self {
        let sr = params.sample_rate.unwrap_or(44_100);
        let ch = params.channels.unwrap_or(2);
        Self {
            codec_id: params.codec_id.clone(),
            sample_rate: sr,
            channels: ch,
            converter: std::ptr::null_mut(),
            pending: None,
            pts: 0,
            time_base: TimeBase::new(1, sr as i64),
            eof: false,
            configured: false,
        }
    }

    /// Initialise the AudioConverter from an ADTS header.
    fn configure(&mut self, hdr: &adts::AdtsHeader) -> Result<()> {
        let fw = sys::framework()
            .map_err(|e| Error::other(format!("AudioToolbox unavailable: {e}")))?;

        // Map ADTS sample-rate index → Hz.
        let sr = SAMPLE_RATES
            .get(hdr.sampling_freq_index as usize)
            .copied()
            .ok_or_else(|| Error::invalid("ADTS: unknown sampling-frequency index"))?;

        let ch = match hdr.channel_configuration {
            1..=6 => hdr.channel_configuration as u32,
            7 => 8,
            _ => return Err(Error::invalid("ADTS: unsupported channel configuration")),
        };

        self.sample_rate = sr;
        self.channels = ch as u16;
        self.time_base = TimeBase::new(1, sr as i64);

        let in_asbd = AudioStreamBasicDescription::mpeg4_aac(sr as f64, ch);
        let out_asbd = AudioStreamBasicDescription::pcm_float32(sr as f64, ch);

        let mut converter: AudioConverterRef = std::ptr::null_mut();
        let status = unsafe {
            sys::audio_converter_new(
                fw,
                &in_asbd,
                &out_asbd,
                &mut converter,
            )
        };
        if status != NO_ERR {
            return Err(Error::other(format!(
                "AudioConverterNew failed: OSStatus {status}"
            )));
        }

        self.converter = converter;
        self.configured = true;
        Ok(())
    }

    /// Decode one ADTS frame and store the result in `self.pending`.
    fn decode_packet(&mut self, data: &[u8]) -> Result<()> {
        let fw = sys::framework()
            .map_err(|e| Error::other(format!("AudioToolbox unavailable: {e}")))?;
        let _ = fw; // borrow check — actually used in configure() and the unsafe call below

        let hdr = adts::parse(data).ok_or_else(|| Error::invalid("AT decoder: bad ADTS sync"))?;
        if hdr.frame_length > data.len() {
            return Err(Error::NeedMore);
        }

        // Configure on first packet.
        if !self.configured {
            self.configure(&hdr)?;
        }

        let header_len = hdr.header_len();
        let raw_aac = &data[header_len..hdr.frame_length];

        // PCM output buffer: 1024 samples × channels × 4 bytes.
        let channels = self.channels as usize;
        let sample_count = 1024usize;
        let buf_size = sample_count * channels * 4;
        let mut pcm_buf = vec![0u8; buf_size];

        // We pass a mutable reference to InputContext through a raw pointer as
        // the user-data argument to AudioConverter — this is the canonical
        // CoreAudio pattern for feeding compressed data from within the
        // callback.
        let mut ctx = InputContext {
            data: raw_aac.as_ptr(),
            len: raw_aac.len() as u32,
            packet_desc: AudioStreamPacketDescription {
                start_offset: 0,
                variable_frames_in_packet: 0,
                data_byte_size: raw_aac.len() as u32,
            },
            consumed: false,
        };

        // Request exactly 1024 PCM frames (one AAC-LC frame's worth).
        // For PCM output each "packet" = 1 frame (frames_per_packet=1 in PCM ASBD).
        let mut output_packet_count: u32 = sample_count as u32;
        let mut abl = AudioBufferList1 {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: channels as u32,
                data_byte_size: buf_size as u32,
                data: pcm_buf.as_mut_ptr(),
            }],
        };

        let fw = sys::framework()
            .map_err(|e| Error::other(format!("AudioToolbox unavailable: {e}")))?;

        let status = unsafe {
            sys::audio_converter_fill_complex_buffer(
                fw,
                self.converter,
                aac_input_callback,
                &mut ctx as *mut InputContext as *mut c_void,
                &mut output_packet_count,
                &mut abl,
                std::ptr::null_mut(),
            )
        };

        if status != NO_ERR && status != 1 {
            // status 1 can mean "more data needed" in some AT versions; treat
            // non-zero as a soft error on decode rather than a hard failure.
            return Err(Error::other(format!(
                "AudioConverterFillComplexBuffer failed: OSStatus {status}"
            )));
        }

        let actual_bytes = abl.buffers[0].data_byte_size as usize;
        if actual_bytes == 0 {
            return Ok(());
        }

        let actual_samples = actual_bytes / (channels * 4);

        let frame = AudioFrame {
            samples: actual_samples as u32,
            pts: Some(self.pts),
            data: vec![pcm_buf[..actual_bytes].to_vec()],
        };
        self.pts += actual_samples as i64;
        self.pending = Some(Frame::Audio(frame));
        Ok(())
    }
}

impl Drop for AacAtDecoder {
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

impl Decoder for AacAtDecoder {
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
        if let Some(f) = self.pending.take() {
            return Ok(f);
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

    fn reset(&mut self) -> Result<()> {
        self.pending = None;
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

/// The input-data callback.  Called by `AudioConverterFillComplexBuffer`
/// when it wants more compressed input packets.
///
/// # Safety
/// `in_user_data` must point to a valid `InputContext`.  The buffer pointer
/// inside `io_data` is set to our raw-AAC slice — it is borrowed from the
/// packet that lives on the Rust call stack for the duration of the
/// `FillComplexBuffer` call, so the lifetime is safe.
unsafe extern "C" fn aac_input_callback(
    _converter: AudioConverterRef,
    io_number_data_packets: *mut u32,
    io_data: *mut AudioBufferList1,
    out_packet_desc: *mut *mut AudioStreamPacketDescription,
    in_user_data: *mut c_void,
) -> sys::OSStatus {
    let ctx = &mut *(in_user_data as *mut InputContext);
    if ctx.consumed || ctx.len == 0 {
        // Signal "no more data" — caller will stop requesting.
        *io_number_data_packets = 0;
        (*io_data).buffers[0].data_byte_size = 0;
        (*io_data).buffers[0].data = std::ptr::null_mut();
        return 0;
    }

    *io_number_data_packets = 1;
    (*io_data).number_buffers = 1;
    (*io_data).buffers[0].data_byte_size = ctx.len;
    (*io_data).buffers[0].data = ctx.data as *mut u8;
    (*io_data).buffers[0].number_channels = 0; // ignored for compressed

    if !out_packet_desc.is_null() {
        *out_packet_desc = &mut ctx.packet_desc;
    }

    ctx.consumed = true;
    0 // noErr
}

/// Factory function registered with the codec registry.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    // Verify framework is reachable before claiming success.
    sys::framework().map_err(|e| Error::other(format!("AudioToolbox unavailable: {e}")))?;
    Ok(Box::new(AacAtDecoder::new(params)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{CodecId, CodecParameters};

    fn params_audio_48k() -> CodecParameters {
        let mut p = CodecParameters::audio(CodecId::new("aac"));
        p.sample_rate = Some(48_000);
        p.channels = Some(2);
        p
    }

    #[test]
    fn make_decoder_returns_ok_when_framework_loads() {
        let r = make_decoder(&params_audio_48k());
        assert!(r.is_ok(), "make_decoder should succeed: {:?}", r.err());
    }
}
