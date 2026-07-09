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

use oxideav_core::Decoder;
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, Result, TimeBase};

use crate::adts;
use crate::adts::SAMPLE_RATES;
use crate::encoder::AacProfile;
use crate::status::status_error;
use crate::sys::{
    self, AudioBuffer, AudioBufferList1, AudioConverterRef, AudioStreamBasicDescription,
    AudioStreamPacketDescription, K_AUDIO_CONVERTER_DECOMPRESSION_MAGIC_COOKIE, NO_ERR,
};

/// State shared with the AudioConverter input callback.
///
/// Holds an arbitrary queue of compressed AAC packets the callback
/// vends one-at-a-time. Required for HE-AAC: AT's HE-AAC decoder
/// requests multiple input packets per output PCM frame for SBR
/// look-ahead, so the callback must keep packets coming until the
/// queue is drained.
struct InputContext {
    /// Pending raw-AAC packets in FIFO order. The front of the queue
    /// is the next packet AT will see.
    queue: Vec<Vec<u8>>,
    /// Packets handed off to AT during the current `FillComplexBuffer`
    /// call. Kept alive here so AT can still reference the bytes after
    /// the callback returns; dropped only when the caller takes the
    /// context back.
    handed_off: Vec<Vec<u8>>,
    /// Reusable packet descriptor — rewritten each callback invocation.
    packet_desc: AudioStreamPacketDescription,
}

/// AudioConverter-backed AAC decoder.
pub struct AacAtDecoder {
    codec_id: CodecId,
    /// Configured sample rate (from first ADTS packet).
    sample_rate: u32,
    /// Channel count.
    channels: u16,
    /// Magic cookie (AudioSpecificConfig + AOT extension for HE / HE-v2)
    /// to forward to the converter via the
    /// `kAudioConverterDecompressionMagicCookie` property.
    /// Empty for AAC LC (the ADTS header is self-describing).
    cookie: Vec<u8>,
    /// Opaque converter handle, null until the first packet arrives.
    converter: AudioConverterRef,
    /// Queued output frames ready to be returned by `receive_frame`.
    /// Vec because one `send_packet` may make AT decide to emit
    /// nothing (look-ahead) or multiple PCM frames (back-pressure).
    pending: Vec<Frame>,
    /// Tracks how many PCM frames we have emitted (for PTS).
    pts: i64,
    /// TimeBase for PTS arithmetic.
    time_base: TimeBase,
    /// Set after `flush()`.
    eof: bool,
    /// Whether `converter` has been initialised.
    configured: bool,
    /// AAC profile (LC / HE / HE-v2). Drives the input ASBD format ID:
    /// HE / HE-v2 use 2048 PCM frames per packet (SBR doubles).
    profile: AacProfile,
    /// Persistent packet queue — AT keeps consuming from this across
    /// many `FillComplexBuffer` calls so we never return "0 packets"
    /// mid-stream (which would put AT into a permanent EOS state).
    input_queue: Vec<Vec<u8>>,
}

// SAFETY: AudioConverterRef is used single-threaded inside the Decoder impl.
// The `*mut` inside it is the opaque handle Apple guarantees is usable from
// the thread it was created on; we never move the handle across threads.
unsafe impl Send for AacAtDecoder {}

impl AacAtDecoder {
    fn new(params: &CodecParameters) -> Self {
        let sr = params.sample_rate.unwrap_or(44_100);
        let ch = params.channels.unwrap_or(2);
        let profile = AacProfile::parse(params.options.get("profile"));
        Self {
            codec_id: params.codec_id.clone(),
            sample_rate: sr,
            channels: ch,
            cookie: params.extradata.clone(),
            converter: std::ptr::null_mut(),
            pending: Vec::new(),
            pts: 0,
            time_base: TimeBase::new(1, sr as i64),
            eof: false,
            configured: false,
            profile,
            input_queue: Vec::new(),
        }
    }

    /// Initialise the AudioConverter from an ADTS header (LC path).
    fn configure_from_adts(&mut self, hdr: &adts::AdtsHeader) -> Result<()> {
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

        self.configure_inner(sr, ch)
    }

    /// Initialise the AudioConverter from the magic cookie + the
    /// pre-supplied `CodecParameters` (HE / HE-v2 path).
    fn configure_from_cookie(&mut self) -> Result<()> {
        // Use the caller-supplied sample_rate / channels — for HE-AAC,
        // these are the BASE rate the AAC LC core uses. The decoder
        // doubles them to derive the output PCM rate.
        let sr = self.sample_rate;
        let ch = self.channels as u32;
        if sr == 0 || ch == 0 {
            return Err(Error::invalid(
                "HE-AAC decoder: sample_rate and channels required when no ADTS header is present",
            ));
        }
        if self.cookie.is_empty() {
            return Err(Error::invalid(
                "HE-AAC decoder: magic cookie (extradata) required to decode HE-AAC streams",
            ));
        }
        self.configure_inner(sr, ch)
    }

