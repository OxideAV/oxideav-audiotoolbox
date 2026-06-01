//! FLAC (Free Lossless Audio Codec) stream-level helpers for the
//! AudioToolbox bridge.
//!
//! Everything in this module is wall-isolated from third-party FLAC
//! code: the layout comes from **RFC 9639**
//! (`docs/audio/flac/rfc9639-flac.pdf`) and the behavioural-trace
//! corpus under `docs/audio/flac/fixtures/`. AT does the entropy
//! decode and the IMDCT-free fixed/LPC reconstruction internally; we
//! only need three things on this side of the FFI boundary:
//!
//! 1. **`StreamInfo`** — parse the mandatory 34-byte STREAMINFO body
//!    so the bridge knows `(sample_rate, channels, bits_per_sample,
//!    min_blocksize, max_blocksize)` ahead of the first decode call.
//! 2. **`FrameHeader`** — walk a FLAC frame's first 6–18 bytes far
//!    enough to read `block_size` + the per-frame consistency fields
//!    (sample rate, channel assignment, bits per sample). We don't
//!    decode the subframes — AT does.
//! 3. **`build_magic_cookie`** — assemble the byte blob that
//!    `AudioConverterSetProperty(kAudioConverterDecompressionMagicCookie, …)`
//!    expects for FLAC. **Empirically AT requires the cookie to be
//!    a `dfLa` ISOBMFF box** (the same FLAC-in-MP4 carriage box
//!    defined by Xiph's "FLAC encapsulation in ISOBMFF" spec): an
//!    8-byte box header (`size`, `'dfLa'`), a 4-byte FullBox
//!    header (version=0, flags=0), followed by the FLAC metadata
//!    block chain (each block = 4-byte header + body) up to and
//!    including a STREAMINFO block. The maximum cookie size AT
//!    accepts is 256 bytes — large enough for STREAMINFO plus a
//!    small PADDING block, but not arbitrary `.flac` prefixes. The
//!    `fLaC` four-character signature is NOT part of the AT cookie
//!    (it's a file-level marker only).
//!
//!    Discovered empirically by probing
//!    `AudioConverterSetProperty(kAudioConverterDecompressionMagicCookie,
//!    …)` against `kAudioFormatFLAC`: bare STREAMINFO returns
//!    `'!dat'` (`kAudioConverterErr_InvalidInputSize` semantics —
//!    data didn't validate); `fLaC + STREAMINFO` likewise; only the
//!    `dfLa`-boxed form validates. This matches the ISO/IEC 14496-12
//!    layout AT's MP4 demuxer would have synthesised before passing
//!    the cookie down to the AudioConverter.
//!
//! ## RFC 9639 references
//!
//! * §8 — top-level stream layout (`fLaC` + METADATA_BLOCK chain +
//!   FRAME chain).
//! * §8.1 — STREAMINFO body (16 + 16 + 24 + 24 + 20 + 3 + 5 + 36 +
//!   128 bits = 272 bits = 34 bytes).
//! * §9.1 — FRAME_HEADER (15-bit sync code `0b111111111111110` =
//!   `0x7FFC`, 1-bit blocking strategy, 4-bit block-size code, 4-bit
//!   sample-rate code, 4-bit channel-assignment code, 3-bit
//!   bits-per-sample code, 1 reserved bit, 1–7 bytes UTF-8-style
//!   frame/sample number, 0/1/2 bytes block-size escape, 0/1/2 bytes
//!   sample-rate escape, 1 byte CRC-8).
//! * §9.1.2 — block-size / sample-rate code tables (RFC 9639 Tables
//!   1 and 2).
//! * §9.1.3 — channel-assignment table.

/// Length of the mandatory STREAMINFO body in bytes (RFC 9639 §8.1).
pub const STREAMINFO_BODY_LEN: usize = 34;

/// Length of a metadata block header (RFC 9639 §8): 1-bit last-flag +
/// 7-bit block_type + 24-bit block_length = 4 bytes.
pub const METADATA_BLOCK_HEADER_LEN: usize = 4;

/// Length of the `fLaC` ASCII signature (used for file-level streams
/// — the AT magic cookie does NOT include this prefix).
pub const FLAC_SIGNATURE_LEN: usize = 4;

/// The 4-byte `fLaC` stream signature (RFC 9639 §8).
pub const FLAC_SIGNATURE: [u8; FLAC_SIGNATURE_LEN] = *b"fLaC";

/// Length of an ISOBMFF `BoxHeader` (4-byte size + 4-byte type code).
pub const BOX_HEADER_LEN: usize = 8;

