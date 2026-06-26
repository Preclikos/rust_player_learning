# Prebuilt distribution — AAR + XCFramework (Phase 1)

**Goal.** Ship the player as **prebuilt binary artifacts** so a consuming app
(BlackZone, and others later) adds it as a normal dependency and **never
compiles Rust / runs cargo-ndk / builds FFmpeg / installs the NDK**. This drops
the slow, fragile cross-compile (3 Android ABIs, iOS slices, FFmpeg, libclang)
out of every consumer's build — it happens once, in our CI.

**Scope (Phase 1).** Package what already exists with the **hand-written**
bindings (the device-verified `app_shared::bridge` + Kotlin/Swift wrappers). No
UniFFI — that's Phase 2, deferred until a third consumer/platform justifies it.
The Rust crates didn't need restructuring: `app-android` (cdylib) and `app-ios`
(staticlib) are already clean shippable outputs; the `.so`/`.a` name is internal.

---

## Android — `:rustplayer` AAR  ✅ DONE + verified

A `com.android.library` module (`app-android/android/rustplayer/`, namespace
`cz.preclikos.rustplayer`) bundles the cdylib (`.so` per ABI) + `libc++_shared`
+ the Kotlin API (`NativeBridge` / `PlayerBridge` / `RustPlayer`, moved out of
`:app`). The `buildRust*` + `copyLibCxxShared` tasks moved here so the `.so`
ship **inside** the AAR. `:app` is now just a smoke-test host
(`implementation(project(":rustplayer"))`, only `MainActivity`).

Publish → GitHub Packages: `cz.preclikos:rustplayer:<version>`
(`maven-publish`, creds from `GITHUB_ACTOR`/`GITHUB_TOKEN` in CI or
`gpr.user`/`gpr.key` locally; version via `-Prustplayer.version=`).

**Verified (Windows + local gradle):** `:rustplayer:assembleRelease` bundles
`classes.jar` + `jni/<abi>/libapp_android.so` (stripped: arm64 29 MB, v7a 16 MB,
x86_64 30 MB) + `libc++_shared`; `:app:assembleDebug` consumes it;
`publishReleasePublicationToMavenLocal` emits aar + sources + pom + module
metadata under `~/.m2/cz/preclikos/rustplayer/0.1.0`. Only the GitHub Packages
push needs real creds (CI).

**Consumer (BlackZone) migration:**
```kotlin
// settings.gradle.kts — add the GitHub Packages repo
dependencyResolutionManagement { repositories {
    maven {
        url = uri("https://maven.pkg.github.com/Preclikos/rust_player_learning")
        credentials { username = providers.gradleProperty("gpr.user").get()
                      password = providers.gradleProperty("gpr.key").get() }
    }
}}
// app build.gradle.kts
implementation("cz.preclikos:rustplayer:0.1.0")
```
Then delete BlackZone's `rust-bridge` cargo-ndk build + the git-rev `player`
pin; use `cz.preclikos.rustplayer.RustPlayer` (+ a `PlayerBridge` whose
`resolveKey` does the `/api/licence` POST). See
[UNIFIED_BRIDGE_FOR_PRODUCT_APPS.md](UNIFIED_BRIDGE_FOR_PRODUCT_APPS.md) for the
host/provider wiring.

## iOS — `RustPlayer` SwiftPM package + `RustPlayerFFI.xcframework`  ⚠️ WRITTEN, Mac-verify pending

`app-ios/packaging/`: a Swift wrapper (`Sources/RustPlayer/RustPlayer.swift` —
control API + `RustPlayerDelegate` events + `RustPlayerProvider` async hooks,
mirroring the Kotlin `RustPlayer`), the C header + modulemap
(`include/`), `Package.swift` (binaryTarget + framework/lib `linkerSettings`),
and `scripts/build_xcframework.sh`. The script cross-builds FFmpeg + the
`app-ios` staticlib for device + both sim arches, **merges** bridge + FFmpeg
`.a` into one lib per slice (`libtool -static`; an xcframework slice holds one
library), lipos the sim arches, and runs `xcodebuild -create-xcframework`.

**Not yet built on a Mac** (no macOS SDK on the Windows dev box). First
`build_xcframework.sh` run on macOS + Xcode validates the FFmpeg merge and the
`Package.swift` link set (frameworks/libs may need a tweak on first link). The
ObjC `main.m` smoke test still exercises the raw FFI directly; the Swift wrapper
is the consumer-facing API (dogfooded by BlackZone's migration).

**Consumer (BlackZone):** add the SwiftPM package; for a release point the
`binaryTarget` at the GitHub Release zip + checksum (CI prints it). Then
`import RustPlayer`, implement `RustPlayerProvider` (`resolveKey` = licence
POST), drive `RustPlayer`.

## CI — `.github/workflows/`  ⚠️ WRITTEN, unrun

- `publish-android.yml` — tag `android-v*` (or manual): Rust + NDK + cargo-ndk,
  `:rustplayer:publish` (release, all ABIs) → GitHub Packages.
- `publish-ios.yml` — tag `ios-v*`, `macos-14`: `build_xcframework.sh` → attach
  zip + checksum to the GitHub Release.

Both are best-effort; first runs will need tuning (Android FFmpeg vendored
build apt deps; iOS link set).

---

## Verification status
| Piece | Status |
|---|---|
| Android AAR build + consume + publishToMavenLocal | ✅ verified locally |
| Android publish to GitHub Packages | ⏳ needs CI run (creds) |
| iOS Swift wrapper / Package.swift / xcframework script | ⚠️ written, Mac build pending |
| CI workflows (both) | ⚠️ written, unrun |
| Gap 1c (product-safe default video) | ✅ committed `7a12dd4`, device-retested |

## Phase 2 (deferred)
UniFFI to auto-generate Kotlin + Swift bindings from one Rust interface — only
when a third consumer/platform makes the hand-written surface costly. The
surface-handle passing (`ANativeWindow`/`CAMetalLayer`) + tight event timing are
why hand-written wins for now.
