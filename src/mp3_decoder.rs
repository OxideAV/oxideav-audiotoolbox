//! MP3 decoder backed by macOS AudioConverter.
//!
//! Input:  one MPEG audio Layer III frame per `Packet` — the 4-byte
//!         header followed by optional 2-byte CRC, the side-information
//!         block, the main-data block, and the trailing padding byte
//!         when present. Frame length is taken from the parsed header
//!         per ISO/IEC 11172-3 §2.4.3.1 (Layer III: `144 * br / sr +
//!         padding` on MPEG-1, `72 * br / sr + padding` on MPEG-2 /
//!         MPEG-2.5).
//! Output: interleaved S16 PCM `AudioFrame`s. MPEG-1 Layer III decodes
//!         to 1152 samples per channel per frame; MPEG-2 LSF /
//!         MPEG-2.5 produce 576. AT may vend the PCM in sub-frame
//!         blocks per `FillComplexBuffer` call, so the per-frame size
//!         invariant the caller can rely on is `samples × channels ×
//!         2 == data.len()` (not a fixed 1152 / 576).
//!
//! MP3 on AudioToolbox is **decode-only** — `kAudioFormatMPEGLayer3`
//! is a decompression-only target; AT ships no MPEG-audio encoder.
//! There is no magic cookie: the per-frame 4-byte header carries
//! everything the converter needs (version / layer / bitrate / sample
//! rate / channel mode). AT can follow bitrate changes mid-stream
//! (VBR), but layer / sample-rate / channel-mode switches mid-stream
//! are stream-format changes the bridge **rejects with typed errors**
//! rather than tear down and rebuild the converter — exactly the
//! shape the iLBC / AMR-NB / AMR-WB / HE-AAC bridges use for their
//! invariant fields.
//!
//! The decoder uses the same persistent input-queue + one-packet-of-
//! slack lookahead pattern as the AMR decoders so the converter never
//! sees "0 packets" mid-stream (which would put it into a permanent
//! end-of-stream state).

use std::ffi::c_void;

use oxideav_core::Decoder;
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, Result, TimeBase};

use crate::mp3::{ChannelMode, FrameHeader, Layer, Version};
use crate::status::status_error;
use crate::sys::{
    self, AudioBuffer, AudioBufferList1, AudioConverterRef, AudioStreamBasicDescription,
    AudioStreamPacketDescription, NO_ERR,
};

/// State shared with the AudioConverter input callback.
///
/// Holds a FIFO of compressed MP3 frames. The callback pops the front
/// of the queue on each invocation and hands a single frame to AT with
/// its packet-description size filled from the byte count of the
/// popped buffer.
struct InputContext {
    queue: Vec<Vec<u8>>,
    /// Frames handed off to AT during the current `FillComplexBuffer`
    /// call — kept alive so the converter can still reference their
    /// bytes after the callback returns.
    handed_off: Vec<Vec<u8>>,
    /// Reusable packet descriptor rewritten on each callback fire.
    packet_desc: AudioStreamPacketDescription,
}

/// Resolved stream configuration latched from the first frame header.
///
/// Mid-stream changes to any of these fields are rejected — they
/// would require a full AudioConverter tear-down + rebuild, and the
/// MPEG-audio family does not produce well-formed streams that vary
/// these fields anyway.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StreamConfig {
    version: Version,
    layer: Layer,
    sample_rate: u32,
    channels: u32,
    channel_mode: ChannelMode,
    samples_per_frame: u32,
}

/// AudioConverter-backed MP3 decoder.
pub struct Mp3AtDecoder {
    codec_id: CodecId,
    /// Lazily constructed on the first packet — we need the first
    /// frame header to know the source sample rate / channel count.
    converter: AudioConverterRef,
    /// `Some(_)` once the first packet has been seen.
    config: Option<StreamConfig>,
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
unsafe impl Send for Mp3AtDecoder {}

impl Mp3AtDecoder {
    fn new(params: &CodecParameters) -> Result<Self> {
        // We don't construct the converter here — wait for the first
        // packet so we can derive the exact sample rate / channel
        // count from the frame header instead of trusting the caller-
        // supplied `CodecParameters` (which the muxer may have parsed
        // from container metadata that disagrees with the elementary
        // stream).
        Ok(Self {
            codec_id: params.codec_id.clone(),
            converter: std::ptr::null_mut(),
            config: None,
            pending: Vec::new(),
            input_queue: Vec::new(),
            pts: 0,
            time_base: TimeBase::new(1, params.sample_rate.unwrap_or(44_100) as i64),
            eof: false,
        })
    }

