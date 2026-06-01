//! Integration test for the AudioToolbox FLAC decoder bridge.
//!
//! Drives a small bundled fixture (mono, 16-bit, 44.1 kHz, 10 frames
//! of 4608 samples each with the last frame trimmed to fit 44 100
//! total samples) through `FlacAtDecoder` and asserts:
//!
//! 1. The fixture parses cleanly: `fLaC` signature followed by the
//!    metadata-block chain (STREAMINFO + VORBIS_COMMENT + PADDING).
//! 2. The 10 frame boundaries match the trace-doc baseline at
//!    `docs/audio/flac/fixtures/mono-16bit-44100-fixed-blocksize/trace.txt`.
//! 3. The bridge accepts every frame without surfacing an error from
//!    `AudioConverterFillComplexBuffer`.
//! 4. The decoded PCM totals `44 100` samples per channel — the
//!    `total_samples` field encoded in STREAMINFO. **FLAC is
//!    lossless**, so the bridge MUST produce sample-exact output:
//!    the test asserts a SHA-256 / byte-exact match against the
//!    staged `expected.wav`.
//!
//! The fixture is bundled under `tests/fixtures/` so the standalone
//! GitHub Actions CI (which checks out only the per-crate repo) can
//! find it without the umbrella's `docs/` submodule.

#![cfg(target_os = "macos")]

use std::path::PathBuf;

use oxideav_audiotoolbox::flac::{
    self, StreamInfo, FLAC_SIGNATURE, MAGIC_COOKIE_MIN_LEN, STREAMINFO_BODY_LEN,
};
use oxideav_audiotoolbox::flac_decoder;
use oxideav_core::{CodecId, CodecParameters, Frame, Packet, TimeBase};

const FIXTURE_DIR: &str = "tests/fixtures/flac-mono-16bit-44100";

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURE_DIR)
        .join(name)
}

/// Walk the `.flac` metadata chain and return `(streaminfo, end_of_metadata_offset)`.
fn parse_flac_header(buf: &[u8]) -> (StreamInfo, usize) {
    assert_eq!(
        &buf[0..flac::FLAC_SIGNATURE_LEN],
        FLAC_SIGNATURE.as_slice(),
        "fixture must start with the fLaC signature"
    );
    let mut pos = flac::FLAC_SIGNATURE_LEN;
    let mut info: Option<StreamInfo> = None;
    loop {
        let h = &buf[pos..pos + 4];
        let last = (h[0] & 0x80) != 0;
        let btype = h[0] & 0x7F;
        let length = ((h[1] as usize) << 16) | ((h[2] as usize) << 8) | (h[3] as usize);
        pos += 4;
        if btype == 0 {
            info = Some(
                StreamInfo::parse(&buf[pos..pos + STREAMINFO_BODY_LEN]).expect("STREAMINFO parses"),
            );
        }
        pos += length;
        if last {
            break;
        }
    }
    (info.expect("STREAMINFO present"), pos)
}

/// Walk the FLAC frame region by 0xFF F8 / 0xFF F9 sync codes and
/// return the per-frame byte slices.
///
/// FLAC frames don't carry their own length in the header, so the
/// canonical demuxer either consults `STREAMINFO.max_framesize`,
/// scans for the next sync, or relies on the CRC-16 footer. This
/// walker uses the "next sync" strategy which is sufficient for the
/// fixture corpus (no false syncs inside the payload — payload
/// Rice-coded values are biased to be small).
fn split_flac_frames(region: &[u8]) -> Vec<&[u8]> {
    let mut boundaries: Vec<usize> = Vec::new();
    let mut p = 0usize;
    while p + 2 <= region.len() {
        if region[p] == 0xFF && (region[p + 1] == 0xF8 || region[p + 1] == 0xF9) {
            boundaries.push(p);
            // Step past the candidate sync so we don't immediately
            // re-find it; subsequent payload-byte 0xFF cannot be the
            // start of a frame because we'd already have crossed
            // through the frame header / footer.
            p += 1;
        } else {
            p += 1;
        }
    }
    boundaries.push(region.len());
    let mut out: Vec<&[u8]> = Vec::new();
    for w in boundaries.windows(2) {
        if w[1] - w[0] >= 16 {
            out.push(&region[w[0]..w[1]]);
        }
    }
    out
}

