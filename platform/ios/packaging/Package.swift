// swift-tools-version:5.9
import PackageDescription

// SwiftPM package for the Rust player. A consumer adds this package and
// `import RustPlayer` — no Rust toolchain, no cargo, no FFmpeg build. The
// prebuilt static lib (player + FFmpeg, all slices) lives in the binary
// `RustPlayerFFI.xcframework`.
//
// Local dev: run `scripts/build_xcframework.sh` to produce the xcframework
// next to this file (the `path:` binaryTarget below).
// Published: swap to the remote `url:`+`checksum:` binaryTarget (CI fills the
// checksum via `swift package compute-checksum RustPlayerFFI.xcframework.zip`).
let package = Package(
    name: "RustPlayer",
    platforms: [.iOS(.v15)],
    products: [
        .library(name: "RustPlayer", targets: ["RustPlayer"]),
    ],
    targets: [
        // Released binary (ios-v0.1.1). Local dev: run build_xcframework.sh and
        // swap to `.binaryTarget(name: "RustPlayerFFI", path: "RustPlayerFFI.xcframework")`.
        // NOTE: bump BOTH url + checksum on every release tag — they must match
        // the zip attached to that tag's GitHub Release.
        .binaryTarget(
            name: "RustPlayerFFI",
            url: "https://github.com/Preclikos/rust_player_learning/releases/download/ios-v0.1.1/RustPlayerFFI.xcframework.zip",
            checksum: "1d0121f705a327695b23c3898873377328c365cb0c6004e06241780fdaf6cdf4"
        ),
        .target(
            name: "RustPlayer",
            dependencies: ["RustPlayerFFI"],
            // The Rust staticlib carries no transitive link directives; the
            // frameworks the player + FFmpeg reference must be named here.
            linkerSettings: [
                .linkedFramework("UIKit"),
                .linkedFramework("QuartzCore"),
                .linkedFramework("Metal"),
                .linkedFramework("MetalKit"),
                .linkedFramework("CoreVideo"),
                .linkedFramework("CoreMedia"),
                .linkedFramework("VideoToolbox"),
                .linkedFramework("AudioToolbox"),
                .linkedFramework("CoreAudio"),
                .linkedFramework("Security"),
                .linkedFramework("SystemConfiguration"),
                .linkedFramework("AVFoundation"),
                .linkedLibrary("c++"),
                .linkedLibrary("iconv"),
                .linkedLibrary("z"),
                .linkedLibrary("bz2"),
            ]
        ),
    ]
)
