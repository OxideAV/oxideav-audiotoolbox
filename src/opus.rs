//! Opus stream-level helpers for the AudioToolbox bridge.
//!
//! Everything here is wall-isolated from third-party Opus code: the
//! layout comes from **RFC 6716** (the Opus codec spec,
//! `docs/audio/opus/rfc6716-opus.txt`) and **RFC 7845** (Ogg-Opus
//! encapsulation, `docs/audio/opus/rfc7845-ogg-opus.txt`). The
//! AudioConverter slot handles the entire SILK / Hybrid / CELT decode
//! and encode internally — we only need two things on this side of
//! the FFI boundary:
//!
//! 1. **`Toc`** — parse the one-byte TOC (Table-of-Contents) header
//!    per RFC 6716 §3.1: 5-bit `config` selector, 1-bit stereo flag,
//!    2-bit packet `code`. The config selects the audio mode
//!    (SILK / Hybrid / CELT), the audio bandwidth (NB / MB / WB /
//!    SWB / FB), and the frame size in milliseconds.
//! 2. **`OpusHead`** — parse and assemble the RFC 7845 §5.1
//!    Identification Header (mandatory Ogg-Opus stream initialisation
//!    packet). The 19-byte minimum body is what AT accepts as the
//!    `kAudioConverterDecompressionMagicCookie` payload for Opus —
//!    discovered empirically by probing the converter against this
//!    exact layout. AT additionally vends its own 28-byte cookie shape
//!    from the encoder side via
//!    `kAudioConverterCompressionMagicCookie` which decoders also
//!    accept (parsed via [`parse_at_compression_cookie`]).
//!
//! The TOC parser is conservative — it only validates the config
//! enum and exposes the resolved (mode / bandwidth / frame_size_ms)
//! triple. AT itself walks the per-packet framing (code 0..=3) and
//! the inner per-frame entropy coder.
//!
//! ## RFC references
//!
//! * RFC 6716 §3.1 — TOC byte layout (`config` × 32, `s` × 1, `c` × 2).
//! * RFC 6716 Table 2 — config → (mode, bandwidth, frame size).
//! * RFC 7845 §5.1 — Identification Header (19-byte body + optional
//!   channel-mapping table for multistream).
//! * RFC 8251 — Opus update (errata + clarifications, no on-wire change).

/// Length of the canonical OpusHead body (RFC 7845 §5.1) when
/// `channel_mapping_family == 0` — the only case that doesn't require
/// the trailing channel-mapping table.
pub const OPUS_HEAD_MIN_LEN: usize = 19;

/// Length of the OpusHead magic ("OpusHead").
pub const OPUS_HEAD_MAGIC_LEN: usize = 8;

/// The 8-byte ASCII signature at the start of every OpusHead packet.
pub const OPUS_HEAD_MAGIC: [u8; OPUS_HEAD_MAGIC_LEN] = *b"OpusHead";

/// Length of AT's compression-side magic cookie. Discovered empirically
/// by reading `kAudioConverterCompressionMagicCookie` back from a
/// freshly configured Opus encoder converter.
pub const AT_COMPRESSION_COOKIE_LEN: usize = 28;

/// Opus encoder/decoder mode (RFC 6716 §2.1.4, Table 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpusMode {
    /// SILK-only (linear-prediction speech coder).
    Silk,
    /// Hybrid SILK + CELT.
    Hybrid,
    /// CELT-only (MDCT-based music/low-latency coder).
    Celt,
}

/// Opus audio bandwidth (RFC 6716 §2.1.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpusBandwidth {
    /// Narrowband — 4 kHz cutoff, internal 8 kHz.
    Nb,
    /// Mediumband — 6 kHz cutoff, internal 12 kHz.
    Mb,
    /// Wideband — 8 kHz cutoff, internal 16 kHz.
    Wb,
    /// Super-wideband — 12 kHz cutoff, internal 24 kHz.
    Swb,
    /// Fullband — 20 kHz cutoff, internal 48 kHz.
    Fb,
}

