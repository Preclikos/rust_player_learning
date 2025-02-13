use super::segment::Segment;

#[derive(Clone)]
pub struct AudioAdaptation {
    pub id: u32,

    pub lang: String,

    pub subsegment_alignment: bool,

    pub representations: Vec<AudioRepresentation>,
}

#[derive(Clone)]
pub struct AudioRepresentation {
    pub id: u32,

    pub base_url: String,
    pub file_url: String,

    pub segment_init: Segment,
    pub segment_range: Segment,
    pub segments: Vec<Segment>,

    pub bandwidth: u64,

    pub codecs: String,
    pub mime_type: String,

    pub audio_sampling_rate: u32,
}
