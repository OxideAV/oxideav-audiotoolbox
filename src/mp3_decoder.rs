//! MPEG audio Layer I/II/III decoder backed by macOS AudioConverter.
//!
//! Input:  one MPEG audio frame per `Packet` — a 32-bit header
//!         followed by the per-frame compressed payload (length
//!         derived from the header). The decoder validates each
//!         incoming frame against [`crate::mp3::FrameHeader`] before
//!         queueing it.
//! Output: interleaved S16 PCM `AudioFrame`s at the rate / channel
//!         count carried in the first frame's header.
//!
//! AudioToolbox exposes MP3 / MP2 / MP1 as **decode-only** targets —
//! there is no paired AT encoder for any MPEG audio layer, so this
//! module registers only a decoder factory under codec id `"mp3"`
//! (and aliased `"mp2"` / `"mp1"` if the user maps them in). Layer
//! selection is automatic: the decoder inspects the first frame's
//! Layer field and constructs the matching ASBD format ID.
//!
//! Converter wiring follows the same persistent input-queue +
//! one-packet-of-slack lookahead pattern as the iLBC / AMR-NB / AAC
//! decoders so AT never sees "0 packets" mid-stream (which would
//! park the converter in a permanent EOS state). The lazy
//! first-packet configure phase mirrors the AAC LC decoder, which
//! also reads its ASBD from the first ADTS frame.

use std::ffi::c_void;

use oxideav_core::Decoder;
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, Result, TimeBase};

use crate::mp3::{FrameHeader, Layer};
use crate::sys::{
    self, AudioBuffer, AudioBufferList1, AudioConverterRef, AudioStreamBasicDescription,
    AudioStreamPacketDescription, NO_ERR,
};

/// State shared with the AudioConverter input callback.
///
/// Holds a FIFO of compressed MPEG-audio frames. The callback pops
/// the front of the queue on each invocation and hands a single
/// packet to AT with its packet-description size filled from the
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

/// AudioConverter-backed MPEG audio Layer I/II/III decoder.
pub struct Mp3AtDecoder {
    codec_id: CodecId,
    /// Configured layer (from first packet).
    layer: Option<Layer>,
    /// Configured sample rate (from first packet header).
    sample_rate: u32,
    /// Channel count (from first packet header).
    channels: u16,
    /// PCM frames per packet for the configured layer. 1152 for
    /// MPEG-1 Layer II/III, 576 for MPEG-2 LSF Layer III, 384 for
    /// Layer I. Used to size the output buffer per
    /// `FillComplexBuffer` call.
    frames_per_packet: u32,
    /// Opaque converter handle, null until the first packet arrives.
    converter: AudioConverterRef,
    /// FIFO of PCM frames ready to be vended via `receive_frame`.
    pending: Vec<Frame>,
    /// Persistent input-packet queue.
    input_queue: Vec<Vec<u8>>,
    pts: i64,
    #[allow(dead_code)]
    time_base: TimeBase,
    eof: bool,
    configured: bool,
}

// SAFETY: AudioConverterRef is used single-threaded inside the Decoder
// impl. The `*mut` inside it is the opaque handle Apple guarantees is
// usable from the thread it was created on; we never move the handle
// across threads.
unsafe impl Send for Mp3AtDecoder {}

impl Mp3AtDecoder {
    fn new(params: &CodecParameters) -> Result<Self> {
        // Pre-validate framework availability so we surface a clean
        // error from `make_decoder` rather than waiting until the
        // first packet arrives.
        sys::framework().map_err(|e| Error::other(format!("AudioToolbox unavailable: {e}")))?;

        Ok(Self {
            codec_id: params.codec_id.clone(),
            layer: None,
            sample_rate: params.sample_rate.unwrap_or(0),
            channels: params.channels.unwrap_or(0),
            frames_per_packet: 0,
            converter: std::ptr::null_mut(),
            pending: Vec::new(),
            input_queue: Vec::new(),
            pts: 0,
            time_base: TimeBase::new(1, params.sample_rate.unwrap_or(44_100) as i64),
            eof: false,
            configured: false,
        })
    }