impl OpusBandwidth {
    /// Internal sample rate of the bandwidth in Hz.
    pub fn internal_sample_rate(self) -> u32 {
        match self {
            OpusBandwidth::Nb => 8_000,
            OpusBandwidth::Mb => 12_000,
            OpusBandwidth::Wb => 16_000,
            OpusBandwidth::Swb => 24_000,
            OpusBandwidth::Fb => 48_000,
        }
    }
}

/// TOC code field — number of frames per packet (RFC 6716 §3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketCode {
    /// Code 0: one frame in the packet.
    Single,
    /// Code 1: two frames of equal size.
    DoubleEqual,
    /// Code 2: two frames of unequal size (first size lacing-encoded).
    DoubleUnequal,
    /// Code 3: arbitrary frame count (1..=48) with optional padding.
    Arbitrary,
}

impl PacketCode {
    /// Resolve a 2-bit `c` field.
    pub fn from_bits(bits: u8) -> Self {
        match bits & 0b11 {
            0 => PacketCode::Single,
            1 => PacketCode::DoubleEqual,
            2 => PacketCode::DoubleUnequal,
            _ => PacketCode::Arbitrary,
        }
    }
}

/// Decoded TOC byte (RFC 6716 §3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Toc {
    /// 5-bit `config` value (0..=31).
    pub config: u8,
    /// Mode selected by `config`.
    pub mode: OpusMode,
    /// Bandwidth selected by `config`.
    pub bandwidth: OpusBandwidth,
    /// Per-frame duration in tenths of a millisecond (so the 2.5 ms
    /// CELT frame is representable as `25`, the maximum 60 ms SILK
    /// frame as `600`). This keeps the spec's full-precision frame
    /// sizes inside an integer.
    pub frame_duration_tenths_ms: u16,
    /// `s` bit — 0 = mono, 1 = stereo.
    pub stereo: bool,
    /// `c` field — packet shape.
    pub code: PacketCode,
}

impl Toc {
    /// Parse the 1-byte TOC field.
    ///
    /// Returns `None` only if `bytes` is empty — every 5-bit `config`
    /// value 0..=31 is valid per RFC 6716 Table 2.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let b = *bytes.first()?;
        let config = b >> 3;
        let stereo = (b >> 2) & 1 == 1;
        let code = PacketCode::from_bits(b & 0b11);
        let (mode, bandwidth, frame_duration_tenths_ms) = config_to_params(config);
        Some(Toc {
            config,
            mode,
            bandwidth,
            frame_duration_tenths_ms,
            stereo,
            code,
        })
    }

    /// Channel count signalled by the TOC `s` bit (mono = 1, stereo = 2).
    pub fn channels(&self) -> u8 {
        if self.stereo {
            2
        } else {
            1
        }
    }

    /// Per-frame PCM sample count when the decoder runs at 48 kHz (the
    /// canonical AT output rate per RFC 7845 §5.1). 48 PCM samples per
    /// ms × tenths/10 = `frame_duration_tenths_ms * 48 / 10`.
    pub fn frame_size_at_48khz(&self) -> u32 {
        (self.frame_duration_tenths_ms as u32 * 48) / 10
    }
}