    /// Build the AudioConverter from the resolved stream configuration.
    fn configure(&mut self, header: &FrameHeader) -> Result<()> {
        if self.config.is_some() {
            return Ok(());
        }
        // Reject layers we don't have a registered factory for — the
        // bridge only registers an `mp3` factory; Layer I and Layer II
        // streams are not in scope and should fall through to the
        // pure-Rust pipeline.
        if header.layer != Layer::LayerIII {
            return Err(Error::unsupported(format!(
                "MP3 decoder: only Layer III is supported (got Layer {:?})",
                header.layer
            )));
        }
        let fw = sys::framework()
            .map_err(|e| Error::unsupported(format!("AudioToolbox unavailable: {e}")))?;
        let channels = header.channels();
        let in_asbd = AudioStreamBasicDescription::mpeg_layer3(
            header.sample_rate as f64,
            channels,
            header.samples_per_frame,
        );
        let out_asbd = AudioStreamBasicDescription::pcm_s16(header.sample_rate as f64, channels);

        let mut converter: AudioConverterRef = std::ptr::null_mut();
        let status = unsafe { sys::audio_converter_new(fw, &in_asbd, &out_asbd, &mut converter) };
        if status != NO_ERR {
            return Err(status_error("AudioConverterNew (MP3 dec)", status));
        }

        self.converter = converter;
        self.config = Some(StreamConfig {
            version: header.version,
            layer: header.layer,
            sample_rate: header.sample_rate,
            channels,
            channel_mode: header.channel_mode,
            samples_per_frame: header.samples_per_frame,
        });
        self.time_base = TimeBase::new(1, header.sample_rate as i64);
        Ok(())
    }

    /// Confirm a subsequent frame is consistent with the latched
    /// configuration. Bitrate changes are allowed (VBR is in scope);
    /// version / layer / sample-rate / channel-mode switches are not.
    fn check_compatible(&self, header: &FrameHeader) -> Result<()> {
        let cfg = self
            .config
            .as_ref()
            .expect("check_compatible called before configure");
        if header.layer != cfg.layer {
            return Err(Error::unsupported(format!(
                "MP3 decoder: mid-stream layer switch ({:?} → {:?}) is not supported",
                cfg.layer, header.layer
            )));
        }
        if header.version != cfg.version {
            return Err(Error::unsupported(format!(
                "MP3 decoder: mid-stream version switch ({:?} → {:?}) is not supported",
                cfg.version, header.version
            )));
        }
        if header.sample_rate != cfg.sample_rate {
            return Err(Error::unsupported(format!(
                "MP3 decoder: mid-stream sample-rate switch ({} → {} Hz) is not supported",
                cfg.sample_rate, header.sample_rate
            )));
        }
        let chans = header.channels();
        if chans != cfg.channels {
            return Err(Error::unsupported(format!(
                "MP3 decoder: mid-stream channel-count switch ({} → {}) is not supported",
                cfg.channels, chans
            )));
        }
        if header.channel_mode != cfg.channel_mode {
            return Err(Error::unsupported(format!(
                "MP3 decoder: mid-stream channel-mode switch ({:?} → {:?}) is not supported",
                cfg.channel_mode, header.channel_mode
            )));
        }
        Ok(())
    }

    /// Parse the leading header of a packet, verify the frame length
    /// matches the packet length, and queue it for decode.
    fn decode_packet(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < 4 {
            return Err(Error::invalid(format!(
                "MP3: packet too short for a frame header ({} bytes)",
                data.len()
            )));
        }
        let header_bytes = [data[0], data[1], data[2], data[3]];
        let header = FrameHeader::parse(header_bytes)
            .ok_or_else(|| Error::invalid("MP3: invalid frame header at packet start"))?;
        if data.len() != header.frame_length {
            return Err(Error::invalid(format!(
                "MP3: packet length {} doesn't match header frame_length {}",
                data.len(),
                header.frame_length
            )));
        }
        if self.config.is_none() {
            self.configure(&header)?;
        } else {
            self.check_compatible(&header)?;
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

    /// Single `FillComplexBuffer` call asking for one frame's worth of
    /// PCM. Returns `true` if a frame was produced.
    fn pull_one_pcm_frame(&mut self) -> Result<bool> {
        let cfg = self.config.as_ref().expect("configured");
        let fw = sys::framework()
            .map_err(|e| Error::unsupported(format!("AudioToolbox unavailable: {e}")))?;

        let frames_per_packet = cfg.samples_per_frame as usize;
        let channels = cfg.channels as usize;
        let buf_size = frames_per_packet * channels * 2; // S16 interleaved
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
                number_channels: cfg.channels,
                data_byte_size: buf_size as u32,
                data: pcm_buf.as_mut_ptr(),
            }],
        };

