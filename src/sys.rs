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
/// kAudioFormatMPEG4AAC_HE  — HE-AAC v1 (AAC LC + SBR).
pub const K_AUDIO_FORMAT_MPEG4_AAC_HE: u32 = 0x61616368; // 'aach'
/// kAudioFormatMPEG4AAC_HE_V2  — HE-AAC v2 (AAC LC + SBR + Parametric Stereo).
pub const K_AUDIO_FORMAT_MPEG4_AAC_HE_V2: u32 = 0x61616370; // 'aacp'
/// kAudioFormatMPEG4AAC_LD  — AAC Low Delay (AOT 23). 512-sample frames.
pub const K_AUDIO_FORMAT_MPEG4_AAC_LD: u32 = 0x6161636C; // 'aacl'
/// kAudioFormatMPEG4AAC_ELD  — AAC Enhanced Low Delay (AOT 39). 512-sample frames.
pub const K_AUDIO_FORMAT_MPEG4_AAC_ELD: u32 = 0x61616365; // 'aace'
/// kAudioFormatAppleLossless
pub const K_AUDIO_FORMAT_APPLE_LOSSLESS: u32 = 0x616C6163; // 'alac'
/// kAudioFormatiLBC — narrow-band speech codec (RFC 3951). Apple's
/// AudioToolbox identifier is the FourCC `'ilbc'`. iLBC is fixed at
/// 8 kHz mono; the mode selector (20 ms vs 30 ms frame size) travels
/// in the ASBD's `frames_per_packet` field (160 vs 240) — there is no
/// magic cookie.
pub const K_AUDIO_FORMAT_ILBC: u32 = 0x696C6263; // 'ilbc'

/// kAudioFormatAMR — Adaptive Multi-Rate Narrowband speech codec
/// (3GPP TS 26.071 / RFC 4867). Apple's AudioToolbox identifier is
/// the FourCC `'samr'`. AMR-NB is fixed at 8 kHz mono with a 20 ms
/// analysis window (160 PCM samples per packet). The on-wire packet
/// is variable-size: the first byte (TOC) selects one of 8 speech
/// modes plus SID + NO_DATA, and the per-mode compressed byte count
/// varies — AudioConverter reads the actual size from the
/// AudioStreamPacketDescription supplied by the input callback.
pub const K_AUDIO_FORMAT_AMR: u32 = 0x73616D72; // 'samr'

/// kAudioFormatMPEGLayer3 — MPEG-1/2 Audio Layer III (MP3). Apple's
/// AudioToolbox identifier is the FourCC `'.mp3'`. AT exposes MP3 as a
/// **decode-only** target via `AudioConverterRef`; there is no paired
/// AT MP3 encoder (the macOS encoder side dropped MP3 long ago and was
/// never re-added). Each MP3 frame begins with a 32-bit header (sync
/// 0xFFE in the top 12 bits, followed by version / layer / bitrate /
/// sample-rate / padding / channel-mode bits) and the per-frame
/// compressed length is derived from the header. MP3 PCM block sizes
/// are codec-defined: 1152 samples per frame for MPEG-1 Layer III and
/// 576 samples per frame for MPEG-2 / MPEG-2.5 Layer III (the LSF
/// variant — Low Sampling Frequency — uses a single granule per
/// frame). AudioConverter handles the per-frame block-size selection
/// internally; the input callback only needs to supply each frame as a
/// single packet with a packet description carrying the byte size and
/// `variable_frames_in_packet = 0` (compressed packets vend a fixed
/// number of PCM frames driven by the header).
pub const K_AUDIO_FORMAT_MPEG_LAYER_3: u32 = 0x2E6D7033; // '.mp3'

/// kAudioFormatMPEGLayer2 — MPEG-1/2 Audio Layer II (MP2). FourCC
/// `'.mp2'`. Also a **decode-only** target on AT. Layer II frames are
/// always 1152 PCM samples (MPEG-1) or 1152 / 576 (MPEG-2 LSF) and the
/// header layout matches Layer III except the layer bits report `10`.
pub const K_AUDIO_FORMAT_MPEG_LAYER_2: u32 = 0x2E6D7032; // '.mp2'

