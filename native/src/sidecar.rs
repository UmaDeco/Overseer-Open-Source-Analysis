//! Advisor sidecar supervisor.
//!
//! Launches the Python career-advisor process that long-polls `/internal/capture` for the captured
//! game payload, computes advice with Icarus's `career_bot`, and POSTs it to `/internal/advice`
//! (which the SPA reads via `/api/advisor/state`). Fully OPTIONAL and guarded: if Python or the
//! sidecar script isn't installed the DLL runs normally and the SPA simply shows "advisor offline".
//!
//! Layout on disk: `<dll_dir>/advisor/uma_sidecar/__main__.py` (+ vendored `career_bot/` engine).

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};

static STARTED: AtomicBool = AtomicBool::new(false);
/// Whether the advisor is allowed to run. The child process is a Python interpreter long-polling
/// this DLL twice a second; leaving it running while Overseer is disabled is exactly the "most of
/// Overseer keeps running in the background" complaint. Gating the bridge (rather than killing the
/// process) keeps re-enabling instant and avoids racing the sidecar's own shutdown path — it sees
/// 204s, backs off to its idle poll, and costs nothing measurable.
static ACTIVE: AtomicBool = AtomicBool::new(true);

fn log(m: &str) {
    crate::tools::log(m);
}

/// Suspend or resume the advisor bridge (called by `runtime::fan_out`).
pub fn set_active(on: bool) {
    if ACTIVE.swap(on, Ordering::Relaxed) != on {
        log(if on { "[advisor] resumed" } else { "[advisor] suspended (Overseer disabled)" });
    }
}

/// May the `/internal/*` bridge serve the sidecar right now?
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Generate a per-boot token, publish it, and spawn the sidecar. Idempotent; non-fatal on any error.
pub fn spawn() {
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    // Per-boot token the sidecar must present on /internal/* (also locks out a stale process). Set it
    // BEFORE the supervisor thread launches Python so the /internal bridge unlocks correctly. This is
    // the only work the boot thread (still IL2CPP-attached) does — cheap and non-blocking.
    let token = gen_token();
    crate::ipc::set_token(token.clone());

    // Supervisor thread: locate the script, resolve Python, then launch and RESPAWN on a crash
    // (non-zero exit) with a crash-loop guard. Everything that spawns a child process (resolve_python's
    // `python --version` probes, the launch loop) lives here so the boot thread never blocks on it. A
    // CLEAN exit (code 0) is the sidecar's own game-is-gone shutdown → we stop. When the game closes,
    // this thread dies with the DLL, so it can't outlive the process.
    std::thread::Builder::new()
        .name("advisor-sup".into())
        .spawn(move || {
            let dir = crate::paths::dll_dir().join("advisor");
            let main = dir.join("uma_sidecar").join("__main__.py");
            if !main.exists() {
                log("[advisor] sidecar not installed (advisor/uma_sidecar) — advisor offline");
                return;
            }
            let Some(python) = resolve_python(&dir) else {
                log("[advisor] no python (bundled pyembed or system) — advisor offline");
                return;
            };
            // OVERSEER_ICARUS_DIR = where career_bot + data live (vendored under advisor/, or the user's Icarus dir).
            let icarus = std::env::var("OVERSEER_ICARUS_DIR").unwrap_or_else(|_| dir.to_string_lossy().into());

            let mut fast_exits = 0u32;
            loop {
                let started = std::time::Instant::now();
                // Detach the child from our console.
                //
                // The field logs show ten restarts with exit code 0xC000013A
                // (STATUS_CONTROL_C_EXIT) — the sidecar was not crashing, it was being KILLED by a
                // console control event delivered to the whole process group. Without a console of
                // its own the Python child inherits ours and dies to any Ctrl+C / Ctrl+Close /
                // logoff signal the game's console receives, and the supervisor then respawns a
                // fresh interpreter each time (visible as repeated "sidecar launched (pid …)").
                //
                //   CREATE_NEW_PROCESS_GROUP — console signals stop propagating to it.
                //
                // DETACHED_PROCESS / CREATE_NO_WINDOW were tried here and BROKE the advisor: with no
                // console the child inherits invalid stdio handles, and `uma_sidecar` logs with
                // `print(..., flush=True)` on its very first line — which then raises, killing it at
                // startup. The symptom is the Live Advisor sitting on "idle" forever while the
                // supervisor quietly respawns it. Redirecting stdio to null is the correct fix:
                // `print` always succeeds, nothing is inherited, and no window appears either.
                const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
                #[cfg(windows)]
                use std::os::windows::process::CommandExt;
                let mut cmd = std::process::Command::new(&python);
                cmd.arg("-m")
                    .arg("uma_sidecar")
                    .current_dir(&dir)
                    .env("OVERSEER_URL", "http://127.0.0.1:1620")
                    .env("OVERSEER_TOKEN", &token)
                    .env("OVERSEER_GAME_PID", std::process::id().to_string())
                    .env("OVERSEER_ICARUS_DIR", &icarus)
                    // Valid, always-writable stdio — see the note above.
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                #[cfg(windows)]
                cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
                let child = cmd.spawn();
                let mut child = match child {
                    Ok(c) => {
                        log(&format!("[advisor] sidecar launched (pid {})", c.id()));
                        c
                    }
                    Err(e) => {
                        log(&format!("[advisor] sidecar launch failed: {e}"));
                        return;
                    }
                };
                let code = child.wait().ok().and_then(|s| s.code());
                if code == Some(0) {
                    log("[advisor] sidecar exited cleanly (code 0) — supervisor stopping");
                    return; // clean self-exit (game gone / shutdown) → done
                }
                // Crash: guard against a hot restart loop (e.g. a broken interpreter/import).
                if started.elapsed().as_secs() < 5 {
                    fast_exits += 1;
                } else {
                    fast_exits = 0;
                }
                if fast_exits > 5 {
                    log("[advisor] sidecar keeps crashing on startup — giving up");
                    return;
                }
                log(&format!("[advisor] sidecar exited (code {code:?}) — restarting in 2s"));
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        })
        .ok();
}

/// Prefer the bundled embeddable interpreter (`advisor/pyembed/python.exe`) so the advisor is truly
/// plug-and-play — no system Python needed. Fall back to a system interpreter for dev/unbundled runs.
fn resolve_python(dir: &std::path::Path) -> Option<String> {
    let bundled = dir.join("pyembed").join("python.exe");
    if bundled.exists() {
        return Some(bundled.to_string_lossy().into_owned());
    }
    find_python()
}

/// A non-cryptographic per-boot token (localhost-only, low-stakes): time-nanos ⊕ pid, hex.
fn gen_token() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    format!("{:032x}", t ^ pid.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

fn find_python() -> Option<String> {
    for cand in ["python", "python3", "py"] {
        let ok = std::process::Command::new(cand)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Some(cand.to_string());
        }
    }
    None
}
