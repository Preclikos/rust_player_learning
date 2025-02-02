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
    duration: Duration,
}

impl<'a> TrackManager<'a> {
    pub fn new(mpd: &'a MPD) -> Result<Self, Box<dyn Error>> {
        let duration = Self::parse_duration(mpd)?;
        Ok(TrackManager { mpd, duration })
    }

    fn parse_duration(mpd: &MPD) -> Result<Duration, Box<dyn Error>> {
        match mpd.media_presentation_duration.parse::<IsoDuration>() {
            Ok(iso_duration) => Ok(iso_to_std_duration(&iso_duration)),
            Err(e) => {
                eprintln!(
                    "Failed to parse media presentation duration: {}",
                    e.position
                );
                Err("Failed to parse media presentation duration".into())
            }
        }
    }

    fn parse_tracks(&mut self) -> Result<(), Box<dyn Error>> {
        let period_first = self.mpd.periods.first();

        let period = match period_first {
            Some(success) => success,
            None => {
                eprintln!("Failed to parse Period");
                return Err("Failed to parse Period".into());
            }
        };

        Ok(())
    }
}