/// kAudioFormatMPEGLayer1 — MPEG-1/2 Audio Layer I (MP1). FourCC
/// `'.mp1'`. Decode-only via AT. Layer I frames are always 384 PCM
/// samples per channel.
pub const K_AUDIO_FORMAT_MPEG_LAYER_1: u32 = 0x2E6D7031; // '.mp1'

/// kAudioFormatAMR_WB — Adaptive Multi-Rate Wideband speech codec
/// (3GPP TS 26.171 / TS 26.201 / RFC 4867). Apple's AudioToolbox
/// identifier is the FourCC `'sawb'`. AMR-WB is fixed at 16 kHz mono
/// with a 20 ms analysis window (320 PCM samples per packet). Like
/// AMR-NB the on-wire packet is variable-size: the first byte (TOC)
/// selects one of 9 speech modes (0..=8) plus SID (9) + NO_DATA (15),
/// and the per-mode compressed byte count varies (17, 23, 32, 36, 40,
/// 46, 50, 58, 60 for the speech modes, 6 for SID, 1 for NO_DATA per
/// RFC 4867 §5.3). AudioConverter reads the actual size from the
/// AudioStreamPacketDescription supplied by the input callback.
pub const K_AUDIO_FORMAT_AMR_WB: u32 = 0x73617762; // 'sawb'

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

    /// Construct an ASBD for MPEG-4 HE-AAC (LC + SBR).
    ///
    /// `sample_rate` is the **output** sample rate (the SBR-doubled rate).
    /// HE-AAC's framesPerPacket is `2048` because SBR doubles the
    /// underlying 1024-sample AAC LC block.
    pub fn mpeg4_aac_he(sample_rate: f64, channels: u32) -> Self {
        Self {
            sample_rate,
            format_id: K_AUDIO_FORMAT_MPEG4_AAC_HE,
            format_flags: 0,
            bytes_per_packet: 0,
            frames_per_packet: 2048,
            bytes_per_frame: 0,
            channels_per_frame: channels,
            bits_per_channel: 0,
            reserved: 0,
        }
    }

    /// Construct an ASBD for MPEG-4 HE-AAC v2 (LC + SBR + Parametric Stereo).
    ///
    /// `channels` must be `2` — HE-AAC v2 only makes sense for stereo
    /// because PS encodes a mono down-mix plus parametric side
    /// information. `frames_per_packet` is again 2048.
    pub fn mpeg4_aac_he_v2(sample_rate: f64, channels: u32) -> Self {
        Self {
            sample_rate,
            format_id: K_AUDIO_FORMAT_MPEG4_AAC_HE_V2,
            format_flags: 0,
            bytes_per_packet: 0,
            frames_per_packet: 2048,
            bytes_per_frame: 0,
            channels_per_frame: channels,
            bits_per_channel: 0,
            reserved: 0,
        }
    }

    /// Construct an ASBD for MPEG-4 AAC Low Delay (AOT 23).
    ///
    /// AAC-LD targets two-way conferencing: the analysis/synthesis window
    /// is shortened so the algorithmic delay is ~20 ms at 48 kHz, against
    /// AAC LC's ~100+ ms. AudioConverter packetises LD at **512 PCM frames
    /// per packet** (no SBR doubling). There is no ADTS framing for LD —
    /// callers configure decode via the magic cookie (AudioSpecificConfig
    /// with AOT 23), exactly like HE-AAC.
    pub fn mpeg4_aac_ld(sample_rate: f64, channels: u32) -> Self {
        Self {
            sample_rate,
            format_id: K_AUDIO_FORMAT_MPEG4_AAC_LD,
            format_flags: 0,
            bytes_per_packet: 0,
            frames_per_packet: 512,
            bytes_per_frame: 0,
            channels_per_frame: channels,
            bits_per_channel: 0,
            reserved: 0,
        }
    }

    /// Construct an ASBD for MPEG-4 AAC Enhanced Low Delay (AOT 39).
    ///
    /// AAC-ELD pushes the delay lower still (down to ~15 ms) by combining
    /// the low-delay core with a delay-optimised SBR (LD-SBR). Despite the
    /// optional SBR, AudioConverter still packetises ELD at **512 PCM
    /// frames per packet** (the LD-SBR variant keeps the 512-sample core
    /// frame), so the output sample rate equals the input rate — unlike
    /// HE-AAC there is no 2× upsample at the converter boundary. Decode is
    /// magic-cookie configured (AudioSpecificConfig with AOT 39).
    pub fn mpeg4_aac_eld(sample_rate: f64, channels: u32) -> Self {
        Self {
            sample_rate,
            format_id: K_AUDIO_FORMAT_MPEG4_AAC_ELD,
            format_flags: 0,
            bytes_per_packet: 0,
            frames_per_packet: 512,
            bytes_per_frame: 0,
            channels_per_frame: channels,
            bits_per_channel: 0,
            reserved: 0,
        }
    }

    /// Construct an ASBD for iLBC (narrow-band speech).
    ///
    /// iLBC is fixed at **8 kHz mono**. Two block sizes are defined by
    /// RFC 3951:
    ///
    /// * 20 ms — 160 PCM samples per packet, 38 compressed bytes
    /// * 30 ms — 240 PCM samples per packet, 50 compressed bytes
    ///
    /// AudioConverter selects the mode from `frames_per_packet`. The
    /// `bytes_per_packet` field is set to the fixed compressed-packet
    /// size for each mode (iLBC is constant-bitrate). AT validates the
    /// geometry against both fields during `AudioConverterNew`.
    pub fn ilbc(frames_per_packet: u32) -> Self {
        let bytes_per_packet = match frames_per_packet {
            160 => 38, // 20 ms mode
            240 => 50, // 30 ms mode
            _ => 0,    // unknown mode — AT will reject the converter
        };
        Self {
            sample_rate: 8_000.0,
            format_id: K_AUDIO_FORMAT_ILBC,
            format_flags: 0,
            bytes_per_packet,
            frames_per_packet,
            bytes_per_frame: 0, // compressed, not meaningful
            channels_per_frame: 1,
            bits_per_channel: 0,
            reserved: 0,
        }
    }

    /// Construct an ASBD for AMR-NB (Adaptive Multi-Rate Narrowband).
    ///
    /// AMR-NB is fixed at **8 kHz mono** with a **20 ms analysis frame**
    /// (160 PCM samples per packet). The compressed byte count is
    /// variable — it depends on the speech mode encoded in the TOC byte
    /// of each packet — so `bytes_per_packet` is left at `0` (the AT
    /// convention for variable-rate compressed inputs). The callback
    /// supplying packets to `AudioConverterFillComplexBuffer` provides
    /// the per-packet byte count through an `AudioStreamPacketDescription`.
    pub fn amr_nb() -> Self {
        Self {
            sample_rate: 8_000.0,
            format_id: K_AUDIO_FORMAT_AMR,
            format_flags: 0,
            bytes_per_packet: 0,    // variable per mode
            frames_per_packet: 160, // 20 ms @ 8 kHz
            bytes_per_frame: 0,     // compressed, not meaningful
            channels_per_frame: 1,
            bits_per_channel: 0,
            reserved: 0,
        }
    }

    /// Construct an ASBD for AMR-WB (Adaptive Multi-Rate Wideband).
    ///
    /// AMR-WB is fixed at **16 kHz mono** with a **20 ms analysis frame**
    /// (320 PCM samples per packet). Like AMR-NB the compressed byte
    /// count varies per packet — the TOC byte selects one of 9 speech
    /// modes plus SID / NO_DATA — so `bytes_per_packet` is left at `0`
    /// and the input callback supplies the per-packet byte count through
    /// the `AudioStreamPacketDescription`.
    pub fn amr_wb() -> Self {
        Self {
            sample_rate: 16_000.0,
            format_id: K_AUDIO_FORMAT_AMR_WB,
            format_flags: 0,
            bytes_per_packet: 0,    // variable per mode
            frames_per_packet: 320, // 20 ms @ 16 kHz
            bytes_per_frame: 0,     // compressed, not meaningful
            channels_per_frame: 1,
            bits_per_channel: 0,
            reserved: 0,
        }
    }

    /// Construct an ASBD for MPEG Layer III (MP3, compressed input).
    ///
    /// MP3 is variable-rate at the packet level (each frame carries its
    /// own bitrate index in the header), so `bytes_per_packet` is `0`
    /// and the input callback supplies the per-packet byte count via an
    /// `AudioStreamPacketDescription`. PCM block size per packet is
    /// codec-defined (1152 for MPEG-1 Layer III, 576 for MPEG-2 /
    /// MPEG-2.5 Layer III) — AT picks the right block size from the
    /// header bits inside each frame, so the `frames_per_packet` field
    /// is left at `0` (the "tell me from the header" sentinel for
    /// AudioConverter compressed inputs).
    ///
    /// `sample_rate` and `channels` must come from the first decoded
    /// frame header. AT will refuse a converter whose declared rate
    /// disagrees with the first frame's header.
    pub fn mpeg_layer_3(sample_rate: f64, channels: u32) -> Self {
        Self {
            sample_rate,
            format_id: K_AUDIO_FORMAT_MPEG_LAYER_3,
            format_flags: 0,
            bytes_per_packet: 0,  // variable (header-driven)
            frames_per_packet: 0, // header-driven (1152 or 576)
            bytes_per_frame: 0,   // compressed
            channels_per_frame: channels,
            bits_per_channel: 0,
            reserved: 0,
        }
    }

    /// Construct an ASBD for MPEG Layer II (MP2, compressed input).
    /// Like Layer III, packet-side metadata is header-driven.
    pub fn mpeg_layer_2(sample_rate: f64, channels: u32) -> Self {
        Self {
            sample_rate,
            format_id: K_AUDIO_FORMAT_MPEG_LAYER_2,
            format_flags: 0,
            bytes_per_packet: 0,
            frames_per_packet: 0,
            bytes_per_frame: 0,
            channels_per_frame: channels,
            bits_per_channel: 0,
            reserved: 0,
        }
    }

    /// Construct an ASBD for MPEG Layer I (MP1, compressed input).
    /// Header-driven like Layer II / III.
    pub fn mpeg_layer_1(sample_rate: f64, channels: u32) -> Self {
        Self {
            sample_rate,
            format_id: K_AUDIO_FORMAT_MPEG_LAYER_1,
            format_flags: 0,
            bytes_per_packet: 0,
            frames_per_packet: 0,
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

    #[test]
    fn asbd_aac_ld_geometry() {
        let a = AudioStreamBasicDescription::mpeg4_aac_ld(48_000.0, 2);
        assert_eq!(a.format_id, K_AUDIO_FORMAT_MPEG4_AAC_LD);
        assert_eq!(a.frames_per_packet, 512); // low-delay core, no SBR doubling
        assert_eq!(a.channels_per_frame, 2);
        assert_eq!(a.bytes_per_packet, 0); // compressed, variable
    }

    #[test]
    fn asbd_aac_eld_geometry() {
        let a = AudioStreamBasicDescription::mpeg4_aac_eld(48_000.0, 2);
        assert_eq!(a.format_id, K_AUDIO_FORMAT_MPEG4_AAC_ELD);
        assert_eq!(a.frames_per_packet, 512);
        assert_eq!(a.channels_per_frame, 2);
    }

    #[test]
    fn asbd_ilbc_20ms_geometry() {
        let a = AudioStreamBasicDescription::ilbc(160);
        assert_eq!(a.format_id, K_AUDIO_FORMAT_ILBC);
        assert_eq!(a.sample_rate, 8_000.0);
        assert_eq!(a.channels_per_frame, 1);
        assert_eq!(a.frames_per_packet, 160);
        assert_eq!(a.bytes_per_packet, 38); // 20 ms iLBC packet
    }

    #[test]
    fn asbd_ilbc_30ms_geometry() {
        let a = AudioStreamBasicDescription::ilbc(240);
        assert_eq!(a.format_id, K_AUDIO_FORMAT_ILBC);
        assert_eq!(a.frames_per_packet, 240);
        assert_eq!(a.bytes_per_packet, 50); // 30 ms iLBC packet
    }

    #[test]
    fn ilbc_fourcc_value() {
        assert_eq!(K_AUDIO_FORMAT_ILBC, u32::from_be_bytes(*b"ilbc"));
    }

    #[test]
    fn amr_fourcc_value() {
        assert_eq!(K_AUDIO_FORMAT_AMR, u32::from_be_bytes(*b"samr"));
    }

    #[test]
    fn amr_wb_fourcc_value() {
        assert_eq!(K_AUDIO_FORMAT_AMR_WB, u32::from_be_bytes(*b"sawb"));
    }

    #[test]
    fn asbd_amr_nb_geometry() {
        let a = AudioStreamBasicDescription::amr_nb();
        assert_eq!(a.format_id, K_AUDIO_FORMAT_AMR);
        assert_eq!(a.sample_rate, 8_000.0);
        assert_eq!(a.channels_per_frame, 1);
        assert_eq!(a.frames_per_packet, 160); // 20 ms @ 8 kHz
        assert_eq!(a.bytes_per_packet, 0); // variable per mode
    }

    #[test]
    fn asbd_amr_wb_geometry() {
        let a = AudioStreamBasicDescription::amr_wb();
        assert_eq!(a.format_id, K_AUDIO_FORMAT_AMR_WB);
        assert_eq!(a.sample_rate, 16_000.0);
        assert_eq!(a.channels_per_frame, 1);
        assert_eq!(a.frames_per_packet, 320); // 20 ms @ 16 kHz
        assert_eq!(a.bytes_per_packet, 0); // variable per mode
    }

    #[test]
    fn mp3_fourcc_values() {
        // '.mp1' / '.mp2' / '.mp3' — Apple's MPEG-1/2 Audio Layer
        // FourCCs as registered in AudioFormat.h.
        assert_eq!(K_AUDIO_FORMAT_MPEG_LAYER_1, u32::from_be_bytes(*b".mp1"));
        assert_eq!(K_AUDIO_FORMAT_MPEG_LAYER_2, u32::from_be_bytes(*b".mp2"));
        assert_eq!(K_AUDIO_FORMAT_MPEG_LAYER_3, u32::from_be_bytes(*b".mp3"));
    }

    #[test]
    fn asbd_mp3_geometry() {
        let a = AudioStreamBasicDescription::mpeg_layer_3(44_100.0, 2);
        assert_eq!(a.format_id, K_AUDIO_FORMAT_MPEG_LAYER_3);
        assert_eq!(a.sample_rate, 44_100.0);
        assert_eq!(a.channels_per_frame, 2);
        // Both byte-count and frame-count are header-driven for MP3.
        assert_eq!(a.bytes_per_packet, 0);
        assert_eq!(a.frames_per_packet, 0);
        assert_eq!(a.bytes_per_frame, 0);
        assert_eq!(a.bits_per_channel, 0);
    }

    #[test]
    fn asbd_mp2_geometry() {
        let a = AudioStreamBasicDescription::mpeg_layer_2(48_000.0, 2);
        assert_eq!(a.format_id, K_AUDIO_FORMAT_MPEG_LAYER_2);
        assert_eq!(a.frames_per_packet, 0);
    }

    #[test]
    fn asbd_mp1_geometry() {
        let a = AudioStreamBasicDescription::mpeg_layer_1(48_000.0, 2);
        assert_eq!(a.format_id, K_AUDIO_FORMAT_MPEG_LAYER_1);
        assert_eq!(a.frames_per_packet, 0);
    }

    #[test]
    fn ld_eld_fourcc_values() {
        // Spell out the FourCC byte mapping so a typo can't slip through.
        assert_eq!(K_AUDIO_FORMAT_MPEG4_AAC_LD, u32::from_be_bytes(*b"aacl"));
        assert_eq!(K_AUDIO_FORMAT_MPEG4_AAC_ELD, u32::from_be_bytes(*b"aace"));
    }
}
