# Android build

Gradle build for the Android side of the player:

- **`:rustplayer`** (`rustplayer/`) — the library module. Kotlin API
  (`RustPlayer`, `RustPlayerProvider`, `PlayerBridge`, `NativeBridge`) + the
  `bridge-android` cdylib (`librustplayer.so`) and `libc++_shared.so` per ABI.
  Published as the AAR `io.github.preclikos:rustplayer` to GitHub Packages
  (`.github/workflows/publish-android.yml`).
- **`:app`** — the demo app, sources in `examples/android/` (wired in by
  `settings.gradle.kts`). It depends on the local `:rustplayer` module, so the
  smoke test and the published AAR build from one source.

The host Activity owns the `SurfaceView`s and hands their `Surface`s to the
player over JNI — the **embed** model real apps use, not a winit
`NativeActivity`.

## Prerequisites

- JDK 17+ (Android Studio's bundled `jbr` works) and the Android SDK.
- The NDK version pinned in `gradle/libs.versions.toml` (`ndk`); the
  `buildRust*` tasks resolve it from the SDK.
- `cargo-ndk` and the Rust targets:

```
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
cargo install cargo-ndk
```

## Build & install

```
cd platform/android/android
./gradlew :app:installDebug          # cargo-ndk (all ABIs) + APK + install
./gradlew :rustplayer:assembleRelease # just the AAR
```

From the repo root, `examples/android/run.ps1` does build → install → launch
→ filtered logcat in one go. For a Rust-only rebuild (arm64, no Gradle) use
`platform/android/build_rust.ps1`.

Consuming the published AAR from another app:

```kotlin
// settings.gradle.kts
maven {
    url = uri("https://maven.pkg.github.com/Preclikos/rust_dash_player")
    credentials { username = …; password = … /* token with read:packages */ }
}
// build.gradle.kts
implementation("io.github.preclikos:rustplayer:<version>")
```

## JNI naming

The Kotlin package is `io.github.preclikos.rustplayer`; the Rust exports in
`platform/android/src/lib.rs` are `Java_io_github_preclikos_rustplayer_NativeBridge_*`.
Renaming the package means renaming every export — they must match exactly.

## Layout

```
platform/android/android/
├── settings.gradle.kts             (:rustplayer + :app → examples/android)
├── build.gradle.kts                (root)
├── gradle/libs.versions.toml       (AGP / Kotlin / SDK / NDK versions)
└── rustplayer/
    ├── build.gradle.kts            (library + cargo-ndk tasks + maven-publish)
    └── src/main/kotlin/io/github/preclikos/rustplayer/
examples/android/
├── build.gradle.kts                (demo app)
├── run.ps1
└── src/main/kotlin/io/github/preclikos/rustplayer/MainActivity.kt
```
