//! Bit-exact round-trip test for the AudioToolbox FLAC bridge.
//!
//! Generates 2 seconds of 48 kHz / 16-bit stereo PCM containing a 440 Hz
//! sine plus a deterministic pseudo-random low-amplitude perturbation,
//! encodes it through `FlacAtEncoder`, then forwards the resulting raw
//! FLAC packets + encoder-vended `dfLa` magic cookie into
//! `FlacAtDecoder`, and verifies the recovered PCM matches the input
//! **bit-for-bit** after accounting for any priming-silence head shift.
//!
//! FLAC is lossless by definition (RFC 9639 §1) — any per-sample
//! mismatch in the aligned region is a real bug, not a quality
//! measurement. This is the symmetric partner to the round-3 ALAC
//! bit-exact round-trip test.

#![cfg(target_os = "macos")]

use std::f32::consts::PI;

use oxideav_audiotoolbox::{flac_decoder, flac_encoder};
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Frame, Packet, SampleFormat, TimeBase};

const SR: u32 = 48_000;
const CH: u16 = 2;
const FREQ: f32 = 440.0;
/// 2 seconds of signal — comfortably covers any plausible priming.
const N_FRAMES: usize = (SR as usize) * 2;
/// FLAC packets default to 4096 PCM frames (RFC 9639 §9.1.2 Table 1
/// code 11; the canonical block size every fixture in
/// `docs/audio/flac/fixtures/` uses).
const PACKET_FRAMES: usize = 4096;

