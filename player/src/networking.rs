use std::{error::Error, time::Instant};

use reqwest::{
    header::{CONTENT_LENGTH, RANGE},
    Client, Response,
};

#[derive(Debug)]
enum RequestType {
    Get,
    Post(String),       // POST with a request body
    RangeGet(u64, u64), // GET with a byte range
}

pub struct HttpClient {
    client: Client,
}

impl HttpClient {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }

    async fn send_request(
        &self,
        url: &str,
        request_type: RequestType,
    ) -> Result<(Response, u64, u64), Box<dyn Error>> {
        let start_time = Instant::now();
        let mut req = self.client.get(url);
        let mut sent_bytes = 0;

        match &request_type {
            RequestType::Get => {
                println!("Sending GET request to: {}", url);
            }
            RequestType::Post(body) => {
                println!("Sending POST request to: {}", url);
                sent_bytes = body.len() as u64;
                req = self.client.post(url).body(body.clone());
            }
            RequestType::RangeGet(start, end) => {
                let range_value = format!("bytes={}-{}", start, end);
                println!("Sending RANGE GET request to: {} with {}", url, range_value);
                req = req.header(RANGE, range_value);
            }
        }

        let response = req.send().await?;
        let duration = start_time.elapsed().as_millis();

        // Measure bytes received
        let received_bytes = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);

        println!(
            "Request completed in {} ms, sent {} bytes, received {} bytes",
            duration, sent_bytes, received_bytes
        );
        Ok((response, sent_bytes, received_bytes))
    }
}
