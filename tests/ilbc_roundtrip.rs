//! Round-trip test for the AudioToolbox iLBC bridge.
//!
//! Generates 2 seconds of 8 kHz mono PCM containing a 1 kHz sine,
//! encodes it through `IlbcAtEncoder`, then decodes the resulting raw
//! iLBC packets through `IlbcAtDecoder`. Asserts:
//!
//! * Encoder-emitted packet sizes match the mode's spec (38 B / 50 B).
//! * Decoder accepts the encoder's packet stream without error.
//! * Per-channel SNR is ≥ 6 dB (iLBC is a low-bitrate speech codec;
//!   sine reconstruction is not transparent and SNR floors here
//!   measure that the analysis / synthesis chain wired up — not that
//!   we approached transparency, which iLBC doesn't claim).
//!
//! iLBC is a CELP-class speech coder targeting voice, so a 1 kHz sine
//! is intentionally adversarial: the codec spends bits on perceptually-
//! tuned voice features and the residual on a pure sine is high. The
//! SNR floor is set deliberately low — the goal is to prove the
//! pipeline is wired, not to grade the codec.

#![cfg(target_os = "macos")]

use std::f32::consts::PI;

use oxideav_audiotoolbox::{ilbc_decoder, ilbc_encoder};
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Frame, Packet, SampleFormat, TimeBase};

const SR: u32 = 8_000;
const CH: u16 = 1;
const FREQ: f32 = 1_000.0;
const N_FRAMES: usize = (SR as usize) * 2;

