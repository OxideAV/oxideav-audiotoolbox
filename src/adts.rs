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
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
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
        if self.protection_absent {
            7
        } else {
            9
        }
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
    let frame_length =
        ((data[3] as usize & 0x03) << 11) | ((data[4] as usize) << 3) | ((data[5] as usize) >> 5);
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
pub fn build_header(payload_len: usize, sf_index: u8, channel_config: u8, profile: u8) -> [u8; 7] {
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

/// Extract the bare AudioSpecificConfig from an AudioToolbox AAC
/// "magic cookie".
///
/// AT vends the AAC compression cookie as an MPEG-4 `ES_Descriptor`
/// (ISO/IEC 14496-1 §7.2.6.5: tag `0x03` → `DecoderConfigDescriptor`
/// tag `0x04` → `DecoderSpecificInfo` tag `0x05`, whose payload is the
/// AudioSpecificConfig of ISO/IEC 14496-3 §1.6.2.1), sometimes still
/// wrapped in an `esds` atom (size + `esds` + version/flags). Consumers
/// of `CodecParameters::extradata` (the MP4 / Matroska muxers, the AAC
/// decoder) expect the bare AudioSpecificConfig, so unwrap it here.
/// Input that is not descriptor-wrapped is returned unchanged.
pub fn asc_from_magic_cookie(cookie: &[u8]) -> Vec<u8> {
    let body = if cookie.len() >= 12 && &cookie[4..8] == b"esds" {
        &cookie[12..]
    } else {
        cookie
    };
    find_decoder_specific_info(body, 0)
        .map(<[u8]>::to_vec)
        .unwrap_or_else(|| cookie.to_vec())
}

/// Wrap a bare AudioSpecificConfig into the MPEG-4 `ES_Descriptor`
/// form AudioToolbox expects as an AAC *decompression* magic cookie
/// (the inverse of [`asc_from_magic_cookie`]). Input that already is an
/// `ES_Descriptor` (first byte `0x03`, which no valid ASC can start
/// with: it would encode audioObjectType 0) is returned unchanged.
pub fn magic_cookie_from_asc(asc: &[u8]) -> Vec<u8> {
    if asc.is_empty() || asc[0] == 0x03 || asc.len() > 0x7f - 20 {
        return asc.to_vec();
    }
    // DecoderSpecificInfo (§7.2.6.7).
    let mut dsi = vec![0x05, asc.len() as u8];
    dsi.extend_from_slice(asc);
    // DecoderConfigDescriptor (§7.2.6.6): objectTypeIndication 0x40
    // (ISO/IEC 14496-3 audio), streamType 0x05 (audio) << 2 | 1
    // (reserved bit), bufferSizeDB / maxBitrate / avgBitrate unknown.
    let mut dcd = vec![0x04, (13 + dsi.len()) as u8, 0x40, 0x15];
    dcd.extend_from_slice(&[0; 11]);
    dcd.extend_from_slice(&dsi);
    // ES_Descriptor (§7.2.6.5): ES_ID 0, no optional fields, then the
    // DecoderConfigDescriptor and a predefined SLConfigDescriptor.
    let sl = [0x06, 0x01, 0x02];
    let mut es = vec![0x03, (3 + dcd.len() + sl.len()) as u8, 0, 0, 0];
    es.extend_from_slice(&dcd);
    es.extend_from_slice(&sl);
    es
}

/// Read an expandable descriptor size (§8.3.3): up to four bytes, seven
/// payload bits each, high bit = continuation. Returns (size, bytes used).
fn descriptor_size(data: &[u8]) -> Option<(usize, usize)> {
    let mut size = 0usize;
    for (i, &b) in data.iter().take(4).enumerate() {
        size = (size << 7) | usize::from(b & 0x7f);
        if b & 0x80 == 0 {
            return Some((size, i + 1));
        }
    }
    None
}

/// Walk the descriptor at the start of `data` (and its children) down to
/// the first `DecoderSpecificInfo` payload.
fn find_decoder_specific_info(data: &[u8], depth: u8) -> Option<&[u8]> {
    if depth > 4 {
        return None;
    }
    let (&tag, rest) = data.split_first()?;
    let (len, used) = descriptor_size(rest)?;
    let payload = rest.get(used..used.checked_add(len)?)?;
    match tag {
        0x05 => Some(payload),
        0x04 => {
            // objectTypeIndication, streamType byte, bufferSizeDB (24 bits),
            // maxBitrate, avgBitrate: 13 bytes before the children.
            find_decoder_specific_info(payload.get(13..)?, depth + 1)
        }
        0x03 => {
            // ES_ID (16 bits) + flags byte, then the optional fields the
            // flags announce.
            let flags = *payload.get(2)?;
            let mut at = 3usize;
            if flags & 0x80 != 0 {
                at += 2; // dependsOn_ES_ID
            }
            if flags & 0x40 != 0 {
                at += 1 + usize::from(*payload.get(at)?); // URL
            }
            if flags & 0x20 != 0 {
                at += 2; // OCR_ES_Id
            }
            let mut children = payload.get(at..)?;
            while !children.is_empty() {
                if let Some(dsi) = find_decoder_specific_info(children, depth + 1) {
                    return Some(dsi);
                }
                let (len, used) = descriptor_size(children.get(1..)?)?;
                children = children.get(1 + used + len..)?;
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ES_Descriptor around a 2-byte AAC-LC 44.1 kHz stereo ASC, as
    /// AudioToolbox vends it.
    fn es_descriptor(asc: &[u8], wrap_in_esds_atom: bool) -> Vec<u8> {
        let mut dsi = vec![0x05, asc.len() as u8];
        dsi.extend_from_slice(asc);
        let mut dcd = vec![0x04, (13 + dsi.len()) as u8, 0x40, 0x15, 0, 0, 0];
        dcd.extend_from_slice(&[0, 1, 0xf4, 0, 0, 1, 0xf4, 0]);
        dcd.extend_from_slice(&dsi);
        let mut es = vec![0x03, (3 + dcd.len() + 3) as u8, 0, 0, 0];
        es.extend_from_slice(&dcd);
        es.extend_from_slice(&[0x06, 1, 2]); // SLConfigDescriptor
        if wrap_in_esds_atom {
            let mut atom = ((12 + es.len()) as u32).to_be_bytes().to_vec();
            atom.extend_from_slice(b"esds");
            atom.extend_from_slice(&[0, 0, 0, 0]);
            atom.extend_from_slice(&es);
            atom
        } else {
            es
        }
    }

    #[test]
    fn magic_cookie_es_descriptor_unwraps_to_asc() {
        let asc = [0x12, 0x10];
        assert_eq!(asc_from_magic_cookie(&es_descriptor(&asc, false)), asc);
        assert_eq!(asc_from_magic_cookie(&es_descriptor(&asc, true)), asc);
    }

    #[test]
    fn magic_cookie_with_long_form_sizes_and_he_asc() {
        // HE-AAC v2 explicit-signalling ASC, sizes in 4-byte form.
        let asc = [0x2b, 0x92, 0x08, 0x00, 0x56, 0xe5, 0x00];
        let mut dsi = vec![0x05, 0x80, 0x80, 0x80, asc.len() as u8];
        dsi.extend_from_slice(&asc);
        let mut dcd = vec![0x04, 0x80, 0x80, 0x80, (13 + dsi.len()) as u8];
        dcd.extend_from_slice(&[0x40, 0x15, 0, 0, 0, 0, 1, 0xf4, 0, 0, 1, 0xf4, 0]);
        dcd.extend_from_slice(&dsi);
        let mut es = vec![0x03, 0x80, 0x80, 0x80, (3 + dcd.len()) as u8, 0, 1, 0];
        es.extend_from_slice(&dcd);
        assert_eq!(asc_from_magic_cookie(&es), asc);
    }

    #[test]
    fn asc_wraps_into_an_es_descriptor_and_back() {
        for asc in [
            &[0x12u8, 0x10][..],
            &[0x2b, 0x92, 0x08, 0x00, 0x56, 0xe5, 0x00],
        ] {
            let cookie = magic_cookie_from_asc(asc);
            assert_eq!(cookie[0], 0x03);
            assert_eq!(asc_from_magic_cookie(&cookie), asc);
            // Already-wrapped input is left alone.
            assert_eq!(magic_cookie_from_asc(&cookie), cookie);
        }
        assert!(magic_cookie_from_asc(&[]).is_empty());
    }

    #[test]
    fn bare_asc_and_garbage_pass_through() {
        assert_eq!(asc_from_magic_cookie(&[0x12, 0x10]), [0x12, 0x10]);
        let truncated = [0x03, 0x20, 0x00];
        assert_eq!(asc_from_magic_cookie(&truncated), truncated);
        assert!(asc_from_magic_cookie(&[]).is_empty());
    }

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
