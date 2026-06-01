# Android build

Gradle wrapper around the `app-android` Rust cdylib. Produces an APK whose
`MainActivity` owns a `SurfaceView` and hands its `Surface` to the embedded
Rust player over JNI (`nativeStart` / `nativeSetSize` / `nativeDestroy`). This
is the **embed** model real apps use — the player renders straight into a
host-owned Surface, not a winit `NativeActivity`.

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

Expected behaviour: the `SurfaceView` shows the decrypted test stream
(MediaCodec HEVC → GLES). `logcat` shows `app-android: embedded player
loaded`, `nativeStart: WxH`, then the playback pipeline logs.

## Adding more ABIs

Edit `abiFilters` in `app/build.gradle` to include `armeabi-v7a`,
`x86`, or `x86_64`. The `buildRust*` tasks pass the same value to
`cargo ndk -t`, so add the matching Rust targets:

```
rustup target add armv7-linux-androideabi
rustup target add x86_64-linux-android
```

## Architecture note

`MainActivity` (Kotlin) is a thin host: it creates a `SurfaceView`, and on
`surfaceChanged` calls `nativeStart(context, surface, w, h)` (or
`nativeSetSize` on later layout changes), then `nativeDestroy` on
`surfaceDestroyed`. The JNI externs live in a companion object with
`@JvmStatic` so the symbol names land on the outer class — matching what
the Rust `extern "system" fn Java_..._nativeStart` declarations expect.
The Rust side (`app-android/src/lib.rs`) seeds `ndk_context` with
(JavaVM, Context), turns the `Surface` into an `ANativeWindow`, builds
`Player::new_from_android_surface`, and plays the bundled encrypted test
stream. There is no winit on this path.

## Project layout

Modern Android project — Kotlin DSL Gradle files, version catalog under
`gradle/libs.versions.toml`, Kotlin sources under `app/src/main/kotlin/`:

```
android/
├── settings.gradle.kts
├── build.gradle.kts                (root)
├── gradle.properties
├── gradle/
│   └── libs.versions.toml          (AGP / Kotlin / SDK versions)
└── app/
    ├── build.gradle.kts            (app module + cargo-ndk tasks)
    └── src/main/
        ├── AndroidManifest.xml
        ├── kotlin/cz/preclikos/rustplayer/MainActivity.kt
        ├── res/values/strings.xml
        └── jniLibs/<abi>/libapp_android.so   (produced by cargo-ndk)
```
