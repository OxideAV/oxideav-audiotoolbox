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
//! | MP3       | yes     | n/a     | yes (Layer III, MPEG-1/2/2.5, decode-only) |
//! | MP2       | yes     | n/a     | yes (Layer II, 1152 samples/frame, decode-only) |
//! | MP1       | yes     | n/a     | yes (Layer I, 384 samples/frame, decode-only) |
//! | FLAC      | yes     | yes     | yes (RFC 9639, 4..=32-bit, up to 192 kHz, lossless dfLa cookie) |
//! | Opus      | yes     | yes     | yes (RFC 6716 / RFC 7845, 1–2 ch family-0, output 8/12/16/24/48 kHz, frame 2.5..60 ms) |
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
pub mod converter;
pub mod flac;
pub mod ilbc;
pub mod inventory;
pub mod mp3;
pub mod opus;
pub mod status;
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
pub mod flac_decoder;
#[cfg(feature = "registry")]
pub mod flac_encoder;
#[cfg(feature = "registry")]
pub mod ilbc_decoder;
#[cfg(feature = "registry")]
pub mod ilbc_encoder;
#[cfg(feature = "registry")]
pub mod mp3_decoder;
#[cfg(feature = "registry")]
pub mod opus_decoder;
#[cfg(feature = "registry")]
pub mod opus_encoder;

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

    // Snapshot the OS's own decode/encode inventory once and register
    // only the halves the running system actually backs, so the
    // registry never claims a codec slot this macOS lacks. The probe
    // is optimistic on failure (empty sets ⇒ register everything;
    // the per-factory error paths still guard at construction time).
    let inv = inventory::OsInventory::probe();

    let cid = CodecId::new("aac");

    // Decoder registration.
    let dec_caps = CodecCapabilities::audio("aac_audiotoolbox")
        .with_lossy(true)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(8)
        .with_max_sample_rate(96_000);

    if inv.decodes(sys::AudioFormatId::Mpeg4AacLc) {
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
    }

    // Encoder registration.
    let enc_caps = CodecCapabilities::audio("aac_audiotoolbox")
        .with_lossy(true)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(8)
        .with_max_sample_rate(96_000);

    if inv.encodes(sys::AudioFormatId::Mpeg4AacLc) {
        ctx.codecs.register(
            CodecInfo::new(cid)
                .capabilities(enc_caps)
                .encoder(encoder::make_encoder),
        );
    }

    register_alac(ctx, &inv);
    register_ilbc(ctx, &inv);
    register_amr_nb(ctx, &inv);
    register_amr_wb(ctx, &inv);
    register_mp1(ctx, &inv);
    register_mp2(ctx, &inv);
    register_mp3(ctx, &inv);
    register_flac(ctx, &inv);
    register_opus(ctx, &inv);
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
fn register_alac(ctx: &mut oxideav_core::RuntimeContext, inv: &inventory::OsInventory) {
    let cid = CodecId::new("alac");

    let dec_caps = CodecCapabilities::audio("alac_audiotoolbox")
        .with_lossy(false)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(8)
        .with_max_sample_rate(384_000);

    if inv.decodes(sys::AudioFormatId::AppleLossless) {
        ctx.codecs.register(
            CodecInfo::new(cid.clone())
                .capabilities(dec_caps)
                .decoder(alac_decoder::make_decoder)
                .tags([CodecTag::fourcc(b"alac"), CodecTag::matroska("A_ALAC")]),
        );
    }

    let enc_caps = CodecCapabilities::audio("alac_audiotoolbox")
        .with_lossy(false)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(8)
        .with_max_sample_rate(384_000);

    if inv.encodes(sys::AudioFormatId::AppleLossless) {
        ctx.codecs.register(
            CodecInfo::new(cid)
                .capabilities(enc_caps)
                .encoder(alac_encoder::make_encoder),
        );
    }
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
fn register_ilbc(ctx: &mut oxideav_core::RuntimeContext, inv: &inventory::OsInventory) {
    let cid = CodecId::new("ilbc");

    let dec_caps = CodecCapabilities::audio("ilbc_audiotoolbox")
        .with_lossy(true)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(1)
        .with_max_sample_rate(8_000);

    if inv.decodes(sys::AudioFormatId::Ilbc) {
        ctx.codecs.register(
            CodecInfo::new(cid.clone())
                .capabilities(dec_caps)
                .decoder(ilbc_decoder::make_decoder)
                .tags([CodecTag::fourcc(b"ilbc"), CodecTag::matroska("A_REAL/iLBC")]),
        );
    }

    let enc_caps = CodecCapabilities::audio("ilbc_audiotoolbox")
        .with_lossy(true)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(1)
        .with_max_sample_rate(8_000);

    if inv.encodes(sys::AudioFormatId::Ilbc) {
        ctx.codecs.register(
            CodecInfo::new(cid)
                .capabilities(enc_caps)
                .encoder(ilbc_encoder::make_encoder),
        );
    }
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
fn register_amr_nb(ctx: &mut oxideav_core::RuntimeContext, inv: &inventory::OsInventory) {
    let cid = CodecId::new("amr_nb");

    let dec_caps = CodecCapabilities::audio("amr_nb_audiotoolbox")
        .with_lossy(true)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(1)
        .with_max_sample_rate(8_000);

    if inv.decodes(sys::AudioFormatId::AmrNb) {
        ctx.codecs.register(
            CodecInfo::new(cid)
                .capabilities(dec_caps)
                .decoder(amr_decoder::make_decoder)
                .tags([CodecTag::fourcc(b"samr"), CodecTag::matroska("A_AMR/NB")]),
        );
    }
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
fn register_amr_wb(ctx: &mut oxideav_core::RuntimeContext, inv: &inventory::OsInventory) {
    let cid = CodecId::new("amr_wb");

    let dec_caps = CodecCapabilities::audio("amr_wb_audiotoolbox")
        .with_lossy(true)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(1)
        .with_max_sample_rate(16_000);

    if inv.decodes(sys::AudioFormatId::AmrWb) {
        ctx.codecs.register(
            CodecInfo::new(cid)
                .capabilities(dec_caps)
                .decoder(amr_wb_decoder::make_decoder)
                .tags([CodecTag::fourcc(b"sawb"), CodecTag::matroska("A_AMR/WB")]),
        );
    }
}

/// Register MP1 (MPEG-1 Audio Layer I) **decoder** factory.
///
/// AudioToolbox exposes `kAudioFormatMPEGLayer1` (`'.mp1'`) as a
/// decompression-only target — the registration is decode-only,
/// mirroring the Layer III asymmetry. The factory is the shared
/// MPEG-audio bridge (`mp3_decoder::make_decoder`), which derives the
/// expected layer from the codec id; a stream whose frames are not
/// Layer I is rejected with a typed `Unsupported` so resolution falls
/// through to the entry that owns the actual layer.
///
/// Tags claimed:
///
/// * FourCC `'.mp1'` — AudioToolbox's identifier.
/// * Matroska `A_MPEG/L1` — Matroska's CodecID for Layer I audio.
///
/// Layer I is fixed at 384 samples per frame across every MPEG
/// version.
#[cfg(feature = "registry")]
fn register_mp1(ctx: &mut oxideav_core::RuntimeContext, inv: &inventory::OsInventory) {
    let cid = CodecId::new("mp1");

    let dec_caps = CodecCapabilities::audio("mp1_audiotoolbox")
        .with_lossy(true)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(2)
        .with_max_sample_rate(48_000);

    if inv.decodes(sys::AudioFormatId::MpegLayer1) {
        ctx.codecs.register(
            CodecInfo::new(cid)
                .capabilities(dec_caps)
                .decoder(mp3_decoder::make_decoder)
                .tags([CodecTag::fourcc(b".mp1"), CodecTag::matroska("A_MPEG/L1")]),
        );
    }
}

/// Register MP2 (MPEG-1 / 2 / 2.5 Audio Layer II) **decoder** factory.
///
/// AudioToolbox exposes `kAudioFormatMPEGLayer2` (`'.mp2'`) as a
/// decompression-only target — decode-only registration through the
/// shared MPEG-audio bridge (`mp3_decoder::make_decoder`), which
/// derives the expected layer from the codec id.
///
/// Tags claimed:
///
/// * FourCC `'.mp2'` — AudioToolbox's identifier.
/// * Matroska `A_MPEG/L2` — Matroska's CodecID for Layer II audio.
/// * WAVE format tag `0x0050` — `WAVE_FORMAT_MPEG`, the RIFF / AVI /
///   WAV tag for MPEG-1 Layer I/II audio chunks (Layer II is the
///   overwhelmingly common payload under this tag).
///
/// Layer II is fixed at 1152 samples per frame across every MPEG
/// version.
#[cfg(feature = "registry")]
fn register_mp2(ctx: &mut oxideav_core::RuntimeContext, inv: &inventory::OsInventory) {
    let cid = CodecId::new("mp2");

    let dec_caps = CodecCapabilities::audio("mp2_audiotoolbox")
        .with_lossy(true)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(2)
        .with_max_sample_rate(48_000);

    if inv.decodes(sys::AudioFormatId::MpegLayer2) {
        ctx.codecs.register(
            CodecInfo::new(cid)
                .capabilities(dec_caps)
                .decoder(mp3_decoder::make_decoder)
                .tags([
                    CodecTag::fourcc(b".mp2"),
                    CodecTag::matroska("A_MPEG/L2"),
                    CodecTag::wave_format(0x0050),
                ]),
        );
    }
}

/// Register MP3 (MPEG-1 / 2 / 2.5 Audio Layer III) **decoder** factory.
///
/// AudioToolbox exposes `kAudioFormatMPEGLayer3` (`'.mp3'`) as a
/// decompression-only target — AT ships no MPEG-audio encoder — so
/// the registration is decode-only.
///
/// Tags claimed:
///
/// * FourCC `'.mp3'` — AudioToolbox's identifier; also matches what
///   ISO/IEC 14496-12 sample-entry tables use for MP3 audio tracks
///   carried in MP4 containers.
/// * `mp4_object_type(0x6B)` — ISO/IEC 14496-1 Object Type Indication
///   for MPEG-1 Audio (Layer 1/2/3), the value `esds` boxes in an MP4
///   carry for MP3 tracks.
/// * Matroska `A_MPEG/L3` — Matroska's CodecID for MP3 audio.
/// * WAVE format tag `0x0055` — Microsoft's `WAVE_FORMAT_MPEGLAYER3`,
///   the value AVI / RIFF / WAV containers use to flag MP3 chunks.
///
/// Capabilities: 1 or 2 channels, sample rates from 8 kHz (MPEG-2.5)
/// through 48 kHz (MPEG-1). The bridge resolves the actual
/// (version × layer × sample-rate × channel-mode) configuration
/// lazily from the first frame header — caller-supplied parameters
/// are advisory.
#[cfg(feature = "registry")]
fn register_mp3(ctx: &mut oxideav_core::RuntimeContext, inv: &inventory::OsInventory) {
    let cid = CodecId::new("mp3");

    let dec_caps = CodecCapabilities::audio("mp3_audiotoolbox")
        .with_lossy(true)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(2)
        .with_max_sample_rate(48_000);

    if inv.decodes(sys::AudioFormatId::MpegLayer3) {
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
}

/// Register FLAC (Free Lossless Audio Codec, RFC 9639) **decoder +
/// encoder** factories.
///
/// AudioToolbox exposes `kAudioFormatFLAC` (`'flac'`) as both a
/// decompression and a compression target on macOS 13+. This
/// registration installs both halves. Round 10 (decode) + round 218
/// (encode) together complete the symmetric FLAC bridge.
///
/// Tags claimed (decode side carries them):
///
/// * FourCC `'flac'` — AudioToolbox's identifier; matches what
///   ISO/IEC 14496-12 sample-entry tables use for FLAC tracks
///   carried in MOV / MP4 containers (`fLaC` box; sample-entry
///   `flac`).
/// * Matroska `A_FLAC` — Matroska's CodecID for FLAC audio tracks.
///
/// Capabilities: up to 8 channels, sample rates up to 192 kHz, bit
/// depths 4..=32 (mapped onto the four ALAC-style source-data flag
/// values per the public `CoreAudioBaseTypes.h` header). The decode
/// bridge resolves the actual `(sample_rate / channels /
/// bits_per_sample / max_blocksize)` configuration from the magic
/// cookie in `CodecParameters::extradata` (or synthesises a placeholder
/// cookie from the explicit parameters for standalone-test paths).
/// The encode bridge accepts S16 / S32 PCM and vends the resulting
/// `dfLa` magic cookie via `output_params.extradata` for downstream
/// muxer use.
#[cfg(feature = "registry")]
fn register_flac(ctx: &mut oxideav_core::RuntimeContext, inv: &inventory::OsInventory) {
    let cid = CodecId::new("flac");

    let dec_caps = CodecCapabilities::audio("flac_audiotoolbox")
        .with_lossy(false)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(8)
        .with_max_sample_rate(192_000);

    if inv.decodes(sys::AudioFormatId::Flac) {
        ctx.codecs.register(
            CodecInfo::new(cid.clone())
                .capabilities(dec_caps)
                .decoder(flac_decoder::make_decoder)
                .tags([CodecTag::fourcc(b"flac"), CodecTag::matroska("A_FLAC")]),
        );
    }

    let enc_caps = CodecCapabilities::audio("flac_audiotoolbox")
        .with_lossy(false)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(8)
        .with_max_sample_rate(192_000);

    if inv.encodes(sys::AudioFormatId::Flac) {
        ctx.codecs.register(
            CodecInfo::new(cid)
                .capabilities(enc_caps)
                .encoder(flac_encoder::make_encoder),
        );
    }
}

/// Register Opus (IETF RFC 6716 / RFC 7845 / RFC 8251) **decoder +
/// encoder** factories.
///
/// AudioToolbox exposes `kAudioFormatOpus` (`'opus'`) as both a
/// decompression and a compression target on the macOS releases that
/// ship the Opus codec slot. The bridge registers a 1-or-2-channel
/// mapping family 0 (RTP layout, RFC 7845 §5.1.1.1) decoder + encoder
/// pair; multi-channel mapping families (1, 255) require container-
/// layer mapping that is the muxer's responsibility.
///
/// Tags claimed:
///
/// * FourCC `'Opus'` — the four-character code used by ISO/IEC 14496-
///   12 sample-entry tables for Opus tracks in MP4 containers.
/// * Matroska `A_OPUS` — Matroska's CodecID for Opus audio.
///
/// Capabilities: 1–2 channels, output sample rates 8 / 12 / 16 / 24 /
/// 48 kHz per RFC 6716 §2.1.1 (the bridge defaults to 48 kHz —
/// the RFC 7845 §5.1 recommended player rate).
#[cfg(feature = "registry")]
fn register_opus(ctx: &mut oxideav_core::RuntimeContext, inv: &inventory::OsInventory) {
    let cid = CodecId::new("opus");

    let dec_caps = CodecCapabilities::audio("opus_audiotoolbox")
        .with_lossy(true)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(2)
        .with_max_sample_rate(48_000);

    if inv.decodes(sys::AudioFormatId::Opus) {
        ctx.codecs.register(
            CodecInfo::new(cid.clone())
                .capabilities(dec_caps)
                .decoder(opus_decoder::make_decoder)
                .tags([CodecTag::fourcc(b"Opus"), CodecTag::matroska("A_OPUS")]),
        );
    }

    let enc_caps = CodecCapabilities::audio("opus_audiotoolbox")
        .with_lossy(true)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10)
        .with_max_channels(2)
        .with_max_sample_rate(48_000);

    if inv.encodes(sys::AudioFormatId::Opus) {
        ctx.codecs.register(
            CodecInfo::new(cid)
                .capabilities(enc_caps)
                .encoder(opus_encoder::make_encoder),
        );
    }
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
    fn registration_matches_the_os_inventory_exactly() {
        // The registered decoder/encoder set must mirror the OS's own
        // inventory claim for every wired codec id, in both
        // directions — presence AND absence.
        use crate::sys::AudioFormatId as F;
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        for (cid, fmt) in [
            ("aac", F::Mpeg4AacLc),
            ("alac", F::AppleLossless),
            ("ilbc", F::Ilbc),
            ("amr_nb", F::AmrNb),
            ("amr_wb", F::AmrWb),
            ("mp1", F::MpegLayer1),
            ("mp2", F::MpegLayer2),
            ("mp3", F::MpegLayer3),
            ("flac", F::Flac),
            ("opus", F::Opus),
        ] {
            let id = CodecId::new(cid);
            let os_dec = crate::inventory::can_decode(fmt).unwrap_or(true);
            let os_enc = crate::inventory::can_encode(fmt).unwrap_or(true);
            assert_eq!(
                ctx.codecs.has_decoder(&id),
                os_dec,
                "{cid}: registered-decoder must mirror the OS decode inventory"
            );
            assert_eq!(
                ctx.codecs.has_encoder(&id),
                os_enc,
                "{cid}: registered-encoder must mirror the OS encode inventory"
            );
        }
    }

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

    #[test]
    fn register_installs_mp3_decoder_only() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        let id = CodecId::new("mp3");
        assert!(
            ctx.codecs.has_decoder(&id),
            "MP3 decoder not registered after register()"
        );
        // AT ships no MPEG-audio encoder — must be decode-only.
        assert!(
            !ctx.codecs.has_encoder(&id),
            "MP3 encoder must not be registered (AT is decode-only)"
        );
    }

    #[test]
    fn register_installs_mp1_mp2_decoders_only() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        for cid in ["mp1", "mp2"] {
            let id = CodecId::new(cid);
            assert!(
                ctx.codecs.has_decoder(&id),
                "{cid} decoder not registered after register()"
            );
            // AT ships no MPEG-audio encoder — decode-only.
            assert!(
                !ctx.codecs.has_encoder(&id),
                "{cid} encoder must not be registered (AT is decode-only)"
            );
        }
    }

    #[test]
    fn register_installs_flac_factories() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        let id = CodecId::new("flac");
        assert!(
            ctx.codecs.has_decoder(&id),
            "FLAC decoder not registered after register()"
        );
        // Round 218 wired the encoder side — FLAC is now symmetric on
        // AT (macOS 13+) and both factories register together.
        assert!(
            ctx.codecs.has_encoder(&id),
            "FLAC encoder not registered after register()"
        );
    }

    #[test]
    fn register_installs_opus_factories() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        let id = CodecId::new("opus");
        assert!(
            ctx.codecs.has_decoder(&id),
            "Opus decoder not registered after register()"
        );
        assert!(
            ctx.codecs.has_encoder(&id),
            "Opus encoder not registered after register()"
        );
    }
}
