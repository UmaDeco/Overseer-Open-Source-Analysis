//! Overseer Plan B — persistent overlay settings.
//!
//! With no Python host, the DLL persists its own UI/toggle state to a small JSON
//! file. Loaded once at boot (after modules install) and re-saved whenever the
//! user changes a control in the overlay — so selections stick across sessions.
//!
//! Defaults (first run): Training + Events skip ON, Race-result OFF, FPS Off,
//! rail docked to the right edge.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use arc_swap::ArcSwap;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

use crate::performance::fps;
use crate::{htt, skip};

// True once apply_on_boot has applied the persisted state. Until then, save_current() must
// NOT run: the live fps/ui_tempo modules still hold pre-apply defaults (0 / 1.0), so a menu
// interaction during the ~5-8s boot window would otherwise overwrite the saved file with
// those defaults (the "my FPS/speed don't persist" bug).
static APPLIED: AtomicBool = AtomicBool::new(false);

fn settings_path() -> std::path::PathBuf {
    crate::paths::local_file("overseer-settings.json")
}

fn slog(msg: &str) {
    crate::tools::log(msg);
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    // Master power switch. When false the bot is fully PASSIVE — no translation, no skips, no free
    // camera, no UI speed-up — regardless of the individual toggles (which keep their values so
    // flipping this back on restores everything). Default ON. The web UI's Off button drives this.
    #[serde(default = "default_true")]
    pub bot_enabled: bool,
    // ── Overseer vs Translation: two INDEPENDENT switches ──────────────────────────────────────
    // `bot_enabled` above is the MASTER ("Overseer enabled"). It used to gate only the skips, the
    // UI tempo, the free camera and the translation dispatch, which is why turning Overseer off
    // felt like it only disabled translation — everything else (analysis, monitoring, exporters,
    // workers, timers) kept running. It now gates every subsystem through `runtime.rs`.
    //
    // `translation_enabled` is the SEPARATE live-translation switch: you can leave Overseer running
    // with translation off, and (with `keep_translation_when_disabled`) leave translation running
    // with Overseer off. The `keep_*_when_disabled` flags are the spec's "unless explicitly
    // configured otherwise" — each lets one subsystem survive the master switch. All default false.
    #[serde(default = "default_true")]
    pub translation_enabled: bool,
    #[serde(default)]
    pub keep_translation_when_disabled: bool,
    #[serde(default)]
    pub keep_analysis_when_disabled: bool,
    #[serde(default)]
    pub keep_monitoring_when_disabled: bool,
    #[serde(default)]
    pub keep_export_when_disabled: bool,
    #[serde(default)]
    pub keep_webhook_when_disabled: bool,
    #[serde(default)]
    pub keep_overlay_when_disabled: bool,
    // ── Career-completion webhooks (see webhook.rs) ────────────────────────────────────────────
    #[serde(default)]
    pub webhook: crate::webhook::WebhookConfig,
    // ── Legacy / inheritance analysis (see legacy.rs) ──────────────────────────────────────────
    #[serde(default = "default_true")]
    pub legacy_capture: bool,
    // `predict_hud` / `predict_hud_x` / `predict_hud_y` (the in-game prediction HUD and its
    // placement) and `choice_marks` (a highlight on the choice buttons) lived here and were
    // removed — predictions live on the web Predictions page now. Old settings files still
    // load: serde ignores unknown fields, so there is nothing to migrate.
    // ── Resource budget ────────────────────────────────────────────────────────────────────────
    // Release the resident NLLB model after this many seconds with no translation work (0 = never).
    // The model is by far the largest allocation Overseer holds (~0.7 GB for the 600M build, ~2 GB
    // for the 1.3B), so idling it back is most of the memory-footprint answer.
    #[serde(default = "default_mtl_idle_unload_s")]
    pub mtl_idle_unload_s: u64,
    // Verbose hot-path logging (per-frame skip/translation diagnostics). Off by default: with it on
    // the log rate during a skip flood is high enough to matter even through the buffered writer.
    #[serde(default)]
    pub verbose_log: bool,
    // Load/stall profiler sampling. Was unconditionally ON, writing a CSV row per notable sample and
    // holding an aggregate table; it is a diagnostic, so it is opt-in now.
    #[serde(default)]
    pub profiler: bool,
    pub skip_training: bool,
    pub skip_events: bool,
    #[serde(default = "default_true")]
    pub skip_shop: bool,
    #[serde(default = "default_true")]
    pub skip_rival: bool,
    #[serde(default = "default_true")]
    pub skip_scene_cutt: bool,
    pub race_result: bool,
    // Hyper Skip (experimental): auto-click the in-race Skip button + auto-tap prompts. Off by default.
    #[serde(default)]
    pub hyper_skip: bool,
    // Auto-confirm advisory WARNING dialogs (e.g. "this will be your Nth consecutive race"). Off by default.
    #[serde(default)]
    pub skip_warnings: bool,
    // Auto-skip the "Inspiration" event (tap its GO! button to advance). Off by default.
    #[serde(default)]
    pub skip_inspiration: bool,
    // Skill-learning auto-confirm (Phase 0: DIAGNOSTICS ONLY — arms the discovery logging so one
    // manual purchase identifies the flow; nothing is clicked yet). Off by default.
    #[serde(default)]
    pub skip_skill_learn: bool,
    // Auto-pick the TOP option on every career event choice. Off by default.
    #[serde(default)]
    pub event_auto_choice: bool,
    // Auto-engage the in-race Fast-Forward (accelerated race playback). Off by default.
    #[serde(default)]
    pub race_ff: bool,
    pub fps: i32,
    #[serde(default = "default_ui_tempo")]
    pub ui_tempo: f32,
    pub rail_right: bool,
    pub energy_x: f32,
    pub energy_y: f32,
    pub bonds_only: bool,
    pub tt_capture: bool,
    // Info panels: all OFF by default — the shipped overlay shows only the
    // Controls panel. Users opt in to Career/Race/Energy from the PANELS section.
    #[serde(default)]
    pub show_career: bool,
    #[serde(default)]
    pub show_race: bool,
    #[serde(default)]
    pub show_energy: bool,
    // Menu layout: true = centered floating window (default), false = docked to a screen edge.
    #[serde(default = "default_true")]
    pub menu_centered: bool,
    // Index into the overlay's menu-key list (which key toggles the menu). 0 = Insert.
    #[serde(default)]
    pub toggle_key: u32,
    // Whether the first-launch "press <key> to open" hint has been seen/dismissed.
    #[serde(default)]
    pub seen_hint: bool,
    // Self-updater: the release tag the user chose "don't ask again" on (e.g. "v3.6.2"). The
    // auto-check on boot stays silent while this matches the latest tag; a newer release clears it.
    #[serde(default)]
    pub update_skip: String,
    // Self-updater: the slimmed changelog we last saw for the current version, so a same-tag hotfix
    // (DLL re-uploaded without a version bump) can diff against it and show only the new lines.
    #[serde(default)]
    pub update_seen_changelog: String,
    // Active Overseer glossary language code (es/fr/…) or None. Drives the Localize::Get glossary
    // miss-fallback (instant in-process translation layer). None/"" = off.
    #[serde(default)]
    pub tl_lang: Option<String>,
    // Enable the async NLLB machine-translation fallback for lines the glossary can't cover. Off =
    // glossary + prior mtl.json cache only (zero-dependency). Mirrors Overseer config.json {mtl}.
    #[serde(default)]
    pub mtl_enabled: bool,
    // Use the higher-quality NLLB-1.3B model (<dll_dir>/nllb-1.3b/) instead of the 600M default.
    #[serde(default)]
    pub mtl_hq: bool,
    // User-supplied proper names to keep in the ORIGINAL language (the player's own trainer name and
    // anything else they don't want translated). Merged with the bundled names.json protection.
    #[serde(default)]
    pub protected_names: Vec<String>,
    // Uncap the character cloth/hair (spring-bone) physics update rate (cosmetic).
    #[serde(default)]
    pub cyspring_uncap: bool,
    // Force the highest 3D model quality tier (cosmetic).
    #[serde(default)]
    pub gfx_quality: bool,
    // (gfx_extras was removed: its whole implementation was aniso + lodBias + shadowResolution, and
    // IL2CPP stripped all three setters out of this build, so the toggle could never do anything.
    // An old settings file with the field still loads — serde just ignores it.)
    // Granular graphics knobs (Performance page "Advanced"). -1 / negative = leave the game's value.
    // These write the QualitySettings statics that SURVIVED stripping; see performance::graphics for
    // why that's the entire surface (the game renders on the built-in pipeline, not URP).
    #[serde(default = "neg_one_i32")]
    pub gfx_aa: i32, // URP msaaSampleCount: 0(off)/2/4/8
    #[serde(default = "neg_one_i32")]
    pub gfx_shadows: i32, // 0=off, 1=hard, 2=soft
    #[serde(default = "neg_one_f32")]
    pub gfx_shadow_dist: f32, // QualitySettings.shadowDistance (0 = shadows off). -1 = leave
    // Display / window QoL.
    #[serde(default)]
    pub always_on_top: bool,
    #[serde(default)]
    pub block_minimize: bool,
    #[serde(default)]
    pub display_mode: i32, // 0 off, 1 borderless, 2 exclusive, 3 windowed
    #[serde(default = "default_one_f32")]
    pub render_scale: f32,
    #[serde(default = "default_one_f32")]
    pub ui_scale: f32,
    // Colour-vision (daltonization) accessibility filter: 0 off, 1 deuteranopia, 2 protanopia,
    // 3 tritanopia. A fullscreen D3D11 post-process applied to the game frame (see cvd.rs).
    #[serde(default)]
    pub cvd_mode: i32,
    /// Colour-vision correction strength, 0..1. The filter preserves luminance, so this controls how
    /// far hues are pushed apart — not brightness. Default 0.85.
    #[serde(default = "default_cvd_strength")]
    pub cvd_strength: f32,
    // Low-resources "potato" master mode: minimum everything for very weak PCs.
    #[serde(default)]
    pub low_spec: bool,
    /// Low Resources tier: 1 Balanced, 2 Aggressive, 3 Extreme. See performance::graphics.
    #[serde(default = "default_low_level")]
    pub low_level: i32,
    /// Manual render-scale override while Low Resources is on (0 = use the tier's default).
    #[serde(default)]
    pub low_render_scale: f32,
    // Use the classic "Controls" rail menu instead of the premium sidebar menu.
    #[serde(default)]
    pub classic_menu: bool,
    // Reserved feature toggle (full build only); the field is
    // always present so the JSON stays stable across builds.
    #[serde(default)]
    pub oracle: bool,
    // Race freecam enabled.
    #[serde(default)]
    pub freecam: bool,
    // Broadcast telemetry HUD enabled — INDEPENDENT of freecam: shows all the live data during any
    // race whether or not the freecam is following a Uma. Default ON.
    #[serde(default = "default_true")]
    pub telemetry: bool,
    // Telemetry HUD — modular per-section toggles (default ON = old behavior) + HUD scale.
    // The skill feed is the big one (it grows the window) — `tele_skills` toggles it.
    #[serde(default = "default_true")]
    pub tele_grade: bool,
    #[serde(default = "default_true")]
    pub tele_portrait: bool,
    #[serde(default = "default_true")]
    pub tele_rival: bool,
    #[serde(default = "default_true")]
    pub tele_skills: bool,
    // Broadcast timing tower — the whole field, F1-style. Default ON.
    #[serde(default = "default_true")]
    pub tele_tower: bool,
    // Broadcast panels (all default ON): win-probability, pace trace, battle callout, head marker.
    #[serde(default = "default_true")]
    pub tele_winprob: bool,
    #[serde(default = "default_true")]
    pub tele_pace: bool,
    #[serde(default = "default_true")]
    pub tele_battle: bool,
    #[serde(default = "default_true")]
    pub tele_marker: bool,
    #[serde(default = "default_one_f32")]
    pub tele_scale: f32,
    // Export each race to JSON on disk (grouped by race type) for the web viewer.
    #[serde(default)]
    pub race_export: bool,
    // Export trained "veteran" umas to overseer_umas/veterans.json (Hakuraku format).
    #[serde(default)]
    pub umas_export: bool,
    // CarrotBlender-style companion feed (uma_bridge → companion overlays). Default ON (passive).
    #[serde(default = "default_true")]
    pub companion_bridge: bool,
    // Freecam 3rd-person camera presets PER CIRCUIT (track id → named presets + which is default).
    // Captured/cycled in-race; renamed/managed in the overlay. Persisted forever.
    #[serde(default)]
    pub cam_tracks: std::collections::HashMap<i32, TrackCams>,
    // Per-window saved geometry (key → [x, y, w, h]) so a window the user resizes/moves reopens
    // at that size/position forever.
    #[serde(default)]
    pub win: std::collections::HashMap<String, [f32; 4]>,
    // Race Director / freecam key binds — Win32 VK codes, indexed by RdKey (see overlay). Rebindable
    // in the menu like the menu-open key. Missing/short → defaults fill in.
    #[serde(default = "default_rd_keys")]
    pub rd_keys: Vec<i32>,
}

