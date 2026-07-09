//! Typed classification of CoreAudio `OSStatus` failure codes.
//!
//! Every CoreAudio C entry point reports failure through a bare
//! 32-bit `OSStatus`. The values are drawn from several per-header
//! enums in the platform SDK (`AudioConverter.h`, `AudioCodec.h`,
//! `AudioFormat.h`, plus the general `kAudio_*` codes in
//! `CoreAudioBaseTypes.h`), most of them packed big-endian FourCCs.
//! Left raw, a failure surfaces as an inscrutable integer like
//! `1718449215` — which is actually `'fmt?'`, "format not supported".
//!
//! [`AtStatus`] gives those values one typed home:
//!
//! * [`AtStatus::from_raw`] classifies a raw `OSStatus`,
//! * [`AtStatus::name`] recovers the platform constant name,
//! * [`AtStatus::kind`] buckets the code into the coarse retry/report
//!   semantics a bridge caller acts on ([`StatusKind`]),
//! * the [`Display`](std::fmt::Display) impl renders
//!   `name ('fourcc' / raw)` for diagnostics, and
//! * [`status_error`] (behind the `registry` feature) converts an
//!   `(operation, status)` pair straight into the matching
//!   `oxideav_core::Error` variant.
//!
//! Several FourCCs are shared across the SDK's per-header enums with
//! the same meaning (e.g. `'what'` is both
//! `kAudioConverterErr_UnspecifiedError` and
//! `kAudioCodecUnspecifiedError`; `'!siz'` is the bad-property-size
//! code in three headers). The classifier treats the *value* as
//! canonical: one variant per value, with the name rendered from the
//! AudioConverter-flavoured constant since the converter API is this
//! bridge's primary surface.

use crate::sys::OSStatus;

/// Pack a 4-byte ASCII tag into the big-endian `OSStatus` value the
/// platform headers spell as a character literal (`'fmt?'`).
const fn fourcc(tag: &[u8; 4]) -> OSStatus {
    i32::from_be_bytes(*tag)
}

// ─────────────────────── AudioConverter.h error codes ───────────────────────

/// `kAudioConverterErr_FormatNotSupported` (`'fmt?'`).
pub const K_AUDIO_CONVERTER_ERR_FORMAT_NOT_SUPPORTED: OSStatus = fourcc(b"fmt?");
/// `kAudioConverterErr_OperationNotSupported` (`'op??'`).
pub const K_AUDIO_CONVERTER_ERR_OPERATION_NOT_SUPPORTED: OSStatus = fourcc(b"op??");
/// `kAudioConverterErr_PropertyNotSupported` (`'prop'`).
pub const K_AUDIO_CONVERTER_ERR_PROPERTY_NOT_SUPPORTED: OSStatus = fourcc(b"prop");
/// `kAudioConverterErr_InvalidInputSize` (`'insz'`).
pub const K_AUDIO_CONVERTER_ERR_INVALID_INPUT_SIZE: OSStatus = fourcc(b"insz");
/// `kAudioConverterErr_InvalidOutputSize` (`'otsz'`).
pub const K_AUDIO_CONVERTER_ERR_INVALID_OUTPUT_SIZE: OSStatus = fourcc(b"otsz");
/// `kAudioConverterErr_UnspecifiedError` (`'what'`). Shared value with
/// `kAudioCodecUnspecifiedError` / `kAudioFormatUnspecifiedError`.
pub const K_AUDIO_CONVERTER_ERR_UNSPECIFIED: OSStatus = fourcc(b"what");
/// `kAudioConverterErr_BadPropertySizeError` (`'!siz'`). Shared value
/// with `kAudioCodecBadPropertySizeError` /
/// `kAudioFormatBadPropertySizeError`.
pub const K_AUDIO_CONVERTER_ERR_BAD_PROPERTY_SIZE: OSStatus = fourcc(b"!siz");
/// `kAudioConverterErr_RequiresPacketDescriptionsError` (`'!pkd'`).
pub const K_AUDIO_CONVERTER_ERR_REQUIRES_PACKET_DESCRIPTIONS: OSStatus = fourcc(b"!pkd");
/// `kAudioConverterErr_InputSampleRateOutOfRange` (`'!isr'`).
pub const K_AUDIO_CONVERTER_ERR_INPUT_SAMPLE_RATE_OUT_OF_RANGE: OSStatus = fourcc(b"!isr");
/// `kAudioConverterErr_OutputSampleRateOutOfRange` (`'!osr'`).
pub const K_AUDIO_CONVERTER_ERR_OUTPUT_SAMPLE_RATE_OUT_OF_RANGE: OSStatus = fourcc(b"!osr");
/// `kAudioConverterErr_HardwareInUse` (`'hwiu'`) — the hardware codec
/// slot is occupied by another converter (iOS-class hardware caps the
/// number of concurrent hardware converters; macOS reports it too when
/// the dedicated engine is saturated).
pub const K_AUDIO_CONVERTER_ERR_HARDWARE_IN_USE: OSStatus = fourcc(b"hwiu");
/// `kAudioConverterErr_NoHardwarePermission` (`'perm'`) — the process
/// is not entitled to use the hardware codec.
pub const K_AUDIO_CONVERTER_ERR_NO_HARDWARE_PERMISSION: OSStatus = fourcc(b"perm");

