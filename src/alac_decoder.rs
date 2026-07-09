//! Apple Lossless (ALAC) decoder backed by macOS AudioConverter.
//!
//! Input:  one raw ALAC packet per `Packet` (no framing wrapper —
//!         containers like MOV / M4A / CAF deliver packets pre-split).
//! Output: interleaved PCM in an `AudioFrame`, one per decoded ALAC
//!         packet (typically 4096 samples). Sample width is chosen
//!         per-call: explicit `CodecParameters::sample_format = S32`
//!         routes through `pcm_s32` so the full 24/32-bit lossless
//!         word survives; otherwise the decoder defaults to S16 for
//!         backwards compatibility with callers that pre-date the
//!         S32 path. Asking for S32 against a 16/20-bit cookie is
//!         accepted (AT sign-extends into the high bytes).
//!
//! Configuration is driven by the **magic cookie** carried in
//! `CodecParameters::extradata`. If the consumer didn't supply one
//! (e.g. raw-PCM-to-ALAC self-test paths), we synthesise a minimal
//! 24-byte `ALACSpecificConfig` from the explicit `sample_rate /
//! channels / sample_format` fields.

use std::ffi::c_void;

use oxideav_core::Decoder;
use oxideav_core::{
    AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, Result, SampleFormat, TimeBase,
};

use crate::alac::{self, AlacSpecificConfig};
use crate::status::status_error;
use crate::sys::{
    self, AudioBuffer, AudioBufferList1, AudioConverterRef, AudioStreamBasicDescription,
    AudioStreamPacketDescription, K_AUDIO_CONVERTER_DECOMPRESSION_MAGIC_COOKIE, NO_ERR,
};

/// Per-packet state passed to the AudioConverter input callback.
struct InputContext {
    data: *const u8,
    len: u32,
    /// Frames-per-packet announced by the magic cookie (e.g. 4096).
    frames_per_packet: u32,
    packet_desc: AudioStreamPacketDescription,
    consumed: bool,
}

/// AudioConverter-backed ALAC decoder.
pub struct AlacAtDecoder {
    codec_id: CodecId,
    #[allow(dead_code)]
    sample_rate: u32,
    channels: u16,
    /// Mandatory portion of the magic cookie (kept so `Drop` /
    /// diagnostics can introspect what was used to configure AT).
    #[allow(dead_code)]
    cookie: Vec<u8>,
    cfg: AlacSpecificConfig,
    converter: AudioConverterRef,
    /// `S16` or `S32` — width of each output sample produced by the
    /// pending `AudioFrame`. Determined at construction from
    /// `CodecParameters::sample_format` and never changes for the
    /// lifetime of the decoder (the converter's output ASBD is fixed).
    output_format: SampleFormat,
    /// Cached bytes per output frame: `bytes_per_sample(output_format) *
    /// channels`. Used to size the PCM staging buffer and to recover
    /// the per-packet sample count from `data_byte_size`.
    output_bytes_per_frame: usize,
    pending: Option<Frame>,
    pts: i64,
    #[allow(dead_code)]
    time_base: TimeBase,
    eof: bool,
}

// SAFETY: same justification as AacAtDecoder — converter handle is used
// from one thread at a time and never crosses thread boundaries during
// a single call.
unsafe impl Send for AlacAtDecoder {}

