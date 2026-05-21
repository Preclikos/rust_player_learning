fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }

    // Some Rust deps (e.g. `ring` via rustls) drag in C++ code with
    // libc++ symbols (`__cxa_pure_virtual`, `operator new`, …). Without an
    // explicit link directive the resulting .so has no NEEDED entry for
    // libc++_shared, so the runtime linker never tries to resolve those
    // symbols. Adding the dylib link here makes the NDK linker write the
    // NEEDED entry — Gradle then bundles libc++_shared.so into the APK
    // and the dynamic linker picks it up automatically on dlopen.
    println!("cargo:rustc-link-lib=dylib=c++_shared");

    // cpal uses AAudio (libaaudio.so) which is only available from API 26+.
    // cargo-ndk defaults to platform 21 which doesn't include it in the
    // sysroot. Adding the API-26 path here makes the build work without
    // requiring --platform 26 on the cargo-ndk command line.
    let ndk_root = find_ndk_root();
    if let Some(ndk) = ndk_root {
        let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
        let ndk_triple = match target_arch.as_str() {
            "aarch64" => "aarch64-linux-android",
            "arm" => "arm-linux-androideabi",
            "x86_64" => "x86_64-linux-android",
            "x86" => "i686-linux-android",
            _ => return,
        };
        let host_tag = if cfg!(target_os = "windows") {
            "windows-x86_64"
        } else if cfg!(target_os = "macos") {
            "darwin-x86_64"
        } else {
            "linux-x86_64"
        };
        println!(
            "cargo:rustc-link-search=native={}/toolchains/llvm/prebuilt/{}/sysroot/usr/lib/{}/26",
            ndk.display(), host_tag, ndk_triple
        );
    }
}

fn find_ndk_root() -> Option<std::path::PathBuf> {
    // 1. Standard env vars set by the user or CI.
    for var in &["ANDROID_NDK_HOME", "ANDROID_NDK_ROOT", "NDK_HOME"] {
        if let Ok(p) = std::env::var(var) {
            let path = std::path::PathBuf::from(p);
            if path.exists() {
                return Some(path);
            }
        }
    }

    // 2. Android SDK env vars — NDK may live under $SDK/ndk/<version>.
    for sdk_var in &["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Ok(sdk) = std::env::var(sdk_var) {
            let ndk_dir = std::path::Path::new(&sdk).join("ndk");
            if let Some(p) = newest_subdir(&ndk_dir) {
                return Some(p);
            }
        }
    }

    // 3. Android Studio default location (Windows / macOS / Linux).
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    let candidates = [
        format!("{}/AppData/Local/Android/Sdk/ndk", home),  // Windows
        format!("{}/Library/Android/sdk/ndk", home),         // macOS
        format!("{}/Android/Sdk/ndk", home),                  // Linux
    ];
    for candidate in &candidates {
        let ndk_dir = std::path::Path::new(candidate);
        if let Some(p) = newest_subdir(ndk_dir) {
            return Some(p);
        }
    }

    None
}

fn newest_subdir(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();
    entries.into_iter().last()
}
