//! AMR-NB (Adaptive Multi-Rate Narrowband) mode + packet helpers.
//!
//! AMR-NB is a fixed 8 kHz mono speech codec (3GPP TS 26.071) with a
//! 20 ms analysis window — 160 PCM samples per packet. Each packet
//! starts with a **TOC** (Table-of-Contents) byte that picks one of
//! ten frame types:
//!
//! | Mode index | Mode      | Net bitrate | Compressed bytes / packet |
//! |------------|-----------|-------------|---------------------------|
//! | 0          | MR475     | 4.75 kbit/s | 13                        |
//! | 1          | MR515     | 5.15 kbit/s | 14                        |
//! | 2          | MR59      | 5.90 kbit/s | 16                        |
//! | 3          | MR67      | 6.70 kbit/s | 18                        |
//! | 4          | MR74      | 7.40 kbit/s | 20                        |
//! | 5          | MR795     | 7.95 kbit/s | 21                        |
//! | 6          | MR102     | 10.2 kbit/s | 27                        |
//! | 7          | MR122     | 12.2 kbit/s | 32                        |
//! | 8          | SID       | (silence)   | 6                         |
//! | 15         | NO_DATA   | (no audio)  | 1 (TOC byte only)         |
//!
//! Byte counts include the leading TOC byte. The numbers above match
//! the **storage format** documented in RFC 4867 §5.1 and 3GPP TS
//! 26.101 Annex A — the same byte stream macOS AudioToolbox accepts
//! through `kAudioFormatAMR`.
//!
//! TOC byte layout (RFC 4867 §4.3.2):
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
//! * `FT` — frame type (mode index, 0..=7 for speech, 8 for SID,
//!   15 for NO_DATA; other values are reserved).
//! * `Q`  — frame quality bit. `1` = frame is good.
//! * `PP` — padding (must be `0`).

/// AMR-NB frame type.
///
/// Variant order follows the 3GPP / RFC 4867 frame-type index so
/// `FrameType::from_toc` is a direct byte-to-enum decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameType {
    /// MR475 — 4.75 kbit/s, 13 bytes/packet.
    Mr475,
    /// MR515 — 5.15 kbit/s, 14 bytes/packet.
    Mr515,
    /// MR59 — 5.90 kbit/s, 16 bytes/packet.
    Mr59,
    /// MR67 — 6.70 kbit/s, 18 bytes/packet.
    Mr67,
    /// MR74 — 7.40 kbit/s, 20 bytes/packet.
    Mr74,
    /// MR795 — 7.95 kbit/s, 21 bytes/packet.
    Mr795,
    /// MR102 — 10.2 kbit/s, 27 bytes/packet.
    Mr102,
    /// MR122 — 12.2 kbit/s, 32 bytes/packet.
    Mr122,
    /// SID — silence descriptor frame, 6 bytes/packet.
    Sid,
    /// NO_DATA — frame skipped (typically used for DTX), 1-byte TOC only.
    NoData,
}

impl FrameType {
    /// Decode the FT (frame-type) nibble of a TOC byte.
    ///
    /// Returns `None` for reserved values (9..=14). The TOC byte
    /// layout is `F FFFF Q PP` per RFC 4867 §4.3.2.
    pub fn from_toc(toc: u8) -> Option<Self> {
        let ft = (toc >> 3) & 0x0F;
        match ft {
            0 => Some(Self::Mr475),
            1 => Some(Self::Mr515),
            2 => Some(Self::Mr59),
            3 => Some(Self::Mr67),
            4 => Some(Self::Mr74),
            5 => Some(Self::Mr795),
            6 => Some(Self::Mr102),
            7 => Some(Self::Mr122),
            8 => Some(Self::Sid),
            15 => Some(Self::NoData),
            _ => None, // 9..=14 reserved
        }
    }

    /// Compressed-packet byte count for this frame type, including the
    /// leading TOC byte (storage format, RFC 4867 §5.1).
    pub fn bytes_per_packet(self) -> usize {
        match self {
            Self::Mr475 => 13,
            Self::Mr515 => 14,
            Self::Mr59 => 16,
            Self::Mr67 => 18,
            Self::Mr74 => 20,
            Self::Mr795 => 21,
            Self::Mr102 => 27,
            Self::Mr122 => 32,
            Self::Sid => 6,
            Self::NoData => 1,
        }
    }

