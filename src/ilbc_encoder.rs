//! iLBC encoder backed by macOS AudioConverter.
//!
//! Input:  interleaved S16 PCM (`AudioFrame`) at **8 kHz mono**. F32
//!         input is accepted as a convenience and converted to S16
//!         before feeding the converter.
//! Output: raw iLBC packets (no framing wrapper — RTP / SIP carriage
//!         lives in the container layer). Packet size is fixed per mode:
//!         38 bytes for 20 ms (160 PCM frames), 50 bytes for 30 ms
//!         (240 PCM frames). The encoder echoes the active mode through
//!         `output_params.options["mode"]` so a downstream decoder can
//!         configure the matching geometry.
//!
//! AudioConverter packets a fixed number of PCM frames per output
//! packet, so a partial trailing frame at flush time is zero-padded up
//! to the block length — the standard convention for end-of-stream in
//! a CBR speech codec.

use std::ffi::c_void;

use oxideav_core::Encoder;
use oxideav_core::{CodecId, CodecParameters, Error, Frame, Packet, Result, TimeBase};

use crate::ilbc::IlbcMode;
use crate::sys::{
    self, AudioBuffer, AudioBufferList1, AudioConverterRef, AudioStreamBasicDescription,
    AudioStreamPacketDescription, NO_ERR,
};

/// State handed to the AudioConverter input callback.
struct PcmContext {
    data: *const u8,
    len: u32,
    bytes_per_packet: u32, // input PCM packet size (= bytes_per_frame for PCM)
    consumed_bytes: u32,
}

/// AudioConverter-backed iLBC encoder.
pub struct IlbcAtEncoder {
    codec_id: CodecId,
    converter: AudioConverterRef,
    mode: IlbcMode,
    /// Bytes per S16 PCM frame at 8 kHz mono — fixed at 2.
    bytes_per_frame: u32,
    /// PCM staging buffer — accumulates input until a full
    /// `mode.frames_per_packet()`-sized chunk is available so each
    /// drain produces a deterministic full iLBC packet.
    staging: Vec<u8>,
    /// Queue of encoded packets ready for `receive_packet`.
    pending: Vec<Packet>,
    out_params: CodecParameters,
    pts: i64,
    time_base: TimeBase,
    eof: bool,
}

// SAFETY: same justification as AacAtEncoder.
unsafe impl Send for IlbcAtEncoder {}

impl IlbcAtEncoder {
    fn new(params: &CodecParameters) -> Result<Self> {
        let fw =
            sys::framework().map_err(|e| Error::other(format!("AudioToolbox unavailable: {e}")))?;

        // iLBC is fixed at 8 kHz mono. Reject anything else early —
        // AudioConverterNew would refuse it anyway, but a typed error
        // is friendlier than `kAudioConverterErr_FormatNotSupported`.
        let sr = params.sample_rate.unwrap_or(8_000);
        let ch = params.channels.unwrap_or(1);
        if sr != 8_000 {
            return Err(Error::unsupported(format!(
                "iLBC encoder: sample_rate must be 8000 (got {sr})"
            )));
        }
        if ch != 1 {
            return Err(Error::unsupported(format!(
                "iLBC encoder: channels must be 1 (got {ch})"
            )));
        }

        let mode = IlbcMode::parse(params.options.get("mode"));

        // We always feed AT S16 PCM — iLBC's analysis stage expects
        // integer samples, F32 PCM works too but adds a needless
        // conversion in the converter graph. We pre-convert F32 to S16
        // in `send_frame` if necessary, which lets us keep a single
        // ASBD here.
        let in_asbd = AudioStreamBasicDescription::pcm_s16(8_000.0, 1);
        let out_asbd = AudioStreamBasicDescription::ilbc(mode.frames_per_packet());

        let mut converter: AudioConverterRef = std::ptr::null_mut();
        let status = unsafe { sys::audio_converter_new(fw, &in_asbd, &out_asbd, &mut converter) };
        if status != NO_ERR {
            return Err(Error::other(format!(
                "AudioConverterNew (iLBC enc, mode={mode:?}) failed: OSStatus {status}"
            )));
        }

        let time_base = TimeBase::new(1, 8_000);
        let mut out_params = CodecParameters::audio(params.codec_id.clone());
        out_params.sample_rate = Some(8_000);
        out_params.channels = Some(1);
        out_params.bit_rate = Some(match mode {
            // RFC 3951 §2: net bitrate = compressed bytes × 8 / frame seconds.
            IlbcMode::Ms20 => 15_200, // 38 B × 8 / 0.020 s
            IlbcMode::Ms30 => 13_333, // 50 B × 8 / 0.030 s
        });
        out_params.options.insert("mode", mode.tag());

        Ok(Self {
            codec_id: params.codec_id.clone(),
            converter,
            mode,
            bytes_per_frame: 2,
            staging: Vec::with_capacity(mode.frames_per_packet() as usize * 2),
            pending: Vec::new(),
            out_params,
            pts: 0,
            time_base,
            eof: false,
        })
    }