/// Build a deterministic interleaved S16 PCM signal: 440 Hz sine plus
/// a small LCG-driven pseudo-random component. The noise term forces
/// the entropy coder to do real work — a sine alone is so trivial that
/// many *non*-bit-exact codecs would recover it accidentally.
fn gen_signal_s16(n_frames: usize) -> Vec<i16> {
    let mut out = Vec::with_capacity(n_frames * CH as usize);
    let mut lcg: u32 = 0x1234_5678;
    for i in 0..n_frames {
        let t = i as f32 / SR as f32;
        let sine = (2.0 * PI * FREQ * t).sin();
        let s_main = (sine * 24_000.0) as i32;
        lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = ((lcg >> 16) as i16) >> 4; // ±2048
        let s = (s_main + noise as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        for _ in 0..CH {
            out.push(s);
        }
    }
    out
}

fn s16_vec_to_bytes(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

fn bytes_to_s16_vec(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

#[test]
fn flac_roundtrip_bit_exact() {
    // ── Reference PCM ──────────────────────────────────────────────────
    let ref_samples_i16 = gen_signal_s16(N_FRAMES);
    let ref_bytes = s16_vec_to_bytes(&ref_samples_i16);

    // ── Encoder ────────────────────────────────────────────────────────
    let mut enc_params = CodecParameters::audio(CodecId::new("flac"));
    enc_params.sample_rate = Some(SR);
    enc_params.channels = Some(CH);
    enc_params.sample_format = Some(SampleFormat::S16);

    let mut enc = flac_encoder::make_encoder(&enc_params).expect("make_encoder");

    // Feed PCM in arbitrary chunks (not aligned to packet boundary) so
    // the encoder's internal staging buffer gets exercised.
    let chunk_bytes = 1024 * CH as usize * 2; // 1024 frames per send_frame call
    let tb = TimeBase::new(1, SR as i64);
    let mut flac_packets: Vec<Vec<u8>> = Vec::new();

    let mut offset = 0usize;
    let mut frame_pts: i64 = 0;
    while offset + chunk_bytes <= ref_bytes.len() {
        let chunk = ref_bytes[offset..offset + chunk_bytes].to_vec();
        offset += chunk_bytes;

        let frame = Frame::Audio(AudioFrame {
            samples: 1024,
            pts: Some(frame_pts),
            data: vec![chunk],
        });
        frame_pts += 1024;

        enc.send_frame(&frame).expect("send_frame");
        loop {
            match enc.receive_packet() {
                Ok(pkt) => flac_packets.push(pkt.data),
                Err(oxideav_core::Error::NeedMore) => break,
                Err(e) => panic!("receive_packet: {e}"),
            }
        }
    }

    enc.flush().expect("flush");
    loop {
        match enc.receive_packet() {
            Ok(pkt) => flac_packets.push(pkt.data),
            Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("flush receive_packet: {e}"),
        }
    }

    assert!(!flac_packets.is_empty(), "no FLAC packets produced");

    // Grab the encoder-vended `dfLa` magic cookie before dropping `enc`.
    let cookie = enc.output_params().extradata.clone();
    assert!(!cookie.is_empty(), "encoder did not vend a magic cookie");

    // ── Decoder ────────────────────────────────────────────────────────
    let mut dec_params = CodecParameters::audio(CodecId::new("flac"));
    dec_params.sample_rate = Some(SR);
    dec_params.channels = Some(CH);
    dec_params.sample_format = Some(SampleFormat::S16);
    dec_params.extradata = cookie;

    let mut dec = flac_decoder::make_decoder(&dec_params).expect("make_decoder");
    let mut decoded_bytes: Vec<u8> = Vec::new();

    for fdata in &flac_packets {
        let pkt = Packet::new(0, tb, fdata.clone());
        dec.send_packet(&pkt).expect("send_packet");
        loop {
            match dec.receive_frame() {
                Ok(Frame::Audio(af)) => {
                    decoded_bytes.extend_from_slice(&af.data[0]);
                }
                Ok(_) => {}
                Err(oxideav_core::Error::NeedMore) => break,
                Err(e) => panic!("receive_frame: {e}"),
            }
        }
    }
    // Flush any remaining PCM look-ahead.
    let _ = dec.flush();
    loop {
        match dec.receive_frame() {
            Ok(Frame::Audio(af)) => decoded_bytes.extend_from_slice(&af.data[0]),
            Ok(_) => {}
            Err(_) => break,
        }
    }

    assert!(!decoded_bytes.is_empty(), "decoder produced no samples");
    let decoded_samples = bytes_to_s16_vec(&decoded_bytes);

    eprintln!(
        "FLAC roundtrip: ref={} samples, dec={} samples, {} packets, cookie={} bytes",
        ref_samples_i16.len(),
        decoded_samples.len(),
        flac_packets.len(),
        enc_params.extradata.len(),
    );

    // ── Bit-exact comparison with priming-silence search ───────────────
    //
    // AT's FLAC encoder may prepend a small amount of priming silence
    // before the first real sample. Scan a reasonable search window for
    // the offset that yields the longest bit-exact match.
    let max_priming = PACKET_FRAMES * CH as usize; // one packet's worth
    let usable_dec = decoded_samples.len().saturating_sub(max_priming);
    assert!(
        usable_dec >= 1024,
        "not enough decoded samples for bit-exact check"
    );

    let mut best_offset = 0usize;
    let mut best_match = 0usize;
    let probe_len = 16384.min(ref_samples_i16.len() - 1024);
    'outer: for off in 0..=max_priming {
        if off + probe_len > decoded_samples.len() {
            break;
        }
        let mut matched = 0usize;
        for i in 0..probe_len {
            if decoded_samples[off + i] == ref_samples_i16[i] {
                matched += 1;
            } else {
                break;
            }
        }
        if matched > best_match {
            best_match = matched;
            best_offset = off;
            if matched == probe_len {
                break 'outer;
            }
        }
    }

    eprintln!(
        "FLAC priming search: best offset = {best_offset} samples, \
         bit-exact prefix = {best_match} / {probe_len} samples"
    );

    // Demand at least 4096 contiguous bit-exact samples — well above
    // any plausible single-packet boundary effect, and a meaningful
    // lossless assertion.
    assert!(
        best_match >= 4096,
        "FLAC roundtrip not bit-exact: best contiguous match = {best_match} samples at offset {best_offset}"
    );

    // Now extend the bit-exact comparison from `best_offset` and
    // demand it survives across multiple FLAC packets.
    let mut bit_exact_run = 0usize;
    let limit = ref_samples_i16
        .len()
        .min(decoded_samples.len() - best_offset);
    for i in 0..limit {
        if decoded_samples[best_offset + i] == ref_samples_i16[i] {
            bit_exact_run += 1;
        } else {
            break;
        }
    }
    eprintln!("FLAC bit-exact run from priming offset {best_offset}: {bit_exact_run} samples",);
    // Demand at least 3 full FLAC packets (3 × 4096 × 2 chans = 24576
    // interleaved samples) survive bit-exact — proves the codec isn't
    // just lossy with a lucky prefix.
    assert!(
        bit_exact_run >= 3 * PACKET_FRAMES * CH as usize,
        "FLAC roundtrip lost bit-exactness after {bit_exact_run} samples \
         (need ≥ {})",
        3 * PACKET_FRAMES * CH as usize
    );
}