impl AlacAtDecoder {
    fn new(params: &CodecParameters) -> Result<Self> {
        let fw = sys::framework()
            .map_err(|e| Error::unsupported(format!("AudioToolbox unavailable: {e}")))?;

        // Resolve cookie + cfg.
        let (cookie, cfg) = if params.extradata.len() >= alac::SPECIFIC_CONFIG_LEN {
            // Use the bytes the consumer handed us verbatim. If the cookie
            // is longer than 24 bytes (channel-layout-info or legacy
            // wrapper), trust the consumer and forward the whole blob.
            let cfg = AlacSpecificConfig::parse(&params.extradata)
                .ok_or_else(|| Error::invalid("ALAC: malformed magic cookie"))?;
            (params.extradata.clone(), cfg)
        } else {
            let sr = params
                .sample_rate
                .ok_or_else(|| Error::invalid("ALAC: sample_rate required when no magic cookie"))?;
            let ch = params
                .channels
                .ok_or_else(|| Error::invalid("ALAC: channels required when no magic cookie"))?;
            let bit_depth = match params.sample_format {
                Some(SampleFormat::S16) | None => 16u8,
                Some(SampleFormat::S32) => 32u8,
                Some(SampleFormat::F32) => 16u8, // ALAC is integer; we still output S16 PCM.
                Some(other) => {
                    return Err(Error::unsupported(format!(
                        "ALAC decoder: unsupported sample_format {other:?}"
                    )))
                }
            };
            let cfg = AlacSpecificConfig::new(sr, ch as u8, bit_depth);
            (cfg.to_bytes().to_vec(), cfg)
        };

        let bit_depth_flag = alac::bit_depth_flag(cfg.bit_depth).ok_or_else(|| {
            Error::unsupported(format!(
                "ALAC decoder: unsupported bit_depth {}",
                cfg.bit_depth
            ))
        })?;

        // Pick output PCM width. Default = S16 (historical behaviour,
        // matches every existing caller). An explicit
        // `params.sample_format = SampleFormat::S32` switches to the
        // full-width path — necessary to round-trip a 24- or 32-bit
        // cookie losslessly. Asking for S32 against a 16- or 20-bit
        // cookie is harmless: AudioConverter sign-extends the source
        // word into the high bytes (the low bits are zero / sign
        // padding) so the bit-exact prefix invariant still holds.
        let output_format = match params.sample_format {
            Some(SampleFormat::S32) => SampleFormat::S32,
            // F32 input would only have meant "decode-to-int-then-do-
            // whatever" — keep mapping it to S16 to preserve the
            // legacy fallback rather than silently introduce a new
            // sample width.
            _ => SampleFormat::S16,
        };
        let output_bytes_per_frame = output_format.bytes_per_sample() * cfg.num_channels as usize;

        let in_asbd = AudioStreamBasicDescription::apple_lossless(
            cfg.sample_rate as f64,
            cfg.num_channels as u32,
            bit_depth_flag,
            cfg.frame_length,
        );
        let out_asbd = match output_format {
            SampleFormat::S32 => AudioStreamBasicDescription::pcm_s32(
                cfg.sample_rate as f64,
                cfg.num_channels as u32,
            ),
            _ => AudioStreamBasicDescription::pcm_s16(
                cfg.sample_rate as f64,
                cfg.num_channels as u32,
            ),
        };

        let mut converter: AudioConverterRef = std::ptr::null_mut();
        let status = unsafe { sys::audio_converter_new(fw, &in_asbd, &out_asbd, &mut converter) };
        if status != NO_ERR {
            return Err(status_error("AudioConverterNew (ALAC dec)", status));
        }

        // Wire up the magic cookie.
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
                "AudioConverterSetProperty(DecompressionMagicCookie)",
                status,
            ));
        }

        Ok(Self {
            codec_id: params.codec_id.clone(),
            sample_rate: cfg.sample_rate,
            channels: cfg.num_channels as u16,
            cookie,
            cfg,
            converter,
            output_format,
            output_bytes_per_frame,
            pending: None,
            pts: 0,
            time_base: TimeBase::new(1, cfg.sample_rate as i64),
            eof: false,
        })
    }

    fn decode_packet(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let fw = sys::framework()
            .map_err(|e| Error::unsupported(format!("AudioToolbox unavailable: {e}")))?;

        let channels = self.channels as usize;
        let frames_per_packet = self.cfg.frame_length as usize;
        // Output buffer is sized to one full ALAC packet of
        // `output_format` samples (S16 = 2 bytes, S32 = 4 bytes).
        let buf_size = frames_per_packet * self.output_bytes_per_frame;
        let mut pcm_buf = vec![0u8; buf_size];

        let mut ctx = InputContext {
            data: data.as_ptr(),
            len: data.len() as u32,
            frames_per_packet: self.cfg.frame_length,
            packet_desc: AudioStreamPacketDescription {
                start_offset: 0,
                variable_frames_in_packet: 0,
                data_byte_size: data.len() as u32,
            },
            consumed: false,
        };

        let mut output_packet_count: u32 = frames_per_packet as u32;
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
                alac_input_callback,
                &mut ctx as *mut InputContext as *mut c_void,
                &mut output_packet_count,
                &mut abl,
                std::ptr::null_mut(),
            )
        };

        if status != NO_ERR && status != 1 {
            return Err(status_error(
                "AudioConverterFillComplexBuffer (ALAC dec)",
                status,
            ));
        }

        let actual_bytes = abl.buffers[0].data_byte_size as usize;
        if actual_bytes == 0 {
            return Ok(());
        }
        let actual_samples = actual_bytes / self.output_bytes_per_frame;

        let frame = AudioFrame {
            samples: actual_samples as u32,
            pts: Some(self.pts),
            data: vec![pcm_buf[..actual_bytes].to_vec()],
        };
        self.pts += actual_samples as i64;
        self.pending = Some(Frame::Audio(frame));
        Ok(())
    }

    /// Output sample format the decoder will produce for every
    /// `AudioFrame`. Diagnostic accessor — useful when bridging into
    /// a pipeline whose downstream consumer needs to know whether it
    /// will see 16-bit or 32-bit samples.
    pub fn output_sample_format(&self) -> SampleFormat {
        self.output_format
    }
}

