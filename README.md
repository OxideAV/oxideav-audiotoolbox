# oxideav-audiotoolbox

macOS AudioToolbox hardware decode/encode bridge for the [oxideav](https://github.com/OxideAV/oxideav) framework.

## Why a bridge crate?

Apple's [AudioToolbox](https://developer.apple.com/documentation/audiotoolbox) exposes the dedicated audio codec engine on Apple Silicon (and equivalent paths on Intel Macs). For AAC encode/decode this is the canonical "hardware AAC" path historically credited with iPhone-quality encodes at very low CPU cost.

This crate is a **thin runtime-loaded bridge** — no compile-time link dependency on AudioToolbox, no Objective-C / Swift. The framework is opened via [`libloading`] on first use; if the load fails, registered factories return `Error::Unsupported` and the framework registry falls back to the pure-Rust codec.

## Platform gating

The whole crate is `#![cfg(target_os = "macos")]`. On Linux / Windows it compiles to an empty rlib; the umbrella `oxideav` crate gates the `register` call behind the same cfg.

## Priority

Hardware factories register with `CodecCapabilities::with_priority(0)` — **lower numbers win at resolution time**, so on macOS hardware paths are preferred over the pure-Rust impls.

## Opt-out

Users who want to force the pure-Rust path can disable hardware acceleration globally via the `oxideav` CLI's `--no-hwaccel` flag (Round 2 work — see issue tracker). The flag works by skipping `oxideav_audiotoolbox::register` (and `oxideav_videotoolbox::register`) when constructing the runtime context.

## Coverage roadmap

| Codec        | Decode             | Encode                       |
|--------------|--------------------|------------------------------|
| AAC LC       | hardware           | hardware (Apple AAC encoder) |
| AAC HE       | hardware           | hardware                     |
| ALAC         | hardware (lossless)| hardware                     |
| AMR-NB / WB  | hardware           | —                            |
| iLBC         | hardware           | —                            |
| FLAC         | software (in AT)   | software                     |
| MP3          | software (in AT)   | —                            |

Round 1 (this commit): scaffolding only. Round 2: AAC LC decode + encode. Round 3: ALAC + AAC HE. Round 4: AMR / iLBC.

## Workspace policy

Calling a system OS framework via FFI is the same shape as calling `libc::malloc` — it's the platform, not a copied algorithm. The workspace's clean-room rule does not apply to this crate.

## License

MIT.
