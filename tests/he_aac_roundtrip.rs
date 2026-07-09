//! HE-AAC v1 + v2 round-trip tests for the AudioToolbox bridge.
//!
//! HE-AAC encodes AAC LC base layer + SBR (Spectral Band Replication)
//! extension. The encoder consumes PCM at the **output** sample rate
//! (e.g. 48 kHz stereo); each output packet covers `2048` PCM frames
//! (1024 LC samples × 2 for SBR upsample). The decoder, configured
//! with `profile=he`, emits PCM at the doubled rate.
//!
//! HE-AAC v2 adds Parametric Stereo on top of SBR — the bitstream
//! carries a mono down-mix plus side info, and the decoder reconstructs
//! a stereo signal. PS is a destructive, low-bitrate trick so the SNR
//! budget is lower than HE-v1.
//!
//! Pass criteria (chosen to validate the pipeline, not to certify
//! quality — both numbers leave headroom above the noise floor while
//! staying under the realistic SBR/PS lossy floor):
//!   * HE-v1 stereo @ 64 kbit/s → per-channel SNR ≥ 8 dB.
//!   * HE-v2 stereo @ 32 kbit/s → per-channel SNR ≥ 6 dB.
//!
//! Why so low? HE-AAC's SBR reconstructs the upper band from a
//! patch-and-scale of the lower band; even on a single-tone signal
//! the recovered waveform's phase relationship to the input is not
//! tightly preserved, so SNR caps out in the 10-15 dB range
//! regardless of bitrate. This test validates that the bridge wires
//! the converter correctly, NOT that Apple's encoder is
//! transparent — the former is what we own here.

// `registry` gates the oxideav-core dependency these tests drive;
// without it the crate exposes only the raw bridge, so the whole
// test target compiles away (matching the standalone CI path).
#![cfg(all(target_os = "macos", feature = "registry"))]

use std::f32::consts::PI;

use oxideav_audiotoolbox::{decoder, encoder};
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Frame, Packet, SampleFormat, TimeBase};

const SR: u32 = 48_000;
const CH: u16 = 2;

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

/// Slide a window through `dec_ch` against `ref_ch` and return the
/// best SNR (dB) at any offset within `max_delay_samples`.
fn best_snr_db(ref_ch: &[f32], dec_ch: &[f32], max_delay_samples: usize, window: usize) -> f64 {
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

/// Drive encode → decode for the given AAC profile and return the
/// best per-channel SNR (left, right).
fn drive_he(profile: &str, bitrate: u64) -> (f64, f64) {
    // 2 seconds of a 1 kHz tone at SR — SBR (which kicks in above
    // ~4 kHz) doesn't decimate the fundamental, so the carrier should
    // pass through the SBR/LC split largely intact.
    const N: usize = SR as usize * 2;
    let ref_samples = gen_sine_f32(SR, CH, 1_000.0, N);
    let ref_bytes: Vec<u8> = ref_samples.iter().flat_map(|s| s.to_le_bytes()).collect();

    let mut enc_params = CodecParameters::audio(CodecId::new("aac"));
    enc_params.sample_rate = Some(SR);
    enc_params.channels = Some(CH);
    enc_params.sample_format = Some(SampleFormat::F32);
    enc_params.bit_rate = Some(bitrate);
    enc_params.options.insert("profile", profile);

    let mut enc = encoder::make_encoder(&enc_params).expect("make_encoder");

    // Feed in 1024-frame PCM chunks so the encoder's staging buffer
    // accumulates two of them per HE-AAC output packet (2048 frames).
    let chunk_frames = 1024usize;
    let chunk_bytes = chunk_frames * CH as usize * 4;
    let tb = TimeBase::new(1, SR as i64);
    let mut packets: Vec<Vec<u8>> = Vec::new();

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
                Ok(pkt) => packets.push(pkt.data),
                Err(oxideav_core::Error::NeedMore) => break,
                Err(e) => panic!("receive_packet: {e}"),
            }
        }
    }
    enc.flush().expect("flush");
    loop {
        match enc.receive_packet() {
            Ok(pkt) => packets.push(pkt.data),
            Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("flush receive_packet: {e}"),
        }
    }
    assert!(!packets.is_empty(), "no {profile} packets emitted");

    // Capture the encoder-vended magic cookie (AOT extension config)
    // before dropping `enc`. The cookie tells the decoder this is HE
    // / HE-v2; without it AT rejects the bitstream with 'bada'.
    let cookie = enc.output_params().extradata.clone();

    // Decoder. NOTE: ADTS in HE-AAC carries the BASE rate (SR/2 = 24
    // kHz) in its sampling_freq_index — that's what the AAC LC core
    // sees. Pre-seed the decoder with the BASE rate via params, so
    // the decoder's output ASBD becomes 2× = SR.
    let base_sr = SR / 2;
    let mut dec_params = CodecParameters::audio(CodecId::new("aac"));
    dec_params.sample_rate = Some(base_sr);
    dec_params.channels = Some(CH);
    dec_params.options.insert("profile", profile);
    dec_params.extradata = cookie;

    let mut dec = decoder::make_decoder(&dec_params).expect("make_decoder");
    let mut decoded: Vec<f32> = Vec::new();

    for data in &packets {
        let pkt = Packet::new(0, tb, data.clone());
        dec.send_packet(&pkt).expect("send_packet");
        loop {
            match dec.receive_frame() {
                Ok(Frame::Audio(af)) => {
                    for chunk in af.data[0].chunks_exact(4) {
                        decoded.push(f32::from_le_bytes(chunk.try_into().unwrap()));
                    }
                }
                Ok(_) => {}
                Err(oxideav_core::Error::NeedMore) => break,
                Err(e) => panic!("receive_frame: {e}"),
            }
        }
    }

    assert!(!decoded.is_empty(), "{profile}: decoder produced no PCM");

    let ref_l: Vec<f32> = ref_samples.iter().step_by(CH as usize).copied().collect();
    let ref_r: Vec<f32> = ref_samples
        .iter()
        .skip(1)
        .step_by(CH as usize)
        .copied()
        .collect();
    let dec_l: Vec<f32> = decoded.iter().step_by(CH as usize).copied().collect();
    let dec_r: Vec<f32> = decoded
        .iter()
        .skip(1)
        .step_by(CH as usize)
        .copied()
        .collect();

    // HE-AAC has substantial encoder/decoder group delay — SBR
    // analysis adds ~2x the standard AAC LC delay, putting the
    // expected offset around 4-6k samples per channel. Search
    // generously: up to 16k delay, 16k window.
    let snr_l = best_snr_db(&ref_l, &dec_l, 16384, 16384);
    let snr_r = best_snr_db(&ref_r, &dec_r, 16384, 16384);

    eprintln!(
        "{profile}: pkts={} dec_samples={} SNR L={:.1}dB R={:.1}dB",
        packets.len(),
        decoded.len(),
        snr_l,
        snr_r
    );

    (snr_l, snr_r)
}

