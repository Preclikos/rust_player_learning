#!/usr/bin/env pwsh
# Cross-compile the app-android cdylib for Android arm64 without going
# through Gradle. Outputs into app-android/android/app/src/main/jniLibs/
# where the APK build can pick it up later.
#
# Usage: ./build_rust.ps1 [release]

param(
    [string]$Profile = "debug"
)

$ErrorActionPreference = 'Stop'

# Locate the NDK. Prefer ANDROID_NDK_HOME, else use the latest installed
# under $LOCALAPPDATA/Android/Sdk/ndk.
if (-not $env:ANDROID_NDK_HOME) {
    $ndkRoot = Join-Path $env:LOCALAPPDATA 'Android\Sdk\ndk'
    if (Test-Path $ndkRoot) {
        $latest = Get-ChildItem $ndkRoot -Directory | Sort-Object Name -Descending | Select-Object -First 1
        if ($latest) { $env:ANDROID_NDK_HOME = $latest.FullName }
    }
}
if (-not $env:ANDROID_NDK_HOME) {
    Write-Error "ANDROID_NDK_HOME not set and no NDK found under `$LOCALAPPDATA/Android/Sdk/ndk"
}
Write-Host "Using NDK: $env:ANDROID_NDK_HOME"

$workspaceRoot = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$jniOut = Resolve-Path (Join-Path $PSScriptRoot 'android/rustplayer/src/main/jniLibs')

$args = @('ndk', '-t', 'arm64-v8a', '-o', $jniOut.Path, 'build', '-p', 'bridge-android')
if ($Profile -eq 'release') { $args += '--release' }

Push-Location $workspaceRoot
try {
    & cargo @args
    if ($LASTEXITCODE -ne 0) { throw "cargo ndk failed with $LASTEXITCODE" }
}
finally { Pop-Location }

$so = Join-Path $jniOut.Path 'arm64-v8a/librustplayer.so'
Write-Host "Built: $so"
Write-Host "Size:  $((Get-Item $so).Length) bytes"
