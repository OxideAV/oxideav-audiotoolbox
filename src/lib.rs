#![cfg(target_os = "macos")]
//! macOS AudioToolbox hardware decode/encode bridge.
//!
//! This crate is a **runtime-loaded** bridge to Apple's
//! [AudioToolbox](https://developer.apple.com/documentation/audiotoolbox)
//! framework. It uses [`libloading`] to `dlopen` the framework on
//! first use, so:
//!
//! * macOS builds have **no compile-time link dependency** on
//!   AudioToolbox; if the framework can't be loaded, the registered
//!   factories return `Error::Unsupported` and the framework registry
//!   falls back to the pure-Rust codec implementation.
//! * No Objective-C / Swift involved. AudioToolbox is a C API.
//!
//! The crate is gated to `cfg(target_os = "macos")` at the source
//! level: on Linux / Windows the entire crate compiles to an empty
//! rlib.
//!
//! # Coverage
//!
//! | Codec  | Decode  | Encode  | HW-accelerated |
//! |--------|---------|---------|----------------|
//! | AAC LC | yes     | yes     | yes (Apple Silicon hardware path) |
//!
//! # Workspace policy
//!
//! Calling a system OS framework via FFI is the same shape as calling
//! `libc::malloc` — it's the platform, not a copied algorithm. The
//! workspace's clean-room rule does not apply.

pub mod adts;
pub mod sys;

#[cfg(feature = "registry")]
pub mod decoder;
#[cfg(feature = "registry")]
pub mod encoder;

#[cfg(feature = "registry")]
use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, CodecTag};

/// Register AAC LC decoder + encoder factories into the supplied
/// [`RuntimeContext`](oxideav_core::RuntimeContext).
///
/// If AudioToolbox is unavailable at runtime (dlopen fails) the function
/// logs a message to stderr and returns without registering — the registry
/// will fall back to the pure-Rust codec implementation for "aac".
///
/// Hardware-accelerated factories are registered with `priority = 10`
/// (lower than the pure-Rust default of 100), so on macOS the AT path
/// is preferred automatically.
#[cfg(feature = "registry")]
pub fn register(ctx: &mut oxideav_core::RuntimeContext) {
    // Load the framework early — if it fails, no-op.
    if let Err(e) = sys::framework() {
        eprintln!("oxideav-audiotoolbox: AudioToolbox unavailable ({e}); skipping registration");
        return;
    }

    let cid = CodecId::new("aac");

    // Decoder registration.
    let dec_caps = CodecCapabilities::audio("aac_audiotoolbox")
        .with_lossy(true)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(8)
        .with_max_sample_rate(96_000);

    ctx.codecs.register(
        CodecInfo::new(cid.clone())
            .capabilities(dec_caps)
            .decoder(decoder::make_decoder)
            .tags([
                CodecTag::wave_format(0x00FF),
                CodecTag::wave_format(0x706D),
                CodecTag::wave_format(0x4143),
                CodecTag::wave_format(0xA106),
                CodecTag::mp4_object_type(0x40),
                CodecTag::matroska("A_AAC"),
            ]),
    );

    // Encoder registration.
    let enc_caps = CodecCapabilities::audio("aac_audiotoolbox")
        .with_lossy(true)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(8)
        .with_max_sample_rate(96_000);

    ctx.codecs.register(
        CodecInfo::new(cid)
            .capabilities(enc_caps)
            .encoder(encoder::make_encoder),
    );
}

#[cfg(feature = "registry")]
oxideav_core::register!("audiotoolbox", register);

#[cfg(test)]
mod register_tests {
    use super::*;
    use oxideav_core::{CodecId, RuntimeContext};

    #[test]
    fn register_installs_factories() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        let id = CodecId::new("aac");
        assert!(
            ctx.codecs.has_decoder(&id),
            "AAC decoder not registered after register()"
        );
        assert!(
            ctx.codecs.has_encoder(&id),
            "AAC encoder not registered after register()"
        );
    }
}
