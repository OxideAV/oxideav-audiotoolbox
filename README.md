# oxideav-audiotoolbox

macOS AudioToolbox hardware decode/encode bridge for the [oxideav](https://github.com/OxideAV/oxideav) framework.

## Why a bridge crate?

Apple's [AudioToolbox](https://developer.apple.com/documentation/audiotoolbox) exposes the dedicated audio codec engine on Apple Silicon (and equivalent paths on Intel Macs). For AAC encode/decode this is the canonical "hardware AAC" path historically credited with iPhone-quality encodes at very low CPU cost.

This crate is a **thin runtime-loaded bridge** — no compile-time link dependency on AudioToolbox, no Objective-C / Swift. The framework is opened via [`libloading`] on first use.

## Fallback behaviour

Two distinct failure paths fall back automatically to the pure-Rust codec:

1. **Load failure** — older macOS, missing framework, sandboxed environment without AT entitlements. `register()` logs and returns without registering, so the SW codec is the only candidate at dispatch.
2. **Init failure** — `AudioConverterNew` returns a non-zero `OSStatus` for the requested ASBD. Common triggers: unsupported sample rate / channel layout, encoder bitrate the device doesn't accelerate, hardware codec slot busy (concurrent-converter cap on iOS-class hardware). The factory returns `Err`; the registry retries the next-priority impl (typically the SW one).

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
`format_id` FourCC into the wired codec set.

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
