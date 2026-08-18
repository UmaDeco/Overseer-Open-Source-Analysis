//! Overseer MOD — internal overlay DLL entry point.
//!
//! Loaded into UmamusumePrettyDerby.exe (by the Frida core's `Module.load`).
//! On attach we start the loopback IPC server and install the D3D11 + imgui
//! overlay via hudhook. From then on the game's own render thread calls our
//! `OverseerOverlay::render`, drawing the HUD inside the swapchain — true
//! in-game rendering, no external window.
//!
//! If the game ever ships on D3D12 / Vulkan, swap `ImguiDx11Hooks` below for
//! `ImguiDx12Hooks` / the Vulkan hook (see build.md).

// Intro player support (native song playback, original-BGM mute, title-scene probe).
// gated with the `banner` feature, like the video player itself.
// Texture/asset localization substrate: one AssetBundle dispatcher + il2cpp texture bindings +
// the PNG-diff pipeline. Phase-4 localizers register callbacks here.
mod assets;
#[cfg(feature = "banner")]
mod audio;
#[cfg(feature = "banner")]
mod bgm;
#[cfg(feature = "overseer")]
mod arbiter;
mod boot;
mod career;
mod clipboard;
mod crashlog;
mod data;
mod diag;
// Self-healing latches/gates + the per-frame stall watchdog (soft-lock recovery).
mod guard;
// Legacy / inheritance analysis: affinity matrix, parent recommendations, spark modelling and the
// multi-career legacy-loop planner (Legacy Loops methodology, driven by Overseer's exact numbers).
mod legacy;
mod loadprof;
// Two-switch power state (Overseer master vs. live translation) + per-subsystem suspend/resume.
mod runtime;
// Career-completion webhooks (Discord + generic JSON).
mod webhook;
// Main-thread work queue (drained on the ui_tempo pump) — marshals il2cpp/GPU work
// from background threads (web server, texture pipeline) onto the game's render thread.
mod mainthread;
mod overseer_compat;
#[cfg(feature = "freecam")]
mod followers;
mod freecam;
#[cfg(feature = "freecam")]
mod race_director;
mod performance;
mod hooks;
mod http;
mod il2cpp;
// Localization content store (translation packs: the six lookup dicts + assets, ArcSwap hot-reload).
mod localize;
// Phase 5: Overseer's dynamic glossary translation layer (Localize::Get miss-fallback, 26 langs).
mod glossary;
// Localization templating: $(plural/ordinal/month ...) expansion + the gettext plural-forms engine.
mod plurals;
mod template;
// Shared CJK-aware text wrapping/fitting engine (dialogue + skill text).
mod wrap;
// Localization hooks — Wave 2: builtin UI strings via Gallop.Localize::Get.
mod loc_ui;
// Broad instant translation via the game's text setters (TMP_Text/UI.Text/TextCommon/TMPUguiCommon).
mod loc_settext;
// Async machine-translation pipeline (worker + cache + re-apply pump) and the in-process NLLB engine.
mod mtl;
mod nllb;
// Localization hooks — Wave 3: texture/atlas image localization (registers on the assets dispatcher).
mod loc_texture;
// Localization — Wave 4: SQLite master.mdb text (SELECT state machine + Sqlite3 hooks).
mod sql;
mod loc_db;
// Localization — Wave 5: StoryTimeline dialogue localization + clip/voice retiming.
mod loc_story;
// Localization — Wave 6: replacement-font swap + TextGenerator hashed text (loc_text disarmed).
mod loc_font;
mod loc_text;
#[cfg(feature = "banner")]
mod intro_player;
mod cvd;
mod htt;
mod htt_il2cpp;
mod hunter;
// Legacy Select succession-affinity badges: exact CalcRelationPoint value + SuccessionSelect Show/Hide gate.
mod affinity;
mod ipc;
mod menu_model;
mod names;
// Shared msgpack (rmpv) tree-walk helpers for the response-hook consumers.
// Only compiled when an rmpv-pulling feature is on.
mod msgpack;
mod overlay;
mod padder;
mod paths;
// Plug-and-play proxy loader (Frida-free): impersonate cri_mana_vpx.dll / UnityPlayer.dll.
mod proxy;
// Live race reader (Race panel + race-result win-gate). Built in both the full
// both builds (the race-result skip needs finish placement).
#[cfg(feature = "raceread")]
mod race;
// Generic IL2CPP managed-object → JSON reflection walker (used by race_export + umas).
#[cfg(feature = "raceread")]
mod il2cpp_json;
// Per-race JSON export (RaceInfo → disk, grouped by race type) for the web viewer.
#[cfg(feature = "raceread")]
mod race_export;
mod reset;
mod umas;
// The single Gallop.HttpHelper::DecompressResponse hook: player-id (race-result gate) + race
// retries + companion-bridge fan-out + full-build extras.
mod response_hook;
// Race REVEAL — decode the server's race result in-process (driven from response_hook's has_reveal branch).
mod race_reveal;
// Event REVEAL — decode every choice's outcome from the server's own reward table, before you pick.
mod event_reveal;
// Event AUDIT — replays event_reveal's recorded outcomes to score the decode rule it shipped.
mod event_audit;
// Pre-race FIELD — every runner's stats/aptitudes/mood, decoded at the race-entry screen.
mod race_field;
// Choice MARKS — screen rects of the live choice buttons, so the overlay can put a ✓/✗ on them.
// Advisor sidecar supervisor — launches the OPTIONAL Python career-advisor (self-guards if absent).
mod sidecar;
mod selfupdate;
mod settings;
mod skip;
// Shared cross-cutting utilities (logging, process clock). See tools/mod.rs.
mod tools;
// Generic UI click/dialog engine used by the SuperSkip legs (result + shop).
mod ui_input;
// Shared IL2CPP helpers for the Team Trials features (hunter + padder).
mod tt_il2cpp;
#[cfg(feature = "banner")]
mod startup_probe;
mod ui_tempo;
mod uma_bridge;
// Background main-thread freeze detector — tells a HANG from a STALL when the game soft-locks.
#[allow(dead_code)]
mod watchdog; // retained but not spawned (see boot.rs) — kept for future opt-in debugging
// Managed-exception tracer — the game suppresses Unity's logger, so C# exceptions are otherwise invisible.
#[allow(dead_code)]
mod exctrace; // retained but NOT installed (see boot.rs) — global Exception..ctor hook is a GC hazard
// In-process web UI server (the Overseer SPA on http://127.0.0.1:1620).
mod webui;
// Native, in-process stand-ins for the companion plugins (horseACT export, CarrotBlender feed).
mod friendlyplugins;
mod update;

