# Android build

Gradle wrapper around the `app-android` Rust cdylib. Produces an APK that
loads `libapp_android.so` via `NativeActivity` and dispatches to
`android_main()`.

## Verified status

- Rust cross-compile (`cargo ndk -t arm64-v8a -p app-android`) works:
  produces `app/src/main/jniLibs/arm64-v8a/libapp_android.so` (~23 MB debug).
- Gradle/APK build not yet tested — gradle not on PATH on this machine.

## One-time setup

### Toolchain (already present on this machine)

- Rust target: `aarch64-linux-android` ✓ installed
- `cargo-ndk` ✓ installed (4.1.2)
- Android NDK: `%LOCALAPPDATA%\Android\Sdk\ndk\26.0.10792818` ✓ installed

### What's missing — Gradle

Pick whichever is easiest for you:

**Option A — Open in Android Studio (recommended for first build)**

```
File → Open → app-android/android
```

Android Studio will detect the project, download the matching Gradle
distribution, and generate the wrapper (`gradlew` + `gradle/wrapper/`).
After that you can either build from inside Studio (▶ Run) or from the
CLI with `./gradlew assembleDebug`.

**Option B — Install standalone Gradle**

```
scoop install gradle    # or: choco install gradle, or download from gradle.org
cd app-android/android
gradle wrapper --gradle-version 8.7   # one-time; creates ./gradlew
./gradlew assembleDebug
```

## Quick Rust-only test (no Gradle needed)

To re-run just the cargo-ndk step (e.g. after editing Rust code), from
the workspace root:

```bash
ANDROID_NDK_HOME=$LOCALAPPDATA/Android/Sdk/ndk/26.0.10792818 \
  cargo ndk -t arm64-v8a \
  -o app-android/android/app/src/main/jniLibs \
  build -p app-android
```

PowerShell equivalent:

```powershell
$env:ANDROID_NDK_HOME = "$env:LOCALAPPDATA\Android\Sdk\ndk\26.0.10792818"
cargo ndk -t arm64-v8a `
  -o app-android/android/app/src/main/jniLibs `
  build -p app-android
```

The result lives at
`app-android/android/app/src/main/jniLibs/arm64-v8a/libapp_android.so`.
Once Gradle is set up, `./gradlew assembleDebug` will run cargo-ndk
again automatically and bundle the `.so` into the APK.

## Build & install (once Gradle is set up)

```
cd app-android/android
./gradlew installDebug           # build + push to connected device
adb logcat -s RustStdoutStderr   # follow Rust log output
```

Expected behaviour: blank window, `RustStdoutStderr` shows
`android_main: starting` and `window created`. Nothing else — the
player library isn't wired in yet.

## Adding more ABIs

Edit `abiFilters` in `app/build.gradle` to include `armeabi-v7a`,
`x86`, or `x86_64`. The `buildRust*` tasks pass the same value to
`cargo ndk -t`, so add the matching Rust targets:

```
rustup target add armv7-linux-androideabi
rustup target add x86_64-linux-android
```

## What's NOT working yet

The APK launches and `android_main()` runs winit's event loop, but
`Player` is not constructed — the player library still depends on
FFmpeg + D3D11VA/VAAPI which are not available on Android. The decoder
refactor (`HwVideoDecoder` trait + `MediaCodec` impl) is the next step
to make actual playback work.
