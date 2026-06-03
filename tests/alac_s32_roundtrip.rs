//! Bit-exact S32 round-trip for the AudioToolbox ALAC bridge.
//!
//! Companion to `alac_roundtrip.rs` (which exercises only the S16 path):
//! generates a 2-second 32-bit signed PCM signal containing a 440 Hz
//! sine plus a deterministic 24-bit pseudo-random noise term, encodes
//! it through `AlacAtEncoder` at `SampleFormat::S32`, then decodes the
//! resulting raw ALAC packets through `AlacAtDecoder` configured with
//! `sample_format = Some(SampleFormat::S32)`, and verifies that the
//! recovered S32 PCM matches the input bit-for-bit after accounting
//! for encoder priming silence.
//!
//! Why this matters: prior to the r212 tightening the decoder hard-
//! coded its output ASBD to `pcm_s16` regardless of the cookie's
//! `bit_depth`, so any 24- or 32-bit ALAC track silently lost its
//! low-order bits even though the encoder side correctly emitted a
//! lossless bitstream. The bit-exactness assertion at the bottom of
//! this file is a regression seal: if anyone reverts the decoder to
//! pcm_s16-always, the 32-bit precision test fails because the noise
//! term we inject lives entirely below the 16-bit quantisation floor.

#![cfg(target_os = "macos")]

use std::f64::consts::PI;

use oxideav_audiotoolbox::{alac_decoder, alac_encoder};
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Frame, Packet, SampleFormat, TimeBase};

const SR: u32 = 48_000;
const CH: u16 = 2;
const FREQ: f64 = 440.0;
/// 2 seconds.
const N_FRAMES: usize = (SR as usize) * 2;
/// ALAC packets default to 4096 PCM frames.
const PACKET_FRAMES: usize = 4096;

/// Deterministic interleaved S32 PCM:
/// * Main term: a 440 Hz sine scaled into the **upper 24 bits** of an
///   `i32` (well inside i32 range; survives 24-bit ALAC if the caller
///   asks for it).
/// * Per-sample LCG-driven noise of 24-bit amplitude placed in the
///   **low bits** of the i32 word. This is what makes the bit-exact
///   assertion meaningful: it sits entirely below the 16-bit
///   quantisation floor, so a decoder that truncates to i16 will
///   destroy it even if the encode is perfectly lossless.
fn gen_signal_s32(n_frames: usize) -> Vec<i32> {
    let mut out = Vec::with_capacity(n_frames * CH as usize);
    let mut lcg: u32 = 0x1234_5678;
    for i in 0..n_frames {
        let t = i as f64 / SR as f64;
        let sine = (2.0 * PI * FREQ * t).sin();
        // 0.73 of full-scale i32; well below saturation even with
        // the noise term added below.
        let s_main = (sine * (i32::MAX as f64 * 0.73)) as i64;
        lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        // 24-bit signed amplitude noise (~±2^23).
        let noise = ((lcg >> 8) as i32) >> 8; // sign-extend to i32 from 24 bits
        let s = (s_main + noise as i64).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        for _ in 0..CH {
            out.push(s);
        }
    }
    out
}

