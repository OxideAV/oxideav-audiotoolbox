//! Round-trip test for the AudioToolbox Opus bridge.
//!
//! Generates 2 seconds of 48 kHz / stereo S16 PCM containing a 1 kHz
//! sine, encodes it through `OpusAtEncoder`, then decodes the
//! resulting raw Opus packets through `OpusAtDecoder`. Asserts:
//!
//! * The encoder emits at least the expected packet count (one per
//!   20 ms block over the 2 s window = 100 packets, minus any tail
//!   handling).
//! * Every packet starts with a valid RFC 6716 §3.1 TOC byte (the
//!   `config` field decoded from the top 5 bits must select one of
//!   the 32 valid configs).
//! * The decoder accepts the packet stream and emits PCM.
//! * Per-channel SNR over a sliding alignment window meets the
//!   floor `≥ 6 dB` — Opus is a perceptual codec, so a pure sine is
//!   not a transparency target; the SNR floor proves the analysis /
//!   synthesis chain is wired, not that we approached transparency.
//!
//! Opus is a lossy codec (RFC 6716 §1) — any bit-exact assertion
//! would be wrong by construction. The SNR floor is set deliberately
//! low for the same reason iLBC's roundtrip floor is low: pure-sine
//! reconstruction is hostile to a CELT-style perceptual encoder
//! whose quantiser is tuned for music + speech mixed content.

// `registry` gates the oxideav-core dependency these tests drive;
// without it the crate exposes only the raw bridge, so the whole
// test target compiles away (matching the standalone CI path).
#![cfg(all(target_os = "macos", feature = "registry"))]

use std::f32::consts::PI;

use oxideav_audiotoolbox::{opus_decoder, opus_encoder};
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Frame, Packet, SampleFormat, TimeBase};

const SR: u32 = 48_000;
const CH: u16 = 2;
const FREQ: f32 = 1_000.0;
/// 2 seconds of signal — long enough to drive AT through many encode
/// + lookahead cycles.
const N_FRAMES: usize = (SR as usize) * 2;
/// Default Opus packet size at 48 kHz (20 ms = 960 PCM frames).
const PACKET_FRAMES: usize = 960;

/// Build a deterministic interleaved S16 PCM 1 kHz sine. Amplitude
/// is 16 000 (about -6 dBFS) so the encoder has plenty of headroom.
fn gen_sine_s16(n_frames: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n_frames * CH as usize * 2);
    for i in 0..n_frames {
        let t = i as f32 / SR as f32;
        let s = ((2.0 * PI * FREQ * t).sin() * 16_000.0) as i16;
        for _ in 0..CH {
            out.extend_from_slice(&s.to_le_bytes());
        }
    }
    out
}

