//! iLBC decoder backed by macOS AudioConverter.
//!
//! Input:  one raw iLBC packet per `Packet` (38 bytes for 20 ms mode or
//!         50 bytes for 30 ms mode; the converter is configured for one
//!         specific geometry at construction time).
//! Output: interleaved S16 PCM `AudioFrame`s, one per decoded packet
//!         worth of PCM (160 or 240 samples). The decoder buffers
//!         frames internally so `receive_frame` can be drained in a
//!         standard loop.
//!
//! There is **no magic cookie** for iLBC. Configuration is the
//! `IlbcMode` selector carried in `CodecParameters::options.get("mode")`
//! (defaults to 30 ms — RFC 3951's compression-favoured mode).
//!
//! AT's iLBC decoder has a small analysis lookahead — the first PCM
//! emitted from a fresh packet stream often spans samples drawn from
//! one or two prior compressed packets. We therefore use a persistent
//! input-packet queue (the same shape as the HE-AAC decoder) so AT can
//! keep pulling compressed packets until it has enough to emit a PCM
//! block, instead of being forced into a permanent end-of-stream state
//! by a callback that returns 0 mid-stream.

use std::ffi::c_void;

use oxideav_core::Decoder;
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, Result, TimeBase};

use crate::ilbc::IlbcMode;
use crate::sys::{
    self, AudioBuffer, AudioBufferList1, AudioConverterRef, AudioStreamBasicDescription,
    AudioStreamPacketDescription, NO_ERR,
};

/// State shared with the AudioConverter input callback.
///
/// Holds a FIFO of compressed iLBC packets. AT calls the input callback
/// once per `FillComplexBuffer` invocation, asking for one compressed
/// packet. We pop the front of the queue and hand it over.
struct InputContext {
    queue: Vec<Vec<u8>>,
    /// Packets handed to AT during this `FillComplexBuffer` call. Kept
    /// alive so AT can still reference their bytes after the callback
    /// returns — released only when the surrounding call completes and
    /// the `InputContext` is dropped.
    handed_off: Vec<Vec<u8>>,
    /// Reusable packet descriptor rewritten on each callback fire.
    packet_desc: AudioStreamPacketDescription,
    frames_per_packet: u32,
}

/// AudioConverter-backed iLBC decoder.
pub struct IlbcAtDecoder {
    codec_id: CodecId,
    mode: IlbcMode,
    converter: AudioConverterRef,
    /// FIFO of PCM frames ready to be vended via `receive_frame`.
    pending: Vec<Frame>,
    /// Persistent input-packet queue.
    input_queue: Vec<Vec<u8>>,
    pts: i64,
    #[allow(dead_code)]
    time_base: TimeBase,
    eof: bool,
}

// SAFETY: same justification as the AAC / ALAC decoders — the converter
// handle is used from one thread at a time, never moved mid-call.
unsafe impl Send for IlbcAtDecoder {}

impl IlbcAtDecoder {
    fn new(params: &CodecParameters) -> Result<Self> {
        let fw =
            sys::framework().map_err(|e| Error::other(format!("AudioToolbox unavailable: {e}")))?;

        // iLBC is fixed at 8 kHz mono. The only configurable knob is the
        // mode (20 vs 30 ms). Sample-rate / channel parameters from the
        // caller are accepted for documentation but ignored for the
        // ASBD build — feeding AT anything other than (8000, 1) yields
        // `kAudioConverterErr_FormatNotSupported`.
        if let Some(sr) = params.sample_rate {
            if sr != 8_000 {
                return Err(Error::unsupported(format!(
                    "iLBC decoder: sample_rate must be 8000 (got {sr})"
                )));
            }
        }
        if let Some(ch) = params.channels {
            if ch != 1 {
                return Err(Error::unsupported(format!(
                    "iLBC decoder: channels must be 1 (got {ch})"
                )));
            }
        }

        let mode = IlbcMode::parse(params.options.get("mode"));

        let in_asbd = AudioStreamBasicDescription::ilbc(mode.frames_per_packet());
        let out_asbd = AudioStreamBasicDescription::pcm_s16(8_000.0, 1);

        let mut converter: AudioConverterRef = std::ptr::null_mut();
        let status = unsafe { sys::audio_converter_new(fw, &in_asbd, &out_asbd, &mut converter) };
        if status != NO_ERR {
            return Err(Error::other(format!(
                "AudioConverterNew (iLBC dec, mode={mode:?}) failed: OSStatus {status}"
            )));
        }

        Ok(Self {
            codec_id: params.codec_id.clone(),
            mode,
            converter,
            pending: Vec::new(),
            input_queue: Vec::new(),
            pts: 0,
            time_base: TimeBase::new(1, 8_000),
            eof: false,
        })
    }

    fn decode_packet(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        if data.len() as u32 != self.mode.bytes_per_packet() {
            // iLBC packets are fixed-size per mode. A different size
            // means the caller picked the wrong mode at construction
            // time, or the upstream framer is broken.
            return Err(Error::invalid(format!(
                "iLBC decoder: packet length {} doesn't match mode {:?} ({} bytes)",
                data.len(),
                self.mode,
                self.mode.bytes_per_packet()
            )));
        }
        self.input_queue.push(data.to_vec());
        self.drain_pcm()?;
        Ok(())
    }

