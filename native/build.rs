//! Build script — link the proxy export definitions.
//!
//! The unified DLL doubles as a plug-and-play proxy loader (Frida-free). When
//! installed it is renamed to `cri_mana_vpx.dll` (or `UnityPlayer.dll`) and dropped
//! next to the game's real DLL; the game loads it, and our `proxy.rs` forwards the
//! impersonated exports to the backed-up original. For the OS loader to accept us as
//! that DLL we must EXPORT the same symbols — `src/proxy.def` lists them and this
//! script hands it to the MSVC linker via `/DEF:`.

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        // Absolute path — link.exe resolves /DEF: relative to its own CWD, not ours.
        let def = std::fs::canonicalize("src/proxy.def").expect("src/proxy.def must exist");
        println!("cargo:rustc-cdylib-link-arg=/DEF:{}", def.display());
        println!("cargo:rerun-if-changed=src/proxy.def");
        // Re-run this script whenever the project's absolute path changes (e.g. the whole repo is
        // MOVED) — otherwise cargo replays the cached /DEF: link-arg pointing at the OLD location and
        // link.exe fails with LNK1104 (cannot open proxy.def). CARGO_MANIFEST_DIR is that absolute path.
        println!("cargo:rerun-if-env-changed=CARGO_MANIFEST_DIR");
        println!("cargo:rerun-if-changed=build.rs");
    }
}
