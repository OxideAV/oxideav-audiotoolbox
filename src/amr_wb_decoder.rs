//! AMR-WB decoder backed by macOS AudioConverter.
//!
//! Input:  one storage-format AMR-WB packet per `Packet` — a TOC byte
//!         followed by the mode-determined number of speech payload
//!         bytes (1, 6, 17, 23, 32, 36, 40, 46, 50, 58, or 60 total
//!         bytes per RFC 4867 §5.3 depending on the frame type encoded
//!         in the TOC).
//! Output: interleaved S16 PCM `AudioFrame`s at 16 kHz mono. Like the
//!         AMR-NB path, AT may vend PCM in sub-frame blocks per
//!         `FillComplexBuffer` call rather than the canonical 320-sample
//!         analysis-frame size — the per-frame size is not load-bearing
//!         for downstream consumers, only `samples × 2 == data.len()`
//!         and `sample_rate == 16000` are. Multiple `FillComplexBuffer`
//!         calls per input packet are normal.
//!
//! AMR-WB on AudioToolbox is **decode-only** — AT exposes
//! `kAudioFormatAMR_WB` (`'sawb'`) as a decompression target but does
//! not ship a paired encoder. There is no magic cookie: the per-packet
//! TOC byte carries the active mode and AT switches mid-stream as the
//! incoming mode index changes.
//!
//! Packet length is variable, so the input callback fills in the
//! `AudioStreamPacketDescription` with the actual byte count of each
//! handed-off packet. The decoder uses the same persistent
//! input-queue + one-packet-of-slack pattern as the AMR-NB / iLBC
//! decoders so the converter never sees "0 packets" mid-stream, which
//! would put it into a permanent end-of-stream state.

use std::ffi::c_void;

use oxideav_core::Decoder;
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, Result, TimeBase};

use crate::amr_wb::{self, FrameType};
use crate::sys::{
    self, AudioBuffer, AudioBufferList1, AudioConverterRef, AudioStreamBasicDescription,
    AudioStreamPacketDescription, NO_ERR,
};

/// State shared with the AudioConverter input callback.
///
/// Holds a FIFO of compressed AMR-WB packets. The callback pops the
/// front of the queue on each invocation and hands a single packet to
/// AT with its packet-description size filled from the storage-format
/// byte count of the popped buffer.
struct InputContext {
    queue: Vec<Vec<u8>>,
    /// Packets handed off to AT during the current `FillComplexBuffer`
    /// call — kept alive so the converter can still reference their
    /// bytes after the callback returns.
    handed_off: Vec<Vec<u8>>,
    /// Reusable packet descriptor rewritten on each callback fire.
    packet_desc: AudioStreamPacketDescription,
}

/// AudioConverter-backed AMR-WB decoder.
pub struct AmrWbAtDecoder {
    codec_id: CodecId,
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

// SAFETY: AudioConverterRef is used single-threaded inside the Decoder
// impl. The `*mut` inside it is the opaque handle Apple guarantees is
// usable from the thread it was created on; we never move the handle
// across threads.
unsafe impl Send for AmrWbAtDecoder {}

impl AmrWbAtDecoder {
    fn new(params: &CodecParameters) -> Result<Self> {
        let fw =
            sys::framework().map_err(|e| Error::other(format!("AudioToolbox unavailable: {e}")))?;

        // AMR-WB is fixed at 16 kHz mono. Reject anything else early —
        // AudioConverterNew would refuse it anyway, but a typed error
        // is friendlier than `kAudioConverterErr_FormatNotSupported`.
        if let Some(sr) = params.sample_rate {
            if sr != 16_000 {
                return Err(Error::unsupported(format!(
                    "AMR-WB decoder: sample_rate must be 16000 (got {sr})"
                )));
            }
        }
        if let Some(ch) = params.channels {
            if ch != 1 {
                return Err(Error::unsupported(format!(
                    "AMR-WB decoder: channels must be 1 (got {ch})"
                )));
            }
        }

        let in_asbd = AudioStreamBasicDescription::amr_wb();
        let out_asbd = AudioStreamBasicDescription::pcm_s16(16_000.0, 1);

        let mut converter: AudioConverterRef = std::ptr::null_mut();
        let status = unsafe { sys::audio_converter_new(fw, &in_asbd, &out_asbd, &mut converter) };
        if status != NO_ERR {
            return Err(Error::other(format!(
                "AudioConverterNew (AMR-WB dec) failed: OSStatus {status}"
            )));
        }

        Ok(Self {
            codec_id: params.codec_id.clone(),
            converter,
            pending: Vec::new(),
            input_queue: Vec::new(),
            pts: 0,
            time_base: TimeBase::new(1, 16_000),
            eof: false,
        })
    }

    /// Validate an AMR-WB storage-format packet against its TOC byte
    /// and queue it for decode.
    fn decode_packet(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let toc = data[0];
        let ft = FrameType::from_toc(toc).ok_or_else(|| {
            Error::invalid(format!(
                "AMR-WB: TOC byte 0x{toc:02x} encodes a reserved frame type (FT={})",
                (toc >> 3) & 0x0F
            ))
        })?;
        let expected = ft.bytes_per_packet();
        if data.len() != expected {
            return Err(Error::invalid(format!(
                "AMR-WB: {ft:?} packet length {} doesn't match storage-format size {expected}",
                data.len()
            )));
        }
        self.input_queue.push(data.to_vec());
        self.drain_pcm()?;
        Ok(())
    }

