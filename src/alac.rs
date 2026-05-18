//! Apple Lossless (ALAC) magic-cookie helpers for the AudioToolbox bridge.
//!
//! ALAC is a stateful codec — the encoder vends a 24-byte
//! `ALACSpecificConfig` (optionally followed by a 24-byte
//! `ALACChannelLayoutInfo`) that the decoder MUST be configured with
//! before any decode call. The cookie is opaque from the caller's
//! perspective; we only build / parse the 24-byte mandatory portion so
//! the bridge can:
//!
//! 1. **Decoder path** — receive a cookie via `CodecParameters::extradata`
//!    and forward it to `AudioConverterSetProperty(kAudioConverterDecompressionMagicCookie, …)`.
//!    If the consumer didn't fill the extradata, we synthesise a
//!    minimal-but-valid cookie from the explicit `sample_rate` /
//!    `channels` / `bits_per_sample` parameters — useful for the
//!    self-roundtrip test path where we control both ends.
//!
//! 2. **Encoder path** — read the cookie back from the converter after
//!    `AudioConverterNew` via `AudioConverterGetProperty(kAudioConverterCompressionMagicCookie, …)`
//!    and surface it through `output_params.extradata`. Downstream
//!    muxers (mov/mp4) need this verbatim to write a working ALAC
//!    track.
//!
//! The 24-byte struct (per Apple's `ALACAudioTypes.h` snapshot in
//! `docs/audio/alac/ALACMagicCookieDescription.txt`):
//!
//! ```text
//! u32 frame_length        // 4096 by default
//! u8  compatible_version  // 0
//! u8  bit_depth           // 16 / 20 / 24 / 32
//! u8  pb                  // 40 (unused tuning)
//! u8  mb                  // 10 (unused tuning)
//! u8  kb                  // 14 (unused tuning)
//! u8  num_channels        // 1..8
//! u16 max_run             // 255 (unused)
//! u32 max_frame_bytes     // 0 = unknown
//! u32 avg_bit_rate        // 0 = unknown
//! u32 sample_rate
//! ```
//!
//! All fields are big-endian on the wire, regardless of file format.

/// Mandatory length of `ALACSpecificConfig` in bytes.
pub const SPECIFIC_CONFIG_LEN: usize = 24;

/// Default ALAC encoder packet size (samples per channel per packet).
/// Apple's documentation calls 4096 the canonical value for maximum
/// compatibility.
pub const DEFAULT_FRAME_LENGTH: u32 = 4096;

/// Mandatory portion of an ALAC magic cookie.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlacSpecificConfig {
    pub frame_length: u32,
    pub compatible_version: u8,
    pub bit_depth: u8,
    pub pb: u8,
    pub mb: u8,
    pub kb: u8,
    pub num_channels: u8,
    pub max_run: u16,
    pub max_frame_bytes: u32,
    pub avg_bit_rate: u32,
    pub sample_rate: u32,
}

impl AlacSpecificConfig {
    /// Build a minimal-but-valid config with Apple's recommended tuning
    /// constants (`pb=40 / mb=10 / kb=14 / max_run=255 / frame_length=4096`)
    /// and the supplied stream geometry.
    pub fn new(sample_rate: u32, num_channels: u8, bit_depth: u8) -> Self {
        Self {
            frame_length: DEFAULT_FRAME_LENGTH,
            compatible_version: 0,
            bit_depth,
            pb: 40,
            mb: 10,
            kb: 14,
            num_channels,
            max_run: 255,
            max_frame_bytes: 0,
            avg_bit_rate: 0,
            sample_rate,
        }
    }

    /// Serialise to the 24-byte big-endian wire format.
    pub fn to_bytes(self) -> [u8; SPECIFIC_CONFIG_LEN] {
        let mut out = [0u8; SPECIFIC_CONFIG_LEN];
        out[0..4].copy_from_slice(&self.frame_length.to_be_bytes());
        out[4] = self.compatible_version;
        out[5] = self.bit_depth;
        out[6] = self.pb;
        out[7] = self.mb;
        out[8] = self.kb;
        out[9] = self.num_channels;
        out[10..12].copy_from_slice(&self.max_run.to_be_bytes());
        out[12..16].copy_from_slice(&self.max_frame_bytes.to_be_bytes());
        out[16..20].copy_from_slice(&self.avg_bit_rate.to_be_bytes());
        out[20..24].copy_from_slice(&self.sample_rate.to_be_bytes());
        out
    }

    /// Parse the mandatory 24-byte portion of an ALAC magic cookie.
    /// `bytes` must be at least 24 bytes long; any trailing
    /// `ALACChannelLayoutInfo` (24 bytes) is ignored at this layer.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < SPECIFIC_CONFIG_LEN {
            return None;
        }
        Some(Self {
            frame_length: u32::from_be_bytes(bytes[0..4].try_into().ok()?),
            compatible_version: bytes[4],
            bit_depth: bytes[5],
            pb: bytes[6],
            mb: bytes[7],
            kb: bytes[8],
            num_channels: bytes[9],
            max_run: u16::from_be_bytes(bytes[10..12].try_into().ok()?),
            max_frame_bytes: u32::from_be_bytes(bytes[12..16].try_into().ok()?),
            avg_bit_rate: u32::from_be_bytes(bytes[16..20].try_into().ok()?),
            sample_rate: u32::from_be_bytes(bytes[20..24].try_into().ok()?),
        })
    }
}

/// Map a PCM bit depth (8 / 16 / 20 / 24 / 32) to the corresponding
/// AudioFormatFlags value used in an Apple Lossless ASBD's
/// `format_flags` field.
pub fn bit_depth_flag(bit_depth: u8) -> Option<u32> {
    match bit_depth {
        16 => Some(crate::sys::K_AF_APPLE_LOSSLESS_16_BIT),
        20 => Some(crate::sys::K_AF_APPLE_LOSSLESS_20_BIT),
        24 => Some(crate::sys::K_AF_APPLE_LOSSLESS_24_BIT),
        32 => Some(crate::sys::K_AF_APPLE_LOSSLESS_32_BIT),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_specific_config() {
        let cfg = AlacSpecificConfig::new(48_000, 2, 16);
        let bytes = cfg.to_bytes();
        let parsed = AlacSpecificConfig::parse(&bytes).expect("parse");
        assert_eq!(parsed, cfg);
        assert_eq!(bytes.len(), SPECIFIC_CONFIG_LEN);

        // Spot-check byte layout for the documented field offsets.
        assert_eq!(&bytes[0..4], &4096u32.to_be_bytes()); // frame_length
        assert_eq!(bytes[5], 16); // bit_depth
        assert_eq!(bytes[6], 40); // pb
        assert_eq!(bytes[7], 10); // mb
        assert_eq!(bytes[8], 14); // kb
        assert_eq!(bytes[9], 2); // num_channels
        assert_eq!(&bytes[10..12], &255u16.to_be_bytes()); // max_run
        assert_eq!(&bytes[20..24], &48_000u32.to_be_bytes()); // sample_rate
    }

    #[test]
    fn parse_rejects_short_input() {
        assert!(AlacSpecificConfig::parse(&[0u8; 23]).is_none());
        assert!(AlacSpecificConfig::parse(&[0u8; 24]).is_some());
    }

    #[test]
    fn bit_depth_flag_table() {
        assert_eq!(bit_depth_flag(16), Some(1));
        assert_eq!(bit_depth_flag(20), Some(2));
        assert_eq!(bit_depth_flag(24), Some(3));
        assert_eq!(bit_depth_flag(32), Some(4));
        assert_eq!(bit_depth_flag(8), None);
    }
}