/// Default Race Director key binds (VK codes), in RdKey order: orbit L/R, zoom in/out, height up/down,
/// prev/next Uma, cycle preset, save preset, toggle first-person.
pub fn default_rd_keys() -> Vec<i32> {
    vec![0x25, 0x27, 0x26, 0x28, 0x49, 0x4B, 0xDB, 0xDD, 0x4F, 0x50, 0x56]
}

/// A named 3rd-person chase pose.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct CamPreset {
    pub name: String,
    pub dist: f32,
    pub yaw: f32,
    pub pitch: f32,
    pub eyeh: f32,
}

/// A circuit's camera presets + which one is the default (index into `presets`).
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct TrackCams {
    pub presets: Vec<CamPreset>,
    pub default_idx: u32,
}

/// Max presets per circuit.
pub const MAX_PRESETS: usize = 4;

fn default_one_f32() -> f32 {
    1.0
}

fn neg_one_i32() -> i32 {
    -1
}

fn neg_one_f32() -> f32 {
    -1.0
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            bot_enabled: true,
            translation_enabled: true,
            keep_translation_when_disabled: false,
            keep_analysis_when_disabled: false,
            keep_monitoring_when_disabled: false,
            keep_export_when_disabled: false,
            keep_webhook_when_disabled: false,
            keep_overlay_when_disabled: false,
            webhook: crate::webhook::WebhookConfig::default(),
            legacy_capture: true,
            mtl_idle_unload_s: default_mtl_idle_unload_s(),
            verbose_log: false,
            profiler: false,
            skip_training: true,
            skip_events: true,
            skip_shop: true,
            skip_rival: true,
            skip_scene_cutt: true,
            // Track the compile-time default: builds with `races_on` (public) default
            // race-result skip ON; otherwise persisted state would force it OFF on a
            // fresh install. Private has no `races_on`, so this stays false there.
            race_result: cfg!(feature = "races_on"),
            hyper_skip: false,
            skip_warnings: false,
            skip_inspiration: false,
            skip_skill_learn: false,
            event_auto_choice: false,
            race_ff: false,
            fps: 0,
            ui_tempo: 1.0,
            rail_right: true,
            energy_x: 60.0,
            energy_y: 60.0,
            bonds_only: false,
            tt_capture: false,
            show_career: false,
            show_race: false,
            show_energy: false,
            menu_centered: true,
            toggle_key: 0,
            seen_hint: false,
            update_skip: String::new(),
            update_seen_changelog: String::new(),
            tl_lang: None,
            mtl_enabled: false,
            mtl_hq: false,
            protected_names: Vec::new(),
            cyspring_uncap: false,
            gfx_quality: false,
            gfx_aa: -1,
            gfx_shadows: -1,
            gfx_shadow_dist: -1.0,
            always_on_top: false,
            block_minimize: false,
            display_mode: 0,
            render_scale: 1.0,
            ui_scale: 1.0,
            cvd_mode: 0,
            cvd_strength: default_cvd_strength(),
            low_spec: false,
            low_level: default_low_level(),
            low_render_scale: 0.0,
            classic_menu: false,
            oracle: false,
            freecam: false,
            telemetry: true,
            tele_grade: true,
            tele_portrait: true,
            tele_rival: true,
            tele_skills: true,
            tele_tower: true,
            tele_winprob: true,
            tele_pace: true,
            tele_battle: true,
            tele_marker: true,
            tele_scale: 1.0,
            cam_tracks: std::collections::HashMap::new(),
            win: std::collections::HashMap::new(),
            race_export: false,
            umas_export: false,
            companion_bridge: true,
            rd_keys: default_rd_keys(),
        }
    }
}