/// Resolve a 5-bit config to (mode, bandwidth, frame_duration ×10).
///
/// Table 2 of RFC 6716 §3.1:
///
/// | config range | mode    | bandwidths      | frame sizes (ms)        |
/// |--------------|---------|-----------------|-------------------------|
/// | 0..=3        | SILK    | NB              | 10, 20, 40, 60          |
/// | 4..=7        | SILK    | MB              | 10, 20, 40, 60          |
/// | 8..=11       | SILK    | WB              | 10, 20, 40, 60          |
/// | 12..=13      | Hybrid  | SWB             | 10, 20                  |
/// | 14..=15      | Hybrid  | FB              | 10, 20                  |
/// | 16..=19      | CELT    | NB              | 2.5, 5, 10, 20          |
/// | 20..=23      | CELT    | WB              | 2.5, 5, 10, 20          |
/// | 24..=27      | CELT    | SWB             | 2.5, 5, 10, 20          |
/// | 28..=31      | CELT    | FB              | 2.5, 5, 10, 20          |
fn config_to_params(config: u8) -> (OpusMode, OpusBandwidth, u16) {
    // SILK rows: 4 entries each at 10/20/40/60 ms (×10 = 100/200/400/600).
    const SILK_DURATIONS: [u16; 4] = [100, 200, 400, 600];
    // Hybrid rows: 2 entries at 10/20 ms.
    const HYBRID_DURATIONS: [u16; 2] = [100, 200];
    // CELT rows: 4 entries at 2.5/5/10/20 ms (×10 = 25/50/100/200).
    const CELT_DURATIONS: [u16; 4] = [25, 50, 100, 200];

    match config {
        0..=3 => (
            OpusMode::Silk,
            OpusBandwidth::Nb,
            SILK_DURATIONS[(config & 3) as usize],
        ),
        4..=7 => (
            OpusMode::Silk,
            OpusBandwidth::Mb,
            SILK_DURATIONS[(config & 3) as usize],
        ),
        8..=11 => (
            OpusMode::Silk,
            OpusBandwidth::Wb,
            SILK_DURATIONS[(config & 3) as usize],
        ),
        12..=13 => (
            OpusMode::Hybrid,
            OpusBandwidth::Swb,
            HYBRID_DURATIONS[(config & 1) as usize],
        ),
        14..=15 => (
            OpusMode::Hybrid,
            OpusBandwidth::Fb,
            HYBRID_DURATIONS[(config & 1) as usize],
        ),
        16..=19 => (
            OpusMode::Celt,
            OpusBandwidth::Nb,
            CELT_DURATIONS[(config & 3) as usize],
        ),
        20..=23 => (
            OpusMode::Celt,
            OpusBandwidth::Wb,
            CELT_DURATIONS[(config & 3) as usize],
        ),
        24..=27 => (
            OpusMode::Celt,
            OpusBandwidth::Swb,
            CELT_DURATIONS[(config & 3) as usize],
        ),
        28..=31 => (
            OpusMode::Celt,
            OpusBandwidth::Fb,
            CELT_DURATIONS[(config & 3) as usize],
        ),
        _ => unreachable!("config is 5-bit, masked to 0..=31"),
    }
}

/// Decoded RFC 7845 §5.1 Identification Header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpusHead {
    /// Always 1 in this version of the spec.
    pub version: u8,
    /// Output channel count (1..=255).
    pub channel_count: u8,
    /// Samples (at 48 kHz) to discard from the start of decoded output
    /// to align with the first valid PCM sample (RFC 7845 §4.2).
    pub pre_skip: u16,
    /// Original input sample rate in Hz (informational only — the
    /// decoder always produces 48 kHz output, see RFC 7845 §5.1).
    pub input_sample_rate: u32,
    /// Q7.8 output gain in dB (signed; 0 means no adjustment).
    pub output_gain: i16,
    /// 0 = RTP (mono+stereo only); 1 = ordered 1..=8 channels;
    /// 255 = ambisonic / pass-through. Non-zero families add a trailing
    /// channel-mapping table per RFC 7845 §5.1.1 — preserved in
    /// `mapping_table` below.
    pub channel_mapping_family: u8,
    /// Optional RFC 7845 §5.1.1 channel mapping table when
    /// `channel_mapping_family != 0`: `[stream_count, coupled_count,
    /// mapping[channels]]`.
    pub mapping_table: Vec<u8>,
}

impl Default for OpusHead {
    fn default() -> Self {
        Self {
            version: 1,
            channel_count: 2,
            pre_skip: 0,
            input_sample_rate: 48_000,
            output_gain: 0,
            channel_mapping_family: 0,
            mapping_table: Vec::new(),
        }
    }
}

