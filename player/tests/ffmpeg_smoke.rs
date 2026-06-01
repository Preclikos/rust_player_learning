//! Smoke test verifying that whatever FFmpeg system libs cargo linked
//! against actually contain the codecs / parsers / hwaccels the player
//! relies on at runtime. Run after a fresh
//! `scripts/build-ffmpeg.sh <platform>` (see FFMPEG_BUILD.md for env
//! var setup) so the test exercises that exact build.
//!
//! The point is to catch missing FFmpeg features here, not two repos
//! downstream during a BlackZone Console release.

#![cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]

use ffmpeg_sys_next as sys;
use std::ffi::{CStr, CString};

#[test]
fn ffmpeg_version_is_in_7_x_series() {
    // The player crate (and the ffmpeg-next 7.1 wrapper it pulls)
    // expects the 7.x ABI surface. Anything else means cargo linked
    // against a wrong system lib; the rest of this test file would
    // give misleading results.
    let v = unsafe { CStr::from_ptr(sys::av_version_info()) }
        .to_str()
        .expect("av_version_info should return UTF-8");
    assert!(
        v.starts_with('7') || v.starts_with("n7"),
        "linked FFmpeg version {v:?} is not 7.x — see FFMPEG_BUILD.md \
         for how to point cargo at the vendored build"
    );
}

#[test]
fn required_decoders_present() {
    // player/src/decoders/ffmpeg_audio.rs maps AudioCodec → AAC/AC3/EAC3
    // player/src/decoders/ffmpeg_hw.rs   maps VideoCodec → H264/HEVC
    // The software fallback path also needs the video decoders, so we
    // assert all five reachable by name.
    for name in ["h264", "hevc", "aac", "ac3", "eac3"] {
        let cname = CString::new(name).unwrap();
        let codec = unsafe { sys::avcodec_find_decoder_by_name(cname.as_ptr()) };
        assert!(
            !codec.is_null(),
            "{name} decoder missing — check --enable-decoder line in \
             scripts/build-ffmpeg.sh"
        );
    }
}

#[test]
fn required_parsers_present() {
    // build-ffmpeg.sh enables h264, hevc, aac, ac3 parsers. av_parser_init
    // returns a heap context we close immediately — we only care that
    // the lookup succeeded.
    use sys::AVCodecID::*;
    for (name, id) in [
        ("h264", AV_CODEC_ID_H264),
        ("hevc", AV_CODEC_ID_HEVC),
        ("aac", AV_CODEC_ID_AAC),
        ("ac3", AV_CODEC_ID_AC3),
    ] {
        unsafe {
            let p = sys::av_parser_init(id as i32);
            assert!(
                !p.is_null(),
                "{name} parser missing — check --enable-parser line in \
                 scripts/build-ffmpeg.sh"
            );
            sys::av_parser_close(p);
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn vaapi_hwaccel_present() {
    // ffmpeg_hw.rs:31 selects AV_HWDEVICE_TYPE_VAAPI on Linux. If
    // build-ffmpeg.sh's --enable-vaapi gets dropped (or the runner is
    // missing libva-dev at FFmpeg configure time), the device type
    // disappears from this iterator.
    assert!(
        hwaccel_present(sys::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI),
        "VAAPI not exposed by FFmpeg — re-run scripts/build-ffmpeg.sh \
         linux with libva-dev installed"
    );
}

#[cfg(target_os = "windows")]
#[test]
fn d3d11va_hwaccel_present() {
    // ffmpeg_hw.rs:29 selects AV_HWDEVICE_TYPE_D3D11VA on Windows.
    assert!(
        hwaccel_present(sys::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA),
        "D3D11VA not exposed by FFmpeg — check --enable-d3d11va in \
         scripts/build-ffmpeg.sh"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn videotoolbox_hwaccel_present() {
    // macOS path is native VTDecompressionSession (decoders/videotoolbox.rs),
    // but the FFmpeg build still ships videotoolbox hwaccels so the
    // software fallback works. Confirm they exist.
    assert!(
        hwaccel_present(sys::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX),
        "VideoToolbox not exposed by FFmpeg — check --enable-hwaccel \
         line in scripts/build-ffmpeg.sh macos-*"
    );
}

#[allow(dead_code)]
fn hwaccel_present(want: sys::AVHWDeviceType) -> bool {
    let mut t = sys::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE;
    loop {
        t = unsafe { sys::av_hwdevice_iterate_types(t) };
        if t == sys::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE {
            return false;
        }
        if t == want {
            return true;
        }
    }
}
