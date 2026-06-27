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
        // Released binary (ios-v0.1.0). Local dev: run build_xcframework.sh and
        // swap to `.binaryTarget(name: "RustPlayerFFI", path: "RustPlayerFFI.xcframework")`.
        .binaryTarget(
            name: "RustPlayerFFI",
            url: "https://github.com/Preclikos/rust_player_learning/releases/download/ios-v0.1.0/RustPlayerFFI.xcframework.zip",
            checksum: "a1a5ffcb0986441f6b3b120c271ed96e5272ddd49bb203c7c6058a09581aaeec"
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