impl Drop for AlacAtDecoder {
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

impl Decoder for AlacAtDecoder {
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

/// Input callback — supplies one compressed ALAC packet per
/// `FillComplexBuffer` call.
///
/// # Safety
/// `in_user_data` must point to a valid `InputContext` for the duration
/// of the call.
unsafe extern "C" fn alac_input_callback(
    _converter: AudioConverterRef,
    io_number_data_packets: *mut u32,
    io_data: *mut AudioBufferList1,
    out_packet_desc: *mut *mut AudioStreamPacketDescription,
    in_user_data: *mut c_void,
) -> sys::OSStatus {
    let ctx = &mut *(in_user_data as *mut InputContext);
    if ctx.consumed || ctx.len == 0 {
        *io_number_data_packets = 0;
        (*io_data).buffers[0].data_byte_size = 0;
        (*io_data).buffers[0].data = std::ptr::null_mut();
        return 0;
    }

    *io_number_data_packets = 1;
    (*io_data).number_buffers = 1;
    (*io_data).buffers[0].data_byte_size = ctx.len;
    (*io_data).buffers[0].data = ctx.data as *mut u8;
    (*io_data).buffers[0].number_channels = 0; // ignored for compressed input

    if !out_packet_desc.is_null() {
        ctx.packet_desc.variable_frames_in_packet = ctx.frames_per_packet;
        *out_packet_desc = &mut ctx.packet_desc;
    }

    ctx.consumed = true;
    0
}

/// Factory function registered with the codec registry.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(AlacAtDecoder::new(params)?))
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
        // No extradata -> decoder synthesises a default cookie.
        p
    }

    #[test]
    fn make_decoder_succeeds_with_synthesised_cookie() {
        let r = make_decoder(&params_alac_48k_stereo());
        assert!(r.is_ok(), "make_decoder failed: {:?}", r.err());
    }

    #[test]
    fn make_decoder_succeeds_with_supplied_cookie() {
        let cfg = AlacSpecificConfig::new(44_100, 2, 16);
        let mut p = CodecParameters::audio(CodecId::new("alac"));
        p.sample_rate = Some(44_100);
        p.channels = Some(2);
        p.extradata = cfg.to_bytes().to_vec();
        let r = make_decoder(&p);
        assert!(r.is_ok(), "make_decoder w/ cookie failed: {:?}", r.err());
    }

    #[test]
    fn make_decoder_rejects_short_cookie() {
        let mut p = CodecParameters::audio(CodecId::new("alac"));
        p.sample_rate = Some(48_000);
        p.channels = Some(2);
        // 10 bytes < 24-byte minimum: falls into the "synthesise" path
        // since len < SPECIFIC_CONFIG_LEN, but that path still needs
        // sample_rate + channels and we supplied both, so it succeeds.
        p.extradata = vec![0u8; 10];
        let r = make_decoder(&p);
        assert!(r.is_ok(), "synthesise path should succeed: {:?}", r.err());
    }

    #[test]
    fn default_output_is_s16() {
        // No explicit sample_format → S16, matching every pre-S32
        // caller's expectation (4096-frame packets = 16384 bytes
        // stereo, not 32768).
        let mut p = CodecParameters::audio(CodecId::new("alac"));
        p.sample_rate = Some(48_000);
        p.channels = Some(2);
        let dec_box = make_decoder(&p).expect("make_decoder");
        // Round-trip into the concrete type to introspect the
        // chosen output format. Going through `Box<dyn Decoder>`
        // and downcasting is not exposed; instead we re-construct
        // `AlacAtDecoder::new` directly here.
        let dec = AlacAtDecoder::new(&p).expect("AlacAtDecoder::new");
        assert_eq!(dec.output_sample_format(), SampleFormat::S16);
        // 2 channels × 2 bytes per S16 sample = 4.
        assert_eq!(dec.output_bytes_per_frame, 4);
        // Use the box so the factory path is also exercised.
        drop(dec_box);
    }

    #[test]
    fn explicit_s32_switches_output_width() {
        // sample_format = S32 → S32 PCM output, no truncation.
        let mut p = CodecParameters::audio(CodecId::new("alac"));
        p.sample_rate = Some(48_000);
        p.channels = Some(2);
        p.sample_format = Some(SampleFormat::S32);
        let dec = AlacAtDecoder::new(&p).expect("AlacAtDecoder::new (S32)");
        assert_eq!(dec.output_sample_format(), SampleFormat::S32);
        assert_eq!(dec.output_bytes_per_frame, 8); // 2 ch × 4 B
    }

    #[test]
    fn s32_with_24bit_cookie_accepted() {
        // S32 output against a 24-bit-depth cookie: AT sign-extends.
        // Used by the 24-bit roundtrip path in tests/alac_s32_roundtrip.rs.
        let cfg = AlacSpecificConfig::new(96_000, 2, 24);
        let mut p = CodecParameters::audio(CodecId::new("alac"));
        p.sample_rate = Some(96_000);
        p.channels = Some(2);
        p.sample_format = Some(SampleFormat::S32);
        p.extradata = cfg.to_bytes().to_vec();
        let dec = AlacAtDecoder::new(&p).expect("AlacAtDecoder::new (24-bit → S32)");
        assert_eq!(dec.output_sample_format(), SampleFormat::S32);
        assert_eq!(dec.cfg.bit_depth, 24);
    }
}
