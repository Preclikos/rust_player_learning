#!/usr/bin/env pwsh
# Build → install → launch → stream logcat for video timing diagnosis.
# Usage:  .\test_android.ps1 [-Release]
param([switch]$Release)

$ErrorActionPreference = 'Stop'
$adb  = "$env:LOCALAPPDATA\Android\Sdk\platform-tools\adb.exe"
$pkg  = 'cz.preclikos.rust_player'
$root = $PSScriptRoot
$android = Join-Path $root 'platform\android\android'

# ── 1. Rust build ────────────────────────────────────────────────────────────
Write-Host "=== Building Rust ($(if ($Release) {'release'} else {'debug'})) ===" -ForegroundColor Cyan
$rustArgs = @('ndk', '-t', 'arm64-v8a',
              '-o', (Join-Path $root 'platform\android\android\rustplayer\src\main\jniLibs'),
              'build', '-p', 'bridge-android')
if ($Release) { $rustArgs += '--release' }
Push-Location $root
cargo @rustArgs
if ($LASTEXITCODE -ne 0) { throw "Rust build failed" }
Pop-Location

# ── 2. Gradle assemble + install ─────────────────────────────────────────────
Write-Host "=== Gradle assembleDebug + install ===" -ForegroundColor Cyan
$env:JAVA_HOME = 'C:\Java\jdk-22.0.2'
Push-Location $android
$gradle = Join-Path $android 'gradlew.bat'
& $gradle assembleDebug
if ($LASTEXITCODE -ne 0) { throw "Gradle build failed" }
Pop-Location

$apk = Get-ChildItem (Join-Path $android 'app\build\outputs\apk\debug\*.apk') |
       Sort-Object LastWriteTime -Descending | Select-Object -First 1
Write-Host "Installing $($apk.Name)..."
& $adb install -r $apk.FullName
if ($LASTEXITCODE -ne 0) { throw "adb install failed" }

# ── 3. Launch ────────────────────────────────────────────────────────────────
Write-Host "=== Launching app ===" -ForegroundColor Cyan
& $adb shell am force-stop $pkg
Start-Sleep -Milliseconds 500
& $adb shell am start -n "$pkg/cz.preclikos.rustplayer.MainActivity"

# ── 4. Stream relevant logcat lines ──────────────────────────────────────────
# Log tag structure (via android_logger, using the Rust lib name as tag):
#   rustplayer           — platform/android lib.rs (nativeStart, track selected)
#   player::decoders::med.. — mediacodec.rs (configured, stall, MAX_IMAGES)
#   player::player (truncated) — player.rs (vsync LATE, sync producer warnings)
#   RustStdoutStderr     — println!/eprintln! (segment consuming/producing)
Write-Host "=== Logcat (Ctrl+C to stop) ===" -ForegroundColor Cyan
Write-Host "    Tags: rustplayer | player:: | RustStdoutStderr | W/E level"
& $adb logcat -c
Start-Sleep -Milliseconds 800

& $adb logcat | Select-String -Pattern `
    'rustplayer|player::|RustStdoutStderr|stall|LATE|MAX_IMAGES|acquire_next|W player|E player' `
    -SimpleMatch:$false
