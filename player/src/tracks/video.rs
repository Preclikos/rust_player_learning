use std::error::Error;

pub struct VideoAdaptation {
    pub frame_rate: String,
    pub representations: Vec<VideoRepresenation>,
}

pub struct VideoRepresenation {}