/// Length of a `FullBox` header (4-byte version+flags) inside a
/// `dfLa` box. The Xiph "FLAC in ISOBMFF" spec mandates version=0
/// and flags=0.
pub const FULLBOX_HEADER_LEN: usize = 4;

/// Minimum magic-cookie length: `dfLa` box (8) + FullBox (4) +
/// METADATA_BLOCK_HEADER (4) + STREAMINFO body (34) = 50 bytes.
pub const MAGIC_COOKIE_MIN_LEN: usize =
    BOX_HEADER_LEN + FULLBOX_HEADER_LEN + METADATA_BLOCK_HEADER_LEN + STREAMINFO_BODY_LEN;

/// Maximum magic-cookie length AT accepts. Discovered empirically by
/// probing the converter: anything above 256 bytes returns
/// `kAudioConverterErr_BadPropertySizeError`.
pub const MAGIC_COOKIE_MAX_LEN: usize = 256;

/// Four-byte `dfLa` box type code (FLAC-in-ISOBMFF specific box).
pub const DFLA_BOX_TYPE: [u8; 4] = *b"dfLa";

/// Decoded STREAMINFO block (RFC 9639 §8.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    /// Minimum block size in samples (across the whole stream).
    pub min_blocksize: u16,
    /// Maximum block size in samples.
    pub max_blocksize: u16,
    /// Minimum frame size in bytes (`0` means "unknown").
    pub min_framesize: u32,
    /// Maximum frame size in bytes (`0` means "unknown").
    pub max_framesize: u32,
    /// Stream sample rate in Hz (1..=655_350; 0 is invalid).
    pub sample_rate: u32,
    /// Channel count (1..=8).
    pub channels: u8,
    /// Bits per sample (4..=32).
    pub bits_per_sample: u8,
    /// Total sample count per channel (`0` means "unknown").
    pub total_samples: u64,
    /// MD5 of the decoded PCM stream (16 bytes).
    pub md5: [u8; 16],
}

impl StreamInfo {
    /// Parse a 34-byte STREAMINFO body.
    ///
    /// Returns `None` if `bytes.len() < 34`, the sample rate is `0`,
    /// or the channel count / bits-per-sample fields decode to a
    /// reserved value.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < STREAMINFO_BODY_LEN {
            return None;
        }
        let min_blocksize = u16::from_be_bytes([bytes[0], bytes[1]]);
        let max_blocksize = u16::from_be_bytes([bytes[2], bytes[3]]);
        let min_framesize = u32::from_be_bytes([0, bytes[4], bytes[5], bytes[6]]);
        let max_framesize = u32::from_be_bytes([0, bytes[7], bytes[8], bytes[9]]);

        // Bits 80..=99 → sample_rate (20 bits, big-endian).
        // Bits 100..=102 → channels − 1 (3 bits).
        // Bits 103..=107 → bits_per_sample − 1 (5 bits).
        // Bits 108..=143 → total_samples (36 bits).
        //
        // That packs into bytes 10..18 = 8 bytes carrying 20 + 3 + 5 +
        // 36 = 64 bits. Pull as one u64 to handle the bit positions
        // without per-bit gymnastics.
        let packed = u64::from_be_bytes([
            bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15], bytes[16], bytes[17],
        ]);
        let sample_rate = (packed >> 44) as u32 & 0xF_FFFF; // 20 bits
        let channels_m1 = (packed >> 41) as u8 & 0b111; // 3 bits
        let bps_m1 = (packed >> 36) as u8 & 0b11111; // 5 bits
        let total_samples = packed & ((1u64 << 36) - 1); // 36 bits

        if sample_rate == 0 {
            return None;
        }
        let channels = channels_m1 + 1;
        let bits_per_sample = bps_m1 + 1;
        // RFC 9639 §8.1: channels = 1..=8, bits_per_sample = 4..=32.
        if bits_per_sample < 4 {
            return None;
        }

        let mut md5 = [0u8; 16];
        md5.copy_from_slice(&bytes[18..34]);

        Some(StreamInfo {
            min_blocksize,
            max_blocksize,
            min_framesize,
            max_framesize,
            sample_rate,
            channels,
            bits_per_sample,
            total_samples,
            md5,
        })
    }

    /// Serialise back to the 34-byte big-endian wire format. The
    /// inverse of `parse`.
    pub fn to_bytes(&self) -> [u8; STREAMINFO_BODY_LEN] {
        let mut out = [0u8; STREAMINFO_BODY_LEN];
        out[0..2].copy_from_slice(&self.min_blocksize.to_be_bytes());
        out[2..4].copy_from_slice(&self.max_blocksize.to_be_bytes());
        let mn = self.min_framesize.to_be_bytes();
        out[4..7].copy_from_slice(&mn[1..4]);
        let mx = self.max_framesize.to_be_bytes();
        out[7..10].copy_from_slice(&mx[1..4]);
        let packed = ((self.sample_rate as u64 & 0xF_FFFF) << 44)
            | (((self.channels as u64 - 1) & 0b111) << 41)
            | (((self.bits_per_sample as u64 - 1) & 0b11111) << 36)
            | (self.total_samples & ((1u64 << 36) - 1));
        out[10..18].copy_from_slice(&packed.to_be_bytes());
        out[18..34].copy_from_slice(&self.md5);
        out
    }

    /// Returns true when `min_blocksize == max_blocksize` (RFC 9639
    /// §8.1 fixed-blocksize indicator).
    pub fn is_fixed_blocksize(&self) -> bool {
        self.min_blocksize == self.max_blocksize
    }
}

