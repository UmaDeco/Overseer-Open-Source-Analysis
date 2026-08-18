//! Portable path resolution — the DLL no longer hardcodes the developer's
//! machine paths. Two kinds of paths:
//!
//!  - **MOD-local files** (settings, logs): live next to `overseer_overlay.dll`
//!    (i.e. inside the game folder), found via `GetModuleFileNameW`. Always
//!    correct on any machine, no config.
//!
//!  - **Team Trials captures**: go into the **Overseer dashboard's own data
//!    folder**. The dashboard publishes its `data/` path on startup to
//!    `%LOCALAPPDATA%\Overseer\datadir.txt`; we read that so captures land right
//!    next to the user's existing history. If the dashboard has never run, we
//!    fall back to `%LOCALAPPDATA%\Overseer\data` (the dashboard's importer also
//!    checks that location).

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Our own module handle, recorded in DllMain. Lets path resolution keep working when
/// the DLL is renamed for the proxy loader (e.g. to `cri_mana_vpx.dll`), where a
/// name-based `GetModuleHandleW("overseer_overlay.dll")` lookup would fail.
static DLL_HMODULE: AtomicUsize = AtomicUsize::new(0);

/// Called once from DllMain with our DLL's HMODULE.
pub fn set_module(h: usize) {
    DLL_HMODULE.store(h, Ordering::Relaxed);
}

/// The DLL's own module handle: the one recorded in DllMain, else a best-effort
/// lookup by the dev build name.
fn module_handle() -> usize {
    let h = DLL_HMODULE.load(Ordering::Relaxed);
    if h != 0 {
        return h;
    }
    unsafe { GetModuleHandleW(wide("overseer_overlay.dll").as_ptr()) as usize }
}

fn module_path() -> Option<PathBuf> {
    let h = module_handle();
    if h == 0 {
        return None;
    }
    unsafe {
        let mut buf = [0u16; 1024];
        let n = GetModuleFileNameW(h as _, buf.as_mut_ptr(), buf.len() as u32);
        if n == 0 {
            return None;
        }
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..n as usize])))
    }
}

/// Cached results of the two Win32 module queries. `GetModuleFileNameW` is a syscall and the
/// results are immutable for the process lifetime, yet `dll_dir()` sits under `log_file()` — which
/// the logger called on EVERY line. Resolving once turns a per-log-line syscall + allocation into
/// a pointer read. (Measured as a real cost during skip floods, where the log rate spikes.)
static DLL_DIR: OnceLock<PathBuf> = OnceLock::new();
static DLL_NAME: OnceLock<String> = OnceLock::new();
static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Directory that contains our DLL — the game's `Plugins/x86_64` folder when installed
/// as the `cri_mana_vpx` proxy, or wherever the dev DLL lives. Falls back to the
/// current dir if the module can't be located. Resolved once, then cached.
pub fn dll_dir() -> PathBuf {
    dll_dir_ref().clone()
}

/// Borrowed form of [`dll_dir`] — no allocation. Prefer this on any path that runs often.
pub fn dll_dir_ref() -> &'static PathBuf {
    DLL_DIR.get_or_init(|| {
        module_path()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."))
    })
}

/// Our DLL's own file name (e.g. `overseer_overlay.dll` or `cri_mana_vpx.dll`), used by
/// the proxy loader to pick which identity to forward.
pub fn dll_file_name() -> String {
    DLL_NAME
        .get_or_init(|| {
            module_path()
                .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_default()
        })
        .clone()
}

/// A MOD-local file next to the DLL (e.g. the settings JSON).
pub fn local_file(name: &str) -> PathBuf {
    dll_dir_ref().join(name)
}

/// The `<dll dir>/overseer-logs/` directory, created ONCE (not per call — `create_dir_all` stats
/// every path component, which the old per-log-line call turned into a syscall storm).
pub fn log_dir() -> &'static Path {
    LOG_DIR.get_or_init(|| {
        let dir = dll_dir_ref().join("overseer-logs");
        let _ = std::fs::create_dir_all(&dir);
        dir
    })
}

/// A MOD log file under `<dll dir>/overseer-logs/`. The folder is ensured once (see [`log_dir`]).
pub fn log_file(name: &str) -> PathBuf {
    log_dir().join(name)
}

fn localappdata_overseer() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("Overseer")
}

/// The Overseer dashboard's `data/` directory. Prefers the path the dashboard
/// published (`%LOCALAPPDATA%\Overseer\datadir.txt`); falls back to
/// `%LOCALAPPDATA%\Overseer\data`.
pub fn overseer_data_dir() -> PathBuf {
    let pointer = localappdata_overseer().join("datadir.txt");
    if let Ok(s) = std::fs::read_to_string(&pointer) {
        let p = PathBuf::from(s.trim());
        if !s.trim().is_empty() && p.is_dir() {
            return p;
        }
    }
    localappdata_overseer().join("data")
}

/// Where the MOD writes Team Trials captures: `<overseer data>/htt/native`.
pub fn tt_capture_dir() -> PathBuf {
    overseer_data_dir().join("htt").join("native")
}
