//! Runtime power state — what Overseer is allowed to DO right now.
//!
//! Before this module there was a single `bot_enabled` flag, and only four things consulted it
//! (the skips, the UI tempo, the free camera, and the translation dispatch). Everything else —
//! the response-hook parsers, career tracking, the companion feed, the Team Trials capture, the
//! opponent hunter, the race reader, the exporters, the MTL worker + NLLB model, the advisor
//! sidecar, the profiler — kept running regardless. Turning Overseer "off" therefore only ever
//! stopped live translation, which is exactly the reported bug.
//!
//! The model here is two INDEPENDENT switches plus per-subsystem opt-outs:
//!
//! * **Overseer enabled** (master). Off = every subsystem below is suspended: no hook does work, no
//!   background worker runs, no timer ticks, no overlay update, no analysis. Hooks stay *installed*
//!   (uninstalling a detour mid-session is a crash risk, and re-installing needs the boot thread's
//!   IL2CPP attachment) but every one of them short-circuits to the original in its first branch,
//!   which is what "suspended" can safely mean inside a live process.
//! * **Translation enabled**. Independent: you can run Overseer with translation off, or leave
//!   translation running while Overseer itself is off (opt-in, see `keep_translation`).
//!
//! `Subsystem` names each gate so a caller reads as `runtime::active(Subsystem::Skip)`, and the
//! `keep_*` overrides express "unless explicitly configured otherwise" from the spec.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Everything that can be independently suspended. Each variant maps to one gate below.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Subsystem {
    /// SuperSkip legs, auto-press, auto-choice, race fast-forward, hyper skip.
    Skip,
    /// UI tempo (DOTween sub-stepping).
    Tempo,
    /// Glossary + MTL text substitution on the game's text setters / Localize.Get.
    Translation,
    /// The NLLB worker thread + the resident model.
    TranslationEngine,
    /// Network-response parsing: career state, account resources, race identity, sparks.
    Analysis,
    /// The optional Python advisor sidecar (Live Advisor).
    Advisor,
    /// Team Trials capture, opponent hunter, deck padder.
    Monitoring,
    /// Race reader + telemetry + free camera.
    Race,
    /// On-disk exporters (races, veterans) and the companion feed.
    Export,
    /// Career-completion webhooks.
    Webhook,
    /// Overlay drawing (the in-game FPS counter) and overlay-driven state.
    Overlay,
    /// The load/stall profiler's sampling.
    Profiler,
}

// ── the two independent master switches ─────────────────────────────────────────────────────────
// Both default ON and are mirrored from settings.rs (which owns persistence). Reads are relaxed
// atomics so any hot path can consult them without touching the settings mutex.
static OVERSEER_ON: AtomicBool = AtomicBool::new(true);
static TRANSLATION_ON: AtomicBool = AtomicBool::new(true);

// ── "unless explicitly configured otherwise" overrides ──────────────────────────────────────────
// Each of these lets one subsystem keep running while the master switch is OFF. All default false:
// disabling Overseer suspends everything until the user opts a piece back in.
static KEEP_TRANSLATION: AtomicBool = AtomicBool::new(false);
static KEEP_ANALYSIS: AtomicBool = AtomicBool::new(false);
static KEEP_MONITORING: AtomicBool = AtomicBool::new(false);
static KEEP_EXPORT: AtomicBool = AtomicBool::new(false);
static KEEP_WEBHOOK: AtomicBool = AtomicBool::new(false);
static KEEP_OVERLAY: AtomicBool = AtomicBool::new(false);

/// Monotonic counter bumped whenever any switch changes, so pollers can cheaply notice a
/// transition without diffing every flag (used by the suspend/resume fan-out).
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// Wall-clock ms of the last transition — the resume path uses it to re-arm time-windowed state.
static LAST_CHANGE_MS: AtomicU64 = AtomicU64::new(0);

fn bump() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
    LAST_CHANGE_MS.store(crate::tools::now_ms(), Ordering::Relaxed);
}

/// Generation counter (changes on every switch flip).
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// Process-clock ms at the last switch flip (0 = never).
pub fn last_change_ms() -> u64 {
    LAST_CHANGE_MS.load(Ordering::Relaxed)
}

