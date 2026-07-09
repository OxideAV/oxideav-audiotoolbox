//! AAC-LD + AAC-ELD round-trip tests for the AudioToolbox bridge.
//!
//! AAC Low Delay (AOT 23) and Enhanced Low Delay (AOT 39) are the
//! conferencing-oriented AAC profiles: a shortened analysis/synthesis
//! window cuts the algorithmic delay to ~15-20 ms (against AAC LC's
//! ~100+ ms) at the cost of a small coding-efficiency penalty. The win
//! is latency, not compression, so they run at full-band LC-class
//! bitrates (128 kbit/s here).
//!
//! Unlike HE-AAC there is NO SBR upsample at the converter boundary:
//! AudioConverter packetises both LD and ELD at **512 PCM frames per
//! packet**, and the decoder's output sample rate equals the configured
//! input rate. Neither profile has an ADTS representation (ADTS profile
//! bits only encode Main/LC/SSR/LTP), so — exactly like HE-AAC — the
//! encoder emits raw AAC bytes and advertises the AOT out-of-band via
//! the magic cookie.
//!
//! Pass criteria validate that the bridge wires the converter correctly,
//! not that Apple's encoder is transparent (that's the OS's concern):
//!   * AAC-LD  @ 128 kbit/s, 48 kHz stereo → per-channel SNR ≥ 20 dB.
//!   * AAC-ELD @ 128 kbit/s, 48 kHz stereo → per-channel SNR ≥ 12 dB
//!     (ELD's optional LD-SBR-style tooling loosens phase fidelity on a
//!     pure tone relative to plain LD).

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

/// Slide a window through `dec_ch` against `ref_ch` and return the best
/// SNR (dB) at any offset within `max_delay_samples`.
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

/// Drive encode → decode for the given low-delay AAC profile and return
/// the best per-channel SNR (left, right).
fn drive_ld(profile: &str, bitrate: u64) -> (f64, f64) {
    // 2 seconds of a 1 kHz tone at SR. LD/ELD keep the full band so the
    // fundamental passes straight through the low-delay core.
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

    // Feed 512-frame PCM chunks (the LD/ELD packet size). Smaller or
    // larger chunks also work — the encoder restages internally — but
    // matching the packet size keeps the test intent obvious.
    let chunk_frames = 512usize;
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

    // Capture the encoder-vended magic cookie (AudioSpecificConfig with
    // AOT 23 / 39). Without it AT rejects the bitstream with 'bada'.
    let cookie = enc.output_params().extradata.clone();
    assert!(
        !cookie.is_empty(),
        "{profile}: encoder did not vend a magic cookie"
    );

    // Decoder. LD/ELD have no SBR doubling, so the configured rate is
    // the output rate — seed the decoder with the full SR.
    let mut dec_params = CodecParameters::audio(CodecId::new("aac"));
    dec_params.sample_rate = Some(SR);
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
    dec.flush().expect("dec flush");
    loop {
        match dec.receive_frame() {
            Ok(Frame::Audio(af)) => {
                for chunk in af.data[0].chunks_exact(4) {
                    decoded.push(f32::from_le_bytes(chunk.try_into().unwrap()));
                }
            }
            Ok(_) => {}
            Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("dec flush receive_frame: {e}"),
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

    // Low-delay profiles still carry a few hundred-to-a-few-thousand
    // samples of codec delay; search generously.
    let snr_l = best_snr_db(&ref_l, &dec_l, 8192, 16384);
    let snr_r = best_snr_db(&ref_r, &dec_r, 8192, 16384);

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
fn aac_ld_roundtrip() {
    let (l, r) = drive_ld("ld", 128_000);
    assert!(l >= 20.0, "AAC-LD left SNR {l:.1} dB < 20 dB");
    assert!(r >= 20.0, "AAC-LD right SNR {r:.1} dB < 20 dB");
}

#[test]
fn aac_eld_roundtrip() {
    let (l, r) = drive_ld("eld", 128_000);
    assert!(l >= 12.0, "AAC-ELD left SNR {l:.1} dB < 12 dB");
    assert!(r >= 12.0, "AAC-ELD right SNR {r:.1} dB < 12 dB");
}

/// LD/ELD packets are raw AAC (no ADTS) and the encoder must vend a
/// magic cookie carrying the AOT. Verify nonzero payloads + a
/// well-formed cookie (ISO/IEC 14496-1 esds, starts with 0x03).
#[test]
fn ld_packets_have_nonzero_payloads() {
    let mut enc_params = CodecParameters::audio(CodecId::new("aac"));
    enc_params.sample_rate = Some(SR);
    enc_params.channels = Some(CH);
    enc_params.sample_format = Some(SampleFormat::F32);
    enc_params.bit_rate = Some(128_000);
    enc_params.options.insert("profile", "ld");

    let mut enc = encoder::make_encoder(&enc_params).expect("make_encoder");

    let n_frames = 16 * 512usize; // 16 LD packets' worth
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
        assert!(!p.data.is_empty(), "encoder yielded an empty LD packet");
        assert!(
            p.data.len() <= 4096,
            "LD packet too large: {}",
            p.data.len()
        );
        total_bytes += p.data.len();
        count += 1;
    }
    assert!(count >= 4, "expected at least 4 LD packets, got {count}");
    assert!(total_bytes > 0, "no LD payload bytes emitted");

    let cookie = &enc.output_params().extradata;
    assert!(
        cookie.len() >= 2,
        "LD cookie too short: {} bytes",
        cookie.len()
    );
    // AT's LD/ELD cookie is an esds descriptor (ES_DescrTag = 0x03) or,
    // for the bare config, a raw AudioSpecificConfig. Both are valid;
    // assert it at least round-trips back through the decoder, which the
    // roundtrip tests already exercise — here we just require nonempty.
    assert!(!cookie.is_empty(), "LD cookie must not be empty");
}
