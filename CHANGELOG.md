# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Round 9: MP3 (MPEG-1/2/2.5 Audio Layer III, ISO/IEC 11172-3 /
  ISO/IEC 13818-3) decode** via `AudioConverterRef`. AudioToolbox
  exposes `kAudioFormatMPEGLayer3` (`'.mp3'`) as a **decode-only**
  target — there is no paired AT encoder for any MPEG audio layer, so
  the registry installs only a decoder factory under codec id `"mp3"`
  with `implementation = "mp3_audiotoolbox"`, `priority = 10`,
  `hardware_accelerated = true`, `lossy = true`. Tags claimed:
  `fourcc(b".mp3")` (MOV `.mp3` sample entries), `mp4_object_type(0x6B)`
  (MPEG-1/2 Audio OTI in `mp4a` sample entries), `matroska(A_MPEG/L3)`,
  and `wave_format(0x0055)` (Microsoft's RIFF/WAVE tag for MP3-in-WAV).
- New `src/mp3.rs` module exposing the `FrameHeader` parser (32-bit
  MPEG audio header — sync + version + layer + bitrate-index +
  sample-rate-index + padding + channel-mode), `Version` /
  `Layer` / `ChannelMode` enums, per-(version × layer) bitrate
  lookup tables matching ISO/IEC 11172-3 §2.4.2.3 and 13818-3
  §2.4.2.3, sample-rate-index tables for MPEG-1 (44.1/48/32 kHz),
  MPEG-2 LSF (22.05/24/16 kHz), and MPEG-2.5 (11.025/12/8 kHz),
  the `samples_per_frame` matrix (384 for Layer I, 1152 for MPEG-1
  Layer II/III, 576 for MPEG-2 LSF Layer III), and `frame_length`
  computation for all (version × layer × padding) combinations.
- `src/mp3_decoder.rs` — `Mp3AtDecoder` implementing
  `oxideav_core::Decoder`. Lazy first-packet configure phase
  inspects the first frame's header and constructs the matching
  ASBD (`mpeg_layer_1` / `mpeg_layer_2` / `mpeg_layer_3`); mid-
  stream layer / sample-rate / channel-mode changes are rejected
  with typed errors (AT would refuse them too, but the typed
  diagnostic is friendlier than the raw OSStatus). Per-packet
  validation: declared frame length from the header must match
  the packet byte count. Persistent input-packet queue with
  one-packet-of-slack lookahead matching the iLBC / AMR / HE-AAC
  pattern so AT never sees "0 packets" mid-stream. `flush()`
  drains the trailing PCM held back by the slack policy and the
  AT decoder's internal lookahead.
- New `K_AUDIO_FORMAT_MPEG_LAYER_3 = '.mp3'`,
  `K_AUDIO_FORMAT_MPEG_LAYER_2 = '.mp2'`, and
  `K_AUDIO_FORMAT_MPEG_LAYER_1 = '.mp1'` constants +
  matching `AudioStreamBasicDescription::mpeg_layer_{1,2,3}`
  constructors in `sys.rs`. Compressed-input ASBDs are
  header-driven (`bytes_per_packet = 0`, `frames_per_packet = 0`).
- Integration test `tests/mp3_decode.rs` using the staged
  MPEG-1 Layer III 128 kbit/s 44.1 kHz stereo fixture at
  `docs/audio/mp3/fixtures/layer3-stereo-44100-128kbps/`:
  - `mp3_decoder_accepts_real_fixture` — feeds all 33 frames
    through the decoder, drains PCM after each + flush, asserts
    ≥ 90 % of the expected 33 × 1152 = 38_016 per-channel
    sample count emerges (the actual count is bit-exact).
  - `mp3_decoder_pcm_resembles_reference` — decodes the entire
    fixture, parses the staged `expected.wav` (PCM S16LE), and
    computes the best-alignment peak SNR over a 2400-sample
    priming-offset search window. **Per-channel SNR ≥ 89 dB**
    measured (the assertion floor is 12 dB to remain
    forgiving of macOS-version IMDCT-rounding drift).
  - `split_frames_walks_real_mp3` — pins the ID3v2 skip +
    `FrameHeader::frame_length` interaction so the test fixture
    yields the expected frame count cleanly.
- New unit tests for `FrameHeader::parse` on every plausible
  header shape (MPEG-1 Layer III 128k 44.1k stereo, padding,
  MPEG-2 LSF Layer III 64k 22.05k mono, MPEG-2.5 Layer III 8k
  8k mono, MPEG-1 Layer II 192k 48k, MPEG-1 Layer I 256k 32k),
  rejection of every reserved field value (sync missing,
  reserved version `01`, reserved layer `00`, free-format
  bitrate-index `0`, reserved bitrate-index `15`, reserved
  sample-rate-index `3`), the `samples_per_frame` matrix, the
  `ChannelMode::channel_count` mapping, decoder construction,
  lazy-configure pinning of `(sample_rate, channels,
  frames_per_packet, layer)`, undersized-frame-body rejection,
  invalid-header rejection, mid-stream layer-change rejection,
  the `'.mp1'` / `'.mp2'` / `'.mp3'` FourCC byte mappings, the
  MP3 / MP2 / MP1 ASBD geometries (compressed inputs, both
  byte-count and frame-count header-driven), and registry
  presence (decoder only — encoder must NOT be registered
  since AT is decode-only for every MPEG audio layer).

## [0.0.3](https://github.com/OxideAV/oxideav-audiotoolbox/compare/v0.0.2...v0.0.3) - 2026-05-30

### Other

- round 7: AMR-NB narrow-band speech decode via AudioConverter
- round 6: iLBC narrow-band speech decode + encode via AudioConverter
- report AT's quantised bitrate via output_params.bit_rate
- round 5: AAC-LD + AAC-ELD encode + decode via AudioConverter
- round 4: HE-AAC v1 + v2 encode + decode via AudioConverter
- round 3: ALAC decode + encode via AudioConverter
- add .gitignore + drop committed Cargo.lock

### Added

- **Round 8: AMR-WB (Adaptive Multi-Rate Wideband, 3GPP TS 26.171 /
  TS 26.201 / RFC 4867) decode** via `AudioConverterRef`. AudioToolbox
  exposes `kAudioFormatAMR_WB` (`'sawb'`) as a **decode-only** target
  — there is no paired AT encoder (same asymmetry as AMR-NB), so the
  registry installs only a decoder factory under codec id `"amr_wb"`
  with `implementation = "amr_wb_audiotoolbox"`, `priority = 10`,
  `hardware_accelerated = true`, `lossy = true`. Tags claimed:
  `fourcc(b"sawb")` (3GPP / ISOBMFF) and `matroska(A_AMR/WB)`.
- New `src/amr_wb.rs` module exposing the `FrameType` enum (9 speech
  modes — MR660 6.60 kbit/s through MR2385 23.85 kbit/s — plus SID
  (FT=9) and NO_DATA (FT=15)), the per-mode storage-format byte-count
  table (1, 6, 17, 23, 32, 36, 40, 46, 50, 58, 60 bytes per packet
  per RFC 4867 §5.3), TOC-byte parsing + builder (`from_toc` /
  `make_toc`), per-mode bitrate lookup, and the
  `FRAMES_PER_PACKET = 320` constant (20 ms @ 16 kHz analysis window).
- `src/amr_wb_decoder.rs` — `AmrWbAtDecoder` implementing
  `oxideav_core::Decoder`. Validates each incoming packet against its
  TOC-derived size table before queueing, returning `Error::invalid`
  for reserved frame types (FT 10..=14 for AMR-WB — one fewer than
  AMR-NB because AMR-WB has nine speech modes) and for size
  mismatches. Persistent input-packet queue with one-packet-of-slack
  lookahead matching the iLBC / AMR-NB / HE-AAC pattern so AT never
  sees "0 packets" mid-stream. `flush()` drains the trailing PCM
  held back by the slack policy.
- New `K_AUDIO_FORMAT_AMR_WB = 'sawb'` constant + matching
  `AudioStreamBasicDescription::amr_wb()` constructor in `sys.rs`
  (16 kHz mono, `bytes_per_packet = 0` for variable-rate input,
  `frames_per_packet = 320` for the 20 ms AMR-WB analysis window).
- Integration test `tests/amr_wb_decode.rs`:
  - `amr_wb_decoder_accepts_all_frame_types` — feeds one
    syntactically-valid packet of every defined frame type
    (NO_DATA, SID, MR660..MR2385) and verifies PCM emerges
    (counted both mid-stream and after `flush`, since AT's
    AMR-WB decoder may vend PCM eagerly rather than holding all
    of it back for the slack-tail drain like AMR-NB does).
  - `amr_wb_decoder_handles_long_no_data_run` — drives 20
    consecutive NO_DATA packets to exercise the 1-byte
    input-descriptor path.
  - `amr_wb_decoder_rejects_size_mismatch` — pins the size-check
    surface area (e.g. MR2385 = 60 bytes; 59-byte packet refused).
  - `amr_wb_decoder_rejects_reserved_frame_type` — pins the FT
    validation surface area (FT 13 → `Error::invalid`).
  - `amr_wb_decoder_reset_clears_state` — verifies `reset()`
    re-arms the decoder for new packets after a previous run.
- New unit tests for `FrameType::from_toc` + `bytes_per_packet`
  + `bit_rate` + `make_toc` round-trip + FT-index uniqueness, the
  AMR-WB ASBD geometry, the `'sawb'` FourCC byte mapping, factory
  sample-rate / channel-count rejection, factory acceptance of
  every defined frame type, and registry presence (decoder only —
  encoder must NOT be registered since AT is decode-only).

- **Round 7: AMR-NB (Adaptive Multi-Rate Narrowband, 3GPP TS 26.071 /
  RFC 4867) decode** via `AudioConverterRef`. AudioToolbox exposes
  `kAudioFormatAMR` (`'samr'`) as a **decode-only** target — there is
  no paired AT encoder, so the registry installs only a decoder
  factory under codec id `"amr_nb"` with
  `implementation = "amr_nb_audiotoolbox"`, `priority = 10`,
  `hardware_accelerated = true`, `lossy = true`. Tags claimed:
  `fourcc(b"samr")` (3GPP / ISOBMFF) and `matroska(A_AMR/NB)`.
- New `src/amr.rs` module exposing the `FrameType` enum (8 speech
  modes — MR475 4.75 kbit/s through MR122 12.2 kbit/s — plus SID
  and NO_DATA), the per-mode storage-format byte-count table
  (1, 6, 13, 14, 16, 18, 20, 21, 27, 32 bytes per packet per RFC
  4867 §5.1), TOC-byte parsing + builder (`from_toc` / `make_toc`),
  per-mode bitrate lookup, and the `FRAMES_PER_PACKET = 160`
  constant (20 ms @ 8 kHz analysis window).
- `src/amr_decoder.rs` — `AmrNbAtDecoder` implementing
  `oxideav_core::Decoder`. Validates each incoming packet against
  its TOC-derived size table before queueing, returning
  `Error::invalid` for reserved frame types (FT 9..=14) and for
  size mismatches. Persistent input-packet queue with
  one-packet-of-slack lookahead (matching the iLBC / HE-AAC
  pattern) so AT never sees "0 packets" mid-stream. `flush()`
  drains the trailing PCM held back by the slack policy.
- New `K_AUDIO_FORMAT_AMR = 'samr'` constant + matching
  `AudioStreamBasicDescription::amr_nb()` constructor in `sys.rs`
  (8 kHz mono, `bytes_per_packet = 0` for variable-rate input,
  `frames_per_packet = 160` for the AMR analysis window).
- Integration test `tests/amr_nb_decode.rs`:
  - `amr_nb_decoder_accepts_all_frame_types` — feeds one
    syntactically-valid packet of every defined frame type
    (NO_DATA, SID, MR475..MR122) and verifies PCM emerges
    after `flush`.
  - `amr_nb_decoder_handles_long_no_data_run` — drives 20
    consecutive NO_DATA packets (the smallest variable-size
    packet path) to exercise the 1-byte input-descriptor path.
  - `amr_nb_decoder_rejects_size_mismatch` — pins the size-check
    surface area (e.g. MR122 = 32 bytes; 31-byte packet refused).
  - `amr_nb_decoder_rejects_reserved_frame_type` — pins the FT
    validation surface area (FT 12 → `Error::invalid`).
  - `amr_nb_decoder_reset_clears_state` — verifies `reset()`
    re-arms the decoder for new packets after a previous run.
- New unit tests for `FrameType::from_toc` + `bytes_per_packet`
  + `bit_rate` + `make_toc` round-trip, the AMR-NB ASBD geometry,
  the `'samr'` FourCC byte mapping, factory sample-rate /
  channel-count rejection, factory acceptance of every defined
  frame type, and registry presence (decoder only — encoder must
  NOT be registered since AT is decode-only).
- Empirical-finding note: AT's AMR-NB decoder vends PCM in
  **120-sample S16 mono blocks** (15 ms at 8 kHz) per
  `FillComplexBuffer` call rather than the 160-sample analysis-
  frame size — an internal AT chunking detail. The test suite
  asserts only the S16 mono byte-count invariant
  (`af.samples × 2 == af.data[0].len()`) and ≥ 1 frame produced
  rather than a fixed per-frame sample count.

- **Round 6: iLBC (Internet Low Bitrate Codec, RFC 3951) decode + encode**
  via `AudioConverterRef`. Adds the `kAudioFormatiLBC` (`'ilbc'`) format ID
  with a matching `AudioStreamBasicDescription::ilbc()` constructor.
  Fixed 8 kHz mono. Two block sizes selected via
  `CodecParameters::options.insert("mode", ...)`:
  - `"20"` (also `"20ms"` / `"ms20"`) — 160 PCM frames per packet,
    38 compressed bytes, 15.2 kbit/s net.
  - `"30"` (also `"30ms"` / `"ms30"`, default) — 240 PCM frames per
    packet, 50 compressed bytes, 13.33 kbit/s net.
- New `src/ilbc.rs` module with `IlbcMode` enum (parser + frame /
  byte geometry / round-trip tag helpers).
- `src/ilbc_decoder.rs` — `IlbcAtDecoder` implementing
  `oxideav_core::Decoder`. Persistent input-packet queue with
  one-packet-of-slack lookahead (mirroring the HE-AAC decoder
  pattern) — AT's iLBC analysis filter draws samples from one or
  two prior compressed packets per emitted PCM block, so a
  callback that returned 0 mid-stream would put the converter into
  a permanent EOS state. `flush()` drains the trailing PCM that
  the slack policy held back.
- `src/ilbc_encoder.rs` — `IlbcAtEncoder` implementing
  `oxideav_core::Encoder`. Internal staging buffer accumulates
  S16 PCM until a full `mode.frames_per_packet()` chunk is
  available, then emits one fixed-size iLBC packet per drain step.
  F32 PCM input is auto-converted to S16 via a length-vs-samples
  heuristic. `flush()` zero-pads any partial trailing PCM up to a
  full packet (standard CBR speech-codec EOS convention).
  Encoder publishes the active mode through
  `output_params.options["mode"]` and the net bitrate through
  `output_params.bit_rate` so downstream muxers can stamp it.
- `register()` now also installs iLBC decoder + encoder factories
  under codec id `"ilbc"` with `implementation = "ilbc_audiotoolbox"`,
  `priority = 10`, `hardware_accelerated = true`, `lossy = true`.
  Tags claimed: `fourcc(b"ilbc")`, `matroska(A_REAL/iLBC)`.
- Integration test `tests/ilbc_roundtrip.rs`:
  - `ilbc_30ms_roundtrip` — 2-second 1 kHz sine at 8 kHz mono via
    30 ms mode, peak SNR ≈ 10.7 dB (≥ 6 dB floor).
  - `ilbc_20ms_roundtrip` — same signal via 20 ms mode, peak SNR
    ≈ 7.8 dB.
  - `ilbc_packets_have_nonzero_payload` — ≥ 4 nonzero 50-byte
    packets within a 16k-frame feed.
- New unit tests for `IlbcMode` parse + geometry + round-trip
  tag, the iLBC ASBD geometry for both modes, the `'ilbc'`
  FourCC byte mapping, sample-rate / channel-count rejection,
  factory mode selection, and bitrate publishing.

### Fixed

- **AAC encoder: `output_params.bit_rate` now reports AT's actual
  delivered rate**, not the requested rate. AudioConverter quantises
  the requested bitrate onto a per-profile / per-sample-rate grid
  (e.g. AAC LC @ 48 kHz stereo accepts a limited set of values; off-grid
  requests are silently rounded to the nearest supported point). The
  encoder now queries `kAudioConverterEncodeBitRate` back after
  `SetProperty` and publishes the post-quantisation value through
  `output_params.bit_rate`. Falls back to the requested value if the
  get-property query is unavailable (older macOS, exotic ASBD). Two new
  regression tests pin the round-trip invariant — `bit_rate` published
  through `output_params` is non-zero, equals the request for the
  canonical 128 kbit/s LC operating point, and lands inside the
  plausible 32k-260k band for off-grid requests.

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
