import org.gradle.api.tasks.Exec

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
}

android {
    namespace = "cz.preclikos.rust_player"
    compileSdk = libs.versions.compileSdk.get().toInt()
    ndkVersion = libs.versions.ndk.get()

    defaultConfig {
        applicationId = "cz.preclikos.rust_player"
        minSdk = libs.versions.minSdk.get().toInt()
        targetSdk = libs.versions.targetSdk.get().toInt()
        versionCode = 1
        versionName = "0.1"

        ndk {
            // arm64-v8a   — modern phones (95% of devices since 2017)
            // armeabi-v7a — older/budget 32-bit ARM phones
            // x86_64      — Android emulator on Intel/AMD PCs
            abiFilters += listOf("arm64-v8a", "armeabi-v7a", "x86_64")
        }
    }

    buildTypes {
        debug {
            isJniDebuggable = true
        }
        release {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    // The jniLibs directory is populated by the buildRust* tasks below.
    sourceSets {
        getByName("main") {
            jniLibs.srcDirs("src/main/jniLibs")
        }
    }
}

// ----------------------------------------------------------------------------
// Rust integration
//
// Runs cargo-ndk to cross-compile the `app-android` crate's cdylib into
// src/main/jniLibs/<abi>/libapp_android.so before Gradle assembles the APK.
//
// Prerequisites:
//   - Android NDK installed and ANDROID_NDK_HOME (or ndkVersion above) set.
//   - cargo-ndk:           cargo install cargo-ndk
//   - aarch64 rust target: rustup target add aarch64-linux-android
// ----------------------------------------------------------------------------

val workspaceDir = file("${rootDir}/../..")
val abis = listOf("arm64-v8a", "armeabi-v7a", "x86_64")

// Mapping from Android ABI name → NDK sysroot subdir for that ABI's libs.
val ndkLibSubdirForAbi = mapOf(
    "arm64-v8a"   to "aarch64-linux-android",
    "armeabi-v7a" to "arm-linux-androideabi",
    "x86_64"      to "x86_64-linux-android",
    "x86"         to "i686-linux-android",
)

// cargo-ndk needs to know where the NDK is. Prefer the one Gradle picked
// (matches the version declared in android.ndkVersion); fall back to
// ANDROID_NDK_HOME or ANDROID_HOME/ndk/<ndkVersion> from the environment.
fun resolveNdkDir(): String? {
    val sdk = android.sdkDirectory
    val fromSdk = file("${sdk}/ndk/${android.ndkVersion}")
    if (fromSdk.exists()) return fromSdk.absolutePath
    return System.getenv("ANDROID_NDK_HOME") ?: System.getenv("NDK_HOME")
}

// Build a `cargo ndk -t <abi1> -t <abi2> …` command. cargo-ndk fans out the
// build across all targets in one invocation and dumps each into the right
// jniLibs/<abi>/ subdir automatically.
fun cargoNdkArgs(release: Boolean): List<String> {
    val args = mutableListOf("cargo", "ndk")
    abis.forEach { args += listOf("-t", it) }
    // --platform 26 matches android.defaultConfig.minSdk. AAudio (used by
    // cpal 0.17) and AHardwareBuffer require API 26+.
    args += listOf("--platform", "26")
    args += listOf("-o", file("${projectDir}/src/main/jniLibs").absolutePath, "build")
    if (release) args += "--release"
    args += listOf("-p", "app-android")
    return args
}

tasks.register<Exec>("buildRustDebug") {
    workingDir = workspaceDir
    resolveNdkDir()?.let { environment("ANDROID_NDK_HOME", it) }
    commandLine = cargoNdkArgs(release = false)
}

tasks.register<Exec>("buildRustRelease") {
    workingDir = workspaceDir
    resolveNdkDir()?.let { environment("ANDROID_NDK_HOME", it) }
    commandLine = cargoNdkArgs(release = true)
}

// Some Rust crates (e.g. `ring`) drag in C++ via the `cc` crate, which adds a
// NEEDED entry for libc++_shared.so to our cdylib. Bundle the matching
// libc++_shared.so for each ABI alongside our .so so the runtime linker finds it.
tasks.register("copyLibCxxShared") {
    doLast {
        val ndkDir = resolveNdkDir()
        if (ndkDir == null) {
            logger.warn("copyLibCxxShared: NDK not found; skipping")
            return@doLast
        }
        val hostCandidates = listOf("windows-x86_64", "linux-x86_64", "darwin-x86_64")
        val host = hostCandidates.firstOrNull {
            file("$ndkDir/toolchains/llvm/prebuilt/$it").exists()
        }
        if (host == null) {
            logger.warn("copyLibCxxShared: no NDK host directory found under $ndkDir")
            return@doLast
        }
        abis.forEach { abi ->
            val libSubdir = ndkLibSubdirForAbi[abi] ?: return@forEach
            val src = file(
                "$ndkDir/toolchains/llvm/prebuilt/$host/sysroot/usr/lib/$libSubdir/libc++_shared.so"
            )
            if (!src.exists()) {
                logger.warn("copyLibCxxShared: $src not found for ABI $abi")
                return@forEach
            }
            copy {
                from(src)
                into(file("${projectDir}/src/main/jniLibs/$abi"))
            }
        }
    }
}

afterEvaluate {
    tasks.named("preDebugBuild").configure {
        dependsOn("buildRustDebug", "copyLibCxxShared")
    }
    tasks.named("preReleaseBuild").configure {
        dependsOn("buildRustRelease", "copyLibCxxShared")
    }
}

tasks.named("clean").configure {
    doFirst {
        delete("${projectDir}/src/main/jniLibs")
    }
}
