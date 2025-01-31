use crate::manifest::MPD;

pub struct TrackManager<'a> {
    mpd: &'a MPD,
}

impl<'a> TrackManager<'a> {
    pub fn new(mpd: &'a MPD) -> Self {
        TrackManager { mpd }
    }
}
