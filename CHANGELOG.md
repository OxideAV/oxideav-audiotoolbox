# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Round 5: AAC-LD + AAC-ELD encode + decode** via `AudioConverterRef`.
  Adds the `kAudioFormatMPEG4AAC_LD` (`'aacl'`, AOT 23) and
  `kAudioFormatMPEG4AAC_ELD` (`'aace'`, AOT 39) format IDs with matching
  `AudioStreamBasicDescription::mpeg4_aac_ld` / `mpeg4_aac_eld`
  constructors (**512 PCM frames per packet** — the shortened low-delay
  core, with no SBR upsample at the converter boundary, so the decoder's
  output sample rate equals the configured input rate). Two new
  `AacProfile` variants `Ld` / `Eld`, selected via
  `CodecParameters::options.get("profile")` = `"ld"` / `"eld"` (also
  `"LD"` / `"aac-ld"` / `"ELD"` / `"aac-eld"`).
- **Raw-framing generalisation**: a private `AacProfile::is_raw()` now
  drives the ADTS-vs-raw output decision. Only bare AAC LC is wrapped in
  a 7-byte ADTS header; every extended AOT (HE / HE-v2 / LD / ELD) is
  emitted as raw AAC bytes with the AOT carried out-of-band in the
  encoder-vended magic cookie. LD / ELD have no ADTS representation
  (ADTS profile bits encode only Main/LC/SSR/LTP), so the cookie path is
  mandatory: the decoder forwards it via
  `kAudioConverterDecompressionMagicCookie`, mirroring the HE path.
- Integration test `tests/ld_aac_roundtrip.rs` with three cases:
  - `aac_ld_roundtrip` — 2-second 1 kHz sine, 48 kHz stereo @
    128 kbit/s AAC-LD, encode → decode → per-channel SNR ≥ 20 dB
    (measured: ~29 dB; LD is near-transparent at full-band bitrates).
  - `aac_eld_roundtrip` — same signal @ 128 kbit/s AAC-ELD, per-channel
    SNR ≥ 12 dB (measured: ~26 dB).
  - `ld_packets_have_nonzero_payloads` — sanity check that the encoder
    emits ≥ 4 nonzero raw AAC packets and a non-empty magic cookie.
- New unit tests for the `Ld` / `Eld` profile parse + frames-per-packet
  (512), the `is_raw()` framing predicate, the two new ASBD geometries,
  and the `'aacl'` / `'aace'` FourCC byte mappings.

### Fixed

- The `register_tests` module in `lib.rs` is now gated on
  `#[cfg(all(test, feature = "registry"))]`. It references the
  `registry`-only `register()` entry point and `oxideav_core`, so a
  macOS `cargo test --no-default-features --lib` previously failed to
  compile. (CI's standalone job runs on Linux, where the whole
  `#![cfg(target_os = "macos")]` crate compiles away, so it never
  surfaced there — but a local macOS standalone test build did.)

- **Round 4: HE-AAC v1 + v2 encode + decode** via `AudioConverterRef`.
  Adds the `kAudioFormatMPEG4AAC_HE` (`'aach'`) and
  `kAudioFormatMPEG4AAC_HE_V2` (`'aacp'`) format IDs with matching
  `AudioStreamBasicDescription::mpeg4_aac_he` / `mpeg4_aac_he_v2`
  constructors (2048 PCM frames per packet, reflecting the SBR 2×
  upsample). Profile selection happens through
  `CodecParameters::options.get("profile")`: `"lc"` (default), `"he"`,
  `"he-v2"`. New `AacProfile` enum exported from `encoder` for
  programmatic use; HE-v2 mono is rejected at construction time per
  the spec (PS only meaningful for stereo).
- **Encoder magic cookie**: the AAC encoder now reads back its
  vended ISO/IEC 14496-1 `esds` descriptor via
  `kAudioConverterCompressionMagicCookie` and exposes it through
  `output_params.extradata` (42 bytes for HE / HE-v2, 2 bytes for
  bare LC AudioSpecificConfig). Required for downstream HE-AAC
  decode because the AOT extension descriptor is not present in
  any ADTS-like framing.
- **Decoder magic cookie**: the AAC decoder accepts the cookie via
  `CodecParameters::extradata` and forwards it through
  `kAudioConverterDecompressionMagicCookie`. Without the cookie,
  AT rejects HE / HE-v2 bitstreams with `kAudioCodecBadDataError`
  (`'bada'`).
- **HE / HE-v2 output framing**: HE-AAC packets are emitted as raw
  AAC bytes (no ADTS wrapper). ADTS only carries the base AAC LC
  profile bits, so wrapping HE-AAC payload in an ADTS header would
  be misleading and trigger decoder mis-identification. The
  encoder's `output_params.extradata` carries the AOT extension
  descriptor needed by downstream consumers. LC encode still emits
  the 7-byte ADTS header for back-compat with stock AAC decoders.
- **Encoder PCM staging buffer** and **decoder input packet queue**:
  both sides now keep enough lookahead beyond the immediate work
  unit to satisfy AT's HE-AAC SBR analysis without ever returning
  "0 packets" from the input callback mid-stream (which would put
  AT into a permanent EOS state and silently halt output). Tuned
  to 5 packets of PCM (encoder) / 4 packets of compressed input
  (decoder) — empirically matches AT's HE-AAC lookahead.