    /// Configure the converter from a parsed frame header.
    ///
    /// MPEG audio bitstreams are allowed to switch bitrate between
    /// frames (VBR) but **not** layer / sample rate / channel mode.
    /// We commit those three on the first frame and reject later
    /// frames whose header disagrees — that's the same invariant
    /// AudioConverter enforces internally.
    fn configure(&mut self, h: &FrameHeader) -> Result<()> {
        let fw =
            sys::framework().map_err(|e| Error::other(format!("AudioToolbox unavailable: {e}")))?;

        let channels = h.channel_mode.channel_count();
        let in_asbd = match h.layer {
            Layer::Layer3 => {
                AudioStreamBasicDescription::mpeg_layer_3(h.sample_rate as f64, channels as u32)
            }
            Layer::Layer2 => {
                AudioStreamBasicDescription::mpeg_layer_2(h.sample_rate as f64, channels as u32)
            }
            Layer::Layer1 => {
                AudioStreamBasicDescription::mpeg_layer_1(h.sample_rate as f64, channels as u32)
            }
        };
        let out_asbd = AudioStreamBasicDescription::pcm_s16(h.sample_rate as f64, channels as u32);

        let mut converter: AudioConverterRef = std::ptr::null_mut();
        let status = unsafe { sys::audio_converter_new(fw, &in_asbd, &out_asbd, &mut converter) };
        if status != NO_ERR {
            return Err(Error::other(format!(
                "AudioConverterNew (MP3 dec, layer={:?}, rate={}, ch={}) failed: OSStatus {status}",
                h.layer, h.sample_rate, channels
            )));
        }

        self.converter = converter;
        self.layer = Some(h.layer);
        self.sample_rate = h.sample_rate;
        self.channels = channels;
        self.frames_per_packet = h.samples();
        self.time_base = TimeBase::new(1, h.sample_rate as i64);
        self.configured = true;
        Ok(())
    }

