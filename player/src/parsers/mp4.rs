use std::error::Error;

/// Take the next `N` bytes off the front of `data`. The sidx comes from the
/// network: a truncated box must be a parse error, not an index panic.
fn take<const N: usize>(data: &mut &[u8]) -> Result<[u8; N], Box<dyn Error>> {
    if data.len() < N {
        return Err(format!("sidx truncated: need {} more bytes, have {}", N, data.len()).into());
    }
    let (head, rest) = data.split_at(N);
    *data = rest;
    Ok(head.try_into().expect("split_at(N) yields N bytes"))
}

fn read_u32(data: &mut &[u8]) -> Result<u32, Box<dyn Error>> {
    Ok(u32::from_be_bytes(take::<4>(data)?))
}

fn read_u16(data: &mut &[u8]) -> Result<u16, Box<dyn Error>> {
    Ok(u16::from_be_bytes(take::<2>(data)?))
}

fn read_u64(data: &mut &[u8]) -> Result<u64, Box<dyn Error>> {
    Ok(u64::from_be_bytes(take::<8>(data)?))
}

// SidxEntry / SidxBox carry every field defined by ISO/IEC 14496-12 §8.16.3
// for completeness/diagnostics — the segment-index pipeline only reads
// `reference_size`, `subsegment_duration`, `timescale`, `earliest_presentation_time`
// and `first_offset`. The remaining fields are kept so the structs are
// faithful representations of the box, which keeps the parser obvious and
// makes ad-hoc `Debug` dumps useful.
#[allow(dead_code)]
#[derive(Debug)]
pub struct SidxEntry {
    pub reference_type: u8,
    pub reference_size: u64,
    pub subsegment_duration: u32,
    pub starts_with_sap: u8,
    pub sap_type: u8,
    pub sap_delta: u32,
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct SidxBox {
    pub size: u32,
    pub version: u8,
    pub flags: u32,
    pub reference_id: u32,
    pub timescale: u32,
    // 64-bit in a version-1 sidx, 32-bit in version 0 (ISO/IEC 14496-12
    // §8.16.3). Stored as u64 either way so a version-1 box parses with the
    // correct field widths — reading these as u32 unconditionally would shift
    // every subsequent field by 8 bytes and mis-read `entry_count`.
    pub earliest_presentation_time: u64,
    pub first_offset: u64,
    pub entry_count: u16,
    pub entries: Vec<SidxEntry>,
}

pub fn parse_sidx(data: &mut &[u8]) -> Result<SidxBox, Box<dyn Error>> {
    // Read the size of the box (we ignore the size field here)
    let size = read_u32(data)?;

    // Read the box type (should be "sidx")
    let type_bytes = take::<4>(data)?;
    let type_str = String::from_utf8_lossy(&type_bytes);

    if type_str != "sidx" {
        return Err("Not a valid sidx box!".into());
    }

    // 64-bit box: `size == 1` means an 8-byte largesize follows the type.
    if size == 1 {
        let _largesize = read_u64(data)?;
    }

    // Read version and flags
    let version_flags = read_u32(data)?;
    let version = (version_flags >> 24) as u8;
    let flags = version_flags & 0x00FFFFFF;

    let reference_id = read_u32(data)?;
    let timescale = read_u32(data)?;
    // version 1 widens earliest_presentation_time + first_offset to 64-bit
    // (ISO/IEC 14496-12 §8.16.3). Reading them as u32 on a version-1 box was
    // the bug: every later field shifted by 8 bytes, so `entry_count` came out
    // garbage (often 0) → empty segment list → the player scheduled no media
    // segments and buffered forever, with no parse error.
    let (earliest_presentation_time, first_offset) = if version >= 1 {
        (read_u64(data)?, read_u64(data)?)
    } else {
        (read_u32(data)? as u64, read_u32(data)? as u64)
    };

    let _reserved = read_u16(data)?;

    let entry_count = read_u16(data)?; // Number of entries in the sidx
    let mut entries = Vec::new();

    // Parse the entries and generate segments
    for _ in 0..entry_count {
        let chunk = read_u32(data)?;
        let reference_type = (chunk >> 31) as u8;
        let reference_size = u64::from(chunk & 0x7FFFFFFF);
        let subsegment_duration = read_u32(data)?;
        let chunk = read_u32(data)?;
        let starts_with_sap = (chunk >> 31) as u8;
        let sap_type = ((chunk >> 28) & 0x7) as u8;
        let sap_delta = chunk & 0x0FFFFFFF;

        entries.push(SidxEntry {
            reference_type,
            reference_size,
            subsegment_duration,
            starts_with_sap,
            sap_type,
            sap_delta,
        });
    }

    // Diagnostic: a version-1 box that previously mis-parsed produced 0
    // entries (→ no media segments). Logging version + entry count makes that
    // failure mode obvious in a single line.
    log::info!(
        "[sidx] version={} timescale={} entries={} ept={} first_offset={}",
        version,
        timescale,
        entries.len(),
        earliest_presentation_time,
        first_offset
    );

    Ok(SidxBox {
        size,
        version,
        flags,
        reference_id,
        timescale,
        earliest_presentation_time,
        first_offset,
        entry_count,
        entries,
    })
}

/// Prefix a raw NALU body with the 4-byte Annex-B start code (`00 00 00 01`).
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
pub fn append_hevc_header(mut nalu_data: Vec<u8>) -> Vec<u8> {
    let mut nalu = vec![0x00, 0x00, 0x00, 0x01];
    nalu.append(&mut nalu_data);
    nalu
}

/// The NALU bodies of a length-prefixed (4-byte, big-endian) HEVC sample, as
/// slices into it (no copy, no start codes). Same validation as
/// [`parse_hevc_nalu`]: a length past the end is an error, 1-3 trailing bytes
/// are ignored.
// Used by the MediaCodec (Android) and FFmpeg HW (Windows, Linux) decoders.
#[cfg_attr(not(any(target_os = "android", target_os = "windows", target_os = "linux")), allow(dead_code))]
pub fn hevc_nalu_bodies(data: &[u8]) -> Result<Vec<&[u8]>, Box<dyn Error>> {
    let mut bodies = Vec::new();
    let mut rest = data;
    while rest.len() >= 4 {
        let (prefix, body) = rest.split_at(4);
        let length = u32::from_be_bytes(prefix.try_into().expect("4-byte prefix")) as usize;
        if length > body.len() {
            return Err("Invalid length: Not enough bytes in the vector".into());
        }
        let (nal, after) = body.split_at(length);
        bodies.push(nal);
        rest = after;
    }
    Ok(bodies)
}

/// Split a length-prefixed (4-byte, big-endian) HEVC sample into Annex-B
/// NALUs, each returned with its `00 00 00 01` start code.
///
/// The sample comes from the network. A length that runs past the end is an
/// error; 1-3 trailing bytes too short to hold a length prefix (padding) are
/// ignored. Both used to panic on the slice index and kill the decode task.
///
/// The decoders use [`hevc_nalu_bodies`] (no copy); this stays as the tests'
/// reference for the same split.
#[cfg(test)]
pub fn parse_hevc_nalu(data: &[u8]) -> Result<Vec<Vec<u8>>, Box<dyn Error>> {
    const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];
    let mut nalus: Vec<Vec<u8>> = Vec::new();
    let mut rest = data;
    while rest.len() >= 4 {
        let (prefix, body) = rest.split_at(4);
        let length = u32::from_be_bytes(prefix.try_into().expect("4-byte prefix")) as usize;
        if length > body.len() {
            return Err("Invalid length: Not enough bytes in the vector".into());
        }
        let (nal, after) = body.split_at(length);
        // One allocation per NALU: start code + body.
        let mut nalu = Vec::with_capacity(START_CODE.len() + nal.len());
        nalu.extend_from_slice(&START_CODE);
        nalu.extend_from_slice(nal);
        nalus.push(nalu);
        rest = after;
    }
    if !rest.is_empty() {
        log::trace!("[mp4] ignoring {} trailing byte(s) after the last NALU", rest.len());
    }
    Ok(nalus)
}

