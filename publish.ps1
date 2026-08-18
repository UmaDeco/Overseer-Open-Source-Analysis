# Package (and optionally publish) an Overseer release for the in-app self-updater.
#
# The updater (native/src/selfupdate.rs) expects each GitHub release to carry two loose assets:
#   cri_mana_vpx.dll        - the built mod DLL (what gets downloaded + swapped in)
#   cri_mana_vpx.dll.hash   - FNV-1a 64-bit hex of that DLL, for same-version HOTFIX detection
# This script builds the DLL, produces both assets in .\dist\, and (with -Publish) creates the
# GitHub release via `gh`. The tag is v<version-from-Cargo.toml>.
#
# The update REPO is compile-time (selfupdate::REPO). Pass -Repo "owner/name" and it's baked into the
# DLL via the OVERSEER_REPO env var the build reads — so a published DLL knows where to check for its
# OWN updates. Without -Repo the updater stays dormant (safe: no repo, no checks, no downloads).
#
# Usage:
#   .\publish.ps1 -Repo "owner/Overseer-Releases"                     # build + stage assets in .\dist
#   .\publish.ps1 -Repo "owner/Overseer-Releases" -Publish -Notes "…" # also create the GitHub release
[CmdletBinding()]
param(
    [string]$Repo = $env:OVERSEER_REPO,   # owner/name; also bakes into the DLL as its update source
    [switch]$Publish,                     # create the GitHub release with `gh` (needs gh auth + repo)
    [string]$Notes = "",                  # release notes (markdown); the updater slims to headers+bullets
    [switch]$SkipBuild                    # package the existing target\release DLL without rebuilding
)
$ErrorActionPreference = 'Stop'
$root  = $PSScriptRoot
$built = Join-Path $root 'native\target\release\overseer_overlay.dll'
$dist  = Join-Path $root 'dist'
$asset = 'cri_mana_vpx.dll'   # must equal selfupdate::DLL_ASSET / DLL_NAME

# Version = single source of truth in native/Cargo.toml.
$verLine = Select-String -Path (Join-Path $root 'native\Cargo.toml') -Pattern '^\s*version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $verLine) { throw "could not read version from native/Cargo.toml" }
$version = $verLine.Matches[0].Groups[1].Value
$tag = "v$version"
Write-Host "Overseer release $tag" -ForegroundColor Cyan
if ($Repo) { Write-Host "  update repo baked into DLL: $Repo" } else { Write-Warning "  no -Repo given: this DLL's updater will be DORMANT (set -Repo to enable one-click updates)" }

# 1. Build (OVERSEER_REPO is read by selfupdate.rs at compile time).
if (-not $SkipBuild) {
    if ($Repo) { $env:OVERSEER_REPO = $Repo }
    Write-Host "building (release + nllb)..." -ForegroundColor Cyan
    & (Join-Path $root 'build_nllb.bat')
    if ($LASTEXITCODE -ne 0) { throw "build failed (exit $LASTEXITCODE)" }
}
if (-not (Test-Path $built)) { throw "built DLL not found at $built" }

# 2. Stage assets in dist\.
New-Item -ItemType Directory -Force -Path $dist | Out-Null
$distDll  = Join-Path $dist $asset
$distHash = Join-Path $dist "$asset.hash"
Copy-Item $built $distDll -Force

# 3. FNV-1a 64-bit hash — MUST match native/src/selfupdate.rs::fnv1a exactly (offset basis
#    0xcbf29ce484222325, prime 0x100000001b3, wrapping mul, lowercase %016x). BigInteger masked to
#    64 bits gives exact wrapping arithmetic without overflow surprises.
$bytes = [System.IO.File]::ReadAllBytes($distDll)
$h     = [bigint]::Parse('14695981039346656037')   # 0xcbf29ce484222325
$prime = [bigint]'1099511628211'                    # 0x100000001b3
$mask  = ([bigint]::Pow(2, 64)) - [bigint]1
foreach ($b in $bytes) {
    $h = $h -bxor ([bigint]$b)
    $h = ($h * $prime) -band $mask
}
$hashHex = ('{0:x16}' -f [uint64]$h)
Set-Content -Path $distHash -Value $hashHex -NoNewline -Encoding ascii

Write-Host "staged:" -ForegroundColor Green
Write-Host ("  {0}  ({1:N0} bytes)" -f $distDll, $bytes.Length)
Write-Host ("  {0}  ({1})" -f $distHash, $hashHex)

# 4. Optionally publish via gh.
if ($Publish) {
    if (-not $Repo) { throw "-Publish requires -Repo (the release must live in the update repo)" }
    $gh = (Get-Command gh -ErrorAction SilentlyContinue)
    if (-not $gh) { throw "gh CLI not found - install GitHub CLI or upload dist\ assets manually" }
    Write-Host "creating GitHub release $tag on $Repo..." -ForegroundColor Cyan
    $notesArg = if ($Notes) { @('--notes', $Notes) } else { @('--generate-notes') }
    & gh release create $tag $distDll $distHash --repo $Repo --title $tag @notesArg
    if ($LASTEXITCODE -ne 0) { throw "gh release create failed (exit $LASTEXITCODE)" }
    Write-Host "published $tag" -ForegroundColor Green
} else {
    Write-Host "`nnot published (no -Publish). To release manually, upload BOTH files in dist\ to a GitHub release tagged $tag." -ForegroundColor Yellow
}
