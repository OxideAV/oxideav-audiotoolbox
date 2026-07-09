//! Safe RAII wrapper around an `AudioConverterRef`.
//!
//! Every codec module in this crate drives the same converter
//! lifecycle by hand: `AudioConverterNew` → property get/set →
//! `AudioConverterFillComplexBuffer` loop → `AudioConverterDispose`,
//! with raw `unsafe` calls and manual disposal on every early-exit
//! path. [`Converter`] owns that lifecycle once:
//!
//! * construction ([`Converter::new`]) loads the framework and creates
//!   the converter, returning a typed [`AtError`] on failure;
//! * `Drop` disposes the handle, so no code path can leak one;
//! * the property surface is typed — magic cookies, packet sizes,
//!   bit rates, prime info, current stream descriptions — with every
//!   `OSStatus` classified through [`AtStatus`](crate::status::AtStatus);
//! * [`Converter::raw`] exposes the handle for the
//!   `FillComplexBuffer` input-callback dance, which stays at the
//!   `sys` level because its buffer-lifetime contract is inherently
//!   caller-specific.
//!
//! The module is feature-independent (no `oxideav-core` types), so
//! `default-features = false` consumers get the safe lifecycle too;
//! under the `registry` feature [`AtError`] converts into
//! `oxideav_core::Error` via `From`.

use std::ffi::c_void;

use crate::status::{AtError, AtStatus};
use crate::sys::{
    self, AudioConverterPrimeInfo, AudioConverterRef, AudioStreamBasicDescription, AudioValueRange,
    Framework, K_AUDIO_CONVERTER_APPLICABLE_ENCODE_BIT_RATES,
    K_AUDIO_CONVERTER_APPLICABLE_ENCODE_SAMPLE_RATES, K_AUDIO_CONVERTER_AVAILABLE_ENCODE_BIT_RATES,
    K_AUDIO_CONVERTER_AVAILABLE_ENCODE_SAMPLE_RATES, K_AUDIO_CONVERTER_COMPRESSION_MAGIC_COOKIE,
    K_AUDIO_CONVERTER_CURRENT_OUTPUT_SD, K_AUDIO_CONVERTER_ENCODE_BIT_RATE,
    K_AUDIO_CONVERTER_MAX_OUTPUT_PACKET_SIZE, K_AUDIO_CONVERTER_PRIME_INFO, NO_ERR,
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

/// Load the framework, mapping the loader error into [`AtError`].
fn framework() -> Result<&'static Framework, AtError> {
    sys::framework().map_err(|e| AtError::FrameworkUnavailable(e.to_string()))
}

/// An owned AudioConverter with RAII disposal and a typed property
/// surface. See the [module docs](self) for the design rationale.
#[derive(Debug)]
pub struct Converter {
    raw: AudioConverterRef,
}

// SAFETY: the handle is only ever used through `&self` / `&mut self`
// on one thread at a time (the wrapper takes no interior mutability),
// matching how the codec modules already use their raw handles.
unsafe impl Send for Converter {}

impl Converter {
    /// Create a converter from `source` to `dest`.
    ///
    /// Returns [`AtError::FrameworkUnavailable`] when AudioToolbox
    /// cannot be loaded, or the classified `AudioConverterNew` status
    /// when the format pair is rejected (typically
    /// `FormatNotSupported`).
    pub fn new(
        source: &AudioStreamBasicDescription,
        dest: &AudioStreamBasicDescription,
    ) -> Result<Self, AtError> {
        let fw = framework()?;
        let mut raw: AudioConverterRef = std::ptr::null_mut();
        let status = unsafe { sys::audio_converter_new(fw, source, dest, &mut raw) };
        check("AudioConverterNew", status)?;
        Ok(Self { raw })
    }

    /// The raw handle, for `FillComplexBuffer` call sites that manage
    /// their own input-callback buffer lifetimes. The handle remains
    /// owned by `self`; do not dispose it.
    pub fn raw(&self) -> AudioConverterRef {
        self.raw
    }

