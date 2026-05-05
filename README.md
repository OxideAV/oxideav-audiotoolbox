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

| Codec  | Decode  | Encode  | HW-accelerated |
|--------|---------|---------|----------------|
| AAC LC | yes     | yes     | yes (Apple Silicon / hardware audio codec engine) |

Round 2 SNR measurement: encode → decode 440 Hz sine at 48 kHz / stereo / 128 kbit/s → **36.7 dB** per channel (well above 25 dB threshold).

## Opt-out

Disable hardware acceleration globally via `CodecPreferences { no_hardware: true, .. }` or the `oxideav` CLI's `--no-hwaccel` flag. The runtime context still registers AT — `oxideav list` shows the `aac_audiotoolbox` row regardless of the flag — only resolution is biased toward the SW path.

## Feature flags

| Feature    | Default | Description                                    |
|------------|---------|------------------------------------------------|
| `registry` | on      | Wires in `oxideav-core` Decoder/Encoder traits and registers factories into the runtime codec registry. Turn off (`default-features = false`) to use only the raw AudioToolbox bridge bindings without the oxideav framework dependency. |

## Coverage roadmap

| Codec        | Decode              | Encode               |
|--------------|---------------------|----------------------|
| AAC LC       | done (round 2)      | done (round 2)       |
| AAC HE       | hardware            | hardware             |
| ALAC         | hardware (lossless) | hardware             |
| AMR-NB / WB  | hardware            | —                    |
| iLBC         | hardware            | —                    |

## Workspace policy

Calling a system OS framework via FFI is the same shape as calling `libc::malloc` — it's the platform, not a copied algorithm. The workspace's clean-room rule does not apply to this crate.

## License

MIT.
