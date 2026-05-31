//! MPEG-1/2/2.5 Audio (Layer I/II/III) frame-header parser.
//!
//! AudioToolbox can consume any MPEG audio layer through one of the
//! `kAudioFormatMPEGLayer{1,2,3}` format IDs, but the converter needs
//! to be told a consistent sample rate and channel count at
//! construction time. The on-wire bitstream carries everything we need
//! in each frame's 32-bit header:
//!
//! ```text
//!   31                                                              0
//!  +--------+--------+--------+--------+
//!  |AAAAAAAA|AAABBCCD|EEEEFFGH|IIJJKLMM|
//!  +--------+--------+--------+--------+
//!     AAAAAAAAAAA  — frame sync (11 bits, always 1)
//!     BB           — version    (00 MPEG-2.5, 01 reserved, 10 MPEG-2, 11 MPEG-1)
//!     CC           — layer      (00 reserved, 01 Layer III, 10 Layer II, 11 Layer I)
//!     D            — CRC bit    (0 = CRC follows header)
//!     EEEE         — bitrate index (per-version/per-layer table lookup)
//!     FF           — sampling-rate index
//!     G            — padding bit
//!     H            — private bit (informational)
//!     II           — channel mode (00 stereo, 01 joint-stereo, 10 dual, 11 mono)
//!     JJ           — mode extension (joint-stereo only)
//!     K            — copyright
//!     L            — original
//!     MM           — emphasis
//! ```
//!
//! Bit layout matches ISO/IEC 11172-3 §2.4.1.3 (MPEG-1) and ISO/IEC
//! 13818-3 §2.4.1.3 (MPEG-2 LSF). The "MPEG-2.5" extension at
//! version=00 is not in either standard — it was added by Fraunhofer
//! to support sample rates of 8/11.025/12 kHz; Apple's AudioToolbox
//! accepts it as an extension of Layer III.
//!
//! Frame length (in bytes) per the spec:
//!
//! * Layer I:    `(12 * BitRate / SampleRate + Padding) * 4`
//! * Layer II:   `144 * BitRate / SampleRate + Padding`
//! * Layer III:  `144 * BitRate / SampleRate + Padding`  (MPEG-1)
//!   — and       `72  * BitRate / SampleRate + Padding`  (MPEG-2 / 2.5 LSF)
//!
//! Samples per frame (constant per (version, layer) cell):
//!
//! |          | Layer I | Layer II | Layer III |
//! |----------|---------|----------|-----------|
//! | MPEG-1   |     384 |     1152 |     1152  |
//! | MPEG-2   |     384 |     1152 |     576   |
//! | MPEG-2.5 |     384 |     1152 |     576   |

/// MPEG audio version field (the 2 bits at positions 19..=20).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    /// MPEG-2.5 (unofficial Fraunhofer extension; sample rates 8/11.025/12 kHz).
    Mpeg25,
    /// MPEG-2 LSF (Low Sampling Frequency; rates 16/22.05/24 kHz).
    Mpeg2,
    /// MPEG-1 (rates 32/44.1/48 kHz).
    Mpeg1,
}

impl Version {
    /// Decode the 2-bit version field (byte 1, bits 4..=3 of a frame
    /// header). Returns `None` for the reserved value `01`.
    pub fn from_bits(bits: u8) -> Option<Self> {
        match bits & 0b11 {
            0b00 => Some(Self::Mpeg25),
            0b10 => Some(Self::Mpeg2),
            0b11 => Some(Self::Mpeg1),
            _ => None, // 0b01 reserved
        }
    }
}

/// MPEG audio layer (the 2 bits at positions 17..=18).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    /// Layer I — 384-sample frames; legacy DCC / VideoCD audio.
    Layer1,
    /// Layer II — 1152-sample frames; broadcast-quality (MUSICAM heritage).
    Layer2,
    /// Layer III — 1152 (MPEG-1) / 576 (MPEG-2 LSF) sample frames; this is MP3.
    Layer3,
}

impl Layer {
    /// Decode the 2-bit layer field. Returns `None` for the reserved
    /// value `00`. Note the encoding is INVERTED from the variant
    /// order (`11` = Layer I, `01` = Layer III).
    pub fn from_bits(bits: u8) -> Option<Self> {
        match bits & 0b11 {
            0b11 => Some(Self::Layer1),
            0b10 => Some(Self::Layer2),
            0b01 => Some(Self::Layer3),
            _ => None, // 0b00 reserved
        }
    }
}

/// Channel mode (bits 7..=6 of the third header byte).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelMode {
    Stereo,
    JointStereo,
    DualChannel,
    Mono,
}