    /// Reset the converter's internal state between independent
    /// streams (drops buffered/primed data; keeps the configuration).
    pub fn reset(&mut self) -> Result<(), AtError> {
        let fw = framework()?;
        let status = unsafe { sys::audio_converter_reset(fw, self.raw) };
        check("AudioConverterReset", status)
    }

    // ─────────────────────── raw property plumbing ───────────────────────

    /// Set a property from raw bytes.
    pub fn set_property_bytes(&mut self, property_id: u32, data: &[u8]) -> Result<(), AtError> {
        let fw = framework()?;
        let status = unsafe {
            sys::audio_converter_set_property(
                fw,
                self.raw,
                property_id,
                data.len() as u32,
                data.as_ptr() as *const c_void,
            )
        };
        check("AudioConverterSetProperty", status)
    }

    /// Get a property's raw bytes, sized via `GetPropertyInfo` first
    /// and truncated to the byte count the framework actually wrote.
    pub fn get_property_bytes(&self, property_id: u32) -> Result<Vec<u8>, AtError> {
        let fw = framework()?;
        let mut size: u32 = 0;
        let status = unsafe {
            sys::audio_converter_get_property_info(
                fw,
                self.raw,
                property_id,
                &mut size,
                std::ptr::null_mut(),
            )
        };
        check("AudioConverterGetPropertyInfo", status)?;
        let mut buf = vec![0u8; size as usize];
        if size > 0 {
            let mut io_size = size;
            let status = unsafe {
                sys::audio_converter_get_property(
                    fw,
                    self.raw,
                    property_id,
                    &mut io_size,
                    buf.as_mut_ptr() as *mut c_void,
                )
            };
            check("AudioConverterGetProperty", status)?;
            buf.truncate(io_size as usize);
        }
        Ok(buf)
    }

    /// Set a `u32`-valued property.
    pub fn set_u32(&mut self, property_id: u32, value: u32) -> Result<(), AtError> {
        self.set_property_bytes(property_id, &value.to_ne_bytes())
    }

    /// Get a `u32`-valued property.
    pub fn get_u32(&self, property_id: u32) -> Result<u32, AtError> {
        let fw = framework()?;
        let mut value: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let status = unsafe {
            sys::audio_converter_get_property(
                fw,
                self.raw,
                property_id,
                &mut size,
                &mut value as *mut u32 as *mut c_void,
            )
        };
        check("AudioConverterGetProperty", status)?;
        Ok(value)
    }

    /// Get a fixed-layout struct-valued property.
    fn get_struct<T: Copy + Default>(&self, property_id: u32) -> Result<T, AtError> {
        let fw = framework()?;
        let mut value = T::default();
        let mut size = std::mem::size_of::<T>() as u32;
        let status = unsafe {
            sys::audio_converter_get_property(
                fw,
                self.raw,
                property_id,
                &mut size,
                &mut value as *mut T as *mut c_void,
            )
        };
        check("AudioConverterGetProperty", status)?;
        Ok(value)
    }

    /// Get an `AudioValueRange`-array property, decoded from the raw
    /// byte payload (16 bytes per range: two native-endian `f64`s).
    fn get_value_ranges(&self, property_id: u32) -> Result<Vec<AudioValueRange>, AtError> {
        let bytes = self.get_property_bytes(property_id)?;
        Ok(bytes
            .chunks_exact(std::mem::size_of::<AudioValueRange>())
            .map(|chunk| AudioValueRange {
                minimum: f64::from_ne_bytes(chunk[0..8].try_into().unwrap()),
                maximum: f64::from_ne_bytes(chunk[8..16].try_into().unwrap()),
            })
            .collect())
    }

    // ──────────────────────── typed property surface ───────────────────────