    fn configure_inner(&mut self, sr: u32, ch: u32) -> Result<()> {
        let fw = sys::framework()
            .map_err(|e| Error::unsupported(format!("AudioToolbox unavailable: {e}")))?;

        self.sample_rate = sr;
        self.channels = ch as u16;
        self.time_base = TimeBase::new(1, sr as i64);

        // HE / HE-v2: the input ADTS / cookie carries the BASE
        // (pre-SBR) sample rate. The decoder's output rate is 2× that,
        // matching what Apple's HE-AAC decoder produces. For HE-v2
        // stereo with a mono-coded down-mix, the decoder still outputs
        // `ch` channels.
        let (in_asbd, out_sr) = match self.profile {
            AacProfile::Lc => (AudioStreamBasicDescription::mpeg4_aac(sr as f64, ch), sr),
            AacProfile::He => (
                AudioStreamBasicDescription::mpeg4_aac_he((sr * 2) as f64, ch),
                sr * 2,
            ),
            AacProfile::HeV2 => (
                AudioStreamBasicDescription::mpeg4_aac_he_v2((sr * 2) as f64, ch),
                sr * 2,
            ),
            // LD / ELD have no SBR upsample at the converter boundary: the
            // configured rate IS the output rate (512-frame low-delay core).
            AacProfile::Ld => (AudioStreamBasicDescription::mpeg4_aac_ld(sr as f64, ch), sr),
            AacProfile::Eld => (
                AudioStreamBasicDescription::mpeg4_aac_eld(sr as f64, ch),
                sr,
            ),
        };
        let out_asbd = AudioStreamBasicDescription::pcm_float32(out_sr as f64, ch);
        self.sample_rate = out_sr;
        self.time_base = TimeBase::new(1, out_sr as i64);

        let mut converter: AudioConverterRef = std::ptr::null_mut();
        let status = unsafe { sys::audio_converter_new(fw, &in_asbd, &out_asbd, &mut converter) };
        if status != NO_ERR {
            return Err(status_error(
                &format!("AudioConverterNew (profile={:?})", self.profile),
                status,
            ));
        }

        // Forward the magic cookie if the caller supplied one. For HE
        // / HE-v2 this is REQUIRED: the AOT extension descriptor is
        // not present in the ADTS header, so without the cookie AT's
        // decoder treats incoming frames as plain LC and rejects them
        // with `kAudioCodecBadDataError` (1650549857 = 'bada').
        if !self.cookie.is_empty() {
            let status = unsafe {
                sys::audio_converter_set_property(
                    fw,
                    converter,
                    K_AUDIO_CONVERTER_DECOMPRESSION_MAGIC_COOKIE,
                    self.cookie.len() as u32,
                    self.cookie.as_ptr() as *const std::ffi::c_void,
                )
            };
            if status != NO_ERR {
                unsafe {
                    let _ = sys::audio_converter_dispose(fw, converter);
                }
                return Err(status_error(
                    "AudioConverterSetProperty(DecompressionMagicCookie)",
                    status,
                ));
            }
        }

        self.converter = converter;
        self.configured = true;
        Ok(())
    }

    /// Stage one input packet onto the input queue, configure the
    /// converter on the first packet, and try to extract PCM frames.
    ///
    /// Input framing is profile-dependent:
    /// * **LC** — expects an ADTS-framed packet. The 7- or 9-byte
    ///   header is stripped before being queued.
    /// * **HE / HE-v2** — expects raw AAC bytes (no ADTS). The
    ///   configuration comes from the magic cookie supplied via
    ///   `CodecParameters::extradata`.
    fn decode_packet(&mut self, data: &[u8]) -> Result<()> {
        let raw_aac = match self.profile {
            AacProfile::Lc => {
                let hdr =
                    adts::parse(data).ok_or_else(|| Error::invalid("AT decoder: bad ADTS sync"))?;
                if hdr.frame_length > data.len() {
                    return Err(Error::NeedMore);
                }
                if !self.configured {
                    self.configure_from_adts(&hdr)?;
                }
                let header_len = hdr.header_len();
                data[header_len..hdr.frame_length].to_vec()
            }
            // HE / HE-v2 / LD / ELD all carry their AOT out-of-band in
            // the magic cookie and arrive as raw AAC bytes (no ADTS).
            AacProfile::He | AacProfile::HeV2 | AacProfile::Ld | AacProfile::Eld => {
                if !self.configured {
                    self.configure_from_cookie()?;
                }
                data.to_vec()
            }
        };
        self.input_queue.push(raw_aac);
        self.drain_pcm()?;
        Ok(())
    }

