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
//! # Status
//!
//! Round 1 (this commit): scaffolding + libloading-based framework
//! loader + symbol-resolution smoke tests. No codecs wired yet.
//!
//! # Workspace policy
//!
//! Calling a system OS framework via FFI is the same shape as calling
//! `libc::malloc` — it's the platform, not a copied algorithm. The
//! workspace's clean-room rule does not apply.

pub mod sys;

/// Stable module path for the registry entry point. Round 2 will wire
/// in `AudioConverterRef`-based factories for AAC (decode + the
/// hardware encoder on Apple Silicon's audio codec engine), ALAC,
/// AMR-NB/WB, and the iLBC variants — all with priority 0
/// (preferred over the pure-Rust impls at priority 100+).
#[cfg(feature = "registry")]
pub fn register(_ctx: &mut oxideav_core::RuntimeContext) {
    // Round 1: framework loads but no factories registered yet.
    let _ = sys::framework();
}
