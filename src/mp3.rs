//! MPEG-1 / MPEG-2 / MPEG-2.5 Audio Layer III frame-header parser.
//!
//! Every MPEG audio elementary stream is a concatenation of self-
//! describing frames whose first 32 bits encode everything the decoder
//! needs to walk to the next frame: format version, layer, error-
//! protection flag, bitrate index, sample-rate index, padding bit,
//! channel mode and a handful of policy bits (private / copyright /
//! original / emphasis).
//!
//! Bit layout per ISO/IEC 11172-3 §2.4.2.3 / ISO/IEC 13818-3 §2.4.2.3
//! (LSB-of-each-field column on the right, big-endian wire order):
//!
//! ```text
//!  byte 0       byte 1       byte 2       byte 3
//!  ┌────────┬─┬─┬─┬──┬──┬──┬──┬─┬─┬──┬──┬─┬─┬──┐
//!  │ syncword          │id│lyr│p│ bitrate │ sr │d│v│ mode │mx│c│o│ em │
//!  │ 11 bits (0xFFE)   │1 │ 2 │1│  4 bits │ 2  │1│1│  2   │ 2│1│1│ 2  │
//!  └────────┴─┴─┴─┴──┴──┴──┴──┴─┴─┴──┴──┴─┴─┴──┘
//! ```
//!
//! `id` is the **MPEG version ID** field. MPEG-1 uses `0b1` (with the
//! adjacent `version_ext` bit also `1`), MPEG-2 uses `0b0` with the
//! ext bit `1`, and the Fraunhofer **MPEG-2.5** extension reuses the
//! `id`/ext = `0b0`/`0b0` slot that ISO 11172 left reserved (see
//! `docs/audio/mp3/MPEG-2.5-GAP.md` — staged from EBU Technical
//! Review 283 + USPTO RE44,897). The `bitrate` and `sample_rate_index`
//! field meanings then depend on the resolved (version × layer) pair.
//!
//! This module only does the **header walk** — given any 4-byte
//! aligned candidate it tells the caller (a) is this a valid header,
//! (b) what's the next-frame distance in bytes, (c) what's the codec
//! configuration (so a `kAudioConverterErr_FormatNotSupported` can be
//! diagnosed before it fires inside AudioConverter). PCM synthesis,
//! Huffman decode and IMDCT are all on the AudioToolbox side.

/// MPEG version derived from the `id` + `version_ext` header bits.
///
/// MPEG-1 covers the original ISO/IEC 11172-3 sample-rate set
/// (32 / 44.1 / 48 kHz). MPEG-2 LSF covers the ISO/IEC 13818-3 half-
/// rate extension (16 / 22.05 / 24 kHz). MPEG-2.5 covers the
/// Fraunhofer low-rate extension (8 / 11.025 / 12 kHz).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    /// MPEG-1 — ISO/IEC 11172-3 (sample rates 32 / 44.1 / 48 kHz).
    Mpeg1,
    /// MPEG-2 LSF — ISO/IEC 13818-3 (sample rates 16 / 22.05 / 24 kHz).
    Mpeg2,
    /// MPEG-2.5 — Fraunhofer extension (sample rates 8 / 11.025 / 12 kHz).
    Mpeg25,
}

/// MPEG audio layer (1 / 2 / 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    /// Layer I (MPEG audio Layer I).
    LayerI,
    /// Layer II (MPEG audio Layer II).
    LayerII,
    /// Layer III (MPEG audio Layer III — "MP3").
    LayerIII,
}

/// Channel-mode field decoded from the header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelMode {
    /// Stereo (two independently-coded channels).
    Stereo,
    /// Joint stereo (MS- and/or intensity-stereo coding).
    JointStereo,
    /// Two independent mono channels carried together.
    DualMono,
    /// Single-channel mono.
    Mono,
}

impl ChannelMode {
    /// Number of channels carried in this mode (mono → 1, all others → 2).
    pub fn channel_count(self) -> u32 {
        match self {
            ChannelMode::Mono => 1,
            _ => 2,
        }
    }
}

