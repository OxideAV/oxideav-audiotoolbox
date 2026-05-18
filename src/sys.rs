//! Runtime-loaded AudioToolbox + supporting framework handles.
//!
//! Loaded once via `OnceLock` on first use and cached for the process
//! lifetime. If any framework fails to dlopen the cache stores the
//! error so subsequent calls don't repeatedly hammer dyld.

use libloading::Library;
use std::sync::OnceLock;

// ──────────────────────────── CoreAudio types ────────────────────────────

/// OSStatus: the 32-bit signed return type used by all CoreAudio C APIs.
pub type OSStatus = i32;

/// No-error sentinel.
pub const NO_ERR: OSStatus = 0;

/// Opaque AudioConverter handle.
#[repr(C)]
pub struct OpaqueAudioConverter(u8, std::marker::PhantomData<*mut ()>);

/// Pointer alias.
pub type AudioConverterRef = *mut OpaqueAudioConverter;

/// kAudioFormatLinearPCM
pub const K_AUDIO_FORMAT_LINEAR_PCM: u32 = 0x6C70636D; // 'lpcm'
/// kAudioFormatMPEG4AAC
pub const K_AUDIO_FORMAT_MPEG4_AAC: u32 = 0x61616320; // 'aac '
/// kAudioFormatAppleLossless
pub const K_AUDIO_FORMAT_APPLE_LOSSLESS: u32 = 0x616C6163; // 'alac'

/// kAudioFormatFlagIsFloat
pub const K_AF_FLAG_IS_FLOAT: u32 = 1 << 0;
/// kAudioFormatFlagIsPacked  (samples fill every bit of the word)
pub const K_AF_FLAG_IS_PACKED: u32 = 1 << 3;
/// kAudioFormatFlagIsSignedInteger
pub const K_AF_FLAG_IS_SIGNED_INTEGER: u32 = 1 << 2;
/// kAudioFormatFlagIsNonInterleaved
pub const K_AF_FLAG_IS_NON_INTERLEAVED: u32 = 1 << 5;

/// kAudioFormatFlagsAppleLossless16BitSourceData — used in `format_flags`
/// of an ALAC ASBD to declare the underlying-PCM bit depth so that the
/// converter can allocate the right state. AT defines four "source data"
/// flag values (16/20/24/32) numbered 1..=4 in the framework headers.
pub const K_AF_APPLE_LOSSLESS_16_BIT: u32 = 1;
pub const K_AF_APPLE_LOSSLESS_20_BIT: u32 = 2;
pub const K_AF_APPLE_LOSSLESS_24_BIT: u32 = 3;
pub const K_AF_APPLE_LOSSLESS_32_BIT: u32 = 4;

/// kAudioConverterEncodeBitRate
pub const K_AUDIO_CONVERTER_ENCODE_BIT_RATE: u32 = 0x62726174; // 'brat'

/// kAudioConverterPropertyMaximumOutputPacketSize
pub const K_AUDIO_CONVERTER_MAX_OUTPUT_PACKET_SIZE: u32 = 0x786F7073; // 'xops'

/// kAudioConverterCurrentInputStreamDescription
pub const K_AUDIO_CONVERTER_CURRENT_INPUT_SD: u32 = 0x61637364; // 'acsd'

/// kAudioConverterDecompressionMagicCookie — set on a decoder converter
/// before its first decode call.
pub const K_AUDIO_CONVERTER_DECOMPRESSION_MAGIC_COOKIE: u32 = 0x646D6763; // 'dmgc'

/// kAudioConverterCompressionMagicCookie — read from an encoder converter
/// after it has been configured. The value is the encoder-vended magic
/// cookie (for ALAC: 24 or 48 bytes).
pub const K_AUDIO_CONVERTER_COMPRESSION_MAGIC_COOKIE: u32 = 0x636D6763; // 'cmgc'

/// AudioStreamBasicDescription — the core format descriptor.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioStreamBasicDescription {
    pub sample_rate: f64,
    pub format_id: u32,
    pub format_flags: u32,
    pub bytes_per_packet: u32,
    pub frames_per_packet: u32,
    pub bytes_per_frame: u32,
    pub channels_per_frame: u32,
    pub bits_per_channel: u32,
    pub reserved: u32,
}