    /// FT (frame-type index) field as encoded in the TOC byte.
    pub fn ft_index(self) -> u8 {
        match self {
            Self::Mr475 => 0,
            Self::Mr515 => 1,
            Self::Mr59 => 2,
            Self::Mr67 => 3,
            Self::Mr74 => 4,
            Self::Mr795 => 5,
            Self::Mr102 => 6,
            Self::Mr122 => 7,
            Self::Sid => 8,
            Self::NoData => 15,
        }
    }

    /// Net bitrate in bits per second (speech modes only; SID and
    /// NO_DATA are not steady streams so return `None`).
    pub fn bit_rate(self) -> Option<u32> {
        match self {
            Self::Mr475 => Some(4_750),
            Self::Mr515 => Some(5_150),
            Self::Mr59 => Some(5_900),
            Self::Mr67 => Some(6_700),
            Self::Mr74 => Some(7_400),
            Self::Mr795 => Some(7_950),
            Self::Mr102 => Some(10_200),
            Self::Mr122 => Some(12_200),
            Self::Sid | Self::NoData => None,
        }
    }
}

/// PCM frames per AMR-NB packet — constant 160 (20 ms @ 8 kHz).
pub const FRAMES_PER_PACKET: u32 = 160;

/// Build a TOC byte for the given frame type, with `F=0` (last frame
/// in burst), `Q=1` (good frame), `PP=0` (no padding bits).
///
/// Used by tests to manufacture a syntactically-valid AMR-NB packet
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
        for ft in 0..=7 {
            let toc = (ft as u8) << 3 | 0b100;
            let parsed = FrameType::from_toc(toc).expect("valid speech mode");
            assert_eq!(parsed.ft_index(), ft as u8);
        }
    }

    #[test]
    fn from_toc_sid() {
        let toc = (8u8 << 3) | 0b100;
        assert_eq!(FrameType::from_toc(toc), Some(FrameType::Sid));
    }

    #[test]
    fn from_toc_no_data() {
        let toc = (15u8 << 3) | 0b100;
        assert_eq!(FrameType::from_toc(toc), Some(FrameType::NoData));
    }

    #[test]
    fn from_toc_reserved_returns_none() {
        for ft in 9..=14u8 {
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
        // Spell out the canonical packet sizes from RFC 4867 §5.1 so a
        // typo can't slip through.
        assert_eq!(FrameType::Mr475.bytes_per_packet(), 13);
        assert_eq!(FrameType::Mr515.bytes_per_packet(), 14);
        assert_eq!(FrameType::Mr59.bytes_per_packet(), 16);
        assert_eq!(FrameType::Mr67.bytes_per_packet(), 18);
        assert_eq!(FrameType::Mr74.bytes_per_packet(), 20);
        assert_eq!(FrameType::Mr795.bytes_per_packet(), 21);
        assert_eq!(FrameType::Mr102.bytes_per_packet(), 27);
        assert_eq!(FrameType::Mr122.bytes_per_packet(), 32);
        assert_eq!(FrameType::Sid.bytes_per_packet(), 6);
        assert_eq!(FrameType::NoData.bytes_per_packet(), 1);
    }

    #[test]
    fn bit_rate_speech_modes() {
        assert_eq!(FrameType::Mr475.bit_rate(), Some(4_750));
        assert_eq!(FrameType::Mr122.bit_rate(), Some(12_200));
        assert_eq!(FrameType::Sid.bit_rate(), None);
        assert_eq!(FrameType::NoData.bit_rate(), None);
    }

    #[test]
    fn make_toc_round_trip() {
        // make_toc + from_toc should reproduce the original frame type
        // for every defined variant.
        for ft in [
            FrameType::Mr475,
            FrameType::Mr515,
            FrameType::Mr59,
            FrameType::Mr67,
            FrameType::Mr74,
            FrameType::Mr795,
            FrameType::Mr102,
            FrameType::Mr122,
            FrameType::Sid,
            FrameType::NoData,
        ] {
            let toc = make_toc(ft);
            let parsed = FrameType::from_toc(toc).expect("round-trip");
            assert_eq!(parsed, ft);
            // F bit must be 0, Q bit must be 1.
            assert_eq!(toc & 0x80, 0, "F bit must be 0");
            assert_eq!(toc & 0x04, 0x04, "Q bit must be 1");
            assert_eq!(toc & 0x03, 0, "PP bits must be 0");
        }
    }

    #[test]
    fn frames_per_packet_constant() {
        // 20 ms at 8 kHz = 160 PCM samples.
        assert_eq!(FRAMES_PER_PACKET, 160);
    }
}
