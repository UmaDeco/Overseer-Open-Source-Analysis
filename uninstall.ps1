# Unified Umamusume bot — uninstaller. Restores the game's original cri_mana_vpx.dll.
#
#   powershell -ExecutionPolicy Bypass -File uninstall.ps1
#   Optional:  -GameDir "D:\SteamLibrary\steamapps\common\UmamusumePrettyDerby"

param(
    # Found through Steam when omitted (see Find-GameDir); pass it if the search misses.
    [string]$GameDir
)
$ErrorActionPreference = "Stop"

# Locate the game without assuming a drive or a library folder. Steam records its own install path in
# the registry, and every extra library the user added is listed in steamapps\libraryfolders.vdf.
# (install.ps1 / deploy.ps1 carry their own copies — each script must run standalone.)
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

if (-not $GameDir) {
    $GameDir = Find-GameDir
    if (-not $GameDir) {
        throw "Could not find UmamusumePrettyDerby in any Steam library.`nPass it explicitly:  .\uninstall.ps1 -GameDir ""D:\SteamLibrary\steamapps\common\UmamusumePrettyDerby"""
    }
}

$plugins = Join-Path $GameDir "UmamusumePrettyDerby_Data\Plugins\x86_64"
$real    = Join-Path $plugins "cri_mana_vpx.dll"
$backup  = Join-Path $plugins "cri_mana_vpx_orig.dll"

if (-not (Test-Path $plugins)) { throw "Plugins folder not found. Is -GameDir correct? ($plugins)" }

$proc = Get-Process -Name "UmamusumePrettyDerby" -ErrorAction SilentlyContinue
if ($proc) { throw "The game is running. Fully close it before uninstalling." }

if (Test-Path $backup) {
    Copy-Item $backup $real -Force          # restore the real DLL over our proxy
    Remove-Item $backup -Force
    Write-Host "Restored the original cri_mana_vpx.dll and removed the proxy." -ForegroundColor Green
} else {
    Write-Warning "No backup (cri_mana_vpx_orig.dll) found. Nothing to restore — the bot may not have been installed here."
}