    /// Drain whole-packet chunks from staging into encoded `Packet`s.
    fn drain_staging(&mut self) -> Result<()> {
        let packet_bytes = (self.mode.frames_per_packet() * self.bytes_per_frame) as usize;
        while self.staging.len() >= packet_bytes {
            // Pop the front `packet_bytes` and encode them.
            let chunk: Vec<u8> = self.staging.drain(..packet_bytes).collect();
            self.encode_one(&chunk)?;
        }
        Ok(())
    }

    /// Single `FillComplexBuffer` call producing one fixed-size iLBC packet.
    fn encode_one(&mut self, pcm: &[u8]) -> Result<()> {
        let fw =
            sys::framework().map_err(|e| Error::other(format!("AudioToolbox unavailable: {e}")))?;

        let out_size = self.mode.bytes_per_packet() as usize;
        let mut out_buf = vec![0u8; out_size];

        let mut ctx = PcmContext {
            data: pcm.as_ptr(),
            len: pcm.len() as u32,
            bytes_per_packet: self.bytes_per_frame,
            consumed_bytes: 0,
        };

        let mut output_packet_count: u32 = 1;
        let mut pkt_desc = AudioStreamPacketDescription::default();
        let mut abl = AudioBufferList1 {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: 1,
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
                "AudioConverterFillComplexBuffer (iLBC enc) failed: OSStatus {status}"
            )));
        }

        let raw_len = abl.buffers[0].data_byte_size as usize;
        if raw_len == 0 || output_packet_count == 0 {
            return Ok(());
        }
        out_buf.truncate(raw_len);

        let samples = self.mode.frames_per_packet() as i64;
        let pkt = Packet::new(0, self.time_base, out_buf)
            .with_pts(self.pts)
            .with_keyframe(true);
        self.pts += samples;
        self.pending.push(pkt);
        Ok(())
    }
}

impl Drop for IlbcAtEncoder {
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

impl Encoder for IlbcAtEncoder {
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
                // Convert F32 → S16 if the caller supplied float PCM.
                // We can't know the sample format from the frame alone,
                // so use a length heuristic: if the byte count equals
                // `samples × 4`, it's F32; if it equals `samples × 2`,
                // S16. Anything else gets fed verbatim as S16.
                let bytes = &af.data[0];
                let n_samples = af.samples as usize;
                if n_samples > 0 && bytes.len() == n_samples * 4 {
                    // F32 → S16
                    let mut s16 = Vec::with_capacity(n_samples * 2);
                    for chunk in bytes.chunks_exact(4) {
                        let f = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                        let v = (f.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                        s16.extend_from_slice(&v.to_le_bytes());
                    }
                    self.staging.extend_from_slice(&s16);
                } else {
                    self.staging.extend_from_slice(bytes);
                }
                self.drain_staging()
            }
            _ => Err(Error::unsupported(
                "IlbcAtEncoder only accepts Audio frames",
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
        // Zero-pad any partial trailing PCM up to a full packet so the
        // last block is delivered. iLBC has a fixed packet rate, so
        // truncating the tail would lose audible content; the standard
        // EOS convention is to feed zero-PCM padding through the
        // encoder's analysis filter.
        if !self.staging.is_empty() {
            let packet_bytes = (self.mode.frames_per_packet() * self.bytes_per_frame) as usize;
            if self.staging.len() < packet_bytes {
                self.staging.resize(packet_bytes, 0);
            }
            self.drain_staging()?;
        }
        self.eof = true;
        Ok(())
    }
}

/// Input callback for the encoder: supplies interleaved S16 PCM to
/// AudioConverter.
///
/// iLBC has no SBR look-ahead — one full packet of PCM in, one full
/// packet of compressed bytes out — so the callback simply hands off
/// the staged buffer in one shot.
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
    if ctx.len == 0 {
        *io_number_data_packets = 0;
        (*io_data).buffers[0].data_byte_size = 0;
        (*io_data).buffers[0].data = std::ptr::null_mut();
        return 0;
    }

