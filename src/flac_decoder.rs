//! FLAC decoder backed by macOS AudioConverter (`kAudioFormatFLAC`).
//!
//! Input:  one **raw FLAC frame** per `Packet` — the 6–18 byte frame
//!         header (sync code + blocking strategy + block size + sample
//!         rate + channel assignment + bits-per-sample + UTF-8 frame
//!         number + optional block-size / sample-rate escape bytes +
//!         CRC-8) followed by one subframe per channel and the
//!         16-bit CRC frame footer (RFC 9639 §9).
//! Output: interleaved S16 PCM `AudioFrame`s. AT may vend PCM in
//!         sub-block chunks per `FillComplexBuffer` call, so the
//!         per-frame size invariant the caller can rely on is
//!         `samples × channels × 2 == data.len()`.
//!
//! FLAC on AudioToolbox is **symmetric** — both decode and encode are
//! exposed on macOS 13+. This module handles the decode side; the
//! encode side ships in a paired `flac_encoder` module in a follow-up
//! round. AT consumes the FLAC stream metadata header as the
//! decompression "magic cookie" wrapped in a `dfLa` ISOBMFF box
//! (Xiph's "FLAC in ISOBMFF" specific box: 8-byte box header +
//! 4-byte FullBox + metadata block chain). See `crate::flac` module
//! docs for the empirical-probe-derived rationale.
//!
//! The decoder uses the same persistent input-queue + one-packet-of-
//! slack lookahead pattern as the MP3 / AMR-NB / AMR-WB bridges so AT
//! never sees "0 packets" mid-stream (which would put it into a
//! permanent end-of-stream state).
//!
//! ## Magic-cookie resolution
//!
//! 1. If `CodecParameters::extradata` is at least the minimum cookie
//!    length (50 bytes — the `dfLa` box) and parses as a valid FLAC
//!    cookie, we use it verbatim.
//! 2. If `extradata` is exactly 34 bytes (a bare STREAMINFO body —
//!    the canonical RFC 9639 wire form), we wrap it in a `dfLa`
//!    box before forwarding to AT.
//! 3. Otherwise we synthesise a minimal cookie from the explicit
//!    `sample_rate / channels / sample_format` parameters. This is
//!    the standalone-test path: the caller knows what it's feeding.
//!
//! ## Frame consistency invariants
//!
//! Mid-stream changes to (sample_rate / channels / bits_per_sample)
//! are rejected with typed `Error::unsupported` — they would require
//! tearing down and rebuilding the AudioConverter, and well-formed
//! FLAC streams never change these fields within one stream. Block
//! size is allowed to vary (variable-blocksize FLAC streams are in
//! scope per RFC 9639 §9.1.1).

use std::ffi::c_void;

use oxideav_core::Decoder;
use oxideav_core::{
    AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, Result, SampleFormat, TimeBase,
};

use crate::flac::{
    self, ChannelAssignment, FrameHeader, StreamInfo, MAGIC_COOKIE_MIN_LEN, STREAMINFO_BODY_LEN,
};
use crate::status::status_error;
use crate::sys::{
    self, AudioBuffer, AudioBufferList1, AudioConverterRef, AudioStreamBasicDescription,
    AudioStreamPacketDescription, K_AUDIO_CONVERTER_DECOMPRESSION_MAGIC_COOKIE, NO_ERR,
};

/// State shared with the AudioConverter input callback.
struct InputContext {
    queue: Vec<Vec<u8>>,
    /// Frames handed off to AT during the current `FillComplexBuffer`
    /// call — kept alive so the converter can still reference their
    /// bytes after the callback returns.
    handed_off: Vec<Vec<u8>>,
    /// Per-call packet descriptor (rewritten on each invocation).
    packet_desc: AudioStreamPacketDescription,
}

/// AudioConverter-backed FLAC decoder.
pub struct FlacAtDecoder {
    codec_id: CodecId,
    info: StreamInfo,
    converter: AudioConverterRef,
    pending: Vec<Frame>,
    input_queue: Vec<Vec<u8>>,
    pts: i64,
    #[allow(dead_code)]
    time_base: TimeBase,
    eof: bool,
}

