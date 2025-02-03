use std::error::Error;

pub struct Segment {
    base_url: String,
    file_url: String,

    start: u64,
    end: u64,
}

impl Segment {
    pub fn new(
        base_url: &String,
        file_url: &String,
        range: &String,
    ) -> Result<Self, Box<dyn Error>> {
        let mut parts = range.split('-');

        let start = parts.next().ok_or("Missing start number")?.parse::<u64>()?;
        let end = parts.next().ok_or("Missing end number")?.parse::<u64>()?;

        Ok(Segment {
            base_url: base_url.to_string(),
            file_url: file_url.to_string(),
            start,
            end,
        })
    }
}