impl AudioStreamBasicDescription {
    /// Construct an ASBD for 32-bit float interleaved PCM.
    pub fn pcm_float32(sample_rate: f64, channels: u32) -> Self {
        let bps = 4u32; // bytes per sample
        Self {
            sample_rate,
            format_id: K_AUDIO_FORMAT_LINEAR_PCM,
            format_flags: K_AF_FLAG_IS_FLOAT | K_AF_FLAG_IS_PACKED,
            bytes_per_packet: bps * channels,
            frames_per_packet: 1,
            bytes_per_frame: bps * channels,
            channels_per_frame: channels,
            bits_per_channel: 32,
            reserved: 0,
        }
    }

    /// Construct an ASBD for 16-bit signed integer interleaved PCM.
    pub fn pcm_s16(sample_rate: f64, channels: u32) -> Self {
        let bps = 2u32;
        Self {
            sample_rate,
            format_id: K_AUDIO_FORMAT_LINEAR_PCM,
            format_flags: K_AF_FLAG_IS_SIGNED_INTEGER | K_AF_FLAG_IS_PACKED,
            bytes_per_packet: bps * channels,
            frames_per_packet: 1,
            bytes_per_frame: bps * channels,
            channels_per_frame: channels,
            bits_per_channel: 16,
            reserved: 0,
        }
    }

    /// Construct an ASBD for MPEG-4 AAC (compressed; no layout enforced).
    pub fn mpeg4_aac(sample_rate: f64, channels: u32) -> Self {
        Self {
            sample_rate,
            format_id: K_AUDIO_FORMAT_MPEG4_AAC,
            format_flags: 0,
            bytes_per_packet: 0,     // variable
            frames_per_packet: 1024, // AAC LC
            bytes_per_frame: 0,
            channels_per_frame: channels,
            bits_per_channel: 0,
            reserved: 0,
        }
    }

    /// Construct an ASBD for Apple Lossless (compressed).
    ///
    /// `bit_depth_flag` is one of `K_AF_APPLE_LOSSLESS_*` and tells
    /// AudioConverter the underlying source PCM bit depth (typically 16
    /// or 24). `frames_per_packet` defaults to 4096 (the ALAC encoder's
    /// canonical packet size; see ALACMagicCookieDescription).
    pub fn apple_lossless(
        sample_rate: f64,
        channels: u32,
        bit_depth_flag: u32,
        frames_per_packet: u32,
    ) -> Self {
        Self {
            sample_rate,
            format_id: K_AUDIO_FORMAT_APPLE_LOSSLESS,
            format_flags: bit_depth_flag,
            bytes_per_packet: 0, // variable, decided by entropy coder
            frames_per_packet,
            bytes_per_frame: 0,
            channels_per_frame: channels,
            bits_per_channel: 0, // ALAC sets this to 0 in the compressed ASBD
            reserved: 0,
        }
    }
}

/// AudioBuffer — a single buffer descriptor used in AudioBufferList.
#[repr(C)]
pub struct AudioBuffer {
    pub number_channels: u32,
    pub data_byte_size: u32,
    pub data: *mut u8,
}

/// AudioBufferList with one buffer slot (the most common case for
/// interleaved PCM and AAC compressed data).
#[repr(C)]
pub struct AudioBufferList1 {
    pub number_buffers: u32,
    pub buffers: [AudioBuffer; 1],
}

/// AudioStreamPacketDescription for compressed data.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct AudioStreamPacketDescription {
    pub start_offset: i64,
    pub variable_frames_in_packet: u32,
    pub data_byte_size: u32,
}

// ─────────────────────────────── Framework ───────────────────────────────

/// Handles to the frameworks the AT bridge needs.
pub struct Framework {
    pub audio_toolbox: Library,
    pub core_foundation: Library,
}

/// Process-wide cache. `OnceLock` so concurrent first calls collapse
/// to a single load.
static FRAMEWORK: OnceLock<Result<Framework, String>> = OnceLock::new();

