pub struct VideoAdaptation {
    pub id: u32,

    pub frame_rate: String,
    pub max_width: u32,
    pub max_height: u32,

    pub par: String,
    pub subsegment_alignment: bool,

    pub representations: Vec<VideoRepresenation>,
}

pub struct VideoRepresenation {
    pub id: u32,

    pub bandwidth: u64,

    pub codecs: String,
    pub mime_type: String,

    pub width: u32,
    pub height: u32,
    pub sar: String,
}
