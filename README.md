# oxideav-audiotoolbox

[![CI](https://github.com/OxideAV/oxideav-audiotoolbox/actions/workflows/ci.yml/badge.svg)](https://github.com/OxideAV/oxideav-audiotoolbox/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/oxideav-audiotoolbox.svg)](https://crates.io/crates/oxideav-audiotoolbox) [![docs.rs](https://docs.rs/oxideav-audiotoolbox/badge.svg)](https://docs.rs/oxideav-audiotoolbox) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

macOS AudioToolbox hardware decode/encode bridge for the [oxideav](https://github.com/OxideAV/oxideav) framework.

## Why a bridge crate?

Apple's [AudioToolbox](https://developer.apple.com/documentation/audiotoolbox) exposes the dedicated audio codec engine on Apple Silicon (and equivalent paths on Intel Macs). For AAC encode/decode this is the canonical "hardware AAC" path historically credited with iPhone-quality encodes at very low CPU cost.

This crate is a **thin runtime-loaded bridge** — no compile-time link dependency on AudioToolbox, no Objective-C / Swift. The framework is opened via [`libloading`] on first use.

## Fallback behaviour

Three distinct failure paths fall back automatically to the pure-Rust codec:

1. **Load failure** — older macOS, missing framework, sandboxed environment without AT entitlements. `register()` logs and returns without registering, so the SW codec is the only candidate at dispatch.
2. **Inventory miss** — `register()` snapshots the OS's own decode/encode format inventory (`inventory::OsInventory`) and registers only the halves the running system actually backs, so the registry never claims a codec slot this macOS lacks. (The probe is optimistic on failure; current systems back every wired slot.)
3. **Init failure** — `AudioConverterNew` returns a non-zero `OSStatus` for the requested ASBD. Common triggers: unsupported sample rate / channel layout, encoder bitrate the device doesn't accelerate, hardware codec slot busy (concurrent-converter cap on iOS-class hardware). The factory returns a **typed** error — format rejections map to `Error::Unsupported`, so the registry retries the next-priority impl (typically the SW one) instead of aborting.

## Error taxonomy

Every CoreAudio `OSStatus` failure is classified through `status::AtStatus`, which covers all 29 documented codes from the platform SDK's `AudioConverter.h` / `AudioCodec.h` / `AudioFormat.h` / `CoreAudioBaseTypes.h` error enums. Failures render as `kAudioCodecBadDataError ('bada' / 1650549857)` — platform constant name, FourCC, and greppable integer — and map onto the semantically-correct `oxideav_core::Error` variant (`Unsupported` / `InvalidData` / `ResourceExhausted` / `Other`) via `status::status_error`. The feature-independent `status::AtError` type gives `default-features = false` consumers the same typed surface.

## Bridge introspection surface

Beyond the per-codec factories, three feature-independent modules expose the platform surface directly:

* **`converter`** — a safe RAII `Converter` owning the AudioConverter lifecycle (create / property get-set / reset / dispose-on-drop) with a typed property surface: magic cookies in both directions, `max_output_packet_size`, encode bit-rate set/get, the current output stream description, `prime_info` (the AAC encoder-delay figure), and applicable/available encode bit-rate + sample-rate grids decoded into `AudioValueRange`s.
* **`inventory`** — global, converter-less AudioFormat queries: the OS decode/encode format-ID sets (50 / 16 entries on current macOS, classified through `AudioFormatId`), per-format encoder bit-rate / sample-rate grids, and the `format_is_vbr` / `format_is_externally_framed` packetisation semantics that decide which transports must carry `AudioStreamPacketDescription`s.
* **`sys`** — the raw FFI layer: runtime symbol resolution, `AudioStreamBasicDescription` constructors for every wired slot plus a pure `validate()` consistency check (all 15 wired compressed constructors both self-validate and are accepted by `AudioConverterNew` on real hardware), the typed `AudioFormatId` classifier, and the converter/format property-ID constant set (each FourCC pinned byte-for-byte by tests; `kAudioFormatProperty_DecodeFormatIDs` verified against the live framework as `'acif'`).

The AAC encoder additionally publishes its edge-priming figures through `output_params().options["priming_frames"]` / `["trailing_frames"]` — the encoder-delay numbers muxers record (MP4 edit lists, gapless metadata) so players can trim the analysis warm-up.

Pipelines that require hardware can opt out of the SW fallback by setting `CodecPreferences { require_hardware: true, .. }` — the registry will then surface the `OSStatus` error instead of degrading silently.

## Platform gating

The whole crate is `#![cfg(target_os = "macos")]`. On Linux / Windows it compiles to an empty rlib; the umbrella `oxideav` crate gates the `register` call behind the same cfg.

## Priority

Hardware factories register with `priority = 10` — **lower numbers win at resolution time**, so on macOS the AudioToolbox path is preferred over the pure-Rust implementation (default priority 100).

## Coverage

| Codec     | Decode  | Encode  | HW-accelerated |
|-----------|---------|---------|----------------|
| AAC LC    | yes     | yes     | yes (Apple Silicon / hardware audio codec engine) |
| HE-AAC v1 | yes     | yes     | yes (LC + SBR, 2× frame upsample)                  |
| HE-AAC v2 | yes     | yes     | yes (LC + SBR + Parametric Stereo, stereo only)    |
| AAC-LD    | yes     | yes     | yes (low-delay AOT 23, 512-frame core, ~20 ms)    |
| AAC-ELD   | yes     | yes     | yes (enhanced low-delay AOT 39, 512-frame core)   |
| ALAC      | yes     | yes     | yes (lossless, S16 / S32 PCM)                     |
| iLBC      | yes     | yes     | yes (RFC 3951 narrow-band speech, 8 kHz mono, 20 ms + 30 ms modes) |
| AMR-NB    | yes     | n/a     | yes (3GPP TS 26.071 narrow-band speech, 8 kHz mono, 8 speech modes + SID + NO_DATA; AT is decode-only) |
| AMR-WB    | yes     | n/a     | yes (3GPP TS 26.171 / 26.201 wideband speech, 16 kHz mono, 9 speech modes + SID + NO_DATA; AT is decode-only) |
| MP3       | yes     | n/a     | yes (MPEG-1 / 2 / 2.5 Audio Layer III, mono + stereo, 8 / 11.025 / 12 / 16 / 22.05 / 24 / 32 / 44.1 / 48 kHz; AT is decode-only — ships no MPEG-audio encoder) |
| MP2       | yes     | n/a     | yes (MPEG Audio Layer II, 1152 samples/frame, shared MPEG-audio bridge; decode-only) |
| MP1       | yes     | n/a     | yes (MPEG-1 Audio Layer I, 384 samples/frame, shared MPEG-audio bridge; decode-only) |
| FLAC      | yes     | yes     | yes (RFC 9639, up to 8 channels, 8 / 16 / 20 / 24 / 32-bit, sample rates up to 192 kHz, fixed + variable blocksize; encoder ships S16 / S32 PCM input, AT-side compressed-depth cap at 24-bit) |
| Opus      | yes     | yes     | yes (RFC 6716 / RFC 7845, 1–2 channels mapping family 0, output rates 8 / 12 / 16 / 24 / 48 kHz, frame durations 2.5 / 5 / 10 / 20 / 40 / 60 ms via `options["frame_duration_ms"]`, default 20 ms) |

All codecs are validated end-to-end by encode→decode (or decode-only,
for the decode-only formats) round-trip tests on synthetic tones:
lossless formats (ALAC, FLAC) are asserted bit-exact, and the lossy /
speech formats are asserted above their per-codec SNR floor (a pure
sine is intentionally adversarial for the CELP-class voice codecs, so
those floors verify the pipeline is wired correctly rather than
claiming transparency). The decode-only formats (AMR-NB, AMR-WB, MP3)
have no paired AT encoder, so the registry installs only their decoder
factories. A typed `AudioFormatId` enum classifies any ASBD's raw
`format_id` FourCC into the wired codec set, exposing the format
family (`is_aac_family`, `is_lossless`, `is_compressed_audio`), the
canonical oxideav codec-id string each slot registers under
(`codec_id_str`), and a printable FourCC for diagnostics
(`fourcc_str`). The decode-only AMR-NB / AMR-WB / MP3 asymmetry is
pinned against the OS's own inventory by test: none of the three
appear in the system encode set, and registration mirrors the
inventory exactly in both directions.

## Opt-out

Disable hardware acceleration globally via `CodecPreferences { no_hardware: true, .. }` or the `oxideav` CLI's `--no-hwaccel` flag. The runtime context still registers AT — `oxideav list` shows the `aac_audiotoolbox` row regardless of the flag — only resolution is biased toward the SW path.

## Feature flags

| Feature    | Default | Description                                    |
|------------|---------|------------------------------------------------|
| `registry` | on      | Wires in `oxideav-core` Decoder/Encoder traits and registers factories into the runtime codec registry. Turn off (`default-features = false`) to use only the raw AudioToolbox bridge bindings without the oxideav framework dependency. |

## Workspace policy

Calling a system OS framework via FFI is the same shape as calling `libc::malloc` — it's the platform, not a copied algorithm. The workspace's clean-room rule does not apply to this crate.

## License

MIT.
