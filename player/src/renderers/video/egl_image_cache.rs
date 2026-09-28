//! Bookkeeping for the Android GLES path's EGL image cache (the EGL calls
//! live in `video_gles_egl`). Kept free of EGL types so its tests run on
//! every host.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]

/// EGL images kept across frames, keyed by the AHardwareBuffer pointer.
///
/// ImageReader cycles a fixed set of 16-32 buffers, and importing one (the
/// gralloc import behind eglCreateImageKHR) cost hundreds of µs of render
/// thread CPU on every frame on PowerVR and Mali. An EGL image holds its own
/// reference to the buffer (EGL_ANDROID_image_native_buffer), so a cached
/// pointer cannot be reused by a different buffer while its image is here.
/// Entries not drawn for [`EglImageCache::IDLE_FRAMES`] are destroyed, which
/// lets the buffers of a replaced ImageReader (seek, switch, reconfigure) go.
#[derive(Default)]
pub(super) struct EglImageCache {
    /// AHB pointer -> (EGLImageKHR, frame it was last drawn in).
    images: std::collections::HashMap<usize, (usize, u64)>,
    frame: u64,
}

impl EglImageCache {
    /// About three seconds at 60 fps: longer than any buffer stays out of
    /// rotation in steady playback, short enough to release a dead reader.
    pub(super) const IDLE_FRAMES: u64 = 180;
    /// Upper bound whatever the rotation: two readers' worth of buffers.
    pub(super) const MAX_IMAGES: usize = 64;

    /// The cached image for `ahb`, marked as used in this frame.
    pub(super) fn get(&mut self, ahb: usize) -> Option<usize> {
        let frame = self.frame;
        self.images.get_mut(&ahb).map(|(image, used)| {
            *used = frame;
            *image
        })
    }

    pub(super) fn insert(&mut self, ahb: usize, image: usize) {
        self.images.insert(ahb, (image, self.frame));
    }

    /// End of a frame: the images to destroy (idle too long, or beyond the
    /// cap, oldest first).
    pub(super) fn end_frame(&mut self) -> Vec<usize> {
        self.frame += 1;
        let frame = self.frame;
        let mut stale: Vec<usize> = self
            .images
            .iter()
            .filter(|(_, (_, used))| frame - used > Self::IDLE_FRAMES)
            .map(|(ahb, _)| *ahb)
            .collect();
        if self.images.len() - stale.len() > Self::MAX_IMAGES {
            let mut live: Vec<(usize, u64)> = self
                .images
                .iter()
                .filter(|(ahb, _)| !stale.contains(ahb))
                .map(|(ahb, (_, used))| (*ahb, *used))
                .collect();
            live.sort_by_key(|(_, used)| *used);
            let excess = live.len() - Self::MAX_IMAGES;
            stale.extend(live.into_iter().take(excess).map(|(ahb, _)| ahb));
        }
        stale
            .into_iter()
            .filter_map(|ahb| self.images.remove(&ahb).map(|(image, _)| image))
            .collect()
    }

    pub(super) fn drain(&mut self) -> Vec<usize> {
        self.images.drain().map(|(_, (image, _))| image).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::EglImageCache;

    #[test]
    fn a_buffer_in_rotation_keeps_its_image() {
        let mut c = EglImageCache::default();
        c.insert(0x10, 0xA);
        for _ in 0..1000 {
            assert_eq!(c.get(0x10), Some(0xA));
            assert!(c.end_frame().is_empty());
        }
    }

    #[test]
    fn an_idle_buffer_is_released() {
        let mut c = EglImageCache::default();
        c.insert(0x10, 0xA);
        c.insert(0x20, 0xB);
        let mut released = Vec::new();
        for _ in 0..=EglImageCache::IDLE_FRAMES + 1 {
            c.get(0x20);
            released.extend(c.end_frame());
        }
        assert_eq!(released, vec![0xA]);
        assert_eq!(c.get(0x10), None);
        assert_eq!(c.get(0x20), Some(0xB));
    }

    #[test]
    fn the_cap_drops_the_oldest_first() {
        let mut c = EglImageCache::default();
        for i in 0..EglImageCache::MAX_IMAGES + 3 {
            c.insert(i, 1000 + i);
            assert!(c.end_frame().len() <= 1);
        }
        assert_eq!(c.images.len(), EglImageCache::MAX_IMAGES);
        assert_eq!(c.get(0), None);
        assert_eq!(c.get(EglImageCache::MAX_IMAGES + 2), Some(1000 + EglImageCache::MAX_IMAGES + 2));
    }
}