    /// Pull PCM frames from AT until either the converter signals it
    /// needs more input, or the queue is empty (preserving a small
    /// look-ahead tail so AT never sees "0 packets left" mid-stream).
    fn drain_pcm(&mut self) -> Result<()> {
        // Keep one packet of slack in the queue — AT's iLBC analysis
        // window extends slightly beyond a single packet boundary, so
        // returning 0 from the callback before EOF puts the converter
        // into a permanent end-of-stream state.
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

        let frames_per_packet = self.mode.frames_per_packet() as usize;
        let buf_size = frames_per_packet * 2; // mono S16
        let mut pcm_buf = vec![0u8; buf_size];

        let mut ctx = InputContext {
            queue: std::mem::take(&mut self.input_queue),
            handed_off: Vec::new(),
            packet_desc: AudioStreamPacketDescription::default(),
            frames_per_packet: self.mode.frames_per_packet(),
        };

        let mut output_packet_count: u32 = frames_per_packet as u32;
        let mut abl = AudioBufferList1 {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: 1,
                data_byte_size: buf_size as u32,
                data: pcm_buf.as_mut_ptr(),
            }],
        };

        let status = unsafe {
            sys::audio_converter_fill_complex_buffer(
                fw,
                self.converter,
                ilbc_input_callback,
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
                "AudioConverterFillComplexBuffer (iLBC dec) failed: OSStatus {status}"
            )));
        }

        let actual_bytes = abl.buffers[0].data_byte_size as usize;
        if actual_bytes == 0 || output_packet_count == 0 {
            return Ok(false);
        }
        let actual_samples = actual_bytes / 2;
        let frame = AudioFrame {
            samples: actual_samples as u32,
            pts: Some(self.pts),
            data: vec![pcm_buf[..actual_bytes].to_vec()],
        };
        self.pts += actual_samples as i64;
        self.pending.push(Frame::Audio(frame));
        Ok(true)
    }

    /// Drain every PCM frame still cached in AT, ignoring the look-ahead
    /// slack heuristic. Called from `flush()`.
    fn drain_pcm_all(&mut self) -> Result<()> {
        for _ in 0..256 {
            if self.input_queue.is_empty() {
                break;
            }
            if !self.pull_one_pcm_frame()? {
                break;
            }
        }
        // One final pull to flush any internal look-ahead PCM.
        for _ in 0..2 {
            if !self.pull_one_pcm_frame()? {
                break;
            }
        }
        Ok(())
    }
}

impl Drop for IlbcAtDecoder {
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

impl Decoder for IlbcAtDecoder {
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

/// Input callback — supplies one compressed iLBC packet per call from
/// the front of the persistent queue.
///
/// # Safety
/// `in_user_data` must point to a valid `InputContext` for the duration
/// of the call.
unsafe extern "C" fn ilbc_input_callback(
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
        variable_frames_in_packet: ctx.frames_per_packet,
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
    // the callback return.
    ctx.handed_off.push(pkt);
    0
}

/// Factory function registered with the codec registry.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(IlbcAtDecoder::new(params)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{CodecId, CodecParameters};

    fn params_ilbc(mode: &str) -> CodecParameters {
        let mut p = CodecParameters::audio(CodecId::new("ilbc"));
        p.sample_rate = Some(8_000);
        p.channels = Some(1);
        p.options.insert("mode", mode);
        p
    }

    #[test]
    fn make_decoder_succeeds_30ms() {
        let r = make_decoder(&params_ilbc("30"));
        assert!(r.is_ok(), "iLBC 30 ms make_decoder failed: {:?}", r.err());
    }

    #[test]
    fn make_decoder_succeeds_20ms() {
        let r = make_decoder(&params_ilbc("20"));
        assert!(r.is_ok(), "iLBC 20 ms make_decoder failed: {:?}", r.err());
    }

    #[test]
    fn make_decoder_default_mode_is_30ms() {
        let mut p = CodecParameters::audio(CodecId::new("ilbc"));
        p.sample_rate = Some(8_000);
        p.channels = Some(1);
        // No "mode" option — should default to 30 ms.
        let r = make_decoder(&p);
        assert!(
            r.is_ok(),
            "iLBC default-mode make_decoder failed: {:?}",
            r.err()
        );
    }

    #[test]
    fn make_decoder_rejects_bad_sample_rate() {
        let mut p = params_ilbc("30");
        p.sample_rate = Some(48_000);
        let r = make_decoder(&p);
        assert!(r.is_err(), "iLBC must reject non-8 kHz sample rate");
    }

    #[test]
    fn make_decoder_rejects_stereo() {
        let mut p = params_ilbc("30");
        p.channels = Some(2);
        let r = make_decoder(&p);
        assert!(r.is_err(), "iLBC must reject stereo");
    }
}
