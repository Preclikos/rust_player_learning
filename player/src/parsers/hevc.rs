//! HEVC bitstream parsing: SPS colour information (bit depth, VUI colour
//! description) and SEI metadata needed for HDR playback.
//!
//! The DASH manifest is not a reliable source of colorimetry — real-world
//! streams signal BT.709 in the MPD while the bitstream carries PQ/BT.2020
//! (our own test stream does exactly this). The SPS VUI is authoritative:
//! `transfer_characteristics` 16 = PQ (HDR10/DV 8.1), 18 = HLG.
//!
//! Everything here is defensive: any malformed input yields `None` and the
//! caller falls back to manifest-level sniffing.

/// RBSP bit reader (MSB first) with Exp-Golomb support.
/// Construct via [`BitReader::new`] AFTER emulation-prevention removal.
pub struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    /// Read `n` bits (n ≤ 32). None on overrun.
    pub fn u(&mut self, n: u32) -> Option<u32> {
        let mut v: u32 = 0;
        for _ in 0..n {
            let byte = *self.data.get(self.bit_pos / 8)?;
            let bit = (byte >> (7 - (self.bit_pos % 8))) & 1;
            v = (v << 1) | bit as u32;
            self.bit_pos += 1;
        }
        Some(v)
    }

    pub fn flag(&mut self) -> Option<bool> {
        self.u(1).map(|b| b == 1)
    }

    /// Unsigned Exp-Golomb (ue(v)).
    pub fn ue(&mut self) -> Option<u32> {
        let mut leading_zeros = 0u32;
        while self.u(1)? == 0 {
            leading_zeros += 1;
            // Defensive cap — a valid ue(v) in SPS never needs more.
            if leading_zeros > 31 {
                return None;
            }
        }
        let suffix = if leading_zeros == 0 { 0 } else { self.u(leading_zeros)? };
        Some((1u32 << leading_zeros) - 1 + suffix)
    }

    /// Signed Exp-Golomb (se(v)).
    pub fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i64;
        // (-1)^(k+1) * ceil(k/2)
        let v = if k % 2 == 0 { -(k / 2) } else { (k + 1) / 2 };
        Some(v as i32)
    }
}

/// Strip emulation-prevention bytes (00 00 03 -> 00 00) from a NAL payload.
pub fn unescape_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0u32;
    for &b in data {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue; // drop the emulation-prevention byte
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(b);
    }
    out
}

/// Colour information extracted from the HEVC SPS (+ its VUI).
/// Field values use the H.273 / HEVC code points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpsColorInfo {
    pub bit_depth_luma: u8,
    pub full_range: bool,
    /// H.273 colour_primaries (1 = BT.709, 9 = BT.2020). 2 = unspecified.
    pub colour_primaries: u8,
    /// H.273 transfer_characteristics (1 = BT.709, 16 = PQ, 18 = HLG).
    pub transfer_characteristics: u8,
    /// H.273 matrix_coeffs (1 = BT.709, 9 = BT.2020 NCL).
    pub matrix_coeffs: u8,
}

impl Default for SpsColorInfo {
    fn default() -> Self {
        Self {
            bit_depth_luma: 8,
            full_range: false,
            colour_primaries: 2,         // unspecified
            transfer_characteristics: 2, // unspecified
            matrix_coeffs: 2,            // unspecified
        }
    }
}

/// HEVC nal_unit_type from the first byte of a NAL unit.
pub fn nal_unit_type(nalu: &[u8]) -> Option<u8> {
    nalu.first().map(|b| (b >> 1) & 0x3F)
}

pub const NAL_SPS: u8 = 33;
pub const NAL_SEI_PREFIX: u8 = 39;
/// Unspecified NAL types carrying Dolby Vision data in single-track
/// streams: 62 = RPU (reshaping/DM metadata), 63 = enhancement layer.
pub const NAL_DV_RPU: u8 = 62;
pub const NAL_DV_EL: u8 = 63;

/// Parse colour info from the first SPS found in `nalus` (raw NALU bytes,
/// no start code / length prefix — the shape `parse_hvcc_nalus` returns).
pub fn parse_sps_color_info(nalus: &[Vec<u8>]) -> Option<SpsColorInfo> {
    nalus
        .iter()
        .find(|n| nal_unit_type(n) == Some(NAL_SPS))
        .and_then(|n| parse_sps(n))
}

