#!/usr/bin/env pwsh
# Build the web shell (bridge-web) into www/pkg/ with wasm-pack and,
# optionally, serve www/ for a local smoke test.
#
# Usage: ./build.ps1 [-Profile dev|release] [-Serve] [-Port 8080]
#
# wasm-pack fetches the wasm-bindgen CLI matching the crate's wasm-bindgen
# version on first use. WebCodecs needs `--cfg=web_sys_unstable_apis`, which
# the workspace `.cargo/config.toml` sets for the wasm32 target.

param(
    [ValidateSet('dev', 'release')]
    [string]$Profile = 'dev',
    [switch]$Serve,
    [int]$Port = 8080
)

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $here

if (-not (Get-Command wasm-pack -ErrorAction SilentlyContinue)) {
    throw "wasm-pack not found. Install with: cargo install wasm-pack"
}
rustup target add wasm32-unknown-unknown | Out-Null

$wpArgs = @('build', '--target', 'web', '--out-dir', 'www/pkg', '--out-name', 'rustplayer')
if ($Profile -eq 'dev') { $wpArgs += '--dev' } else { $wpArgs += '--release' }
& wasm-pack @wpArgs
if ($LASTEXITCODE -ne 0) { throw "wasm-pack failed ($LASTEXITCODE)" }

$wasm = Get-Item 'www/pkg/rustplayer_bg.wasm'
Write-Host ("built {0} ({1:N1} MiB)" -f $wasm.FullName, ($wasm.Length / 1MB))

if ($Serve) {
    Write-Host "serving http://localhost:$Port/ (Ctrl+C to stop)"
    Set-Location 'www'
    python -m http.server $Port
}