impl ChannelMode {
    /// Decode the 2-bit channel-mode field.
    pub fn from_bits(bits: u8) -> Self {
        match bits & 0b11 {
            0b00 => Self::Stereo,
            0b01 => Self::JointStereo,
            0b10 => Self::DualChannel,
            _ => Self::Mono,
        }
    }

    /// Channel count this mode delivers to the decoder.
    pub fn channel_count(self) -> u16 {
        match self {
            Self::Mono => 1,
            _ => 2,
        }
    }
}

/// Bitrate table per ISO/IEC 11172-3 §2.4.2.3 (MPEG-1) and ISO/IEC
/// 13818-3 §2.4.2.3 (MPEG-2 LSF). Index 0 is the "free format" sentinel
/// (returned as `None`); index 15 is reserved (also `None`). All values
/// are kilobits/s.
///
/// Selector tuple is `(version, layer)`.
fn bitrate_kbps(version: Version, layer: Layer, idx: u8) -> Option<u32> {
    if idx == 0 || idx == 15 {
        return None;
    }
    let i = (idx as usize) - 1;
    // 14 valid bitrate values (indexes 1..=14).
    let table: &[u32; 14] = match (version, layer) {
        // MPEG-1 Layer I.
        (Version::Mpeg1, Layer::Layer1) => &[
            32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
        ],
        // MPEG-1 Layer II.
        (Version::Mpeg1, Layer::Layer2) => &[
            32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
        ],
        // MPEG-1 Layer III.
        (Version::Mpeg1, Layer::Layer3) => &[
            32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
        ],
        // MPEG-2 / 2.5 Layer I (single shared table per 13818-3).
        (Version::Mpeg2 | Version::Mpeg25, Layer::Layer1) => &[
            32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
        ],
        // MPEG-2 / 2.5 Layer II + Layer III share the same table.
        (Version::Mpeg2 | Version::Mpeg25, Layer::Layer2 | Layer::Layer3) => {
            &[8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160]
        }
    };
    Some(table[i] * 1000)
}

/// Sampling-rate table. Selector is `(version, idx)`; idx=3 is reserved.
fn sample_rate_hz(version: Version, idx: u8) -> Option<u32> {
    if idx >= 3 {
        return None;
    }
    let table: [u32; 3] = match version {
        Version::Mpeg1 => [44_100, 48_000, 32_000],
        Version::Mpeg2 => [22_050, 24_000, 16_000],
        Version::Mpeg25 => [11_025, 12_000, 8_000],
    };
    Some(table[idx as usize])
}

/// PCM samples a single frame at this (version, layer) emits.
pub fn samples_per_frame(version: Version, layer: Layer) -> u32 {
    match (version, layer) {
        (_, Layer::Layer1) => 384,
        (Version::Mpeg1, Layer::Layer2) => 1152,
        (Version::Mpeg1, Layer::Layer3) => 1152,
        (Version::Mpeg2 | Version::Mpeg25, Layer::Layer2) => 1152,
        (Version::Mpeg2 | Version::Mpeg25, Layer::Layer3) => 576,
    }
}

/// Decoded MPEG audio frame header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub version: Version,
    pub layer: Layer,
    /// CRC-protection flag (true = no CRC); ignored at the converter
    /// level since AT skips the optional 16-bit CRC itself.
    pub no_crc: bool,
    pub bitrate: u32,
    pub sample_rate: u32,
    pub padding: bool,
    pub channel_mode: ChannelMode,
}

impl FrameHeader {
    /// Parse a 32-bit MPEG audio frame header from its on-wire bytes.
    ///
    /// Returns `None` if any field is reserved / malformed (sync
    /// missing, reserved version / layer, reserved bitrate-index 15,
    /// reserved sampling-rate-index 3, free-format bitrate-index 0).
    pub fn parse(bytes: [u8; 4]) -> Option<Self> {
        // Sync — top 11 bits of the 32-bit header must all be 1.
        // bytes[0] = AAAA AAAA → all 8 must be 1
        // bytes[1] = AAAB BCCD → top 3 must be 1
        if bytes[0] != 0xFF {
            return None;
        }
        if (bytes[1] & 0xE0) != 0xE0 {
            return None;
        }

        let version = Version::from_bits((bytes[1] >> 3) & 0b11)?;
        let layer = Layer::from_bits((bytes[1] >> 1) & 0b11)?;
        let no_crc = (bytes[1] & 0x01) != 0;

        let br_idx = (bytes[2] >> 4) & 0x0F;
        let bitrate = bitrate_kbps(version, layer, br_idx)?;

        let sr_idx = (bytes[2] >> 2) & 0x03;
        let sample_rate = sample_rate_hz(version, sr_idx)?;

        let padding = (bytes[2] & 0x02) != 0;
        let channel_mode = ChannelMode::from_bits((bytes[3] >> 6) & 0b11);

        Some(Self {
            version,
            layer,
            no_crc,
            bitrate,
            sample_rate,
            padding,
            channel_mode,
        })
    }