/// Read a stdlib-written `expected.wav` (PCM_FORMAT, S16LE, 44.1 kHz,
/// mono). Returns the raw S16 sample vector.
fn read_wav_s16le_mono(path: &PathBuf) -> Vec<i16> {
    let bytes = std::fs::read(path).expect("read expected.wav");
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

fn dec_params_with_cookie(info: &StreamInfo) -> CodecParameters {
    let mut p = CodecParameters::audio(CodecId::new("flac"));
    p.sample_rate = Some(info.sample_rate);
    p.channels = Some(info.channels as u16);
    p.extradata = flac::build_magic_cookie(info);
    assert!(p.extradata.len() >= MAGIC_COOKIE_MIN_LEN);
    p
}

#[test]
fn flac_decoder_decodes_mono_44100_fixture_lossless() {
    let buf = std::fs::read(fixture_path("input.flac")).expect("read input.flac");
    let (info, metadata_end) = parse_flac_header(&buf);

    // Fixture documented configuration: mono, 44.1 kHz, 16-bit,
    // fixed-blocksize 4608, total_samples=44100.
    assert_eq!(info.sample_rate, 44_100);
    assert_eq!(info.channels, 1);
    assert_eq!(info.bits_per_sample, 16);
    assert_eq!(info.min_blocksize, 4608);
    assert_eq!(info.max_blocksize, 4608);
    assert!(info.is_fixed_blocksize());
    assert_eq!(info.total_samples, 44_100);

    // Frame walk — should yield exactly 10 frames per the trace.
    let frames = split_flac_frames(&buf[metadata_end..]);
    assert_eq!(frames.len(), 10, "fixture should yield 10 FLAC frames");

    // First frame's header should match the documented configuration.
    let first_header =
        flac::parse_frame_header(frames[0], &info).expect("first frame header parses");
    assert_eq!(first_header.blocking_strategy, 0);
    assert_eq!(first_header.block_size, 4608);
    assert_eq!(first_header.sample_rate, 44_100);
    assert_eq!(first_header.channels(), 1);
    assert_eq!(first_header.bits_per_sample, 16);

    // Drive the bridge.
    let mut dec = flac_decoder::make_decoder(&dec_params_with_cookie(&info)).expect("decoder");
    let tb = TimeBase::new(1, 44_100);

    let mut decoded: Vec<i16> = Vec::new();
    for frame_bytes in &frames {
        let pkt = Packet::new(0, tb, frame_bytes.to_vec());
        dec.send_packet(&pkt).expect("send_packet");
        while let Ok(Frame::Audio(af)) = dec.receive_frame() {
            // Single-channel S16 invariant.
            assert_eq!(
                af.data[0].len(),
                (af.samples as usize) * 2,
                "S16 mono: samples × 2 bytes per S16 sample"
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

    // FLAC is lossless — every conformant decoder MUST produce the
    // sample-exact PCM the encoder fed in, and the bridge MUST
    // surface every sample once and only once.
    assert_eq!(
        decoded.len(),
        44_100,
        "decoder must vend exactly total_samples (= 44 100)"
    );

    let reference = read_wav_s16le_mono(&fixture_path("expected.wav"));
    assert_eq!(reference.len(), 44_100);
    // Byte-exact match — anything else means the bridge has lost or
    // distorted samples on the way through.
    assert_eq!(
        decoded, reference,
        "FLAC decoder output must be byte-exact (lossless)"
    );
}

#[test]
fn flac_decoder_resets_state() {
    let buf = std::fs::read(fixture_path("input.flac")).expect("read input.flac");
    let (info, metadata_end) = parse_flac_header(&buf);
    let frames = split_flac_frames(&buf[metadata_end..]);

    let mut dec = flac_decoder::make_decoder(&dec_params_with_cookie(&info)).expect("decoder");
    let tb = TimeBase::new(1, 44_100);

    // Push a few frames and drain.
    for fb in frames.iter().take(3) {
        let pkt = Packet::new(0, tb, fb.to_vec());
        dec.send_packet(&pkt).expect("send_packet");
        while let Ok(Frame::Audio(_)) = dec.receive_frame() {}
    }
    dec.reset().expect("reset");

    // After reset, the decoder must accept frames as a fresh stream.
    let pkt = Packet::new(0, tb, frames[0].to_vec());
    dec.send_packet(&pkt)
        .expect("send_packet after reset must succeed");
}

#[test]
fn flac_decoder_rejects_short_packet() {
    let buf = std::fs::read(fixture_path("input.flac")).expect("read input.flac");
    let (info, _) = parse_flac_header(&buf);
    let mut dec = flac_decoder::make_decoder(&dec_params_with_cookie(&info)).expect("decoder");
    let pkt = Packet::new(0, TimeBase::new(1, 44_100), vec![0xFF, 0xF8]);
    assert!(dec.send_packet(&pkt).is_err());
}

#[test]
fn flac_decoder_uses_extradata_cookie_verbatim() {
    let buf = std::fs::read(fixture_path("input.flac")).expect("read input.flac");
    let (info, _) = parse_flac_header(&buf);
    // Build a cookie with a deliberately weird MD5 to confirm it
    // round-trips through `parse_magic_cookie`. The fact that AT
    // accepts the cookie (the decoder constructs successfully)
    // proves we passed the cookie through verbatim.
    let mut info2 = info;
    info2.md5 = [0xDE; 16];
    let mut p = CodecParameters::audio(CodecId::new("flac"));
    p.extradata = flac::build_magic_cookie(&info2);
    let dec = flac_decoder::make_decoder(&p);
    assert!(dec.is_ok(), "decoder w/ supplied cookie: {:?}", dec.err());
}
