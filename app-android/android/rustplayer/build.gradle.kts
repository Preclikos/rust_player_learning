import org.gradle.api.tasks.Exec

// Publishable Android library: the Rust player (`.so` per ABI) + the idiomatic
// Kotlin API (NativeBridge / PlayerBridge / RustPlayer). Consumers add this as
// a normal Gradle dependency (an AAR from GitHub Packages) and never compile
// Rust / run cargo-ndk / install the NDK themselves.
//
//   implementation("cz.preclikos:rustplayer:<version>")
//
// The `:app` module here is just a smoke-test host that consumes this library.

plugins {
    alias(libs.plugins.android.library)
    alias(libs.plugins.kotlin.android)
    `maven-publish`
}

android {
    namespace = "cz.preclikos.rustplayer"
    compileSdk = libs.versions.compileSdk.get().toInt()
    ndkVersion = libs.versions.ndk.get()

    defaultConfig {
        minSdk = libs.versions.minSdk.get().toInt()
        ndk {
            abiFilters += listOf("arm64-v8a", "armeabi-v7a", "x86_64")
        }
    }

    buildTypes {
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

    // jniLibs are populated by the buildRust* tasks below (release for the
    // published artifact; debug for local `:app` runs).
    sourceSets {
        getByName("main") {
            jniLibs.srcDirs("src/main/jniLibs")
        }
    }

    // Expose the `release` variant as the publishable software component.
    publishing {
        singleVariant("release") {
            withSourcesJar()
        }
    }
}

// ----------------------------------------------------------------------------
// Rust integration (moved here from :app so the .so ships inside the AAR).
//
// Cross-compiles the `app-android` crate's cdylib into this module's
// src/main/jniLibs/<abi>/libapp_android.so before Gradle assembles the library.
// In CI the publish job runs the *release* build for all ABIs; local `:app`
// debug runs trigger the debug build.
// ----------------------------------------------------------------------------

val workspaceDir = file("${rootDir}/../..")
val abis = listOf("arm64-v8a", "armeabi-v7a", "x86_64")

val ndkLibSubdirForAbi = mapOf(
    "arm64-v8a" to "aarch64-linux-android",
    "armeabi-v7a" to "arm-linux-androideabi",
    "x86_64" to "x86_64-linux-android",
    "x86" to "i686-linux-android",
)

fun resolveNdkDir(): String? {
    val sdk = android.sdkDirectory
    val fromSdk = file("${sdk}/ndk/${android.ndkVersion}")
    if (fromSdk.exists()) return fromSdk.absolutePath
    return System.getenv("ANDROID_NDK_HOME") ?: System.getenv("NDK_HOME")
}

fun cargoNdkArgs(release: Boolean): List<String> {
    val args = mutableListOf("cargo", "ndk")
    abis.forEach { args += listOf("-t", it) }
    // --platform 26 matches minSdk. AAudio (cpal) + AHardwareBuffer need API 26+.
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

// ring/rustls drag in libc++_shared via the cc crate (NEEDED entry); bundle the
// matching libc++_shared.so for each ABI alongside our .so.
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

// ----------------------------------------------------------------------------
// Publishing — GitHub Packages Maven (https://maven.pkg.github.com/...).
// CI provides GITHUB_ACTOR / GITHUB_TOKEN; locally set gpr.user / gpr.key in
// ~/.gradle/gradle.properties (a PAT with read:packages / write:packages).
//   ./gradlew :rustplayer:publish          # → GitHub Packages
//   ./gradlew :rustplayer:publishToMavenLocal
// ----------------------------------------------------------------------------

publishing {
    publications {
        register<MavenPublication>("release") {
            groupId = "cz.preclikos"
            artifactId = "rustplayer"
            version = (project.findProperty("rustplayer.version") as String?) ?: "0.1.0"
            afterEvaluate { from(components["release"]) }
        }
    }
    repositories {
        maven {
            name = "GitHubPackages"
            url = uri("https://maven.pkg.github.com/Preclikos/rust_player_learning")
            credentials {
                username = (project.findProperty("gpr.user") as String?)
                    ?: System.getenv("GITHUB_ACTOR")
                password = (project.findProperty("gpr.key") as String?)
                    ?: System.getenv("GITHUB_TOKEN")
            }
        }
    }
}
