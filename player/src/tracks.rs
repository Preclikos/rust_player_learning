pub mod audio;
pub mod segment;
pub mod text;
pub mod video;

use crate::manifest::{AdaptationSet, Representation, MPD};
use crate::parsers::mp4::parse_sidx;
use crate::tracks::audio::{AudioAdaptation, AudioRepresentation};
use crate::tracks::text::{TextAdaptation, TextRepresenation};
use crate::tracks::video::{VideoAdaptation, VideoRepresenation};
use crate::utils::time::iso_to_std_duration;

use iso8601_duration::Duration as IsoDuration;
use segment::Segment;
use std::error::Error;
use std::time::Duration;

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
    pub fn new(base_url: String, mpd: &MPD) -> Result<Self, Box<dyn Error>> {
        let duration = Self::parse_duration(mpd)?;
        let tracks = Self::parse_tracks(base_url, mpd)?;

        Ok(Tracks {
            duration,
            video: tracks.video,
            audio: tracks.audio,
            text: tracks.text,
        })
    }

    fn parse_range(range: &String) -> Result<(u64, u64), Box<dyn Error>> {
        let mut parts = range.split('-');

        let start = parts.next().ok_or("Missing start number")?.parse::<u64>()?;
        let end = parts.next().ok_or("Missing end number")?.parse::<u64>()?;
        Ok((start, end))
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

    fn parse_video_representation(
        base_url: &String,
        representation: &Representation,
    ) -> Result<VideoRepresenation, Box<dyn Error>> {
        let codecs = match &representation.codecs {
            Some(value) => value.to_string(),
            None => {
                return Err(format!(
                    "Cannot get codecs from Representation Id: {}",
                    representation.id
                )
                .into())
            }
        };

        let width = match &representation.width {
            Some(value) => value,
            None => {
                return Err(format!(
                    "Cannot get width from Representation Id: {}",
                    representation.id
                )
                .into())
            }
        };

        let height = match &representation.height {
            Some(value) => value,
            None => {
                return Err(format!(
                    "Cannot get height from Representation Id: {}",
                    representation.id
                )
                .into())
            }
        };

        let sar = match &representation.sar {
            Some(value) => value.to_string(),
            None => {
                return Err(format!(
                    "Cannot get height from Representation Id: {}",
                    representation.id
                )
                .into())
            }
        };

        let url_base = base_url.to_string();
        let file_url = representation.base_url.value.to_string();

        let base_segment = match &representation.segment_base {
            Some(segment) => segment,
            None => {
                return Err(format!(
                    "Cannot get segmentBase from Representation Id: {}",
                    representation.id
                )
                .into())
            }
        };

        let init_range = Self::parse_range(&base_segment.initialization.range)?;
        let init_segment = Segment::new(&url_base, &file_url, init_range.0, init_range.1)?;
        let index_range = Self::parse_range(&base_segment.index_range)?;
        let index_segment = Segment::new(&url_base, &file_url, index_range.0, index_range.1)?;

        let segments: Vec<Segment> = vec![];

        match representation.mime_type.as_str() {
            "video/mp4" => {
                let index_vec = index_segment.download()?;
                let mut index_slice = &index_vec[..];
                let sidx = parse_sidx(index_range.1, &mut index_slice);
            }
            _ => {
                return Err(format!(
                    "Representation with type {} not supported",
                    representation.mime_type
                )
                .into())
            }
        }

        let video_representation = VideoRepresenation {
            id: representation.id,
            base_url: url_base,
            file_url: file_url,
            bandwidth: representation.bandwidth,
            codecs: codecs,
            mime_type: representation.mime_type.to_string(),
            width: *width,
            height: *height,
            sar: sar,
            segment_init: init_segment,
            segment_range: index_segment,
            segments: segments,
        };

        Ok(video_representation)
    }

    fn parse_video_adaptation(
        base_url: &String,
        adaptation: &AdaptationSet,
    ) -> Result<VideoAdaptation, Box<dyn Error>> {
        let mut video_representations: Vec<VideoRepresenation> = vec![];

        let frame_rate = match &adaptation.frame_rate {
            Some(value) => value.to_string(),
            None => {
                return Err(format!(
                    "Cannot get frameRate from AdaptationSet Id: {}",
                    adaptation.id
                )
                .into())
            }
        };

        let max_width = match &adaptation.max_width {
            Some(value) => value,
            None => {
                return Err(format!(
                    "Cannot get maxWidth from AdaptationSet Id: {}",
                    adaptation.id
                )
                .into())
            }
        };

        let max_height = match &adaptation.max_height {
            Some(value) => value,
            None => {
                return Err(format!(
                    "Cannot get maxHeight from AdaptationSet Id: {}",
                    adaptation.id
                )
                .into())
            }
        };

        let par = match &adaptation.par {
            Some(value) => value.to_string(),
            None => {
                return Err(
                    format!("Cannot get PAR from AdaptationSet Id: {}", adaptation.id).into(),
                )
            }
        };

        let subsegment_alignment = match &adaptation.subsegment_alignment {
            Some(value) => value.clone(),
            None => false,
        };

        let representations = &adaptation.representations;
        for representation in representations {
            let video_representation = Self::parse_video_representation(base_url, representation)?;
            video_representations.push(video_representation);
        }

        let video_adaptation = VideoAdaptation {
            id: adaptation.id,
            subsegment_alignment: subsegment_alignment,
            frame_rate: frame_rate,
            max_width: *max_width,
            max_height: *max_height,
            par: par,
            representations: video_representations,
        };

        Ok(video_adaptation)
    }

    fn parse_audio_adaptation(
        base_url: &String,
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

    fn parse_text_adaptation(
        base_url: &String,
        adaptation: &AdaptationSet,
    ) -> Result<TextAdaptation, Box<dyn Error>> {
        let mut text_representations: Vec<TextRepresenation> = vec![];

        let representations = &adaptation.representations;
        for representation in representations {
            text_representations.push(TextRepresenation {});
        }

        Ok(TextAdaptation {
            representations: text_representations,
        })
    }

    fn parse_tracks(base_url: String, mpd: &MPD) -> Result<TracksResult, Box<dyn Error>> {
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
                    let value = Self::parse_video_adaptation(&base_url, adaptation)?;
                    video_adaptations.push(value);
                }
                "audio" => {
                    let value = Self::parse_audio_adaptation(&base_url, adaptation)?;
                    audio_adaptations.push(value);
                }
                "text" => {
                    let value = Self::parse_text_adaptation(&base_url, adaptation)?;
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