    /// Pull PCM frames from AT until either the converter signals it
    /// needs more input, or the queue is down to a single packet
    /// (preserving the look-ahead tail so AT never sees "0 packets
    /// left" mid-stream).
    fn drain_pcm(&mut self) -> Result<()> {
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

        let frames_per_packet = amr_wb::FRAMES_PER_PACKET as usize;
        let buf_size = frames_per_packet * 2; // mono S16
        let mut pcm_buf = vec![0u8; buf_size];

        let mut ctx = InputContext {
            queue: std::mem::take(&mut self.input_queue),
            handed_off: Vec::new(),
            packet_desc: AudioStreamPacketDescription::default(),
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
                amr_wb_input_callback,
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
                "AudioConverterFillComplexBuffer (AMR-WB dec) failed: OSStatus {status}"
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

    /// Drain every PCM frame still cached in AT — called from `flush`.
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

impl Drop for AmrWbAtDecoder {
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

impl Decoder for AmrWbAtDecoder {
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

/// Input callback — supplies one compressed AMR-WB packet per call
/// from the front of the persistent queue. The packet's storage-format
/// byte count is written into the packet descriptor so AT can read
/// the variable size from the right place.
///
/// # Safety
/// `in_user_data` must point to a valid `InputContext` for the
/// duration of the call.
unsafe extern "C" fn amr_wb_input_callback(
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
        variable_frames_in_packet: amr_wb::FRAMES_PER_PACKET,
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
    Ok(Box::new(AmrWbAtDecoder::new(params)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amr_wb::{make_toc, FrameType};
    use oxideav_core::{CodecId, CodecParameters};

    fn params_amr_wb() -> CodecParameters {
        let mut p = CodecParameters::audio(CodecId::new("amr_wb"));
        p.sample_rate = Some(16_000);
        p.channels = Some(1);
        p
    }

    #[test]
    fn make_decoder_succeeds() {
        let r = make_decoder(&params_amr_wb());
        assert!(r.is_ok(), "AMR-WB make_decoder failed: {:?}", r.err());
    }

    #[test]
    fn make_decoder_rejects_bad_sample_rate() {
        let mut p = params_amr_wb();
        // 8 kHz is AMR-NB's rate — AMR-WB must refuse it.
        p.sample_rate = Some(8_000);
        let r = make_decoder(&p);
        assert!(r.is_err(), "AMR-WB must reject non-16 kHz");
    }

    #[test]
    fn make_decoder_rejects_stereo() {
        let mut p = params_amr_wb();
        p.channels = Some(2);
        let r = make_decoder(&p);
        assert!(r.is_err(), "AMR-WB must reject stereo");
    }

    #[test]
    fn send_packet_rejects_reserved_frame_type() {
        let mut dec = AmrWbAtDecoder::new(&params_amr_wb()).expect("decoder construct");
        // FT = 10 is reserved for AMR-WB; construct the TOC byte directly.
        let bad_toc = (10u8 << 3) | 0b100;
        let pkt = Packet::new(0, TimeBase::new(1, 16_000), vec![bad_toc; 17]);
        let r = dec.send_packet(&pkt);
        assert!(r.is_err(), "must reject reserved FT=10");
    }

    #[test]
    fn send_packet_rejects_size_mismatch() {
        let mut dec = AmrWbAtDecoder::new(&params_amr_wb()).expect("decoder construct");
        // Valid MR660 TOC but wrong byte count (16 instead of 17).
        let toc = make_toc(FrameType::Mr660);
        let pkt = Packet::new(0, TimeBase::new(1, 16_000), vec![toc; 16]);
        let r = dec.send_packet(&pkt);
        assert!(r.is_err(), "must reject size mismatch");
    }

    #[test]
    fn send_packet_accepts_no_data_frame() {
        let mut dec = AmrWbAtDecoder::new(&params_amr_wb()).expect("decoder construct");
        let toc = make_toc(FrameType::NoData);
        let pkt = Packet::new(0, TimeBase::new(1, 16_000), vec![toc]);
        let r = dec.send_packet(&pkt);
        assert!(r.is_ok(), "NO_DATA packet should queue without error");
    }

    #[test]
    fn send_packet_accepts_sid_frame() {
        let mut dec = AmrWbAtDecoder::new(&params_amr_wb()).expect("decoder construct");
        let toc = make_toc(FrameType::Sid);
        let pkt = Packet::new(0, TimeBase::new(1, 16_000), vec![toc; 6]);
        let r = dec.send_packet(&pkt);
        assert!(r.is_ok(), "SID packet should queue without error");
    }

    #[test]
    fn send_packet_accepts_speech_mode_packets() {
        let mut dec = AmrWbAtDecoder::new(&params_amr_wb()).expect("decoder construct");
        let modes = [
            FrameType::Mr660,
            FrameType::Mr885,
            FrameType::Mr1265,
            FrameType::Mr1425,
            FrameType::Mr1585,
            FrameType::Mr1825,
            FrameType::Mr1985,
            FrameType::Mr2305,
            FrameType::Mr2385,
        ];
        for ft in modes {
            let toc = make_toc(ft);
            let mut buf = vec![toc];
            buf.resize(ft.bytes_per_packet(), 0);
            let pkt = Packet::new(0, TimeBase::new(1, 16_000), buf);
            let r = dec.send_packet(&pkt);
            assert!(r.is_ok(), "{ft:?} packet should queue: {:?}", r.err());
        }
    }
}