/// Parse one SPS NAL unit (including the 2-byte NAL header).
fn parse_sps(nalu: &[u8]) -> Option<SpsColorInfo> {
    // Skip the 2-byte NAL unit header, unescape the RBSP.
    let rbsp = unescape_rbsp(nalu.get(2..)?);
    let mut r = BitReader::new(&rbsp);

    r.u(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.u(3)?;
    r.u(1)?; // sps_temporal_id_nesting_flag
    skip_profile_tier_level(&mut r, max_sub_layers_minus1)?;

    r.ue()?; // sps_seq_parameter_set_id
    let chroma_format_idc = r.ue()?;
    if chroma_format_idc == 3 {
        r.u(1)?; // separate_colour_plane_flag
    }
    r.ue()?; // pic_width_in_luma_samples
    r.ue()?; // pic_height_in_luma_samples
    if r.flag()? {
        // conformance_window: left/right/top/bottom offsets
        r.ue()?;
        r.ue()?;
        r.ue()?;
        r.ue()?;
    }
    let bit_depth_luma = 8 + r.ue()? as u8;
    r.ue()?; // bit_depth_chroma_minus8
    let log2_max_poc_lsb_minus4 = r.ue()?;
    if log2_max_poc_lsb_minus4 > 12 {
        return None; // spec range 0..12 — bail on garbage
    }

    let sub_layer_ordering_info_present = r.flag()?;
    let ord_start = if sub_layer_ordering_info_present { 0 } else { max_sub_layers_minus1 };
    for _ in ord_start..=max_sub_layers_minus1 {
        r.ue()?; // sps_max_dec_pic_buffering_minus1
        r.ue()?; // sps_max_num_reorder_pics
        r.ue()?; // sps_max_latency_increase_plus1
    }

    r.ue()?; // log2_min_luma_coding_block_size_minus3
    r.ue()?; // log2_diff_max_min_luma_coding_block_size
    r.ue()?; // log2_min_luma_transform_block_size_minus2
    r.ue()?; // log2_diff_max_min_luma_transform_block_size
    r.ue()?; // max_transform_hierarchy_depth_inter
    r.ue()?; // max_transform_hierarchy_depth_intra

    if r.flag()? {
        // scaling_list_enabled_flag
        if r.flag()? {
            // sps_scaling_list_data_present_flag
            skip_scaling_list_data(&mut r)?;
        }
    }

    r.u(1)?; // amp_enabled_flag
    r.u(1)?; // sample_adaptive_offset_enabled_flag
    if r.flag()? {
        // pcm_enabled_flag
        r.u(4)?; // pcm_sample_bit_depth_luma_minus1
        r.u(4)?; // pcm_sample_bit_depth_chroma_minus1
        r.ue()?; // log2_min_pcm_luma_coding_block_size_minus3
        r.ue()?; // log2_diff_max_min_pcm_luma_coding_block_size
        r.u(1)?; // pcm_loop_filter_disabled_flag
    }

    let num_short_term_ref_pic_sets = r.ue()?;
    if num_short_term_ref_pic_sets > 64 {
        return None; // spec max
    }
    // NumDeltaPocs per parsed set — needed by inter-predicted sets.
    let mut num_delta_pocs: Vec<u32> = Vec::with_capacity(num_short_term_ref_pic_sets as usize);
    for idx in 0..num_short_term_ref_pic_sets {
        let n = skip_st_ref_pic_set(&mut r, idx, &num_delta_pocs)?;
        num_delta_pocs.push(n);
    }

    if r.flag()? {
        // long_term_ref_pics_present_flag
        let num_long_term = r.ue()?;
        if num_long_term > 32 {
            return None;
        }
        for _ in 0..num_long_term {
            r.u(log2_max_poc_lsb_minus4 + 4)?; // lt_ref_pic_poc_lsb_sps
            r.u(1)?; // used_by_curr_pic_lt_sps_flag
        }
    }

    r.u(1)?; // sps_temporal_mvp_enabled_flag
    r.u(1)?; // strong_intra_smoothing_enabled_flag

    let mut info = SpsColorInfo {
        bit_depth_luma,
        ..Default::default()
    };

    if r.flag()? {
        // vui_parameters_present_flag
        if r.flag()? {
            // aspect_ratio_info_present_flag
            let idc = r.u(8)?;
            if idc == 255 {
                r.u(16)?; // sar_width
                r.u(16)?; // sar_height
            }
        }
        if r.flag()? {
            // overscan_info_present_flag
            r.u(1)?;
        }
        if r.flag()? {
            // video_signal_type_present_flag
            r.u(3)?; // video_format
            info.full_range = r.flag()?;
            if r.flag()? {
                // colour_description_present_flag
                info.colour_primaries = r.u(8)? as u8;
                info.transfer_characteristics = r.u(8)? as u8;
                info.matrix_coeffs = r.u(8)? as u8;
            }
        }
        // Remaining VUI fields are irrelevant for colour.
    }

    Some(info)
}

/// profile_tier_level(1, maxNumSubLayersMinus1) — fixed-width, skip in full.
fn skip_profile_tier_level(r: &mut BitReader, max_sub_layers_minus1: u32) -> Option<()> {
    // general_*: profile_space(2) tier(1) idc(5) compat(32) constraint+reserved(43) inbld(1)
    r.u(8)?;
    r.u(32)?;
    r.u(32)?;
    r.u(12)?;
    r.u(8)?; // general_level_idc

    let mut profile_present = [false; 8];
    let mut level_present = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        profile_present[i] = r.flag()?;
        level_present[i] = r.flag()?;
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            r.u(2)?; // reserved_zero_2bits
        }
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if profile_present[i] {
            r.u(32)?;
            r.u(32)?;
            r.u(24)?; // 88 bits of sub-layer profile info
        }
        if level_present[i] {
            r.u(8)?;
        }
    }
    Some(())
}