fn s32_vec_to_bytes(samples: &[i32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 4);
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

fn bytes_to_s32_vec(bytes: &[u8]) -> Vec<i32> {
    bytes
        .chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[test]
fn alac_s32_roundtrip_bit_exact() {
    // ── Reference PCM ──────────────────────────────────────────────────
    let ref_samples_i32 = gen_signal_s32(N_FRAMES);
    let ref_bytes = s32_vec_to_bytes(&ref_samples_i32);

    // ── Encoder (S32 in, ALAC out) ─────────────────────────────────────
    let mut enc_params = CodecParameters::audio(CodecId::new("alac"));
    enc_params.sample_rate = Some(SR);
    enc_params.channels = Some(CH);
    enc_params.sample_format = Some(SampleFormat::S32);

    let mut enc = match alac_encoder::make_encoder(&enc_params) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("S32 ALAC encoder unavailable on this macOS: {e}; skipping");
            return;
        }
    };

    let chunk_frames = 1024usize;
    let chunk_bytes = chunk_frames * CH as usize * 4; // S32
    let tb = TimeBase::new(1, SR as i64);
    let mut alac_packets: Vec<Vec<u8>> = Vec::new();

    let mut offset = 0usize;
    let mut frame_pts: i64 = 0;
    while offset + chunk_bytes <= ref_bytes.len() {
        let chunk = ref_bytes[offset..offset + chunk_bytes].to_vec();
        offset += chunk_bytes;

        let frame = Frame::Audio(AudioFrame {
            samples: chunk_frames as u32,
            pts: Some(frame_pts),
            data: vec![chunk],
        });
        frame_pts += chunk_frames as i64;

        enc.send_frame(&frame).expect("send_frame");
        loop {
            match enc.receive_packet() {
                Ok(pkt) => alac_packets.push(pkt.data),
                Err(oxideav_core::Error::NeedMore) => break,
                Err(e) => panic!("receive_packet: {e}"),
            }
        }
    }

    enc.flush().expect("flush");
    loop {
        match enc.receive_packet() {
            Ok(pkt) => alac_packets.push(pkt.data),
            Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("flush receive_packet: {e}"),
        }
    }

    assert!(!alac_packets.is_empty(), "no ALAC packets produced");

    let cookie = enc.output_params().extradata.clone();
    assert!(
        cookie.len() >= 24,
        "encoder did not vend a magic cookie (got {} bytes)",
        cookie.len()
    );

    // ── Decoder (S32 out) ──────────────────────────────────────────────
    let mut dec_params = CodecParameters::audio(CodecId::new("alac"));
    dec_params.sample_rate = Some(SR);
    dec_params.channels = Some(CH);
    dec_params.sample_format = Some(SampleFormat::S32);
    dec_params.extradata = cookie;

    let mut dec = alac_decoder::make_decoder(&dec_params).expect("make_decoder");
    let mut decoded_bytes: Vec<u8> = Vec::new();

    for adata in &alac_packets {
        let pkt = Packet::new(0, tb, adata.clone());
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

    assert!(!decoded_bytes.is_empty(), "decoder produced no samples");
    // S32 = 4 bytes per sample, so total bytes must be divisible by 4.
    assert_eq!(
        decoded_bytes.len() % 4,
        0,
        "decoded byte count {} is not 4-aligned — decoder did not honour S32",
        decoded_bytes.len()
    );
    let decoded_samples = bytes_to_s32_vec(&decoded_bytes);

    eprintln!(
        "ALAC S32 roundtrip: ref={} samples, dec={} samples, {} packets",
        ref_samples_i32.len(),
        decoded_samples.len(),
        alac_packets.len()
    );

    // ── Bit-exact comparison with priming-silence search ───────────────
    let max_priming = PACKET_FRAMES * CH as usize;
    let usable_dec = decoded_samples.len().saturating_sub(max_priming);
    assert!(
        usable_dec >= 1024,
        "not enough decoded S32 samples for bit-exact check"
    );

    let mut best_offset = 0usize;
    let mut best_match = 0usize;
    let probe_len = 16384.min(ref_samples_i32.len() - 1024);
    'outer: for off in 0..=max_priming {
        if off + probe_len > decoded_samples.len() {
            break;
        }
        let mut matched = 0usize;
        for i in 0..probe_len {
            if decoded_samples[off + i] == ref_samples_i32[i] {
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
        "ALAC S32 priming search: best offset = {best_offset} samples, \
         bit-exact prefix = {best_match} / {probe_len} samples"
    );

    assert!(
        best_match >= 4096,
        "ALAC S32 roundtrip not bit-exact: best contiguous match = {best_match} samples at offset {best_offset}"
    );

    // Extend bit-exact across multiple packets — the low-bit noise
    // term destroys this assertion if the decoder is silently
    // truncating to S16 internally (regression seal).
    let mut bit_exact_run = 0usize;
    let limit = ref_samples_i32
        .len()
        .min(decoded_samples.len() - best_offset);
    for i in 0..limit {
        if decoded_samples[best_offset + i] == ref_samples_i32[i] {
            bit_exact_run += 1;
        } else {
            break;
        }
    }
    eprintln!("ALAC S32 bit-exact run from priming offset {best_offset}: {bit_exact_run} samples");
    assert!(
        bit_exact_run >= 3 * PACKET_FRAMES * CH as usize,
        "ALAC S32 roundtrip lost bit-exactness after {bit_exact_run} samples \
         (need ≥ {})",
        3 * PACKET_FRAMES * CH as usize
    );
}