fn default_ui_tempo() -> f32 { 1.0 }

fn default_cvd_strength() -> f32 { 0.85 }

fn default_low_level() -> i32 { 1 }

/// Idle seconds before the NLLB model is released back to the OS. 5 minutes is long enough that a
/// normal reading pace never reloads it, short enough that leaving the game parked doesn't hold a
/// multi-hundred-megabyte model resident.
fn default_mtl_idle_unload_s() -> u64 { 300 }

fn cache() -> &'static Mutex<Settings> {
    static CACHE: OnceLock<Mutex<Settings>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let s = load_file();
        // Seed the lock-free hot-path mirrors from the same load.
        TL_LANG_SET.store(s.tl_lang.as_deref().is_some_and(|l| !l.is_empty()), Ordering::Relaxed);
        TL_LANG.store(std::sync::Arc::new(s.tl_lang.clone()));
        MTL_ON.store(s.mtl_enabled, Ordering::Relaxed);
        BOT_ON.store(s.bot_enabled, Ordering::Relaxed);
        Mutex::new(s)
    })
}

// Lock-free mirrors of the two settings read on the per-set_text hot path (dispatch →
// translation_active/eligible). Reading these avoids taking the settings Mutex — which webui
// handlers hold across disk writes — so a control-panel click can never stall the game's render
// thread inside the set_text detour. Written by the setters below (single writer each).
static TL_LANG: Lazy<ArcSwap<Option<String>>> = Lazy::new(|| ArcSwap::from_pointee(None));
/// Non-allocating mirror of `TL_LANG.is_some()` — see `tl_lang_set()`.
static TL_LANG_SET: AtomicBool = AtomicBool::new(false);
static MTL_ON: AtomicBool = AtomicBool::new(false);
/// Master power mirror — read on the same hot paths (translation dispatch, skip ticks) so a lock is
/// never taken there. Default true; seeded in cache(), written by set_bot_enabled. When false every
/// active behavior short-circuits.
static BOT_ON: AtomicBool = AtomicBool::new(true);

fn load_file() -> Settings {
    std::fs::read_to_string(settings_path())
        .ok()
        // Tolerate a UTF-8 BOM: external editors (PowerShell 5.1, Notepad) prepend one, and
        // serde_json rejects it — silently resetting EVERY setting to defaults. Strip it.
        .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok())
        .unwrap_or_default()
}

