//! Integration test for the AudioToolbox MP3 decoder bridge.
//!
//! Drives a small bundled fixture (MPEG-1 Layer III, 44.1 kHz, stereo,
//! 128 kbit/s CBR, 33 frames including the Xing/Info header frame)
//! through `Mp3AtDecoder` and asserts:
//!
//! 1. Every frame walk succeeds — the ISO/IEC 11172-3 §2.4.3.1
//!    `frame_length = 144 * br / sr + padding` formula reproduces the
//!    exact frame boundaries that the trace-doc baseline counted.
//! 2. The bridge accepts every emitted packet without surfacing an
//!    error from `AudioConverterFillComplexBuffer`.
//! 3. PCM totals reach the expected ~32 × 1152 = 36 864 samples per
//!    channel (less the canonical decoder-delay priming silence that
//!    every MPEG-audio decoder vends as its first chunk).
//! 4. SNR against the staged `expected.wav` reference is comfortably
//!    above a "decoder is wired correctly" threshold. We're not
//!    claiming bit-exactness because AT's decoder runs its own
//!    polyphase / dewindowing rounding and the staged reference came
//!    from a different black-box decoder — but the two should be
//!    within a few dB of each other given the input is a clean
//!    sine-and-noise mix.
//!
//! The fixture is bundled under `tests/fixtures/` so the standalone
//! GitHub Actions CI (which checks out only the per-crate repo) can
//! find it without the umbrella's `docs/` submodule.

// `registry` gates the oxideav-core dependency these tests drive;
// without it the crate exposes only the raw bridge, so the whole
// test target compiles away (matching the standalone CI path).
#![cfg(all(target_os = "macos", feature = "registry"))]

use std::path::PathBuf;

use oxideav_audiotoolbox::mp3::FrameHeader;
use oxideav_audiotoolbox::mp3_decoder;
use oxideav_core::{CodecId, CodecParameters, Frame, Packet, TimeBase};

const FIXTURE_DIR: &str = "tests/fixtures/mp3-layer3-stereo-44100-128kbps";

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURE_DIR)
        .join(name)
}

/// Skip an ID3v2 header at offset 0 if present, returning the offset
/// of the first MPEG audio byte. ID3v2 length is a 32-bit syncsafe
/// integer at bytes 6..10 of the 10-byte header.
fn skip_id3v2(buf: &[u8]) -> usize {
    if buf.len() < 10 || &buf[0..3] != b"ID3" {
        return 0;
    }
    let s = &buf[6..10];
    let len = ((s[0] as u32 & 0x7F) << 21)
        | ((s[1] as u32 & 0x7F) << 14)
        | ((s[2] as u32 & 0x7F) << 7)
        | (s[3] as u32 & 0x7F);
    10 + len as usize
}

/// Walk the MPEG-audio elementary stream into a vector of frame
/// byte-slices, each one exactly one frame long.
fn split_frames(stream: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 4 <= stream.len() {
        let hdr = [
            stream[pos],
            stream[pos + 1],
            stream[pos + 2],
            stream[pos + 3],
        ];
        let parsed = match FrameHeader::parse(hdr) {
            Some(h) => h,
            None => {
                // Try to resync — bump one byte and retry. (Real MP3
                // demuxers do this on garbage tail-bytes / APE / Lyrics
                // tags trailing the audio.)
                pos += 1;
                continue;
            }
        };
        if pos + parsed.frame_length > stream.len() {
            break;
        }
        out.push(&stream[pos..pos + parsed.frame_length]);
        pos += parsed.frame_length;
    }
    out
}

