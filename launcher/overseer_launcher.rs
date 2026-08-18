//! Overseer — Umamusume bot launcher.
//!
//! A tiny double-clickable front end so Overseer feels like a normal "run the exe" bot: it starts
//! Umamusume (which auto-loads the in-process Overseer DLL via the `cri_mana_vpx` proxy) and opens the
//! web control panel. The bot itself is a DLL, not an exe, because it must run INSIDE the game process
//! to hook the engine (translation, skips, camera, FPS) — this launcher is just the convenient on-ramp.
//!
//! Build:  rustc -O overseer_launcher.rs -o ../Overseer.exe
//! The game/plugins paths point at the Steam install, so this exe keeps working no matter where the
//! project source lives.

use std::path::Path;
use std::process::Command;
use std::{thread, time::Duration};

const GAME_DIR: &str = r"C:\Program Files (x86)\Steam\steamapps\common\UmamusumePrettyDerby";
const STEAM_APP_ID: &str = "3224770"; // Umamusume: Pretty Derby (Steam)
const UI_URL: &str = "http://localhost:1620";

fn plugins() -> std::path::PathBuf {
    Path::new(GAME_DIR).join(r"UmamusumePrettyDerby_Data\Plugins\x86_64")
}

/// Open a URL / protocol handler (web UI, steam://) through the shell.
fn shell_open(target: &str) {
    let _ = Command::new("cmd").args(["/C", "start", "", target]).spawn();
}

fn main() {
    println!("========================================");
    println!("   OVERSEER  ·  Umamusume bot launcher");
    println!("========================================\n");

    // 1. Installed? The installer backs up the original middleware DLL as cri_mana_vpx_orig.dll and
    //    drops our proxy in as cri_mana_vpx.dll. Presence of the backup = Overseer is installed.
    let installed = plugins().join("cri_mana_vpx_orig.dll").exists()
        && plugins().join("cri_mana_vpx.dll").exists();
    if installed {
        println!("[ok]  Overseer is installed.");
    } else {
        println!("[!!]  Overseer isn't installed in the game folder yet.");
        println!("      Run  install.ps1  once (right-click -> Run with PowerShell), then relaunch this.\n");
    }

    // 2. Already running? Then just re-open the panel.
    let running = tasklist_has("UmamusumePrettyDerby.exe");
    if running {
        println!("[ok]  Umamusume is already running — opening the control panel.");
    } else {
        // Launch THROUGH Steam (steam://rungameid), NOT the exe directly. A direct exe launch skips
        // Steam's auth handshake and leaves the game stuck before it loads the Overseer DLL; the
        // Steam URL boots it properly (verified: the in-process web server comes up within seconds).
        println!("[..]  Starting Umamusume via Steam …");
        shell_open(&format!("steam://rungameid/{STEAM_APP_ID}"));
        println!("[ok]  Launch requested. (Steam will start it — accept any login/update prompt.)");
    }

    // 3. Open the control panel once the in-process web server has had a moment to come up.
    println!("[..]  Opening the control panel: {UI_URL}");
    if !running {
        thread::sleep(Duration::from_secs(9));
    }
    shell_open(UI_URL);

    println!("\nOverseer is running. You can close this window.");
    println!("Turn the bot ON/OFF anytime from the panel's top-right power button.");
    thread::sleep(Duration::from_secs(4));
}

/// True if a process with this image name is running (best-effort via tasklist).
fn tasklist_has(image: &str) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {image}"), "/NH"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(image))
        .unwrap_or(false)
}
