use std::error::Error;

fn read_u32(data: &mut &[u8]) -> u32 {
    let result = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    *data = &data[4..]; // Move the slice forward
    result
}

fn read_u16(data: &mut &[u8]) -> u16 {
    let result = u16::from_be_bytes([data[0], data[1]]);
    *data = &data[2..]; // Move the slice forward
    result
}

#[derive(Debug)]
pub struct SidxEntry {
    pub reference_type: u8,
    pub reference_size: u64,
    pub subsegment_duration: u32,
    pub starts_with_sap: u8,
    pub sap_type: u8,
    pub sap_delta: u32,
}

#[derive(Debug)]
pub struct SidxBox {
    pub size: u32,
    pub version: u8,
    pub flags: u32,
    pub reference_id: u32,
    pub timescale: u32,
    pub earliest_presentation_time: u32,
    pub first_offset: u32,
    pub entry_count: u16,
    pub entries: Vec<SidxEntry>,
}

pub fn parse_sidx(data: &mut &[u8]) -> Result<SidxBox, Box<dyn Error>> {
    // Read the size of the box (we ignore the size field here)
    let size = read_u32(data);

    // Read the box type (should be "sidx")
    let type_str = &data[0..4];
    let type_str = String::from_utf8_lossy(type_str);
    *data = &data[4..]; // Move the slice forward

    if type_str != "sidx" {
        return Err("Not a valid sidx box!".into());
    }

    // Read version and flags
    let version_flags = read_u32(data);
    let version = (version_flags >> 24) as u8;
    let flags = version_flags & 0x00FFFFFF;

    let reference_id = read_u32(data);
    let timescale = read_u32(data);
    let earliest_presentation_time = read_u32(data);
    let first_offset = read_u32(data); // Offset where the first segment starts

    *data = &data[2..]; // Move the slice forward reserved 16bits

    let entry_count = read_u16(data); // Number of entries in the sidx
    let mut entries = Vec::new();

    // Parse the entries and generate segments
    for _ in 0..entry_count {
        let chunk = read_u32(data);
        let reference_type = (chunk >> 31) as u8;
        let reference_size = u64::from(chunk & 0x7FFFFFFF);
        let subsegment_duration = read_u32(data);
        let chunk = read_u32(data);
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

pub fn find_mdat_box(file_data: &[u8]) -> Option<(usize, usize)> {
    let mut offset = 0;
    while offset + 8 <= file_data.len() {
        // Read the box size (4 bytes, big-endian).
        let size = u32::from_be_bytes([
            file_data[offset],
            file_data[offset + 1],
            file_data[offset + 2],
            file_data[offset + 3],
        ]) as usize;
        // Read the box type (4 bytes).
        let typ = &file_data[offset + 4..offset + 8];
        if typ == b"mdat" {
            return Some((offset, size));
        }
        // Prevent an infinite loop on invalid box size.
        if size < 8 {
            break;
        }
        offset += size;
    }
    None
}