    /// Install the decompression magic cookie (decoder-side
    /// out-of-band configuration: AudioSpecificConfig for AAC, the
    /// ALAC cookie, the `dfLa` STREAMINFO for FLAC, …).
    pub fn set_decompression_magic_cookie(&mut self, cookie: &[u8]) -> Result<(), AtError> {
        self.set_property_bytes(sys::K_AUDIO_CONVERTER_DECOMPRESSION_MAGIC_COOKIE, cookie)
    }

    /// Read the encoder-vended compression magic cookie for
    /// downstream muxer use.
    pub fn compression_magic_cookie(&self) -> Result<Vec<u8>, AtError> {
        self.get_property_bytes(K_AUDIO_CONVERTER_COMPRESSION_MAGIC_COOKIE)
    }

    /// Largest compressed packet the converter can produce, in bytes
    /// — the safe output-buffer size for one packet per
    /// `FillComplexBuffer` call.
    pub fn max_output_packet_size(&self) -> Result<u32, AtError> {
        self.get_u32(K_AUDIO_CONVERTER_MAX_OUTPUT_PACKET_SIZE)
    }

    /// Request an encode bit rate (bits per second). The codec
    /// quantises to its nearest supported rate; read
    /// [`Converter::encode_bit_rate`] back for the value it settled
    /// on.
    pub fn set_encode_bit_rate(&mut self, bits_per_second: u32) -> Result<(), AtError> {
        self.set_u32(K_AUDIO_CONVERTER_ENCODE_BIT_RATE, bits_per_second)
    }

    /// The encoder's current bit rate (bits per second).
    pub fn encode_bit_rate(&self) -> Result<u32, AtError> {
        self.get_u32(K_AUDIO_CONVERTER_ENCODE_BIT_RATE)
    }

    /// The converter's current output stream description — the
    /// framework's canonicalised copy, with any fields it completed.
    ///
    /// Only the output-side selector is exposed: the input-side
    /// `'acsd'` selector is rejected with `PropertyNotSupported` by
    /// every converter probed on current macOS (encode, decode, and
    /// PCM-to-PCM alike), so the wrapper does not offer a method that
    /// can never succeed. The caller supplied the input ASBD to
    /// [`Converter::new`] and can keep its own copy.
    pub fn current_output_stream_description(
        &self,
    ) -> Result<AudioStreamBasicDescription, AtError> {
        self.get_struct(K_AUDIO_CONVERTER_CURRENT_OUTPUT_SD)
    }

    /// The converter's edge-priming frame counts. For an AAC encode
    /// converter, `leading_frames` is the encoder delay (priming)
    /// that container formats record so players can trim it.
    pub fn prime_info(&self) -> Result<AudioConverterPrimeInfo, AtError> {
        self.get_struct(K_AUDIO_CONVERTER_PRIME_INFO)
    }

    /// Encode bit rates applicable to the converter's *current*
    /// configuration (sample rate / channel count), as value ranges
    /// (discrete rates have `minimum == maximum`).
    pub fn applicable_encode_bit_rates(&self) -> Result<Vec<AudioValueRange>, AtError> {
        self.get_value_ranges(K_AUDIO_CONVERTER_APPLICABLE_ENCODE_BIT_RATES)
    }

    /// Every encode bit rate the destination format supports across
    /// configurations.
    pub fn available_encode_bit_rates(&self) -> Result<Vec<AudioValueRange>, AtError> {
        self.get_value_ranges(K_AUDIO_CONVERTER_AVAILABLE_ENCODE_BIT_RATES)
    }

    /// Encode sample rates applicable to the current configuration.
    pub fn applicable_encode_sample_rates(&self) -> Result<Vec<AudioValueRange>, AtError> {
        self.get_value_ranges(K_AUDIO_CONVERTER_APPLICABLE_ENCODE_SAMPLE_RATES)
    }

