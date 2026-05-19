# Android build

Gradle wrapper around the `app-android` Rust cdylib. Produces an APK that
loads `libapp_android.so` via `NativeActivity` and dispatches to
`android_main()`.

## One-time setup

1. Install Android Studio (or just the command-line tools) and the NDK
   (version listed in `app/build.gradle` under `ndkVersion`).
2. Set `ANDROID_NDK_HOME` to the NDK install path, or `ANDROID_HOME` and let
   cargo-ndk resolve it.
3. Install Rust target and helpers:
   ```
   rustup target add aarch64-linux-android
   cargo install cargo-ndk
   ```
4. Generate the Gradle wrapper (first time only):
   ```
   cd app-android/android
   gradle wrapper --gradle-version 8.7
   ```

## Build & install

```
cd app-android/android
./gradlew assembleDebug          # build APK only
./gradlew installDebug           # build + push to connected device
adb logcat -s RustStdoutStderr   # follow Rust log output
```

The `buildRustDebug` Gradle task runs `cargo ndk` and emits
`libapp_android.so` into `app/src/main/jniLibs/arm64-v8a/`, which Gradle
then packages into the APK automatically.

## Adding more ABIs

Edit `abiFilters` in `app/build.gradle` to include `armeabi-v7a`, `x86`, or
`x86_64`. The `buildRust*` tasks pass the same value to `cargo ndk -t`, so
add the corresponding Rust targets:

```
rustup target add armv7-linux-androideabi
rustup target add x86_64-linux-android
```

## Current state

The APK launches and `android_main()` runs winit's event loop, but
`Player` is not yet hooked up — the player library still depends on
FFmpeg + D3D11VA/VAAPI which are not available on Android. The decoder
refactor (`HwVideoDecoder` trait + `MediaCodec` impl) is the next step.