/// Get (or load) the framework handles. Returns the cached `Err` if a
/// previous load attempt failed.
pub fn framework() -> Result<&'static Framework, &'static str> {
    FRAMEWORK.get_or_init(load).as_ref().map_err(|s| s.as_str())
}

fn load() -> Result<Framework, String> {
    let audio_toolbox = open("/System/Library/Frameworks/AudioToolbox.framework/AudioToolbox")?;
    let core_foundation =
        open("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")?;
    Ok(Framework {
        audio_toolbox,
        core_foundation,
    })
}

fn open(path: &str) -> Result<Library, String> {
    // SAFETY: dlopen on a fixed system framework path with no init
    // callbacks; equivalent to a normal program startup load.
    unsafe { Library::new(path) }.map_err(|e| format!("dlopen {path}: {e}"))
}

// ─────────────────────────── Typed FFI wrappers ──────────────────────────

/// Type alias for the `AudioConverterComplexInputDataProc` callback signature.
/// Called by `AudioConverterFillComplexBuffer` when it needs input data.
///
/// Signature (C):
/// ```c
/// OSStatus callback(
///     AudioConverterRef              inAudioConverter,
///     UInt32                        *ioNumberDataPackets,
///     AudioBufferList               *ioData,
///     AudioStreamPacketDescription **outDataPacketDescription,
///     void                          *inUserData
/// );
/// ```
pub type AudioConverterInputDataProc = unsafe extern "C" fn(
    converter: AudioConverterRef,
    io_number_data_packets: *mut u32,
    io_data: *mut AudioBufferList1,
    out_packet_desc: *mut *mut AudioStreamPacketDescription,
    user_data: *mut std::ffi::c_void,
) -> OSStatus;

/// Thin wrapper that resolves and calls `AudioConverterNew`.
///
/// # Safety
/// All pointer arguments must satisfy the documented CoreAudio conventions.
pub unsafe fn audio_converter_new(
    fw: &Framework,
    in_source_format: *const AudioStreamBasicDescription,
    in_dest_format: *const AudioStreamBasicDescription,
    out_audio_converter: *mut AudioConverterRef,
) -> OSStatus {
    type Fn = unsafe extern "C" fn(
        *const AudioStreamBasicDescription,
        *const AudioStreamBasicDescription,
        *mut AudioConverterRef,
    ) -> OSStatus;
    let f: libloading::Symbol<Fn> = fw
        .audio_toolbox
        .get(b"AudioConverterNew\0")
        .expect("AudioConverterNew not found");
    f(in_source_format, in_dest_format, out_audio_converter)
}

/// Thin wrapper for `AudioConverterDispose`.
///
/// # Safety
/// `converter` must be a valid AudioConverterRef obtained from `AudioConverterNew`.
pub unsafe fn audio_converter_dispose(fw: &Framework, converter: AudioConverterRef) -> OSStatus {
    type Fn = unsafe extern "C" fn(AudioConverterRef) -> OSStatus;
    let f: libloading::Symbol<Fn> = fw
        .audio_toolbox
        .get(b"AudioConverterDispose\0")
        .expect("AudioConverterDispose not found");
    f(converter)
}

/// Thin wrapper for `AudioConverterReset`.
///
/// # Safety
/// `converter` must be a valid AudioConverterRef.
pub unsafe fn audio_converter_reset(fw: &Framework, converter: AudioConverterRef) -> OSStatus {
    type Fn = unsafe extern "C" fn(AudioConverterRef) -> OSStatus;
    let f: libloading::Symbol<Fn> = fw
        .audio_toolbox
        .get(b"AudioConverterReset\0")
        .expect("AudioConverterReset not found");
    f(converter)
}

/// Thin wrapper for `AudioConverterSetProperty`.
///
/// # Safety
/// Caller must ensure `in_data` points to a valid value of size `in_data_size`
/// for the given property selector.
pub unsafe fn audio_converter_set_property(
    fw: &Framework,
    converter: AudioConverterRef,
    in_property_id: u32,
    in_data_size: u32,
    in_data: *const std::ffi::c_void,
) -> OSStatus {
    type Fn =
        unsafe extern "C" fn(AudioConverterRef, u32, u32, *const std::ffi::c_void) -> OSStatus;
    let f: libloading::Symbol<Fn> = fw
        .audio_toolbox
        .get(b"AudioConverterSetProperty\0")
        .expect("AudioConverterSetProperty not found");
    f(converter, in_property_id, in_data_size, in_data)
}