impl OpusHead {
    /// Build a mono OpusHead with default fields (version=1, no pre-skip,
    /// channel_mapping_family=0).
    pub fn mono(input_sample_rate: u32) -> Self {
        Self {
            version: 1,
            channel_count: 1,
            pre_skip: 0,
            input_sample_rate,
            output_gain: 0,
            channel_mapping_family: 0,
            mapping_table: Vec::new(),
        }
    }

    /// Build a stereo OpusHead with default fields.
    pub fn stereo(input_sample_rate: u32) -> Self {
        Self {
            version: 1,
            channel_count: 2,
            pre_skip: 0,
            input_sample_rate,
            output_gain: 0,
            channel_mapping_family: 0,
            mapping_table: Vec::new(),
        }
    }

    /// Parse an OpusHead packet body (with or without the leading
    /// "OpusHead" magic — both shapes are accepted).
    ///
    /// Returns `None` if the body is too short, the version major nibble
    /// is incompatible, or the channel-mapping table is truncated.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let body = if bytes.len() >= OPUS_HEAD_MAGIC_LEN
            && &bytes[..OPUS_HEAD_MAGIC_LEN] == OPUS_HEAD_MAGIC.as_slice()
        {
            &bytes[OPUS_HEAD_MAGIC_LEN..]
        } else {
            bytes
        };
        if body.len() < OPUS_HEAD_MIN_LEN - OPUS_HEAD_MAGIC_LEN {
            return None;
        }
        let version = body[0];
        // RFC 7845 §5.1: accept any minor version (upper nibble must
        // match the published major).
        if (version >> 4) != 0 {
            return None;
        }
        let channel_count = body[1];
        if channel_count == 0 {
            return None;
        }
        let pre_skip = u16::from_le_bytes([body[2], body[3]]);
        let input_sample_rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
        let output_gain = i16::from_le_bytes([body[8], body[9]]);
        let channel_mapping_family = body[10];

        let mapping_table = if channel_mapping_family == 0 {
            Vec::new()
        } else {
            let needed = 2 + channel_count as usize;
            if body.len() < 11 + needed {
                return None;
            }
            body[11..11 + needed].to_vec()
        };

        Some(OpusHead {
            version,
            channel_count,
            pre_skip,
            input_sample_rate,
            output_gain,
            channel_mapping_family,
            mapping_table,
        })
    }

    /// Serialise to the on-wire body form (without the "OpusHead"
    /// magic) — the layout AT accepts as the decompression magic
    /// cookie.
    pub fn to_body_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(OPUS_HEAD_MIN_LEN - OPUS_HEAD_MAGIC_LEN);
        out.push(self.version);
        out.push(self.channel_count);
        out.extend_from_slice(&self.pre_skip.to_le_bytes());
        out.extend_from_slice(&self.input_sample_rate.to_le_bytes());
        out.extend_from_slice(&self.output_gain.to_le_bytes());
        out.push(self.channel_mapping_family);
        if self.channel_mapping_family != 0 {
            out.extend_from_slice(&self.mapping_table);
        }
        out
    }

    /// Serialise to the full on-wire packet form (with the
    /// "OpusHead" 8-byte ASCII magic prefix).
    pub fn to_packet_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(OPUS_HEAD_MIN_LEN);
        out.extend_from_slice(&OPUS_HEAD_MAGIC);
        out.extend_from_slice(&self.to_body_bytes());
        out
    }
}

