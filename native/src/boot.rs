//! Overseer Plan B — native bootstrap.
//!
//! On DLL attach we cannot touch IL2CPP yet (GameAssembly.dll loads after our
//! DllMain), so we spawn a worker thread that:
//!   1) waits for GameAssembly.dll,
//!   2) resolves the IL2CPP C API + attaches the thread to the domain,
//!   3) installs every native module (career reader, SuperSkip, FPS, race),
//!   4) marks the engine ready.
//! From then on the game's own threads drive our hooks and the overlay renders
//! the shared state. No Frida, no Python — this is the full-native runtime.
//!
//! A concise startup report is written to logs/overseer-native.log.

use std::time::Duration;

use crate::performance::fps;
use crate::htt;
use crate::il2cpp;
use crate::ipc;
#[cfg(feature = "raceread")]
use crate::race;
use crate::settings;
use crate::skip;

fn log(msg: &str) {
    crate::tools::log(msg);
}

/// Spawn the native engine thread. hudhook re-invokes `new_with_engine()` (→ this) EVERY time it
/// rebuilds the render loop on a D3D swapchain reset (window resize / display-mode change), so this
/// can be called dozens of times per process. Boot must run ONCE: the IL2CPP hooks it installs live
/// for the whole process, and re-running it spawns redundant boot / scene-probe / audio threads —
/// the extra IL2CPP-attached probe threads are a GC hazard (a stray one alive during a collection
/// trips "Collecting from unknown thread", e.g. at graduation/career-end). Guard it process-once.
/// The D3D capture + overlay in `new_with_engine` correctly still run each time (new device).
pub fn spawn() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static BOOTED: AtomicBool = AtomicBool::new(false);
    if BOOTED.swap(true, Ordering::SeqCst) {
        return; // already booted this process
    }
    std::thread::spawn(|| {
        log("==== Overseer native engine starting ====");
        log("Overseer MOD");
        crate::career::mark_session_start(); // anchor "this session" stat-scope to launch time
        ipc::set_status("waiting for GameAssembly…");

        // 1) Wait for GameAssembly.dll.
        let mut waited: u64 = 0;
        while !il2cpp::game_loaded() {
            std::thread::sleep(Duration::from_millis(250));
            waited += 250;
            if waited > 180_000 {
                log("TIMEOUT: GameAssembly.dll never appeared");
                return;
            }
        }
        log(&format!("step1: GameAssembly loaded ({waited}ms)"));

        // 2) Resolve the IL2CPP exports (GetProcAddress only — no managed calls).
        if let Err(e) = il2cpp::init() {
            log(&format!("step2: il2cpp::init FAILED: {e}"));
            return;
        }
        log("step2: exports resolved");

        // 3) Wait for the IL2CPP domain to exist (domain_get = no alloc, safe).
        ipc::set_status("waiting for IL2CPP runtime…");
        let mut rwait: u64 = 0;
        while il2cpp::domain().is_null() {
            std::thread::sleep(Duration::from_millis(250));
            rwait += 250;
            if rwait > 180_000 {
                log("step3: TIMEOUT domain");
                return;
            }
        }
        log(&format!("step3: domain present ({rwait}ms)"));

        // 3b) Let the runtime/GC fully settle before we touch it. With the proxy
        //     loader we reach this point during early init; attaching into a
        //     freshly-created domain races the GC. A short settle window makes
        //     the proxy path behave like the (working) late-injection path.
        std::thread::sleep(Duration::from_secs(5));
        log("step3b: settle done");

        // 4) Attach this thread, then confirm classes resolve.
        let overseer_thread = il2cpp::attach_current_thread();
        log("step4: thread attached");
        let mut cwait: u64 = 0;
        while il2cpp::class("Gallop.ButtonCommon").is_null() {
            std::thread::sleep(Duration::from_millis(250));
            cwait += 250;
            if cwait > 60_000 {
                log("step4: TIMEOUT classes");
                return;
            }
        }
        log(&format!("step5: classes resolvable — runtime ready ({}ms total)", waited + rwait + cwait));

        // Arm the crash detector before installing our hooks, so a fault in any of them is
        // logged with a breadcrumb to overseer-crash.log.
        crate::crashlog::install();

        // Active-scene probe (gates the intro player on the title screen) + intro-song
        // audio worker + BGM mute API. Private (`banner`) build only; the video player's
        // device capture runs separately and early (from new_with_engine).
        #[cfg(feature = "banner")]
        {
            crate::startup_probe::spawn();
            crate::audio::spawn();
            crate::bgm::init();
        }

        // External mod DLLs in overseer_plugins/ are LOADED much EARLIER by the version.dll
        // proxy (in its DllMain, before the overlay and IL2CPP) so self-contained proxy-style
        // mods install their hooks in time. See proxy/src/lib.rs::load_plugins_early.
        //
        // SDK-style plugins (they export `overseer_init` and hook through a host-supplied
        // vtable) can't act on a bare LoadLibrary — they need the host to call their init
        // AFTER il2cpp is up. We do exactly that here, now that the runtime is ready,
        // handing them an Overseer-backed compatible vtable. Self-contained mods
        // (no such export) are left untouched (already started by the early loader).
        log(&format!("plugins(sdk): {}", crate::overseer_compat::init_plugins()));
        // If an external SDK plugin (e.g. a companion feed) loaded from overseer_plugins/, it already
        // owns the overlay UDP channel — stand our native feed down so the two don't double-send.
        if crate::overseer_compat::sdk_plugins_loaded() > 0 {
            crate::uma_bridge::set_external_active(true);
            log("companion feed: external SDK plugin present -> native feed deferred");
        }


        // Exception tracer REMOVED: hooking System.Exception..ctor globally fires on EVERY exception
        // the game constructs — including IL2CPP-internal ones raised during garbage collection — and
        // a detour on that path can put the GC on a thread it doesn't expect ("collect from unknown
        // thread", the reported launch crash). It was a diagnostic for the Scout FormatException, now
        // long resolved, so it's gone rather than merely dormant.

        let (tr_ok, ev_ok, snotes) = skip::install();
        log(&format!("superskip: training={tr_ok} events={ev_ok} [{}]", snotes.trim_end()));
        crate::diag::record_install("superskip", &format!("training={tr_ok} events={ev_ok} [{}]", snotes.trim_end()));
        match skip::install_race_result() {
            Ok(note) => {
                log(&format!("race-result (off by default): armed [{}]", note.trim_end()));
                crate::diag::record_install("race-result", &format!("armed [{}]", note.trim_end()));
            }
            Err(e) => {
                log(&format!("race-result: not armed ({e})"));
                crate::diag::record_install("race-result", &format!("NOT armed ({e})"));
            }
        }
        {
            let fnotes = crate::followers::install();
            log(&format!("auto-unfollow (off by default): {fnotes}"));
            crate::diag::record_install("auto-unfollow", &fnotes);
        }
        {
            let fps_status = fps::install();
            log(&format!("fps control: {fps_status}"));
            crate::diag::record_install("fps control", &fps_status);
        }
        match crate::ui_tempo::install() {
            Ok(detail) => { log(&format!("ui tempo: {detail}")); crate::diag::record_install("ui tempo", detail); }
            Err(e) => { log(&format!("ui tempo: deferred ({e})")); crate::diag::record_install("ui tempo", &format!("deferred ({e})")); }
        }
        crate::crashlog::crumb(4);
        // cyspring (cloth-physics uncap) NOT installed — its CySpringController.Init detour crashed
        // during the URA Finale ending (AV, breadcrumb 31) and the feature is negligible QoL. Code
        // kept intact; we simply don't arm the hook, so on_init never runs. Re-enable by restoring
        // the install() call if the crash is ever root-caused.
        let _ = crate::performance::cyspring::install; // keep the symbol referenced (no-op)
        crate::crashlog::crumb(1);
        match crate::performance::graphics::install() {
            Ok(()) => { log("graphics tweaks: OK"); crate::diag::record_install("graphics tweaks", "OK"); }
            Err(e) => { log(&format!("graphics tweaks: deferred ({e})")); crate::diag::record_install("graphics tweaks", &format!("deferred ({e})")); }
        }
        crate::crashlog::crumb(2);
        match crate::performance::display::install() {
            Ok(()) => { log("display tweaks: OK"); crate::diag::record_install("display tweaks", "OK"); }
            Err(e) => { log(&format!("display tweaks: deferred ({e})")); crate::diag::record_install("display tweaks", &format!("deferred ({e})")); }
        }
        crate::crashlog::crumb(3);
        crate::performance::display::install_window();
        crate::crashlog::crumb(0);
        #[cfg(feature = "raceread")]
        {
            let r = race::install();
            log(&format!("race reader: {r}"));
            crate::diag::record_install("race reader", &r);
        }

        #[cfg(feature = "freecam")]
        {
            let r = crate::freecam::install();
            log(&format!("freecam: {r}"));
            crate::diag::record_install("freecam", &r);
        }

        // Network response hook: reads each msgpack API response to identify the player's horse
        // (Top-1 race-result skip gate), remaining race retries, feed the companion bridge, and —
        // full-build-only extras. One detour for all.
        {
            crate::response_hook::install();
            log("response hook: armed");
            crate::diag::record_install("response_hook", "armed");
        }

        // Texture/asset localization dispatcher (Phase 3 substrate). ONE owner for AssetBundle
        // loads; Phase-4 localizers register callbacks per asset class. A pure pass-through until
        // then, so arming it now just validates the 5 hooks are ABI-correct against live loads.
        match crate::assets::install() {
            Ok(()) => {
                log("assets: dispatcher armed");
                crate::diag::record_install("assets", "armed");
            }
            Err(e) => {
                log(&format!("assets: deferred ({e})"));
                crate::diag::record_install("assets", &format!("deferred ({e})"));
            }
        }

        // Companion-overlay bridge: forward requests (CompressRequest hook) + responses (fed from the
        // DecompressResponse hook) to companion overlays over UDP 17229,
        // so tools that used a separate capture plugin work with Overseer directly.
        {
            crate::uma_bridge::install();
            log("uma_bridge: request capture armed");
            crate::diag::record_install("uma_bridge", "armed");
        }

        // HorseTheTrails — native Team Trials capture (hooks TeamStadiumResult).
        // Runs while this boot thread is still IL2CPP-attached (scan needs it).
        {
            let r = htt::install();
            log(&format!("HorseTheTrails: {r}"));
            crate::diag::record_install("HorseTheTrails", &r);
        }

        // Veterans export (Hakuraku-format trained-uma dump). Needs the same attached
        // boot thread (it scans the assembly for the TrainedChara[] method).
        {
            let r = crate::umas::install();
            log(&format!("veterans export: {r}"));
            crate::diag::record_install("veterans export", &r);
        }

        // Team Trials deck-profile capture (hooks TeamStadiumDeckBuilder.Setup/Release to track the
        // live team-edit screen, so the padder can drive its grid). Non-fatal if it misses.
        {
            let r = crate::padder::install();
            log(&format!("tt padder: {r}"));
            crate::diag::record_install("tt padder", &r);
        }
        {
            let r = crate::hunter::install();
            log(&format!("tt hunter: {r}"));
            crate::diag::record_install("tt hunter", &r);
        }

        // Soft reset (GameSystem.SoftwareReset) — reload to title without killing the process.
        // Resolved by name; fired from the overlay via reset::request(), executed on the main-thread
        // TweenManager.Update pump (reset::poll()).
        {
            let r = crate::reset::install();
            log(&format!("soft reset: {r}"));
            crate::diag::record_install("soft reset", &r);
        }

        // Succession-affinity badges (Legacy Select): hooks Gallop.SingleModeUtils.CalcRelationPoint for
        // the game's EXACT affinity value + the SingleModeStartStepSuccessionSelect Show/Hide screen gate.
        // Needs the still-attached boot thread (assembly scan). Non-fatal on a miss.
        {
            let r = crate::affinity::install();
            log(&format!("affinity: {r}"));
            crate::diag::record_install("affinity", &r);
        }

        // Advisor sidecar (OPTIONAL Python career-advisor): long-polls the captured payload and POSTs
        // advice back to /internal/advice for the SPA. Fully guarded — a no-op if python or the sidecar
        // script isn't installed, so the DLL runs normally with the advisor simply "offline".
        crate::sidecar::spawn();

        // Coexistence build variant: report which hooks Overseer owns vs ceded to a co-resident mod.
        #[cfg(feature = "overseer")]
        log(&format!("hook arbiter: {}", crate::arbiter::report()));

        // Apply persisted toggle state (SuperSkip / Race-result / FPS / TT) AND the runtime power
        // state — the master Overseer switch and the independent translation switch, plus each
        // subsystem's "keep running while disabled" exception. Everything below reads that state,
        // so it has to land before the localization/MTL stack comes up.
        settings::apply_on_boot();
        log("settings: applied persisted state");
        log(&format!("runtime: {}", crate::runtime::status_json()));
        {
            let cfg = crate::webhook::config();
            let live = cfg.targets.iter().filter(|t| t.enabled && !t.url.trim().is_empty()).count();
            log(&format!(
                "webhooks: {} ({live} endpoint{} configured)",
                if cfg.enabled { "enabled" } else { "disabled" },
                if live == 1 { "" } else { "s" }
            ));
            crate::diag::record_install("webhook", if cfg.enabled { "enabled" } else { "disabled" });
        }

        // Load the active translation pack (localized_data/ next to the DLL), if present. The
        // localization hooks (Phase-4 waves) read from this store; no-op when no pack is installed.
        crate::localize::reload();
        // Load the Overseer glossary for the configured language (Localize::Get miss-fallback).
        crate::glossary::reload();
        // Async MTL pipeline: spawn the (non-attached, pure-Rust) worker + load the active language's
        // mtl.json cache. The worker never touches il2cpp; it hands results to the ui_tempo re-apply
        // pump. NLLB model load is kicked off inside spawn_worker (no-op until the model is installed).
        crate::mtl::spawn_worker();
        crate::mtl::load_names(); // proper-noun protection (skill/character/race names stay English)
        crate::mtl::reload();

        // Localization Wave 2: builtin UI strings via Gallop.Localize::Get. A cede/miss is non-fatal
        // (the game just runs untranslated), matching the assets dispatcher's tolerance.
        match crate::loc_ui::install() {
            Ok(()) => {
                log("loc/ui: Localize.Get hooked");
                crate::diag::record_install("loc_ui", "armed");
            }
            Err(e) => {
                log(&format!("loc/ui: deferred ({e})"));
                crate::diag::record_install("loc_ui", &format!("deferred ({e})"));
            }
        }

        // Broad instant translation: hook the game's text setters (TMP_Text/UI.Text/TextCommon/
        // TMPUguiCommon) so every dynamically-set string runs through the glossary — this is what
        // gives Overseer-level coverage; Localize.Get alone only reaches static UI keys.
        match crate::loc_settext::install() {
            Ok(()) => {
                log("loc/settext: text setters hooked");
                crate::diag::record_install("loc_settext", "armed");
            }
            Err(e) => {
                log(&format!("loc/settext: deferred ({e})"));
                crate::diag::record_install("loc_settext", &format!("deferred ({e})"));
            }
        }

        // Overseer PACK-localization hooks. Pack support is tier-0 of the translation stack: a
        // community pack's human strings win over the glossary/NLLB tiers wherever the pack covers
        // them (its output is marked as our own, so the MTL tiers pass it through untouched).
        //
        // Gated on a pack actually being present (localize::reload ran above): without dicts these
        // hooks can only ever miss, so installing them would be pure detour risk for zero value.
        // A pack dropped in later needs a game restart to arm them — reload() hot-swaps dict CONTENT,
        // but hooks install once, at boot.
        //
        // loc_story stays OFF unconditionally: its StoryTimelineData field-pokes ran at offsets never
        // validated on this Global/Steam build and corrupted the story/race timeline (the historical
        // soft-lock — confirmed by removal). Re-enable only after per-offset verification, never just
        // because a pack ships story dicts. Timeline text still translates via set_text + NLLB.
        // loc_text (hashed_dict whole-string swaps) also stays off — Localize.Get + set_text already
        // cover its surface on this build.
        if crate::localize::is_loaded() {
            match crate::loc_db::install() {
                Ok(()) => {
                    log("loc/db: master.mdb text hooks armed (pack tier-0)");
                    crate::diag::record_install("loc_db", "armed (pack)");
                }
                Err(e) => {
                    log(&format!("loc/db: deferred ({e})"));
                    crate::diag::record_install("loc_db", &format!("deferred ({e})"));
                }
            }
            match crate::loc_texture::install() {
                Ok(()) => {
                    log("loc/texture: replacement-texture localizer armed (pack tier-0)");
                    crate::diag::record_install("loc_texture", "armed (pack)");
                }
                Err(e) => {
                    log(&format!("loc/texture: deferred ({e})"));
                    crate::diag::record_install("loc_texture", &format!("deferred ({e})"));
                }
            }
        } else {
            crate::diag::record_install("loc_db", "idle (no pack)");
            crate::diag::record_install("loc_texture", "idle (no pack)");
            log("loc: no translation pack — glossary+MTL path only (drop localized_data/ next to the DLL and restart to add one)");
        }
        // Font + overflow localizer (loc_font). Arms when EITHER a pack names a replacement font OR a
        // target MTL language is set — the latter drives the OS-dynamic-font glyph fix + text-overflow
        // remediation for non-Latin / longer translations, which needs no pack. Placed OUTSIDE the pack
        // branch (after assets::install() for the AssetBundle trampolines, and after localize::reload())
        // so it arms in the MTL-only, packless case; the Awake hooks choose pack-vs-MTL-vs-nothing at
        // runtime. The per-handle resolve + OS-font probe detail is recorded under "loc_font_os".
        let want_font = crate::localize::data().config.replacement_font_name.is_some()
            || crate::settings::tl_lang().is_some();
        if want_font {
            match crate::loc_font::install() {
                Ok(()) => {
                    log("loc/font: font + overflow localizer armed");
                    crate::diag::record_install("loc_font", "armed");
                }
                Err(e) => {
                    log(&format!("loc/font: deferred ({e})"));
                    crate::diag::record_install("loc_font", &format!("deferred ({e})"));
                }
            }
        } else {
            crate::diag::record_install("loc_font", "idle (no pack font, no target language)");
        }
        crate::diag::record_install("loc_story", "disabled (unvalidated offsets — see boot.rs)");

        // Install is done. Hooks now run on the GAME's (already-attached) threads,
        // so this boot thread no longer needs to be attached. DETACH it cleanly
        // and let it exit — leaving it attached + alive made the shutdown GC
        // "collect from an unknown thread" when the game closes. Detaching
        // unregisters it from the GC so teardown is clean.
        il2cpp::detach_thread(overseer_thread);
        ipc::set_status("Overseer native engine ready");
        // Signal the web UI (webui.rs) that hooks are live so it reports "attached".
        ipc::set_core_ready(true);
        // Freeze detector. Started last, once the tween pump that feeds its heartbeat is armed.
        // (watchdog removed — it was a diagnostic-only background thread added for the Scout
        //  investigation; a background thread is a "collect from unknown thread" GC hazard and it has
        //  served its purpose.)
        log("==== ready (boot thread detached) ====");

        // Self-updater: check for a newer Overseer release on boot. It is DORMANT until a release repo
        // is configured (selfupdate::REPO / OVERSEER_REPO env at build) — `check` early-returns and
        // does nothing when unconfigured, so this is safe to always call and can never pull the wrong
        // DLL. Results surface in the web UI banner (/api/update/status), not the old imgui dialog.
        // `false` = respect the per-version "don't ask again" skip (the manual web button passes true).
        crate::selfupdate::check(false);
    });
}