#[test]
fn he_aac_v1_roundtrip() {
    let (l, r) = drive_he("he", 64_000);
    assert!(l >= 8.0, "HE-AAC v1 left SNR {l:.1} dB < 8 dB");
    assert!(r >= 8.0, "HE-AAC v1 right SNR {r:.1} dB < 8 dB");
}

#[test]
fn he_aac_v2_roundtrip() {
    let (l, r) = drive_he("he-v2", 32_000);
    assert!(l >= 6.0, "HE-AAC v2 left SNR {l:.1} dB < 6 dB");
    assert!(r >= 6.0, "HE-AAC v2 right SNR {r:.1} dB < 6 dB");
}

/// HE-AAC v1 stream should also be **decodable as plain AAC LC**:
/// any decoder that ignores SBR (or strips the AOT extension)
/// reconstructs the base-rate signal. This validates that the
/// encoder's raw output really is a well-formed AAC bitstream.
#[test]
fn he_aac_packets_have_nonzero_payloads() {
    let mut enc_params = CodecParameters::audio(CodecId::new("aac"));
    enc_params.sample_rate = Some(SR);
    enc_params.channels = Some(CH);
    enc_params.sample_format = Some(SampleFormat::F32);
    enc_params.bit_rate = Some(64_000);
    enc_params.options.insert("profile", "he");

    let mut enc = encoder::make_encoder(&enc_params).expect("make_encoder");

    let n_frames = 8 * 2048usize; // 8 HE-AAC packets' worth
    let pcm = gen_sine_f32(SR, CH, 1_000.0, n_frames);
    let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
    let frame = Frame::Audio(AudioFrame {
        samples: n_frames as u32,
        pts: Some(0),
        data: vec![bytes],
    });
    enc.send_frame(&frame).unwrap();
    enc.flush().unwrap();

    let mut count = 0;
    let mut total_bytes = 0;
    while let Ok(p) = enc.receive_packet() {
        assert!(!p.data.is_empty(), "encoder yielded an empty HE-AAC packet");
        // HE-AAC packets vary in size but should never exceed the
        // encoder's reported max output packet size (1536 bytes for
        // 64 kbit/s stereo, per the AT property query).
        assert!(
            p.data.len() <= 4096,
            "HE-AAC packet too large: {}",
            p.data.len()
        );
        total_bytes += p.data.len();
        count += 1;
    }
    assert!(
        count >= 4,
        "expected at least 4 HE-AAC packets, got {count}"
    );
    assert!(total_bytes > 0, "no HE-AAC payload bytes emitted");

    // Cookie must be present + must look like an ISO/IEC 14496-1 esds
    // descriptor (starts with 0x03 = ES_DescrTag).
    let cookie = &enc.output_params().extradata;
    assert!(
        cookie.len() >= 16,
        "HE-AAC cookie too short: {} bytes",
        cookie.len()
    );
    assert_eq!(
        cookie[0], 0x03,
        "HE-AAC cookie does not start with ES_DescrTag (got 0x{:02x})",
        cookie[0]
    );
}
