//! Plug-and-play proxy loader (Frida-free) — ported from the upstream mod's Windows proxy-DLL loader.
//!
//! The unified DLL loads itself into Umamusume by impersonating a DLL the game
//! already loads: primarily **cri_mana_vpx.dll** (CriWare Mana VPX video codec,
//! in `UmamusumePrettyDerby_Data/Plugins/x86_64/`), which loads *after*
//! GameAssembly.dll so IL2CPP is resolvable at DllMain. A secondary
//! **UnityPlayer.dll** identity is also exported for an early-load vector.
//!
//! Mechanism: `src/proxy.def` (linked via build.rs `/DEF:`) exports the same
//! symbols the real DLL does. Each is a naked `jmp qword ptr [rip+ORIG]` stub
//! (the `proxy_proc!` macro). At `init()` we `LoadLibraryW` the backed-up real DLL
//! and fill each `ORIG` via `GetProcAddress`, so every forwarded call tail-jumps
//! straight into the genuine implementation. `init()` runs SYNCHRONOUSLY in DllMain
//! before the game can call any export.
//!
//! Install layout: the installer renames the game's real `cri_mana_vpx.dll` to
//! `cri_mana_vpx_orig.dll` (kept next to us) and drops this DLL as
//! `cri_mana_vpx.dll`. `paths::dll_dir()` resolves that folder via our own module
//! handle (recorded in DllMain), so the backup and settings/logs all sit together.

#![allow(non_snake_case, non_upper_case_globals, dead_code)]

use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

use crate::paths;

/// Export `$name` as a naked symbol that tail-jumps to `$orig` (an 8-byte slot filled
/// at init with the real function's address).
macro_rules! proxy_proc {
    ($name:ident, $orig:ident) => {
        static mut $orig: usize = 0;
        std::arch::global_asm!(
            concat!(".globl ", stringify!($name)),
            concat!(stringify!($name), ":"),
            "    jmp qword ptr [rip + {}]",
            sym $orig
        );
    };
}

// cri_mana_vpx.dll exports (primary vector).
proxy_proc!(criVvp9_GetAlphaInterface, criVvp9_GetAlphaInterface_orig);
proxy_proc!(criVvp9_GetInterface, criVvp9_GetInterface_orig);
proxy_proc!(criVvp9_SetUserAllocator, criVvp9_SetUserAllocator_orig);

// UnityPlayer.dll export (secondary / early vector).
proxy_proc!(UnityMain, UnityMain_orig);

fn log(msg: &str) {
    crate::tools::log(msg);
}

unsafe fn load_real(path: &std::path::Path) -> usize {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    LoadLibraryW(wide.as_ptr()) as usize
}

unsafe fn proc(h: usize, name: &[u8]) -> usize {
    GetProcAddress(h as _, name.as_ptr()).map(|f| f as usize).unwrap_or(0)
}

/// Forward the impersonated exports to the backed-up real DLL. Selects which identity
/// to forward by our OWN on-disk file name. A no-op when running under the dev name
/// (`overseer_overlay.dll`, e.g. loaded by Frida for iteration) — nothing calls the
/// forwarded exports then. Call this SYNCHRONOUSLY from DllMain(PROCESS_ATTACH).
pub fn init() {
    let name = paths::dll_file_name().to_ascii_lowercase();
    let dir = paths::dll_dir();

    unsafe {
        if name.contains("cri_mana_vpx") {
            let real = dir.join("cri_mana_vpx_orig.dll");
            let h = load_real(&real);
            if h == 0 {
                log(&format!("[proxy] FAILED to load real DLL at {}", real.display()));
                return;
            }
            criVvp9_GetAlphaInterface_orig = proc(h, b"criVvp9_GetAlphaInterface\0");
            criVvp9_GetInterface_orig = proc(h, b"criVvp9_GetInterface\0");
            criVvp9_SetUserAllocator_orig = proc(h, b"criVvp9_SetUserAllocator\0");
            log("[proxy] cri_mana_vpx exports forwarded");
        } else if name.contains("unityplayer") {
            let real = dir.join("UnityPlayer_orig.dll");
            let h = load_real(&real);
            if h == 0 {
                log(&format!("[proxy] FAILED to load real DLL at {}", real.display()));
                return;
            }
            UnityMain_orig = proc(h, b"UnityMain\0");
            log("[proxy] UnityPlayer export forwarded");
        } else {
            // Dev name (overseer_overlay.dll / Frida load) — no forwarding needed.
            log(&format!("[proxy] running as '{name}' — no export forwarding"));
        }
    }
}
