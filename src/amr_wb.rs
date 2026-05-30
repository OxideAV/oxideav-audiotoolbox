//! AMR-WB (Adaptive Multi-Rate Wideband) mode + packet helpers.
//!
//! AMR-WB is a fixed 16 kHz mono speech codec (3GPP TS 26.171 / TS
//! 26.201) with a 20 ms analysis window — 320 PCM samples per packet.
//! Each packet starts with a **TOC** (Table-of-Contents) byte that
//! picks one of eleven frame types:
//!
//! | Mode index | Mode      | Net bitrate  | Compressed bytes / packet |
//! |------------|-----------|--------------|---------------------------|
//! | 0          | MR660     | 6.60 kbit/s  | 17                        |
//! | 1          | MR885     | 8.85 kbit/s  | 23                        |
//! | 2          | MR1265    | 12.65 kbit/s | 32                        |
//! | 3          | MR1425    | 14.25 kbit/s | 36                        |
//! | 4          | MR1585    | 15.85 kbit/s | 40                        |
//! | 5          | MR1825    | 18.25 kbit/s | 46                        |
//! | 6          | MR1985    | 19.85 kbit/s | 50                        |
//! | 7          | MR2305    | 23.05 kbit/s | 58                        |
//! | 8          | MR2385    | 23.85 kbit/s | 60                        |
//! | 9          | SID       | (silence)    | 6                         |
//! | 15         | NO_DATA   | (no audio)   | 1 (TOC byte only)         |
//!
//! Byte counts include the leading TOC byte. The numbers above match
//! the **storage format** documented in RFC 4867 §5.3 — the byte
//! count for each speech mode is `ceil(payload_bits / 8) + 1` where
//! `payload_bits` is the body length given in 3GPP TS 26.201 Table 2
//! (132, 177, 253, 285, 317, 365, 397, 461, 477 bits) and the `+1` is
//! the TOC byte. The same byte stream is what macOS AudioToolbox
//! accepts through `kAudioFormatAMR_WB`.
//!
//! TOC byte layout (RFC 4867 §4.3.2 — shared with AMR-NB):
//!
//! ```text
//! +---+---+---+---+---+---+---+---+
//! | F |   FT (4 bits)   | Q | P | P |
//! +---+---+---+---+---+---+---+---+
//!   7   6   5   4   3   2   1   0
//! ```
//!
//! * `F`  — follow-up bit. `1` = another frame follows in the same
//!   "speech burst" (RFC 4867 storage mode); `0` = last frame.
//! * `FT` — frame type (mode index, 0..=8 for speech, 9 for SID,
//!   15 for NO_DATA; other values 10..=14 are reserved/SPEECH_LOST
//!   and not accepted by Apple's decoder).
//! * `Q`  — frame quality bit. `1` = frame is good.
//! * `PP` — padding (must be `0`).
//!
//! Note that the SID frame for AMR-WB is at **FT = 9** rather than
//! FT = 8 like AMR-NB — the speech-mode index range is one wider
//! (0..=8 vs 0..=7) because AMR-WB defines nine speech modes.

/// AMR-WB frame type.
///
/// Variant order follows the 3GPP / RFC 4867 frame-type index so
/// `FrameType::from_toc` is a direct byte-to-enum decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameType {
    /// MR660 — 6.60 kbit/s, 17 bytes/packet.
    Mr660,
    /// MR885 — 8.85 kbit/s, 23 bytes/packet.
    Mr885,
    /// MR1265 — 12.65 kbit/s, 32 bytes/packet.
    Mr1265,
    /// MR1425 — 14.25 kbit/s, 36 bytes/packet.
    Mr1425,
    /// MR1585 — 15.85 kbit/s, 40 bytes/packet.
    Mr1585,
    /// MR1825 — 18.25 kbit/s, 46 bytes/packet.
    Mr1825,
    /// MR1985 — 19.85 kbit/s, 50 bytes/packet.
    Mr1985,
    /// MR2305 — 23.05 kbit/s, 58 bytes/packet.
    Mr2305,
    /// MR2385 — 23.85 kbit/s, 60 bytes/packet.
    Mr2385,
    /// SID — silence descriptor frame, 6 bytes/packet (FT = 9).
    Sid,
    /// NO_DATA — frame skipped (typically used for DTX), 1-byte TOC only.
    NoData,
}