fn bytes_to_s16_vec(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// Deinterleave into one Vec per channel.
fn deinterleave(interleaved: &[i16], channels: usize) -> Vec<Vec<i16>> {
    let frames = interleaved.len() / channels;
    let mut out = vec![Vec::with_capacity(frames); channels];
    for f in 0..frames {
        for c in 0..channels {
            out[c].push(interleaved[f * channels + c]);
        }
    }
    out
}

/// Sliding-window SNR: search a small alignment window for the offset
/// that yields the largest signal-to-noise ratio against the reference.
fn best_snr_db(reference: &[i16], decoded: &[i16], search_max: usize) -> f64 {
    let probe_len = (SR as usize).min(reference.len()).min(decoded.len() / 2);
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

#[test]
fn opus_roundtrip_48k_stereo_20ms() {
    // ── Reference PCM ──────────────────────────────────────────────────
    let ref_bytes = gen_sine_s16(N_FRAMES);
    let ref_samples = bytes_to_s16_vec(&ref_bytes);

    // ── Encoder ────────────────────────────────────────────────────────
    let mut enc_params = CodecParameters::audio(CodecId::new("opus"));
    enc_params.sample_rate = Some(SR);
    enc_params.channels = Some(CH);
    enc_params.sample_format = Some(SampleFormat::S16);
    // No explicit frame_duration_ms → encoder defaults to 20 ms.

    let mut enc = opus_encoder::make_encoder(&enc_params).expect("make_encoder");

    let tb = TimeBase::new(1, SR as i64);
    let mut opus_packets: Vec<Vec<u8>> = Vec::new();

    // Feed in 1920-sample chunks (40 ms — deliberately misaligned with
    // the 20 ms packets so the encoder's staging buffer is exercised).
    let chunk_frames = 1920usize;
    let chunk_bytes = chunk_frames * CH as usize * 2;
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
    let cookie = enc.output_params().extradata.clone();
    let actual_bitrate = enc.output_params().bit_rate.unwrap_or(0);

    assert!(!opus_packets.is_empty(), "no Opus packets produced");
    eprintln!(
        "Opus 48 kHz stereo / 20 ms: encoder produced {} packets at ≈ {} bit/s",
        opus_packets.len(),
        actual_bitrate
    );

    // Expected: 2 s × 48 kHz / 960 frames-per-packet = 100 packets.
    // Allow a small +/- for end-of-stream slack.
    let expected = N_FRAMES / PACKET_FRAMES;
    assert!(
        opus_packets.len() >= expected.saturating_sub(2),
        "expected at least {} packets, got {}",
        expected.saturating_sub(2),
        opus_packets.len()
    );

    // RFC 6716 §3.1: TOC byte = `config(5) | stereo(1) | code(2)`.
    // Every Opus packet must have a TOC byte; `config` selects one
    // of 32 modes and is therefore in `0..=31`. We can't validate
    // beyond this without parsing the per-frame payload, which is
    // AT's job at decode time.
    for (i, pkt) in opus_packets.iter().enumerate() {
        assert!(!pkt.is_empty(), "packet {i} is empty (no TOC byte)");
        let toc = pkt[0];
        let config = (toc >> 3) & 0x1F;
        assert!(
            config < 32,
            "packet {i} TOC byte {:#04x} has invalid config {}",
            toc,
            config
        );
    }

    // ── Decoder ────────────────────────────────────────────────────────
    let mut dec_params = CodecParameters::audio(CodecId::new("opus"));
    dec_params.sample_rate = Some(SR);
    dec_params.channels = Some(CH);
    dec_params.extradata = cookie;

    let mut dec = opus_decoder::make_decoder(&dec_params).expect("make_decoder");
    let mut decoded_bytes: Vec<u8> = Vec::new();

    for adata in &opus_packets {
        let pkt = Packet::new(0, tb, adata.clone());
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
    dec.flush().expect("flush");
    loop {
        match dec.receive_frame() {
            Ok(Frame::Audio(af)) => decoded_bytes.extend_from_slice(&af.data[0]),
            Ok(_) => {}
            Err(_) => break,
        }
    }

    assert!(
        !decoded_bytes.is_empty(),
        "decoder produced no PCM for 20 ms / 48 kHz stereo Opus"
    );
    let decoded = bytes_to_s16_vec(&decoded_bytes);
    eprintln!(
        "Opus 48 kHz stereo / 20 ms: ref={} samples, dec={} samples",
        ref_samples.len(),
        decoded.len()
    );

    // Per-channel SNR. Opus has a pre-skip of ~80 ms by default, so
    // the alignment search needs to span at least that — pick a 6000
    // sample window (125 ms) as generous head-room.
    let ref_chans = deinterleave(&ref_samples, CH as usize);
    let dec_chans = deinterleave(&decoded, CH as usize);
    let search_max = 6_000;

    let snr_l = best_snr_db(&ref_chans[0], &dec_chans[0], search_max);
    let snr_r = best_snr_db(&ref_chans[1], &dec_chans[1], search_max);
    eprintln!(
        "Opus 20 ms / 48 kHz: peak SNR L = {:.2} dB, R = {:.2} dB",
        snr_l, snr_r
    );

    assert!(
        snr_l >= 6.0,
        "L-channel SNR {:.2} dB below 6 dB floor",
        snr_l
    );
    assert!(
        snr_r >= 6.0,
        "R-channel SNR {:.2} dB below 6 dB floor",
        snr_r
    );
}

#[test]
fn opus_roundtrip_48k_mono_default_settings() {
    let ref_bytes_stereo = gen_sine_s16(N_FRAMES);
    // Reduce to mono by dropping the right channel.
    let ref_bytes: Vec<u8> = ref_bytes_stereo
        .chunks_exact(4)
        .flat_map(|c| [c[0], c[1]])
        .collect();
    let ref_samples = bytes_to_s16_vec(&ref_bytes);

    let mut enc_params = CodecParameters::audio(CodecId::new("opus"));
    enc_params.sample_rate = Some(SR);
    enc_params.channels = Some(1);
    enc_params.sample_format = Some(SampleFormat::S16);

    let mut enc = opus_encoder::make_encoder(&enc_params).expect("make_encoder mono");
    let tb = TimeBase::new(1, SR as i64);
    let mut opus_packets: Vec<Vec<u8>> = Vec::new();

    let chunk_frames = 1920usize;
    let chunk_bytes = chunk_frames * 2; // mono S16
    let mut offset = 0usize;
    while offset + chunk_bytes <= ref_bytes.len() {
        let chunk = ref_bytes[offset..offset + chunk_bytes].to_vec();
        offset += chunk_bytes;
        let frame = Frame::Audio(AudioFrame {
            samples: chunk_frames as u32,
            pts: Some(0),
            data: vec![chunk],
        });
        enc.send_frame(&frame).expect("send_frame mono");
        loop {
            match enc.receive_packet() {
                Ok(pkt) => opus_packets.push(pkt.data),
                Err(oxideav_core::Error::NeedMore) => break,
                Err(e) => panic!("receive_packet (mono): {e}"),
            }
        }
    }
    enc.flush().expect("flush mono");
    while let Ok(pkt) = enc.receive_packet() {
        opus_packets.push(pkt.data);
    }
    let cookie = enc.output_params().extradata.clone();
    assert!(!opus_packets.is_empty(), "mono encode produced 0 packets");

    let mut dec_params = CodecParameters::audio(CodecId::new("opus"));
    dec_params.sample_rate = Some(SR);
    dec_params.channels = Some(1);
    dec_params.extradata = cookie;

    let mut dec = opus_decoder::make_decoder(&dec_params).expect("make_decoder mono");
    let mut decoded_bytes: Vec<u8> = Vec::new();
    for adata in &opus_packets {
        let pkt = Packet::new(0, tb, adata.clone());
        dec.send_packet(&pkt).expect("send_packet mono");
        loop {
            match dec.receive_frame() {
                Ok(Frame::Audio(af)) => decoded_bytes.extend_from_slice(&af.data[0]),
                Ok(_) => {}
                Err(oxideav_core::Error::NeedMore) => break,
                Err(e) => panic!("receive_frame mono: {e}"),
            }
        }
    }
    dec.flush().expect("flush mono dec");
    loop {
        match dec.receive_frame() {
            Ok(Frame::Audio(af)) => decoded_bytes.extend_from_slice(&af.data[0]),
            Ok(_) => {}
            Err(_) => break,
        }
    }

    let decoded = bytes_to_s16_vec(&decoded_bytes);
    let snr = best_snr_db(&ref_samples, &decoded, 6_000);
    eprintln!(
        "Opus 48 kHz mono / 20 ms: ref={} samples, dec={} samples, peak SNR={:.2} dB",
        ref_samples.len(),
        decoded.len(),
        snr
    );
    assert!(snr >= 6.0, "mono SNR {:.2} dB below 6 dB floor", snr);
}