    /// Loop pulling PCM frames from AT until it asks for more input than
    /// we have (or until our look-ahead heuristic suggests we should
    /// wait for the next `send_packet`).
    fn drain_pcm(&mut self) -> Result<()> {
        // HE / HE-v2 needs ~3-4 input packets of look-ahead before
        // emitting the first PCM block; LC is 1-1. We keep at least
        // `keep` packets in the queue so the input callback can hand
        // AT lookahead without ever returning 0 (which would mark the
        // input as EOS).
        let keep = match self.profile {
            AacProfile::Lc => 0,
            AacProfile::He | AacProfile::HeV2 => 4,
            // LD / ELD have a short analysis window; 1 packet of slack is
            // enough to keep the input callback from returning 0 mid-stream.
            AacProfile::Ld | AacProfile::Eld => 1,
        };
        while self.input_queue.len() > keep {
            let made = self.pull_one_pcm_frame()?;
            if !made {
                break;
            }
        }
        Ok(())
    }

    /// Single `FillComplexBuffer` call asking for one output frame's
    /// worth of PCM. Returns `true` if a frame was produced and pushed
    /// onto `self.pending`.
    fn pull_one_pcm_frame(&mut self) -> Result<bool> {
        let fw = sys::framework()
            .map_err(|e| Error::unsupported(format!("AudioToolbox unavailable: {e}")))?;

        let channels = self.channels as usize;
        let sample_count = self.profile.frames_per_packet() as usize;
        let buf_size = sample_count * channels * 4;
        let mut pcm_buf = vec![0u8; buf_size];

        let mut ctx = InputContext {
            // `queue` is moved out so the callback can drain it freely;
            // we put whatever's left back at the end of the call.
            queue: std::mem::take(&mut self.input_queue),
            handed_off: Vec::new(),
            packet_desc: AudioStreamPacketDescription::default(),
        };

        let mut output_packet_count: u32 = sample_count as u32;
        let mut abl = AudioBufferList1 {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: channels as u32,
                data_byte_size: buf_size as u32,
                data: pcm_buf.as_mut_ptr(),
            }],
        };

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

        // Restore whatever's left of the queue.
        self.input_queue = std::mem::take(&mut ctx.queue);

        if status != NO_ERR && status != 1 {
            return Err(status_error("AudioConverterFillComplexBuffer", status));
        }

        let actual_bytes = abl.buffers[0].data_byte_size as usize;
        if actual_bytes == 0 || output_packet_count == 0 {
            return Ok(false);
        }
        let actual_samples = actual_bytes / (channels * 4);
        let frame = AudioFrame {
            samples: actual_samples as u32,
            pts: Some(self.pts),
            data: vec![pcm_buf[..actual_bytes].to_vec()],
        };
        self.pts += actual_samples as i64;
        self.pending.push(Frame::Audio(frame));
        Ok(true)
    }

    /// Drain remaining PCM from the converter at flush time — ignores
    /// the look-ahead `keep` heuristic and pulls until the converter
    /// stops emitting.
    fn drain_pcm_all(&mut self) -> Result<()> {
        for _ in 0..32 {
            if self.input_queue.is_empty() {
                break;
            }
            if !self.pull_one_pcm_frame()? {
                break;
            }
        }
        // One final empty pull to flush any internal lookahead PCM.
        for _ in 0..4 {
            if !self.pull_one_pcm_frame()? {
                break;
            }
        }
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
        if !self.pending.is_empty() {
            return Ok(self.pending.remove(0));
        }
        if self.eof {
            return Err(Error::Eof);
        }
        Err(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        if self.configured {
            self.drain_pcm_all()?;
        }
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

/// The input-data callback.  Called by `AudioConverterFillComplexBuffer`
/// when it wants more compressed input packets.
///
/// Hands one packet at a time off the front of `ctx.queue`. Returns 0
/// packets only when the queue is empty — caller is expected to stage
/// enough look-ahead packets before invoking AT.
///
/// # Safety
/// `in_user_data` must point to a valid `InputContext` for the duration
/// of the call. The buffer pointer placed in `io_data.buffers[0].data`
/// references the front packet of `ctx.queue`, which is kept alive
/// across this callback invocation because we don't pop the front until
/// AT has had its chance to consume it.
unsafe extern "C" fn aac_input_callback(
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

    // Pop the front packet — Vec::remove(0) is O(n) but the queue is
    // tiny (1-5 packets) and this only fires when AT needs more input,
    // so the cost is negligible against the AT call itself.
    let pkt = ctx.queue.remove(0);
    ctx.packet_desc = AudioStreamPacketDescription {
        start_offset: 0,
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

    // Move the popped packet to `handed_off` so its bytes survive past
    // the callback return. AT may dereference the buffer until the
    // surrounding FillComplexBuffer call completes; only AFTER that does
    // the caller drop `ctx`, releasing the packet.
    ctx.handed_off.push(pkt);
    0 // noErr
}

/// Factory function registered with the codec registry.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    // Verify framework is reachable before claiming success.
    sys::framework().map_err(|e| Error::unsupported(format!("AudioToolbox unavailable: {e}")))?;
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