impl FrameType {
    /// Decode the FT (frame-type) nibble of a TOC byte.
    ///
    /// Returns `None` for values reserved per RFC 4867 §4.3.2
    /// (10..=14 for AMR-WB — note this differs from AMR-NB where
    /// only 9..=14 are reserved).
    pub fn from_toc(toc: u8) -> Option<Self> {
        let ft = (toc >> 3) & 0x0F;
        match ft {
            0 => Some(Self::Mr660),
            1 => Some(Self::Mr885),
            2 => Some(Self::Mr1265),
            3 => Some(Self::Mr1425),
            4 => Some(Self::Mr1585),
            5 => Some(Self::Mr1825),
            6 => Some(Self::Mr1985),
            7 => Some(Self::Mr2305),
            8 => Some(Self::Mr2385),
            9 => Some(Self::Sid),
            15 => Some(Self::NoData),
            _ => None, // 10..=14 reserved / SPEECH_LOST etc.
        }
    }

    /// Compressed-packet byte count for this frame type, including the
    /// leading TOC byte (storage format, RFC 4867 §5.3).
    pub fn bytes_per_packet(self) -> usize {
        match self {
            Self::Mr660 => 17,
            Self::Mr885 => 23,
            Self::Mr1265 => 32,
            Self::Mr1425 => 36,
            Self::Mr1585 => 40,
            Self::Mr1825 => 46,
            Self::Mr1985 => 50,
            Self::Mr2305 => 58,
            Self::Mr2385 => 60,
            Self::Sid => 6,
            Self::NoData => 1,
        }
    }

    /// FT (frame-type index) field as encoded in the TOC byte.
    pub fn ft_index(self) -> u8 {
        match self {
            Self::Mr660 => 0,
            Self::Mr885 => 1,
            Self::Mr1265 => 2,
            Self::Mr1425 => 3,
            Self::Mr1585 => 4,
            Self::Mr1825 => 5,
            Self::Mr1985 => 6,
            Self::Mr2305 => 7,
            Self::Mr2385 => 8,
            Self::Sid => 9,
            Self::NoData => 15,
        }
    }

    /// Net bitrate in bits per second (speech modes only; SID and
    /// NO_DATA are not steady streams so return `None`).
    pub fn bit_rate(self) -> Option<u32> {
        match self {
            Self::Mr660 => Some(6_600),
            Self::Mr885 => Some(8_850),
            Self::Mr1265 => Some(12_650),
            Self::Mr1425 => Some(14_250),
            Self::Mr1585 => Some(15_850),
            Self::Mr1825 => Some(18_250),
            Self::Mr1985 => Some(19_850),
            Self::Mr2305 => Some(23_050),
            Self::Mr2385 => Some(23_850),
            Self::Sid | Self::NoData => None,
        }
    }
}

/// PCM frames per AMR-WB packet — constant 320 (20 ms @ 16 kHz).
pub const FRAMES_PER_PACKET: u32 = 320;