/// Thin wrapper for `AudioConverterGetProperty`.
///
/// # Safety
/// `io_data_size` in/out must match the property size for `in_property_id`.
pub unsafe fn audio_converter_get_property(
    fw: &Framework,
    converter: AudioConverterRef,
    in_property_id: u32,
    io_data_size: *mut u32,
    out_data: *mut std::ffi::c_void,
) -> OSStatus {
    type Fn =
        unsafe extern "C" fn(AudioConverterRef, u32, *mut u32, *mut std::ffi::c_void) -> OSStatus;
    let f: libloading::Symbol<Fn> = fw
        .audio_toolbox
        .get(b"AudioConverterGetProperty\0")
        .expect("AudioConverterGetProperty not found");
    f(converter, in_property_id, io_data_size, out_data)
}

/// Thin wrapper for `AudioConverterGetPropertyInfo`.
///
/// # Safety
/// Standard CoreAudio safety requirements.
#[allow(dead_code)]
pub unsafe fn audio_converter_get_property_info(
    fw: &Framework,
    converter: AudioConverterRef,
    in_property_id: u32,
    out_size: *mut u32,
    out_writable: *mut u8,
) -> OSStatus {
    type Fn = unsafe extern "C" fn(AudioConverterRef, u32, *mut u32, *mut u8) -> OSStatus;
    let f: libloading::Symbol<Fn> = fw
        .audio_toolbox
        .get(b"AudioConverterGetPropertyInfo\0")
        .expect("AudioConverterGetPropertyInfo not found");
    f(converter, in_property_id, out_size, out_writable)
}

/// Thin wrapper for `AudioConverterFillComplexBuffer`.
///
/// # Safety
/// All pointers must satisfy the documented CoreAudio conventions.
pub unsafe fn audio_converter_fill_complex_buffer(
    fw: &Framework,
    converter: AudioConverterRef,
    in_input_data_proc: AudioConverterInputDataProc,
    in_input_data_proc_user_data: *mut std::ffi::c_void,
    io_output_data_packet_size: *mut u32,
    out_output_data: *mut AudioBufferList1,
    out_packet_description: *mut AudioStreamPacketDescription,
) -> OSStatus {
    type Fn = unsafe extern "C" fn(
        AudioConverterRef,
        AudioConverterInputDataProc,
        *mut std::ffi::c_void,
        *mut u32,
        *mut AudioBufferList1,
        *mut AudioStreamPacketDescription,
    ) -> OSStatus;
    let f: libloading::Symbol<Fn> = fw
        .audio_toolbox
        .get(b"AudioConverterFillComplexBuffer\0")
        .expect("AudioConverterFillComplexBuffer not found");
    f(
        converter,
        in_input_data_proc,
        in_input_data_proc_user_data,
        io_output_data_packet_size,
        out_output_data,
        out_packet_description,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: every framework on this Mac loads cleanly + a
    /// stable AT entry point resolves.
    #[test]
    fn frameworks_load() {
        let fw = framework().expect("framework load");
        let _: libloading::Symbol<unsafe extern "C" fn()> = unsafe {
            fw.audio_toolbox
                .get(b"AudioConverterNew\0")
                .expect("AudioConverterNew symbol")
        };
        let _: libloading::Symbol<unsafe extern "C" fn()> = unsafe {
            fw.core_foundation
                .get(b"CFRetain\0")
                .expect("CFRetain symbol")
        };
    }

    #[test]
    fn asbd_pcm_float32_geometry() {
        let a = AudioStreamBasicDescription::pcm_float32(48_000.0, 2);
        assert_eq!(a.format_id, K_AUDIO_FORMAT_LINEAR_PCM);
        assert_eq!(a.bits_per_channel, 32);
        assert_eq!(a.bytes_per_frame, 8); // 2 channels × 4 bytes
        assert_eq!(a.frames_per_packet, 1);
    }
}