// ───────────────────────── AudioCodec.h error codes ─────────────────────────

/// `kAudioCodecUnknownPropertyError` (`'who?'`).
pub const K_AUDIO_CODEC_UNKNOWN_PROPERTY: OSStatus = fourcc(b"who?");
/// `kAudioCodecIllegalOperationError` (`'nope'`).
pub const K_AUDIO_CODEC_ILLEGAL_OPERATION: OSStatus = fourcc(b"nope");
/// `kAudioCodecUnsupportedFormatError` (`'!dat'`).
pub const K_AUDIO_CODEC_UNSUPPORTED_FORMAT: OSStatus = fourcc(b"!dat");
/// `kAudioCodecStateError` (`'!stt'`).
pub const K_AUDIO_CODEC_STATE_ERROR: OSStatus = fourcc(b"!stt");
/// `kAudioCodecNotEnoughBufferSpaceError` (`'!buf'`).
pub const K_AUDIO_CODEC_NOT_ENOUGH_BUFFER_SPACE: OSStatus = fourcc(b"!buf");
/// `kAudioCodecBadDataError` (`'bada'`) — the compressed input
/// violates the codec's bitstream rules. The one code that
/// unambiguously means "the *data* is bad" rather than "the *call*
/// was bad".
pub const K_AUDIO_CODEC_BAD_DATA: OSStatus = fourcc(b"bada");

// ───────────────────────── AudioFormat.h error codes ────────────────────────

/// `kAudioFormatUnknownFormatError` (`'!fmt'`).
pub const K_AUDIO_FORMAT_UNKNOWN_FORMAT: OSStatus = fourcc(b"!fmt");
/// `kAudioFormatBadSpecifierSizeError` (`'!spc'`).
pub const K_AUDIO_FORMAT_BAD_SPECIFIER_SIZE: OSStatus = fourcc(b"!spc");

// ─────────────────── CoreAudioBaseTypes.h general codes ─────────────────────

/// `kAudio_UnimplementedError` (-4).
pub const K_AUDIO_UNIMPLEMENTED: OSStatus = -4;
/// `kAudio_FileNotFoundError` (-43).
pub const K_AUDIO_FILE_NOT_FOUND: OSStatus = -43;
/// `kAudio_FilePermissionError` (-54).
pub const K_AUDIO_FILE_PERMISSION: OSStatus = -54;
/// `kAudio_TooManyFilesOpenError` (-42).
pub const K_AUDIO_TOO_MANY_FILES_OPEN: OSStatus = -42;
/// `kAudio_BadFilePathError` (`'!pth'`).
pub const K_AUDIO_BAD_FILE_PATH: OSStatus = fourcc(b"!pth");
/// `kAudio_ParamError` (-50) — an argument to the call was invalid
/// (null pointer, out-of-range value, inconsistent descriptor).
pub const K_AUDIO_PARAM_ERROR: OSStatus = -50;
/// `kAudio_MemFullError` (-108).
pub const K_AUDIO_MEM_FULL: OSStatus = -108;

/// Coarse semantic bucket for an [`AtStatus`] — the axis a bridge
/// caller actually branches on when deciding how to surface a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusKind {
    /// The call succeeded.
    Ok,
    /// The request asked for something this system's AudioToolbox does
    /// not provide (format, property, sample rate, operation,
    /// entitlement). Maps to `Error::Unsupported`: valid input, missing
    /// capability — the registry should fall back to another impl.
    Unsupported,
    /// The compressed input violates the codec's bitstream rules.
    /// Maps to `Error::InvalidData`; not retryable with the same bytes.
    InvalidData,
    /// A finite resource ran out — memory, output buffer space, or the
    /// hardware codec slot. Maps to `Error::ResourceExhausted`;
    /// potentially transient.
    ResourceExhausted,
    /// The *call* was malformed — wrong buffer geometry, bad property
    /// size, missing packet descriptions, illegal converter state.
    /// These indicate a bridge bug rather than a media problem; maps
    /// to `Error::Other` with the full story in the message.
    Usage,
    /// Unspecified or unrecognised code. Maps to `Error::Other`.
    Other,
}