/// Build a deterministic interleaved S16 PCM 1 kHz sine — full-scale
/// amplitude is `0.5` to leave the codec room without clipping.
fn gen_sine_s16(n_frames: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n_frames * 2);
    for i in 0..n_frames {
        let t = i as f32 / SR as f32;
        let s = ((2.0 * PI * FREQ * t).sin() * 16_000.0) as i16;
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

/// Sliding-window SNR: search a small alignment window for the offset
/// that yields the largest signal-to-noise ratio against the reference.
fn best_snr_db(reference: &[i16], decoded: &[i16], search_max: usize) -> f64 {
    let probe_len = 16_000.min(reference.len()).min(decoded.len() / 2);
    let mut best_db = f64::NEG_INFINITY;
    let max_off = search_max.min(decoded.len().saturating_sub(probe_len));
    for off in 0..=max_off {
        let mut signal = 0.0f64;
        let mut noise = 0.0f64;
        for i in 0..probe_len {
            let r = reference[i] as f64;
            let d = decoded[off + i] as f64;
            signal += r * r;
            noise += (r - d) * (r - d);
        }
        if noise == 0.0 {
            return f64::INFINITY;
        }
        let snr = 10.0 * (signal / noise).log10();
        if snr > best_db {
            best_db = snr;
        }
    }
    best_db
}

fn run_roundtrip(mode_tag: &str, expected_pkt_bytes: usize, expected_pkt_samples: usize) {
    // ── Reference PCM ──────────────────────────────────────────────────
    let ref_bytes = gen_sine_s16(N_FRAMES);
    let ref_samples = bytes_to_s16_vec(&ref_bytes);

    // ── Encoder ────────────────────────────────────────────────────────
    let mut enc_params = CodecParameters::audio(CodecId::new("ilbc"));
    enc_params.sample_rate = Some(SR);
    enc_params.channels = Some(CH);
    enc_params.sample_format = Some(SampleFormat::S16);
    enc_params.options.insert("mode", mode_tag);

    let mut enc = ilbc_encoder::make_encoder(&enc_params).expect("make_encoder");

    let tb = TimeBase::new(1, SR as i64);
    let mut ilbc_packets: Vec<Vec<u8>> = Vec::new();

    // Feed in 320-sample chunks (40 ms) — deliberately misaligned with
    // both 20 ms (160 frames) and 30 ms (240 frames) packets so the
    // encoder's staging buffer is exercised.
    let chunk_frames = 320usize;
    let chunk_bytes = chunk_frames * 2;
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
                Ok(pkt) => ilbc_packets.push(pkt.data),
                Err(oxideav_core::Error::NeedMore) => break,
                Err(e) => panic!("receive_packet: {e}"),
            }
        }
    }
    enc.flush().expect("flush");
    loop {
        match enc.receive_packet() {
            Ok(pkt) => ilbc_packets.push(pkt.data),
            Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("flush receive_packet: {e}"),
        }
    }

    assert!(!ilbc_packets.is_empty(), "no iLBC packets produced");
    eprintln!(
        "iLBC {} ms: encoder produced {} packets",
        mode_tag,
        ilbc_packets.len()
    );

    // Verify the fixed packet size — this is the canonical iLBC
    // invariant per the mode geometry.
    for (i, pkt) in ilbc_packets.iter().enumerate() {
        assert_eq!(
            pkt.len(),
            expected_pkt_bytes,
            "packet {i} has size {} (expected {})",
            pkt.len(),
            expected_pkt_bytes
        );
    }

    // Expected number of packets: ceil(2 s × 8000 Hz / frames_per_packet).
    // 30 ms: 16000 / 240 = 66.6 → 67. 20 ms: 16000 / 160 = 100.
    let min_expected = N_FRAMES / expected_pkt_samples;
    assert!(
        ilbc_packets.len() >= min_expected,
        "expected ≥ {min_expected} packets, got {}",
        ilbc_packets.len()
    );

    // ── Decoder ────────────────────────────────────────────────────────
    let mut dec_params = CodecParameters::audio(CodecId::new("ilbc"));
    dec_params.sample_rate = Some(SR);
    dec_params.channels = Some(CH);
    dec_params.options.insert("mode", mode_tag);

    let mut dec = ilbc_decoder::make_decoder(&dec_params).expect("make_decoder");
    let mut decoded_bytes: Vec<u8> = Vec::new();

    for adata in &ilbc_packets {
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
    dec.flush().expect("flush");
    // Drain the trailing PCM exposed by flush.
    loop {
        match dec.receive_frame() {
            Ok(Frame::Audio(af)) => {
                decoded_bytes.extend_from_slice(&af.data[0]);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    assert!(
        !decoded_bytes.is_empty(),
        "decoder produced no PCM for {mode_tag} ms mode"
    );
    let decoded = bytes_to_s16_vec(&decoded_bytes);
    eprintln!(
        "iLBC {} ms: ref={} samples, dec={} samples",
        mode_tag,
        ref_samples.len(),
        decoded.len()
    );

    // ── SNR ────────────────────────────────────────────────────────────
    //
    // iLBC encoders have ~1-2 packets of look-ahead (the LPC analysis
    // frame extends slightly beyond the synthesised block). Search up
    // to 3 packets of priming offset.
    let search_max = expected_pkt_samples * 3;
    let snr = best_snr_db(&ref_samples, &decoded, search_max);
    eprintln!("iLBC {} ms: peak SNR = {:.2} dB", mode_tag, snr);

    // 6 dB is well below transparency but proves the codec is not just
    // emitting noise. iLBC on a 1 kHz sine reconstructs with limited
    // fidelity because the codebook is voice-tuned; the floor reflects
    // that the pipeline is wired, not that we hit a quality target.
    assert!(
        snr >= 6.0,
        "iLBC {} ms SNR {:.2} dB below 6 dB floor (likely a wiring bug)",
        mode_tag,
        snr
    );
}

#[test]
fn ilbc_30ms_roundtrip() {
    run_roundtrip("30", 50, 240);
}

#[test]
fn ilbc_20ms_roundtrip() {
    run_roundtrip("20", 38, 160);
}

/// Verify each emitted packet is non-zero (a silent encoder would
/// satisfy the size invariant but not actually carry signal).
#[test]
fn ilbc_packets_have_nonzero_payload() {
    let ref_bytes = gen_sine_s16(N_FRAMES);

    let mut enc_params = CodecParameters::audio(CodecId::new("ilbc"));
    enc_params.sample_rate = Some(SR);
    enc_params.channels = Some(CH);
    enc_params.sample_format = Some(SampleFormat::S16);
    enc_params.options.insert("mode", "30");

    let mut enc = ilbc_encoder::make_encoder(&enc_params).expect("make_encoder");
    let frame = Frame::Audio(AudioFrame {
        samples: N_FRAMES as u32,
        pts: Some(0),
        data: vec![ref_bytes],
    });
    enc.send_frame(&frame).expect("send_frame");
    enc.flush().expect("flush");

    let mut nonzero = 0;
    loop {
        match enc.receive_packet() {
            Ok(pkt) => {
                if pkt.data.iter().any(|&b| b != 0) {
                    nonzero += 1;
                }
            }
            Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("receive_packet: {e}"),
        }
    }
    assert!(
        nonzero >= 4,
        "expected ≥ 4 nonzero iLBC packets, got {nonzero}"
    );
}