/// AT's encoder-vended Opus magic cookie. Discovered empirically by
/// reading `kAudioConverterCompressionMagicCookie` back from a freshly
/// configured Opus encoder converter — the 28-byte payload has the
/// following layout (all integers big-endian):
///
/// ```text
/// offset  size  field                  notes
/// 0       4     reserved/version       observed: 0x00000800
/// 4       4     sample_rate (Hz)       e.g. 0x0000bb80 = 48000
/// 8       4     frames_per_packet      e.g. 0x000003c0 = 960
/// 12      4     pre_skip-ish (signed)  observed: 0xfffffc18 = -1000
/// 16      4     channel_count          e.g. 0x00000002
/// 20      8     trailing zeros
/// ```
///
/// The cookie can be passed verbatim to a decoder converter via
/// `kAudioConverterDecompressionMagicCookie` and AT accepts it
/// (round-trip validated). The shape is AT-specific and does NOT
/// follow the RFC 7845 §5.1 OpusHead layout — different field order
/// and endianness — so we parse it explicitly here rather than
/// reusing `OpusHead::parse`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AtCompressionCookie {
    /// Sample rate field (Hz, big-endian u32 at offset 4).
    pub sample_rate: u32,
    /// Frames-per-packet field (big-endian u32 at offset 8).
    pub frames_per_packet: u32,
    /// Channel count field (big-endian u32 at offset 16).
    pub channel_count: u32,
}

