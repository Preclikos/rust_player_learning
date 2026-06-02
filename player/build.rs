fn main() {
    if cfg!(target_os = "linux") {
        println!("cargo:rustc-link-lib=va");
    }

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        cc::Build::new()
            .file("c/ffmpeg_log_shim.c")
            .include(format!("{}/vendor/windows-x64/include", manifest_dir))
            .compile("ffmpeg_log_shim");
    }
}