/// Parse the simple PCM payload out of a stdlib-written
/// `expected.wav` (PCM_FORMAT, S16LE, 44.1 kHz, stereo). Returns the
/// raw S16 sample vector (interleaved L, R, L, R …).
fn read_wav_s16le(path: &PathBuf) -> Vec<i16> {
    let bytes = std::fs::read(path).expect("read expected.wav");
    // Find the `data` chunk header. WAV's RIFF chunks are
    // [id (4) | size (4 LE) | payload (size bytes)]; the top-level
    // RIFF wrapper is followed by 4 bytes of form (`WAVE`) then a
    // sequence of sub-chunks. Just scan for `data`.
    let mut pos = 12;
    let payload_start;
    let payload_len;
    loop {
        assert!(pos + 8 <= bytes.len(), "no data chunk in WAV");
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        if id == b"data" {
            payload_start = pos + 8;
            payload_len = size;
            break;
        }
        pos += 8 + size;
    }
    let sample_bytes = &bytes[payload_start..payload_start + payload_len];
    let mut out = Vec::with_capacity(sample_bytes.len() / 2);
    for chunk in sample_bytes.chunks_exact(2) {
        out.push(i16::from_le_bytes([chunk[0], chunk[1]]));
    }
    out
}

fn dec_params() -> CodecParameters {
    let mut p = CodecParameters::audio(CodecId::new("mp3"));
    p.sample_rate = Some(44_100);
    p.channels = Some(2);
    p
}

#[test]
fn mp3_decoder_decodes_staged_fixture() {
    let mp3 = std::fs::read(fixture_path("input.mp3")).expect("read input.mp3");
    let id3_skip = skip_id3v2(&mp3);
    let frames = split_frames(&mp3[id3_skip..]);
    // The trace baseline at docs/audio/mp3/fixtures/layer3-stereo-
    // 44100-128kbps/trace.txt records 33 HEADER lines (the leading
    // Xing/Info header frame plus 32 audio frames).
    assert_eq!(
        frames.len(),
        33,
        "fixture should yield 33 MPEG audio frames"
    );

    // First frame's header should match the documented configuration.
    let first_header = FrameHeader::parse([frames[0][0], frames[0][1], frames[0][2], frames[0][3]])
        .expect("first header parses");
    assert_eq!(first_header.bit_rate, 128_000);
    assert_eq!(first_header.sample_rate, 44_100);
    assert_eq!(first_header.channels(), 2);
    assert_eq!(first_header.samples_per_frame, 1152);

    let mut dec = mp3_decoder::make_decoder(&dec_params()).expect("make_decoder");
    let tb = TimeBase::new(1, 44_100);

    let mut decoded: Vec<i16> = Vec::new();
    for frame_bytes in &frames {
        let pkt = Packet::new(0, tb, frame_bytes.to_vec());
        dec.send_packet(&pkt).expect("send_packet");
        while let Ok(Frame::Audio(af)) = dec.receive_frame() {
            assert_eq!(
                af.data[0].len(),
                (af.samples as usize) * 2 * 2,
                "S16 interleaved stereo: samples × 2 chans × 2 bytes"
            );
            for chunk in af.data[0].chunks_exact(2) {
                decoded.push(i16::from_le_bytes([chunk[0], chunk[1]]));
            }
        }
    }
    dec.flush().expect("flush");
    while let Ok(Frame::Audio(af)) = dec.receive_frame() {
        for chunk in af.data[0].chunks_exact(2) {
            decoded.push(i16::from_le_bytes([chunk[0], chunk[1]]));
        }
    }

    // Each audio frame decodes to 1152 stereo samples × 2 channels =
    // 2304 interleaved i16 values; 32 audio frames + the leading Xing
    // header frame (which AT decodes to silence) should produce close
    // to 33 × 2304 = 76 032 i16 values. Decoder-delay priming may
    // shift exact totals by a small fixed amount, so we assert a
    // comfortable range rather than an exact match.
    eprintln!("MP3 decoder produced {} i16 samples", decoded.len());
    assert!(
        decoded.len() >= 60_000,
        "decoder produced too little PCM ({})",
        decoded.len()
    );
    assert!(
        decoded.len() <= 80_000,
        "decoder produced too much PCM ({})",
        decoded.len()
    );

    // Compare against the staged reference WAV via SNR. The two
    // decoders (AT vs whatever produced expected.wav) are independent
    // implementations of the same format, so we compare after aligning
    // on the longer-common-prefix sample count and assert "PCM looks
    // like the reference" rather than bit-exactness.
    let reference = read_wav_s16le(&fixture_path("expected.wav"));
    let n = decoded.len().min(reference.len());
    assert!(n >= 60_000, "not enough common samples to compare ({n})");

    // Find best-match offset within a priming-delay window. Every
    // MPEG-audio decoder emits a priming-silence block as its first
    // output (529 PCM samples per channel canonically per ISO 11172-3
    // Annex C, but real decoders vary — AT's exact number is not
    // public). Two decoders comparing the same file end up with PCM
    // streams shifted by the *difference* of their priming counts.
    // ±4608 stereo-sample-positions (= ±2 MPEG-1 LIII frames per
    // channel) is comfortably wide.
    let mut best_offset = 0i32;
    let mut best_snr_db = f64::NEG_INFINITY;
    for offset in (-9216i32..=9216).step_by(2) {
        let (a_start, b_start) = if offset >= 0 {
            (offset as usize, 0usize)
        } else {
            (0usize, (-offset) as usize)
        };
        if a_start >= decoded.len() || b_start >= reference.len() {
            continue;
        }
        let avail = (decoded.len() - a_start).min(reference.len() - b_start);
        if avail < 20_000 {
            continue;
        }
        let mut sig_energy = 0f64;
        let mut noise_energy = 0f64;
        for i in 0..avail {
            let s = reference[b_start + i] as f64;
            let d = decoded[a_start + i] as f64;
            sig_energy += s * s;
            let e = s - d;
            noise_energy += e * e;
        }
        if noise_energy <= 0.0 || sig_energy <= 0.0 {
            continue;
        }
        let snr_db = 10.0 * (sig_energy / noise_energy).log10();
        if snr_db > best_snr_db {
            best_snr_db = snr_db;
            best_offset = offset;
        }
    }
    eprintln!("MP3 decoder SNR vs staged reference: {best_snr_db:.1} dB @ offset {best_offset}");
    // Two independent MP3 decoders on identical input should land
    // within a few dB of each other across the priming-aligned window.
    // We assert > 30 dB — well above "wired correctly" (which would be
    // > 15 dB) and well below transparency.
    assert!(
        best_snr_db > 30.0,
        "MP3 decoder SNR vs reference {best_snr_db:.1} dB < 30 dB threshold"
    );
}

