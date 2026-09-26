<#
.SYNOPSIS
  Tag a release of the player on every platform, the way the shared version
  line requires, with the checks a human keeps forgetting.

.DESCRIPTION
  One version line covers android/ios/web: the next release is max(all
  *-v* tags on the REMOTE) + 1, never a remembered number. The script:
    1. refuses a dirty tree or a HEAD that is not on origin/master;
    2. reads the remote tags and computes the next version (or takes -Version);
    3. requires a green conformance run covering HEAD (gh CLI), unless
       -SkipConformanceCheck;
    4. tags android-vX / ios-vX / web-vX at HEAD and pushes the tags;
    5. with -WaitAndBumpIos, waits for the iOS publish run, reads the
       xcframework checksum from the GitHub release and commits the
       Package.swift bump - the step that was missed on ios-v0.1.9, 0.1.12,
       0.1.25 and 0.1.29.

.EXAMPLE
  ./scripts/release.ps1 -WaitAndBumpIos
  ./scripts/release.ps1 -Version 0.2.0 -Platforms android,web
#>
param(
    [string]$Version = "",
    [string[]]$Platforms = @("android", "ios", "web"),
    [switch]$SkipConformanceCheck,
    [switch]$WaitAndBumpIos,
    [string]$Repo = "Preclikos/rust_player_learning"
)
$ErrorActionPreference = 'Stop'
$env:MSYS_NO_PATHCONV = '1'
$remote = "https://github.com/$Repo.git"
$helper = @('-c', "credential.helper=!gh auth git-credential")

function Invoke-Git { param([string[]]$GitArgs) & git @helper @GitArgs; if ($LASTEXITCODE -ne 0) { throw "git $($GitArgs -join ' ') failed" } }

Push-Location (Split-Path -Parent $PSScriptRoot)
try {
    # 1. clean tree, HEAD pushed
    if (git status --porcelain) { throw "working tree is dirty - commit or stash first" }
    Invoke-Git @('fetch', $remote, 'master')
    $head = (git rev-parse HEAD).Trim()
    $remoteHead = (git rev-parse FETCH_HEAD).Trim()
    git merge-base --is-ancestor $head $remoteHead 2>$null
    if ($LASTEXITCODE -ne 0) {
        throw "HEAD $($head.Substring(0,7)) is not on origin/master - push first"
    }

    # 2. next version from the REMOTE tags
    $tags = (git @helper ls-remote --tags $remote) -split "`n" |
        Where-Object { $_ -match 'refs/tags/(android|ios|web)-v(\d+\.\d+\.\d+)$' } |
        ForEach-Object { [version]$Matches[2] } | Sort-Object -Unique
    $max = if ($tags) { $tags[-1] } else { [version]"0.0.0" }
    if (-not $Version) {
        $Version = "{0}.{1}.{2}" -f $max.Major, $max.Minor, ($max.Build + 1)
    } elseif ([version]$Version -le $max) {
        throw "version $Version is not above the newest remote tag $max"
    }
    Write-Host "newest remote tag: $max -> releasing $Version at $($head.Substring(0,7)) on: $($Platforms -join ', ')"

    # 3. conformance coverage of HEAD
    if (-not $SkipConformanceCheck) {
        $runs = gh run list --repo $Repo --workflow conformance --branch master --limit 100 --json headSha,status,conclusion,databaseId | ConvertFrom-Json
        $exact = $runs | Where-Object { $_.headSha -eq $head } | Select-Object -First 1
        if ($exact) {
            if ($exact.status -ne 'completed' -or $exact.conclusion -ne 'success') {
                throw "conformance run $($exact.databaseId) for HEAD is $($exact.status)/$($exact.conclusion) - wait or fix, or -SkipConformanceCheck"
            }
            Write-Host "conformance run $($exact.databaseId) for HEAD: success"
        } else {
            $ok = $false
            foreach ($r in $runs) {
                if ($r.status -ne 'completed' -or $r.conclusion -ne 'success') { continue }
                git merge-base --is-ancestor $r.headSha $head 2>$null
                if ($LASTEXITCODE -ne 0) { continue }
                $changed = @(git diff --name-only $r.headSha $head -- player).Count
                if ($changed -eq 0) { Write-Host "conformance run $($r.databaseId) on ancestor $($r.headSha.Substring(0,7)) covers HEAD (no player/** change since)"; $ok = $true }
                else { throw "newest green conformance run is on $($r.headSha.Substring(0,7)) but player/** changed since ($changed file(s)) and HEAD has no run - wait for it or -SkipConformanceCheck" }
                break
            }
            if (-not $ok) { throw "no green conformance run covers HEAD" }
        }
    }

    # 4. tag + push
    $tagNames = $Platforms | ForEach-Object { "$_-v$Version" }
    foreach ($t in $tagNames) { git tag $t $head; if ($LASTEXITCODE -ne 0) { throw "tag $t failed" } }
    Invoke-Git (@('push', $remote) + $tagNames)
    Start-Sleep -Seconds 20
    gh run list --repo $Repo --limit 6 --json databaseId,name,status,headBranch --jq '.[] | "\(.databaseId) \(.name) \(.status) \(.headBranch)"'

    # 5. iOS: wait for the xcframework and pin it in Package.swift
    if ($WaitAndBumpIos -and ($Platforms -contains 'ios')) {
        $tag = "ios-v$Version"
        Write-Host "waiting for the $tag publish run (typically 25-60 min)..."
        $runId = $null
        for ($i = 0; $i -lt 90 -and -not $runId; $i++) {
            $r = gh run list --repo $Repo --workflow publish-ios.yml --limit 10 --json databaseId,headBranch,status | ConvertFrom-Json | Where-Object { $_.headBranch -eq $tag } | Select-Object -First 1
            if ($r) { $runId = $r.databaseId } else { Start-Sleep -Seconds 10 }
        }
        if (-not $runId) { throw "no publish-ios run appeared for $tag" }
        gh run watch $runId --repo $Repo --exit-status | Out-Null
        $body = gh release view $tag --repo $Repo --json body --jq .body
        if ($body -notmatch 'checksum:\s*([0-9a-f]{64})') { throw "no checksum in the $tag release body" }
        $checksum = $Matches[1]
        $pkg = 'platform/ios/packaging/Package.swift'
        $content = Get-Content $pkg -Raw
        $content = $content -replace 'releases/download/ios-v[0-9.]+/RustPlayerFFI\.xcframework\.zip', "releases/download/$tag/RustPlayerFFI.xcframework.zip"
        $content = $content -replace 'checksum: "[0-9a-f]{64}"', "checksum: `"$checksum`""
        [IO.File]::WriteAllText((Resolve-Path $pkg), $content)
        git add $pkg
        git commit -q -m "chore(ios): point Package.swift at $tag xcframework"
        Invoke-Git @('push', $remote, 'HEAD:master')
        Write-Host "Package.swift pinned to $tag ($($checksum.Substring(0,8))...) and pushed"
    }
} finally {
    Pop-Location
}
