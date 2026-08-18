# Unified Umamusume bot — plug-and-play installer (Frida-free proxy loader).
#
# Installs the built DLL as a cri_mana_vpx.dll proxy next to the game's real one.
# The game loads it automatically; our proxy forwards the real codec calls and
# starts the bot + web UI (http://127.0.0.1:1620). No Frida, no Python.
#
#   Right-click -> Run with PowerShell,  or:  powershell -ExecutionPolicy Bypass -File install.ps1
#   The game is located automatically through Steam; override only if that fails:
#   Optional:  -GameDir "D:\SteamLibrary\steamapps\common\UmamusumePrettyDerby"
#
# Every path this script uses is derived from its OWN location ($PSScriptRoot) or found at runtime.
# Nothing is hardcoded to a particular machine — run it from a fresh clone, on any drive, any user.

param(
    # Where the game is. Left empty, it is FOUND: Steam's registry entry -> every library folder in
    # libraryfolders.vdf. Only pass this if the search misses (or the game isn't on Steam).
    [string]$GameDir,
    # Every source below is OPTIONAL and defaults to a path next to this script, so a fresh clone
    # installs without editing anything. A missing one is reported and skipped, never fatal.
    # Overseer's per-language glossary data (glossary.json + glossary_ui.json per lang). Copied next
    # to the DLL so the web UI language picker + Localize::Get glossary fallback work.
    [string]$GlossarySrc = (Join-Path (Split-Path $PSScriptRoot -Parent) "Overseer-old-prototype\overseer\translations"),
    # The NLLB-200 CTranslate2 model dir (model.bin + tokenizer.json + ...). Copied to <plugins>\nllb\
    # so the in-process machine-translation fallback works. ~647 MB (fetch it with dl_model.py).
    [string]$ModelSrc = (Join-Path $PSScriptRoot "nllb-model"),
    # The higher-quality NLLB-1.3B model → <plugins>\nllb-1.3b\ (~1.4 GB). Enables the HQ toggle.
    [string]$HqModelSrc = (Join-Path $PSScriptRoot "nllb-1.3b-model"),
    # Bundled common-UI string list for pre-warming → <plugins>\glossary\common_ui.json.
    [string]$CommonUiSrc = (Join-Path $PSScriptRoot "nllb-common-ui.json"),
    # Icarus career_bot engine + data for the Advisor page (career_bot\ + data\ + succession_master_data\).
    # Copied into <plugins>\advisor so the plug-and-play sidecar can run the real advisor. Expected as
    # a sibling checkout of this repo.
    [string]$IcarusSrc = (Join-Path (Split-Path $PSScriptRoot -Parent) "Umamusume-Icarus")
)
$ErrorActionPreference = "Stop"

# Locate the game without assuming a drive or a library folder. Steam records its own install path in
# the registry, and every extra library the user added is listed in steamapps\libraryfolders.vdf — so
# a game on D:\SteamLibrary is found the same way as one in Program Files. Returns $null if the game
# isn't in any of them, which the caller turns into an actionable error.
# (deploy.ps1 carries its own copy of this on purpose — both scripts must run standalone.)
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

    # Each library root also advertises the others in libraryfolders.vdf ("path" "D:\\SteamLibrary").
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
        throw "Could not find UmamusumePrettyDerby in any Steam library.`nPass it explicitly:  .\install.ps1 -GameDir ""D:\SteamLibrary\steamapps\common\UmamusumePrettyDerby"""
    }
    Write-Host "Found game via Steam: $GameDir" -ForegroundColor Green
}

$plugins = Join-Path $GameDir "UmamusumePrettyDerby_Data\Plugins\x86_64"
$real    = Join-Path $plugins "cri_mana_vpx.dll"
$backup  = Join-Path $plugins "cri_mana_vpx_orig.dll"
$src     = Join-Path $PSScriptRoot "native\target\release\overseer_overlay.dll"

Write-Host "Game folder : $GameDir"
Write-Host "Plugins     : $plugins"

if (-not (Test-Path $plugins)) { throw "Plugins folder not found. Is -GameDir correct? ($plugins)" }
if (-not (Test-Path $src))     { throw "Built DLL not found: $src`nRun 'cargo build --release' in native\ first." }

$proc = Get-Process -Name "UmamusumePrettyDerby" -ErrorAction SilentlyContinue
if ($proc) { throw "The game is running. Fully close it before installing (the DLL is locked while it runs)." }

# SAFETY: only back up when no backup exists yet. If cri_mana_vpx_orig.dll already
# exists, the current cri_mana_vpx.dll is OUR proxy from a previous install — backing
# it up again would overwrite the real DLL with the proxy and destroy it.
if (-not (Test-Path $backup)) {
    if (Test-Path $real) {
        Copy-Item $real $backup
        Write-Host "Backed up real cri_mana_vpx.dll  ->  cri_mana_vpx_orig.dll" -ForegroundColor Green
    } else {
        Write-Warning "No existing cri_mana_vpx.dll to back up (unusual, but continuing)."
    }
} else {
    Write-Host "Backup already present -> this is a re-install/update; keeping the real DLL safe." -ForegroundColor Yellow
}

Copy-Item $src $real -Force
$size = [math]::Round((Get-Item $real).Length / 1MB, 2)
Write-Host "Installed unified DLL as cri_mana_vpx.dll ($size MB)." -ForegroundColor Green