        let status = unsafe {
            sys::audio_converter_fill_complex_buffer(
                fw,
                self.converter,
                mp3_input_callback,
                &mut ctx as *mut InputContext as *mut c_void,
                &mut output_packet_count,
                &mut abl,
                std::ptr::null_mut(),
            )
        };

        // Restore whatever's left of the queue.
        self.input_queue = std::mem::take(&mut ctx.queue);

        if status != NO_ERR && status != 1 {
            return Err(status_error(
                "AudioConverterFillComplexBuffer (MP3 dec)",
                status,
            ));
        }

        let actual_bytes = abl.buffers[0].data_byte_size as usize;
        if actual_bytes == 0 || output_packet_count == 0 {
            return Ok(false);
        }
        let actual_samples = actual_bytes / (channels * 2);
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
        if self.config.is_none() {
            // Nothing was ever configured (no packets sent) — nothing to drain.
            return Ok(());
        }
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

impl Drop for Mp3AtDecoder {
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

impl Decoder for Mp3AtDecoder {
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

/// Input callback — supplies one compressed MP3 frame per call from
/// the front of the persistent queue. The frame's byte count is
/// written into the packet descriptor so AT can read the variable
/// size from the right place.
///
/// # Safety
/// `in_user_data` must point to a valid `InputContext` for the
/// duration of the call.
unsafe extern "C" fn mp3_input_callback(
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
    // Re-parse the header to recover samples_per_packet (used by AT to
    // assign a packet description). Cheap — header is the first 4
    // bytes and is always valid by the time we get here (decode_packet
    // already validated it).
    let header = FrameHeader::parse([pkt[0], pkt[1], pkt[2], pkt[3]]);
    let samples = header.map(|h| h.samples_per_frame).unwrap_or(1152);

    ctx.packet_desc = AudioStreamPacketDescription {
        start_offset: 0,
        variable_frames_in_packet: samples,
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
    Ok(Box::new(Mp3AtDecoder::new(params)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{CodecId, CodecParameters};

    fn params_mp3() -> CodecParameters {
        let mut p = CodecParameters::audio(CodecId::new("mp3"));
        p.sample_rate = Some(44_100);
        p.channels = Some(2);
        p
    }

    fn synth_mp3_frame(
        version: Version,
        layer: Layer,
        bitrate_index: u8,
        sr_index: u8,
        padding: bool,
        channel_mode_bits: u8,
    ) -> Vec<u8> {
        let (ext, id) = match version {
            Version::Mpeg1 => (1u8, 1u8),
            Version::Mpeg2 => (1u8, 0u8),
            Version::Mpeg25 => (0u8, 0u8),
        };
        let layer_bits = match layer {
            Layer::LayerI => 0b11u8,
            Layer::LayerII => 0b10u8,
            Layer::LayerIII => 0b01u8,
        };
        let pad = if padding { 1u8 } else { 0u8 };
        let b0 = 0xFFu8;
        let b1 = (0b111u8 << 5) | (ext << 4) | (id << 3) | (layer_bits << 1) | 1u8; // no CRC
        let b2 = (bitrate_index << 4) | (sr_index << 2) | (pad << 1);
        let b3 = (channel_mode_bits & 0b11) << 6;
        let header = [b0, b1, b2, b3];
        let parsed = FrameHeader::parse(header).expect("synthetic header parses");
        let mut buf = header.to_vec();
        buf.resize(parsed.frame_length, 0);
        buf
    }

    #[test]
    fn make_decoder_succeeds() {
        let r = make_decoder(&params_mp3());
        assert!(r.is_ok(), "MP3 make_decoder failed: {:?}", r.err());
    }

    #[test]
    fn send_packet_rejects_short_buffer() {
        let mut dec = Mp3AtDecoder::new(&params_mp3()).expect("decoder construct");
        let pkt = Packet::new(0, TimeBase::new(1, 44_100), vec![0xFF, 0xFB]); // only 2 bytes
        let r = dec.send_packet(&pkt);
        assert!(r.is_err(), "must reject too-short packet");
    }

    #[test]
    fn send_packet_rejects_invalid_header() {
        let mut dec = Mp3AtDecoder::new(&params_mp3()).expect("decoder construct");
        let pkt = Packet::new(0, TimeBase::new(1, 44_100), vec![0x00; 417]);
        let r = dec.send_packet(&pkt);
        assert!(r.is_err(), "must reject non-MPEG-audio packet");
    }

    #[test]
    fn send_packet_rejects_layer1_layer2() {
        // Only Layer III is in scope for the AT factory; Layer I and
        // Layer II must surface a typed unsupported error so the
        // registry falls through to the pure-Rust path.
        let mut dec = Mp3AtDecoder::new(&params_mp3()).expect("decoder construct");
        let pkt = Packet::new(
            0,
            TimeBase::new(1, 44_100),
            synth_mp3_frame(Version::Mpeg1, Layer::LayerI, 1, 0, false, 0b00),
        );
        assert!(dec.send_packet(&pkt).is_err());

        let mut dec2 = Mp3AtDecoder::new(&params_mp3()).expect("decoder construct");
        let pkt2 = Packet::new(
            0,
            TimeBase::new(1, 44_100),
            synth_mp3_frame(Version::Mpeg1, Layer::LayerII, 3, 0, false, 0b00),
        );
        assert!(dec2.send_packet(&pkt2).is_err());
    }

    #[test]
    fn send_packet_rejects_packet_length_mismatch() {
        let mut dec = Mp3AtDecoder::new(&params_mp3()).expect("decoder construct");
        let mut buf = synth_mp3_frame(Version::Mpeg1, Layer::LayerIII, 9, 0, false, 0b00);
        buf.pop(); // 416 instead of 417
        let pkt = Packet::new(0, TimeBase::new(1, 44_100), buf);
        let r = dec.send_packet(&pkt);
        assert!(
            r.is_err(),
            "must reject packet length / frame-length mismatch"
        );
    }

    #[test]
    fn send_packet_rejects_mid_stream_layer_switch() {
        let mut dec = Mp3AtDecoder::new(&params_mp3()).expect("decoder construct");
        // First packet: MPEG-1 Layer III @ 128 kbit/s stereo.
        let p1 = Packet::new(
            0,
            TimeBase::new(1, 44_100),
            synth_mp3_frame(Version::Mpeg1, Layer::LayerIII, 9, 0, false, 0b00),
        );
        // AT will likely fail to actually decode all-zero payload, but
        // the bridge accepts the packet through send_packet.
        let _ = dec.send_packet(&p1);
        // Second packet: Layer II — must be rejected as an unsupported
        // mid-stream switch.
        let p2 = Packet::new(
            0,
            TimeBase::new(1, 44_100),
            synth_mp3_frame(Version::Mpeg1, Layer::LayerII, 10, 0, false, 0b00),
        );
        let r = dec.send_packet(&p2);
        assert!(r.is_err(), "must reject mid-stream layer switch");
    }

    #[test]
    fn send_packet_rejects_mid_stream_sample_rate_switch() {
        let mut dec = Mp3AtDecoder::new(&params_mp3()).expect("decoder construct");
        let p1 = Packet::new(
            0,
            TimeBase::new(1, 44_100),
            synth_mp3_frame(Version::Mpeg1, Layer::LayerIII, 9, 0, false, 0b00),
        );
        let _ = dec.send_packet(&p1);
        // Switch to 48 kHz — sr_index 1 on the MPEG-1 row.
        let p2 = Packet::new(
            0,
            TimeBase::new(1, 48_000),
            synth_mp3_frame(Version::Mpeg1, Layer::LayerIII, 9, 1, false, 0b00),
        );
        let r = dec.send_packet(&p2);
        assert!(r.is_err(), "must reject mid-stream sample-rate switch");
    }

    #[test]
    fn send_packet_rejects_mid_stream_channel_mode_switch() {
        let mut dec = Mp3AtDecoder::new(&params_mp3()).expect("decoder construct");
        let p1 = Packet::new(
            0,
            TimeBase::new(1, 44_100),
            synth_mp3_frame(Version::Mpeg1, Layer::LayerIII, 9, 0, false, 0b00), // stereo
        );
        let _ = dec.send_packet(&p1);
        let p2 = Packet::new(
            0,
            TimeBase::new(1, 44_100),
            synth_mp3_frame(Version::Mpeg1, Layer::LayerIII, 9, 0, false, 0b11), // mono
        );
        let r = dec.send_packet(&p2);
        assert!(r.is_err(), "must reject mid-stream channel-mode switch");
    }

    #[test]
    fn send_packet_allows_bitrate_change_vbr() {
        // VBR: same layer + sample-rate + channel-mode, different
        // bitrate index — bridge must accept it.
        let mut dec = Mp3AtDecoder::new(&params_mp3()).expect("decoder construct");
        let p1 = Packet::new(
            0,
            TimeBase::new(1, 44_100),
            synth_mp3_frame(Version::Mpeg1, Layer::LayerIII, 9, 0, false, 0b00), // 128 kbit/s
        );
        let _ = dec.send_packet(&p1);
        let p2 = Packet::new(
            0,
            TimeBase::new(1, 44_100),
            synth_mp3_frame(Version::Mpeg1, Layer::LayerIII, 11, 0, false, 0b00), // 192 kbit/s
        );
        let r = dec.send_packet(&p2);
        assert!(
            r.is_ok(),
            "VBR (bitrate-only change) must be accepted: {:?}",
            r.err()
        );
    }
}