    /// Every encode sample rate the destination format supports.
    pub fn available_encode_sample_rates(&self) -> Result<Vec<AudioValueRange>, AtError> {
        self.get_value_ranges(K_AUDIO_CONVERTER_AVAILABLE_ENCODE_SAMPLE_RATES)
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            if let Ok(fw) = sys::framework() {
                // Disposal failure at drop time is unreportable; the
                // handle is dead to us either way.
                unsafe {
                    let _ = sys::audio_converter_dispose(fw, self.raw);
                }
            }
            self.raw = std::ptr::null_mut();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::{AudioStreamBasicDescription as Asbd, K_AUDIO_FORMAT_MPEG4_AAC};

    fn pcm_to_aac() -> Converter {
        let src = Asbd::pcm_float32(44_100.0, 2);
        let dst = Asbd::mpeg4_aac(44_100.0, 2);
        Converter::new(&src, &dst).expect("PCM→AAC converter")
    }

    #[test]
    fn create_query_and_drop_pcm_to_aac_encoder() {
        let c = pcm_to_aac();
        assert!(!c.raw().is_null());

        // The AAC encoder must report a positive max packet size —
        // this is the output-buffer sizing contract every encode
        // module in this crate relies on.
        let max = c.max_output_packet_size().expect("max packet size");
        assert!(max > 0, "AAC max output packet size should be > 0");

        // Drop disposes; a second converter must be creatable after.
        drop(c);
        let c2 = pcm_to_aac();
        assert!(!c2.raw().is_null());
    }

    #[test]
    fn current_output_stream_description_reflects_the_destination() {
        // Encode direction: the output side is the AAC slot.
        let c = pcm_to_aac();
        let output = c
            .current_output_stream_description()
            .expect("current output ASBD (encode)");
        assert_eq!(
            output.format_id, K_AUDIO_FORMAT_MPEG4_AAC,
            "encode output side should be the AAC slot"
        );
        assert_eq!(output.sample_rate, 44_100.0);
        assert_eq!(output.channels_per_frame, 2);

        // Decode direction: the output side is linear PCM with the
        // fields the framework completed.
        let src = Asbd::mpeg4_aac(44_100.0, 2);
        let dst = Asbd::pcm_float32(44_100.0, 2);
        let c = Converter::new(&src, &dst).expect("AAC→PCM converter");
        let output = c
            .current_output_stream_description()
            .expect("current output ASBD (decode)");
        assert!(
            output.is_linear_pcm(),
            "decode output side should be linear PCM"
        );
        assert_eq!(output.sample_rate, 44_100.0);
    }

    #[test]
    fn encode_bit_rate_round_trips_through_the_codec() {
        let mut c = pcm_to_aac();
        let rates = c
            .applicable_encode_bit_rates()
            .expect("applicable encode bit rates");
        assert!(
            !rates.is_empty(),
            "AAC encoder should expose applicable bit rates"
        );

        // Pick a discrete applicable rate and set it; the codec must
        // accept it verbatim (it is by definition applicable).
        let pick = rates
            .iter()
            .find(|r| r.is_discrete() && r.minimum >= 64_000.0)
            .or_else(|| rates.first())
            .copied()
            .expect("at least one rate");
        c.set_encode_bit_rate(pick.minimum as u32)
            .expect("set applicable bit rate");
        let got = c.encode_bit_rate().expect("read back bit rate");
        assert!(
            pick.contains(got as f64),
            "read-back rate {got} should fall in the applicable range \
             [{}, {}]",
            pick.minimum,
            pick.maximum
        );

        // The available (all-configuration) set must be a superset
        // shape-wise: non-empty and containing the applicable pick.
        let avail = c
            .available_encode_bit_rates()
            .expect("available encode bit rates");
        assert!(!avail.is_empty());
        assert!(
            avail.iter().any(|r| r.contains(pick.minimum)),
            "applicable rate {} should appear in the available set",
            pick.minimum
        );
    }

    #[test]
    fn encode_sample_rate_queries_report_the_aac_grid() {
        let c = pcm_to_aac();
        let rates = c
            .available_encode_sample_rates()
            .expect("available encode sample rates");
        assert!(
            !rates.is_empty(),
            "AAC encoder should expose encode sample rates"
        );
        // 44.1 kHz — the configured rate — must be encodable.
        assert!(
            rates.iter().any(|r| r.contains(44_100.0)),
            "44.1 kHz should be an available AAC encode sample rate; got {rates:?}"
        );
        let applicable = c
            .applicable_encode_sample_rates()
            .expect("applicable encode sample rates");
        assert!(
            applicable.iter().any(|r| r.contains(44_100.0)),
            "the configured 44.1 kHz should be applicable; got {applicable:?}"
        );
    }

    #[test]
    fn aac_encoder_reports_priming_frames() {
        let c = pcm_to_aac();
        let prime = c.prime_info().expect("prime info");
        // The AAC encoder has a nonzero analysis delay: the first
        // output packet describes PCM from before the stream start.
        // Exact figures are codec-version-specific, so assert the
        // load-bearing property (present and positive) rather than a
        // magic number.
        assert!(
            prime.leading_frames > 0,
            "AAC encode converter should report leading (priming) frames; got {prime:?}"
        );
    }

    #[test]
    fn aac_encoder_vends_a_magic_cookie() {
        let c = pcm_to_aac();
        let cookie = c.compression_magic_cookie().expect("compression cookie");
        assert!(
            !cookie.is_empty(),
            "AAC encode converter should vend a non-empty magic cookie"
        );
    }

    #[test]
    fn reset_succeeds_on_a_fresh_converter() {
        let mut c = pcm_to_aac();
        c.reset().expect("reset");
    }

    #[test]
    fn pcm_to_pcm_conversion_pair_is_accepted() {
        // Sample-format conversion (s16 → f32) is the simplest
        // converter the framework offers; the wrapper must handle a
        // non-codec pair too.
        let src = Asbd::pcm_s16(48_000.0, 2);
        let dst = Asbd::pcm_float32(48_000.0, 2);
        let c = Converter::new(&src, &dst).expect("PCM→PCM converter");
        assert!(!c.raw().is_null());
    }

    #[test]
    fn unknown_format_pair_is_rejected_with_typed_error() {
        // A FourCC the system has no codec for must surface as a
        // classified error, not a panic or a bare integer.
        let src = Asbd {
            format_id: u32::from_be_bytes(*b"zzzz"),
            sample_rate: 48_000.0,
            channels_per_frame: 2,
            frames_per_packet: 1024,
            ..Asbd::default()
        };
        let dst = Asbd::pcm_float32(48_000.0, 2);
        let err = Converter::new(&src, &dst).expect_err("bogus format must fail");
        match err {
            AtError::Os { op, status } => {
                assert_eq!(op, "AudioConverterNew");
                // The exact code is an OS implementation detail
                // (observed: 'fmt?'); the classification contract is
                // that it is a *named* failure, not Unknown.
                assert!(
                    status.name().is_some(),
                    "rejection status should classify to a named code, got {status}"
                );
            }
            other => panic!("expected Os error, got {other:?}"),
        }
    }

    #[cfg(feature = "registry")]
    #[test]
    fn at_error_converts_into_core_error() {
        let err = AtError::Os {
            op: "AudioConverterNew",
            status: AtStatus::FormatNotSupported,
        };
        let core: oxideav_core::Error = err.into();
        assert!(
            matches!(core, oxideav_core::Error::Unsupported(_)),
            "fmt? should map to Unsupported: {core:?}"
        );

        let err = AtError::FrameworkUnavailable("dlopen failed".into());
        let core: oxideav_core::Error = err.into();
        assert!(
            matches!(core, oxideav_core::Error::Unsupported(_)),
            "framework-unavailable should map to Unsupported: {core:?}"
        );
    }
}