/// Build a TOC byte for the given frame type, with `F=0` (last frame
/// in burst), `Q=1` (good frame), `PP=0` (no padding bits).
///
/// Used by tests to manufacture a syntactically-valid AMR-WB packet
/// stream without requiring an external encoder.
pub fn make_toc(ft: FrameType) -> u8 {
    // F=0 (single-frame burst), FT = ft.ft_index(), Q=1, PP=00.
    // Layout: 0 FFFF 1 00 → (ft << 3) | 0b100.
    (ft.ft_index() << 3) | 0b100
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_toc_speech_modes() {
        // F=0, Q=1, PP=00 means TOC = (ft << 3) | 0b100 = (ft << 3) | 4.
        // AMR-WB has 9 speech modes (0..=8) — one more than AMR-NB.
        for ft in 0..=8 {
            let toc = (ft as u8) << 3 | 0b100;
            let parsed = FrameType::from_toc(toc).expect("valid speech mode");
            assert_eq!(parsed.ft_index(), ft as u8);
        }
    }

    #[test]
    fn from_toc_sid() {
        // AMR-WB's SID lives at FT = 9, not FT = 8 like AMR-NB.
        let toc = (9u8 << 3) | 0b100;
        assert_eq!(FrameType::from_toc(toc), Some(FrameType::Sid));
    }

    #[test]
    fn from_toc_no_data() {
        let toc = (15u8 << 3) | 0b100;
        assert_eq!(FrameType::from_toc(toc), Some(FrameType::NoData));
    }

    #[test]
    fn from_toc_reserved_returns_none() {
        // AMR-WB reserves FT = 10..=14 (one fewer reserved value than
        // AMR-NB because AMR-WB has nine speech modes vs eight).
        for ft in 10..=14u8 {
            let toc = (ft << 3) | 0b100;
            assert_eq!(
                FrameType::from_toc(toc),
                None,
                "ft={ft} must be reserved → None"
            );
        }
    }

    #[test]
    fn bytes_per_packet_table() {
        // Spell out the canonical packet sizes from RFC 4867 §5.3 so a
        // typo can't slip through. Each value is `ceil(payload_bits/8) + 1`
        // where payload_bits is from 3GPP TS 26.201 (132, 177, 253, 285,
        // 317, 365, 397, 461, 477) plus the TOC byte.
        assert_eq!(FrameType::Mr660.bytes_per_packet(), 17);
        assert_eq!(FrameType::Mr885.bytes_per_packet(), 23);
        assert_eq!(FrameType::Mr1265.bytes_per_packet(), 32);
        assert_eq!(FrameType::Mr1425.bytes_per_packet(), 36);
        assert_eq!(FrameType::Mr1585.bytes_per_packet(), 40);
        assert_eq!(FrameType::Mr1825.bytes_per_packet(), 46);
        assert_eq!(FrameType::Mr1985.bytes_per_packet(), 50);
        assert_eq!(FrameType::Mr2305.bytes_per_packet(), 58);
        assert_eq!(FrameType::Mr2385.bytes_per_packet(), 60);
        assert_eq!(FrameType::Sid.bytes_per_packet(), 6);
        assert_eq!(FrameType::NoData.bytes_per_packet(), 1);
    }

    #[test]
    fn bit_rate_speech_modes() {
        assert_eq!(FrameType::Mr660.bit_rate(), Some(6_600));
        assert_eq!(FrameType::Mr2385.bit_rate(), Some(23_850));
        assert_eq!(FrameType::Sid.bit_rate(), None);
        assert_eq!(FrameType::NoData.bit_rate(), None);
    }

    #[test]
    fn make_toc_round_trip() {
        // make_toc + from_toc should reproduce the original frame type
        // for every defined variant.
        for ft in [
            FrameType::Mr660,
            FrameType::Mr885,
            FrameType::Mr1265,
            FrameType::Mr1425,
            FrameType::Mr1585,
            FrameType::Mr1825,
            FrameType::Mr1985,
            FrameType::Mr2305,
            FrameType::Mr2385,
            FrameType::Sid,
            FrameType::NoData,
        ] {
            let toc = make_toc(ft);
            let parsed = FrameType::from_toc(toc).expect("round-trip");
            assert_eq!(parsed, ft);
            // F bit must be 0, Q bit must be 1, PP must be 0.
            assert_eq!(toc & 0x80, 0, "F bit must be 0");
            assert_eq!(toc & 0x04, 0x04, "Q bit must be 1");
            assert_eq!(toc & 0x03, 0, "PP bits must be 0");
        }
    }

    #[test]
    fn frames_per_packet_constant() {
        // 20 ms at 16 kHz = 320 PCM samples.
        assert_eq!(FRAMES_PER_PACKET, 320);
    }

    #[test]
    fn ft_index_no_overlap() {
        // Every variant must map to a distinct FT index — no two enum
        // values share a TOC encoding.
        let all = [
            FrameType::Mr660,
            FrameType::Mr885,
            FrameType::Mr1265,
            FrameType::Mr1425,
            FrameType::Mr1585,
            FrameType::Mr1825,
            FrameType::Mr1985,
            FrameType::Mr2305,
            FrameType::Mr2385,
            FrameType::Sid,
            FrameType::NoData,
        ];
        let mut seen = [false; 16];
        for ft in all {
            let idx = ft.ft_index() as usize;
            assert!(!seen[idx], "FT index {idx} used twice");
            seen[idx] = true;
        }
    }
}