fn write_file(s: &Settings) {
    if let Ok(j) = serde_json::to_string_pretty(s) {
        // Atomic write (temp + rename): a crash mid-write must never leave a torn/corrupt
        // settings file — every user preference lives here.
        let path = settings_path();
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, j).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Apply persisted settings at startup. Call after all modules are installed.
pub fn apply_on_boot() {
    let mut s = load_file();
    // Removed features (free camera + Team Trials capture + cloth-physics uncap): keep them OFF
    // regardless of any persisted state, so no stale saved value can re-activate them. The race
    // win-probability/telemetry HUD is the separate `telemetry` setting and is UNAFFECTED.
    s.freecam = false;
    s.tt_capture = false;
    s.cyspring_uncap = false;
    // Verbose logging + profiler are diagnostics: apply them FIRST so everything below logs at the
    // level the user chose (and so the profiler doesn't sample the boot burst when it's off).
    crate::tools::log::set_trace(s.verbose_log);
    crate::loadprof::set_enabled(s.profiler);
    crate::skip::apply(&s);
    crate::performance::apply(&s);
    crate::cvd::set_mode(s.cvd_mode as u8); // restore the accessibility colour filter
    crate::cvd::set_strength(s.cvd_strength);
    crate::ui_tempo::apply(&s);
    crate::htt::apply(&s);
    #[cfg(feature = "freecam")]
    crate::freecam::apply(&s);
    #[cfg(feature = "raceread")]
    crate::race_export::apply(&s);
    crate::friendlyplugins::apply(&s);
    crate::umas::apply(&s);
    crate::mtl::set_user_names(s.protected_names.clone()); // keep the player's own name untranslated
    crate::webhook::apply(&s);
    // Runtime power state LAST: its fan-out starts/stops the background workers, so every module
    // above must already hold its persisted configuration when it runs.
    crate::runtime::apply(&s);
    TL_LANG_SET.store(s.tl_lang.as_deref().is_some_and(|l| !l.is_empty()), Ordering::Relaxed);
    if let Ok(mut c) = cache().lock() {
        *c = s;
    }
    APPLIED.store(true, Ordering::Relaxed);
}

/// Team Trials in-process capture toggle (WinHTTP tap).
pub fn tt_capture() -> bool {
    cache().lock().map(|c| c.tt_capture).unwrap_or(false)
}
pub fn set_tt_capture(on: bool) {
    htt::set_enabled(on);
    if let Ok(mut c) = cache().lock() {
        c.tt_capture = on;
        write_file(&c);
    }
}

/// Self-updater: the release tag the user silenced ("don't ask again"), or "" if none.
pub fn update_skip() -> String {
    cache().lock().map(|c| c.update_skip.clone()).unwrap_or_default()
}
pub fn set_update_skip(tag: &str) {
    if let Ok(mut c) = cache().lock() {
        c.update_skip = tag.to_string();
        write_file(&c);
    }
}

/// Self-updater: the slimmed changelog last shown for the current version (hotfix diff baseline).
pub fn update_seen_changelog() -> String {
    cache().lock().map(|c| c.update_seen_changelog.clone()).unwrap_or_default()
}
pub fn set_update_seen_changelog(notes: &str) {
    if let Ok(mut c) = cache().lock() {
        if c.update_seen_changelog != notes {
            c.update_seen_changelog = notes.to_string();
            write_file(&c);
        }
    }
}

/// Persist current module state (Training/Events/Races/FPS), preserving the
/// rail side. Call from the overlay whenever one of those controls changes.
pub fn save_current() {
    // Guard against the boot-window clobber: before apply_on_boot has run, the live fps/
    // ui_tempo modules hold defaults, and writing them would wipe the saved values.
    if !APPLIED.load(Ordering::Relaxed) {
        return;
    }
    if let Ok(mut c) = cache().lock() {
        // RAW values only. The gated is_* getters AND in the master switch, so persisting them while
        // the bot was OFF wrote `false` over every checkbox (and 1.0 over the tempo) — the user's
        // saved choices were silently destroyed by any save during that window. Persist what the
        // user CHOSE; the master switch gates behavior, never storage.
        c.skip_training = skip::raw_train_enabled();
        c.skip_events = skip::raw_event_enabled();
        c.skip_shop = skip::raw_shop_enabled();
        c.skip_rival = skip::raw_rival_enabled();
        // (skip_scene_cutt intentionally untouched: scene skip has no implementation — its setter is
        // a no-op and its getter hardcodes false, so reading it back can only ever erase the field.)
        c.race_result = skip::raw_race_result_enabled();
        c.race_ff = skip::raw_race_ff_enabled();
        c.hyper_skip = skip::raw_hyper_enabled();
        c.skip_skill_learn = skip::raw_skill_learn_enabled();
        // These three were MISSING here — the checkboxes silently reset to their defaults on every
        // game restart because the web toggle updated the live flag but never the persisted file.
        c.skip_warnings = skip::raw_warnings_enabled();
        c.skip_inspiration = skip::raw_inspiration_enabled();
        c.event_auto_choice = skip::raw_event_choice_enabled();
        c.fps = fps::current();
        c.ui_tempo = crate::ui_tempo::stored_tempo();
        write_file(&c);
    }
}

/// Active Overseer glossary language (es/fr/…) or None (glossary translation off). Lock-free (hot
/// path): reads the mirror, not the settings Mutex.
pub fn tl_lang() -> Option<String> {
    cache(); // ensure the mirror is seeded
    TL_LANG.load().as_ref().clone()
}

/// Is a target language selected? Same question as `tl_lang().is_some()` but WITHOUT the clone —
/// the clone is a heap allocation, and this is asked once per on-screen string per frame from the
/// set_text hot path. Mirrors into a plain bool so the common case is a single relaxed load.
#[inline]
pub fn tl_lang_set() -> bool {
    cache(); // ensure the mirror is seeded
    TL_LANG_SET.load(Ordering::Relaxed)
}
pub fn set_tl_lang(lang: Option<String>) {
    let lang = lang.filter(|s| !s.is_empty());
    let snapshot = {
        let mut c = match cache().lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        c.tl_lang = lang.clone();
        c.clone()
    }; // drop the lock BEFORE disk I/O so the hot path never waits on a write
    TL_LANG_SET.store(lang.is_some(), Ordering::Relaxed);
    TL_LANG.store(std::sync::Arc::new(lang));
    crate::loc_settext::clear_uf_cache(); // language change → drop the per-component classification
    write_file(&snapshot);
}

/// Master power switch: is the bot allowed to act at all? Lock-free (read on skip/translation hot
/// paths). When false, all active behaviors are suppressed while the individual toggles keep their
/// values. Default true.
pub fn bot_enabled() -> bool {
    cache(); // ensure the mirror is seeded
    BOT_ON.load(Ordering::Relaxed)
}
pub fn set_bot_enabled(on: bool) {
    let snapshot = {
        let mut c = match cache().lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        c.bot_enabled = on;
        c.clone()
    };
    BOT_ON.store(on, Ordering::Relaxed);
    // Drive the runtime gate too — this is what actually suspends/resumes the background workers,
    // timers and the resident translation model rather than just muting the hooks.
    crate::runtime::set_overseer_enabled(on);
    write_file(&snapshot);
}

/// Live-translation switch, INDEPENDENT of the master switch above.
pub fn translation_enabled() -> bool {
    cache();
    crate::runtime::translation_enabled()
}
pub fn set_translation_enabled(on: bool) {
    let snapshot = {
        let mut c = match cache().lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        c.translation_enabled = on;
        c.clone()
    };
    crate::runtime::set_translation_enabled(on);
    write_file(&snapshot);
}

/// Per-subsystem "keep running while Overseer is disabled" overrides. Each writes through to the
/// runtime gate (live) and to disk (persisted).
macro_rules! keep_setting {
    ($get:ident, $set:ident, $field:ident, $rt_set:path) => {
        pub fn $get() -> bool {
            cache().lock().map(|c| c.$field).unwrap_or(false)
        }
        pub fn $set(on: bool) {
            let snapshot = {
                let mut c = match cache().lock() {
                    Ok(c) => c,
                    Err(_) => return,
                };
                c.$field = on;
                c.clone()
            };
            $rt_set(on);
            write_file(&snapshot);
        }
    };
}
keep_setting!(keep_translation, set_keep_translation, keep_translation_when_disabled, crate::runtime::set_keep_translation);
keep_setting!(keep_analysis, set_keep_analysis, keep_analysis_when_disabled, crate::runtime::set_keep_analysis);
keep_setting!(keep_monitoring, set_keep_monitoring, keep_monitoring_when_disabled, crate::runtime::set_keep_monitoring);
keep_setting!(keep_export, set_keep_export, keep_export_when_disabled, crate::runtime::set_keep_export);
keep_setting!(keep_webhook, set_keep_webhook, keep_webhook_when_disabled, crate::runtime::set_keep_webhook);
keep_setting!(keep_overlay, set_keep_overlay, keep_overlay_when_disabled, crate::runtime::set_keep_overlay);

/// Verbose hot-path logging (per-frame skip/translation diagnostics).
pub fn verbose_log() -> bool {
    cache().lock().map(|c| c.verbose_log).unwrap_or(false)
}
pub fn set_verbose_log(on: bool) {
    crate::tools::log::set_trace(on);
    if let Ok(mut c) = cache().lock() {
        c.verbose_log = on;
        write_file(&c);
    }
}

/// Load/stall profiler sampling (diagnostic; opt-in).
pub fn profiler() -> bool {
    cache().lock().map(|c| c.profiler).unwrap_or(false)
}
pub fn set_profiler(on: bool) {
    crate::loadprof::set_enabled(on);
    if let Ok(mut c) = cache().lock() {
        c.profiler = on;
        write_file(&c);
    }
}

/// Seconds of translation idleness before the NLLB model is released (0 = keep it resident).
pub fn mtl_idle_unload_s() -> u64 {
    cache().lock().map(|c| c.mtl_idle_unload_s).unwrap_or(300)
}
pub fn set_mtl_idle_unload_s(v: u64) {
    if let Ok(mut c) = cache().lock() {
        c.mtl_idle_unload_s = v.min(24 * 3600);
        write_file(&c);
    }
}

/// Whether the Legacy Select affinity capture feeds the inheritance analyser.
pub fn legacy_capture() -> bool {
    cache().lock().map(|c| c.legacy_capture).unwrap_or(true)
}
pub fn set_legacy_capture(on: bool) {
    if let Ok(mut c) = cache().lock() {
        c.legacy_capture = on;
        write_file(&c);
    }
}


/// Current webhook configuration (clone).
pub fn webhook_config() -> crate::webhook::WebhookConfig {
    cache().lock().map(|c| c.webhook.clone()).unwrap_or_default()
}
/// Replace + persist the webhook configuration, pushing it live.
pub fn set_webhook_config(cfg: crate::webhook::WebhookConfig) {
    let snapshot = {
        let mut c = match cache().lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        c.webhook = cfg;
        c.clone()
    };
    crate::webhook::apply(&snapshot);
    write_file(&snapshot);
}

/// Whether the async NLLB machine-translation fallback is enabled. Lock-free (hot path).
pub fn mtl_enabled() -> bool {
    cache(); // ensure the mirror is seeded
    MTL_ON.load(Ordering::Relaxed)
}
pub fn set_mtl_enabled(on: bool) {
    let snapshot = {
        let mut c = match cache().lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        c.mtl_enabled = on;
        c.clone()
    };
    MTL_ON.store(on, Ordering::Relaxed);
    write_file(&snapshot);
}

/// User-protected names (the player's trainer name etc. — kept in the original language).
pub fn protected_names() -> Vec<String> {
    cache().lock().map(|c| c.protected_names.clone()).unwrap_or_default()
}
pub fn set_protected_names(list: Vec<String>) {
    crate::mtl::set_user_names(list.clone()); // apply live (takes effect on the next set_text)
    if let Ok(mut c) = cache().lock() {
        c.protected_names = list;
        write_file(&c);
    }
}

/// Whether the high-quality NLLB-1.3B model is selected (vs. the 600M default).
pub fn mtl_hq() -> bool {
    cache().lock().map(|c| c.mtl_hq).unwrap_or(false)
}
pub fn set_mtl_hq(on: bool) {
    let snapshot = {
        let mut c = match cache().lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        c.mtl_hq = on;
        c.clone()
    };
    write_file(&snapshot);
}

/// Which edge the rail is docked to (true = right, false = left).
pub fn rail_right() -> bool {
    cache().lock().map(|c| c.rail_right).unwrap_or(true)
}

/// Flip / set the rail side and persist it.
pub fn set_rail_right(right: bool) {
    if let Ok(mut c) = cache().lock() {
        c.rail_right = right;
        write_file(&c);
    }
}

/// Menu layout: true = centered floating window, false = docked to an edge.
pub fn menu_centered() -> bool {
    cache().lock().map(|c| c.menu_centered).unwrap_or(true)
}

pub fn set_menu_centered(on: bool) {
    if let Ok(mut c) = cache().lock() {
        c.menu_centered = on;
        write_file(&c);
    }
}

/// Index of the key that opens/closes the overlay menu (see overlay's MENU_KEYS).
pub fn toggle_key() -> u32 {
    cache().lock().map(|c| c.toggle_key).unwrap_or(0)
}

pub fn set_toggle_key(idx: u32) {
    if let Ok(mut c) = cache().lock() {
        c.toggle_key = idx;
        write_file(&c);
    }
}

/// Race Director / freecam key bind (Win32 VK code) for action `i` (RdKey order). Falls back to the
/// built-in default if the saved list is missing/short.
pub fn rd_key(i: usize) -> i32 {
    cache()
        .lock()
        .ok()
        .and_then(|c| c.rd_keys.get(i).copied())
        .unwrap_or_else(|| default_rd_keys().get(i).copied().unwrap_or(0))
}

/// Rebind Race Director action `i` to VK code `vk` (persisted).
pub fn set_rd_key(i: usize, vk: i32) {
    if let Ok(mut c) = cache().lock() {
        let def = default_rd_keys();
        if c.rd_keys.len() < def.len() {
            c.rd_keys = def; // backfill an old/short saved list before editing
        }
        if let Some(slot) = c.rd_keys.get_mut(i) {
            *slot = vk;
        }
        write_file(&c);
    }
}

/// Whether the first-launch "press <key> to open the menu" hint has been seen.
pub fn seen_hint() -> bool {
    cache().lock().map(|c| c.seen_hint).unwrap_or(false)
}
pub fn set_seen_hint(on: bool) {
    if let Ok(mut c) = cache().lock() {
        c.seen_hint = on;
        write_file(&c);
    }
}

/// Whether the cloth/hair (spring-bone) physics update rate is uncapped.
pub fn cyspring_uncap() -> bool {
    cache().lock().map(|c| c.cyspring_uncap).unwrap_or(false)
}

pub fn set_cyspring_uncap(on: bool) {
    crate::performance::cyspring::set_enabled(on);
    if let Ok(mut c) = cache().lock() {
        c.cyspring_uncap = on;
        write_file(&c);
    }
}

/// Force the highest 3D model quality tier.
pub fn gfx_quality() -> bool {
    cache().lock().map(|c| c.gfx_quality).unwrap_or(false)
}

pub fn set_gfx_quality(on: bool) {
    crate::performance::graphics::set_quality_unlocked(on);
    if let Ok(mut c) = cache().lock() {
        c.gfx_quality = on;
        write_file(&c);
    }
}

// ── granular graphics knobs (each pushes live to the graphics module + persists) ──
pub fn gfx_aa() -> i32 { cache().lock().map(|c| c.gfx_aa).unwrap_or(-1) }
pub fn set_gfx_aa(v: i32) {
    crate::performance::graphics::set_aa(v);
    if let Ok(mut c) = cache().lock() { c.gfx_aa = v; write_file(&c); }
}
pub fn gfx_shadows() -> i32 { cache().lock().map(|c| c.gfx_shadows).unwrap_or(-1) }
pub fn set_gfx_shadows(v: i32) {
    crate::performance::graphics::set_shadows(v);
    if let Ok(mut c) = cache().lock() { c.gfx_shadows = v; write_file(&c); }
}
pub fn gfx_shadow_dist() -> f32 { cache().lock().map(|c| c.gfx_shadow_dist).unwrap_or(-1.0) }
pub fn set_gfx_shadow_dist(v: f32) {
    crate::performance::graphics::set_shadow_distance(v);
    if let Ok(mut c) = cache().lock() { c.gfx_shadow_dist = v; write_file(&c); }
}

// ── Display / window QoL ──
pub fn always_on_top() -> bool {
    cache().lock().map(|c| c.always_on_top).unwrap_or(false)
}
pub fn set_always_on_top(on: bool) {
    crate::performance::display::set_always_on_top(on);
    if let Ok(mut c) = cache().lock() {
        c.always_on_top = on;
        write_file(&c);
    }
}

pub fn block_minimize() -> bool {
    cache().lock().map(|c| c.block_minimize).unwrap_or(false)
}
pub fn set_block_minimize(on: bool) {
    crate::performance::display::set_block_minimize(on);
    if let Ok(mut c) = cache().lock() {
        c.block_minimize = on;
        write_file(&c);
    }
}

pub fn display_mode() -> i32 {
    cache().lock().map(|c| c.display_mode).unwrap_or(0)
}
pub fn set_display_mode(m: i32) {
    crate::performance::display::set_display_mode(m);
    if let Ok(mut c) = cache().lock() {
        c.display_mode = m;
        write_file(&c);
    }
}

pub fn render_scale() -> f32 {
    cache().lock().map(|c| c.render_scale).unwrap_or(1.0)
}
pub fn set_render_scale(s: f32) {
    crate::performance::display::set_render_scale(s);
    if let Ok(mut c) = cache().lock() {
        c.render_scale = s;
        write_file(&c);
    }
}

pub fn ui_scale() -> f32 {
    cache().lock().map(|c| c.ui_scale).unwrap_or(1.0)
}
pub fn set_ui_scale(s: f32) {
    crate::performance::display::set_ui_scale(s);
    if let Ok(mut c) = cache().lock() {
        c.ui_scale = s;
        write_file(&c);
    }
}

/// Colour-vision (daltonization) accessibility filter mode: 0 off, 1 deuter, 2 protan, 3 tritan.
pub fn cvd_mode() -> i32 {
    cache().lock().map(|c| c.cvd_mode).unwrap_or(0)
}
/// Colour-vision correction strength (0..1).
pub fn cvd_strength() -> f32 {
    cache().lock().map(|c| c.cvd_strength).unwrap_or(0.85)
}
pub fn set_cvd_strength(v: f32) {
    let v = v.clamp(0.0, 1.0);
    crate::cvd::set_strength(v);
    if let Ok(mut c) = cache().lock() {
        c.cvd_strength = v;
        write_file(&c);
    }
}

/// Set the CVD filter mode — pushes live to the render-thread filter AND persists. Clamped 0..=3.
pub fn set_cvd_mode(v: i32) {
    let v = v.clamp(0, 3);
    crate::cvd::set_mode(v as u8);
    if let Ok(mut c) = cache().lock() {
        c.cvd_mode = v;
        write_file(&c);
    }
}

/// Low-resources "potato" master mode (overrides the graphics enhancements).
pub fn low_spec() -> bool {
    cache().lock().map(|c| c.low_spec).unwrap_or(false)
}
pub fn set_low_spec(on: bool) {
    crate::performance::set_low_spec(on);
    if let Ok(mut c) = cache().lock() {
        c.low_spec = on;
        write_file(&c);
    }
}

// NOTE: `low_render_scale` had a getter/setter pair here driving a slider on the Performance page.
// Both are gone — `display::set_render_scale` has been a deliberate no-op since 2026-07-17, so the
// slider moved a number that reached nothing. The struct field is kept (with a serde default) purely
// so existing settings files still load; nothing reads it.

/// Low Resources tier (1..3).
pub fn low_level() -> i32 {
    // Clamped on READ as well as write: a settings file saved by an earlier build can hold 4, from a
    // short-lived "Minimum" tier that has since been removed for doing nothing. Clamping here means
    // such a file lands on Extreme instead of showing no tier selected at all in the UI.
    cache().lock().map(|c| c.low_level.clamp(1, 3)).unwrap_or(1)
}
pub fn set_low_level(v: i32) {
    let v = v.clamp(1, 3);
    crate::performance::set_low_level(v);
    if let Ok(mut c) = cache().lock() {
        c.low_level = v;
        write_file(&c);
    }
}

/// Use the classic "Controls" rail menu instead of the premium sidebar menu.
pub fn classic_menu() -> bool {
    cache().lock().map(|c| c.classic_menu).unwrap_or(false)
}
pub fn set_classic_menu(on: bool) {
    if let Ok(mut c) = cache().lock() {
        c.classic_menu = on;
        write_file(&c);
    }
}

/// "Bonds only" view mode for the info panel (hide stats/aptitudes).
pub fn bonds_only() -> bool {
    cache().lock().map(|c| c.bonds_only).unwrap_or(false)
}
pub fn set_bonds_only(on: bool) {
    if let Ok(mut c) = cache().lock() {
        c.bonds_only = on;
        write_file(&c);
    }
}

/// Whether the info panel window is shown at all.
pub fn show_career() -> bool {
    cache().lock().map(|c| c.show_career).unwrap_or(false)
}
pub fn set_show_career(on: bool) {
    if let Ok(mut c) = cache().lock() {
        c.show_career = on;
        write_file(&c);
    }
}

/// Whether the live Race panel is shown (it still self-hides when no race is active).
pub fn show_race() -> bool {
    cache().lock().map(|c| c.show_race).unwrap_or(false)
}
pub fn set_show_race(on: bool) {
    if let Ok(mut c) = cache().lock() {
        c.show_race = on;
        write_file(&c);
    }
}

/// Whether the floating info chip is shown.
pub fn show_energy() -> bool {
    cache().lock().map(|c| c.show_energy).unwrap_or(false)
}
pub fn set_show_energy(on: bool) {
    if let Ok(mut c) = cache().lock() {
        c.show_energy = on;
        write_file(&c);
    }
}

/// Reserved feature toggle, persisted. Module call is full-build only.
pub fn oracle() -> bool {
    cache().lock().map(|c| c.oracle).unwrap_or(false)
}
pub fn set_oracle(on: bool) {
    if let Ok(mut c) = cache().lock() {
        c.oracle = on;
        write_file(&c);
    }
}

/// The PERSISTED free-camera preference, read straight from disk and NOT subject to the
/// force-off that `apply_on_boot` applies. `freecam::install` uses this to decide whether to arm
/// its engine-wide transform/camera detours: those cost frame time on every object the game moves,
/// so they must not be armed for a feature the user has switched off. Read once at boot.
pub fn freecam_pref() -> bool {
    static PREF: OnceLock<bool> = OnceLock::new();
    *PREF.get_or_init(|| load_file().freecam)
}

/// Race freecam enabled + persisted. Module call is freecam-build only. Suppressed while the master
/// switch is off (the saved pref is preserved and returns when the bot is turned back on).
pub fn freecam() -> bool {
    bot_enabled() && cache().lock().map(|c| c.freecam).unwrap_or(false)
}
pub fn set_freecam(on: bool) {
    #[cfg(feature = "freecam")]
    crate::freecam::set_enabled(on);
    if let Ok(mut c) = cache().lock() {
        c.freecam = on;
        write_file(&c);
    }
}

// ── Telemetry HUD — modular section toggles + scale + presets ────────────────
/// Master telemetry-HUD toggle (independent of freecam).
pub fn telemetry() -> bool { cache().lock().map(|c| c.telemetry).unwrap_or(true) }
pub fn set_telemetry(on: bool) {
    if let Ok(mut c) = cache().lock() { c.telemetry = on; write_file(&c); }
}
pub fn tele_grade() -> bool { cache().lock().map(|c| c.tele_grade).unwrap_or(true) }
pub fn tele_portrait() -> bool { cache().lock().map(|c| c.tele_portrait).unwrap_or(true) }
pub fn tele_rival() -> bool { cache().lock().map(|c| c.tele_rival).unwrap_or(true) }
pub fn tele_skills() -> bool { cache().lock().map(|c| c.tele_skills).unwrap_or(true) }
pub fn tele_tower() -> bool { cache().lock().map(|c| c.tele_tower).unwrap_or(true) }
pub fn tele_winprob() -> bool { cache().lock().map(|c| c.tele_winprob).unwrap_or(true) }
pub fn tele_pace() -> bool { cache().lock().map(|c| c.tele_pace).unwrap_or(true) }
pub fn tele_battle() -> bool { cache().lock().map(|c| c.tele_battle).unwrap_or(true) }
pub fn tele_marker() -> bool { cache().lock().map(|c| c.tele_marker).unwrap_or(true) }
pub fn tele_scale() -> f32 { cache().lock().map(|c| c.tele_scale).unwrap_or(1.0) }
pub fn set_tele_grade(v: bool) { if let Ok(mut c) = cache().lock() { c.tele_grade = v; write_file(&c); } }
pub fn set_tele_portrait(v: bool) { if let Ok(mut c) = cache().lock() { c.tele_portrait = v; write_file(&c); } }
pub fn set_tele_rival(v: bool) { if let Ok(mut c) = cache().lock() { c.tele_rival = v; write_file(&c); } }
pub fn set_tele_skills(v: bool) { if let Ok(mut c) = cache().lock() { c.tele_skills = v; write_file(&c); } }
pub fn set_tele_tower(v: bool) { if let Ok(mut c) = cache().lock() { c.tele_tower = v; write_file(&c); } }
pub fn set_tele_winprob(v: bool) { if let Ok(mut c) = cache().lock() { c.tele_winprob = v; write_file(&c); } }
pub fn set_tele_pace(v: bool) { if let Ok(mut c) = cache().lock() { c.tele_pace = v; write_file(&c); } }
pub fn set_tele_battle(v: bool) { if let Ok(mut c) = cache().lock() { c.tele_battle = v; write_file(&c); } }
pub fn set_tele_marker(v: bool) { if let Ok(mut c) = cache().lock() { c.tele_marker = v; write_file(&c); } }
pub fn set_tele_scale(v: f32) { if let Ok(mut c) = cache().lock() { c.tele_scale = v.clamp(0.6, 2.0); write_file(&c); } }
/// "Broadcast" preset — clean for showing/recording: keep the broadcast panels (tower/win/pace/
/// callout/marker), trim the busy per-Uma extras (rival deltas + skill feed).
pub fn tele_preset_broadcast() {
    if let Ok(mut c) = cache().lock() {
        c.telemetry = true; // a preset implies you want the HUD on
        c.tele_grade = true; c.tele_portrait = true; c.tele_rival = false; c.tele_skills = false;
        c.tele_tower = true; c.tele_winprob = true; c.tele_pace = true; c.tele_battle = true; c.tele_marker = true;
        write_file(&c);
    }
}
/// "Full" preset — every section on.
pub fn tele_preset_full() {
    if let Ok(mut c) = cache().lock() {
        c.telemetry = true;
        c.tele_grade = true; c.tele_portrait = true; c.tele_rival = true; c.tele_skills = true;
        c.tele_tower = true; c.tele_winprob = true; c.tele_pace = true; c.tele_battle = true; c.tele_marker = true;
        write_file(&c);
    }
}

/// Export each race to JSON on disk (grouped by race type) for the web viewer.
pub fn race_export() -> bool {
    cache().lock().map(|c| c.race_export).unwrap_or(false)
}
pub fn set_race_export(on: bool) {
    #[cfg(feature = "raceread")]
    crate::race_export::set_enabled(on);
    if let Ok(mut c) = cache().lock() {
        c.race_export = on;
        write_file(&c);
    }
}

/// Export trained veteran umas to overseer_umas/veterans.json (Hakuraku format).
pub fn umas_export() -> bool {
    cache().lock().map(|c| c.umas_export).unwrap_or(false)
}
pub fn set_umas_export(on: bool) {
    crate::umas::set_enabled(on);
    if let Ok(mut c) = cache().lock() {
        c.umas_export = on;
        write_file(&c);
    }
}



/// A circuit's camera presets (clone). Empty if none saved.
pub fn cam_presets(track: i32) -> Vec<CamPreset> {
    cache().lock().ok().and_then(|c| c.cam_tracks.get(&track).map(|t| t.presets.clone())).unwrap_or_default()
}

/// The circuit's default preset index (clamped to a valid preset, or 0).
pub fn cam_default_idx(track: i32) -> usize {
    cache()
        .lock()
        .ok()
        .and_then(|c| c.cam_tracks.get(&track).map(|t| (t.default_idx as usize).min(t.presets.len().saturating_sub(1))))
        .unwrap_or(0)
}

/// The pose of the circuit's default preset: Some((dist,yaw,pitch,eyeH)) if any preset exists.
pub fn cam_default_pose(track: i32) -> Option<(f32, f32, f32, f32)> {
    cache().lock().ok().and_then(|c| {
        c.cam_tracks.get(&track).and_then(|t| {
            t.presets.get((t.default_idx as usize).min(t.presets.len().saturating_sub(1)))
                .map(|p| (p.dist, p.yaw, p.pitch, p.eyeh))
        })
    })
}

/// Add a new preset to a circuit (capped at MAX_PRESETS). Returns its index, or None if full.
pub fn cam_add_preset(track: i32, name: &str, dist: f32, yaw: f32, pitch: f32, eyeh: f32) -> Option<usize> {
    if let Ok(mut c) = cache().lock() {
        let t = c.cam_tracks.entry(track).or_default();
        if t.presets.len() >= MAX_PRESETS {
            return None;
        }
        t.presets.push(CamPreset { name: name.to_string(), dist, yaw, pitch, eyeh });
        let idx = t.presets.len() - 1;
        write_file(&c);
        return Some(idx);
    }
    None
}

/// Overwrite an existing preset's pose (keeps its name).
pub fn cam_update_preset(track: i32, idx: usize, dist: f32, yaw: f32, pitch: f32, eyeh: f32) {
    if let Ok(mut c) = cache().lock() {
        if let Some(p) = c.cam_tracks.get_mut(&track).and_then(|t| t.presets.get_mut(idx)) {
            p.dist = dist; p.yaw = yaw; p.pitch = pitch; p.eyeh = eyeh;
            write_file(&c);
        }
    }
}

pub fn cam_rename_preset(track: i32, idx: usize, name: &str) {
    if let Ok(mut c) = cache().lock() {
        if let Some(p) = c.cam_tracks.get_mut(&track).and_then(|t| t.presets.get_mut(idx)) {
            p.name = name.to_string();
            write_file(&c);
        }
    }
}

pub fn cam_delete_preset(track: i32, idx: usize) {
    if let Ok(mut c) = cache().lock() {
        if let Some(t) = c.cam_tracks.get_mut(&track) {
            if idx < t.presets.len() {
                t.presets.remove(idx);
                if t.default_idx as usize >= t.presets.len() {
                    t.default_idx = 0;
                }
                write_file(&c);
            }
        }
    }
}

pub fn cam_set_default(track: i32, idx: usize) {
    if let Ok(mut c) = cache().lock() {
        if let Some(t) = c.cam_tracks.get_mut(&track) {
            if idx < t.presets.len() {
                t.default_idx = idx as u32;
                write_file(&c);
            }
        }
    }
}

/// Saved geometry [x,y,w,h] for a window key, if the user has moved/resized it.
pub fn win_rect(key: &str) -> Option<[f32; 4]> {
    cache().lock().ok().and_then(|c| c.win.get(key).copied())
}
/// Persist a window's geometry (only writes when it actually changed, to limit disk writes).
pub fn set_win_rect(key: &str, rect: [f32; 4]) {
    if let Ok(mut c) = cache().lock() {
        let changed = c.win.get(key).map(|r| {
            (r[0] - rect[0]).abs() > 1.0 || (r[1] - rect[1]).abs() > 1.0
                || (r[2] - rect[2]).abs() > 1.0 || (r[3] - rect[3]).abs() > 1.0
        }).unwrap_or(true);
        if changed {
            c.win.insert(key.to_string(), rect);
            write_file(&c);
        }
    }
}

/// Saved screen position of the floating info chip.
pub fn energy_pos() -> (f32, f32) {
    cache().lock().map(|c| (c.energy_x, c.energy_y)).unwrap_or((60.0, 60.0))
}

/// Persist the info chip position (called when the user finishes dragging it).
pub fn set_energy_pos(x: f32, y: f32) {
    if let Ok(mut c) = cache().lock() {
        c.energy_x = x;
        c.energy_y = y;
        write_file(&c);
    }
}
