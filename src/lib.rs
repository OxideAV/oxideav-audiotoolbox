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
//! | Codec     | Decode  | Encode  | HW-accelerated |
//! |-----------|---------|---------|----------------|
//! | AAC LC    | yes     | yes     | yes (Apple Silicon hardware path)      |
//! | HE-AAC v1 | yes     | yes     | yes (LC + SBR, 2× upsample)            |
//! | HE-AAC v2 | yes     | yes     | yes (LC + SBR + Parametric Stereo)     |
//! | AAC-LD    | yes     | yes     | yes (low-delay AOT 23, 512-frame core) |
//! | AAC-ELD   | yes     | yes     | yes (enhanced low-delay AOT 39)        |
//! | ALAC      | yes     | yes     | yes (lossless, S16 / S32 PCM)          |
//! | iLBC      | yes     | yes     | yes (8 kHz mono, 20 ms + 30 ms modes)  |
//! | AMR-NB    | yes     | n/a     | yes (8 kHz mono, 8 speech modes + SID, decode-only) |
//! | AMR-WB    | yes     | n/a     | yes (16 kHz mono, 9 speech modes + SID, decode-only) |
//! | MP3       | yes     | n/a     | yes (MPEG-1/2/2.5 Audio Layer III, decode-only) |
//!
//! # Workspace policy
//!
//! Calling a system OS framework via FFI is the same shape as calling
//! `libc::malloc` — it's the platform, not a copied algorithm. The
//! workspace's clean-room rule does not apply.

pub mod adts;
pub mod alac;
pub mod amr;
pub mod amr_wb;
pub mod ilbc;
pub mod mp3;
pub mod sys;

#[cfg(feature = "registry")]
pub mod alac_decoder;
#[cfg(feature = "registry")]
pub mod alac_encoder;
#[cfg(feature = "registry")]
pub mod amr_decoder;
#[cfg(feature = "registry")]
pub mod amr_wb_decoder;
#[cfg(feature = "registry")]
pub mod decoder;
#[cfg(feature = "registry")]
pub mod encoder;
#[cfg(feature = "registry")]
pub mod ilbc_decoder;
#[cfg(feature = "registry")]
pub mod ilbc_encoder;
#[cfg(feature = "registry")]
pub mod mp3_decoder;

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

    register_alac(ctx);
    register_ilbc(ctx);
    register_amr_nb(ctx);
    register_amr_wb(ctx);
    register_mp3(ctx);
}

/// Register Apple Lossless (ALAC) decoder + encoder factories.
///
/// Tags claimed:
///
/// * FourCC `'alac'` — used by MOV / MP4 / CAF sample-entry tables.
/// * Matroska `A_ALAC` — Matroska's CodecID for ALAC tracks.
///
/// ALAC is lossless so `with_lossy(false)`. Priority matches AAC at 10
/// so the HW path wins over any future pure-Rust ALAC implementation.
#[cfg(feature = "registry")]
fn register_alac(ctx: &mut oxideav_core::RuntimeContext) {
    let cid = CodecId::new("alac");

    let dec_caps = CodecCapabilities::audio("alac_audiotoolbox")
        .with_lossy(false)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(8)
        .with_max_sample_rate(384_000);

    ctx.codecs.register(
        CodecInfo::new(cid.clone())
            .capabilities(dec_caps)
            .decoder(alac_decoder::make_decoder)
            .tags([CodecTag::fourcc(b"alac"), CodecTag::matroska("A_ALAC")]),
    );

    let enc_caps = CodecCapabilities::audio("alac_audiotoolbox")
        .with_lossy(false)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(8)
        .with_max_sample_rate(384_000);

    ctx.codecs.register(
        CodecInfo::new(cid)
            .capabilities(enc_caps)
            .encoder(alac_encoder::make_encoder),
    );
}

