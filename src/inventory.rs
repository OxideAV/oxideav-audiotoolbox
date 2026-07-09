//! OS codec inventory — global (converter-less) AudioFormat queries.
//!
//! The framework can be asked, before any converter exists, which
//! format IDs this system decodes and encodes, what a given encoder's
//! bit-rate / sample-rate grids look like, and whether a format's
//! packets are variable-size or externally framed. This module wraps
//! that query surface with typed results:
//!
//! * [`decodable_format_ids`] / [`encodable_format_ids`] — the
//!   system's decode/encode sets, classified through
//!   [`AudioFormatId`];
//! * [`can_decode`] / [`can_encode`] — membership tests, used to
//!   verify this crate's registration claims (including the
//!   decode-only AMR / MP3 asymmetry) against the OS's own inventory;
//! * [`available_encode_bit_rates`] / [`available_encode_sample_rates`]
//!   — per-format encoder grids as [`AudioValueRange`]s;
//! * [`format_is_vbr`] / [`format_is_externally_framed`] — per-ASBD
//!   packetisation semantics, the properties that decide whether a
//!   transport must carry `AudioStreamPacketDescription`s.
//!
//! Everything here is feature-independent (errors are
//! [`AtError`](crate::status::AtError)), so `default-features = false`
//! consumers can probe the OS inventory without pulling
//! `oxideav-core`.

use std::ffi::c_void;

use crate::status::{AtError, AtStatus};
use crate::sys::{
    self, AudioFormatId, AudioStreamBasicDescription, AudioValueRange,
    K_AUDIO_FORMAT_PROPERTY_AVAILABLE_ENCODE_BIT_RATES,
    K_AUDIO_FORMAT_PROPERTY_AVAILABLE_ENCODE_SAMPLE_RATES,
    K_AUDIO_FORMAT_PROPERTY_DECODE_FORMAT_IDS, K_AUDIO_FORMAT_PROPERTY_ENCODE_FORMAT_IDS,
    K_AUDIO_FORMAT_PROPERTY_FORMAT_IS_EXTERNALLY_FRAMED, K_AUDIO_FORMAT_PROPERTY_FORMAT_IS_VBR,
    NO_ERR,
};

/// Convert a raw `OSStatus` into `Ok(())` / `Err(AtError::Os)`.
fn check(op: &'static str, raw: sys::OSStatus) -> Result<(), AtError> {
    if raw == NO_ERR {
        Ok(())
    } else {
        Err(AtError::Os {
            op,
            status: AtStatus::from_raw(raw),
        })
    }
}

/// Fetch a global AudioFormat property's raw bytes: size it via
/// `AudioFormatGetPropertyInfo`, then read it, truncating to the byte
/// count the framework actually wrote.
fn get_global_bytes(property_id: u32, specifier: &[u8]) -> Result<Vec<u8>, AtError> {
    let fw = sys::framework().map_err(|e| AtError::FrameworkUnavailable(e.to_string()))?;
    let spec_ptr = if specifier.is_empty() {
        std::ptr::null()
    } else {
        specifier.as_ptr() as *const c_void
    };
    let mut size: u32 = 0;
    let status = unsafe {
        sys::audio_format_get_property_info(
            fw,
            property_id,
            specifier.len() as u32,
            spec_ptr,
            &mut size,
        )
    };
    check("AudioFormatGetPropertyInfo", status)?;
    let mut buf = vec![0u8; size as usize];
    if size > 0 {
        let mut io_size = size;
        let status = unsafe {
            sys::audio_format_get_property(
                fw,
                property_id,
                specifier.len() as u32,
                spec_ptr,
                &mut io_size,
                buf.as_mut_ptr() as *mut c_void,
            )
        };
        check("AudioFormatGetProperty", status)?;
        buf.truncate(io_size as usize);
    }
    Ok(buf)
}

/// Decode a raw byte payload into the `u32` format IDs it packs
/// (native-endian, 4 bytes each), classified through
/// [`AudioFormatId`].
fn parse_format_ids(bytes: &[u8]) -> Vec<AudioFormatId> {
    bytes
        .chunks_exact(4)
        .map(|c| AudioFormatId::from_u32(u32::from_ne_bytes(c.try_into().unwrap())))
        .collect()
}