/// Typed view of a CoreAudio `OSStatus` as returned by the
/// AudioToolbox entry points this bridge calls.
///
/// [`AtStatus::from_raw`] is total: every documented failure code the
/// converter/codec/format headers define maps to a named variant, and
/// anything else is carried verbatim in [`AtStatus::Unknown`] so
/// `from_raw(s).as_raw() == s` for all inputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtStatus {
    /// `noErr` (0).
    Ok,
    /// `'fmt?'` — the requested format (or format pair) is not
    /// supported. Also `kAudioFormatUnsupportedDataFormatError`, which
    /// shares the value.
    FormatNotSupported,
    /// `'op??'` — the operation is not supported by this converter.
    OperationNotSupported,
    /// `'prop'` — the property is not supported. Also
    /// `kAudioFormatUnsupportedPropertyError`, which shares the value.
    PropertyNotSupported,
    /// `'insz'` — the supplied input buffer size is invalid.
    InvalidInputSize,
    /// `'otsz'` — the supplied output buffer size is invalid.
    InvalidOutputSize,
    /// `'what'` — unspecified error.
    Unspecified,
    /// `'!siz'` — the property value size was wrong.
    BadPropertySize,
    /// `'!pkd'` — the input format is packetised and the call must
    /// supply `AudioStreamPacketDescription`s but did not.
    RequiresPacketDescriptions,
    /// `'!isr'` — input sample rate out of the converter's range.
    InputSampleRateOutOfRange,
    /// `'!osr'` — output sample rate out of the converter's range.
    OutputSampleRateOutOfRange,
    /// `'hwiu'` — hardware codec slot busy.
    HardwareInUse,
    /// `'perm'` — no entitlement to use the hardware codec.
    NoHardwarePermission,
    /// `'who?'` — the codec does not know the property.
    UnknownProperty,
    /// `'nope'` — the operation is illegal in the codec's current
    /// configuration (e.g. writing an init-time property after
    /// initialisation).
    IllegalOperation,
    /// `'!dat'` — the codec does not support the supplied data format.
    UnsupportedFormat,
    /// `'!stt'` — the codec is in the wrong state for the call
    /// (e.g. producing packets before it was initialised).
    StateError,
    /// `'!buf'` — the supplied output buffer is too small.
    NotEnoughBufferSpace,
    /// `'bada'` — the compressed input violates the codec's bitstream
    /// rules.
    BadData,
    /// `'!fmt'` — the format API does not recognise the format.
    UnknownFormat,
    /// `'!spc'` — the format-API specifier size was wrong.
    BadSpecifierSize,
    /// `'!pth'` — bad file path.
    BadFilePath,
    /// -4 — the call is not implemented on this system.
    Unimplemented,
    /// -43 — file not found.
    FileNotFound,
    /// -54 — file permission denied.
    FilePermission,
    /// -42 — too many files open.
    TooManyFilesOpen,
    /// -50 — an argument to the call was invalid.
    ParamError,
    /// -108 — memory allocation failed.
    MemFull,
    /// Any `OSStatus` value without a documented AudioToolbox meaning.
    /// Carries the raw value so nothing is lost in reporting.
    Unknown(OSStatus),
}

