//! Opus header parser / builder (RFC 7845 §5.1 ID header — "OpusHead").
//!
//! This module is wall-isolated: it knows the OpusHead wire form per
//! RFC 7845 and nothing about the SILK / CELT bitstream itself (that
//! lives inside the AudioToolbox decoder).
//!
//! ## Wire form (RFC 7845 §5.1, Figure 2)
//!
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |      'O'      |      'p'      |      'u'      |      's'      |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |      'H'      |      'e'      |      'a'      |      'd'      |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |  Version = 1  | Channel Count |           Pre-skip            |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                     Input Sample Rate (Hz)                    |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |   Output Gain (Q7.8 in dB)    | Mapping Family|               |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+               :
//! |                                                               |
//! :               Optional Channel Mapping Table...               :
//! |                                                               |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```
//!
//! All multi-byte fields are **little-endian** (RFC 7845 §5.1, bullets
//! 4–6). When Mapping Family = 0 the channel mapping table is omitted
//! (RFC 7845 §5.1.1.1) and the total OpusHead length is **19 bytes**.
//!
//! ## Magic-cookie role
//!
//! `AudioConverterSetProperty(kAudioConverterDecompressionMagicCookie,
//! …)` accepts the raw OpusHead bytes — this is the same identification
//! header an Ogg-Opus stream stores in its first packet (RFC 7845 §3).
//! The AT decoder reads the channel count, pre-skip, and mapping
//! family out of the cookie; the per-packet TOC byte (RFC 6716 §3.1)
//! supplies bandwidth + frame-size metadata at decode time.

/// Wall-isolated error type for OpusHead parsing. Stays out of the
/// `oxideav_core::Error` namespace so the `opus` module can compile
/// with `--no-default-features` (no `oxideav-core` dep). The
/// `opus_decoder` / `opus_encoder` modules — which ARE gated behind
/// `registry` — wrap this into `oxideav_core::Error::invalid` at the
/// API boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusHeadError(pub String);

impl std::fmt::Display for OpusHeadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for OpusHeadError {}

/// Magic signature bytes — first 8 octets of every OpusHead.
pub const MAGIC: &[u8; 8] = b"OpusHead";

/// Length of an OpusHead with mapping family 0 (no channel mapping
/// table). Per RFC 7845 §5.1: 8-byte magic + 1 version + 1 channel
/// count + 2 pre-skip + 4 input sample rate + 2 output gain +
/// 1 mapping family = 19 bytes.
pub const HEAD_LEN_FAMILY_0: usize = 19;

/// Parsed OpusHead structure (RFC 7845 §5.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpusHead {
    /// Version. MUST be 1 (RFC 7845 §5.1 bullet 2); upper nibble
    /// reserved for backwards-compatible extensions.
    pub version: u8,
    /// Output channel count `C`. MUST NOT be 0 (RFC 7845 §5.1 bullet 3).
    pub channels: u8,
    /// Pre-skip in 48 kHz samples to discard from decoder output at
    /// start of playback (RFC 7845 §5.1 bullet 4). The 80 ms / 3840-
    /// sample value is the RFC-recommended minimum for full encoder
    /// convergence.
    pub pre_skip: u16,
    /// Original input sample rate in Hz, **before** encoding. Purely
    /// informational; the decoder always outputs at one of the five
    /// supported rates (RFC 7845 §5.1 bullet 5). A value of 0 means
    /// "unspecified".
    pub input_sample_rate: u32,
    /// Output gain in Q7.8 dB (RFC 7845 §5.1 bullet 6). Applied as
    /// `sample *= pow(10, gain / (20.0 * 256))`.
    pub output_gain: i16,
    /// Channel mapping family (RFC 7845 §5.1 bullet 7 + §5.1.1):
    /// 0 = RTP mono/stereo, 1 = Vorbis-order surround, 255 = generic.
    pub mapping_family: u8,
    /// Channel mapping table tail (present when `mapping_family != 0`).
    /// Verbatim — `stream_count`, `coupled_count`, then `channels` map
    /// bytes per RFC 7845 §5.1.1.
    pub mapping_table: Vec<u8>,
}

/// Default pre-skip recommended by RFC 7845 §4.2 (80 ms at 48 kHz =
/// 3840 samples) — long enough for full convergence of the Opus
/// decoder's internal state.
pub const DEFAULT_PRE_SKIP: u16 = 3_840;

impl OpusHead {
    /// Build a minimal OpusHead for mapping family 0 (the
    /// 1-or-2-channel RTP layout) from the decoder's expected output
    /// rate and channel count. Pre-skip defaults to
    /// `DEFAULT_PRE_SKIP`, gain to 0, input sample rate echoes the
    /// output rate.
    pub fn family_0(channels: u8, sample_rate: u32) -> Self {
        Self {
            version: 1,
            channels,
            pre_skip: DEFAULT_PRE_SKIP,
            input_sample_rate: sample_rate,
            output_gain: 0,
            mapping_family: 0,
            mapping_table: Vec::new(),
        }
    }