/// Decoded MPEG audio frame header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    /// Resolved MPEG version (1 / 2 / 2.5).
    pub version: Version,
    /// Resolved layer (I / II / III).
    pub layer: Layer,
    /// CRC-16 protection bit set? (`0` in the header means "protection
    /// present", `1` means "no protection" — Rust bool inverted for
    /// caller convenience: `true` = CRC follows the header).
    pub crc_protected: bool,
    /// Bitrate in bits per second (already decoded — `None` is the
    /// reserved index for "free format").
    pub bit_rate: u32,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Padding bit (1 ⇒ one extra slot in this frame).
    pub padding: bool,
    /// Channel mode (stereo / joint-stereo / dual-mono / mono).
    pub channel_mode: ChannelMode,
    /// Total frame length in bytes (header + side-info + payload +
    /// padding slot, **including** the 4-byte header itself).
    pub frame_length: usize,
    /// PCM samples per channel produced by decoding this frame.
    pub samples_per_frame: u32,
}

impl FrameHeader {
    /// Number of channels (1 for mono, 2 otherwise).
    pub fn channels(&self) -> u32 {
        self.channel_mode.channel_count()
    }

    /// Attempt to parse a 32-bit MPEG-audio frame header.
    ///
    /// Returns `Some(_)` only if every field decodes to a defined
    /// value: a real sync word, a non-reserved layer, a non-reserved
    /// version, a bitrate index that's in the per-(version, layer)
    /// table, and a sample-rate index that's not `0b11`. Free-format
    /// streams (bitrate index `0b0000`) are intentionally rejected
    /// here — the rest of the crate has no story for them and
    /// AudioConverter doesn't support them either.
    pub fn parse(bytes: [u8; 4]) -> Option<Self> {
        // Sync word: 11 bits of all-ones (0xFFE).
        let sync_hi = bytes[0];
        let sync_lo = bytes[1] >> 5; // top three bits of byte 1 complete the syncword
        if sync_hi != 0xFF || sync_lo != 0b111 {
            return None;
        }

        // version_ext (bit 4 of byte 1) + id (bit 3) jointly select the
        // version. Per ISO/IEC 11172-3 §2.4.2.3 + MPEG-2.5 extension
        // (datavoyage table at docs/audio/mp3/datavoyage-mpgscript-
        // mpeghdr.html):
        //
        //   ext = 1, id = 1 → MPEG-1
        //   ext = 1, id = 0 → MPEG-2 LSF
        //   ext = 0, id = 0 → MPEG-2.5
        //   ext = 0, id = 1 → reserved (reject)
        let version_ext = (bytes[1] >> 4) & 0b1;
        let id = (bytes[1] >> 3) & 0b1;
        let version = match (version_ext, id) {
            (1, 1) => Version::Mpeg1,
            (1, 0) => Version::Mpeg2,
            (0, 0) => Version::Mpeg25,
            _ => return None,
        };

        // Layer field, bits 2..1 of byte 1. `00` is reserved.
        let layer_bits = (bytes[1] >> 1) & 0b11;
        let layer = match layer_bits {
            0b11 => Layer::LayerI,
            0b10 => Layer::LayerII,
            0b01 => Layer::LayerIII,
            _ => return None,
        };

        // Protection bit (bit 0 of byte 1). 0 → CRC follows header; 1 → no CRC.
        let crc_protected = (bytes[1] & 0b1) == 0;

        // Bitrate index, top 4 bits of byte 2. `0000` is free format,
        // `1111` is reserved. We reject both.
        let bitrate_index = (bytes[2] >> 4) & 0b1111;
        let bit_rate = bitrate_lookup(version, layer, bitrate_index)?;

        // Sample-rate index, bits 3..2 of byte 2. `11` is reserved.
        let sr_index = (bytes[2] >> 2) & 0b11;
        let sample_rate = sample_rate_lookup(version, sr_index)?;

        // Padding bit, bit 1 of byte 2.
        let padding = ((bytes[2] >> 1) & 0b1) == 1;

        // Channel-mode field, top 2 bits of byte 3.
        let ch_bits = (bytes[3] >> 6) & 0b11;
        let channel_mode = match ch_bits {
            0b00 => ChannelMode::Stereo,
            0b01 => ChannelMode::JointStereo,
            0b10 => ChannelMode::DualMono,
            0b11 => ChannelMode::Mono,
            _ => unreachable!(),
        };

        // PCM samples per frame per (version × layer). Derived directly
        // from ISO 11172-3 §2.4.2.1 (Layer-III "1152 samples per frame
        // on MPEG-1") + §2.4.2.1 footnote-style: MPEG-2 LSF / MPEG-2.5
        // halve Layer-III to 576 samples per frame.
        let samples_per_frame = samples_per_frame(version, layer);

        let frame_length = frame_length_bytes(layer, bit_rate, sample_rate, padding);

        Some(FrameHeader {
            version,
            layer,
            crc_protected,
            bit_rate,
            sample_rate,
            padding,
            channel_mode,
            frame_length,
            samples_per_frame,
        })
    }
}