impl AtStatus {
    /// Classify a raw `OSStatus`.
    pub fn from_raw(raw: OSStatus) -> Self {
        match raw {
            crate::sys::NO_ERR => Self::Ok,
            K_AUDIO_CONVERTER_ERR_FORMAT_NOT_SUPPORTED => Self::FormatNotSupported,
            K_AUDIO_CONVERTER_ERR_OPERATION_NOT_SUPPORTED => Self::OperationNotSupported,
            K_AUDIO_CONVERTER_ERR_PROPERTY_NOT_SUPPORTED => Self::PropertyNotSupported,
            K_AUDIO_CONVERTER_ERR_INVALID_INPUT_SIZE => Self::InvalidInputSize,
            K_AUDIO_CONVERTER_ERR_INVALID_OUTPUT_SIZE => Self::InvalidOutputSize,
            K_AUDIO_CONVERTER_ERR_UNSPECIFIED => Self::Unspecified,
            K_AUDIO_CONVERTER_ERR_BAD_PROPERTY_SIZE => Self::BadPropertySize,
            K_AUDIO_CONVERTER_ERR_REQUIRES_PACKET_DESCRIPTIONS => Self::RequiresPacketDescriptions,
            K_AUDIO_CONVERTER_ERR_INPUT_SAMPLE_RATE_OUT_OF_RANGE => Self::InputSampleRateOutOfRange,
            K_AUDIO_CONVERTER_ERR_OUTPUT_SAMPLE_RATE_OUT_OF_RANGE => {
                Self::OutputSampleRateOutOfRange
            }
            K_AUDIO_CONVERTER_ERR_HARDWARE_IN_USE => Self::HardwareInUse,
            K_AUDIO_CONVERTER_ERR_NO_HARDWARE_PERMISSION => Self::NoHardwarePermission,
            K_AUDIO_CODEC_UNKNOWN_PROPERTY => Self::UnknownProperty,
            K_AUDIO_CODEC_ILLEGAL_OPERATION => Self::IllegalOperation,
            K_AUDIO_CODEC_UNSUPPORTED_FORMAT => Self::UnsupportedFormat,
            K_AUDIO_CODEC_STATE_ERROR => Self::StateError,
            K_AUDIO_CODEC_NOT_ENOUGH_BUFFER_SPACE => Self::NotEnoughBufferSpace,
            K_AUDIO_CODEC_BAD_DATA => Self::BadData,
            K_AUDIO_FORMAT_UNKNOWN_FORMAT => Self::UnknownFormat,
            K_AUDIO_FORMAT_BAD_SPECIFIER_SIZE => Self::BadSpecifierSize,
            K_AUDIO_BAD_FILE_PATH => Self::BadFilePath,
            K_AUDIO_UNIMPLEMENTED => Self::Unimplemented,
            K_AUDIO_FILE_NOT_FOUND => Self::FileNotFound,
            K_AUDIO_FILE_PERMISSION => Self::FilePermission,
            K_AUDIO_TOO_MANY_FILES_OPEN => Self::TooManyFilesOpen,
            K_AUDIO_PARAM_ERROR => Self::ParamError,
            K_AUDIO_MEM_FULL => Self::MemFull,
            other => Self::Unknown(other),
        }
    }

    /// Recover the raw `OSStatus`. Round-trips
    /// `from_raw(raw).as_raw() == raw` for every value.
    pub fn as_raw(self) -> OSStatus {
        match self {
            Self::Ok => crate::sys::NO_ERR,
            Self::FormatNotSupported => K_AUDIO_CONVERTER_ERR_FORMAT_NOT_SUPPORTED,
            Self::OperationNotSupported => K_AUDIO_CONVERTER_ERR_OPERATION_NOT_SUPPORTED,
            Self::PropertyNotSupported => K_AUDIO_CONVERTER_ERR_PROPERTY_NOT_SUPPORTED,
            Self::InvalidInputSize => K_AUDIO_CONVERTER_ERR_INVALID_INPUT_SIZE,
            Self::InvalidOutputSize => K_AUDIO_CONVERTER_ERR_INVALID_OUTPUT_SIZE,
            Self::Unspecified => K_AUDIO_CONVERTER_ERR_UNSPECIFIED,
            Self::BadPropertySize => K_AUDIO_CONVERTER_ERR_BAD_PROPERTY_SIZE,
            Self::RequiresPacketDescriptions => K_AUDIO_CONVERTER_ERR_REQUIRES_PACKET_DESCRIPTIONS,
            Self::InputSampleRateOutOfRange => K_AUDIO_CONVERTER_ERR_INPUT_SAMPLE_RATE_OUT_OF_RANGE,
            Self::OutputSampleRateOutOfRange => {
                K_AUDIO_CONVERTER_ERR_OUTPUT_SAMPLE_RATE_OUT_OF_RANGE
            }
            Self::HardwareInUse => K_AUDIO_CONVERTER_ERR_HARDWARE_IN_USE,
            Self::NoHardwarePermission => K_AUDIO_CONVERTER_ERR_NO_HARDWARE_PERMISSION,
            Self::UnknownProperty => K_AUDIO_CODEC_UNKNOWN_PROPERTY,
            Self::IllegalOperation => K_AUDIO_CODEC_ILLEGAL_OPERATION,
            Self::UnsupportedFormat => K_AUDIO_CODEC_UNSUPPORTED_FORMAT,
            Self::StateError => K_AUDIO_CODEC_STATE_ERROR,
            Self::NotEnoughBufferSpace => K_AUDIO_CODEC_NOT_ENOUGH_BUFFER_SPACE,
            Self::BadData => K_AUDIO_CODEC_BAD_DATA,
            Self::UnknownFormat => K_AUDIO_FORMAT_UNKNOWN_FORMAT,
            Self::BadSpecifierSize => K_AUDIO_FORMAT_BAD_SPECIFIER_SIZE,
            Self::BadFilePath => K_AUDIO_BAD_FILE_PATH,
            Self::Unimplemented => K_AUDIO_UNIMPLEMENTED,
            Self::FileNotFound => K_AUDIO_FILE_NOT_FOUND,
            Self::FilePermission => K_AUDIO_FILE_PERMISSION,
            Self::TooManyFilesOpen => K_AUDIO_TOO_MANY_FILES_OPEN,
            Self::ParamError => K_AUDIO_PARAM_ERROR,
            Self::MemFull => K_AUDIO_MEM_FULL,
            Self::Unknown(raw) => raw,
        }
    }