// SAFETY: AudioConverterRef is used single-threaded inside the Decoder
// impl. We never move the handle across threads during one call.
unsafe impl Send for FlacAtDecoder {}

impl FlacAtDecoder {
    fn new(params: &CodecParameters) -> Result<Self> {
        let fw = sys::framework()
            .map_err(|e| Error::unsupported(format!("AudioToolbox unavailable: {e}")))?;

        // Resolve the STREAMINFO from extradata (cookie or bare body)
        // or synthesise a placeholder from explicit parameters.
        let (cookie, info) = resolve_cookie_and_info(params)?;

        let bit_depth_flag = flac::bit_depth_flag(info.bits_per_sample).ok_or_else(|| {
            Error::unsupported(format!(
                "FLAC decoder: unsupported bits_per_sample {}",
                info.bits_per_sample
            ))
        })?;

        let in_asbd = AudioStreamBasicDescription::flac(
            info.sample_rate as f64,
            info.channels as u32,
            bit_depth_flag,
            info.max_blocksize as u32,
        );
        let out_asbd =
            AudioStreamBasicDescription::pcm_s16(info.sample_rate as f64, info.channels as u32);

        let mut converter: AudioConverterRef = std::ptr::null_mut();
        let status = unsafe { sys::audio_converter_new(fw, &in_asbd, &out_asbd, &mut converter) };
        if status != NO_ERR {
            return Err(status_error("AudioConverterNew (FLAC dec)", status));
        }

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
                "AudioConverterSetProperty(DecompressionMagicCookie / FLAC)",
                status,
            ));
        }

        let tb = TimeBase::new(1, info.sample_rate as i64);
        Ok(Self {
            codec_id: params.codec_id.clone(),
            info,
            converter,
            pending: Vec::new(),
            input_queue: Vec::new(),
            pts: 0,
            time_base: tb,
            eof: false,
        })
    }

    /// Parse the leading header of a packet, verify it's consistent
    /// with the latched STREAMINFO, and queue it for decode.
    fn decode_packet(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < 5 {
            return Err(Error::invalid(format!(
                "FLAC: packet too short for a frame header ({} bytes)",
                data.len()
            )));
        }
        let header = flac::parse_frame_header(data, &self.info)
            .ok_or_else(|| Error::invalid("FLAC: invalid frame header at packet start"))?;
        self.check_compatible(&header)?;
        self.input_queue.push(data.to_vec());
        self.drain_pcm()?;
        Ok(())
    }

    /// Confirm a subsequent frame is consistent with the latched
    /// STREAMINFO. Block-size changes ARE allowed (variable-blocksize
    /// streams); sample-rate / channel-count / bit-depth switches
    /// are not.
    fn check_compatible(&self, header: &FrameHeader) -> Result<()> {
        if header.sample_rate != self.info.sample_rate {
            return Err(Error::unsupported(format!(
                "FLAC decoder: mid-stream sample-rate switch ({} → {} Hz) is not supported",
                self.info.sample_rate, header.sample_rate
            )));
        }
        if header.channels() != self.info.channels {
            return Err(Error::unsupported(format!(
                "FLAC decoder: mid-stream channel-count switch ({} → {}) is not supported",
                self.info.channels,
                header.channels()
            )));
        }
        if header.bits_per_sample != self.info.bits_per_sample {
            return Err(Error::unsupported(format!(
                "FLAC decoder: mid-stream bit-depth switch ({} → {}) is not supported",
                self.info.bits_per_sample, header.bits_per_sample
            )));
        }
        Ok(())
    }

    /// Drain PCM frames from AT until either the converter signals it
    /// needs more input, or the queue is down to a single packet
    /// (preserving the look-ahead tail).
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
        let fw = sys::framework()
            .map_err(|e| Error::unsupported(format!("AudioToolbox unavailable: {e}")))?;

        let channels = self.info.channels as usize;
        let frames_per_packet = self.info.max_blocksize as usize;
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
                number_channels: self.info.channels as u32,
                data_byte_size: buf_size as u32,
                data: pcm_buf.as_mut_ptr(),
            }],
        };

        let status = unsafe {
            sys::audio_converter_fill_complex_buffer(
                fw,
                self.converter,
                flac_input_callback,
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
                "AudioConverterFillComplexBuffer (FLAC dec)",
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
        for _ in 0..256 {
            if self.input_queue.is_empty() {
                break;
            }
            if !self.pull_one_pcm_frame()? {
                break;
            }
        }
        // Final pull to flush any internal look-ahead PCM.
        for _ in 0..2 {
            if !self.pull_one_pcm_frame()? {
                break;
            }
        }
        Ok(())
    }
}

