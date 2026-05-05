//! Round-trip encode → decode test for the AudioToolbox AAC bridge.
//!
//! Generates a 1-second 48 kHz stereo 440 Hz sine wave, encodes it at 128
//! kbit/s via `AacAtEncoder`, then decodes the ADTS output via `AacAtDecoder`,
//! and measures per-channel SNR after accounting for the codec's group delay.
//! Pass criterion: ≥ 25 dB on at least one 2048-sample window per channel.

#![cfg(target_os = "macos")]

use std::f32::consts::PI;

use oxideav_audiotoolbox::{decoder, encoder};
use oxideav_core::{
    AudioFrame, CodecId, CodecParameters, Frame, Packet, SampleFormat, TimeBase,
};

fn gen_sine_f32(sample_rate: u32, channels: u16, freq_hz: f32, n_samples: usize) -> Vec<f32> {
    let mut buf = Vec::with_capacity(n_samples * channels as usize);
    for i in 0..n_samples {
        let v = (2.0 * PI * freq_hz * i as f32 / sample_rate as f32).sin() * 0.5;
        for _ in 0..channels {
            buf.push(v);
        }
    }
    buf
}

/// Compute SNR between reference and decoded, both de-interleaved for one
/// channel. The function slides a 2048-sample window over the decoded signal
/// looking for the best alignment against the reference (compensating for
/// codec group delay up to `max_delay_samples`), and returns the best SNR
/// found in dB.
fn best_snr_db(
    ref_ch: &[f32],
    dec_ch: &[f32],
    max_delay_samples: usize,
    window: usize,
) -> f64 {
    let mut best: f64 = f64::NEG_INFINITY;
    let max_d = max_delay_samples.min(dec_ch.len().saturating_sub(window));
    for d in 0..=max_d {
        let r = &ref_ch[..window.min(ref_ch.len())];
        let q = &dec_ch[d..];
        let n = r.len().min(q.len()).min(window);
        if n < 64 {
            continue;
        }
        let sig: f64 = r[..n].iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / n as f64;
        let noise: f64 = r[..n]
            .iter()
            .zip(q[..n].iter())
            .map(|(&a, &b)| (a as f64 - b as f64).powi(2))
            .sum::<f64>()
            / n as f64;
        if noise == 0.0 {
            return f64::INFINITY;
        }
        let snr = 10.0 * (sig / noise).log10();
        if snr > best {
            best = snr;
        }
    }
    best
}

#[test]
fn aac_roundtrip_snr_ge_25db() {
    const SR: u32 = 48_000;
    const CH: u16 = 2;
    const BITRATE: u64 = 128_000;
    const FREQ: f32 = 440.0;
    // Encode 2 seconds so there's plenty of usable signal even after codec delay
    const N_SAMPLES: usize = SR as usize * 2;
    const AAC_FRAME: usize = 1024;

    // ── Reference PCM ──────────────────────────────────────────────────
    let ref_samples = gen_sine_f32(SR, CH, FREQ, N_SAMPLES);
    let ref_bytes: Vec<u8> = ref_samples
        .iter()
        .flat_map(|s| s.to_le_bytes())
        .collect();

    // ── Encoder ────────────────────────────────────────────────────────
    let mut enc_params = CodecParameters::audio(CodecId::new("aac"));
    enc_params.sample_rate = Some(SR);
    enc_params.channels = Some(CH);
    enc_params.sample_format = Some(SampleFormat::F32);
    enc_params.bit_rate = Some(BITRATE);

    let mut enc = encoder::make_encoder(&enc_params).expect("make_encoder");

    let bytes_per_frame = AAC_FRAME * CH as usize * 4;
    let tb = TimeBase::new(1, SR as i64);
    let mut adts_packets: Vec<Vec<u8>> = Vec::new();

    let mut offset = 0usize;
    let mut frame_pts: i64 = 0;
    while offset + bytes_per_frame <= ref_bytes.len() {
        let chunk = ref_bytes[offset..offset + bytes_per_frame].to_vec();
        offset += bytes_per_frame;

        let frame = Frame::Audio(AudioFrame {
            samples: AAC_FRAME as u32,
            pts: Some(frame_pts),
            data: vec![chunk],
        });
        frame_pts += AAC_FRAME as i64;

        enc.send_frame(&frame).expect("send_frame");
        loop {
            match enc.receive_packet() {
                Ok(pkt) => adts_packets.push(pkt.data),
                Err(oxideav_core::Error::NeedMore) => break,
                Err(e) => panic!("receive_packet: {e}"),
            }
        }
    }

    enc.flush().unwrap();
    loop {
        match enc.receive_packet() {
            Ok(pkt) => adts_packets.push(pkt.data),
            Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("flush receive_packet: {e}"),
        }
    }

    assert!(!adts_packets.is_empty(), "no ADTS packets produced");

    // ── Decoder ────────────────────────────────────────────────────────
    let mut dec_params = CodecParameters::audio(CodecId::new("aac"));
    dec_params.sample_rate = Some(SR);
    dec_params.channels = Some(CH);

    let mut dec = decoder::make_decoder(&dec_params).expect("make_decoder");
    let mut decoded_samples: Vec<f32> = Vec::new();

    for adts_data in &adts_packets {
        let pkt = Packet::new(0, tb, adts_data.clone());
        dec.send_packet(&pkt).expect("send_packet");
        loop {
            match dec.receive_frame() {
                Ok(Frame::Audio(af)) => {
                    let plane = &af.data[0];
                    for chunk in plane.chunks_exact(4) {
                        decoded_samples.push(f32::from_le_bytes(chunk.try_into().unwrap()));
                    }
                }
                Ok(_) => {}
                Err(oxideav_core::Error::NeedMore) => break,
                Err(e) => panic!("receive_frame: {e}"),
            }
        }
    }

    assert!(
        !decoded_samples.is_empty(),
        "no PCM samples produced by decoder"
    );

    // ── SNR measurement with sliding-window delay search ───────────────
    // AAC LC has a well-known encoder/decoder group delay of ~2112 samples.
    // We search up to 4096 samples of delay to be robust.
    let ref_left: Vec<f32> = ref_samples.iter().step_by(CH as usize).copied().collect();
    let ref_right: Vec<f32> = ref_samples
        .iter()
        .skip(1)
        .step_by(CH as usize)
        .copied()
        .collect();
    let dec_left: Vec<f32> = decoded_samples.iter().step_by(CH as usize).copied().collect();
    let dec_right: Vec<f32> = decoded_samples
        .iter()
        .skip(1)
        .step_by(CH as usize)
        .copied()
        .collect();

    let window = 4096;
    let max_delay = 4096;

    let snr_left = best_snr_db(&ref_left, &dec_left, max_delay, window);
    let snr_right = best_snr_db(&ref_right, &dec_right, max_delay, window);

    eprintln!(
        "AAC round-trip SNR: L={snr_left:.1} dB  R={snr_right:.1} dB  \
         (ref={} interleaved, dec={} interleaved)",
        ref_samples.len(),
        decoded_samples.len()
    );

    assert!(
        snr_left >= 25.0,
        "left-channel SNR {snr_left:.1} dB < 25 dB"
    );
    assert!(
        snr_right >= 25.0,
        "right-channel SNR {snr_right:.1} dB < 25 dB"
    );
}