/// Register iLBC (Internet Low Bitrate Codec) decoder + encoder
/// factories. Fixed 8 kHz mono; the 20 ms vs 30 ms mode is carried in
/// `CodecParameters::options["mode"]` (defaults to 30 ms).
///
/// Tags claimed:
///
/// * FourCC `'ilbc'` — AT's identifier; also used by some MOV / 3GPP
///   sample-entry tables.
/// * Matroska `A_REAL/iLBC` — Matroska's CodecID for iLBC tracks.
#[cfg(feature = "registry")]
fn register_ilbc(ctx: &mut oxideav_core::RuntimeContext) {
    let cid = CodecId::new("ilbc");

    let dec_caps = CodecCapabilities::audio("ilbc_audiotoolbox")
        .with_lossy(true)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(1)
        .with_max_sample_rate(8_000);

    ctx.codecs.register(
        CodecInfo::new(cid.clone())
            .capabilities(dec_caps)
            .decoder(ilbc_decoder::make_decoder)
            .tags([CodecTag::fourcc(b"ilbc"), CodecTag::matroska("A_REAL/iLBC")]),
    );

    let enc_caps = CodecCapabilities::audio("ilbc_audiotoolbox")
        .with_lossy(true)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(1)
        .with_max_sample_rate(8_000);

    ctx.codecs.register(
        CodecInfo::new(cid)
            .capabilities(enc_caps)
            .encoder(ilbc_encoder::make_encoder),
    );
}

/// Register AMR-NB (Adaptive Multi-Rate Narrowband) **decoder** factory.
///
/// AudioToolbox exposes `kAudioFormatAMR` (`'samr'`) as a decompression
/// target but does not ship a paired encoder, so this registration is
/// decode-only. Tags claimed:
///
/// * FourCC `'samr'` — the canonical 3GPP / ISOBMFF identifier; used by
///   MOV / MP4 / 3GP sample-entry tables (`SampleEntry.format = 'samr'`).
/// * Matroska `A_AMR/NB` — Matroska's CodecID for AMR-NB tracks.
#[cfg(feature = "registry")]
fn register_amr_nb(ctx: &mut oxideav_core::RuntimeContext) {
    let cid = CodecId::new("amr_nb");

    let dec_caps = CodecCapabilities::audio("amr_nb_audiotoolbox")
        .with_lossy(true)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(1)
        .with_max_sample_rate(8_000);

    ctx.codecs.register(
        CodecInfo::new(cid)
            .capabilities(dec_caps)
            .decoder(amr_decoder::make_decoder)
            .tags([CodecTag::fourcc(b"samr"), CodecTag::matroska("A_AMR/NB")]),
    );
}

/// Register AMR-WB (Adaptive Multi-Rate Wideband) **decoder** factory.
///
/// AudioToolbox exposes `kAudioFormatAMR_WB` (`'sawb'`) as a
/// decompression target but does not ship a paired encoder, so this
/// registration is decode-only (mirroring the AMR-NB asymmetry). Tags
/// claimed:
///
/// * FourCC `'sawb'` — the canonical 3GPP / ISOBMFF identifier; used
///   by MOV / MP4 / 3GP sample-entry tables (`SampleEntry.format = 'sawb'`).
/// * Matroska `A_AMR/WB` — Matroska's CodecID for AMR-WB tracks.
#[cfg(feature = "registry")]
fn register_amr_wb(ctx: &mut oxideav_core::RuntimeContext) {
    let cid = CodecId::new("amr_wb");

    let dec_caps = CodecCapabilities::audio("amr_wb_audiotoolbox")
        .with_lossy(true)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(1)
        .with_max_sample_rate(16_000);

    ctx.codecs.register(
        CodecInfo::new(cid)
            .capabilities(dec_caps)
            .decoder(amr_wb_decoder::make_decoder)
            .tags([CodecTag::fourcc(b"sawb"), CodecTag::matroska("A_AMR/WB")]),
    );
}