    /// The platform-SDK constant name for the code, or `None` for
    /// `Unknown`. Where a value is shared across headers, the
    /// AudioConverter-flavoured name is reported (the converter API is
    /// this bridge's primary surface).
    pub fn name(self) -> Option<&'static str> {
        Some(match self {
            Self::Ok => "noErr",
            Self::FormatNotSupported => "kAudioConverterErr_FormatNotSupported",
            Self::OperationNotSupported => "kAudioConverterErr_OperationNotSupported",
            Self::PropertyNotSupported => "kAudioConverterErr_PropertyNotSupported",
            Self::InvalidInputSize => "kAudioConverterErr_InvalidInputSize",
            Self::InvalidOutputSize => "kAudioConverterErr_InvalidOutputSize",
            Self::Unspecified => "kAudioConverterErr_UnspecifiedError",
            Self::BadPropertySize => "kAudioConverterErr_BadPropertySizeError",
            Self::RequiresPacketDescriptions => {
                "kAudioConverterErr_RequiresPacketDescriptionsError"
            }
            Self::InputSampleRateOutOfRange => "kAudioConverterErr_InputSampleRateOutOfRange",
            Self::OutputSampleRateOutOfRange => "kAudioConverterErr_OutputSampleRateOutOfRange",
            Self::HardwareInUse => "kAudioConverterErr_HardwareInUse",
            Self::NoHardwarePermission => "kAudioConverterErr_NoHardwarePermission",
            Self::UnknownProperty => "kAudioCodecUnknownPropertyError",
            Self::IllegalOperation => "kAudioCodecIllegalOperationError",
            Self::UnsupportedFormat => "kAudioCodecUnsupportedFormatError",
            Self::StateError => "kAudioCodecStateError",
            Self::NotEnoughBufferSpace => "kAudioCodecNotEnoughBufferSpaceError",
            Self::BadData => "kAudioCodecBadDataError",
            Self::UnknownFormat => "kAudioFormatUnknownFormatError",
            Self::BadSpecifierSize => "kAudioFormatBadSpecifierSizeError",
            Self::BadFilePath => "kAudio_BadFilePathError",
            Self::Unimplemented => "kAudio_UnimplementedError",
            Self::FileNotFound => "kAudio_FileNotFoundError",
            Self::FilePermission => "kAudio_FilePermissionError",
            Self::TooManyFilesOpen => "kAudio_TooManyFilesOpenError",
            Self::ParamError => "kAudio_ParamError",
            Self::MemFull => "kAudio_MemFullError",
            Self::Unknown(_) => return None,
        })
    }

    /// The coarse semantic bucket ([`StatusKind`]) for the code.
    pub fn kind(self) -> StatusKind {
        match self {
            Self::Ok => StatusKind::Ok,
            Self::FormatNotSupported
            | Self::OperationNotSupported
            | Self::PropertyNotSupported
            | Self::InputSampleRateOutOfRange
            | Self::OutputSampleRateOutOfRange
            | Self::UnknownProperty
            | Self::UnsupportedFormat
            | Self::UnknownFormat
            | Self::NoHardwarePermission
            | Self::Unimplemented => StatusKind::Unsupported,
            Self::BadData => StatusKind::InvalidData,
            Self::HardwareInUse | Self::NotEnoughBufferSpace | Self::MemFull => {
                StatusKind::ResourceExhausted
            }
            Self::InvalidInputSize
            | Self::InvalidOutputSize
            | Self::BadPropertySize
            | Self::RequiresPacketDescriptions
            | Self::IllegalOperation
            | Self::StateError
            | Self::BadSpecifierSize
            | Self::BadFilePath
            | Self::FileNotFound
            | Self::FilePermission
            | Self::TooManyFilesOpen
            | Self::ParamError => StatusKind::Usage,
            Self::Unspecified | Self::Unknown(_) => StatusKind::Other,
        }
    }

    /// `true` for `noErr`.
    pub fn is_ok(self) -> bool {
        matches!(self, Self::Ok)
    }
}