    let requested = *io_number_data_packets;
    let available = (ctx.len / ctx.bytes_per_packet).max(1);
    let n_packets = if requested == 0 {
        available
    } else {
        available.min(requested)
    };

    let bytes = n_packets * ctx.bytes_per_packet;
    *io_number_data_packets = n_packets;
    (*io_data).number_buffers = 1;
    (*io_data).buffers[0].data_byte_size = bytes;
    (*io_data).buffers[0].data = ctx.data as *mut u8;

    ctx.data = ctx.data.add(bytes as usize);
    ctx.len -= bytes;
    ctx.consumed_bytes += bytes;
    0
}

/// Factory function registered with the codec registry.
pub fn make_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    Ok(Box::new(IlbcAtEncoder::new(params)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{CodecId, CodecParameters, SampleFormat};

    fn params_ilbc(mode: &str) -> CodecParameters {
        let mut p = CodecParameters::audio(CodecId::new("ilbc"));
        p.sample_rate = Some(8_000);
        p.channels = Some(1);
        p.sample_format = Some(SampleFormat::S16);
        p.options.insert("mode", mode);
        p
    }

    #[test]
    fn make_encoder_succeeds_30ms() {
        let r = make_encoder(&params_ilbc("30"));
        assert!(r.is_ok(), "iLBC 30 ms make_encoder failed: {:?}", r.err());
    }

    #[test]
    fn make_encoder_succeeds_20ms() {
        let r = make_encoder(&params_ilbc("20"));
        assert!(r.is_ok(), "iLBC 20 ms make_encoder failed: {:?}", r.err());
    }

    #[test]
    fn make_encoder_default_mode_is_30ms() {
        let mut p = CodecParameters::audio(CodecId::new("ilbc"));
        p.sample_rate = Some(8_000);
        p.channels = Some(1);
        let enc = make_encoder(&p).expect("encoder construct");
        let mode_tag = enc
            .output_params()
            .options
            .get("mode")
            .expect("mode in output params");
        assert_eq!(mode_tag, "30");
    }

    #[test]
    fn make_encoder_publishes_bitrate_30ms() {
        let enc = make_encoder(&params_ilbc("30")).expect("encoder construct");
        let br = enc.output_params().bit_rate.expect("bit_rate published");
        // RFC 3951: 30 ms mode = 50 B × 8 / 0.030 s = 13.33 kbit/s.
        assert_eq!(br, 13_333);
    }

    #[test]
    fn make_encoder_publishes_bitrate_20ms() {
        let enc = make_encoder(&params_ilbc("20")).expect("encoder construct");
        let br = enc.output_params().bit_rate.expect("bit_rate published");
        // 20 ms mode = 38 B × 8 / 0.020 s = 15.2 kbit/s.
        assert_eq!(br, 15_200);
    }

    #[test]
    fn make_encoder_rejects_bad_sample_rate() {
        let mut p = params_ilbc("30");
        p.sample_rate = Some(16_000);
        let r = make_encoder(&p);
        assert!(r.is_err(), "iLBC must reject non-8 kHz");
    }

    #[test]
    fn make_encoder_rejects_stereo() {
        let mut p = params_ilbc("30");
        p.channels = Some(2);
        let r = make_encoder(&p);
        assert!(r.is_err(), "iLBC must reject stereo");
    }
}