#[test]
fn mp3_decoder_rejects_short_packet() {
    let mut dec = mp3_decoder::make_decoder(&dec_params()).expect("make_decoder");
    let pkt = Packet::new(0, TimeBase::new(1, 44_100), vec![0xFF, 0xFB, 0x90]); // 3 bytes
    let r = dec.send_packet(&pkt);
    assert!(r.is_err(), "must reject packet too short for a header");
}

#[test]
fn mp3_decoder_reset_clears_state() {
    let mp3 = std::fs::read(fixture_path("input.mp3")).expect("read input.mp3");
    let id3_skip = skip_id3v2(&mp3);
    let frames = split_frames(&mp3[id3_skip..]);
    let mut dec = mp3_decoder::make_decoder(&dec_params()).expect("make_decoder");
    let tb = TimeBase::new(1, 44_100);

    // Push a few frames so internal state has something to clear.
    for frame_bytes in frames.iter().take(4) {
        let pkt = Packet::new(0, tb, frame_bytes.to_vec());
        dec.send_packet(&pkt).expect("send_packet");
    }
    dec.reset().expect("reset");

    // After reset, the decoder should accept new packets without
    // returning Eof from a previous flush.
    let pkt = Packet::new(0, tb, frames[0].to_vec());
    dec.send_packet(&pkt)
        .expect("send_packet after reset must succeed");
}
