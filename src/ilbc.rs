//! iLBC mode (frame size) selector.
//!
//! Internet Low Bitrate Codec (RFC 3951) is a fixed-format speech codec:
//!
//! | Mode  | PCM frames / packet | Compressed bytes / packet | Net bitrate |
//! |-------|---------------------|---------------------------|-------------|
//! | 20 ms | 160                 | 38                        | 15.2 kbit/s |
//! | 30 ms | 240                 | 50                        | 13.33 kbit/s |
//!
//! Both modes run at **8 kHz mono**. AudioToolbox's iLBC implementation
//! selects between the two modes purely from the `frames_per_packet`
//! field of the compressed-side `AudioStreamBasicDescription` — there
//! is no separate property and no magic cookie. This module just maps
//! a human-friendly `"20"` / `"30"` option string into the right
//! geometry constants.

/// iLBC mode (corresponds to the analysis/synthesis block length).
///
/// The 30 ms block wins for compression efficiency (13.33 kbit/s vs
/// 15.2 kbit/s) and is the default in most SIP / RTP gateway
/// deployments — hence the `#[default]` marker on the `Ms30` variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum IlbcMode {
    /// 20-millisecond block — 160 PCM samples in, 38 compressed bytes
    /// out, net 15.2 kbit/s. Lower algorithmic delay; preferred for
    /// two-way conversational telephony.
    Ms20,
    /// 30-millisecond block — 240 PCM samples in, 50 compressed bytes
    /// out, net 13.33 kbit/s. Slightly better compression, slightly
    /// higher delay; the default in many SIP/RTP deployments.
    #[default]
    Ms30,
}

impl IlbcMode {
    /// Parse `CodecParameters::options.get("mode")` (or any equivalent
    /// string). Unknown / missing values fall back to 30 ms.
    pub fn parse(opt: Option<&str>) -> Self {
        match opt {
            Some("20") | Some("20ms") | Some("ms20") => Self::Ms20,
            Some("30") | Some("30ms") | Some("ms30") => Self::Ms30,
            _ => Self::default(),
        }
    }

    /// PCM frames per packet (= the block length in samples at 8 kHz).
    pub fn frames_per_packet(self) -> u32 {
        match self {
            Self::Ms20 => 160,
            Self::Ms30 => 240,
        }
    }

    /// Compressed bytes per packet.
    pub fn bytes_per_packet(self) -> u32 {
        match self {
            Self::Ms20 => 38,
            Self::Ms30 => 50,
        }
    }

    /// Tag string the encoder echoes back through `output_params.options`
    /// so a downstream decoder can pick the matching geometry.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Ms20 => "20",
            Self::Ms30 => "30",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_strings() {
        assert_eq!(IlbcMode::parse(Some("20")), IlbcMode::Ms20);
        assert_eq!(IlbcMode::parse(Some("20ms")), IlbcMode::Ms20);
        assert_eq!(IlbcMode::parse(Some("ms20")), IlbcMode::Ms20);
        assert_eq!(IlbcMode::parse(Some("30")), IlbcMode::Ms30);
        assert_eq!(IlbcMode::parse(Some("30ms")), IlbcMode::Ms30);
        assert_eq!(IlbcMode::parse(None), IlbcMode::Ms30); // default
        assert_eq!(IlbcMode::parse(Some("xyz")), IlbcMode::Ms30);
    }

    #[test]
    fn ms20_geometry() {
        assert_eq!(IlbcMode::Ms20.frames_per_packet(), 160);
        assert_eq!(IlbcMode::Ms20.bytes_per_packet(), 38);
    }

    #[test]
    fn ms30_geometry() {
        assert_eq!(IlbcMode::Ms30.frames_per_packet(), 240);
        assert_eq!(IlbcMode::Ms30.bytes_per_packet(), 50);
    }

    #[test]
    fn tag_roundtrips() {
        assert_eq!(IlbcMode::parse(Some(IlbcMode::Ms20.tag())), IlbcMode::Ms20);
        assert_eq!(IlbcMode::parse(Some(IlbcMode::Ms30.tag())), IlbcMode::Ms30);
    }
}