/// scaling_list_data() — skip.
fn skip_scaling_list_data(r: &mut BitReader) -> Option<()> {
    for size_id in 0..4u32 {
        let matrix_count = if size_id == 3 { 2 } else { 6 };
        for _ in 0..matrix_count {
            if !r.flag()? {
                // scaling_list_pred_mode_flag == 0
                r.ue()?; // scaling_list_pred_matrix_id_delta
            } else {
                let coef_num = std::cmp::min(64, 1 << (4 + (size_id << 1)));
                if size_id > 1 {
                    r.se()?; // scaling_list_dc_coef_minus8
                }
                for _ in 0..coef_num {
                    r.se()?; // scaling_list_delta_coef
                }
            }
        }
    }
    Some(())
}

/// st_ref_pic_set(stRpsIdx) — skip; returns NumDeltaPocs[stRpsIdx].
fn skip_st_ref_pic_set(r: &mut BitReader, idx: u32, num_delta_pocs: &[u32]) -> Option<u32> {
    let inter_pred = if idx != 0 { r.flag()? } else { false };
    if inter_pred {
        // Inside the SPS loop idx is never == num_short_term_ref_pic_sets,
        // so delta_idx_minus1 is absent and RefRpsIdx = idx - 1.
        let ref_num = *num_delta_pocs.get(idx as usize - 1)?;
        r.u(1)?; // delta_rps_sign
        r.ue()?; // abs_delta_rps_minus1
        let mut count = 0u32;
        for _ in 0..=ref_num {
            let used_by_curr = r.flag()?;
            let use_delta = if used_by_curr { true } else { r.flag()? };
            if use_delta {
                count += 1;
            }
        }
        Some(count)
    } else {
        let num_negative = r.ue()?;
        let num_positive = r.ue()?;
        if num_negative + num_positive > 32 {
            return None;
        }
        for _ in 0..num_negative + num_positive {
            r.ue()?; // delta_poc_sX_minus1
            r.u(1)?; // used_by_curr_pic_sX_flag
        }
        Some(num_negative + num_positive)
    }
}

// ---------------------------------------------------------------------------
// SEI parsing (HDR metadata)
// ---------------------------------------------------------------------------

/// Dynamic (per-access-unit) HDR10+ metadata from the ST 2094-40 SEI.
/// Converted to nits; only the fields the mobius tonemap consumes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hdr10PlusInfo {
    /// max(maxscl[0..3]) of window 0 — the scene peak.
    pub max_scl_nits: f32,
    /// average_maxrgb of window 0 — the scene average of max(R,G,B).
    pub avg_maxrgb_nits: f32,
}

/// Static HDR10 metadata from the mastering-display / content-light-level
/// SEI messages (typically present on every IDR).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HdrStaticInfo {
    /// max_display_mastering_luminance (SEI 137), nits.
    pub mastering_peak_nits: Option<f32>,
    /// max_content_light_level (SEI 144), nits. 0 = unknown per spec.
    pub max_cll_nits: Option<f32>,
}