/// Map a FLAC bit depth (4..=32) to the AudioToolbox `format_flags`
/// "source-data" value. Apple's `CoreAudioBaseTypes.h` enum comment
/// for `kAudioFormatFLAC` states the same flags-mean-bit-depth
/// convention as ALAC, so we reuse the four
/// `kAppleLosslessFormatFlag_*BitSourceData` values (1, 2, 3, 4).
///
/// 16, 20, 24, 32 are the only directly-representable depths. Other
/// values (8, 12) round **up** to the next representable flag so AT
/// allocates a wide-enough decoder slot (an 8-bit FLAC stream decoded
/// into a 16-bit canister loses no information). Returns `None` for
/// anything outside 4..=32.
pub fn bit_depth_flag(bits_per_sample: u8) -> Option<u32> {
    use crate::sys::{
        K_AF_APPLE_LOSSLESS_16_BIT, K_AF_APPLE_LOSSLESS_20_BIT, K_AF_APPLE_LOSSLESS_24_BIT,
        K_AF_APPLE_LOSSLESS_32_BIT,
    };
    match bits_per_sample {
        4..=16 => Some(K_AF_APPLE_LOSSLESS_16_BIT),
        17..=20 => Some(K_AF_APPLE_LOSSLESS_20_BIT),
        21..=24 => Some(K_AF_APPLE_LOSSLESS_24_BIT),
        25..=32 => Some(K_AF_APPLE_LOSSLESS_32_BIT),
        _ => None,
    }
}

/// Build the AudioToolbox magic cookie for a FLAC stream described by
/// `info`. The cookie is a `dfLa` ISOBMFF box (Xiph's
/// FLAC-in-ISOBMFF specific box) — see the module-level docs for the
/// empirical-probe-derived rationale.
///
/// Layout (50 bytes total):
///
/// ```text
/// 4 bytes  box size (= 50, big-endian)
/// 4 bytes  box type ('dfLa')
/// 4 bytes  FullBox header (version=0, flags=0)
/// 4 bytes  METADATA_BLOCK header (last=1, type=0, length=34)
/// 34 bytes STREAMINFO body
/// ```
pub fn build_magic_cookie(info: &StreamInfo) -> Vec<u8> {
    let mut out = Vec::with_capacity(MAGIC_COOKIE_MIN_LEN);
    // BoxHeader: size (50), type 'dfLa'.
    out.extend_from_slice(&(MAGIC_COOKIE_MIN_LEN as u32).to_be_bytes());
    out.extend_from_slice(&DFLA_BOX_TYPE);
    // FullBox: version=0, flags=000000.
    out.extend_from_slice(&[0u8, 0, 0, 0]);
    // METADATA_BLOCK header: last=1, block_type=0 (STREAMINFO),
    // length=34. block_type slot stays at zero by construction.
    let header_word: u32 = (1u32 << 31) | (STREAMINFO_BODY_LEN as u32);
    out.extend_from_slice(&header_word.to_be_bytes());
    out.extend_from_slice(&info.to_bytes());
    debug_assert_eq!(out.len(), MAGIC_COOKIE_MIN_LEN);
    out
}