/// Parse AT's 28-byte compression-side cookie. Returns `None` if the
/// length doesn't match.
pub fn parse_at_compression_cookie(cookie: &[u8]) -> Option<AtCompressionCookie> {
    if cookie.len() < AT_COMPRESSION_COOKIE_LEN {
        return None;
    }
    let sample_rate = u32::from_be_bytes([cookie[4], cookie[5], cookie[6], cookie[7]]);
    let frames_per_packet = u32::from_be_bytes([cookie[8], cookie[9], cookie[10], cookie[11]]);
    let channel_count = u32::from_be_bytes([cookie[16], cookie[17], cookie[18], cookie[19]]);
    Some(AtCompressionCookie {
        sample_rate,
        frames_per_packet,
        channel_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─────── TOC ───────

    #[test]
    fn toc_silk_nb_10ms_mono_code0() {
        // config=0 (SILK NB 10 ms), s=0 (mono), c=0
        let toc = Toc::parse(&[0x00]).expect("parse");
        assert_eq!(toc.config, 0);
        assert_eq!(toc.mode, OpusMode::Silk);
        assert_eq!(toc.bandwidth, OpusBandwidth::Nb);
        assert_eq!(toc.frame_duration_tenths_ms, 100); // 10 ms
        assert!(!toc.stereo);
        assert_eq!(toc.channels(), 1);
        assert_eq!(toc.code, PacketCode::Single);
        assert_eq!(toc.frame_size_at_48khz(), 480);
    }

    #[test]
    fn toc_silk_wb_20ms_stereo_code0() {
        // config=9 (SILK WB 20 ms), s=1 (stereo), c=0
        // TOC = (9<<3) | (1<<2) | 0 = 0x4C
        let toc = Toc::parse(&[0x4C]).expect("parse");
        assert_eq!(toc.config, 9);
        assert_eq!(toc.mode, OpusMode::Silk);
        assert_eq!(toc.bandwidth, OpusBandwidth::Wb);
        assert_eq!(toc.frame_duration_tenths_ms, 200);
        assert!(toc.stereo);
        assert_eq!(toc.channels(), 2);
    }

    #[test]
    fn toc_celt_fb_20ms_stereo_code0() {
        // config=31 (CELT FB 20 ms), s=1, c=0
        // TOC = (31<<3) | (1<<2) | 0 = 0xFC
        let toc = Toc::parse(&[0xFC]).expect("parse");
        assert_eq!(toc.config, 31);
        assert_eq!(toc.mode, OpusMode::Celt);
        assert_eq!(toc.bandwidth, OpusBandwidth::Fb);
        assert_eq!(toc.frame_duration_tenths_ms, 200);
        assert!(toc.stereo);
        assert_eq!(toc.code, PacketCode::Single);
        assert_eq!(toc.frame_size_at_48khz(), 960);
    }

    #[test]
    fn toc_celt_fb_2_5ms_low_latency() {
        // config=28 (CELT FB 2.5 ms), s=0, c=0 -> (28<<3)|0|0 = 0xE0
        let toc = Toc::parse(&[0xE0]).expect("parse");
        assert_eq!(toc.config, 28);
        assert_eq!(toc.mode, OpusMode::Celt);
        assert_eq!(toc.bandwidth, OpusBandwidth::Fb);
        assert_eq!(toc.frame_duration_tenths_ms, 25); // 2.5 ms
        assert_eq!(toc.frame_size_at_48khz(), 120);
    }

    #[test]
    fn toc_hybrid_swb_10ms_mono() {
        // config=12 (Hybrid SWB 10 ms)
        // TOC = (12<<3)|0|0 = 0x60
        let toc = Toc::parse(&[0x60]).expect("parse");
        assert_eq!(toc.mode, OpusMode::Hybrid);
        assert_eq!(toc.bandwidth, OpusBandwidth::Swb);
        assert_eq!(toc.frame_duration_tenths_ms, 100);
    }

    #[test]
    fn toc_silk_max_frame_60ms_mb() {
        // config=7 (SILK MB 60 ms)
        // TOC = (7<<3)|0|0 = 0x38
        let toc = Toc::parse(&[0x38]).expect("parse");
        assert_eq!(toc.mode, OpusMode::Silk);
        assert_eq!(toc.bandwidth, OpusBandwidth::Mb);
        assert_eq!(toc.frame_duration_tenths_ms, 600); // 60 ms (max SILK)
        assert_eq!(toc.frame_size_at_48khz(), 2880);
    }

    #[test]
    fn toc_packet_codes_decode_correctly() {
        // config=31, stereo=0 — vary the low 2 bits
        assert_eq!(Toc::parse(&[0xF8]).unwrap().code, PacketCode::Single);
        assert_eq!(Toc::parse(&[0xF9]).unwrap().code, PacketCode::DoubleEqual);
        assert_eq!(Toc::parse(&[0xFA]).unwrap().code, PacketCode::DoubleUnequal);
        assert_eq!(Toc::parse(&[0xFB]).unwrap().code, PacketCode::Arbitrary);
    }

    #[test]
    fn toc_empty_input_rejected() {
        assert!(Toc::parse(&[]).is_none());
    }

    #[test]
    fn bandwidth_internal_sample_rates() {
        assert_eq!(OpusBandwidth::Nb.internal_sample_rate(), 8_000);
        assert_eq!(OpusBandwidth::Mb.internal_sample_rate(), 12_000);
        assert_eq!(OpusBandwidth::Wb.internal_sample_rate(), 16_000);
        assert_eq!(OpusBandwidth::Swb.internal_sample_rate(), 24_000);
        assert_eq!(OpusBandwidth::Fb.internal_sample_rate(), 48_000);
    }

    // ─────── OpusHead ───────

    #[test]
    fn opushead_roundtrip_stereo() {
        let h = OpusHead {
            version: 1,
            channel_count: 2,
            pre_skip: 312,
            input_sample_rate: 48_000,
            output_gain: 0,
            channel_mapping_family: 0,
            mapping_table: Vec::new(),
        };
        let body = h.to_body_bytes();
        assert_eq!(body.len(), OPUS_HEAD_MIN_LEN - OPUS_HEAD_MAGIC_LEN);
        let parsed = OpusHead::parse(&body).expect("parse body-only");
        assert_eq!(parsed, h);
    }

    #[test]
    fn opushead_roundtrip_with_magic_prefix() {
        let h = OpusHead::stereo(48_000);
        let pkt = h.to_packet_bytes();
        assert_eq!(pkt.len(), OPUS_HEAD_MIN_LEN);
        assert_eq!(&pkt[..OPUS_HEAD_MAGIC_LEN], b"OpusHead");
        let parsed = OpusHead::parse(&pkt).expect("parse with magic");
        assert_eq!(parsed, h);
    }

    #[test]
    fn opushead_mono_default() {
        let h = OpusHead::mono(48_000);
        assert_eq!(h.channel_count, 1);
        assert_eq!(h.input_sample_rate, 48_000);
        assert_eq!(h.channel_mapping_family, 0);
    }

    #[test]
    fn opushead_rejects_zero_channels() {
        let mut h = OpusHead::stereo(48_000);
        h.channel_count = 0;
        let body = h.to_body_bytes();
        assert!(OpusHead::parse(&body).is_none());
    }

    #[test]
    fn opushead_rejects_too_short() {
        let buf = [0u8; 5];
        assert!(OpusHead::parse(&buf).is_none());
    }

    #[test]
    fn opushead_rejects_truncated_mapping_table() {
        // family=1 implies a trailing 2 + channel_count bytes; truncate
        // them to trigger the bounds check.
        let mut h = OpusHead::stereo(48_000);
        h.channel_mapping_family = 1;
        h.mapping_table = vec![1, 0, 0, 1]; // valid for stereo
        let mut bytes = h.to_body_bytes();
        bytes.truncate(bytes.len() - 2); // drop trailing 2 mapping bytes
        assert!(OpusHead::parse(&bytes).is_none());
    }

    #[test]
    fn opushead_round_trips_5_1_multistream() {
        let h = OpusHead {
            version: 1,
            channel_count: 6,
            pre_skip: 312,
            input_sample_rate: 48_000,
            output_gain: 0,
            channel_mapping_family: 1,
            // [stream_count, coupled_count, mapping[6]]
            mapping_table: vec![4, 2, 0, 4, 1, 2, 3, 5],
        };
        let body = h.to_body_bytes();
        let parsed = OpusHead::parse(&body).expect("parse 5.1");
        assert_eq!(parsed, h);
    }

    #[test]
    fn opushead_le_field_layout() {
        // Spell out the canonical byte layout for one fixture so a
        // future re-org of `to_body_bytes` can't silently re-order
        // fields.
        let h = OpusHead {
            version: 1,
            channel_count: 2,
            pre_skip: 0,
            input_sample_rate: 48_000, // 0xBB80
            output_gain: 0,
            channel_mapping_family: 0,
            mapping_table: Vec::new(),
        };
        let body = h.to_body_bytes();
        assert_eq!(body[0], 1); // version
        assert_eq!(body[1], 2); // channels
        assert_eq!(&body[2..4], &[0, 0]); // pre_skip LE
        assert_eq!(&body[4..8], &[0x80, 0xBB, 0, 0]); // 48000 LE
        assert_eq!(&body[8..10], &[0, 0]); // gain
        assert_eq!(body[10], 0); // family
    }

    // ─────── AT compression cookie ───────

    #[test]
    fn parse_at_compression_cookie_stereo_48k_20ms() {
        // Verbatim byte capture from
        // `kAudioConverterCompressionMagicCookie` on a stereo 48 kHz
        // 20 ms encoder converter.
        let cookie = [
            0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0xBB, 0x80, 0x00, 0x00, 0x03, 0xC0, 0xFF, 0xFF,
            0xFC, 0x18, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let c = parse_at_compression_cookie(&cookie).expect("parse AT cookie");
        assert_eq!(c.sample_rate, 48_000);
        assert_eq!(c.frames_per_packet, 960);
        assert_eq!(c.channel_count, 2);
    }

    #[test]
    fn parse_at_compression_cookie_mono_48k_20ms() {
        // Verbatim byte capture from a mono 48 kHz 20 ms encoder.
        let cookie = [
            0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0xBB, 0x80, 0x00, 0x00, 0x03, 0xC0, 0xFF, 0xFF,
            0xFC, 0x18, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let c = parse_at_compression_cookie(&cookie).expect("parse mono cookie");
        assert_eq!(c.channel_count, 1);
    }

    #[test]
    fn parse_at_compression_cookie_rejects_short() {
        let cookie = [0u8; 10];
        assert!(parse_at_compression_cookie(&cookie).is_none());
    }
}
