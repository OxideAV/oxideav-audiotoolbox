# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Round 2: real AAC LC decode + encode factories** via `AudioConverterRef`
  loaded at runtime through `libloading`.
- `sys.rs` — full AudioConverter FFI bindings:
  `AudioConverterNew`, `AudioConverterDispose`, `AudioConverterReset`,
  `AudioConverterSetProperty`, `AudioConverterGetProperty`,
  `AudioConverterGetPropertyInfo`, `AudioConverterFillComplexBuffer`.
  Property keys `kAudioConverterEncodeBitRate` and
  `kAudioConverterPropertyMaximumOutputPacketSize` inlined as constants.
  `AudioStreamBasicDescription`, `AudioBufferList1`,
  `AudioStreamPacketDescription` and `AudioBuffer` structs defined.
  `AudioStreamBasicDescription::pcm_float32`, `pcm_s16`, `mpeg4_aac`
  convenience constructors added.
- `adts.rs` — lightweight ADTS framing helpers:
  `parse()` (strip header on decode side),
  `build_header()` (synthesise 7-byte ADTS header on encode side),
  `sample_rate_index()` (Hz → sampling-frequency-index table lookup).
- `decoder.rs` — `AacAtDecoder` implementing `oxideav_core::Decoder`.
  Accepts ADTS-framed AAC input packets; output is interleaved F32 PCM.
  Converter configured lazily on first packet from ADTS header.
  `make_decoder()` factory function registered with the codec registry.
- `encoder.rs` — `AacAtEncoder` implementing `oxideav_core::Encoder`.
  Accepts interleaved F32 (or S16) PCM `AudioFrame`s; output is
  ADTS-framed AAC. Target bitrate set via
  `kAudioConverterEncodeBitRate`. ADTS header synthesised from the
  configured sample-rate index + channel configuration.
  `make_encoder()` factory registered with the codec registry.
- `register()` installs both factories under codec id `"aac"` with:
  `implementation = "aac_audiotoolbox"`, `priority = 10`,
  `hardware_accelerated = true`, `lossy = true`.
  Tags claimed: `wFormatTag(0x00FF/0x706D/0x4143/0xA106)`,
  `mp4_oti(0x40)`, `matroska(A_AAC)`.
  Runtime dlopen failure → log + no-op (pure-Rust fallback kicks in).
- `decoder` and `encoder` modules now gated behind `#[cfg(feature = "registry")]`
  so `--no-default-features` builds remain free of `oxideav-core`.
- Integration test `tests/roundtrip.rs`: 2-second 440 Hz sine wave,
  48 kHz stereo, encoded at 128 kbit/s and decoded; per-channel SNR
  measured with sliding-window delay search — both channels ≥ 25 dB
  (measured: **36.7 dB**).

### Changed (round 1 → round 2)

- `register()` now installs real decoder and encoder factories instead
  of being a no-op.
- Initial scaffolding: `#![cfg(target_os = "macos")]` crate that
  dlopens AudioToolbox + CoreFoundation via `libloading` on first
  use. Smoke test verifies symbol resolution for `AudioConverterNew`
  + `CFRetain`.
- Unified `register(&mut RuntimeContext)` entry point matching the
  framework convention.
- Standalone-friendly: default-on `registry` feature gates the
  `oxideav-core` dep + the `register` fn.
- README documents the priority-10 placement (hardware preferred over
  pure-Rust) and the `--no-hwaccel` CLI opt-out.
