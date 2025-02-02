pub mod audio;
pub mod text;
pub mod video;

use crate::manifest::{AdaptationSet, MPD};
use crate::tracks::audio::{AudioAdaptation, AudioRepresentation};
use crate::tracks::text::{TextAdaptation, TextRepresenation};
use crate::tracks::video::{VideoAdaptation, VideoRepresenation};
use iso8601_duration::Duration as IsoDuration;
use std::error::Error;
use std::time::Duration;

fn iso_to_std_duration(iso_duration: &IsoDuration) -> Duration {
    let total_seconds =
        iso_duration.hour * 3600.0 + iso_duration.minute * 60.0 + iso_duration.second;

    Duration::from_secs_f32(total_seconds)
}

struct TracksResult {
    video: Vec<VideoAdaptation>,
    audio: Vec<AudioAdaptation>,
    text: Vec<TextAdaptation>,
}

pub struct Tracks {
    duration: Duration,
    video: Vec<VideoAdaptation>,
    audio: Vec<AudioAdaptation>,
    text: Vec<TextAdaptation>,
}

impl Tracks {
    pub fn new(mpd: &MPD) -> Result<Self, Box<dyn Error>> {
        let duration = Self::parse_duration(mpd)?;
        let tracks = Self::parse_tracks(mpd)?;

        Ok(Tracks {
            duration,
            video: tracks.video,
            audio: tracks.audio,
            text: tracks.text,
        })
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
        let mut video_representations: Vec<VideoRepresenation> = vec![];

        let frame_rate = match &adaptation.frame_rate {
            Some(value) => value,
            None => {
                return Err(format!(
                    "Cannot get frameRate from AdaptationSet Id: {}",
                    adaptation.id
                )
                .into())
            }
        };

        let representations = &adaptation.representations;
        for representation in representations {
            let video_representation = VideoRepresenation {};
            video_representations.push(video_representation);
        }

        let video_adaptation = VideoAdaptation {
            frame_rate: frame_rate.to_string(),
            representations: video_representations,
        };

        Ok(video_adaptation)
    }

    fn parse_audio_adaptation(
        adaptation: &AdaptationSet,
    ) -> Result<AudioAdaptation, Box<dyn Error>> {
        let mut audio_representations: Vec<AudioRepresentation> = vec![];

        let representations = &adaptation.representations;
        for representation in representations {
            audio_representations.push(AudioRepresentation {});
        }

        Ok(AudioAdaptation {
            representations: audio_representations,
        })
    }

    fn parse_text_adaptation(adaptation: &AdaptationSet) -> Result<TextAdaptation, Box<dyn Error>> {
        let mut text_representations: Vec<TextRepresenation> = vec![];

        let representations = &adaptation.representations;
        for representation in representations {
            text_representations.push(TextRepresenation {});
        }

        Ok(TextAdaptation {
            representations: text_representations,
        })
    }

    fn parse_tracks(mpd: &MPD) -> Result<TracksResult, Box<dyn Error>> {
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

        Ok(TracksResult {
            video: video_adaptations,
            audio: audio_adaptations,
            text: text_adaptations,
        })
    }
}
