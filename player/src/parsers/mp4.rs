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

pub fn apped_hevc_header(mut nalu_data: Vec<u8>) -> Vec<u8> {
    let nalu_header: Vec<u8> = vec![0x00, 0x00, 0x00, 0x01];
    let mut nalu = nalu_header.clone();
    nalu.append(nalu_data.as_mut());

    nalu
}

pub fn parse_hevc_nalu(data: &[u8]) -> Result<Vec<Vec<u8>>, Box<dyn Error>> {
    let mut nalus: Vec<Vec<u8>> = vec![];

    let nalu_header: Vec<u8> = vec![0x00, 0x00, 0x00, 0x01];

    let mut index = 0;
    while index < data.len() {
        let byte_array: [u8; 4] = match data[index..index + 4].try_into() {
            Ok(success) => success,
            Err(e) => return Err(format!("Failed to convert {}", e).into()),
        };

        let length_u32 = u32::from_be_bytes(byte_array);
        let length = usize::try_from(length_u32).unwrap();

        index += 4;

        if index + length > data.len() {
            return Err("Invalid length: Not enough bytes in the vector".into());
        }

        let chunk: Vec<u8> = data[index..index + length].to_vec();
        let mut chunk_mut = chunk.clone();
        index += length;

        let mut nalu = nalu_header.clone();
        nalu.append(&mut chunk_mut);

        nalus.push(nalu);
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