# Ship the 26-language glossary data (glossary.json + glossary_ui.json per lang) next to the DLL.
$glossaryDst = Join-Path $plugins "glossary"
if (Test-Path $GlossarySrc) {
    $copied = 0
    Get-ChildItem $GlossarySrc -Directory -ErrorAction SilentlyContinue | ForEach-Object {
        $g  = Join-Path $_.FullName "glossary.json"
        $gu = Join-Path $_.FullName "glossary_ui.json"
        if ((Test-Path $g) -or (Test-Path $gu)) {
            $d = Join-Path $glossaryDst $_.Name
            New-Item -ItemType Directory -Force $d | Out-Null
            if (Test-Path $g)  { Copy-Item $g  $d -Force }
            if (Test-Path $gu) { Copy-Item $gu $d -Force }
            $copied++
        }
    }
    Write-Host "Copied glossary data for $copied languages -> $glossaryDst" -ForegroundColor Green
    # Protected proper-names list (skill/character/race names kept English). Language-independent.
    $names = Join-Path $GlossarySrc "names.json"
    if (Test-Path $names) {
        New-Item -ItemType Directory -Force $glossaryDst | Out-Null
        Copy-Item $names (Join-Path $glossaryDst "names.json") -Force
        Write-Host "Copied names.json (protected proper names)." -ForegroundColor Green
    }
} else {
    Write-Host "Glossary source not found ($GlossarySrc) - skipping (glossary translation is optional)." -ForegroundColor Yellow
}
# Ship the NLLB machine-translation model (model.bin + tokenizer.json + vocab) into <plugins>\nllb\.
$modelDst = Join-Path $plugins "nllb"
if (Test-Path (Join-Path $ModelSrc "model.bin")) {
    New-Item -ItemType Directory -Force $modelDst | Out-Null
    $mfiles = "model.bin","config.json","shared_vocabulary.txt","tokenizer.json","special_tokens_map.json","tokenizer_config.json"
    foreach ($f in $mfiles) {
        $p = Join-Path $ModelSrc $f
        if (Test-Path $p) { Copy-Item $p (Join-Path $modelDst $f) -Force }
    }
    $mb = [math]::Round((Get-Item (Join-Path $modelDst "model.bin")).Length / 1MB, 0)
    Write-Host "Installed NLLB translation model -> $modelDst ($mb MB)." -ForegroundColor Green
} else {
    Write-Host "NLLB model not found ($ModelSrc) - skipping (machine-translation fallback disabled; glossary still works)." -ForegroundColor Yellow
}

# Higher-quality 1.3B model (optional; enables the HQ toggle).
$hqDst = Join-Path $plugins "nllb-1.3b"
if (Test-Path (Join-Path $HqModelSrc "model.bin")) {
    New-Item -ItemType Directory -Force $hqDst | Out-Null
    foreach ($f in "model.bin","config.json","shared_vocabulary.txt","shared_vocabulary.json","tokenizer.json","special_tokens_map.json","tokenizer_config.json") {
        $p = Join-Path $HqModelSrc $f; if (Test-Path $p) { Copy-Item $p (Join-Path $hqDst $f) -Force }
    }
    $mb = [math]::Round((Get-Item (Join-Path $hqDst "model.bin")).Length / 1MB, 0)
    Write-Host "Installed HQ NLLB-1.3B model -> $hqDst ($mb MB)." -ForegroundColor Green
} else {
    Write-Host "HQ 1.3B model not found ($HqModelSrc) - skipping (600M still works; HQ toggle disabled)." -ForegroundColor Yellow
}

# Common-UI pre-warm list (optional).
if (Test-Path $CommonUiSrc) {
    New-Item -ItemType Directory -Force $glossaryDst | Out-Null
    Copy-Item $CommonUiSrc (Join-Path $glossaryDst "common_ui.json") -Force
    Write-Host "Installed common_ui.json (pre-warm list)." -ForegroundColor Green
}

# Advisor sidecar bundle (Live Advisor page, re-introduced 2026-07): vendor the Icarus career_bot
# engine + data plus our uma_sidecar/msgpack/pyembed into <plugins>\advisor so the plug-and-play
# sidecar runs the real advisor. .py/.json wildcards deliberately skip __pycache__, .venv and the
# data\presets\ user JSONs.
$advisorDst = Join-Path $plugins "advisor"
$advisorSrc = Join-Path $PSScriptRoot "advisor"   # uma_sidecar + msgpack + pyembed (bundled interpreter)
if ((Test-Path $IcarusSrc) -and (Test-Path $advisorSrc)) {
    New-Item -ItemType Directory -Force (Join-Path $advisorDst "career_bot\scenarios") | Out-Null
    New-Item -ItemType Directory -Force (Join-Path $advisorDst "data") | Out-Null
    Copy-Item (Join-Path $IcarusSrc "career_bot\*.py") (Join-Path $advisorDst "career_bot") -Force
    Copy-Item (Join-Path $IcarusSrc "career_bot\scenarios\*.py") (Join-Path $advisorDst "career_bot\scenarios") -Force
    Copy-Item (Join-Path $IcarusSrc "data\*.json") (Join-Path $advisorDst "data") -Force
    $succ = Join-Path $IcarusSrc "succession_master_data"
    if (Test-Path $succ) { Copy-Item $succ $advisorDst -Recurse -Force }
    foreach ($d in "uma_sidecar","msgpack","pyembed") {
        $p = Join-Path $advisorSrc $d
        if (Test-Path $p) { Copy-Item $p $advisorDst -Recurse -Force }
    }
    Write-Host "Installed advisor sidecar bundle (career_bot + data + uma_sidecar + pyembed) -> $advisorDst" -ForegroundColor Green
} else {
    Write-Host "Icarus source not found ($IcarusSrc) - skipping advisor bundle (Live Advisor stays offline)." -ForegroundColor Yellow
}
Write-Host ""
Write-Host "Done. Launch the game, then:" -ForegroundColor Cyan
Write-Host "  * open  http://127.0.0.1:1620   (the web control panel)"
Write-Host "  * press Insert in-game for the overlay menu"
Write-Host "To remove: run uninstall.ps1"