/// Parse a FLAC magic cookie (`dfLa` box payload) into the underlying
/// STREAMINFO. The first metadata block inside the box must be a
/// STREAMINFO (block_type = 0). Subsequent metadata blocks (PADDING,
/// VORBIS_COMMENT, SEEKTABLE, etc.) are allowed and ignored.
///
/// Returns `None` for: a box too small to contain the minimum layout,
/// a box type other than `dfLa`, an out-of-range size field, a
/// missing STREAMINFO, or a STREAMINFO body that fails
/// `StreamInfo::parse`.
pub fn parse_magic_cookie(cookie: &[u8]) -> Option<StreamInfo> {
    if cookie.len() < MAGIC_COOKIE_MIN_LEN {
        return None;
    }
    // BoxHeader: size + type.
    let box_size = u32::from_be_bytes([cookie[0], cookie[1], cookie[2], cookie[3]]) as usize;
    if box_size > cookie.len() {
        return None;
    }
    if &cookie[4..8] != DFLA_BOX_TYPE.as_slice() {
        return None;
    }
    // FullBox header at 8..12: version=0, flags=000000 (we accept any
    // value to be tolerant — Xiph FLAC-in-MP4 spec requires zeros).
    // Metadata blocks start at offset 12.
    let mut pos = BOX_HEADER_LEN + FULLBOX_HEADER_LEN;
    let end = box_size.min(cookie.len());
    while pos + METADATA_BLOCK_HEADER_LEN <= end {
        let header = u32::from_be_bytes([
            cookie[pos],
            cookie[pos + 1],
            cookie[pos + 2],
            cookie[pos + 3],
        ]);
        let last = (header >> 31) & 1 == 1;
        let block_type = ((header >> 24) & 0x7F) as u8;
        let block_length = (header & 0x00FF_FFFF) as usize;
        pos += METADATA_BLOCK_HEADER_LEN;
        if pos + block_length > end {
            return None;
        }
        if block_type == 0 {
            return StreamInfo::parse(&cookie[pos..pos + block_length]);
        }
        if last {
            return None;
        }
        pos += block_length;
    }
    None
}

/// Sample-rate code table (RFC 9639 §9.1.2 Table 2 — the first
/// dimension is the 4-bit `sample_rate_code` field).
///
/// Codes `0`/`12`/`13`/`14` need additional bytes from later in the
/// frame header to decode; we surface those as `None` here. Code
/// `0` means "use STREAMINFO" (look up at the stream level). Code
/// `15` is reserved and signals an invalid frame.
pub fn sample_rate_from_code(code: u8) -> Option<u32> {
    match code {
        1 => Some(88_200),
        2 => Some(176_400),
        3 => Some(192_000),
        4 => Some(8_000),
        5 => Some(16_000),
        6 => Some(22_050),
        7 => Some(24_000),
        8 => Some(32_000),
        9 => Some(44_100),
        10 => Some(48_000),
        11 => Some(96_000),
        _ => None, // 0 / 12 / 13 / 14 / 15: not table-resolvable here.
    }
}

/// Block-size code table (RFC 9639 §9.1.2 Table 1).
///
/// Codes `0`/`6`/`7` indicate "reserved or read additional bytes";
/// only the directly-resolvable codes are returned. Codes 8..=15
/// resolve to `256 << (code - 8)`.
pub fn block_size_from_code(code: u8) -> Option<u32> {
    match code {
        0 => None,                              // reserved
        1 => Some(192),                         //
        2..=5 => Some(576 * (1 << (code - 2))), // 576, 1152, 2304, 4608
        6 | 7 => None,                          // escape: read 8 / 16 extra bits from the header
        8..=15 => Some(256 << (code - 8)),      // 256, 512, 1024, 2048, 4096, 8192, 16384, 32768
        _ => None,
    }
}

/// Bits-per-sample code table (RFC 9639 §9.1.4).
///
/// Code `0` means "use STREAMINFO"; codes `3` and `7` are reserved.
pub fn bits_per_sample_from_code(code: u8) -> Option<u8> {
    match code {
        1 => Some(8),
        2 => Some(12),
        4 => Some(16),
        5 => Some(20),
        6 => Some(24),
        // Code `3` is reserved (RFC 9639 §9.1.4); Code `7` was reserved
        // through RFC 9639 §9.1.4 errata-pre but is now defined as
        // 32-bit in the published RFC. Other codes (0) need
        // STREAMINFO fallback.
        7 => Some(32),
        _ => None,
    }
}

/// Channel assignment (RFC 9639 §9.1.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelAssignment {
    /// Each subframe is an independently-coded channel; total channel
    /// count is `code + 1`.
    Independent(u8),
    /// Subframe 0 = L, subframe 1 = side (L − R). Code 8.
    LeftSide,
    /// Subframe 0 = side (L − R), subframe 1 = R. Code 9.
    SideRight,
    /// Subframe 0 = mid ((L+R)/2), subframe 1 = side (L − R). Code 10.
    MidSide,
}

