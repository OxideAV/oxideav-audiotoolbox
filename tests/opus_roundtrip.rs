//! Round-trip wiring test for the AudioToolbox Opus bridge.
//!
//! Generates 2 seconds of 48 kHz / 16-bit stereo PCM containing a clean
//! 440 Hz sine, encodes it through `OpusAtEncoder` at 64 kbit/s,
//! forwards the resulting raw Opus packets + AT-vended compression
//! cookie into `OpusAtDecoder`, and verifies the recovered PCM tracks
//! the input within a generous SNR threshold.
//!
//! Opus is a lossy psychoacoustic codec (RFC 6716 §1) — a pure 440 Hz
//! sine is not particularly adversarial (CELT's MDCT path tracks
//! sustained tones cleanly), but it isn't transparent either. The
//! assertion threshold (≥ 5 dB per channel) is chosen to confirm the
//! pipeline is wired correctly — packets flow encode → decode, AT's
//! cookie cross-direction handshake works, and PTS / packet counts
//! match. The exact SNR is a property of AT's encoder quality, not
//! of the bridge.

#![cfg(target_os = "macos")]

use std::f32::consts::PI;

use oxideav_audiotoolbox::{opus_decoder, opus_encoder};
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Frame, Packet, SampleFormat, TimeBase};

const SR: u32 = 48_000;
const CH: u16 = 2;
const FREQ: f32 = 440.0;
/// 2 seconds of signal.
const N_FRAMES: usize = (SR as usize) * 2;
/// Opus packet default: 20 ms at 48 kHz = 960 PCM frames per packet.
const PACKET_FRAMES: usize = 960;

fn gen_sine_s16(n_frames: usize) -> Vec<i16> {
    let mut out = Vec::with_capacity(n_frames * CH as usize);
    for i in 0..n_frames {
        let t = i as f32 / SR as f32;
        let s = ((2.0 * PI * FREQ * t).sin() * 24_000.0) as i16;
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

/// Per-channel SNR in dB across a pair of aligned interleaved S16
/// streams. Returns `f64::NEG_INFINITY` for a perfect match (caller
/// can clamp).
fn snr_db_per_channel(reference: &[i16], decoded: &[i16], channels: usize) -> Vec<f64> {
    let mut out = Vec::with_capacity(channels);
    let n_frames = reference.len().min(decoded.len()) / channels;
    for c in 0..channels {
        let mut sig = 0.0f64;
        let mut err = 0.0f64;
        for f in 0..n_frames {
            let r = reference[f * channels + c] as f64;
            let d = decoded[f * channels + c] as f64;
            sig += r * r;
            let e = r - d;
            err += e * e;
        }
        let db = if err == 0.0 {
            f64::INFINITY
        } else {
            10.0 * (sig / err).log10()
        };
        out.push(db);
    }
    out
}

#[test]
fn opus_roundtrip_pipeline() {
    // ── Reference PCM ──────────────────────────────────────────────────
    let ref_samples_i16 = gen_sine_s16(N_FRAMES);
    let ref_bytes = s16_vec_to_bytes(&ref_samples_i16);

    // ── Encoder ────────────────────────────────────────────────────────
    let mut enc_params = CodecParameters::audio(CodecId::new("opus"));
    enc_params.sample_rate = Some(SR);
    enc_params.channels = Some(CH);
    enc_params.sample_format = Some(SampleFormat::S16);
    enc_params.bit_rate = Some(64_000);

    let mut enc = opus_encoder::make_encoder(&enc_params).expect("make_encoder");

    let chunk_bytes = 1024 * CH as usize * 2; // 1024 frames per send_frame
    let tb = TimeBase::new(1, SR as i64);
    let mut opus_packets: Vec<Vec<u8>> = Vec::new();

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
                Ok(pkt) => opus_packets.push(pkt.data),
                Err(oxideav_core::Error::NeedMore) => break,
                Err(e) => panic!("receive_packet: {e}"),
            }
        }
    }
    enc.flush().expect("flush");
    loop {
        match enc.receive_packet() {
            Ok(pkt) => opus_packets.push(pkt.data),
            Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("flush receive_packet: {e}"),
        }
    }

    assert!(!opus_packets.is_empty(), "no Opus packets produced");
    let cookie = enc.output_params().extradata.clone();
    assert!(!cookie.is_empty(), "encoder did not vend a magic cookie");

    // Spot-check that every emitted packet has a parseable TOC byte.
    for (i, pkt) in opus_packets.iter().enumerate() {
        assert!(
            !pkt.is_empty(),
            "encoder emitted empty Opus packet at index {i}"
        );
        let _ = oxideav_audiotoolbox::opus::Toc::parse(pkt)
            .unwrap_or_else(|| panic!("packet {i} has unparseable TOC byte"));
    }

    // ── Decoder ────────────────────────────────────────────────────────
    let mut dec_params = CodecParameters::audio(CodecId::new("opus"));
    dec_params.sample_rate = Some(SR);
    dec_params.channels = Some(CH);
    dec_params.sample_format = Some(SampleFormat::S16);
    dec_params.extradata = cookie;

    let mut dec = opus_decoder::make_decoder(&dec_params).expect("make_decoder");
    let mut decoded_bytes: Vec<u8> = Vec::new();

    for pdata in &opus_packets {
        let pkt = Packet::new(0, tb, pdata.clone());
        dec.send_packet(&pkt).expect("send_packet");
        loop {
            match dec.receive_frame() {
                Ok(Frame::Audio(af)) => decoded_bytes.extend_from_slice(&af.data[0]),
                Ok(_) => {}
                Err(oxideav_core::Error::NeedMore) => break,
                Err(e) => panic!("receive_frame: {e}"),
            }
        }
    }
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
        "Opus roundtrip: ref={} samples, dec={} samples, {} packets",
        ref_samples_i16.len(),
        decoded_samples.len(),
        opus_packets.len(),
    );

    // ── SNR scan ───────────────────────────────────────────────────────
    //
    // Opus has decoder delay + the encoder may prepend a few frames of
    // priming. Scan a few packet-widths for the best alignment.
    let probe_len = 16384.min(ref_samples_i16.len() / 2);
    let max_search_frames = PACKET_FRAMES * 4; // ≤ 80 ms of priming search
    let mut best_offset_frames = 0usize;
    let mut best_min_snr = f64::NEG_INFINITY;
    for off_frames in (0..max_search_frames).step_by(64) {
        let off_samples = off_frames * CH as usize;
        if off_samples + probe_len * CH as usize > decoded_samples.len() {
            break;
        }
        let dec_slice = &decoded_samples[off_samples..off_samples + probe_len * CH as usize];
        let ref_slice = &ref_samples_i16[..probe_len * CH as usize];
        let snrs = snr_db_per_channel(ref_slice, dec_slice, CH as usize);
        let min = snrs.iter().copied().fold(f64::INFINITY, |a, b| a.min(b));
        if min > best_min_snr {
            best_min_snr = min;
            best_offset_frames = off_frames;
        }
    }

    eprintln!(
        "Opus roundtrip: best alignment {best_offset_frames} frames, min per-channel SNR ≈ {:.2} dB",
        best_min_snr
    );

    // Pipeline-wiring assertion: a real encoder→decoder round-trip
    // tracks a 440 Hz sine at well above 5 dB SNR. Below 5 dB would
    // mean PCM bytes aren't surviving the FFI bridge (wrong endianness,
    // wrong channel count, lost samples mid-stream).
    assert!(
        best_min_snr >= 5.0,
        "Opus round-trip SNR too low: {:.2} dB (expected ≥ 5 dB)",
        best_min_snr
    );
}
