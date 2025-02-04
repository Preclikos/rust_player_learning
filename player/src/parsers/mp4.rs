use crate::tracks::segment::Segment;

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

pub fn parse_sidx(segment_offset: u64, data: &mut &[u8]) {
    // Read the size of the box (we ignore the size field here)
    let size = read_u32(data);

    // Read the box type (should be "sidx")
    let type_str = &data[0..4];
    let type_str = String::from_utf8_lossy(type_str);
    *data = &data[4..]; // Move the slice forward

    if type_str != "sidx" {
        println!("Not a valid sidx box!");
        return;
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

    // Print basic sidx box info
    println!("sidx box information:");
    println!("  size: {}", size);
    println!("  version: {}", version);
    println!("  flags: {:#X}", flags);
    println!("  reference_id: {}", reference_id);
    println!("  timescale: {}", timescale);
    println!(
        "  earliest_presentation_time: {}",
        earliest_presentation_time
    );
    println!("  first_offset: {}", first_offset);
    println!("  entry_count: {}", entry_count);

    let mut startByte = segment_offset + u64::from(size) + u64::from(first_offset);
    // Parse the entries and generate segments
    for i in 0..entry_count {
        // Read chunk (4 bytes)
        let mut chunk = read_u32(data);
        // Extract referenceType (1 bit) and referenceSize (31 bits)
        let reference_type = (chunk >> 31) & 0x1;
        let reference_size = chunk & 0x7FFFFFFF;

        // Read the subsegment duration (4 bytes)
        let subsegment_duration = read_u32(data);

        // Read chunk for SAP-related information (4 bytes)
        chunk = read_u32(data);

        // Extract startsWithSap, sapType, sapDelta
        let starts_with_sap = (chunk >> 31) & 0x1;
        let sap_type = (chunk >> 28) & 0x7;
        let sap_delta = chunk & 0x0FFFFFFF;

        // Debug output (optional)
        println!("Processing segment {}", i + 1);
        println!("  Segment Start Size: {}", startByte);
        println!(
            "  Segment End Size: {}",
            startByte + u64::from(reference_size) - 1
        );
        println!("  Reference Type: {}", reference_type);
        println!("  Reference Size: {}", reference_size);
        println!("  Subsegment Duration: {}", subsegment_duration);
        println!("  Starts with SAP: {}", starts_with_sap);
        println!("  SAP Type: {}", sap_type);
        println!("  SAP Delta: {}", sap_delta);

        startByte += u64::from(reference_size);
    }
}