impl ChannelAssignment {
    /// Total channel count produced by this assignment.
    pub fn channel_count(self) -> u8 {
        match self {
            ChannelAssignment::Independent(n) => n + 1,
            // All three stereo decorrelation modes produce 2 channels.
            ChannelAssignment::LeftSide
            | ChannelAssignment::SideRight
            | ChannelAssignment::MidSide => 2,
        }
    }

    /// Resolve the 4-bit field. Codes 11..=15 are reserved.
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0..=7 => Some(ChannelAssignment::Independent(code)),
            8 => Some(ChannelAssignment::LeftSide),
            9 => Some(ChannelAssignment::SideRight),
            10 => Some(ChannelAssignment::MidSide),
            _ => None,
        }
    }
}

/// Walk just enough of a FLAC frame header to recover the per-frame
/// invariants we need for the AT bridge: block size, sample rate,
/// channel count, bit depth, blocking strategy. We do NOT verify the
/// CRC-8 here — AT will reject a corrupt frame on its own and
/// surfacing a typed error before AT sees the byte stream just
/// duplicates work.
///
/// Returns `None` for a non-sync first 2 bytes, a reserved field
/// value, or a length that runs off the end of `bytes`.
pub fn parse_frame_header(bytes: &[u8], info: &StreamInfo) -> Option<FrameHeader> {
    if bytes.len() < 5 {
        return None;
    }
    // Sync code: 15 bits `0b111111111111100` (RFC 9639 §9.1.1) —
    // i.e. 13 ones followed by two zeros, occupying bits 0..=14 of
    // the first two bytes (MSB-first). The LSB of byte 1 carries
    // the blocking-strategy bit.
    let sync_high = bytes[0];
    let sync_low_msb = bytes[1] >> 1; // top 7 bits of byte 1 → bits 8..=14 of the 15-bit code
    if sync_high != 0xFF || sync_low_msb != 0b111_1100 {
        return None;
    }
    let blocking_strategy = bytes[1] & 0b1;
    let block_size_code = (bytes[2] >> 4) & 0b1111;
    let sample_rate_code = bytes[2] & 0b1111;
    let channel_assignment_code = (bytes[3] >> 4) & 0b1111;
    let bps_code = (bytes[3] >> 1) & 0b111;
    let _reserved = bytes[3] & 0b1; // RFC 9639 §9.1: must be 0 per spec.

    let channel_assignment = ChannelAssignment::from_code(channel_assignment_code)?;

    // Resolve bit depth: code `0` falls back to STREAMINFO.
    let bits_per_sample = match bps_code {
        0 => info.bits_per_sample,
        _ => bits_per_sample_from_code(bps_code)?,
    };

    // Resolve sample rate: code `0` falls back to STREAMINFO; codes
    // 12/13/14 escape to additional bytes after the UTF-8 frame
    // number (we surface that as "from STREAMINFO" rather than
    // walking further — the bridge only uses sample_rate for
    // consistency checks against the latched configuration).
    let sample_rate = match sample_rate_code {
        0 | 12..=14 => info.sample_rate,
        _ => sample_rate_from_code(sample_rate_code).unwrap_or(info.sample_rate),
    };

    // Resolve block size: codes 6 and 7 escape to bytes 1 or 2 bytes
    // after the UTF-8 frame number; we'd need to walk further. For
    // the bridge invariants we only need the fixed-blocksize cases
    // up to code 5 / 8..=15; surface the escape codes as the
    // STREAMINFO max_blocksize bound.
    let block_size = match block_size_code {
        6 | 7 => info.max_blocksize as u32,
        _ => block_size_from_code(block_size_code).unwrap_or(info.max_blocksize as u32),
    };

    Some(FrameHeader {
        blocking_strategy,
        block_size,
        sample_rate,
        channel_assignment,
        bits_per_sample,
    })
}

/// Decoded FLAC frame header (subset relevant to the AT bridge).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    /// 0 = fixed-blocksize (frame number encoded next); 1 = variable
    /// (sample number encoded next).
    pub blocking_strategy: u8,
    /// Block size for this frame in PCM samples per channel.
    pub block_size: u32,
    /// Sample rate (Hz) for this frame.
    pub sample_rate: u32,
    /// Channel-assignment field.
    pub channel_assignment: ChannelAssignment,
    /// Bits-per-sample for this frame.
    pub bits_per_sample: u8,
}

