pub mod audio;
pub mod segment;
pub mod text;
pub mod video;

use crate::manifest::{
    find_audio_channel_count, find_descriptor_values, find_switchable_ids,
    slice_adaptation_set, slice_representation, AdaptationSet, Representation, MPD,
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

        // earliest_presentation_time + first_offset are u64 (version-1 sidx may
        // exceed 32 bits); subsegment_duration is always 32-bit per spec.
        let mut start = sidx.earliest_presentation_time;

        let mut start_byte = offset + sidx.first_offset;
        for entry in entires.iter() {
            let end = start_byte + (entry.reference_size - 1);

            let end_time = start + u64::from(entry.subsegment_duration);

            let segment = Segment::new(
                base_url,
                file_url,
                start_byte,
                end,
                Some(start),
                Some(end_time),
                Some(sidx.timescale),
            )?;
            segments.push(segment);

            start_byte += entry.reference_size;
            start = end_time;
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

        // @width / @height are optional in DASH — some manifests (notably
        // series episodes) omit them on the Representation. Default to 0 and let
        // the decoder report the real dimensions from the bitstream / decoded
        // frame (VideoToolbox via the CVPixelBuffer, FFmpeg via the stream,
        // MediaCodec via INFO_OUTPUT_FORMAT_CHANGED), which is authoritative
        // anyway — same as every other field we read from the bitstream.
        let width = representation.width.unwrap_or(0);
        let height = representation.height.unwrap_or(0);

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
        let codecs_str = codecs.as_str();
        let dv_codec_sniff = codecs_str.starts_with("dvh1")
            || codecs_str.starts_with("dvhe")
            || codecs_str.starts_with("dvav");
        // HEVC Main10 / VP9 profile-2 == 10-bit. Real DASH manifests that
        // carry these codecs almost always carry HDR10 content (no one
        // ships Main10 SDR over DASH — they'd ship plain Main 8-bit and
        // save ~10% bitrate). When the explicit Colour/Transfer
        // metadata is missing, treat the 10-bit codec profile as
        // sufficient evidence of HDR10. Inaccurate corner case (true
        // 10-bit SDR) is preferable to false-negative HDR detection,
        // which silently breaks AbrVideoProfile::SdrOnly / capability
        // filtering downstream.
        let hdr10_codec_sniff = codecs_str.starts_with("hvc1.2")
            || codecs_str.starts_with("hev1.2")
            || codecs_str.starts_with("vp09.2");

        let mut hdr10 = hdr10_codec_sniff;
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
            width,
            height,
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

    /// Parse one `<AdaptationSet contentType="video">` into a `VideoAdaptation`
    /// plus the set of *other* AdaptationSet ids it advertises as
    /// switching-equivalent via `urn:mpeg:dash:adaptation-set-switching:2016`.
    /// The caller (`parse_tracks`) uses the second value to merge connected
    /// components into a single logical adaptation before exposing the list
    /// to the host.
    async fn parse_video_adaptation(
        base_url: &String,
        adaptation: &AdaptationSet,
        raw_mpd: &str,
        http: &HttpClient,
    ) -> Result<(VideoAdaptation, Vec<u32>), Box<dyn Error>> {
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

        // SwitchingProperty IDs come from the same adaptation_block we just
        // sliced for HDR / DV detection — no second pass over raw_mpd.
        let switchable_with = adaptation_block
            .map(find_switchable_ids)
            .unwrap_or_default();

        Ok((video_adaptation, switchable_with))
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

    async fn parse_text_adaptation(
        base_url: &str,
        adaptation: &AdaptationSet,
        http: &HttpClient,
    ) -> Result<TextAdaptation, Box<dyn Error>> {
        let mut text_representations: Vec<TextRepresenation> = vec![];

        for representation in &adaptation.representations {
            let url_base = base_url.to_string();
            let file_url = representation.base_url.value.to_string();

            let mut segment_init = None;
            let mut segment_range = None;
            let mut segments: Vec<Segment> = Vec::new();
            let mut single_file_url: Option<String> = None;

            match &representation.segment_base {
                Some(sb) => {
                    // CMAF text track: init + sidx-driven subsegments.
                    let (init_start, init_end) = Self::parse_range(&sb.initialization.range)?;
                    segment_init = Some(Segment::new(
                        &url_base, &file_url, init_start, init_end, None, None, None,
                    )?);
                    let (idx_start, idx_end) = Self::parse_range(&sb.index_range)?;
                    let idx_seg = Segment::new(
                        &url_base, &file_url, idx_start, idx_end, None, None, None,
                    )?;
                    segments = match idx_seg.download(http, RequestKind::InitSegment).await {
                        Ok(dl) => {
                            let mut slice = &dl.data[..];
                            match parse_sidx(&mut slice) {
                                Ok(sidx) => Self::generate_segments_from_sidx(
                                    &url_base,
                                    &file_url,
                                    sidx,
                                    idx_end + 1,
                                )
                                .unwrap_or_default(),
                                Err(e) => {
                                    log::warn!(
                                        "[text] sidx parse failed for repr {}: {}",
                                        representation.id, e
                                    );
                                    Vec::new()
                                }
                            }
                        }
                        Err(e) => {
                            log::warn!(
                                "[text] sidx download failed for repr {}: {}",
                                representation.id, e
                            );
                            Vec::new()
                        }
                    };
                    segment_range = Some(idx_seg);
                }
                None => {
                    // Single-file delivery — the typical "external .vtt
                    // per language" pattern. Compose the absolute URL
                    // once; text_play will GET it whole at activation.
                    if !file_url.is_empty() {
                        single_file_url = Some(format!("{}{}", url_base, file_url));
                    }
                }
            }

            text_representations.push(TextRepresenation {
                id: representation.id,
                codecs: representation.codecs.clone().unwrap_or_default(),
                mime_type: representation.mime_type.clone(),
                bandwidth: representation.bandwidth,
                base_url: url_base,
                file_url,
                segment_init,
                segment_range,
                segments,
                single_file_url,
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

        // For video: collect (adaptation, switchable_with) so we can merge
        // switching-equivalent adaptation sets after the loop.
        let mut video_pairs: Vec<(VideoAdaptation, Vec<u32>)> = vec![];
        let mut audio_adaptations: Vec<AudioAdaptation> = vec![];
        let mut text_adaptations: Vec<TextAdaptation> = vec![];

        let adaptation_sets = &period.adaptation_sets;
        for adaptation in adaptation_sets {
            let content_type = adaptation.content_type.as_str();
            match content_type {
                "video" => {
                    let pair = Self::parse_video_adaptation(
                        &base_url, adaptation, raw_mpd, http,
                    )
                    .await?;
                    video_pairs.push(pair);
                }
                "audio" => {
                    let value = Self::parse_audio_adaptation(
                        &base_url, adaptation, raw_mpd, http,
                    )
                    .await?;
                    audio_adaptations.push(value);
                }
                "text" => {
                    let value = Self::parse_text_adaptation(&base_url, adaptation, http).await?;
                    text_adaptations.push(value);
                }
                other => log::warn!("AdaptationSet content_type {} ignored", other),
            }
        }

        let video_adaptations = merge_switchable_adaptations(video_pairs);

        Ok(TracksResult {
            video: video_adaptations,
            audio: audio_adaptations,
            text: text_adaptations,
        })
    }
}

/// Merge AdaptationSets that declare themselves switching-equivalent via
/// `urn:mpeg:dash:adaptation-set-switching:2016` into one logical
/// `VideoAdaptation` per connected component. Sets with no switching links
/// pass through unchanged.
///
/// The switching relation is treated as an undirected graph (the spec says
/// it SHOULD be symmetric, but real manifests are sometimes one-sided —
/// be defensive: an `A→B` edge implies `B→A`). Connected components are
/// computed via a simple union-find on adaptation IDs.
///
/// Merge strategy for the resulting `VideoAdaptation`:
///   * `id` — taken from the lowest-id member (stable ordering for callers)
///   * `max_width` / `max_height` — element-wise max across members
///   * `frame_rate` — taken from the member with the highest-bandwidth rep
///   * `subsegment_alignment` — AND of all (conservative: a single non-aligned
///     member disqualifies the whole pool)
///   * `roles` — deduplicated union
///   * `representations` — concatenated, sorted by bandwidth ascending so
///     bitrate-based ABR sees a monotonic ladder (cheap, stable, matches
///     the order most UIs want to render)
fn merge_switchable_adaptations(
    pairs: Vec<(VideoAdaptation, Vec<u32>)>,
) -> Vec<VideoAdaptation> {
    if pairs.is_empty() {
        return vec![];
    }

    // Index by id so the switching declarations can resolve to positions.
    let id_to_idx: std::collections::HashMap<u32, usize> = pairs
        .iter()
        .enumerate()
        .map(|(i, (a, _))| (a.id, i))
        .collect();

    // Union-find over positions in `pairs`.
    let n = pairs.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]]; // path compression
            x = parent[x];
        }
        x
    }
    fn union(parent: &mut [usize], a: usize, b: usize) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            // Attach larger-index root to smaller, so the *lowest* original
            // adaptation id ends up as the component representative once
            // we read back ids in member order. Order is insertion order
            // (== MPD order), so this generally aligns with manifest id order.
            if ra < rb {
                parent[rb] = ra;
            } else {
                parent[ra] = rb;
            }
        }
    }

    for (i, (_, switchable)) in pairs.iter().enumerate() {
        for sw_id in switchable {
            if let Some(&j) = id_to_idx.get(sw_id) {
                if j != i {
                    union(&mut parent, i, j);
                }
            } else {
                log::warn!(
                    "[tracks] adaptation-set-switching: id {} not found among video adaptations — ignoring",
                    sw_id,
                );
            }
        }
    }

    // Group by component root, preserving insertion order.
    let mut groups: std::collections::BTreeMap<usize, Vec<usize>> =
        std::collections::BTreeMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }

    // Materialise each group into a merged VideoAdaptation. Single-member
    // groups pass through unchanged.
    let mut pairs_opt: Vec<Option<(VideoAdaptation, Vec<u32>)>> =
        pairs.into_iter().map(Some).collect();

    groups
        .into_values()
        .map(|members| {
            if members.len() == 1 {
                pairs_opt[members[0]].take().expect("group member taken twice").0
            } else {
                let taken: Vec<VideoAdaptation> = members
                    .iter()
                    .map(|&i| pairs_opt[i].take().expect("group member taken twice").0)
                    .collect();
                merge_video_adaptation_group(taken)
            }
        })
        .collect()
}

