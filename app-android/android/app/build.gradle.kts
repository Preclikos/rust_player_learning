plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
}

android {
    namespace = "cz.preclikos.rust_player"
    compileSdk = libs.versions.compileSdk.get().toInt()

    defaultConfig {
        applicationId = "cz.preclikos.rust_player"
        minSdk = libs.versions.minSdk.get().toInt()
        targetSdk = libs.versions.targetSdk.get().toInt()
        versionCode = 1
        versionName = "0.1"

        ndk {
            // Which ABIs land in the APK. The actual .so come from the
            // :rustplayer library dependency (this just filters them).
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
}

dependencies {
    // The Rust player + Kotlin API. In a real consumer this is
    //   implementation("cz.preclikos:rustplayer:<version>")  // from GitHub Packages
    // Here it's the local module so the smoke-test app and the published AAR
    // build from one source. The .so + libc++_shared ship inside :rustplayer,
    // and its buildRust* tasks cross-compile the cdylib on demand.
    implementation(project(":rustplayer"))
}