// ── master switch ───────────────────────────────────────────────────────────────────────────────

/// Is Overseer as a whole enabled? This is the user-facing "Overseer ON/OFF" pill.
#[inline]
pub fn overseer_enabled() -> bool {
    OVERSEER_ON.load(Ordering::Relaxed)
}

/// Flip the master switch. Fans out to the subsystems that need an explicit stop/start (worker
/// threads, resident models, timers) — the rest simply start returning early from `active()`.
pub fn set_overseer_enabled(on: bool) {
    if OVERSEER_ON.swap(on, Ordering::Relaxed) == on {
        return;
    }
    bump();
    crate::tools::log(if on {
        "[runtime] Overseer ENABLED — all subsystems resuming"
    } else {
        "[runtime] Overseer DISABLED — suspending all subsystems (translation, analysis, monitoring, timers, workers)"
    });
    fan_out();
}

/// Is live translation enabled? INDEPENDENT of the master switch except that the master switch
/// suppresses it unless `keep_translation` is set.
#[inline]
pub fn translation_enabled() -> bool {
    TRANSLATION_ON.load(Ordering::Relaxed)
}

/// Flip the translation switch (does not touch the master switch).
pub fn set_translation_enabled(on: bool) {
    if TRANSLATION_ON.swap(on, Ordering::Relaxed) == on {
        return;
    }
    bump();
    crate::tools::log(if on {
        "[runtime] translation ENABLED"
    } else {
        "[runtime] translation DISABLED (Overseer itself is unaffected)"
    });
    fan_out();
}

// ── per-subsystem overrides ─────────────────────────────────────────────────────────────────────

macro_rules! keep_flag {
    ($get:ident, $set:ident, $cell:ident, $label:literal) => {
        pub fn $get() -> bool {
            $cell.load(Ordering::Relaxed)
        }
        pub fn $set(on: bool) {
            if $cell.swap(on, Ordering::Relaxed) != on {
                bump();
                crate::tools::log(&format!(
                    concat!("[runtime] keep-", $label, "-while-disabled = {}"),
                    on
                ));
                fan_out();
            }
        }
    };
}

keep_flag!(keep_translation, set_keep_translation, KEEP_TRANSLATION, "translation");
keep_flag!(keep_analysis, set_keep_analysis, KEEP_ANALYSIS, "analysis");
keep_flag!(keep_monitoring, set_keep_monitoring, KEEP_MONITORING, "monitoring");
keep_flag!(keep_export, set_keep_export, KEEP_EXPORT, "export");
keep_flag!(keep_webhook, set_keep_webhook, KEEP_WEBHOOK, "webhook");
keep_flag!(keep_overlay, set_keep_overlay, KEEP_OVERLAY, "overlay");

// ── the gate ────────────────────────────────────────────────────────────────────────────────────

/// May `sub` do work right now? One relaxed atomic load in the common (enabled) case.
#[inline]
pub fn active(sub: Subsystem) -> bool {
    let master = OVERSEER_ON.load(Ordering::Relaxed);
    match sub {
        // Translation has its own switch AND its own keep-alive override.
        Subsystem::Translation | Subsystem::TranslationEngine => {
            TRANSLATION_ON.load(Ordering::Relaxed)
                && (master || KEEP_TRANSLATION.load(Ordering::Relaxed))
        }
        Subsystem::Analysis => master || KEEP_ANALYSIS.load(Ordering::Relaxed),
        Subsystem::Monitoring => master || KEEP_MONITORING.load(Ordering::Relaxed),
        Subsystem::Export => master || KEEP_EXPORT.load(Ordering::Relaxed),
        // A career webhook is a REPORT, not an action: keeping it alive while Overseer is off is
        // the common ask ("don't touch my game, just tell me when a run finishes").
        Subsystem::Webhook => master || KEEP_WEBHOOK.load(Ordering::Relaxed),
        Subsystem::Overlay => master || KEEP_OVERLAY.load(Ordering::Relaxed),
        // Everything that actively changes the game or costs frame time follows the master switch
        // with no override — "disabled" has to mean the game runs exactly as it would unmodded.
        Subsystem::Skip
        | Subsystem::Tempo
        | Subsystem::Race
        | Subsystem::Advisor
        | Subsystem::Profiler => master,
    }
}

