fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Some Rust deps (e.g. `ring` via rustls) drag in C++ code with
    // libc++ symbols (`__cxa_pure_virtual`, `operator new`, …). Without an
    // explicit link directive the resulting .so has no NEEDED entry for
    // libc++_shared, so the runtime linker never tries to resolve those
    // symbols. Adding the dylib link here makes the NDK linker write the
    // NEEDED entry — Gradle then bundles libc++_shared.so into the APK
    // and the dynamic linker picks it up automatically on dlopen.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        println!("cargo:rustc-link-lib=dylib=c++_shared");
    }
}