/// All HDR-relevant payloads found in one SEI prefix NAL unit.
#[derive(Clone, Copy, Debug, Default)]
pub struct SeiHdrMetadata {
    pub hdr10plus: Option<Hdr10PlusInfo>,
    pub static_info: HdrStaticInfo,
}

const SEI_MASTERING_DISPLAY: u32 = 137;
const SEI_CONTENT_LIGHT_LEVEL: u32 = 144;
const SEI_USER_DATA_REGISTERED_T35: u32 = 4;

/// Parse the HDR-relevant SEI payloads out of one SEI prefix NAL unit
/// (raw NALU bytes including the 2-byte header). Unknown payloads are
/// skipped; malformed ones abort the scan and return what was found.
pub fn parse_sei_hdr_metadata(nalu: &[u8]) -> SeiHdrMetadata {
    let mut out = SeiHdrMetadata::default();
    let Some(payload) = nalu.get(2..) else {
        return out;
    };
    let rbsp = unescape_rbsp(payload);
    let mut d = &rbsp[..];

    // sei_message loop: payload_type and payload_size are both coded as
    // sequences of 0xFF (add 255) terminated by the final byte.
    loop {
        let mut payload_type: u32 = 0;
        loop {
            match d.first() {
                Some(&b) => {
                    d = &d[1..];
                    payload_type += b as u32;
                    if b != 0xFF {
                        break;
                    }
                }
                None => return out,
            }
        }
        let mut payload_size: usize = 0;
        loop {
            match d.first() {
                Some(&b) => {
                    d = &d[1..];
                    payload_size += b as usize;
                    if b != 0xFF {
                        break;
                    }
                }
                None => return out,
            }
        }
        let Some(body) = d.get(..payload_size) else {
            return out;
        };
        match payload_type {
            SEI_MASTERING_DISPLAY => {
                // 3×(primary x,y u16) + white point (x,y u16) = 20 bytes,
                // then max_display_mastering_luminance u32 (0.0001 nits),
                // min u32.
                if body.len() >= 24 {
                    let max = u32::from_be_bytes([body[20], body[21], body[22], body[23]]);
                    out.static_info.mastering_peak_nits = Some(max as f32 * 0.0001);
                }
            }
            SEI_CONTENT_LIGHT_LEVEL => {
                if body.len() >= 2 {
                    let max_cll = u16::from_be_bytes([body[0], body[1]]);
                    if max_cll > 0 {
                        out.static_info.max_cll_nits = Some(max_cll as f32);
                    }
                }
            }
            SEI_USER_DATA_REGISTERED_T35 => {
                if let Some(info) = parse_t35_hdr10plus(body) {
                    out.hdr10plus = Some(info);
                }
            }
            _ => {}
        }
        d = &d[payload_size..];
        // rbsp_trailing_bits: 0x80 (stop bit + alignment), possibly
        // followed by zero padding, terminates the message list. A real
        // payload type byte can also be ≥0x80 (137/144 are), so only
        // treat it as trailing when nothing but zeros follows.
        if d.is_empty() || (d[0] == 0x80 && d[1..].iter().all(|&b| b == 0)) {
            return out;
        }
    }
}

