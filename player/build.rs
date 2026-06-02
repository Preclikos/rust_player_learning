fn main() {
    if cfg!(target_os = "linux") {
        println!("cargo:rustc-link-lib=va");
    }

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
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