/// Every format ID this system's AudioToolbox can decode **from**.
/// Formats outside this crate's wired set appear as
/// `AudioFormatId::Unknown(raw)`.
pub fn decodable_format_ids() -> Result<Vec<AudioFormatId>, AtError> {
    Ok(parse_format_ids(&get_global_bytes(
        K_AUDIO_FORMAT_PROPERTY_DECODE_FORMAT_IDS,
        &[],
    )?))
}

/// Every format ID this system's AudioToolbox can encode **to**.
pub fn encodable_format_ids() -> Result<Vec<AudioFormatId>, AtError> {
    Ok(parse_format_ids(&get_global_bytes(
        K_AUDIO_FORMAT_PROPERTY_ENCODE_FORMAT_IDS,
        &[],
    )?))
}

/// `true` when the system can decode the given format.
pub fn can_decode(id: AudioFormatId) -> Result<bool, AtError> {
    Ok(decodable_format_ids()?.contains(&id))
}

/// `true` when the system can encode to the given format.
pub fn can_encode(id: AudioFormatId) -> Result<bool, AtError> {
    Ok(encodable_format_ids()?.contains(&id))
}

/// The encoder bit-rate grid for `format`, as value ranges (discrete
/// rates have `minimum == maximum`). Formats without an encoder
/// return an error (typically `UnsupportedFormat` /
/// `UnknownFormat`) or an empty grid, depending on the OS.
pub fn available_encode_bit_rates(format: AudioFormatId) -> Result<Vec<AudioValueRange>, AtError> {
    let spec = format.as_u32().to_ne_bytes();
    let bytes = get_global_bytes(K_AUDIO_FORMAT_PROPERTY_AVAILABLE_ENCODE_BIT_RATES, &spec)?;
    Ok(parse_value_ranges(&bytes))
}

/// The encoder sample-rate grid for `format`.
pub fn available_encode_sample_rates(
    format: AudioFormatId,
) -> Result<Vec<AudioValueRange>, AtError> {
    let spec = format.as_u32().to_ne_bytes();
    let bytes = get_global_bytes(K_AUDIO_FORMAT_PROPERTY_AVAILABLE_ENCODE_SAMPLE_RATES, &spec)?;
    Ok(parse_value_ranges(&bytes))
}

fn parse_value_ranges(bytes: &[u8]) -> Vec<AudioValueRange> {
    bytes
        .chunks_exact(std::mem::size_of::<AudioValueRange>())
        .map(|chunk| AudioValueRange {
            minimum: f64::from_ne_bytes(chunk[0..8].try_into().unwrap()),
            maximum: f64::from_ne_bytes(chunk[8..16].try_into().unwrap()),
        })
        .collect()
}

/// Fetch a `u32`-boolean ASBD-specified property.
fn asbd_bool_property(
    property_id: u32,
    asbd: &AudioStreamBasicDescription,
) -> Result<bool, AtError> {
    let spec = unsafe {
        std::slice::from_raw_parts(
            asbd as *const AudioStreamBasicDescription as *const u8,
            std::mem::size_of::<AudioStreamBasicDescription>(),
        )
    };
    let bytes = get_global_bytes(property_id, spec)?;
    if bytes.len() < 4 {
        return Err(AtError::Os {
            op: "AudioFormatGetProperty",
            status: AtStatus::BadPropertySize,
        });
    }
    Ok(u32::from_ne_bytes(bytes[0..4].try_into().unwrap()) != 0)
}

/// `true` when the format's packets vary in size (VBR), meaning a
/// transport must carry per-packet byte counts.
pub fn format_is_vbr(asbd: &AudioStreamBasicDescription) -> Result<bool, AtError> {
    asbd_bool_property(K_AUDIO_FORMAT_PROPERTY_FORMAT_IS_VBR, asbd)
}

/// `true` when packet boundaries cannot be recovered from the byte
/// stream alone, so transport must carry
/// `AudioStreamPacketDescription`s.
pub fn format_is_externally_framed(asbd: &AudioStreamBasicDescription) -> Result<bool, AtError> {
    asbd_bool_property(K_AUDIO_FORMAT_PROPERTY_FORMAT_IS_EXTERNALLY_FRAMED, asbd)
}