impl std::fmt::Display for AtStatus {
    /// Renders `name ('fourcc' / raw)` for named FourCC codes,
    /// `name (raw)` for named small-integer codes, `OSStatus 'fourcc'
    /// (raw)` for unrecognised printable FourCCs, and `OSStatus raw`
    /// otherwise — so a log line always carries both the human name
    /// (when known) and the greppable integer.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let raw = self.as_raw();
        let printable = raw > 0
            && raw
                .to_be_bytes()
                .iter()
                .all(|&b| b.is_ascii_graphic() || b == b' ');
        let tag = || -> String {
            raw.to_be_bytes()
                .iter()
                .map(|&b| b as char)
                .collect::<String>()
        };
        match (self.name(), printable) {
            (Some(name), true) => write!(f, "{name} ('{}' / {raw})", tag()),
            (Some(name), false) => write!(f, "{name} ({raw})"),
            (None, true) => write!(f, "OSStatus '{}' ({raw})", tag()),
            (None, false) => write!(f, "OSStatus {raw}"),
        }
    }
}

/// A failed bridge operation: either the framework could not be
/// loaded at all, or a named AudioToolbox call returned a non-zero
/// `OSStatus`.
///
/// This is the error type of the feature-independent bridge surface
/// (the [`Converter`](crate::converter::Converter) wrapper and the
/// global-format query helpers) — it depends only on `std`, so
/// `default-features = false` consumers get typed errors too. Under
/// the `registry` feature it converts into `oxideav_core::Error` via
/// `From`, following the same [`StatusKind`] mapping as
/// [`status_error`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AtError {
    /// The AudioToolbox framework could not be dlopen-ed (older
    /// macOS, sandbox without the framework, non-mac host). Carries
    /// the loader's message.
    FrameworkUnavailable(String),
    /// A named AudioToolbox call failed with the given status.
    Os {
        /// The operation that failed, e.g. `"AudioConverterNew"`.
        op: &'static str,
        /// The classified status code.
        status: AtStatus,
    },
}

impl AtError {
    /// The classified status for `Os` failures, `None` for
    /// `FrameworkUnavailable`.
    pub fn status(&self) -> Option<AtStatus> {
        match self {
            Self::FrameworkUnavailable(_) => None,
            Self::Os { status, .. } => Some(*status),
        }
    }
}

impl std::fmt::Display for AtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FrameworkUnavailable(msg) => write!(f, "AudioToolbox unavailable: {msg}"),
            Self::Os { op, status } => write!(f, "{op} failed: {status}"),
        }
    }
}

impl std::error::Error for AtError {}

/// `FrameworkUnavailable` maps to `Error::Unsupported` (the
/// capability is absent on this system; the registry falls back to
/// the pure-Rust impl); `Os` failures route through [`status_error`].
#[cfg(feature = "registry")]
impl From<AtError> for oxideav_core::Error {
    fn from(e: AtError) -> Self {
        match e {
            AtError::FrameworkUnavailable(_) => oxideav_core::Error::unsupported(e.to_string()),
            AtError::Os { op, status } => status_error(op, status.as_raw()),
        }
    }
}

