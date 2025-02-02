use crate::manifest::MPD;
use iso8601_duration::Duration as IsoDuration;
use std::error::Error;
use std::time::Duration;

fn iso_to_std_duration(iso_duration: &IsoDuration) -> Duration {
    let total_seconds =
        iso_duration.hour * 3600.0 + iso_duration.minute * 60.0 + iso_duration.second;

    Duration::from_secs_f32(total_seconds)
}

pub struct TrackManager<'a> {
    mpd: &'a MPD,
    duration: Option<Duration>,
}

impl<'a> TrackManager<'a> {
    pub fn new(mpd: &'a MPD) -> Self {
        TrackManager {
            mpd: &mpd,
            duration: None,
        }
    }

    pub fn parse_tracks(&mut self) -> Result<(), Box<dyn Error>> {
        let period_first = self.mpd.periods.first();

        let period = match period_first {
            Some(success) => success,
            None => {
                eprintln!("Failed to parse Period");
                return Err("Failed to parse Period".into());
            }
        };

        let parsed_duration = &self.mpd.media_presentation_duration.parse::<IsoDuration>();
        match parsed_duration {
            Ok(success) => {
                let duration = iso_to_std_duration(&success);
                self.duration = Some(duration)
            }
            Err(e) => {
                eprintln!("Failed to parse Priod - duration: {}", e.position);
                return Err("Failed to parse Period".into());
            }
        };

        Ok(())
    }
}