/// ST 2094-40 (HDR10+) dynamic metadata inside the ITU-T T.35 SEI.
/// Returns window-0 maxscl/average in nits.
fn parse_t35_hdr10plus(body: &[u8]) -> Option<Hdr10PlusInfo> {
    // itu_t_t35_country_code 0xB5 (USA), terminal_provider_code 0x003C
    // (Samsung), terminal_provider_oriented_code 0x0001,
    // application_identifier 4 (ST 2094-40), application_version.
    if body.len() < 7
        || body[0] != 0xB5
        || u16::from_be_bytes([body[1], body[2]]) != 0x003C
        || u16::from_be_bytes([body[3], body[4]]) != 0x0001
        || body[5] != 4
    {
        return None;
    }
    let mut r = BitReader::new(&body[7..]);

    let num_windows = r.u(2)?;
    if num_windows == 0 || num_windows > 3 {
        return None;
    }
    // Window 1.. (beyond the mandatory window 0) geometry: 11 fixed fields.
    for _ in 1..num_windows {
        r.u(16)?; // window_upper_left_corner_x
        r.u(16)?; // window_upper_left_corner_y
        r.u(16)?; // window_lower_right_corner_x
        r.u(16)?; // window_lower_right_corner_y
        r.u(16)?; // center_of_ellipse_x
        r.u(16)?; // center_of_ellipse_y
        r.u(8)?;  // rotation_angle
        r.u(16)?; // semimajor_axis_internal_ellipse
        r.u(16)?; // semimajor_axis_external_ellipse
        r.u(16)?; // semiminor_axis_external_ellipse
        r.u(1)?;  // overlap_process_option
    }
    r.u(27)?; // targeted_system_display_maximum_luminance (0.0001 nits)
    if r.flag()? {
        // targeted_system_display_actual_peak_luminance: 5+5 bit dims,
        // 4-bit entries.
        let rows = r.u(5)?;
        let cols = r.u(5)?;
        for _ in 0..rows * cols {
            r.u(4)?;
        }
    }

    // Window 0 is the first per-window block — that's all the tonemap
    // needs; remaining windows aren't parsed.
    // maxscl / average_maxrgb are 17-bit, units of 0.1 nits (range
    // 0..100000 = 10 000 nits).
    let m0 = r.u(17)? as f32;
    let m1 = r.u(17)? as f32;
    let m2 = r.u(17)? as f32;
    let avg = r.u(17)? as f32;

    let max_scl_nits = m0.max(m1).max(m2) * 0.1;
    let avg_maxrgb_nits = avg * 0.1;
    if max_scl_nits <= 0.0 {
        return None;
    }
    Some(Hdr10PlusInfo {
        max_scl_nits,
        avg_maxrgb_nits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb() {
        // bits: 1 -> 0 | 010 -> 1 | 011 -> 2 | 00100 -> 3
        let data = [0b1_010_011_0, 0b0100_0000];
        let mut r = BitReader::new(&data);
        assert_eq!(r.ue(), Some(0));
        assert_eq!(r.ue(), Some(1));
        assert_eq!(r.ue(), Some(2));
        assert_eq!(r.ue(), Some(3));
    }

    fn bits_to_bytes(bits: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        for chunk in bits.as_bytes().chunks(8) {
            let mut b = 0u8;
            for (i, &c) in chunk.iter().enumerate() {
                if c == b'1' {
                    b |= 1 << (7 - i);
                }
            }
            bytes.push(b);
        }
        bytes
    }

    #[test]
    fn sei_hdr_metadata() {
        // T35 HDR10+ payload: 7-byte header + bit fields.
        let mut t35 = vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04, 0x00];
        let mut bits = String::new();
        bits += "01"; // num_windows = 1
        bits += &format!("{:027b}", 4_000_000); // targeted max luminance (400 nits)
        bits += "0"; // actual_peak_luminance_flag
        bits += &format!("{:017b}", 10_000); // maxscl[0] = 1000 nits
        bits += &format!("{:017b}", 8_000);  // maxscl[1]
        bits += &format!("{:017b}", 6_000);  // maxscl[2]
        bits += &format!("{:017b}", 2_000);  // average_maxrgb = 200 nits
        t35.extend_from_slice(&bits_to_bytes(&bits));

        // SEI NAL: header, CLL message (type 144), T35 message (type 4),
        // trailing 0x80.
        let mut nalu = vec![NAL_SEI_PREFIX << 1, 0x01];
        nalu.extend_from_slice(&[144, 4, 0x03, 0xE8, 0x01, 0x90]); // MaxCLL=1000, MaxFALL=400
        nalu.push(4);
        nalu.push(t35.len() as u8);
        nalu.extend_from_slice(&t35);
        nalu.push(0x80);

        let meta = parse_sei_hdr_metadata(&nalu);
        assert_eq!(meta.static_info.max_cll_nits, Some(1000.0));
        let hp = meta.hdr10plus.expect("hdr10plus parsed");
        assert!((hp.max_scl_nits - 1000.0).abs() < 0.01, "{}", hp.max_scl_nits);
        assert!((hp.avg_maxrgb_nits - 200.0).abs() < 0.01, "{}", hp.avg_maxrgb_nits);
    }

    #[test]
    fn sei_mastering_display() {
        let mut body = vec![0u8; 24];
        // max_display_mastering_luminance = 10_000_000 * 0.0001 = 1000 nits
        body[20..24].copy_from_slice(&10_000_000u32.to_be_bytes());
        let mut nalu = vec![NAL_SEI_PREFIX << 1, 0x01];
        nalu.push(137);
        nalu.push(body.len() as u8);
        nalu.extend_from_slice(&body);
        nalu.push(0x80);
        let meta = parse_sei_hdr_metadata(&nalu);
        assert_eq!(meta.static_info.mastering_peak_nits, Some(1000.0));
        assert!(meta.hdr10plus.is_none());
    }

    #[test]
    fn rbsp_unescape() {
        assert_eq!(unescape_rbsp(&[0, 0, 3, 1]), vec![0, 0, 1]);
        assert_eq!(unescape_rbsp(&[0, 0, 3, 0, 0, 3, 2]), vec![0, 0, 0, 0, 2]);
        assert_eq!(unescape_rbsp(&[1, 2, 3]), vec![1, 2, 3]);
    }

    // Real SPS from an x265 PQ BT.2020 Main10 encode (extracted with
    // `ffprobe -show_data`-style dump): 1280x720, bit depth 10,
    // primaries 9, transfer 16 (PQ), matrix 9, limited range.
    // Synthesised here from x265 defaults — regenerate with:
    //   x265 --input-res 1280x720 --fps 24 --profile main10
    //        --colorprim bt2020 --transfer smpte2084 --colormatrix bt2020nc
    #[test]
    fn sps_pq_bt2020() {
        // Hand-built minimal SPS with the fields we read. Built with a bit
        // writer mirroring parse_sps's read order (no scaling list, no PCM,
        // 0 st_ref_pic_sets, VUI with colour description only).
        let mut bits = String::new();
        bits += "0000";                  // sps_video_parameter_set_id
        bits += "000";                   // sps_max_sub_layers_minus1 = 0
        bits += "1";                     // sps_temporal_id_nesting_flag
        bits += &"0".repeat(8 + 32 + 32 + 12); // PTL general (88 bits, content irrelevant)
        bits += "00000000";              // general_level_idc
        bits += "1";                     // sps_seq_parameter_set_id ue = 0
        bits += "010";                   // chroma_format_idc ue = 1 (4:2:0)
        bits += "1";                     // pic_width ue = 0 (value irrelevant)
        bits += "1";                     // pic_height ue = 0
        bits += "0";                     // conformance_window_flag
        bits += "011";                   // bit_depth_luma_minus8 ue = 2 -> 10-bit
        bits += "011";                   // bit_depth_chroma_minus8 ue = 2
        bits += "1";                     // log2_max_poc_lsb_minus4 ue = 0
        bits += "1";                     // sub_layer_ordering_info_present = 1
        bits += "1"; bits += "1"; bits += "1"; // dec_pic_buffering/reorder/latency ue = 0
        bits += "1"; bits += "1"; bits += "1"; bits += "1"; // cb/tb size ue = 0
        bits += "1"; bits += "1";        // transform hierarchy depths ue = 0
        bits += "0";                     // scaling_list_enabled_flag
        bits += "0";                     // amp_enabled_flag
        bits += "0";                     // sample_adaptive_offset_enabled_flag
        bits += "0";                     // pcm_enabled_flag
        bits += "1";                     // num_short_term_ref_pic_sets ue = 0
        bits += "0";                     // long_term_ref_pics_present_flag
        bits += "0";                     // sps_temporal_mvp_enabled_flag
        bits += "0";                     // strong_intra_smoothing_enabled_flag
        bits += "1";                     // vui_parameters_present_flag
        bits += "0";                     // aspect_ratio_info_present_flag
        bits += "0";                     // overscan_info_present_flag
        bits += "1";                     // video_signal_type_present_flag
        bits += "101";                   // video_format = 5 (unspecified)
        bits += "0";                     // video_full_range_flag = 0
        bits += "1";                     // colour_description_present_flag
        bits += "00001001";              // colour_primaries = 9
        bits += "00010000";              // transfer_characteristics = 16
        bits += "00001001";              // matrix_coeffs = 9

        let mut bytes = Vec::new();
        for chunk in bits.as_bytes().chunks(8) {
            let mut b = 0u8;
            for (i, &c) in chunk.iter().enumerate() {
                if c == b'1' {
                    b |= 1 << (7 - i);
                }
            }
            bytes.push(b);
        }
        // Prepend a 2-byte NAL header (type 33 = SPS).
        let mut nalu = vec![(NAL_SPS << 1), 0x01];
        nalu.extend_from_slice(&bytes);

        let info = parse_sps_color_info(&[nalu]).expect("parse");
        assert_eq!(info.bit_depth_luma, 10);
        assert!(!info.full_range);
        assert_eq!(info.colour_primaries, 9);
        assert_eq!(info.transfer_characteristics, 16);
        assert_eq!(info.matrix_coeffs, 9);
    }
}