/// Convenience for the many `if !runtime::active(X) { return; }` call sites.
#[inline]
pub fn suspended(sub: Subsystem) -> bool {
    !active(sub)
}

// ── suspend / resume fan-out ────────────────────────────────────────────────────────────────────

/// Push the current power state into the subsystems that own a thread, a timer, or a resident
/// allocation — the ones a passive `active()` check can't stop. Safe to call repeatedly (every
/// callee is idempotent) and safe from any thread: nothing here touches IL2CPP.
fn fan_out() {
    // Translation engine: park the worker and (optionally) release the NLLB model's RAM. This is
    // the single biggest allocation Overseer holds, so releasing it is most of the "8 GB" answer.
    let tl = active(Subsystem::TranslationEngine);
    crate::mtl::set_worker_active(tl);
    crate::nllb::set_engine_active(tl);

    // Monitoring timers (opponent hunter auto-roll, Team Trials capture).
    if !active(Subsystem::Monitoring) {
        crate::hunter::suspend();
    }

    // Profiler sampling — it writes CSV rows and holds an aggregate table, so it is opt-in AND
    // suspended with the master switch. ANDing both is load-bearing: driving it from the subsystem
    // gate alone would silently switch the profiler ON for everyone the moment Overseer is enabled.
    crate::loadprof::set_enabled(active(Subsystem::Profiler) && crate::settings::profiler());

    // Advisor sidecar: stop long-polling (the child process idles rather than being killed, so
    // re-enabling is instant and we never race its own shutdown path).
    crate::sidecar::set_active(active(Subsystem::Advisor));
}

/// Seed the runtime state from persisted settings at boot, then fan out once. Called from
/// `settings::apply_on_boot` so the very first frame already reflects the user's saved choice.
pub fn apply(s: &crate::settings::Settings) {
    OVERSEER_ON.store(s.bot_enabled, Ordering::Relaxed);
    TRANSLATION_ON.store(s.translation_enabled, Ordering::Relaxed);
    KEEP_TRANSLATION.store(s.keep_translation_when_disabled, Ordering::Relaxed);
    KEEP_ANALYSIS.store(s.keep_analysis_when_disabled, Ordering::Relaxed);
    KEEP_MONITORING.store(s.keep_monitoring_when_disabled, Ordering::Relaxed);
    KEEP_EXPORT.store(s.keep_export_when_disabled, Ordering::Relaxed);
    KEEP_WEBHOOK.store(s.keep_webhook_when_disabled, Ordering::Relaxed);
    KEEP_OVERLAY.store(s.keep_overlay_when_disabled, Ordering::Relaxed);
    bump();
    fan_out();
}

/// A JSON-ready snapshot for the web UI (both switches + every override + the derived per-subsystem
/// verdicts, so the UI can show exactly what is running).
pub fn status_json() -> serde_json::Value {
    serde_json::json!({
        "overseer_enabled": overseer_enabled(),
        "translation_enabled": translation_enabled(),
        "keep_when_disabled": {
            "translation": keep_translation(),
            "analysis": keep_analysis(),
            "monitoring": keep_monitoring(),
            "export": keep_export(),
            "webhook": keep_webhook(),
            "overlay": keep_overlay(),
        },
        "active": {
            "skip": active(Subsystem::Skip),
            "tempo": active(Subsystem::Tempo),
            "translation": active(Subsystem::Translation),
            "translation_engine": active(Subsystem::TranslationEngine),
            "analysis": active(Subsystem::Analysis),
            "advisor": active(Subsystem::Advisor),
            "monitoring": active(Subsystem::Monitoring),
            "race": active(Subsystem::Race),
            "export": active(Subsystem::Export),
            "webhook": active(Subsystem::Webhook),
            "overlay": active(Subsystem::Overlay),
            "profiler": active(Subsystem::Profiler),
        },
    })
}