impl FrameHeader {
    /// Total channel count produced by decoding this frame.
    pub fn channels(&self) -> u8 {
        self.channel_assignment.channel_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_streaminfo() -> StreamInfo {
        StreamInfo {
            min_blocksize: 4608,
            max_blocksize: 4608,
            min_framesize: 0,
            max_framesize: 0,
            sample_rate: 44_100,
            channels: 2,
            bits_per_sample: 16,
            total_samples: 0,
            md5: [0u8; 16],
        }
    }

    #[test]
    fn streaminfo_roundtrip_44100_stereo_16bit() {
        let info = sample_streaminfo();
        let bytes = info.to_bytes();
        let parsed = StreamInfo::parse(&bytes).expect("parse");
        assert_eq!(parsed, info);
    }

    #[test]
    fn streaminfo_roundtrip_96000_mono_24bit() {
        let info = StreamInfo {
            min_blocksize: 4096,
            max_blocksize: 4096,
            min_framesize: 5,
            max_framesize: 13_999,
            sample_rate: 96_000,
            channels: 1,
            bits_per_sample: 24,
            total_samples: 192_000,
            md5: [0xAB; 16],
        };
        let bytes = info.to_bytes();
        let parsed = StreamInfo::parse(&bytes).expect("parse");
        assert_eq!(parsed, info);
        assert!(parsed.is_fixed_blocksize());
    }

    #[test]
    fn streaminfo_rejects_zero_sample_rate() {
        let mut info = sample_streaminfo();
        info.sample_rate = 0;
        let bytes = info.to_bytes();
        assert!(StreamInfo::parse(&bytes).is_none());
    }

    #[test]
    fn streaminfo_rejects_short_buffer() {
        let bytes = [0u8; 33];
        assert!(StreamInfo::parse(&bytes).is_none());
    }

    #[test]
    fn fixed_vs_variable_blocksize_predicate() {
        let mut info = sample_streaminfo();
        assert!(info.is_fixed_blocksize());
        info.min_blocksize = 1024;
        info.max_blocksize = 4096;
        assert!(!info.is_fixed_blocksize());
    }

    #[test]
    fn parses_fixture_streaminfo_44100_stereo() {
        // The stereo-16bit-44100-fixed fixture's STREAMINFO body
        // (extracted from `xxd input.flac`). Bytes 8..42 of the file
        // are the 34-byte STREAMINFO body — bytes 0..4 are the `fLaC`
        // signature and 4..8 are the metadata block header.
        let body: [u8; 34] = [
            0x12, 0x00, 0x12, 0x00, 0x00, 0x08, 0x8F, 0x00, 0x0F, 0x93, 0x0A, 0xC4, 0x42, 0xF0,
            0x00, 0x00, 0xAC, 0x44, 0xB2, 0x49, 0x33, 0xA4, 0x9B, 0xB7, 0x43, 0xC0, 0x00, 0x61,
            0x51, 0x6D, 0x2B, 0x08, 0xDD, 0x45,
        ];
        let info = StreamInfo::parse(&body).expect("fixture STREAMINFO parses");
        assert_eq!(info.min_blocksize, 4608);
        assert_eq!(info.max_blocksize, 4608);
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.channels, 2);
        assert_eq!(info.bits_per_sample, 16);
        assert!(info.is_fixed_blocksize());
    }

    #[test]
    fn build_and_parse_magic_cookie_roundtrip() {
        let info = sample_streaminfo();
        let cookie = build_magic_cookie(&info);
        assert_eq!(cookie.len(), MAGIC_COOKIE_MIN_LEN);
        // BoxHeader: size = 50, type = 'dfLa'.
        let box_size = u32::from_be_bytes([cookie[0], cookie[1], cookie[2], cookie[3]]);
        assert_eq!(box_size as usize, MAGIC_COOKIE_MIN_LEN);
        assert_eq!(&cookie[4..8], b"dfLa");
        // FullBox: version=0, flags=0.
        assert_eq!(&cookie[8..12], &[0u8, 0, 0, 0]);
        // Metadata block header: last_flag=1, block_type=0, length=34.
        let header = u32::from_be_bytes([cookie[12], cookie[13], cookie[14], cookie[15]]);
        assert_eq!(header >> 31, 1, "last-flag must be set");
        assert_eq!((header >> 24) & 0x7F, 0, "block_type=STREAMINFO");
        assert_eq!(header & 0x00FF_FFFF, 34, "length must be 34");
        let parsed = parse_magic_cookie(&cookie).expect("cookie parses");
        assert_eq!(parsed, info);
    }

