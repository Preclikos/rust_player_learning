pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "rust_dash_player_android"
// The demo app lives with the other examples; it builds against the local
// :rustplayer module so the AAR and the smoke test share one source.
include(":app")
project(":app").projectDir = file("../../../examples/android")
include(":rustplayer")