pub fn aac_sampling_frequency_index_to_u32(index: u8) -> u32 {
    match index {
        0 => 96000,
        1 => 88200,
        2 => 64000,
        3 => 48000,
        4 => 44100,
        5 => 32000,
        6 => 24000,
        7 => 22050,
        8 => 16000,
        9 => 12000,
        10 => 11025,
        11 => 8000,
        12 => 7350,
        _ => 44100,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nalus_get_start_codes_and_trailing_padding_is_ignored() {
        // Two NALUs (3 and 1 bytes), then 2 bytes of padding.
        let sample = [0, 0, 0, 3, 0xAA, 0xBB, 0xCC, 0, 0, 0, 1, 0xDD, 0, 0];
        let nalus = parse_hevc_nalu(&sample).unwrap();
        assert_eq!(nalus, vec![vec![0, 0, 0, 1, 0xAA, 0xBB, 0xCC], vec![0, 0, 0, 1, 0xDD]]);
    }

    #[test]
    fn nalu_bodies_match_the_start_code_split() {
        let sample = [0, 0, 0, 3, 0xAA, 0xBB, 0xCC, 0, 0, 0, 1, 0xDD, 0, 0];
        let bodies = hevc_nalu_bodies(&sample).unwrap();
        let with_codes = parse_hevc_nalu(&sample).unwrap();
        assert_eq!(bodies.len(), with_codes.len());
        for (b, n) in bodies.iter().zip(&with_codes) {
            assert_eq!(&n[4..], *b);
        }
        assert!(hevc_nalu_bodies(&[0, 0, 0, 9, 1, 2]).is_err());
    }

    #[test]
    fn nalu_length_past_the_end_is_an_error_not_a_panic() {
        assert!(parse_hevc_nalu(&[0, 0, 0, 9, 1, 2]).is_err());
    }

    #[test]
    fn truncated_sidx_is_an_error_not_a_panic() {
        let full: Vec<u8> = [
            &[0u8, 0, 0, 44][..], b"sidx", &[0, 0, 0, 0], &[0, 0, 0, 1], &[0, 0, 0x3E, 0x80],
            &[0, 0, 0, 0], &[0, 0, 0, 0], &[0, 0], &[0, 1],
            &[0, 0, 0x10, 0], &[0, 0, 0x3E, 0x80], &[0x90, 0, 0, 0],
        ]
        .concat();
        let mut ok = full.as_slice();
        let sidx = parse_sidx(&mut ok).unwrap();
        assert_eq!(sidx.entries.len(), 1);
        assert_eq!(sidx.entries[0].reference_size, 0x1000);
        for cut in 0..full.len() {
            let mut short = &full[..cut];
            assert!(parse_sidx(&mut short).is_err(), "cut at {cut} must be an error");
        }
    }
}
