# oxideav-audiotoolbox

macOS AudioToolbox hardware decode/encode bridge for the [oxideav](https://github.com/OxideAV/oxideav) framework.

## Why a bridge crate?

Apple's [AudioToolbox](https://developer.apple.com/documentation/audiotoolbox) exposes the dedicated audio codec engine on Apple Silicon (and equivalent paths on Intel Macs). For AAC encode/decode this is the canonical "hardware AAC" path historically credited with iPhone-quality encodes at very low CPU cost.

This crate is a **thin runtime-loaded bridge** — no compile-time link dependency on AudioToolbox, no Objective-C / Swift. The framework is opened via [`libloading`] on first use; if the load fails, registered factories return `Error::Unsupported` and the framework registry falls back to the pure-Rust codec.

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

Disable hardware acceleration globally via `CodecPreferences { no_hardware: true }` or the `oxideav` CLI's `--no-hwaccel` flag to force the pure-Rust fallback.

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