    /// Serialise to the RFC 7845 §5.1 wire form (little-endian
    /// multi-byte fields). The returned byte vector is suitable for
    /// `AudioConverterSetProperty(kAudioConverterDecompressionMagicCookie)`
    /// — AT consumes exactly the OpusHead bytes the Ogg ID page
    /// carries.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEAD_LEN_FAMILY_0 + self.mapping_table.len());
        out.extend_from_slice(MAGIC);
        out.push(self.version);
        out.push(self.channels);
        out.extend_from_slice(&self.pre_skip.to_le_bytes());
        out.extend_from_slice(&self.input_sample_rate.to_le_bytes());
        out.extend_from_slice(&self.output_gain.to_le_bytes());
        out.push(self.mapping_family);
        out.extend_from_slice(&self.mapping_table);
        out
    }

    /// Parse an OpusHead from its wire form. Validates the magic
    /// signature, version byte, channel-count non-zero rule, and (for
    /// mapping families other than 0) the presence of the mapping
    /// table.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, OpusHeadError> {
        if buf.len() < HEAD_LEN_FAMILY_0 {
            return Err(OpusHeadError(format!(
                "OpusHead too short: {} bytes (need at least {})",
                buf.len(),
                HEAD_LEN_FAMILY_0
            )));
        }
        if &buf[0..8] != MAGIC {
            return Err(OpusHeadError(format!(
                "OpusHead magic mismatch: {:02x?} != b\"OpusHead\"",
                &buf[0..8]
            )));
        }
        let version = buf[8];
        // RFC 7845 §5.1 bullet 2: major version is the upper 4 bits;
        // values with major = 0 are accepted as backwards-compatible.
        if (version & 0xF0) != 0 {
            return Err(OpusHeadError(format!(
                "OpusHead version major bits != 0: {version:#x}"
            )));
        }
        let channels = buf[9];
        if channels == 0 {
            return Err(OpusHeadError("OpusHead channel count is 0".to_string()));
        }
        let pre_skip = u16::from_le_bytes([buf[10], buf[11]]);
        let input_sample_rate = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        let output_gain = i16::from_le_bytes([buf[16], buf[17]]);
        let mapping_family = buf[18];

        let mapping_table = if mapping_family == 0 {
            Vec::new()
        } else {
            // RFC 7845 §5.1.1: table is `stream_count` (1 B) +
            // `coupled_count` (1 B) + `channels` map bytes.
            let need = 1 + 1 + channels as usize;
            if buf.len() < HEAD_LEN_FAMILY_0 + need {
                return Err(OpusHeadError(format!(
                    "OpusHead mapping-family={mapping_family} requires {need} extra bytes; have {}",
                    buf.len().saturating_sub(HEAD_LEN_FAMILY_0)
                )));
            }
            buf[HEAD_LEN_FAMILY_0..HEAD_LEN_FAMILY_0 + need].to_vec()
        };

        Ok(Self {
            version,
            channels,
            pre_skip,
            input_sample_rate,
            output_gain,
            mapping_family,
            mapping_table,
        })
    }
}

/// Return the per-packet frame count at 48 kHz for one of the six
/// valid Opus frame sizes (RFC 6716 Table 2): 2.5 / 5 / 10 / 20 / 40
/// / 60 ms → 120 / 240 / 480 / 960 / 1920 / 2880 frames. Any other
/// duration is rejected.
pub fn frames_per_packet_48k(duration_ms: f64) -> Result<u32, OpusHeadError> {
    // (`fpp = sample_rate * duration_ms / 1000`); for 48 kHz the six
    // valid values are explicit.
    match duration_ms {
        d if (d - 2.5).abs() < 1e-9 => Ok(120),
        d if (d - 5.0).abs() < 1e-9 => Ok(240),
        d if (d - 10.0).abs() < 1e-9 => Ok(480),
        d if (d - 20.0).abs() < 1e-9 => Ok(960),
        d if (d - 40.0).abs() < 1e-9 => Ok(1920),
        d if (d - 60.0).abs() < 1e-9 => Ok(2880),
        _ => Err(OpusHeadError(format!(
            "invalid Opus frame duration {duration_ms} ms (valid: 2.5, 5, 10, 20, 40, 60)"
        ))),
    }
}

/// Default per-packet duration: 20 ms (`fpp = sample_rate * 20 / 1000`).
pub const DEFAULT_FRAME_DURATION_MS: f64 = 20.0;