/// PCM samples per channel for a frame at the given (version × layer).
///
/// ISO/IEC 11172-3 §2.4.2.1 + 13818-3 §2.4.2.1: Layer I always 384;
/// Layer II always 1152; Layer III is 1152 on MPEG-1 but 576 on the
/// half-rate MPEG-2 / MPEG-2.5 variants.
fn samples_per_frame(version: Version, layer: Layer) -> u32 {
    match layer {
        Layer::LayerI => 384,
        Layer::LayerII => 1152,
        Layer::LayerIII => match version {
            Version::Mpeg1 => 1152,
            Version::Mpeg2 | Version::Mpeg25 => 576,
        },
    }
}

/// Compute the on-wire frame length in bytes.
///
/// Closed-form per ISO/IEC 11172-3 §2.4.3.1: for a Layer-I frame the
/// length is `(12 * br / sr + padding) * 4`; for Layer-II / -III it's
/// `(samples_per_frame * br / 8) / sr + padding`. Here `samples_per_
/// frame` is the per-(version × layer) value from `samples_per_frame`.
fn frame_length_bytes(layer: Layer, bit_rate: u32, sample_rate: u32, padding: bool) -> usize {
    let pad = if padding { 1u32 } else { 0 };
    let len = match layer {
        Layer::LayerI => {
            // 12 * bit_rate / sample_rate + padding (slots of 4 bytes).
            ((12 * bit_rate / sample_rate) + pad) * 4
        }
        Layer::LayerII => {
            // 144 * bit_rate / sample_rate + padding (slots of 1 byte).
            // Layer II always uses 1152 samples per frame; 1152/8 = 144.
            (144 * bit_rate / sample_rate) + pad
        }
        Layer::LayerIII => {
            // Layer III: samples-per-frame depends on version.
            // We don't have the version here, but the canonical slot
            // counts are 72 * br / sr on MPEG-2 / 2.5 (576 samples) and
            // 144 * br / sr on MPEG-1 (1152 samples). The caller passes
            // the *resolved* bit_rate and sample_rate from the per-
            // version table, so we infer the multiplier from the
            // sample_rate band: any of the MPEG-1 rates (32/44.1/48 kHz
            // → ≥ 32000) takes the 144 path; the half-rate set takes 72.
            //
            // This avoids threading `Version` through purely arithmetic
            // helpers without adding ambiguity, because the sample-rate
            // bands are disjoint:
            //   MPEG-1   → 32_000, 44_100, 48_000
            //   MPEG-2   → 16_000, 22_050, 24_000
            //   MPEG-2.5 →  8_000, 11_025, 12_000
            let mul = if sample_rate >= 32_000 { 144 } else { 72 };
            (mul * bit_rate / sample_rate) + pad
        }
    };
    len as usize
}