/// Register MPEG-1/2/2.5 Audio Layer I/II/III (MP1 / MP2 / MP3)
/// **decoder** factory.
///
/// AudioToolbox exposes the MPEG audio layers (`kAudioFormatMPEGLayer1`
/// / `kAudioFormatMPEGLayer2` / `kAudioFormatMPEGLayer3`,
/// FourCC `'.mp1'` / `'.mp2'` / `'.mp3'`) as decompression targets but
/// does not ship a paired encoder for any of them, so this
/// registration is decode-only. The decoder factory accepts any of the
/// three layers — it sniffs the first frame's header and configures
/// the converter for the appropriate format ID and PCM block size
/// (1152 for MPEG-1 Layer II/III, 576 for MPEG-2 LSF Layer III, 384
/// for Layer I).
///
/// Codec id `"mp3"` is the primary registration (MP3 is by far the
/// dominant consumer; Layer I/II content is rare outside DAB / VCD).
/// Tags claimed:
///
/// * FourCC `'.mp3'` — Apple's MP3 identifier; also matches some MOV
///   `mp4a`-aliasing sample-entry tables.
/// * `mp4_object_type(0x6B)` — ISO/IEC 14496-1 ObjectTypeIndication
///   for MPEG-1 / MPEG-2 audio (used by some MP4 / 3GPP files that
///   wrap MP3 in an `mp4a` sample entry with this OTI).
/// * Matroska `A_MPEG/L3` — Matroska's CodecID for MP3 tracks.
/// * wFormatTag `0x0055` — Microsoft's RIFF/WAVE tag for MP3 (used
///   by WAV containers carrying MP3 payload).
#[cfg(feature = "registry")]
fn register_mp3(ctx: &mut oxideav_core::RuntimeContext) {
    let cid = CodecId::new("mp3");

    let dec_caps = CodecCapabilities::audio("mp3_audiotoolbox")
        .with_lossy(true)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(2)
        .with_max_sample_rate(48_000);

    ctx.codecs.register(
        CodecInfo::new(cid)
            .capabilities(dec_caps)
            .decoder(mp3_decoder::make_decoder)
            .tags([
                CodecTag::fourcc(b".mp3"),
                CodecTag::mp4_object_type(0x6B),
                CodecTag::matroska("A_MPEG/L3"),
                CodecTag::wave_format(0x0055),
            ]),
    );
}

#[cfg(feature = "registry")]
oxideav_core::register!("audiotoolbox", register);

// `register_tests` exercises the `register()` entry point, which only
// exists under the `registry` feature. Without this gate, a macOS
// `--no-default-features` test build fails to compile (the symbols are
// absent). CI's standalone job runs on Linux where the whole
// `#![cfg(target_os = "macos")]` crate compiles away, so it never hit
// this — but a local macOS standalone `cargo test --lib` did.
#[cfg(all(test, feature = "registry"))]
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

    #[test]
    fn register_installs_alac_factories() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        let id = CodecId::new("alac");
        assert!(
            ctx.codecs.has_decoder(&id),
            "ALAC decoder not registered after register()"
        );
        assert!(
            ctx.codecs.has_encoder(&id),
            "ALAC encoder not registered after register()"
        );
    }

    #[test]
    fn register_installs_ilbc_factories() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        let id = CodecId::new("ilbc");
        assert!(
            ctx.codecs.has_decoder(&id),
            "iLBC decoder not registered after register()"
        );
        assert!(
            ctx.codecs.has_encoder(&id),
            "iLBC encoder not registered after register()"
        );
    }

    #[test]
    fn register_installs_amr_nb_decoder_only() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        let id = CodecId::new("amr_nb");
        assert!(
            ctx.codecs.has_decoder(&id),
            "AMR-NB decoder not registered after register()"
        );
        // AT exposes AMR-NB decode only — encoder must NOT be present.
        assert!(
            !ctx.codecs.has_encoder(&id),
            "AMR-NB encoder must not be registered (AT is decode-only)"
        );
    }

    #[test]
    fn register_installs_mp3_decoder_only() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        let id = CodecId::new("mp3");
        assert!(
            ctx.codecs.has_decoder(&id),
            "MP3 decoder not registered after register()"
        );
        // AT exposes MP3 decode only — encoder must NOT be present.
        assert!(
            !ctx.codecs.has_encoder(&id),
            "MP3 encoder must not be registered (AT is decode-only)"
        );
    }

    #[test]
    fn register_installs_amr_wb_decoder_only() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        let id = CodecId::new("amr_wb");
        assert!(
            ctx.codecs.has_decoder(&id),
            "AMR-WB decoder not registered after register()"
        );
        // AT exposes AMR-WB decode only — encoder must NOT be present.
        assert!(
            !ctx.codecs.has_encoder(&id),
            "AMR-WB encoder must not be registered (AT is decode-only)"
        );
    }
}
