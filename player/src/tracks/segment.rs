use reqwest::{
    blocking::{get, Client},
    header::{HeaderValue, RANGE},
};
use std::{error::Error, str::Bytes};

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

    pub fn download(&mut self) -> Result<Vec<u8>, Box<dyn Error>> {
        let client = Client::new();

        let range_value = format!("bytes={}-{}", self.start, self.end);
        let range_header = HeaderValue::from_str(&range_value).unwrap();

        let url = format!("{}{}", &self.base_url, &self.file_url);
        let response = client.get(url).header(RANGE, range_header).send();

        let response_bytes = match response {
            Ok(success) => success.bytes(),
            Err(e) => return Err(format!("Segment response error: {}", e).into()),
        };

        let bytes = match response_bytes {
            Ok(success) => success,
            Err(e) => return Err(format!("Cannot read segment bytes: {}", e).into()),
        };

        Ok(bytes.to_vec())
    }
}