/// Bitrate lookup keyed by (version × layer × index).
///
/// Returns `None` for index `0b0000` (free format) and `0b1111`
/// (reserved). Tables are the canonical ISO/IEC 11172-3 §2.4.2.3
/// Table 2.4.B.2.[1-3] / 13818-3 §2.4.2.3 Table B.[7-9] values.
fn bitrate_lookup(version: Version, layer: Layer, index: u8) -> Option<u32> {
    if index == 0 || index == 0b1111 {
        return None;
    }
    let idx = (index - 1) as usize;
    // 14 entries per (version, layer) row, all in kbit/s.
    let row: &[u32; 14] = match (version, layer) {
        // MPEG-1 (ISO/IEC 11172-3 §2.4.2.3 Table 2.4.B.2.1/2/3).
        (Version::Mpeg1, Layer::LayerI) => &[
            32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
        ],
        (Version::Mpeg1, Layer::LayerII) => &[
            32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
        ],
        (Version::Mpeg1, Layer::LayerIII) => &[
            32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
        ],
        // MPEG-2 LSF + MPEG-2.5 share one bitrate table per layer
        // (ISO/IEC 13818-3 §2.4.2.3 Table B.7/8/9 — Layer-II and Layer-
        // III collapse to identical values per the LSF spec).
        (Version::Mpeg2, Layer::LayerI) | (Version::Mpeg25, Layer::LayerI) => &[
            32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
        ],
        (Version::Mpeg2, Layer::LayerII)
        | (Version::Mpeg25, Layer::LayerII)
        | (Version::Mpeg2, Layer::LayerIII)
        | (Version::Mpeg25, Layer::LayerIII) => {
            &[8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160]
        }
    };
    Some(row[idx] * 1_000)
}