use hudhook::hooks::dx11::ImguiDx11Hooks;

use overlay::OverseerOverlay;

/// DLL entry point. Replaces hudhook's `hudhook!` macro so we can, at PROCESS_ATTACH:
///   1. record our own module handle for path resolution (the DLL is renamed to
///      `cri_mana_vpx.dll` when installed as the proxy loader), and
///   2. forward the impersonated proxy exports to the real DLL **synchronously**,
///      before the game can call any of them (the Frida-free, plug-and-play load).
/// Then it installs the imgui overlay + engine exactly as the macro did.
#[no_mangle]
pub unsafe extern "system" fn DllMain(
    hmodule: hudhook::windows::Win32::Foundation::HINSTANCE,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) {
    if reason == hudhook::windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH {
        paths::set_module(hmodule.0 as usize);
        proxy::init();
        std::thread::spawn(move || {
            if let Err(e) = hudhook::Hudhook::builder()
                .with::<ImguiDx11Hooks>(OverseerOverlay::new_with_engine())
                .with_hmodule(hmodule)
                .build()
                .apply()
            {
                hudhook::tracing::error!("Couldn't apply hooks: {e:?}");
                hudhook::eject();
            }
        });
    }
}

impl OverseerOverlay {
    /// Construct the render loop and start the native engine. The engine thread
    /// waits for GameAssembly, resolves IL2CPP, installs every native module
    /// (career reader, SuperSkip, FPS, race), and publishes into the shared
    /// state the overlay renders. No Frida, no Python, no TCP.
    pub fn new_with_engine() -> Self {
        boot::spawn();
        // Serve the web control panel immediately — it needs no IL2CPP and shows
        // "waiting for game" until boot signals the engine is ready.
        webui::spawn();
        // Start the video player's D3D11 device capture early (independent of the IL2CPP
        // boot) so the intro can draw over the splash logos within ~1 s of launch.
        #[cfg(feature = "banner")]
        intro_player::spawn_capture();
        OverseerOverlay::new()
    }
}
