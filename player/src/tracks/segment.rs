use std::{error::Error, time::Duration};

use crate::net::{HttpClient, RequestKind};

#[derive(Clone)]
pub struct Segment {
    base_url: String,
    file_url: String,
    start: u64,
    end: u64,
    start_time: Duration,
    end_time: Duration,
}

/// Bandwidth-tracking result from a segment download: payload bytes plus
/// wall-clock elapsed. Used by the EWMA in `video_play` to compute the
/// `Position.bandwidth_bps` field on `PlayerEvent`.
pub struct DownloadResult {
    pub data: Vec<u8>,
    pub elapsed: Duration,
}

impl Segment {
    pub fn new(
        base_url: &String,
        file_url: &String,
        start: u64,
        end: u64,
        start_time_base: Option<u64>,
        end_time_base: Option<u64>,
        timescale: Option<u32>,
    ) -> Result<Self, Box<dyn Error>> {
        let timescale = match timescale {
            Some(time) => time,
            None => 0,
        };

        let to_duration = |time: u64| -> Duration {
            if timescale == 0 {
                Duration::ZERO
            } else {
                Duration::from_micros(time * 1_000_000 / (timescale as u64))
            }
        };

        let start_time = start_time_base.map(to_duration).unwrap_or(Duration::ZERO);
        let end_time = end_time_base.map(to_duration).unwrap_or(Duration::ZERO);

        Ok(Segment {
            base_url: base_url.to_string(),
            file_url: file_url.to_string(),
            start,
            end,
            start_time,
            end_time,
        })
    }

    pub fn start_time(&self) -> Duration {
        self.start_time
    }

    pub fn end_time(&self) -> Duration {
        self.end_time
    }

    /// Fetch the segment's byte range through the centralised `HttpClient`,
    /// returning both payload and elapsed time so callers can compute an
    /// EWMA bandwidth estimate.
    ///
    /// `kind` lets the caller distinguish init segments (`InitSegment`)
    /// from media segments (`Segment`) so an interceptor can route them
    /// differently (e.g. different CDN, different auth).
    pub async fn download(
        &self,
        http: &HttpClient,
        kind: RequestKind,
    ) -> Result<DownloadResult, Box<dyn Error + Send + Sync>> {
        let url = format!("{}{}", &self.base_url, &self.file_url);
        let started = std::time::Instant::now();
        let bytes = http.get_range(url, kind, self.start, self.end).await?;
        Ok(DownloadResult {
            data: bytes.to_vec(),
            elapsed: started.elapsed(),
        })
    }
}
