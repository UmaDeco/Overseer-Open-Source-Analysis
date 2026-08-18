//! Main-thread freeze watchdog — turns "the game soft-locked" into a log line that names the culprit.
//!
//! ## Why this exists
//!
//! The user reports the game soft-locking when entering the Scout (gacha) menu. Every existing source
//! of evidence is structurally blind to it:
//!
//! * `overseer-crash.log` only fires on an EXCEPTION. A soft lock throws nothing, so it stays silent.
//! * `overseer-loadprof.csv` looks like frame instrumentation but `loadprof::frame()` has exactly one
//!   call site — `overlay.rs`, inside the D3D Present path — and it records a gap only when the NEXT
//!   frame arrives. In a true hang there is no next frame, so it writes ZERO rows. Every row in that
//!   file is a hitch the game RECOVERED from; none of them can be this bug.
//! * The mod's own logs just stop, which is indistinguishable from the user closing the game.
//!
//! So the one question that decides where to look next has never been answerable:
//!
//! * **HANG** — the main thread is blocked or spinning (ticks stop). A mod hook is a prime suspect,
//!   and [`crate::crashlog::current_step`] names the one we're inside.
//! * **STALL** — the engine is healthy and rendering (ticks continue); some view's state machine is
//!   waiting on an event that already fired or never will. Suspect ordering/timing (e.g. UI tempo
//!   outrunning real-time loads), NOT a blocked thread.
//!
//! ## How
//!
//! `ui_tempo::update_hook` (the detour on DOTween's per-frame main-thread tick) bumps a counter and
//! stamps a timestamp. This thread — deliberately a BACKGROUND thread, so it keeps running while the
//! main thread is wedged — polls that stamp and logs when it goes stale. Pure observation: it reads
//! atomics and writes a log line. It never touches il2cpp/Unity (illegal off the main thread), takes
//! no lock the main thread could hold, and cannot change game behaviour.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Consider the main thread stuck after this long with no frame. Well past any legitimate hitch
/// (asset loads/scene transitions routinely block for hundreds of ms; 2s is not normal).
const STUCK_MS: u64 = 2_000;
/// Once stuck, re-report on this cadence so the log shows whether it's terminal or just a long hitch.
const REPEAT_MS: u64 = 5_000;
/// Don't start judging until the tween pump has actually armed and ticked (boot takes several seconds).
const WARMUP_TICKS: u64 = 10;

static STARTED: AtomicBool = AtomicBool::new(false);

fn log(msg: &str) {
    crate::tools::log(msg);
}

/// Start the watchdog thread. Idempotent.
pub fn spawn() {
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    std::thread::spawn(|| {
        let mut reported_at: Option<u64> = None; // last-tick stamp we already reported on
        let mut last_report_ms: u64 = 0;
        let mut live = false;
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let (ticks, last_ms) = crate::ui_tempo::heartbeat();
            if ticks < WARMUP_TICKS || last_ms == 0 {
                continue; // not ticking yet — still booting, or the pump never armed
            }
            // Prove the heartbeat is REAL, exactly once. Without this, a watchdog whose heartbeat
            // never armed is silent — identical to a watchdog reporting a healthy game. Silence is
            // only evidence if we know we were actually watching.
            if !live {
                live = true;
                log(&format!("[watchdog] heartbeat live ({ticks} main-thread frames) — silence from here means healthy"));
            }
            let now = crate::tools::now_ms();
            let stale = now.saturating_sub(last_ms);
            if stale < STUCK_MS {
                // Healthy. If we'd reported a freeze, say that it ENDED — a freeze the game recovers
                // from is a hitch, not the soft lock, and conflating them would send us hunting ghosts.
                if let Some(t) = reported_at.take() {
                    log(&format!(
                        "[watchdog] main thread RECOVERED after ~{froze}ms (ticks={ticks}) — a hitch, not the soft lock",
                        froze = now.saturating_sub(t)
                    ));
                }
                continue;
            }
            // Stuck. Report once per freeze, then every REPEAT_MS while it persists.
            let first = reported_at.map_or(true, |t| t != last_ms);
            if first {
                reported_at = Some(last_ms);
            } else if now.saturating_sub(last_report_ms) < REPEAT_MS {
                continue;
            }
            last_report_ms = now;
            // EVERYTHING read here must be a pure atomic read — this runs on a BACKGROUND thread, and
            // touching managed IL2CPP off a GC-attached thread is a fatal "Collecting from unknown
            // thread". `effective_tempo_now()` was NOT safe (it lazily binds DOTween via IL2CPP), so
            // report the STORED tempo (a plain atomic) instead; the pin flags below show whether it was
            // actually applied. `current_step`, `gacha_active`, `story_playing`, `choice_pending` are
            // all atomic reads.
            log(&format!(
                "[watchdog] MAIN THREAD STUCK {stale}ms | HANG (ticks frozen at {ticks}) | step={step} | gacha={gacha} story={story} choice={choice} | tempo={tempo:.1}x{first_tag}",
                step = crate::crashlog::current_step(),
                gacha = crate::ui_tempo::gacha_active(),
                story = crate::skip::event::story_playing(),
                choice = crate::skip::result::choice_pending(),
                tempo = crate::ui_tempo::stored_tempo(),
                first_tag = if first { " | FIRST" } else { "" },
            ));
        }
    });
    log("[watchdog] armed — will report if the main thread stops ticking for >2s");
}
