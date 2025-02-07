use reqwest::{
    header::{HeaderValue, RANGE},
    Client,
};
use std::error::Error;

#[derive(Clone)]
pub struct Segment {
    base_url: String,
    file_url: String,
    start: u64,
    pub end: u64,
}

impl Segment {
    pub fn new(
        base_url: &String,
        file_url: &String,

        start: u64,
        end: u64,
    ) -> Result<Self, Box<dyn Error>> {
        Ok(Segment {
            base_url: base_url.to_string(),
            file_url: file_url.to_string(),
            start,
            end,
        })
    }

    pub async fn download(&self) -> Result<Vec<u8>, Box<dyn Error>> {
        let client = Client::new();

        let range_value = format!("bytes={}-{}", self.start, self.end);
        let range_header = HeaderValue::from_str(&range_value).unwrap();

        let url = format!("{}{}", &self.base_url, &self.file_url);
        let response = client.get(url).header(RANGE, range_header).send().await;

        let response_bytes = match response {
            Ok(success) => success.bytes().await,
            Err(e) => return Err(format!("Segment response error: {}", e).into()),
        };

        let bytes = match response_bytes {
            Ok(success) => success,
            Err(e) => return Err(format!("Cannot read segment bytes: {}", e).into()),
        };

        Ok(bytes.to_vec())
    }
}
