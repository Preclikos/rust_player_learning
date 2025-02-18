pub mod audio;
pub mod segment;
pub mod text;
pub mod video;

use crate::manifest::{AdaptationSet, Representation, MPD};
use crate::parsers::mp4::{parse_sidx, SidxBox};
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

#[derive(Clone)]
pub struct Tracks {
    pub duration: Duration,
    pub video: Vec<VideoAdaptation>,
    pub audio: Vec<AudioAdaptation>,
    pub text: Vec<TextAdaptation>,
}

impl Tracks {
    pub async fn new(base_url: String, mpd: &MPD) -> Result<Self, Box<dyn Error>> {
        let duration = Self::parse_duration(mpd)?;
        let tracks = Self::parse_tracks(base_url, mpd).await?;

        Ok(Tracks {
            duration,
            video: tracks.video,
            audio: tracks.audio,
            text: tracks.text,
        })
    }

    fn parse_range(range: &str) -> Result<(u64, u64), Box<dyn Error>> {
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

    fn generate_segments_from_sidx(
        base_url: &String,
        file_url: &String,
        sidx: SidxBox,
        offset: u64,
    ) -> Result<Vec<Segment>, Box<dyn Error>> {
        let entires = sidx.entries;
        let mut segments: Vec<Segment> = vec![];

        let mut start_byte = offset + u64::from(sidx.first_offset);
        for entry in entires.iter() {
            let end = start_byte + (entry.reference_size - 1);
            let segment = Segment::new(base_url, file_url, start_byte, end)?;
            segments.push(segment);

            start_byte += entry.reference_size
        }

        Ok(segments)
    }

    async fn parse_video_representation(
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

        let (init_start, init_end) = Self::parse_range(&base_segment.initialization.range)?;
        let init_segment = Segment::new(&url_base, &file_url, init_start, init_end)?;
        let (index_start, index_end) = Self::parse_range(&base_segment.index_range)?;
        let index_segment = Segment::new(&url_base, &file_url, index_start, index_end)?;

        let segments: Vec<Segment>;

        match representation.mime_type.as_str() {
            "video/mp4" => {
                let index_vec = index_segment.download().await?;
                let mut index_slice = &index_vec[..];
                let sidx = parse_sidx(&mut index_slice)?;
                segments =
                    Self::generate_segments_from_sidx(&url_base, &file_url, sidx, index_end + 1)?;
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
            file_url,
            bandwidth: representation.bandwidth,
            codecs,
            mime_type: representation.mime_type.to_string(),
            width: *width,
            height: *height,
            sar,
            segment_init: init_segment,
            segment_range: index_segment,
            segments,
        };

        Ok(video_representation)
    }

    async fn parse_audio_representation(
        base_url: &String,
        representation: &Representation,
    ) -> Result<AudioRepresentation, Box<dyn Error>> {
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

        let audio_sampling_rate = match &representation.audio_sampling_rate {
            Some(value) => value,
            None => {
                return Err(format!(
                    "Cannot get audioSamplingRate from Representation Id: {}",
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

        let (init_start, init_end) = Self::parse_range(&base_segment.initialization.range)?;
        let init_segment = Segment::new(&url_base, &file_url, init_start, init_end)?;
        let (index_start, index_end) = Self::parse_range(&base_segment.index_range)?;
        let index_segment = Segment::new(&url_base, &file_url, index_start, index_end)?;

        let segments: Vec<Segment>;

        match representation.mime_type.as_str() {
            "audio/mp4" => {
                let index_vec = index_segment.download().await?;
                let mut index_slice = &index_vec[..];
                let sidx = parse_sidx(&mut index_slice)?;
                segments =
                    Self::generate_segments_from_sidx(&url_base, &file_url, sidx, index_end + 1)?;
            }
            _ => {
                return Err(format!(
                    "Representation with type {} not supported",
                    representation.mime_type
                )
                .into())
            }
        }

        let audio_representation = AudioRepresentation {
            id: representation.id,
            base_url: url_base,
            file_url,
            bandwidth: representation.bandwidth,
            codecs,
            mime_type: representation.mime_type.to_string(),
            audio_sampling_rate: *audio_sampling_rate,
            segment_init: init_segment,
            segment_range: index_segment,
            segments,
        };

        Ok(audio_representation)
    }

    async fn parse_video_adaptation(
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
        /*
                let par = match &adaptation.par {
                    Some(value) => value.to_string(),
                    None => {
                        return Err(
                            format!("Cannot get PAR from AdaptationSet Id: {}", adaptation.id).into(),
                        )
                    }
                };
        */
        let subsegment_alignment = match &adaptation.subsegment_alignment {
            Some(value) => *value,
            None => false,
        };

        let representations = &adaptation.representations;
        for representation in representations {
            let video_representation =
                Self::parse_video_representation(base_url, representation).await?;
            video_representations.push(video_representation);
        }

        let video_adaptation = VideoAdaptation {
            id: adaptation.id,
            subsegment_alignment,
            frame_rate,
            max_width: *max_width,
            max_height: *max_height,
            //par,
            representations: video_representations,
        };

        Ok(video_adaptation)
    }

    async fn parse_audio_adaptation(
        base_url: &String,
        adaptation: &AdaptationSet,
    ) -> Result<AudioAdaptation, Box<dyn Error>> {
        let mut audio_representations: Vec<AudioRepresentation> = vec![];

        let lang = match &adaptation.lang {
            Some(value) => value.to_string(),
            None => {
                return Err(
                    format!("Cannot get lang from AdaptationSet Id: {}", adaptation.id).into(),
                )
            }
        };

        let subsegment_alignment = match &adaptation.subsegment_alignment {
            Some(value) => *value,
            None => false,
        };

        let representations = &adaptation.representations;
        for representation in representations {
            let audio_representation =
                Self::parse_audio_representation(base_url, representation).await?;
            audio_representations.push(audio_representation);
        }

        Ok(AudioAdaptation {
            id: adaptation.id,
            lang,
            subsegment_alignment,
            representations: audio_representations,
        })
    }

    fn parse_text_adaptation(
        base_url: &str,
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

    async fn parse_tracks(base_url: String, mpd: &MPD) -> Result<TracksResult, Box<dyn Error>> {
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
                    let value = Self::parse_video_adaptation(&base_url, adaptation).await?;
                    video_adaptations.push(value);
                }
                "audio" => {
                    let value = Self::parse_audio_adaptation(&base_url, adaptation).await?;
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