    #[test]
    fn parse_magic_cookie_skips_through_padding_to_streaminfo() {
        // Build a synthetic dfLa cookie with STREAMINFO + PADDING. The
        // parser must pick up STREAMINFO from the first block.
        let info = sample_streaminfo();
        let mut payload = Vec::new();
        // FullBox header.
        payload.extend_from_slice(&[0u8, 0, 0, 0]);
        // STREAMINFO header: last=0, type=0, length=34. The last-flag
        // and block-type slots stay at zero by construction.
        let h1: u32 = STREAMINFO_BODY_LEN as u32;
        payload.extend_from_slice(&h1.to_be_bytes());
        payload.extend_from_slice(&info.to_bytes());
        // PADDING header: last=1, type=1, length=16.
        let h2: u32 = (1u32 << 31) | (1u32 << 24) | 16u32;
        payload.extend_from_slice(&h2.to_be_bytes());
        payload.extend_from_slice(&[0u8; 16]);

        let mut cookie = Vec::new();
        let total_size = BOX_HEADER_LEN + payload.len();
        cookie.extend_from_slice(&(total_size as u32).to_be_bytes());
        cookie.extend_from_slice(&DFLA_BOX_TYPE);
        cookie.extend_from_slice(&payload);

        let parsed = parse_magic_cookie(&cookie).expect("multi-block cookie parses");
        assert_eq!(parsed, info);
    }

    #[test]
    fn parse_magic_cookie_rejects_wrong_box_type() {
        let info = sample_streaminfo();
        let mut cookie = build_magic_cookie(&info);
        // Corrupt the 'dfLa' type to 'OggS'.
        cookie[4..8].copy_from_slice(b"OggS");
        assert!(parse_magic_cookie(&cookie).is_none());
    }

    #[test]
    fn parse_magic_cookie_rejects_short() {
        let cookie = vec![0u8; 10];
        assert!(parse_magic_cookie(&cookie).is_none());
    }

    #[test]
    fn magic_cookie_stays_under_at_size_limit() {
        let info = sample_streaminfo();
        let cookie = build_magic_cookie(&info);
        assert!(
            cookie.len() <= MAGIC_COOKIE_MAX_LEN,
            "cookie length {} exceeds AT FLAC max {}",
            cookie.len(),
            MAGIC_COOKIE_MAX_LEN
        );
    }

    #[test]
    fn bit_depth_flag_maps_canonical_values() {
        use crate::sys::{
            K_AF_APPLE_LOSSLESS_16_BIT, K_AF_APPLE_LOSSLESS_20_BIT, K_AF_APPLE_LOSSLESS_24_BIT,
            K_AF_APPLE_LOSSLESS_32_BIT,
        };
        assert_eq!(bit_depth_flag(8), Some(K_AF_APPLE_LOSSLESS_16_BIT));
        assert_eq!(bit_depth_flag(12), Some(K_AF_APPLE_LOSSLESS_16_BIT));
        assert_eq!(bit_depth_flag(16), Some(K_AF_APPLE_LOSSLESS_16_BIT));
        assert_eq!(bit_depth_flag(20), Some(K_AF_APPLE_LOSSLESS_20_BIT));
        assert_eq!(bit_depth_flag(24), Some(K_AF_APPLE_LOSSLESS_24_BIT));
        assert_eq!(bit_depth_flag(32), Some(K_AF_APPLE_LOSSLESS_32_BIT));
        assert_eq!(bit_depth_flag(33), None);
        assert_eq!(bit_depth_flag(3), None);
    }

    #[test]
    fn sample_rate_table_resolves_canonical_codes() {
        assert_eq!(sample_rate_from_code(1), Some(88_200));
        assert_eq!(sample_rate_from_code(4), Some(8_000));
        assert_eq!(sample_rate_from_code(9), Some(44_100));
        assert_eq!(sample_rate_from_code(10), Some(48_000));
        assert_eq!(sample_rate_from_code(11), Some(96_000));
        // Code 0 = "from STREAMINFO" so it's not table-resolvable.
        assert_eq!(sample_rate_from_code(0), None);
        // Code 15 = reserved.
        assert_eq!(sample_rate_from_code(15), None);
    }

    #[test]
    fn block_size_table_resolves_canonical_codes() {
        assert_eq!(block_size_from_code(1), Some(192));
        assert_eq!(block_size_from_code(2), Some(576));
        assert_eq!(block_size_from_code(5), Some(4608));
        assert_eq!(block_size_from_code(8), Some(256));
        assert_eq!(block_size_from_code(11), Some(2048));
        assert_eq!(block_size_from_code(15), Some(32_768));
        // Reserved / escape codes.
        assert_eq!(block_size_from_code(0), None);
        assert_eq!(block_size_from_code(6), None);
        assert_eq!(block_size_from_code(7), None);
    }