    /// Validate an MPEG audio frame against its header and queue it
    /// for decode. Lazily configures the converter on the first frame.
    fn decode_packet(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < 4 {
            return Err(Error::invalid(format!(
                "MP3: packet too short for header ({} bytes)",
                data.len()
            )));
        }
        let header_bytes = [data[0], data[1], data[2], data[3]];
        let h = FrameHeader::parse(header_bytes).ok_or_else(|| {
            Error::invalid(format!(
                "MP3: invalid frame header 0x{:02x}{:02x}{:02x}{:02x}",
                data[0], data[1], data[2], data[3]
            ))
        })?;

        // Sanity-check the declared frame length matches the packet
        // size we were handed. We tolerate one extra trailing byte
        // (some demuxers include the ID3-tag boundary byte) but
        // refuse anything that looks like a split frame.
        let expected = h.frame_length() as usize;
        if data.len() < expected {
            return Err(Error::invalid(format!(
                "MP3: {:?} frame declares {expected} bytes but packet carries {}",
                h.layer,
                data.len()
            )));
        }

        if !self.configured {
            self.configure(&h)?;
        } else {
            // Reject mid-stream layer / sample-rate / channel-mode
            // switches — AT would refuse them too, and the typed error
            // is more diagnostic than `kAudioConverterErr_*`.
            if Some(h.layer) != self.layer {
                return Err(Error::invalid(format!(
                    "MP3: mid-stream layer change ({:?} → {:?}) not supported",
                    self.layer.unwrap(),
                    h.layer
                )));
            }
            if h.sample_rate != self.sample_rate {
                return Err(Error::invalid(format!(
                    "MP3: mid-stream sample-rate change ({} → {}) not supported",
                    self.sample_rate, h.sample_rate
                )));
            }
            if h.channel_mode.channel_count() != self.channels {
                return Err(Error::invalid(format!(
                    "MP3: mid-stream channel-count change ({} → {}) not supported",
                    self.channels,
                    h.channel_mode.channel_count()
                )));
            }
        }

        self.input_queue.push(data[..expected].to_vec());
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

        let frames_per_packet = self.frames_per_packet as usize;
        let channels = self.channels as usize;
        let buf_size = frames_per_packet * 2 * channels; // S16
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
                number_channels: self.channels as u32,
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

        self.input_queue = std::mem::take(&mut ctx.queue);

        // AT returns `1` ("input-data-end") as a soft signal that the
        // callback returned zero packets — not an error condition for
        // our purposes; the actual `data_byte_size` tells us how much
        // PCM was emitted on this call.
        if status != NO_ERR && status != 1 {
            return Err(Error::other(format!(
                "AudioConverterFillComplexBuffer (MP3 dec) failed: OSStatus {status}"
            )));
        }

        let actual_bytes = abl.buffers[0].data_byte_size as usize;
        if actual_bytes == 0 || output_packet_count == 0 {
            return Ok(false);
        }
        let bytes_per_pcm_frame = 2 * channels;
        let actual_samples = actual_bytes / bytes_per_pcm_frame;
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
        if !self.configured {
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

/// Input callback — supplies one compressed MPEG audio frame per call
/// from the front of the persistent queue. The packet's byte count is
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
    // `variable_frames_in_packet = 0` is the AT convention for
    // compressed packets that vend a header-driven PCM frame count.
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

    #[test]
    fn make_decoder_succeeds() {
        let r = make_decoder(&params_mp3());
        assert!(r.is_ok(), "MP3 make_decoder failed: {:?}", r.err());
    }

    /// Build a minimum-viable MPEG-1 Layer III 128 kbit/s 44.1 kHz
    /// stereo header followed by a zero-payload body. AT will refuse
    /// to actually decode this (the bitstream is structurally invalid
    /// past the header), but the header validation + lazy-configure
    /// path runs without ever calling `FillComplexBuffer` since the
    /// look-ahead slack policy holds the only queued packet.
    fn synth_mp3_packet() -> Vec<u8> {
        // Header bytes for: ver=11 (MPEG-1), layer=01 (L3), crc=1
        // (no CRC), br_idx=9 (128k), sr_idx=0 (44_100), pad=0,
        // mode=00 (stereo).
        let b0 = 0xFF;
        let b1 = 0xE0 | (0b11 << 3) | (0b01 << 1) | 1;
        let b2 = 9u8 << 4; // br_idx=9 (128k), sr_idx=0 (44.1k), pad=0
        let b3 = 0u8;
        let mut pkt = vec![b0, b1, b2, b3];
        // Frame length = 144 * 128000 / 44100 = 417. Pad with zeros.
        pkt.resize(417, 0);
        pkt
    }

    #[test]
    fn send_packet_lazy_configures() {
        let mut dec = Mp3AtDecoder::new(&params_mp3()).expect("decoder construct");
        let tb = TimeBase::new(1, 44_100);
        let pkt = Packet::new(0, tb, synth_mp3_packet());
        let r = dec.send_packet(&pkt);
        assert!(r.is_ok(), "send_packet: {:?}", r.err());
        assert!(dec.configured, "first packet must configure converter");
        assert_eq!(dec.sample_rate, 44_100);
        assert_eq!(dec.channels, 2);
        assert_eq!(dec.frames_per_packet, 1152);
        assert_eq!(dec.layer, Some(Layer::Layer3));
    }

    #[test]
    fn send_packet_rejects_short_packet() {
        let mut dec = Mp3AtDecoder::new(&params_mp3()).expect("decoder construct");
        let tb = TimeBase::new(1, 44_100);
        let pkt = Packet::new(0, tb, vec![0xFF, 0xFB]); // < 4 bytes
        let r = dec.send_packet(&pkt);
        assert!(r.is_err(), "must reject packet smaller than the header");
    }

    #[test]
    fn send_packet_rejects_invalid_header() {
        let mut dec = Mp3AtDecoder::new(&params_mp3()).expect("decoder construct");
        let tb = TimeBase::new(1, 44_100);
        // No sync (top 11 bits not all 1).
        let pkt = Packet::new(0, tb, vec![0x00, 0x00, 0x00, 0x00]);
        let r = dec.send_packet(&pkt);
        assert!(r.is_err(), "must reject packet with bad header");
    }

    #[test]
    fn send_packet_rejects_undersized_frame_body() {
        let mut dec = Mp3AtDecoder::new(&params_mp3()).expect("decoder construct");
        let tb = TimeBase::new(1, 44_100);
        // Valid header but only 4 bytes total (declares 417).
        let mut pkt_bytes = synth_mp3_packet();
        pkt_bytes.truncate(64); // way short
        let pkt = Packet::new(0, tb, pkt_bytes);
        let r = dec.send_packet(&pkt);
        assert!(
            r.is_err(),
            "must reject packet shorter than declared length"
        );
    }

    #[test]
    fn send_packet_rejects_layer_switch() {
        let mut dec = Mp3AtDecoder::new(&params_mp3()).expect("decoder construct");
        let tb = TimeBase::new(1, 44_100);
        // First — Layer III at 44.1 / stereo — configures.
        let pkt = Packet::new(0, tb, synth_mp3_packet());
        dec.send_packet(&pkt).expect("first packet");

        // Second — Layer II header at the same rate / channels.
        let b0 = 0xFF;
        let b1 = 0xE0 | (0b11 << 3) | (0b10 << 1) | 1;
        let b2 = (10u8 << 4) | (1u8 << 2); // 192k L2 @ 48k — pretend match would still fail rate
        let b3 = 0u8;
        let mut switch = vec![b0, b1, b2, b3];
        // L2 192k @ 48k: 144*192000/48000 = 576
        switch.resize(576, 0);
        let pkt2 = Packet::new(0, tb, switch);
        let r = dec.send_packet(&pkt2);
        assert!(r.is_err(), "must reject mid-stream layer change");
    }

    #[test]
    fn reset_clears_state() {
        let mut dec = Mp3AtDecoder::new(&params_mp3()).expect("decoder construct");
        let tb = TimeBase::new(1, 44_100);
        let pkt = Packet::new(0, tb, synth_mp3_packet());
        dec.send_packet(&pkt).expect("first packet");
        dec.reset().expect("reset");
        // After reset, the decoder should accept new packets without
        // returning Eof from a previous flush.
        let pkt2 = Packet::new(0, tb, synth_mp3_packet());
        dec.send_packet(&pkt2)
            .expect("send_packet after reset must succeed");
    }
}
