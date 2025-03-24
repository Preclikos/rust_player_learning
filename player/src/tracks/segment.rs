use reqwest::{
    header::{HeaderValue, RANGE},
    Client,
};
use std::{error::Error, time::Duration};

#[derive(Clone)]
pub struct Segment {
    base_url: String,
    file_url: String,
    start: u64,
    end: u64,
    start_time: Duration,
    start_time_base: u32,
    end_time: Duration,
    end_time_base: u32,
    timescale: u32,
}

impl Segment {
    pub fn new(
        base_url: &String,
        file_url: &String,
        start: u64,
        end: u64,
        start_time_base: Option<u32>,
        end_time_base: Option<u32>,
        timescale: Option<u32>,
    ) -> Result<Self, Box<dyn Error>> {
        let timescale = match timescale {
            Some(time) => time,
            None => 0,
        };

        let start_time = match start_time_base {
            Some(time) => (Duration::from_secs((time / timescale) as u64), time),
            None => (Duration::from_secs(0), 0),
        };

        let end_time = match end_time_base {
            Some(time) => (Duration::from_secs((time / timescale) as u64), time),
            None => (Duration::from_secs(0), 0),
        };

        Ok(Segment {
            base_url: base_url.to_string(),
            file_url: file_url.to_string(),
            start,
            end,
            start_time: start_time.0,
            start_time_base: start_time.1,
            end_time: end_time.0,
            end_time_base: end_time.1,
            timescale,
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
