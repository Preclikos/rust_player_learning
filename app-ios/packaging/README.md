# RustPlayer — SwiftPM package

Prebuilt Rust video player for iOS as a Swift package. Consumers `import
RustPlayer` and never compile Rust / build FFmpeg.

> **Verification status:** written, **not yet built on a Mac**. The Swift
> wrapper, headers, `Package.swift`, and `build_xcframework.sh` need a first
> `scripts/build_xcframework.sh` run on macOS + Xcode to validate the link set
> (frameworks/libs in `Package.swift`) and the FFmpeg merge. See the Phase-1
> distribution handoff.

## Build the binary (macOS, once per release)

```sh
app-ios/packaging/scripts/build_xcframework.sh
```

Produces `RustPlayerFFI.xcframework` (device + simulator slices: Rust player +
FFmpeg merged into one static lib per slice, plus the C headers) and a zipped
copy with its `swift package compute-checksum`.

## Consume

Local (this repo checked out): the `path:` binary target in `Package.swift`
points at the built xcframework. Published: swap to the `url:`+`checksum:`
target (CI fills the checksum; the zip is attached to a GitHub Release).

```swift
import RustPlayer

final class Provider: RustPlayerProvider {
    func resolveKey(kid: Data) async throws -> Data { /* licence server */ }
    // intercept defaults to passthrough; override for auth/URL rewrite.
}

let player = RustPlayer()
player.delegate = self          // RustPlayerDelegate — events on main
player.start(layer: metalLayer, manifestURL: url, provider: Provider())
player.togglePlayPause()
player.seek(toMs: 30_000)
```

The API mirrors the Android `RustPlayer` (Kotlin) and the unified
`app_shared::bridge` event/JSON contract.