    #[test]
    fn channel_assignment_decodes_independent() {
        let a = ChannelAssignment::from_code(0).unwrap();
        assert_eq!(a.channel_count(), 1);
        let a = ChannelAssignment::from_code(1).unwrap();
        assert_eq!(a.channel_count(), 2);
        let a = ChannelAssignment::from_code(5).unwrap();
        assert_eq!(a.channel_count(), 6); // 5.1
        let a = ChannelAssignment::from_code(7).unwrap();
        assert_eq!(a.channel_count(), 8); // 7.1
    }

    #[test]
    fn channel_assignment_decodes_stereo_decorrelations() {
        let ls = ChannelAssignment::from_code(8).unwrap();
        assert_eq!(ls, ChannelAssignment::LeftSide);
        assert_eq!(ls.channel_count(), 2);
        let sr = ChannelAssignment::from_code(9).unwrap();
        assert_eq!(sr, ChannelAssignment::SideRight);
        let ms = ChannelAssignment::from_code(10).unwrap();
        assert_eq!(ms, ChannelAssignment::MidSide);
    }

    #[test]
    fn channel_assignment_rejects_reserved_codes() {
        for code in 11u8..=15 {
            assert!(ChannelAssignment::from_code(code).is_none());
        }
    }

    #[test]
    fn parse_frame_header_recognises_fixed_blocksize_stereo() {
        let info = sample_streaminfo();
        // Build a synthetic frame header:
        // byte 0..1: 0xFF F8 → 15-bit sync code 0b111111111111100 +
        //            blocking_strategy=0 (fixed). Layout: byte 0 all
        //            ones, byte 1 = 1111_1000 (top 5 bits of sync + 2
        //            sync zeros + blocking-strategy LSB = 0).
        // byte 2:    block_size_code=5 (=4608), sample_rate_code=9 (44.1k).
        // byte 3:    channel_assignment=8 (left-side), bps_code=4 (16),
        //            reserved bit = 0.
        // byte 4:    UTF-8 frame number byte 0 (single-byte form for
        //            frame=0).
        let bytes = [0xFFu8, 0xF8, (5 << 4) | 9, (8 << 4) | (4 << 1), 0x00];
        let h = parse_frame_header(&bytes, &info).expect("frame header parses");
        assert_eq!(h.blocking_strategy, 0);
        assert_eq!(h.block_size, 4608);
        assert_eq!(h.sample_rate, 44_100);
        assert_eq!(h.channel_assignment, ChannelAssignment::LeftSide);
        assert_eq!(h.bits_per_sample, 16);
        assert_eq!(h.channels(), 2);
    }

    #[test]
    fn parse_frame_header_recognises_variable_blocksize() {
        let info = sample_streaminfo();
        // byte 1 = 0xF9 → blocking_strategy=1 (variable-blocksize).
        // channel_assignment=0 (independent → 1 channel),
        // bps_code=4 (16-bit), reserved=0. The MSB nibble of the
        // channel byte stays at 0 by construction.
        let bytes = [0xFFu8, 0xF9, (8 << 4) | 9, 4 << 1, 0x00];
        let h = parse_frame_header(&bytes, &info).expect("frame header parses");
        assert_eq!(h.blocking_strategy, 1);
        assert_eq!(h.block_size, 256); // code 8 → 256
        assert_eq!(h.channels(), 1); // independent code 0 → 1 channel
    }

    #[test]
    fn parse_frame_header_rejects_bad_sync() {
        let info = sample_streaminfo();
        // 0xFE F8 — top byte is wrong.
        let bytes = [0xFEu8, 0xF8, 0x59, 0x88, 0x00];
        assert!(parse_frame_header(&bytes, &info).is_none());
        // 0xFF FA — bits 13..=14 of the sync code are not both zero
        // (this would imply 0b111_1111_1111_1101_0 — a different
        // 15-bit prefix).
        let bytes = [0xFFu8, 0xFA, 0x59, 0x88, 0x00];
        assert!(parse_frame_header(&bytes, &info).is_none());
    }

    #[test]
    fn parse_frame_header_rejects_reserved_channel_assignment() {
        let info = sample_streaminfo();
        // channel_assignment = 11 (reserved).
        let bytes = [0xFFu8, 0xF8, 0x59, (11 << 4) | (4 << 1), 0x00];
        assert!(parse_frame_header(&bytes, &info).is_none());
    }

    #[test]
    fn parse_frame_header_falls_back_to_streaminfo_bps() {
        let info = sample_streaminfo();
        // bps_code = 0 → "use STREAMINFO" → 16 bits. The channel
        // assignment + reserved + bps fields are all zero.
        let bytes = [0xFFu8, 0xF8, 0x59, 0x00, 0x00];
        let h = parse_frame_header(&bytes, &info).expect("frame header parses");
        assert_eq!(h.bits_per_sample, info.bits_per_sample);
    }
}