- `encoder.rs` now publishes `output_params` with `sample_rate`,
  `channels`, `bit_rate`, `extradata`, and `options["profile"]`
  echoed back so a downstream consumer (e.g. an MP4 muxer or a
  paired decoder) can reconstruct full configuration from the
  encoder's vended values alone.
- Integration test `tests/he_aac_roundtrip.rs` with three cases:
  - `he_aac_v1_roundtrip` — 2-second 1 kHz sine, 48 kHz stereo @
    64 kbit/s HE-AAC, encode → decode → per-channel SNR ≥ 8 dB
    (measured: ~11 dB; SBR's patch-and-scale reconstruction caps
    the recoverable phase fidelity below transparency).
  - `he_aac_v2_roundtrip` — same signal @ 32 kbit/s HE-AAC v2 with
    Parametric Stereo, per-channel SNR ≥ 6 dB (measured: ~10 dB).
  - `he_aac_packets_have_nonzero_payloads` — sanity check that the
    encoder emits ≥ 4 nonzero raw AAC packets within a 16k-frame
    feed and that the cookie starts with `0x03` (ES_DescrTag).

### Changed

- AAC LC encoder semantics: `send_frame` now stages PCM in an
  internal buffer and drains in `frames_per_packet`-sized chunks
  rather than encoding one frame per send. Behaviour is identical
  for callers that already submitted 1024-frame chunks; callers
  passing differently-sized chunks now see the expected
  packetisation. Required for HE / HE-v2 (which want 2048-frame
  packets) but applied uniformly so the contract is consistent.
- AAC LC decoder semantics: `send_packet` now queues the packet
  and lets the converter pull from the queue across multiple
  `FillComplexBuffer` calls, instead of feeding one packet per
  call. PCM frames are accumulated into a queue drained by
  `receive_frame`. Backwards compatible — `receive_frame` returns
  the same frame sequence.
- `encoder::flush()` drains AT's internal SBR lookahead with up to
  16 zero-input `FillComplexBuffer` calls so the last few packets
  of every stream are recovered. Without this drain, HE-AAC
  streams lose ~4 trailing packets.
- `decoder::flush()` mirrors the encoder: drains the AT decoder's
  internal buffer with extra zero-input pulls.

### Round 3 — ALAC (Apple Lossless) decode + encode

- ALAC decode + encode via `AudioConverterRef` with magic-cookie wiring.
- `alac.rs` — 24-byte `ALACSpecificConfig` magic-cookie builder + parser
  (big-endian wire format per Apple's `ALACMagicCookieDescription` doc
  snapshot in `docs/audio/alac/`). Default tuning constants (frame_length
  4096, pb=40, mb=10, kb=14, max_run=255) match Apple's documented
  recommendation. `bit_depth_flag()` maps PCM bit depth to the
  AudioFormatFlags value the AT ASBD expects.
- `alac_decoder.rs` — `AlacAtDecoder` implementing `oxideav_core::Decoder`.
  Reads the magic cookie from `CodecParameters::extradata` and forwards
  it to AT via `kAudioConverterDecompressionMagicCookie`. If no cookie
  is supplied, synthesises a minimal-but-valid one from the explicit
  `sample_rate / channels / sample_format` parameters. Output is
  interleaved S16 PCM, one `AudioFrame` per ALAC packet.
- `alac_encoder.rs` — `AlacAtEncoder` implementing `oxideav_core::Encoder`.
  Internal staging buffer accumulates PCM until a full 4096-frame
  packet is available, then emits one ALAC packet per drain step. The
  encoder-vended magic cookie is read back via
  `kAudioConverterCompressionMagicCookie` and exposed through
  `output_params.extradata` so downstream muxers (mov / m4a / caf) can
  emit a working ALAC track.
- `sys.rs` — added FourCC + property constants for ALAC:
  `kAudioFormatAppleLossless`, the four
  `kAppleLosslessFormatFlag_*BitSourceData` values, and the
  `kAudioConverterDecompression/CompressionMagicCookie` property keys.
  New `AudioStreamBasicDescription::apple_lossless()` constructor.
- `register()` now also installs ALAC decoder + encoder factories under
  codec id `"alac"` with implementation `alac_audiotoolbox`,
  priority 10, `hardware_accelerated = true`, `lossy = false`. Tags
  claimed: `fourcc(b"alac")`, `matroska(A_ALAC)`.
- Integration test `tests/alac_roundtrip.rs`: 2-second 48 kHz / 16-bit
  stereo sine+LCG-noise mix, encode → decode through the AT bridge,
  asserts **190,464 / 192,000 samples bit-exact** with zero priming
  silence — proves the codec is truly lossless end-to-end.

## [0.0.2](https://github.com/OxideAV/oxideav-audiotoolbox/compare/v0.0.1...v0.0.2) - 2026-05-06

### Other

- drop dead `linkme` dep
- clarify load-vs-init fallback + document require_hardware opt-out
- apply cargo fmt (rustfmt CI compliance)
- round 2: real AAC LC decode + encode via AudioConverterRef
- auto-register via oxideav_core::register! macro (linkme distributed slice)

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
