fn main() {
    // Build scripts run on the HOST, so `cfg!(target_os = ...)` reflects the
    // host, not the build target — use CARGO_CFG_TARGET_OS (as the windows
    // branch below does). The old `cfg!(target_os = "linux")` emitted `-lva`
    // whenever the *runner* was Linux, which broke Android cross-builds on a
    // Linux CI (the NDK has no libva) while passing on a Windows host.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "linux" {
        println!("cargo:rustc-link-lib=va");
    }

    if target_os == "windows" {
        // Compile the FFmpeg log shim. The shim forward-declares av_log_format_line2
        // and av_log_set_callback without including libavutil/log.h, so no FFmpeg
        // include path is needed here — symbols resolve against the libavutil
        // already linked by ffmpeg-sys-next.
        cc::Build::new()
            .file("c/ffmpeg_log_shim.c")
            .compile("ffmpeg_log_shim");
    }
}