/// Convert an `(operation, raw status)` failure pair into the
/// matching [`oxideav_core::Error`] variant, per the
/// [`StatusKind`] bucket:
///
/// * [`StatusKind::Unsupported`] → `Error::Unsupported` (registry
///   falls back to the next-priority impl),
/// * [`StatusKind::InvalidData`] → `Error::InvalidData`,
/// * [`StatusKind::ResourceExhausted`] → `Error::ResourceExhausted`,
/// * [`StatusKind::Usage`] / [`StatusKind::Other`] / `Ok` →
///   `Error::Other` (an `Ok` status handed to an error constructor is
///   itself a bridge bug; the message preserves the whole story).
///
/// The message is `"{op} failed: {status}"` with the status rendered
/// through [`AtStatus`]'s `Display`, so callers keep the exact
/// context string they used before while gaining the decoded name.
#[cfg(feature = "registry")]
pub fn status_error(op: &str, raw: OSStatus) -> oxideav_core::Error {
    use oxideav_core::Error;
    let status = AtStatus::from_raw(raw);
    let msg = format!("{op} failed: {status}");
    match status.kind() {
        StatusKind::Unsupported => Error::unsupported(msg),
        StatusKind::InvalidData => Error::invalid(msg),
        StatusKind::ResourceExhausted => Error::resource_exhausted(msg),
        StatusKind::Ok | StatusKind::Usage | StatusKind::Other => Error::other(msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every named variant with its raw value, platform name, and kind.
    fn wired() -> Vec<(OSStatus, AtStatus, &'static str, StatusKind)> {
        use AtStatus as S;
        use StatusKind as K;
        vec![
            (0, S::Ok, "noErr", K::Ok),
            (
                fourcc(b"fmt?"),
                S::FormatNotSupported,
                "kAudioConverterErr_FormatNotSupported",
                K::Unsupported,
            ),
            (
                fourcc(b"op??"),
                S::OperationNotSupported,
                "kAudioConverterErr_OperationNotSupported",
                K::Unsupported,
            ),
            (
                fourcc(b"prop"),
                S::PropertyNotSupported,
                "kAudioConverterErr_PropertyNotSupported",
                K::Unsupported,
            ),
            (
                fourcc(b"insz"),
                S::InvalidInputSize,
                "kAudioConverterErr_InvalidInputSize",
                K::Usage,
            ),
            (
                fourcc(b"otsz"),
                S::InvalidOutputSize,
                "kAudioConverterErr_InvalidOutputSize",
                K::Usage,
            ),
            (
                fourcc(b"what"),
                S::Unspecified,
                "kAudioConverterErr_UnspecifiedError",
                K::Other,
            ),
            (
                fourcc(b"!siz"),
                S::BadPropertySize,
                "kAudioConverterErr_BadPropertySizeError",
                K::Usage,
            ),
            (
                fourcc(b"!pkd"),
                S::RequiresPacketDescriptions,
                "kAudioConverterErr_RequiresPacketDescriptionsError",
                K::Usage,
            ),
            (
                fourcc(b"!isr"),
                S::InputSampleRateOutOfRange,
                "kAudioConverterErr_InputSampleRateOutOfRange",
                K::Unsupported,
            ),
            (
                fourcc(b"!osr"),
                S::OutputSampleRateOutOfRange,
                "kAudioConverterErr_OutputSampleRateOutOfRange",
                K::Unsupported,
            ),
            (
                fourcc(b"hwiu"),
                S::HardwareInUse,
                "kAudioConverterErr_HardwareInUse",
                K::ResourceExhausted,
            ),
            (
                fourcc(b"perm"),
                S::NoHardwarePermission,
                "kAudioConverterErr_NoHardwarePermission",
                K::Unsupported,
            ),
            (
                fourcc(b"who?"),
                S::UnknownProperty,
                "kAudioCodecUnknownPropertyError",
                K::Unsupported,
            ),
            (
                fourcc(b"nope"),
                S::IllegalOperation,
                "kAudioCodecIllegalOperationError",
                K::Usage,
            ),
            (
                fourcc(b"!dat"),
                S::UnsupportedFormat,
                "kAudioCodecUnsupportedFormatError",
                K::Unsupported,
            ),
            (
                fourcc(b"!stt"),
                S::StateError,
                "kAudioCodecStateError",
                K::Usage,
            ),
            (
                fourcc(b"!buf"),
                S::NotEnoughBufferSpace,
                "kAudioCodecNotEnoughBufferSpaceError",
                K::ResourceExhausted,
            ),
            (
                fourcc(b"bada"),
                S::BadData,
                "kAudioCodecBadDataError",
                K::InvalidData,
            ),
            (
                fourcc(b"!fmt"),
                S::UnknownFormat,
                "kAudioFormatUnknownFormatError",
                K::Unsupported,
            ),
            (
                fourcc(b"!spc"),
                S::BadSpecifierSize,
                "kAudioFormatBadSpecifierSizeError",
                K::Usage,
            ),
            (
                fourcc(b"!pth"),
                S::BadFilePath,
                "kAudio_BadFilePathError",
                K::Usage,
            ),
            (
                -4,
                S::Unimplemented,
                "kAudio_UnimplementedError",
                K::Unsupported,
            ),
            (-43, S::FileNotFound, "kAudio_FileNotFoundError", K::Usage),
            (
                -54,
                S::FilePermission,
                "kAudio_FilePermissionError",
                K::Usage,
            ),
            (
                -42,
                S::TooManyFilesOpen,
                "kAudio_TooManyFilesOpenError",
                K::Usage,
            ),
            (-50, S::ParamError, "kAudio_ParamError", K::Usage),
            (
                -108,
                S::MemFull,
                "kAudio_MemFullError",
                K::ResourceExhausted,
            ),
        ]
    }

    #[test]
    fn classify_round_trip_name_and_kind_for_every_wired_code() {
        for (raw, variant, name, kind) in wired() {
            let got = AtStatus::from_raw(raw);
            assert_eq!(got, variant, "raw {raw} should classify to {variant:?}");
            assert_eq!(got.as_raw(), raw, "{variant:?} should round-trip raw");
            assert_eq!(got.name(), Some(name), "{variant:?} name");
            assert_eq!(got.kind(), kind, "{variant:?} kind");
        }
    }

    #[test]
    fn wired_raw_values_are_distinct() {
        // The classifier is only total-and-unambiguous if no two named
        // variants claim the same raw value.
        let mut raws: Vec<OSStatus> = wired().into_iter().map(|(raw, ..)| raw).collect();
        raws.sort_unstable();
        let before = raws.len();
        raws.dedup();
        assert_eq!(raws.len(), before, "duplicate raw OSStatus in wired set");
    }

    #[test]
    fn unknown_round_trips_and_reports_other() {
        let raw = fourcc(b"zzzz");
        let s = AtStatus::from_raw(raw);
        assert_eq!(s, AtStatus::Unknown(raw));
        assert_eq!(s.as_raw(), raw);
        assert_eq!(s.name(), None);
        assert_eq!(s.kind(), StatusKind::Other);
        assert!(!s.is_ok());
    }

    #[test]
    fn ok_is_ok_and_nothing_else_is() {
        assert!(AtStatus::from_raw(0).is_ok());
        for (raw, ..) in wired() {
            if raw != 0 {
                assert!(!AtStatus::from_raw(raw).is_ok(), "raw {raw} must not be ok");
            }
        }
    }

    #[test]
    fn display_renders_name_and_fourcc() {
        // Named FourCC code: name + tag + integer.
        assert_eq!(
            AtStatus::FormatNotSupported.to_string(),
            "kAudioConverterErr_FormatNotSupported ('fmt?' / 1718449215)"
        );
        // Named small-integer code: name + integer, no tag.
        assert_eq!(AtStatus::ParamError.to_string(), "kAudio_ParamError (-50)");
        // Unknown printable FourCC: tag + integer.
        assert_eq!(
            AtStatus::Unknown(fourcc(b"zzzz")).to_string(),
            format!("OSStatus 'zzzz' ({})", fourcc(b"zzzz"))
        );
        // Unknown non-printable: bare integer.
        assert_eq!(AtStatus::Unknown(-9999).to_string(), "OSStatus -9999");
    }

    #[test]
    fn known_bad_data_code_matches_documented_integer() {
        // 'bada' = 1650549857 — the value HE-AAC decode rejections
        // historically surfaced as a bare integer in this crate's
        // error strings. Pin the packed value so the decoded name can
        // be trusted in logs.
        assert_eq!(K_AUDIO_CODEC_BAD_DATA, 1650549857);
        assert_eq!(
            AtStatus::from_raw(1650549857).to_string(),
            "kAudioCodecBadDataError ('bada' / 1650549857)"
        );
    }

    #[cfg(feature = "registry")]
    #[test]
    fn status_error_maps_kind_to_core_error_variant() {
        use oxideav_core::Error;
        let e = status_error("AudioConverterNew", fourcc(b"fmt?"));
        assert!(
            matches!(e, Error::Unsupported(_)),
            "fmt? → Unsupported: {e:?}"
        );
        assert_eq!(
            e.to_string(),
            "unsupported: AudioConverterNew failed: \
             kAudioConverterErr_FormatNotSupported ('fmt?' / 1718449215)"
        );

        let e = status_error("FillComplexBuffer", fourcc(b"bada"));
        assert!(
            matches!(e, Error::InvalidData(_)),
            "bada → InvalidData: {e:?}"
        );

        let e = status_error("AudioConverterNew", fourcc(b"hwiu"));
        assert!(
            matches!(e, Error::ResourceExhausted(_)),
            "hwiu → ResourceExhausted: {e:?}"
        );

        let e = status_error("SetProperty", -50);
        assert!(matches!(e, Error::Other(_)), "-50 → Other: {e:?}");

        let e = status_error("SetProperty", fourcc(b"zzzz"));
        assert!(matches!(e, Error::Other(_)), "unknown → Other: {e:?}");
    }
}
