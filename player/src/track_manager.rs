use crate::manifest::{AdaptationSet, MPD};
use crate::tracks::audio::AudioAdaptation;
use crate::tracks::text::TextAdaptation;
use crate::tracks::video::VideoAdaptation;
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
        let tracks = Self::parse_tracks(mpd)?;
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

    fn parse_video_adaptation(
        adaptation: &AdaptationSet,
    ) -> Result<VideoAdaptation, Box<dyn Error>> {
        Ok(VideoAdaptation {})
    }

    fn parse_audio_adaptation(
        adaptation: &AdaptationSet,
    ) -> Result<AudioAdaptation, Box<dyn Error>> {
        Ok(AudioAdaptation {})
    }

    fn parse_text_adaptation(adaptation: &AdaptationSet) -> Result<TextAdaptation, Box<dyn Error>> {
        Ok(TextAdaptation {})
    }

    fn parse_tracks(mpd: &MPD) -> Result<(), Box<dyn Error>> {
        let period = match mpd.periods.first() {
            Some(success) => success,
            None => {
                eprintln!("Failed to parse Period");
                return Err("Failed to parse Period".into());
            }
        };

        let mut video_adaptations: Vec<VideoAdaptation> = vec![];
        let mut audio_adaptations: Vec<AudioAdaptation> = vec![];
        let mut text_adaptations: Vec<TextAdaptation> = vec![];

        let adaptation_sets = &period.adaptation_sets;
        for adaptation in adaptation_sets {
            let content_type = adaptation.content_type.as_str();
            match content_type {
                "video" => {
                    let value = Self::parse_video_adaptation(adaptation)?;
                    video_adaptations.push(value);
                }
                "audio" => {
                    let value = Self::parse_audio_adaptation(adaptation)?;
                    audio_adaptations.push(value);
                }
                "text" => {
                    let value = Self::parse_text_adaptation(adaptation)?;
                    text_adaptations.push(value);
                }
                _ => println!("Not supported"),
            }
        }

        Ok(())
    }
}
