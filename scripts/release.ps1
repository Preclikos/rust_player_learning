# Windows convenience wrapper: the release procedure lives in release.sh
# (bash, so the same file runs in Git Bash, on macOS and on Linux).
#   ./scripts/release.ps1 --wait-and-bump-ios
#   ./scripts/release.ps1 --version 0.2.0 --platforms android,web
$bash = Get-Command bash -ErrorAction SilentlyContinue
if (-not $bash) { $bash = Get-Command 'C:\Program Files\Git\bin\bash.exe' -ErrorAction SilentlyContinue }
if (-not $bash) { throw "bash not found (install Git for Windows)" }
& $bash.Source (Join-Path $PSScriptRoot 'release.sh') @args
exit $LASTEXITCODE