/// A one-shot snapshot of the OS's decode/encode format sets, for
/// callers that gate many decisions on the inventory without
/// re-querying the framework per format (e.g. `register()`, which
/// checks up to nine codec slots).
///
/// [`OsInventory::probe`] is infallible by design: if either query
/// fails (framework missing, property refused), the corresponding
/// set is left empty and the membership tests report `true` — the
/// caller falls back to optimistic registration and the per-factory
/// error paths still guard at construction time.
#[derive(Clone, Debug, Default)]
pub struct OsInventory {
    decodable: Vec<AudioFormatId>,
    encodable: Vec<AudioFormatId>,
}

impl OsInventory {
    /// Snapshot the OS inventory. Never fails; see the type docs for
    /// the degraded-mode semantics.
    pub fn probe() -> Self {
        Self {
            decodable: decodable_format_ids().unwrap_or_default(),
            encodable: encodable_format_ids().unwrap_or_default(),
        }
    }

    /// `true` when the OS decodes `id` — or when the decode set could
    /// not be queried (optimistic fallback).
    pub fn decodes(&self, id: AudioFormatId) -> bool {
        self.decodable.is_empty() || self.decodable.contains(&id)
    }

    /// `true` when the OS encodes to `id` — or when the encode set
    /// could not be queried (optimistic fallback).
    pub fn encodes(&self, id: AudioFormatId) -> bool {
        self.encodable.is_empty() || self.encodable.contains(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::AudioFormatId as F;

    // ─── registration parity: the OS's own inventory must back every
    //     claim `lib.rs::register()` makes ───

    #[test]
    fn os_decode_inventory_covers_every_registered_decoder() {
        // Every format slot this crate registers a decoder for (plus
        // the Layer I / II slots the sys surface exposes constructors
        // for) must appear in the OS's decode set.
        for id in [
            F::Mpeg4AacLc,
            F::Mpeg4AacHe,
            F::Mpeg4AacHeV2,
            F::Mpeg4AacLd,
            F::Mpeg4AacEld,
            F::AppleLossless,
            F::Ilbc,
            F::AmrNb,
            F::AmrWb,
            F::MpegLayer1,
            F::MpegLayer2,
            F::MpegLayer3,
            F::Flac,
            F::Opus,
            F::LinearPcm,
        ] {
            assert!(
                can_decode(id).expect("decode inventory"),
                "{id:?} should be OS-decodable"
            );
        }
    }

    #[test]
    fn os_encode_inventory_covers_every_registered_encoder() {
        for id in [
            F::Mpeg4AacLc,
            F::Mpeg4AacHe,
            F::Mpeg4AacHeV2,
            F::Mpeg4AacLd,
            F::Mpeg4AacEld,
            F::AppleLossless,
            F::Ilbc,
            F::Flac,
            F::Opus,
            F::LinearPcm,
        ] {
            assert!(
                can_encode(id).expect("encode inventory"),
                "{id:?} should be OS-encodable"
            );
        }
    }

    #[test]
    fn decode_only_formats_have_no_os_encoder() {
        // The crate registers AMR-NB / AMR-WB / MP3 as decode-only.
        // The OS's own inventory backs the asymmetry: none of them
        // (nor Layer I / II) appear in the encode set.
        for id in [
            F::AmrNb,
            F::AmrWb,
            F::MpegLayer1,
            F::MpegLayer2,
            F::MpegLayer3,
        ] {
            assert!(
                !can_encode(id).expect("encode inventory"),
                "{id:?} must not be OS-encodable (decode-only registration)"
            );
        }
    }

    // ─── encoder grids ───

    #[test]
    fn opus_encode_bit_rate_grid_is_discrete() {
        let rates = available_encode_bit_rates(F::Opus).expect("Opus bit rates");
        assert!(
            !rates.is_empty(),
            "Opus encoder should expose a bit-rate grid"
        );
        assert!(
            rates.iter().all(|r| r.is_discrete()),
            "observed Opus grid is a list of discrete rates: {rates:?}"
        );
        assert!(
            rates.iter().any(|r| r.contains(32_000.0)),
            "32 kbit/s should be an Opus encode rate: {rates:?}"
        );
    }

    #[test]
    fn aac_encode_sample_rate_grid_contains_the_cd_rate() {
        let rates = available_encode_sample_rates(F::Mpeg4AacLc).expect("AAC sample rates");
        assert!(!rates.is_empty());
        assert!(
            rates.iter().any(|r| r.contains(44_100.0)),
            "44.1 kHz should be an AAC encode sample rate: {rates:?}"
        );
    }

    #[test]
    fn flac_encode_sample_rate_grid_answers() {
        // Observed quirk worth pinning: the FLAC encoder answers the
        // sample-rate query with a single all-zero range (an
        // "unconstrained" wildcard) rather than an explicit grid. The
        // load-bearing assertion is that the query itself is
        // supported for a format the encode inventory lists.
        let rates = available_encode_sample_rates(F::Flac).expect("FLAC sample rates");
        assert!(!rates.is_empty(), "FLAC grid query should answer non-empty");
    }

    #[test]
    fn amr_nb_encoder_grid_is_refused_with_named_status() {
        // AMR-NB has no OS encoder; asking for its encode grid fails
        // with the classified format-not-supported code, not a bare
        // integer (observed: 'fmt?').
        let err = available_encode_bit_rates(F::AmrNb).expect_err("AMR-NB has no encoder");
        assert_eq!(
            err.status(),
            Some(AtStatus::FormatNotSupported),
            "expected a classified rejection, got {err}"
        );
    }

    // ─── packetisation semantics ───

    #[test]
    fn vbr_and_external_framing_match_the_wired_transports() {
        // The VBR formats whose bridges supply per-packet
        // AudioStreamPacketDescriptions...
        for (name, asbd) in [
            ("aac", AudioStreamBasicDescription::mpeg4_aac(44_100.0, 2)),
            (
                "mp3",
                AudioStreamBasicDescription::mpeg_layer3(44_100.0, 2, 1152),
            ),
            ("opus", AudioStreamBasicDescription::opus(48_000.0, 2, 960)),
        ] {
            assert!(format_is_vbr(&asbd).expect("fvbr"), "{name} should be VBR");
            assert!(
                format_is_externally_framed(&asbd).expect("fexf"),
                "{name} should be externally framed"
            );
        }
        // ...and the constant-size formats that need neither.
        for (name, asbd) in [
            ("ilbc", AudioStreamBasicDescription::ilbc(240)),
            ("pcm", AudioStreamBasicDescription::pcm_float32(44_100.0, 2)),
        ] {
            assert!(!format_is_vbr(&asbd).expect("fvbr"), "{name} should be CBR");
            assert!(
                !format_is_externally_framed(&asbd).expect("fexf"),
                "{name} should be self-framed"
            );
        }
    }

    #[test]
    fn inventory_lists_are_deduplicated_views_of_real_data() {
        // Both lists must be non-trivial (the OS ships dozens of
        // decoders) and every entry must round-trip through the
        // classifier without loss.
        let dec = decodable_format_ids().expect("decode ids");
        let enc = encodable_format_ids().expect("encode ids");
        assert!(
            dec.len() >= 15,
            "decode set unexpectedly small: {}",
            dec.len()
        );
        assert!(
            enc.len() >= 8,
            "encode set unexpectedly small: {}",
            enc.len()
        );
        for id in dec.iter().chain(enc.iter()) {
            assert_eq!(
                *id,
                F::from_u32(id.as_u32()),
                "classifier round-trip must be lossless"
            );
        }
    }

    #[test]
    fn os_inventory_snapshot_matches_the_live_queries() {
        let inv = OsInventory::probe();
        assert!(inv.decodes(F::Mpeg4AacLc));
        assert!(inv.encodes(F::Mpeg4AacLc));
        assert!(inv.decodes(F::AmrNb));
        assert!(!inv.encodes(F::AmrNb), "AMR-NB is decode-only");
        assert!(!inv.encodes(F::MpegLayer3), "MP3 is decode-only");
        // The degraded default (empty sets) is optimistic.
        let empty = OsInventory::default();
        assert!(empty.decodes(F::Unknown(0x7878_7878)));
        assert!(empty.encodes(F::Unknown(0x7878_7878)));
    }
}
