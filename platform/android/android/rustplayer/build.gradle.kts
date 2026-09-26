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
// Cross-compiles the `bridge-android` crate's cdylib into this module's
// src/main/jniLibs/<abi>/librustplayer.so before Gradle assembles the library.
// In CI the publish job runs the *release* build for all ABIs; local `:app`
// debug runs trigger the debug build.
// ----------------------------------------------------------------------------

// rootDir = platform/android/android (the Gradle project); the cargo workspace
// root is three levels up.
val workspaceDir = file("${rootDir}/../../..")
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
    args += listOf("-p", "bridge-android")
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
    // Line tables (file:line, inlined frames) in the release .so, so a native
    // crash in Crashlytics can be symbolicated. Only this Android build gets
    // them (env, not [profile.release]) — the desktop/iOS/web releases keep
    // their own settings. The debug info never ships: stripRustRelease moves
    // it into build/native-symbols (uploaded to Crashlytics only) and strips
    // the .so in the AAR.
    environment("CARGO_PROFILE_RELEASE_DEBUG", "line-tables-only")
    environment("CARGO_PROFILE_RELEASE_STRIP", "none")
    commandLine = cargoNdkArgs(release = true)
}

// ----------------------------------------------------------------------------
// Native symbols for crash reports.
//
// The release .so is built with line tables and a GNU build-id
// (.cargo/config.toml). This task keeps that unstripped copy under
// build/native-symbols/<abi>/librustplayer.so and strips the copy that goes
// into the AAR with --strip-all: the AAR (and every APK built from it) holds
// no symbol table and no debug info, only the JNI exports. llvm-strip keeps
// the build-id note, so the shipped library and the unstripped one still
// match. The unstripped set is NEVER published (this repo and its packages
// are public): the publish workflow uploads it straight to Crashlytics and
// deletes it (see docs/RELEASING.md, "Native crash symbols").
// ----------------------------------------------------------------------------
val nativeSymbolsDir = layout.buildDirectory.dir("native-symbols")

fun llvmTool(name: String): File? {
    val ndkDir = resolveNdkDir() ?: return null
    val exe = if (System.getProperty("os.name").lowercase().contains("windows")) "$name.exe" else name
    return listOf("windows-x86_64", "linux-x86_64", "darwin-x86_64")
        .map { file("$ndkDir/toolchains/llvm/prebuilt/$it/bin/$exe") }
        .firstOrNull { it.exists() }
}

fun run(vararg cmd: String) {
    val p = ProcessBuilder(*cmd).redirectErrorStream(true).start()
    val out = p.inputStream.bufferedReader().readText()
    if (p.waitFor() != 0) throw GradleException("${cmd.joinToString(" ")} failed: $out")
}

tasks.register("stripRustRelease") {
    dependsOn("buildRustRelease")
    doLast {
        val strip = llvmTool("llvm-strip") ?: throw GradleException("llvm-strip not found in the NDK")
        val readelf = llvmTool("llvm-readelf") ?: throw GradleException("llvm-readelf not found in the NDK")
        val symbolsRoot = nativeSymbolsDir.get().asFile
        delete(symbolsRoot)
        abis.forEach { abi ->
            val so = file("${projectDir}/src/main/jniLibs/$abi/librustplayer.so")
            if (!so.exists()) throw GradleException("stripRustRelease: $so missing")
            // A symbol file without a build-id can never be matched to a crash.
            val p = ProcessBuilder(readelf.absolutePath, "-n", so.absolutePath).start()
            val notes = p.inputStream.bufferedReader().readText()
            p.waitFor()
            if (!notes.contains("Build ID")) {
                throw GradleException("$so has no GNU build-id (check .cargo/config.toml rustflags)")
            }
            val keep = File(symbolsRoot, "$abi/librustplayer.so")
            keep.parentFile.mkdirs()
            so.copyTo(keep, overwrite = true)
            run(strip.absolutePath, "--strip-all", so.absolutePath)
            logger.lifecycle("rustplayer $abi: ${keep.length() / 1_048_576} MiB with symbols -> ${so.length() / 1_048_576} MiB stripped")
        }
    }
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
        dependsOn("buildRustRelease", "stripRustRelease", "copyLibCxxShared")
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