    /// PCM samples this frame's payload emits when decoded (per the
    /// (version, layer) lookup).
    pub fn samples(self) -> u32 {
        samples_per_frame(self.version, self.layer)
    }

    /// Total frame length in bytes — header + (optional CRC) + payload
    /// + padding slot.
    ///
    /// Matches the formulas in ISO/IEC 11172-3 §2.4.3.1 (Layer I uses
    /// 4-byte slots; Layer II / III use 1-byte slots), extended to
    /// MPEG-2 LSF Layer III which uses 72 instead of 144 as the
    /// multiplier because each LSF frame has a single granule.
    pub fn frame_length(self) -> u32 {
        let pad = u32::from(self.padding);
        let br = self.bitrate;
        let sr = self.sample_rate;
        match (self.version, self.layer) {
            (_, Layer::Layer1) => (12 * br / sr + pad) * 4,
            (Version::Mpeg1, Layer::Layer2 | Layer::Layer3) => 144 * br / sr + pad,
            (Version::Mpeg2 | Version::Mpeg25, Layer::Layer2) => 144 * br / sr + pad,
            (Version::Mpeg2 | Version::Mpeg25, Layer::Layer3) => 72 * br / sr + pad,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 4-byte header for the given (version, layer, bitrate-idx,
    /// sr-idx, padding, channel-mode) tuple. Sync + reserved bits zeroed.
    fn build(
        ver_bits: u8,
        layer_bits: u8,
        no_crc: bool,
        br_idx: u8,
        sr_idx: u8,
        pad: bool,
        mode_bits: u8,
    ) -> [u8; 4] {
        let b0 = 0xFF;
        let b1 = 0xE0 | ((ver_bits & 0b11) << 3) | ((layer_bits & 0b11) << 1) | u8::from(no_crc);
        let b2 = ((br_idx & 0x0F) << 4) | ((sr_idx & 0b11) << 2) | (u8::from(pad) << 1);
        let b3 = (mode_bits & 0b11) << 6;
        [b0, b1, b2, b3]
    }

    #[test]
    fn parses_mpeg1_layer3_128kbps_44_1khz_stereo() {
        // The canonical "MP3 128k 44.1k stereo" header.
        // ver=11 (MPEG-1), layer=01 (LIII), no-crc=1, br_idx=9 (128k),
        // sr_idx=0 (44_100), padding=0, mode=00 (stereo).
        let bytes = build(0b11, 0b01, true, 9, 0, false, 0b00);
        let h = FrameHeader::parse(bytes).expect("must parse");
        assert_eq!(h.version, Version::Mpeg1);
        assert_eq!(h.layer, Layer::Layer3);
        assert_eq!(h.bitrate, 128_000);
        assert_eq!(h.sample_rate, 44_100);
        assert!(!h.padding);
        assert_eq!(h.channel_mode, ChannelMode::Stereo);
        // Frame length: 144 * 128000 / 44100 + 0 = 417.
        assert_eq!(h.frame_length(), 417);
        assert_eq!(h.samples(), 1152);
    }

    #[test]
    fn parses_mpeg1_layer3_128kbps_44_1khz_with_padding() {
        let bytes = build(0b11, 0b01, true, 9, 0, true, 0b00);
        let h = FrameHeader::parse(bytes).expect("parse");
        // Padding slot is one byte for Layer III.
        assert_eq!(h.frame_length(), 418);
    }

    #[test]
    fn parses_mpeg2_layer3_lsf() {
        // MPEG-2 Layer III at 64 kbit/s 22.05 kHz mono — typical for
        // low-bitrate streaming content.
        // ver=10 (MPEG-2), layer=01 (LIII), br_idx=8 (64k per LSF L3),
        // sr_idx=0 (22050), mode=11 (mono).
        let bytes = build(0b10, 0b01, true, 8, 0, false, 0b11);
        let h = FrameHeader::parse(bytes).expect("parse");
        assert_eq!(h.version, Version::Mpeg2);
        assert_eq!(h.layer, Layer::Layer3);
        assert_eq!(h.bitrate, 64_000);
        assert_eq!(h.sample_rate, 22_050);
        assert_eq!(h.channel_mode, ChannelMode::Mono);
        // LSF Layer III samples = 576.
        assert_eq!(h.samples(), 576);
        // 72 * 64000 / 22050 = 209.
        assert_eq!(h.frame_length(), 72 * 64_000 / 22_050);
    }

    #[test]
    fn parses_mpeg25_layer3() {
        // MPEG-2.5 Layer III at 8 kbit/s 8 kHz mono — extreme low end
        // used by some voice-mail bitstreams.
        // ver=00 (MPEG-2.5), layer=01 (LIII), br_idx=1 (8k), sr_idx=2 (8000).
        let bytes = build(0b00, 0b01, true, 1, 2, false, 0b11);
        let h = FrameHeader::parse(bytes).expect("parse");
        assert_eq!(h.version, Version::Mpeg25);
        assert_eq!(h.layer, Layer::Layer3);
        assert_eq!(h.bitrate, 8_000);
        assert_eq!(h.sample_rate, 8_000);
        assert_eq!(h.samples(), 576);
    }

    #[test]
    fn parses_mpeg1_layer2_192kbps_48khz_stereo() {
        // MPEG-1 Layer II at 192 kbit/s 48 kHz stereo — broadcast preset.
        // ver=11, layer=10 (LII), br_idx=10 (192k LII), sr_idx=1 (48000).
        let bytes = build(0b11, 0b10, true, 10, 1, false, 0b00);
        let h = FrameHeader::parse(bytes).expect("parse");
        assert_eq!(h.layer, Layer::Layer2);
        assert_eq!(h.bitrate, 192_000);
        assert_eq!(h.sample_rate, 48_000);
        // 144 * 192000 / 48000 = 576.
        assert_eq!(h.frame_length(), 576);
        assert_eq!(h.samples(), 1152);
    }

    #[test]
    fn parses_mpeg1_layer1_32khz() {
        // MPEG-1 Layer I at 256 kbit/s 32 kHz stereo.
        // ver=11, layer=11 (LI), br_idx=8 (256k LI).
        let bytes = build(0b11, 0b11, true, 8, 2, false, 0b00);
        let h = FrameHeader::parse(bytes).expect("parse");
        assert_eq!(h.layer, Layer::Layer1);
        assert_eq!(h.bitrate, 256_000);
        assert_eq!(h.sample_rate, 32_000);
        // (12 * 256000 / 32000 + 0) * 4 = 384.
        assert_eq!(h.frame_length(), 384);
        assert_eq!(h.samples(), 384);
    }

    #[test]
    fn rejects_missing_sync() {
        // Top 11 bits not all 1 → not a header.
        let bytes = [0xFE, 0xFF, 0x00, 0x00];
        assert!(FrameHeader::parse(bytes).is_none());
    }

    #[test]
    fn rejects_reserved_version() {
        // ver=01 reserved per ISO/IEC 13818-3.
        let bytes = build(0b01, 0b01, true, 9, 0, false, 0b00);
        assert!(FrameHeader::parse(bytes).is_none());
    }

    #[test]
    fn rejects_reserved_layer() {
        // layer=00 reserved.
        let bytes = build(0b11, 0b00, true, 9, 0, false, 0b00);
        assert!(FrameHeader::parse(bytes).is_none());
    }

    #[test]
    fn rejects_free_format_bitrate() {
        // br_idx=0 is "free format" — we don't accept it (the frame
        // length isn't recoverable without external state). Some
        // pathological streams encode this; AT would refuse to drive a
        // converter from one anyway.
        let bytes = build(0b11, 0b01, true, 0, 0, false, 0b00);
        assert!(FrameHeader::parse(bytes).is_none());
    }

    #[test]
    fn rejects_reserved_bitrate_index() {
        let bytes = build(0b11, 0b01, true, 15, 0, false, 0b00);
        assert!(FrameHeader::parse(bytes).is_none());
    }

    #[test]
    fn rejects_reserved_sample_rate_index() {
        let bytes = build(0b11, 0b01, true, 9, 3, false, 0b00);
        assert!(FrameHeader::parse(bytes).is_none());
    }

    #[test]
    fn channel_mode_count_table() {
        assert_eq!(ChannelMode::Stereo.channel_count(), 2);
        assert_eq!(ChannelMode::JointStereo.channel_count(), 2);
        assert_eq!(ChannelMode::DualChannel.channel_count(), 2);
        assert_eq!(ChannelMode::Mono.channel_count(), 1);
    }

    #[test]
    fn samples_per_frame_matrix() {
        assert_eq!(samples_per_frame(Version::Mpeg1, Layer::Layer1), 384);
        assert_eq!(samples_per_frame(Version::Mpeg1, Layer::Layer2), 1152);
        assert_eq!(samples_per_frame(Version::Mpeg1, Layer::Layer3), 1152);
        assert_eq!(samples_per_frame(Version::Mpeg2, Layer::Layer1), 384);
        assert_eq!(samples_per_frame(Version::Mpeg2, Layer::Layer2), 1152);
        assert_eq!(samples_per_frame(Version::Mpeg2, Layer::Layer3), 576);
        assert_eq!(samples_per_frame(Version::Mpeg25, Layer::Layer3), 576);
    }
}