impl Drop for FlacAtDecoder {
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

impl Decoder for FlacAtDecoder {
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

/// Resolve the (cookie bytes, parsed STREAMINFO) pair from a
/// `CodecParameters`. The caller may supply either a full magic
/// cookie (full or partial `.flac` metadata chain) via `extradata`,
/// or rely on the explicit `sample_rate / channels / sample_format`
/// fields if they didn't have a cookie to forward.
fn resolve_cookie_and_info(params: &CodecParameters) -> Result<(Vec<u8>, StreamInfo)> {
    if params.extradata.len() >= MAGIC_COOKIE_MIN_LEN {
        // Treat as a full cookie / .flac prefix.
        if let Some(info) = flac::parse_magic_cookie(&params.extradata) {
            return Ok((params.extradata.clone(), info));
        }
        return Err(Error::invalid(
            "FLAC: extradata is large enough for a cookie but failed to parse",
        ));
    }
    if params.extradata.len() == STREAMINFO_BODY_LEN {
        // Bare 34-byte STREAMINFO body — wrap it.
        let info = StreamInfo::parse(&params.extradata)
            .ok_or_else(|| Error::invalid("FLAC: malformed STREAMINFO body in extradata"))?;
        let cookie = flac::build_magic_cookie(&info);
        return Ok((cookie, info));
    }
    // No cookie supplied → synthesise from explicit fields. Default
    // max block size of 4608 (= RFC 9639 §9.1.2 Table 1 code 5)
    // is what every fixture in `docs/audio/flac/fixtures/` uses;
    // it gives AT enough output-buffer slack for any in-scope
    // stream.
    let sr = params
        .sample_rate
        .ok_or_else(|| Error::invalid("FLAC: sample_rate required when no magic cookie"))?;
    let ch = params
        .channels
        .ok_or_else(|| Error::invalid("FLAC: channels required when no magic cookie"))?
        as u8;
    let bps = match params.sample_format {
        Some(SampleFormat::S16) | None => 16u8,
        Some(SampleFormat::S32) => 24u8,
        Some(SampleFormat::F32) => 16u8,
        Some(other) => {
            return Err(Error::unsupported(format!(
                "FLAC decoder: unsupported sample_format {other:?}"
            )))
        }
    };
    if !(1..=8).contains(&ch) {
        return Err(Error::unsupported(format!(
            "FLAC decoder: channel count {ch} out of range (1..=8)"
        )));
    }
    // RFC 9639 §8.1 allows min_blocksize == 16, but AT's FLAC magic
    // cookie validator rejects anything below 192 (the smallest code-
    // table block size). Pin synth min to 192 so the cookie passes
    // AT's sanity check even for streams that turn out to be fixed-
    // blocksize at the canonical 4608.
    let info = StreamInfo {
        min_blocksize: 192,
        max_blocksize: 4608,
        min_framesize: 0,
        max_framesize: 0,
        sample_rate: sr,
        channels: ch,
        bits_per_sample: bps,
        total_samples: 0,
        md5: [0u8; 16],
    };
    let cookie = flac::build_magic_cookie(&info);
    Ok((cookie, info))
}

/// Map the `ChannelAssignment` to the channel count AT will emit.
/// Currently the same as `ChannelAssignment::channel_count` — kept
/// here so a future bridge addition (e.g. surfacing the decorrelation
/// mode through `AudioFrame::channel_layout`) has a single place to
/// touch.
#[allow(dead_code)]
fn channel_layout_label(c: ChannelAssignment) -> &'static str {
    match c {
        ChannelAssignment::Independent(_) => "independent",
        ChannelAssignment::LeftSide => "left+side",
        ChannelAssignment::SideRight => "side+right",
        ChannelAssignment::MidSide => "mid+side",
    }
}

/// Input callback — supplies one compressed FLAC frame per call from
/// the front of the persistent queue. The frame's byte count is
/// written into the packet descriptor so AT can read the variable
/// size from the right place.
///
/// # Safety
/// `in_user_data` must point to a valid `InputContext` for the
/// duration of the call.
unsafe extern "C" fn flac_input_callback(
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
        // FLAC's per-packet sample count varies (variable-blocksize
        // streams) but AT doesn't actually consult this field for
        // FLAC decode — it parses the frame header itself. Set it
        // to 0 (= "let the converter figure it out").
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
    Ok(Box::new(FlacAtDecoder::new(params)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{CodecId, CodecParameters};

    fn params_flac_44100_stereo_16bit() -> CodecParameters {
        let mut p = CodecParameters::audio(CodecId::new("flac"));
        p.sample_rate = Some(44_100);
        p.channels = Some(2);
        p.sample_format = Some(SampleFormat::S16);
        p
    }

    #[test]
    fn make_decoder_succeeds_without_cookie() {
        // Synthesise-from-parameters path.
        let r = make_decoder(&params_flac_44100_stereo_16bit());
        assert!(r.is_ok(), "make_decoder failed: {:?}", r.err());
    }

    #[test]
    fn make_decoder_succeeds_with_full_cookie() {
        let info = StreamInfo {
            min_blocksize: 4608,
            max_blocksize: 4608,
            min_framesize: 0,
            max_framesize: 0,
            sample_rate: 96_000,
            channels: 1,
            bits_per_sample: 24,
            total_samples: 0,
            md5: [0u8; 16],
        };
        let mut p = CodecParameters::audio(CodecId::new("flac"));
        p.extradata = flac::build_magic_cookie(&info);
        let r = make_decoder(&p);
        assert!(r.is_ok(), "make_decoder w/ cookie failed: {:?}", r.err());
    }

    #[test]
    fn make_decoder_accepts_bare_streaminfo_body_as_extradata() {
        let info = StreamInfo {
            min_blocksize: 4096,
            max_blocksize: 4096,
            min_framesize: 0,
            max_framesize: 0,
            sample_rate: 48_000,
            channels: 2,
            bits_per_sample: 16,
            total_samples: 0,
            md5: [0u8; 16],
        };
        let mut p = CodecParameters::audio(CodecId::new("flac"));
        p.extradata = info.to_bytes().to_vec();
        let r = make_decoder(&p);
        assert!(
            r.is_ok(),
            "make_decoder w/ bare STREAMINFO failed: {:?}",
            r.err()
        );
    }

    #[test]
    fn make_decoder_rejects_unsupported_sample_format() {
        let mut p = CodecParameters::audio(CodecId::new("flac"));
        p.sample_rate = Some(44_100);
        p.channels = Some(2);
        p.sample_format = Some(SampleFormat::U8);
        let r = make_decoder(&p);
        assert!(r.is_err());
    }

    #[test]
    fn send_packet_rejects_short_packet() {
        let mut dec = make_decoder(&params_flac_44100_stereo_16bit()).expect("decoder");
        let pkt = Packet::new(0, TimeBase::new(1, 44_100), vec![0xFF, 0xF8]);
        assert!(dec.send_packet(&pkt).is_err());
    }

    #[test]
    fn send_packet_rejects_invalid_sync() {
        let mut dec = make_decoder(&params_flac_44100_stereo_16bit()).expect("decoder");
        // 6 bytes that aren't a FLAC sync code.
        let pkt = Packet::new(0, TimeBase::new(1, 44_100), vec![0x00; 16]);
        assert!(dec.send_packet(&pkt).is_err());
    }

    /// Build a synthetic FLAC frame header that parses but is
    /// otherwise empty payload. The bridge only needs the first ~5
    /// bytes to walk the header invariants; AT itself will discover
    /// the body is malformed and surface an OSStatus, but the bridge
    /// `send_packet` call is expected to accept it.
    fn synth_frame_header_stereo_44100() -> Vec<u8> {
        // Layout per RFC 9639 §9.1:
        //   0xFF 0xF8 — sync + blocking_strategy=0
        //   block_size_code=5 (4608) + sample_rate_code=9 (44.1k)
        //   channel_assignment=8 (left-side, 2-channel) +
        //     bps_code=4 (16) + reserved=0
        //   UTF-8 frame number (single-byte form, frame=0)
        let mut buf: Vec<u8> = vec![0xFF, 0xF8, (5 << 4) | 9, (8 << 4) | (4 << 1), 0x00];
        // Dummy CRC-8 placeholder + a few bytes of "payload".
        buf.extend_from_slice(&[0xAA; 16]);
        // Trailing 16-bit CRC footer placeholder.
        buf.push(0xBB);
        buf.push(0xCC);
        buf
    }

    #[test]
    fn send_packet_rejects_mid_stream_channel_count_switch() {
        let mut p = CodecParameters::audio(CodecId::new("flac"));
        p.sample_rate = Some(44_100);
        p.channels = Some(2);
        p.sample_format = Some(SampleFormat::S16);
        let mut dec = make_decoder(&p).expect("decoder");

        let pkt1 = Packet::new(
            0,
            TimeBase::new(1, 44_100),
            synth_frame_header_stereo_44100(),
        );
        // AT may fail to decode the all-fake payload; bridge layer
        // should accept the first packet without complaint.
        let _ = dec.send_packet(&pkt1);

        // Second packet: same sync but channel_assignment=0
        // (independent, 1 channel). bps_code=4 (16-bit) so the only
        // mid-stream change is the channel count.
        let mut bad = synth_frame_header_stereo_44100();
        bad[3] = 4 << 1;
        let pkt2 = Packet::new(0, TimeBase::new(1, 44_100), bad);
        let r = dec.send_packet(&pkt2);
        assert!(r.is_err(), "must reject mid-stream channel-count switch");
    }

    #[test]
    fn send_packet_rejects_mid_stream_bit_depth_switch() {
        let mut p = CodecParameters::audio(CodecId::new("flac"));
        p.sample_rate = Some(44_100);
        p.channels = Some(2);
        p.sample_format = Some(SampleFormat::S16);
        let mut dec = make_decoder(&p).expect("decoder");

        let pkt1 = Packet::new(
            0,
            TimeBase::new(1, 44_100),
            synth_frame_header_stereo_44100(),
        );
        let _ = dec.send_packet(&pkt1);

        // Switch to bps_code=6 (24-bit).
        let mut bad = synth_frame_header_stereo_44100();
        bad[3] = (8 << 4) | (6 << 1);
        let pkt2 = Packet::new(0, TimeBase::new(1, 44_100), bad);
        let r = dec.send_packet(&pkt2);
        assert!(r.is_err(), "must reject mid-stream bit-depth switch");
    }

    #[test]
    fn channel_layout_label_covers_each_assignment() {
        assert_eq!(
            channel_layout_label(ChannelAssignment::Independent(0)),
            "independent"
        );
        assert_eq!(
            channel_layout_label(ChannelAssignment::LeftSide),
            "left+side"
        );
        assert_eq!(
            channel_layout_label(ChannelAssignment::SideRight),
            "side+right"
        );
        assert_eq!(channel_layout_label(ChannelAssignment::MidSide), "mid+side");
    }
}
