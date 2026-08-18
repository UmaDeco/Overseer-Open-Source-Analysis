# Deploy the built DLL as the cri_mana_vpx proxy, restart the game, and PROVE the new build is live.
#
# Exists because two mistakes cost a lot of debugging time and both look like "my code is broken":
#   * Copying to the game ROOT instead of Plugins\x86_64. Unity loads plugins from Plugins\x86_64, but
#     a root copy still gets mapped (exe dir wins by-name loads) -> BOTH builds load -> two mods detour
#     the same il2cpp addresses -> instant 0xC0000005 at boot, with nothing in overseer-crash.log.
#   * Killing "umamusume" (no such process; it's UmamusumePrettyDerby), leaving the old instance alive
#     holding :1620, so Steam ignores the relaunch and the API answers with the STALE build's JSON.
param(
    [switch]$NoLaunch,
    # Found through Steam when omitted (see Find-GameDir); pass it if the search misses.
    [string]$GameDir,
    # Sibling checkout supplying the advisor's career_bot + data.
    [string]$IcarusSrc = (Join-Path (Split-Path $PSScriptRoot -Parent) "Umamusume-Icarus")
)

$ErrorActionPreference = 'Stop'

# Locate the game without assuming a drive or a library folder. Steam records its own install path in
# the registry, and every extra library the user added is listed in steamapps\libraryfolders.vdf.
# (install.ps1 carries its own copy of this on purpose — both scripts must run standalone.)
function Find-GameDir {
    $roots = @()
    foreach ($k in 'HKCU:\Software\Valve\Steam', 'HKLM:\SOFTWARE\WOW6432Node\Valve\Steam', 'HKLM:\SOFTWARE\Valve\Steam') {
        try {
            $p = Get-ItemProperty -Path $k -ErrorAction Stop
            foreach ($v in @($p.SteamPath, $p.InstallPath)) { if ($v) { $roots += $v } }
        } catch {}
    }
    # ${...} is required: bare $env:ProgramFiles(x86) parses as $env:ProgramFiles + literal "(x86)".
    $roots += "${env:ProgramFiles(x86)}\Steam"
    $roots += "$env:ProgramFiles\Steam"

    $libraries = @()
    foreach ($r in ($roots | Select-Object -Unique)) {
        if (-not $r) { continue }
        $libraries += $r
        $vdf = Join-Path $r "steamapps\libraryfolders.vdf"
        if (Test-Path $vdf) {
            foreach ($m in [regex]::Matches((Get-Content $vdf -Raw), '"path"\s*"([^"]+)"')) {
                $libraries += $m.Groups[1].Value -replace '\\\\', '\'
            }
        }
    }
    foreach ($lib in ($libraries | Select-Object -Unique)) {
        $candidate = Join-Path $lib "steamapps\common\UmamusumePrettyDerby"
        if (Test-Path (Join-Path $candidate "UmamusumePrettyDerby_Data\Plugins\x86_64")) { return $candidate }
    }
    return $null
}

$src = Join-Path $PSScriptRoot "native\target\release\overseer_overlay.dll"
if (-not $GameDir) {
    $GameDir = Find-GameDir
    if (-not $GameDir) { throw "could not find UmamusumePrettyDerby in any Steam library - pass -GameDir" }
}
$game = $GameDir
$dst  = "$game\UmamusumePrettyDerby_Data\Plugins\x86_64\cri_mana_vpx.dll"

if (-not (Test-Path $src)) { throw "no build at $src - run build_nllb.bat first" }

# A stray root copy double-loads the mod and crashes the game. Never leave one behind.
$stray = "$game\cri_mana_vpx.dll"
if (Test-Path $stray) { Remove-Item $stray -Force; Write-Host "removed stray root cri_mana_vpx.dll (would double-load)" }

Get-Process UmamusumePrettyDerby -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2
if (Get-Process UmamusumePrettyDerby -ErrorAction SilentlyContinue) { throw "game still running - cannot replace a mapped DLL" }

Copy-Item $src $dst -Force
$a = Get-Item $src; $b = Get-Item $dst
if ($a.Length -ne $b.Length) { throw "deploy verify FAILED: size $($a.Length) != $($b.Length)" }
Write-Host "deployed -> Plugins\x86_64\cri_mana_vpx.dll  ($($b.Length) bytes, $($b.LastWriteTime))"

# Sync the advisor sidecar bundle too - the DLL once outran it (a stale sidecar exited code 0
# silently). career_bot + data come from the Icarus dev build, uma_sidecar from this repo. pyembed is
# huge and already installed by install.ps1, so it is NOT synced here; *.py/*.json skip __pycache__
# and the data\presets\ user JSONs.
$icarus = $IcarusSrc
$advDst = "$game\UmamusumePrettyDerby_Data\Plugins\x86_64\advisor"
if ((Test-Path $icarus) -and (Test-Path $advDst)) {
    Copy-Item "$icarus\career_bot\*.py" "$advDst\career_bot" -Force
    Copy-Item "$icarus\career_bot\scenarios\*.py" "$advDst\career_bot\scenarios" -Force
    Copy-Item "$icarus\data\*.json" "$advDst\data" -Force
    Copy-Item (Join-Path $PSScriptRoot "advisor\uma_sidecar\*.py") "$advDst\uma_sidecar" -Force
    Write-Host "synced advisor bundle (career_bot + data + uma_sidecar) -> $advDst"
}

if ($NoLaunch) { return }
Start-Process "steam://rungameid/3224770"   # MUST launch via Steam: a direct exe launch hangs before the DLL loads
Write-Host "launched via Steam; waiting for the in-process web server..."
for ($i = 0; $i -lt 90; $i++) {
    Start-Sleep -Seconds 2
    try { Invoke-RestMethod "http://127.0.0.1:1620/api/mod/performance" -TimeoutSec 2 | Out-Null; Write-Host "web UI up: http://127.0.0.1:1620"; return } catch {}
}
Write-Host "WARNING: web server never came up - check overseer-logs\overseer-native.log"
