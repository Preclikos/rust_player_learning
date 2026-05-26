pub mod audio;
pub mod segment;
pub mod text;
pub mod video;

use crate::manifest::{
    find_audio_channel_count, find_descriptor_values, slice_adaptation_set,
    slice_representation, AdaptationSet, Representation, MPD,
};
use crate::net::{HttpClient, RequestKind};
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
    pub async fn new(
        base_url: String,
        mpd: &MPD,
        raw_mpd: &str,
        http: &HttpClient,
    ) -> Result<Self, Box<dyn Error>> {
        let duration = Self::parse_duration(mpd)?;
        let tracks = Self::parse_tracks(base_url, mpd, raw_mpd, http).await?;

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
                log::error!(
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

        let mut start = sidx.earliest_presentation_time.clone();

        let mut start_byte = offset + u64::from(sidx.first_offset);
        for entry in entires.iter() {
            let end = start_byte + (entry.reference_size - 1);

            let end_time_u32 = (start + entry.subsegment_duration);

            let segment = Segment::new(
                base_url,
                file_url,
                start_byte,
                end,
                Some(start),
                Some(end_time_u32),
                Some(sidx.timescale),
            )?;
            segments.push(segment);

            start_byte += entry.reference_size;
            start = end_time_u32;
        }

        Ok(segments)
    }

    async fn parse_video_representation(
        base_url: &String,
        representation: &Representation,
        adaptation_block: Option<&str>,
        http: &HttpClient,
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

        // @sar (Sample Aspect Ratio) is optional in DASH and defaults to
        // 1:1 per the spec. Don't reject the stream over a missing one.
        let sar = representation
            .sar
            .clone()
            .unwrap_or_else(|| "1:1".to_string());

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
        let init_segment =
            Segment::new(&url_base, &file_url, init_start, init_end, None, None, None)?;
        let (index_start, index_end) = Self::parse_range(&base_segment.index_range)?;
        let index_segment = Segment::new(
            &url_base,
            &file_url,
            index_start,
            index_end,
            None,
            None,
            None,
        )?;

        let segments: Vec<Segment>;

        match representation.mime_type.as_str() {
            "video/mp4" => {
                let index_dl = index_segment
                    .download(http, RequestKind::InitSegment)
                    .await
                    .map_err(|e| -> Box<dyn Error> {
                        format!("video sidx download: {}", e).into()
                    })?;
                let mut index_slice = &index_dl.data[..];
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

        // HDR / DV detection. Per spec §5: HDR10 = ColourPrimaries=9 OR
        // TransferCharacteristics=16/18 in a SupplementalProperty. DV = an
        // EssentialProperty with dolby_vision in the schemeIdUri (codec
        // sniff via `dvh1.*` is a fallback for malformed manifests). We
        // walk the raw XML of the AdaptationSet (which we can't parse via
        // serde — see manifest.rs note) plus the Representation block.
        let codecs_for_dv = codecs.as_str();
        let dv_codec_sniff = codecs_for_dv.starts_with("dvh1")
            || codecs_for_dv.starts_with("dvhe")
            || codecs_for_dv.starts_with("dvav");

        let mut hdr10 = false;
        let mut dolby_vision = dv_codec_sniff;
        if let Some(block) = adaptation_block {
            let rep_block =
                slice_representation(block, representation.id).unwrap_or(block);
            let combined = format!("{}\n{}", block, rep_block);
            // ColourPrimaries=9 (BT.2020) → HDR10.
            let primaries = find_descriptor_values(&combined, "ColourPrimaries");
            if primaries.iter().any(|v| v == "9") {
                hdr10 = true;
            }
            // TransferCharacteristics=16 (PQ / SMPTE ST 2084) or 18 (HLG)
            // → HDR10.
            let xfer = find_descriptor_values(&combined, "TransferCharacteristics");
            if xfer.iter().any(|v| v == "16" || v == "18") {
                hdr10 = true;
            }
            // EssentialProperty dolby_vision_profile.
            if !find_descriptor_values(&combined, "dolby_vision").is_empty() {
                dolby_vision = true;
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
            hdr10,
            dolby_vision,
        };

        Ok(video_representation)
    }

    async fn parse_audio_representation(
        base_url: &String,
        representation: &Representation,
        adaptation_block: Option<&str>,
        http: &HttpClient,
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
        let init_segment =
            Segment::new(&url_base, &file_url, init_start, init_end, None, None, None)?;
        let (index_start, index_end) = Self::parse_range(&base_segment.index_range)?;
        let index_segment = Segment::new(
            &url_base,
            &file_url,
            index_start,
            index_end,
            None,
            None,
            None,
        )?;

        let segments: Vec<Segment>;

        match representation.mime_type.as_str() {
            "audio/mp4" => {
                let index_dl = index_segment
                    .download(http, RequestKind::InitSegment)
                    .await
                    .map_err(|e| -> Box<dyn Error> {
                        format!("audio sidx download: {}", e).into()
                    })?;
                let mut index_slice = &index_dl.data[..];
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

        // Pull <AudioChannelConfiguration value="N"/> from the raw XML
        // for this Representation. (Serde can't read it for the same
        // reason it can't read SupplementalProperty — interleaved with
        // other children.)
        let channels = adaptation_block
            .and_then(|b| slice_representation(b, representation.id))
            .and_then(|rb| find_audio_channel_count(rb))
            .or_else(|| {
                // Some manifests put it at AdaptationSet level instead.
                adaptation_block.and_then(|b| find_audio_channel_count(b))
            });

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
            channels,
        };

        Ok(audio_representation)
    }

    async fn parse_video_adaptation(
        base_url: &String,
        adaptation: &AdaptationSet,
        raw_mpd: &str,
        http: &HttpClient,
    ) -> Result<VideoAdaptation, Box<dyn Error>> {
        let mut video_representations: Vec<VideoRepresenation> = vec![];

        // DASH lets @frameRate, @maxWidth, @maxHeight live either on the
        // AdaptationSet or on each Representation. Real-world manifests
        // (single-rep, certain encoders) routinely omit them at the
        // adaptation level. Fall back gracefully so we don't reject the
        // whole stream over missing metadata.
        //
        // For frame_rate, prefer the highest-bandwidth representation's
        // value — adaptation sets with mixed framerates (e.g. low-rez
        // 24fps + high-rez 60fps) should advertise the headline rate,
        // not whichever happened to be listed first.
        let frame_rate = adaptation.frame_rate.clone().or_else(|| {
            adaptation
                .representations
                .iter()
                .filter(|r| r.frame_rate.is_some())
                .max_by_key(|r| r.bandwidth)
                .and_then(|r| r.frame_rate.clone())
        })
        .unwrap_or_default();

        let max_width = adaptation.max_width.unwrap_or_else(|| {
            adaptation
                .representations
                .iter()
                .filter_map(|r| r.width)
                .max()
                .unwrap_or(0)
        });

        let max_height = adaptation.max_height.unwrap_or_else(|| {
            adaptation
                .representations
                .iter()
                .filter_map(|r| r.height)
                .max()
                .unwrap_or(0)
        });
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

        let adaptation_block = slice_adaptation_set(raw_mpd, adaptation.id);

        let representations = &adaptation.representations;
        for representation in representations {
            let video_representation = Self::parse_video_representation(
                base_url,
                representation,
                adaptation_block,
                http,
            )
            .await?;
            video_representations.push(video_representation);
        }

        // Flatten serde's `Vec<Property>` to plain role-value strings.
        let roles = adaptation
            .roles
            .iter()
            .filter_map(|p| p.value.clone())
            .collect();

        let video_adaptation = VideoAdaptation {
            id: adaptation.id,
            subsegment_alignment,
            frame_rate,
            max_width,
            max_height,
            //par,
            roles,
            representations: video_representations,
        };

        Ok(video_adaptation)
    }

    async fn parse_audio_adaptation(
        base_url: &String,
        adaptation: &AdaptationSet,
        raw_mpd: &str,
        http: &HttpClient,
    ) -> Result<AudioAdaptation, Box<dyn Error>> {
        let mut audio_representations: Vec<AudioRepresentation> = vec![];

        // @lang is optional in DASH (and often omitted on the single
        // audio adaptation of a mono-lingual stream). Treat absence as
        // unknown — AudioAdaptation::language() already returns None when
        // the string is empty.
        let lang = adaptation.lang.clone().unwrap_or_default();

        let subsegment_alignment = match &adaptation.subsegment_alignment {
            Some(value) => *value,
            None => false,
        };

        let adaptation_block = slice_adaptation_set(raw_mpd, adaptation.id);

        let representations = &adaptation.representations;
        for representation in representations {
            let audio_representation = Self::parse_audio_representation(
                base_url,
                representation,
                adaptation_block,
                http,
            )
            .await?;
            audio_representations.push(audio_representation);
        }

        let roles = adaptation
            .roles
            .iter()
            .filter_map(|p| p.value.clone())
            .collect();

        Ok(AudioAdaptation {
            id: adaptation.id,
            lang,
            subsegment_alignment,
            roles,
            representations: audio_representations,
        })
    }

    fn parse_text_adaptation(
        _base_url: &str,
        adaptation: &AdaptationSet,
    ) -> Result<TextAdaptation, Box<dyn Error>> {
        let mut text_representations: Vec<TextRepresenation> = vec![];

        for representation in &adaptation.representations {
            text_representations.push(TextRepresenation {
                id: representation.id,
                codecs: representation.codecs.clone().unwrap_or_default(),
                mime_type: representation.mime_type.clone(),
                bandwidth: representation.bandwidth,
            });
        }

        let lang = adaptation.lang.clone().unwrap_or_default();
        let roles = adaptation
            .roles
            .iter()
            .filter_map(|p| p.value.clone())
            .collect();

        Ok(TextAdaptation {
            id: adaptation.id,
            lang,
            roles,
            representations: text_representations,
        })
    }

    async fn parse_tracks(
        base_url: String,
        mpd: &MPD,
        raw_mpd: &str,
        http: &HttpClient,
    ) -> Result<TracksResult, Box<dyn Error>> {
        let period = match mpd.periods.first() {
            Some(success) => success,
            None => {
                log::error!("Failed to parse Period");
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
                    let value = Self::parse_video_adaptation(
                        &base_url, adaptation, raw_mpd, http,
                    )
                    .await?;
                    video_adaptations.push(value);
                }
                "audio" => {
                    let value = Self::parse_audio_adaptation(
                        &base_url, adaptation, raw_mpd, http,
                    )
                    .await?;
                    audio_adaptations.push(value);
                }
                "text" => {
                    let value = Self::parse_text_adaptation(&base_url, adaptation)?;
                    text_adaptations.push(value);
                }
                other => log::warn!("AdaptationSet content_type {} ignored", other),
            }
        }

        Ok(TracksResult {
            video: video_adaptations,
            audio: audio_adaptations,
            text: text_adaptations,
        })
    }
}