/// Sample-rate lookup keyed by (version × index).
///
/// `0b11` is reserved on every version. The MPEG-2.5 row is per the
/// Fraunhofer extension (8 / 11.025 / 12 kHz) documented in
/// `docs/audio/mp3/MPEG-2.5-GAP.md` (EBU Technical Review 283 +
/// USPTO RE44,897 + datavoyage MPEG-audio-header reference).
fn sample_rate_lookup(version: Version, index: u8) -> Option<u32> {
    if index == 0b11 {
        return None;
    }
    let idx = index as usize;
    let row: &[u32; 3] = match version {
        Version::Mpeg1 => &[44_100, 48_000, 32_000],
        Version::Mpeg2 => &[22_050, 24_000, 16_000],
        Version::Mpeg25 => &[11_025, 12_000, 8_000],
    };
    Some(row[idx])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-construct a 32-bit MPEG audio header from its decoded
    /// fields. Used by tests; mirrors the bit layout documented at the
    /// top of this file.
    fn build_header(
        version: Version,
        layer: Layer,
        crc_present: bool,
        bitrate_index: u8,
        sr_index: u8,
        padding: bool,
        channel_mode_bits: u8,
    ) -> [u8; 4] {
        let (ext, id) = match version {
            Version::Mpeg1 => (1u8, 1u8),
            Version::Mpeg2 => (1u8, 0u8),
            Version::Mpeg25 => (0u8, 0u8),
        };
        let layer_bits = match layer {
            Layer::LayerI => 0b11u8,
            Layer::LayerII => 0b10u8,
            Layer::LayerIII => 0b01u8,
        };
        let proto = if crc_present { 0u8 } else { 1u8 };
        let pad = if padding { 1u8 } else { 0u8 };
        let b0 = 0xFFu8;
        let b1 = (0b111u8 << 5) | (ext << 4) | (id << 3) | (layer_bits << 1) | proto;
        let b2 = (bitrate_index << 4) | (sr_index << 2) | (pad << 1);
        let b3 = (channel_mode_bits & 0b11) << 6;
        [b0, b1, b2, b3]
    }

    #[test]
    fn parses_mpeg1_layer3_44100_128kbps_stereo() {
        // The canonical fixture header: MPEG-1 LIII, no CRC, br index
        // 9 → 128 kbit/s, sr index 0 → 44.1 kHz, no padding, stereo.
        let bytes = build_header(
            Version::Mpeg1,
            Layer::LayerIII,
            false,
            9,
            0,
            false,
            0b00, // stereo
        );
        let h = FrameHeader::parse(bytes).expect("header parse");
        assert_eq!(h.version, Version::Mpeg1);
        assert_eq!(h.layer, Layer::LayerIII);
        assert!(!h.crc_protected);
        assert_eq!(h.bit_rate, 128_000);
        assert_eq!(h.sample_rate, 44_100);
        assert!(!h.padding);
        assert_eq!(h.channel_mode, ChannelMode::Stereo);
        assert_eq!(h.samples_per_frame, 1152);
        // 144 * 128000 / 44100 = 417.96... → 417 (truncated, no pad).
        assert_eq!(h.frame_length, 417);
    }

    #[test]
    fn padding_bit_adds_one_byte_for_layer3() {
        let bytes = build_header(Version::Mpeg1, Layer::LayerIII, false, 9, 0, true, 0b00);
        let h = FrameHeader::parse(bytes).expect("header parse");
        assert!(h.padding);
        assert_eq!(h.frame_length, 418);
    }

    #[test]
    fn rejects_bad_sync_word() {
        // Sync word missing one bit.
        let bytes = [0xFE, 0xFB, 0x90, 0x00];
        assert_eq!(FrameHeader::parse(bytes), None);
    }

    #[test]
    fn rejects_reserved_version() {
        // ext = 0, id = 1 is the reserved combination.
        let bytes = [0xFF, 0b1110_1010, 0x90, 0x00];
        assert_eq!(FrameHeader::parse(bytes), None);
    }

    #[test]
    fn rejects_reserved_layer() {
        // layer bits = 0b00 is reserved.
        let bytes = build_header(Version::Mpeg1, Layer::LayerIII, false, 9, 0, false, 0b00);
        // Zero out the layer field (bits 2..1 of byte 1).
        let mut bad = bytes;
        bad[1] &= !0b110;
        assert_eq!(FrameHeader::parse(bad), None);
    }

    #[test]
    fn rejects_free_format_bitrate_index() {
        let bytes = build_header(Version::Mpeg1, Layer::LayerIII, false, 0, 0, false, 0b00);
        assert_eq!(FrameHeader::parse(bytes), None);
    }

    #[test]
    fn rejects_reserved_bitrate_index() {
        let bytes = build_header(
            Version::Mpeg1,
            Layer::LayerIII,
            false,
            0b1111,
            0,
            false,
            0b00,
        );
        assert_eq!(FrameHeader::parse(bytes), None);
    }

    #[test]
    fn rejects_reserved_sample_rate_index() {
        let bytes = build_header(Version::Mpeg1, Layer::LayerIII, false, 9, 0b11, false, 0b00);
        assert_eq!(FrameHeader::parse(bytes), None);
    }

    #[test]
    fn channel_mode_decodes_all_four_variants() {
        for (bits, mode) in [
            (0b00, ChannelMode::Stereo),
            (0b01, ChannelMode::JointStereo),
            (0b10, ChannelMode::DualMono),
            (0b11, ChannelMode::Mono),
        ] {
            let bytes = build_header(Version::Mpeg1, Layer::LayerIII, false, 9, 0, false, bits);
            let h = FrameHeader::parse(bytes).expect("header parse");
            assert_eq!(h.channel_mode, mode);
        }
        // Channel-count derivation.
        assert_eq!(ChannelMode::Mono.channel_count(), 1);
        assert_eq!(ChannelMode::Stereo.channel_count(), 2);
        assert_eq!(ChannelMode::JointStereo.channel_count(), 2);
        assert_eq!(ChannelMode::DualMono.channel_count(), 2);
    }

    #[test]
    fn mpeg2_layer3_22050_64kbps() {
        // MPEG-2 LSF rates: Layer-III @ 22.05 kHz / 64 kbit/s.
        // Bitrate row for LSF Layer-III is [8,16,24,32,40,48,56,64,...]
        // — 64 kbit/s sits at table position 7 → header bitrate_index 8.
        // Sample-rate index 0 on MPEG-2 → 22.05 kHz.
        let bytes = build_header(Version::Mpeg2, Layer::LayerIII, false, 8, 0, false, 0b00);
        let h = FrameHeader::parse(bytes).expect("header parse");
        assert_eq!(h.version, Version::Mpeg2);
        assert_eq!(h.bit_rate, 64_000);
        assert_eq!(h.sample_rate, 22_050);
        assert_eq!(h.samples_per_frame, 576);
        // 72 * 64000 / 22050 = 208.98… → 208 (truncated).
        assert_eq!(h.frame_length, 208);
    }

    #[test]
    fn mpeg25_layer3_11025_32kbps() {
        // MPEG-2.5 (Fraunhofer extension) at 11.025 kHz / 32 kbit/s mono.
        // Bitrate row for shared LSF Layer-III is [8,16,24,32,40,...] —
        // 32 kbit/s sits at table position 3 → header bitrate_index 4.
        let bytes = build_header(Version::Mpeg25, Layer::LayerIII, false, 4, 0, false, 0b11);
        let h = FrameHeader::parse(bytes).expect("header parse");
        assert_eq!(h.version, Version::Mpeg25);
        assert_eq!(h.bit_rate, 32_000);
        assert_eq!(h.sample_rate, 11_025);
        assert_eq!(h.samples_per_frame, 576);
        assert_eq!(h.channels(), 1);
        // 72 * 32000 / 11025 = 208.97… → 208.
        assert_eq!(h.frame_length, 208);
    }

    #[test]
    fn layer1_frame_length_is_quad_slot_aligned() {
        // MPEG-1 Layer I @ 32 kHz / 32 kbit/s, no padding.
        // Bitrate index → 32 kbit/s on the MPEG-1 Layer-I row is index 1.
        let bytes = build_header(Version::Mpeg1, Layer::LayerI, false, 1, 2, false, 0b00);
        let h = FrameHeader::parse(bytes).expect("header parse");
        assert_eq!(h.layer, Layer::LayerI);
        assert_eq!(h.bit_rate, 32_000);
        assert_eq!(h.sample_rate, 32_000);
        assert_eq!(h.samples_per_frame, 384);
        // (12 * 32000 / 32000) * 4 = 48 bytes.
        assert_eq!(h.frame_length, 48);
    }

    #[test]
    fn layer2_frame_length_192kbps_44100() {
        // MPEG-1 Layer II @ 44.1 kHz / 192 kbit/s stereo, padding off.
        // Bitrate index → 192 kbit/s on the MPEG-1 Layer-II row is index 10.
        let bytes = build_header(Version::Mpeg1, Layer::LayerII, false, 10, 0, false, 0b00);
        let h = FrameHeader::parse(bytes).expect("header parse");
        assert_eq!(h.layer, Layer::LayerII);
        assert_eq!(h.bit_rate, 192_000);
        assert_eq!(h.sample_rate, 44_100);
        assert_eq!(h.samples_per_frame, 1152);
        // 144 * 192000 / 44100 = 626.93… → 626.
        assert_eq!(h.frame_length, 626);
    }

    #[test]
    fn padding_bit_adds_four_bytes_for_layer1() {
        // Layer-I padding adds one *slot* of 4 bytes, not 1 byte.
        let bytes = build_header(Version::Mpeg1, Layer::LayerI, false, 1, 2, true, 0b00);
        let h = FrameHeader::parse(bytes).expect("header parse");
        // (12 * 32000 / 32000 + 1) * 4 = 52 bytes.
        assert_eq!(h.frame_length, 52);
    }

    #[test]
    fn crc_protected_bit_flips_polarity() {
        // protection bit 0 → CRC follows; 1 → no CRC.
        let with_crc = build_header(Version::Mpeg1, Layer::LayerIII, true, 9, 0, false, 0b00);
        let no_crc = build_header(Version::Mpeg1, Layer::LayerIII, false, 9, 0, false, 0b00);
        assert!(FrameHeader::parse(with_crc).unwrap().crc_protected);
        assert!(!FrameHeader::parse(no_crc).unwrap().crc_protected);
    }
}