/// Default per-packet frame count at 48 kHz: 20 ms × 48 kHz = 960.
pub const DEFAULT_FRAMES_PER_PACKET_48K: u32 = 960;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_0_geometry() {
        let h = OpusHead::family_0(2, 48_000);
        assert_eq!(h.version, 1);
        assert_eq!(h.channels, 2);
        assert_eq!(h.pre_skip, DEFAULT_PRE_SKIP);
        assert_eq!(h.input_sample_rate, 48_000);
        assert_eq!(h.output_gain, 0);
        assert_eq!(h.mapping_family, 0);
        assert!(h.mapping_table.is_empty());
    }

    #[test]
    fn family_0_serialises_to_19_bytes() {
        let h = OpusHead::family_0(2, 48_000);
        let bytes = h.to_bytes();
        assert_eq!(bytes.len(), HEAD_LEN_FAMILY_0);
        assert_eq!(&bytes[0..8], MAGIC);
        assert_eq!(bytes[8], 1); // version
        assert_eq!(bytes[9], 2); // channels
        assert_eq!(u16::from_le_bytes([bytes[10], bytes[11]]), DEFAULT_PRE_SKIP);
        assert_eq!(
            u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
            48_000
        );
        assert_eq!(i16::from_le_bytes([bytes[16], bytes[17]]), 0);
        assert_eq!(bytes[18], 0); // mapping family
    }

    #[test]
    fn roundtrip_family_0_mono() {
        let h = OpusHead::family_0(1, 48_000);
        let bytes = h.to_bytes();
        let parsed = OpusHead::from_bytes(&bytes).unwrap();
        assert_eq!(parsed, h);
    }

    #[test]
    fn roundtrip_family_0_stereo() {
        let h = OpusHead::family_0(2, 48_000);
        let bytes = h.to_bytes();
        let parsed = OpusHead::from_bytes(&bytes).unwrap();
        assert_eq!(parsed, h);
    }

    #[test]
    fn parse_rejects_bad_magic() {
        let mut bytes = OpusHead::family_0(1, 48_000).to_bytes();
        bytes[0] = b'X';
        assert!(OpusHead::from_bytes(&bytes).is_err());
    }

    #[test]
    fn parse_rejects_short_buf() {
        let bytes = vec![0u8; 18];
        assert!(OpusHead::from_bytes(&bytes).is_err());
    }

    #[test]
    fn parse_rejects_zero_channels() {
        let mut h = OpusHead::family_0(1, 48_000);
        h.channels = 0;
        let bytes = h.to_bytes();
        assert!(OpusHead::from_bytes(&bytes).is_err());
    }

    #[test]
    fn parse_rejects_unsupported_major_version() {
        let mut h = OpusHead::family_0(1, 48_000);
        h.version = 0x10; // major = 1 ⇒ not compatible
        let bytes = h.to_bytes();
        assert!(OpusHead::from_bytes(&bytes).is_err());
    }

    #[test]
    fn parse_family_1_round_trip() {
        // RFC 7845 §5.1.1: family 1 has a 2-byte (stream_count +
        // coupled_count) prefix plus one map byte per channel.
        let h = OpusHead {
            version: 1,
            channels: 6,
            pre_skip: DEFAULT_PRE_SKIP,
            input_sample_rate: 48_000,
            output_gain: 0,
            mapping_family: 1,
            mapping_table: vec![4, 2, 0, 4, 1, 2, 3, 5], // stream=4, coupled=2, map=[0,4,1,2,3,5]
        };
        let bytes = h.to_bytes();
        assert_eq!(bytes.len(), HEAD_LEN_FAMILY_0 + 2 + 6);
        let parsed = OpusHead::from_bytes(&bytes).unwrap();
        assert_eq!(parsed, h);
    }

    #[test]
    fn family_1_short_table_rejected() {
        // Family 1 declared but mapping table truncated.
        let mut bytes = OpusHead::family_0(2, 48_000).to_bytes();
        bytes[18] = 1; // mapping family = 1
                       // No table bytes appended → parse must fail.
        assert!(OpusHead::from_bytes(&bytes).is_err());
    }

    #[test]
    fn frames_per_packet_at_48k_covers_all_six_durations() {
        assert_eq!(frames_per_packet_48k(2.5).unwrap(), 120);
        assert_eq!(frames_per_packet_48k(5.0).unwrap(), 240);
        assert_eq!(frames_per_packet_48k(10.0).unwrap(), 480);
        assert_eq!(frames_per_packet_48k(20.0).unwrap(), 960);
        assert_eq!(frames_per_packet_48k(40.0).unwrap(), 1920);
        assert_eq!(frames_per_packet_48k(60.0).unwrap(), 2880);
    }

    #[test]
    fn frames_per_packet_rejects_invalid_duration() {
        assert!(frames_per_packet_48k(15.0).is_err());
        assert!(frames_per_packet_48k(0.0).is_err());
    }

    #[test]
    fn default_constants_are_consistent() {
        assert_eq!(
            DEFAULT_FRAMES_PER_PACKET_48K,
            frames_per_packet_48k(DEFAULT_FRAME_DURATION_MS).unwrap()
        );
    }
}