fn merge_video_adaptation_group(members: Vec<VideoAdaptation>) -> VideoAdaptation {
    debug_assert!(members.len() >= 2);

    let mut iter = members.into_iter();
    let mut base = iter.next().expect("non-empty group");

    // Find the rep with the highest bandwidth across the whole group; its
    // adaptation's frame_rate wins (matches the fallback rule used for a
    // single adaptation in parse_video_adaptation).
    let mut best_fps_repr_bw = base
        .representations
        .iter()
        .map(|r| r.bandwidth)
        .max()
        .unwrap_or(0);
    let mut best_fps = base.frame_rate.clone();

    for sibling in iter {
        // ---- scalar fields ----
        base.max_width = base.max_width.max(sibling.max_width);
        base.max_height = base.max_height.max(sibling.max_height);
        base.subsegment_alignment = base.subsegment_alignment && sibling.subsegment_alignment;
        let sibling_max_bw = sibling
            .representations
            .iter()
            .map(|r| r.bandwidth)
            .max()
            .unwrap_or(0);
        if sibling_max_bw > best_fps_repr_bw {
            best_fps_repr_bw = sibling_max_bw;
            best_fps = sibling.frame_rate.clone();
        }

        // ---- roles: deduplicated union, insertion order ----
        for role in sibling.roles {
            if !base.roles.contains(&role) {
                base.roles.push(role);
            }
        }

        // ---- representations: append; final sort happens once below ----
        base.representations.extend(sibling.representations);
    }

    base.frame_rate = best_fps;
    base.representations
        .sort_by_key(|r| r.bandwidth);

    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracks::segment::Segment;

    fn stub_seg() -> Segment {
        Segment::new(&String::new(), &String::new(), 0, 0, None, None, None)
            .expect("stub segment")
    }

    fn rep(id: u32, bandwidth: u64, codecs: &str, w: u32, h: u32) -> VideoRepresenation {
        VideoRepresenation {
            id,
            base_url: String::new(),
            file_url: String::new(),
            segment_init: stub_seg(),
            segment_range: stub_seg(),
            segments: Vec::new(),
            bandwidth,
            codecs: codecs.to_string(),
            mime_type: "video/mp4".to_string(),
            width: w,
            height: h,
            sar: "1:1".to_string(),
            hdr10: false,
            dolby_vision: false,
        }
    }

    fn adaptation(
        id: u32,
        max_w: u32,
        max_h: u32,
        frame_rate: &str,
        reps: Vec<VideoRepresenation>,
    ) -> VideoAdaptation {
        VideoAdaptation {
            id,
            frame_rate: frame_rate.to_string(),
            max_width: max_w,
            max_height: max_h,
            subsegment_alignment: true,
            roles: vec!["main".to_string()],
            representations: reps,
        }
    }

    #[test]
    fn no_switching_passes_through_unchanged() {
        let a1 = adaptation(1, 1920, 1080, "24/1", vec![rep(100, 6_000_000, "hvc1.1.6", 1920, 1080)]);
        let a2 = adaptation(2, 3840, 2160, "24/1", vec![rep(200, 14_000_000, "hvc1.2.4", 3840, 2160)]);
        let pairs = vec![(a1.clone(), vec![]), (a2.clone(), vec![])];

        let out = merge_switchable_adaptations(pairs);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, 1);
        assert_eq!(out[1].id, 2);
    }

    #[test]
    fn mutual_switching_merges_into_one_adaptation() {
        // Mirrors the user's real manifest: SDR 1080p adaptation declares
        // switching to HDR 4K adaptation, and vice versa.
        let sdr = adaptation(
            223705,
            1920,
            1080,
            "24/1",
            vec![
                rep(315074, 6_000_000, "hvc1.1.6.L120.90", 1920, 1080),
                rep(315075, 3_000_000, "hvc1.1.6.L93.90", 1280, 720),
                rep(315076, 1_500_000, "hvc1.1.6.L90.90", 854, 480),
            ],
        );
        let hdr = adaptation(
            223714,
            3840,
            2160,
            "24/1",
            vec![
                rep(315086, 14_000_000, "hvc1.2.4.L150.90", 3840, 2160),
                rep(315087, 8_000_000, "hvc1.2.4.L150.90", 2560, 1440),
            ],
        );
        let pairs = vec![
            (sdr, vec![223714]),
            (hdr, vec![223705]),
        ];

        let out = merge_switchable_adaptations(pairs);
        assert_eq!(out.len(), 1, "switchable adaptations should merge");

        let merged = &out[0];
        assert_eq!(merged.id, 223705, "lowest member id wins");
        assert_eq!(merged.max_width, 3840, "max_width = max across members");
        assert_eq!(merged.max_height, 2160, "max_height = max across members");
        assert_eq!(merged.representations.len(), 5, "all 5 reps in pool");

        // Reps sorted ascending by bandwidth so ABR sees a monotonic ladder.
        let bandwidths: Vec<u64> =
            merged.representations.iter().map(|r| r.bandwidth).collect();
        assert_eq!(bandwidths, vec![1_500_000, 3_000_000, 6_000_000, 8_000_000, 14_000_000]);
    }

    #[test]
    fn asymmetric_switching_still_merges() {
        // Defensive: even if only one side declares the link, treat it as
        // undirected so we don't strand the host on a half-broken manifest.
        let a = adaptation(1, 1280, 720, "24/1", vec![rep(10, 3_000_000, "hvc1.1.6", 1280, 720)]);
        let b = adaptation(2, 1920, 1080, "24/1", vec![rep(20, 6_000_000, "hvc1.1.6", 1920, 1080)]);
        // Only `a` declares switching to `b`.
        let pairs = vec![(a, vec![2]), (b, vec![])];

        let out = merge_switchable_adaptations(pairs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].representations.len(), 2);
    }

    #[test]
    fn chain_switching_collapses_to_one_pool() {
        // A↔B and B↔C — all three should land in the same component even
        // though A and C don't directly mention each other.
        let a = adaptation(1, 854, 480, "24/1", vec![rep(10, 1_000_000, "hvc1", 854, 480)]);
        let b = adaptation(2, 1280, 720, "24/1", vec![rep(20, 3_000_000, "hvc1", 1280, 720)]);
        let c = adaptation(3, 1920, 1080, "24/1", vec![rep(30, 6_000_000, "hvc1", 1920, 1080)]);
        let pairs = vec![
            (a, vec![2]),
            (b, vec![1, 3]),
            (c, vec![2]),
        ];

        let out = merge_switchable_adaptations(pairs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].representations.len(), 3);
        assert_eq!(out[0].max_width, 1920);
    }

    #[test]
    fn switching_to_unknown_id_is_ignored() {
        let a = adaptation(1, 1280, 720, "24/1", vec![rep(10, 3_000_000, "hvc1", 1280, 720)]);
        // References id 99 which isn't in the video set (might be audio,
        // or an MPD typo). Should be tolerated and `a` passes through alone.
        let pairs = vec![(a, vec![99])];

        let out = merge_switchable_adaptations(pairs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, 1);
    }

    #[test]
    fn roles_are_deduplicated_on_merge() {
        let mut a = adaptation(1, 1280, 720, "24/1", vec![rep(10, 3_000_000, "hvc1", 1280, 720)]);
        a.roles = vec!["main".to_string()];
        let mut b = adaptation(2, 1920, 1080, "24/1", vec![rep(20, 6_000_000, "hvc1", 1920, 1080)]);
        b.roles = vec!["main".to_string(), "alternate".to_string()];

        let pairs = vec![(a, vec![2]), (b, vec![1])];
        let out = merge_switchable_adaptations(pairs);
        assert_eq!(out[0].roles, vec!["main".to_string(), "alternate".to_string()]);
    }

    #[test]
    fn subsegment_alignment_is_anded() {
        let mut a = adaptation(1, 1280, 720, "24/1", vec![rep(10, 3_000_000, "hvc1", 1280, 720)]);
        a.subsegment_alignment = true;
        let mut b = adaptation(2, 1920, 1080, "24/1", vec![rep(20, 6_000_000, "hvc1", 1920, 1080)]);
        b.subsegment_alignment = false;

        let pairs = vec![(a, vec![2]), (b, vec![1])];
        let out = merge_switchable_adaptations(pairs);
        // Conservative: one non-aligned member disqualifies the merged pool.
        assert!(!out[0].subsegment_alignment);
    }
}
