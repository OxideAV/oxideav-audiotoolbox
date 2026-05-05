//! ADTS (Audio Data Transport Stream) framing helpers for the AudioToolbox
//! bridge.
//!
//! AudioConverter produces **raw** AAC frames with no container header.  The
//! encoder side synthesises a 7-byte ADTS prefix from the configured ASBD
//! so the output can be consumed by any standard AAC decoder.  The decoder
//! side strips the ADTS prefix before feeding the raw payload to
//! AudioConverter.
//!
//! Header layout (no CRC, 7 bytes, ISO/IEC 13818-7 §6.2 / 14496-3 §1.A.2):
//!
//! ```text
//! syncword              12 b  (0xFFF)
//! id                     1 b  (0 = MPEG-4)
//! layer                  2 b  (always 0)
//! protection_absent      1 b  (1 = no CRC)
//! profile                2 b  (AAC-LC = 1, i.e. object_type-1)
//! sampling_freq_index    4 b
//! private_bit            1 b  (0)
//! channel_configuration  3 b
//! original_copy          1 b  (0)
//! home                   1 b  (0)
//! copyright_id_bit       1 b  (0)
//! copyright_id_start     1 b  (0)
//! aac_frame_length      13 b  (includes the 7-byte header)
//! adts_buffer_fullness  11 b  (0x7FF = VBR)
//! number_of_raw_blocks   2 b  (0 = single raw_data_block)
//! ```

/// Sample-rate index table (ISO/IEC 14496-3 §1.6.2).
pub const SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
    8_000, 7_350,
];

/// Return the ADTS sampling-frequency-index for a given sample rate, or
/// `None` if the rate is not in the table.
pub fn sample_rate_index(sample_rate: u32) -> Option<u8> {
    SAMPLE_RATES
        .iter()
        .position(|&r| r == sample_rate)
        .map(|i| i as u8)
}

/// Parsed subset of an ADTS header — enough to drive the AudioConverter.
#[derive(Clone, Debug)]
pub struct AdtsHeader {
    /// Total frame size including the 7-byte header.
    pub frame_length: usize,
    /// Whether a 2-byte CRC follows the fixed header (protection_absent=0 means CRC present).
    pub protection_absent: bool,
    /// AAC sampling-frequency-index (0..=12).
    pub sampling_freq_index: u8,
    /// Channel configuration (1..=7 for standard layouts).
    pub channel_configuration: u8,
}

impl AdtsHeader {
    /// Byte length of just the header portion.
    pub fn header_len(&self) -> usize {
        if self.protection_absent { 7 } else { 9 }
    }
}

/// Parse the first ADTS header from `data`.  Returns `None` when fewer than
/// 7 bytes are available or the syncword is missing.
pub fn parse(data: &[u8]) -> Option<AdtsHeader> {
    if data.len() < 7 {
        return None;
    }
    // Syncword must be 0xFFF (12 bits).
    if data[0] != 0xFF || (data[1] & 0xF0) != 0xF0 {
        return None;
    }
    let protection_absent = (data[1] & 0x01) != 0;
    let sampling_freq_index = (data[2] >> 2) & 0x0F;
    let channel_configuration = ((data[2] & 0x01) << 2) | (data[3] >> 6);
    let frame_length = ((data[3] as usize & 0x03) << 11)
        | ((data[4] as usize) << 3)
        | ((data[5] as usize) >> 5);
    Some(AdtsHeader {
        frame_length,
        protection_absent,
        sampling_freq_index,
        channel_configuration,
    })
}

/// Build a 7-byte ADTS header (no CRC) for one raw AAC frame of `payload_len`
/// bytes.
///
/// `profile` is the AAC object type minus 1 (1 = AAC-LC).
pub fn build_header(
    payload_len: usize,
    sf_index: u8,
    channel_config: u8,
    profile: u8,
) -> [u8; 7] {
    let total = payload_len + 7;
    // protection_absent = 1 (no CRC), MPEG-4, layer = 0
    // Byte 0-1: syncword + id(0) + layer(00) + protection_absent(1)
    let b0 = 0xFF_u8;
    let b1 = 0xF1_u8; // 1111 0001

    // Byte 2: profile(2b) + sf_index(4b) + private(1b) + chan_cfg hi(1b)
    let b2 = ((profile & 0x03) << 6) | ((sf_index & 0x0F) << 2) | ((channel_config >> 2) & 0x01);

    // Byte 3: chan_cfg lo(2b) + orig(0) + home(0) + cprt_id(0) + cprt_start(0) + frame_len hi(2b)
    let b3 = ((channel_config & 0x03) << 6) | (((total >> 11) & 0x03) as u8);

    // Byte 4: frame_len bits 10-3
    let b4 = ((total >> 3) & 0xFF) as u8;

    // Byte 5: frame_len bits 2-0 + buffer_fullness hi 5 bits (0x7FF = VBR → 11111)
    let b5 = (((total & 0x07) << 5) | 0x1F) as u8;

    // Byte 6: buffer_fullness lo 6 bits (111111) + number_of_raw_blocks(2b) = 0
    let b6 = 0xFC_u8;

    [b0, b1, b2, b3, b4, b5, b6]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_header() {
        // Encode then parse a 512-byte AAC-LC 48 kHz stereo frame.
        let sf = sample_rate_index(48_000).unwrap();
        let hdr = build_header(512, sf, 2, 1 /* AAC-LC profile */);
        let parsed = parse(&hdr).unwrap();
        assert_eq!(parsed.frame_length, 512 + 7);
        assert_eq!(parsed.sampling_freq_index, sf);
        assert_eq!(parsed.channel_configuration, 2);
    }

    #[test]
    fn sample_rate_index_known_rates() {
        assert_eq!(sample_rate_index(48_000), Some(3));
        assert_eq!(sample_rate_index(44_100), Some(4));
        assert_eq!(sample_rate_index(8_000), Some(11));
        assert_eq!(sample_rate_index(99_999), None);
    }
}
