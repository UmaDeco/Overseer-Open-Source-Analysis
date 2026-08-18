//! Self-healing latches + the stall watchdog — Overseer's answer to "it just stops responding".
//!
//! Every soft lock found in this codebase has the same shape: a boolean or a captured handle that
//! says "an Overseer action is in progress / a screen is armed", set by one hook and cleared by
//! another. When the clearing hook doesn't run — the view was torn down a different way, a network
//! error bounced the flow to the title, a coroutine was collapsed, the game was alt-tabbed mid
//! transition — the flag stays set forever and everything gated on it silently dies:
//!
//! | flag                          | stuck ⇒ |
//! |-------------------------------|---------|
//! | `hooks::IN_OVERSEER`          | every skip stops firing (already had an ad-hoc watchdog) |
//! | `ui_input::BUSY`              | no button is ever auto-pressed again |
//! | `loc_settext::ENTRY_DEPTH`    | every string forwards untranslated |
//! | `skip::result::WINDOW_OPEN`   | the result skip stays armed outside its race |
//! | `shop::DRIVING` / `*_UNTIL`   | the shop flow never completes |
//! | `mtl::IN_REAPPLY`             | translations stop being applied |
//!
//! Rather than keep adding one-off rescue code per flag, this module provides:
//!
//! * [`Latch`] — a boolean that CANNOT stay set: it stores the deadline alongside the value, so a
//!   reader past the deadline sees `false` and the flag self-heals with no watchdog involvement.
//! * [`Gate`] — an armed/disarmed window with the same guarantee plus an explicit reason string for
//!   the logs.
//! * [`Progress`] — "the thing I asked the game to do should have happened by now": stamp an
//!   expectation, and the watchdog reports (and optionally runs a recovery) if it never resolves.
//! * [`tick`] — the per-frame watchdog, driven from the main-thread pump. It never touches IL2CPP,
//!   so it is safe wherever it runs, and it only ever CLEARS state (never invokes game code), which
//!   is the one recovery action that can't make things worse.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

fn now_ms() -> u64 {
    crate::tools::now_ms()
}

// ── Latch ───────────────────────────────────────────────────────────────────────────────────────

/// A boolean with a built-in expiry. `set()` records both the value and the wall-clock deadline;
/// `get()` returns false once the deadline passes. There is no code path that leaves it stuck,
/// because "stuck" isn't representable: the deadline is part of the stored state.
///
/// Packing: the deadline (ms since process start) lives in the low 63 bits and the value in the
/// top bit of a single `AtomicU64`, so reads and writes stay lock-free and torn states impossible.
pub struct Latch {
    state: AtomicU64,
    /// How long a `set(true)` stays true if nobody clears it.
    ttl_ms: u64,
    name: &'static str,
}

const VAL_BIT: u64 = 1 << 63;

impl Latch {
    pub const fn new(name: &'static str, ttl_ms: u64) -> Self {
        Latch { state: AtomicU64::new(0), ttl_ms, name }
    }

    /// Set the latch. It automatically reads back false after `ttl_ms`.
    #[inline]
    pub fn arm(&self) {
        self.state.store(VAL_BIT | ((now_ms() + self.ttl_ms) & !VAL_BIT), Ordering::Relaxed);
    }

    /// Set the latch with a one-off lifetime (overrides `ttl_ms` for this arm only).
    #[inline]
    pub fn arm_for(&self, ms: u64) {
        self.state.store(VAL_BIT | ((now_ms() + ms) & !VAL_BIT), Ordering::Relaxed);
    }

    /// Clear the latch explicitly (the normal, happy path).
    #[inline]
    pub fn clear(&self) {
        self.state.store(0, Ordering::Relaxed);
    }

    /// Is the latch currently set AND still inside its lifetime?
    #[inline]
    pub fn get(&self) -> bool {
        let s = self.state.load(Ordering::Relaxed);
        s & VAL_BIT != 0 && now_ms() < (s & !VAL_BIT)
    }

    /// True if the latch was set but has now expired — i.e. somebody forgot to clear it. Used by
    /// the watchdog to LOG the leak (the value itself already reads false, so behaviour is fixed
    /// either way; this is how the leak becomes visible instead of silent).
    #[inline]
    pub fn leaked(&self) -> bool {
        let s = self.state.load(Ordering::Relaxed);
        s & VAL_BIT != 0 && now_ms() >= (s & !VAL_BIT)
    }

    /// Consume a leak report: returns true once per leak, then resets the latch to a clean zero.
    pub fn take_leak(&self) -> bool {
        if self.leaked() {
            self.state.store(0, Ordering::Relaxed);
            return true;
        }
        false
    }

    pub fn name(&self) -> &'static str {
        self.name
    }
}

// ── Gate ────────────────────────────────────────────────────────────────────────────────────────

/// An armed window (e.g. "the race-result screen is up, auto-advance may act"). Same self-healing
/// guarantee as [`Latch`], with an arm timestamp so callers can reason about age, and a hard cap
/// that the watchdog reports.
pub struct Gate {
    open: AtomicBool,
    since_ms: AtomicU64,
    max_ms: u64,
    name: &'static str,
}

impl Gate {
    pub const fn new(name: &'static str, max_ms: u64) -> Self {
        Gate { open: AtomicBool::new(false), since_ms: AtomicU64::new(0), max_ms, name }
    }

    pub fn arm(&self) {
        self.since_ms.store(now_ms(), Ordering::Relaxed);
        self.open.store(true, Ordering::Relaxed);
    }

    /// Disarm; returns whether it had been armed (so callers can log only the real transition).
    pub fn disarm(&self) -> bool {
        self.since_ms.store(0, Ordering::Relaxed);
        self.open.swap(false, Ordering::Relaxed)
    }

    /// Open AND inside its maximum lifetime. A gate past `max_ms` reads closed — the auto-advance
    /// engine can therefore never keep pressing buttons on a screen it lost track of.
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed) && self.age_ms() < self.max_ms
    }

    /// Raw open flag, ignoring the lifetime (for diagnostics only).
    pub fn raw(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }

    /// How long the gate has been open. Keyed off the OPEN flag, not off a zero timestamp: the
    /// process clock genuinely reads 0 for the first millisecond, so a `since_ms == 0` sentinel made
    /// a gate armed in that window report an infinite age — i.e. read closed the instant it opened.
    pub fn age_ms(&self) -> u64 {
        if !self.open.load(Ordering::Relaxed) {
            return u64::MAX;
        }
        now_ms().saturating_sub(self.since_ms.load(Ordering::Relaxed))
    }

    /// Watchdog hook: if the gate has been open past its cap, close it and report once.
    fn reap(&self) -> bool {
        if self.open.load(Ordering::Relaxed) && self.age_ms() >= self.max_ms {
            self.disarm();
            return true;
        }
        false
    }

    pub fn name(&self) -> &'static str {
        self.name
    }
}

// ── Progress expectations ───────────────────────────────────────────────────────────────────────

/// "I told the game to advance; if it hasn't by `deadline`, something is wrong."
///
/// Skip and training transitions are the reported failure: Overseer fires the game's own skip and
/// then waits for the next screen. If the skip was swallowed (an input block that never lifted, a
/// coroutine collapsed early, a dialog stacked on top) nothing ever advances and the player sits on
/// a dead screen. A `Progress` turns that indefinite wait into a bounded one: the expectation is
/// stamped when we act, cleared when the game demonstrably moved on, and escalated by the watchdog
/// when it doesn't — first a retry, then a full stand-down that hands control back to the player.
pub struct Progress {
    /// Deadline (ms) for the current expectation; 0 = nothing pending.
    deadline: AtomicU64,
    /// When the expectation was stamped (for the log).
    since: AtomicU64,
    /// Escalation step: 0 = fresh, 1 = retried once, 2 = gave up (stood down).
    stage: AtomicU32,
    timeout_ms: u64,
    name: &'static str,
}

/// What the watchdog decided about a pending expectation this tick.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum Escalation {
    /// Nothing pending, or still inside the deadline.
    None,
    /// First timeout — the caller should retry its action once.
    Retry,
    /// Second timeout — the caller must stand down and let the player take over.
    StandDown,
}

impl Progress {
    pub const fn new(name: &'static str, timeout_ms: u64) -> Self {
        Progress {
            deadline: AtomicU64::new(0),
            since: AtomicU64::new(0),
            stage: AtomicU32::new(0),
            timeout_ms,
            name,
        }
    }

    /// Stamp "I expect the game to progress from here".
    pub fn expect(&self) {
        let now = now_ms();
        self.since.store(now, Ordering::Relaxed);
        self.deadline.store(now + self.timeout_ms, Ordering::Relaxed);
        self.stage.store(0, Ordering::Relaxed);
    }

    /// The game moved on — clear the expectation and the escalation ladder.
    pub fn resolved(&self) {
        if self.deadline.swap(0, Ordering::Relaxed) != 0 {
            self.stage.store(0, Ordering::Relaxed);
        }
    }

    /// Is an expectation currently outstanding?
    pub fn pending(&self) -> bool {
        self.deadline.load(Ordering::Relaxed) != 0
    }

    /// How long the current expectation has been outstanding (0 if none).
    pub fn age_ms(&self) -> u64 {
        let s = self.since.load(Ordering::Relaxed);
        if s == 0 || !self.pending() {
            0
        } else {
            now_ms().saturating_sub(s)
        }
    }

    /// Has the caller already been told to stand down for this expectation?
    pub fn stood_down(&self) -> bool {
        self.stage.load(Ordering::Relaxed) >= 2
    }

    /// Push a pending deadline `ms` further out (no-op when nothing is pending). Used when the game
    /// regains focus: the frames it spent in the background must not count against the expectation.
    pub fn defer(&self, ms: u64) {
        let d = self.deadline.load(Ordering::Relaxed);
        if d != 0 {
            self.deadline.store(now_ms().max(d) + ms, Ordering::Relaxed);
        }
    }

    /// Watchdog step. Returns what the owner should do; each escalation fires at most once per
    /// stage, and the deadline is extended so the next stage lands one timeout later.
    pub fn poll(&self) -> Escalation {
        let d = self.deadline.load(Ordering::Relaxed);
        if d == 0 || now_ms() < d {
            return Escalation::None;
        }
        let stage = self.stage.fetch_add(1, Ordering::Relaxed);
        match stage {
            0 => {
                self.deadline.store(now_ms() + self.timeout_ms, Ordering::Relaxed);
                Escalation::Retry
            }
            1 => {
                // Keep the record (stood_down() stays true) but stop re-firing.
                self.deadline.store(0, Ordering::Relaxed);
                self.stage.store(2, Ordering::Relaxed);
                Escalation::StandDown
            }
            _ => {
                self.deadline.store(0, Ordering::Relaxed);
                self.stage.store(2, Ordering::Relaxed);
                Escalation::None
            }
        }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }
}

// ── recovery counters (surfaced in the web UI so a soft lock leaves evidence) ────────────────────

static RECOVERIES: AtomicU64 = AtomicU64::new(0);
static LAST_RECOVERY_MS: AtomicU64 = AtomicU64::new(0);
static LAST_RECOVERY: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Record (and log) that Overseer rescued itself from a stuck state.
pub fn note_recovery(what: &str) {
    RECOVERIES.fetch_add(1, Ordering::Relaxed);
    LAST_RECOVERY_MS.store(now_ms(), Ordering::Relaxed);
    if let Ok(mut g) = LAST_RECOVERY.lock() {
        *g = what.to_string();
    }
    crate::tools::log(&format!("[guard] recovered: {what}"));
}

/// (count, ms since the last one, description) for the diagnostics endpoint.
pub fn recovery_stats() -> (u64, u64, String) {
    let n = RECOVERIES.load(Ordering::Relaxed);
    let last = LAST_RECOVERY_MS.load(Ordering::Relaxed);
    let age = if last == 0 { 0 } else { now_ms().saturating_sub(last) };
    let what = LAST_RECOVERY.lock().map(|g| g.clone()).unwrap_or_default();
    (n, age, what)
}

// ── main-thread frame stamp (used to tell HANG from STALL without a second thread) ───────────────

static LAST_TICK_MS: AtomicU64 = AtomicU64::new(0);
static TICKS: AtomicU64 = AtomicU64::new(0);

/// Is the game window in the foreground? Used to hold the progression watchdog off while the
/// player is elsewhere.
///
/// WINDOW FOCUS is a real source of false positives here. Alt-tabbing away can pause or heavily
/// throttle the game's own update loop while our deadlines keep ticking on wall-clock — so a skip
/// that was progressing perfectly would look stalled purely because the player checked their
/// browser, and the watchdog would "recover" from a non-problem (standing a working skip down).
/// Cheap: two Win32 calls, once per frame.
fn game_foreground() -> bool {
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    // Cached: two user32 round trips per frame is real work at 200 fps, and focus changes on a human
    // timescale. Re-checked four times a second, far finer than the 3 s refocus grace.
    static CACHED: AtomicBool = AtomicBool::new(true);
    static CHECKED_MS: AtomicU64 = AtomicU64::new(0);
    let now = now_ms();
    if now.saturating_sub(CHECKED_MS.load(Ordering::Relaxed)) < 250 {
        return CACHED.load(Ordering::Relaxed);
    }
    CHECKED_MS.store(now, Ordering::Relaxed);
    let fg = unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            false
        } else {
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            pid == GetCurrentProcessId()
        }
    };
    CACHED.store(fg, Ordering::Relaxed);
    fg
}

/// Process-clock ms at which the game most recently held focus.
static LAST_FOREGROUND_MS: AtomicU64 = AtomicU64::new(0);
/// How long after regaining focus the progression watchdog stays quiet, so the first frames back
/// (which the game spends re-acquiring devices and catching up) are never judged as a stall.
const REFOCUS_GRACE_MS: u64 = 3_000;

/// Per-frame watchdog. Driven from the single main-thread pump (`ui_tempo::update_hook`). Pure
/// atomics + logging — it never calls into the game, so it is safe at any point in the frame and
/// can't itself become the thing that hangs.
pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    let now = now_ms();
    LAST_TICK_MS.store(now, Ordering::Relaxed);

    // 1. The click engine's busy flag. `auto_press` sets it around a synchronous game call; if that
    //    call diverts (an exception unwound through, a view teardown re-entered us) the flag stuck
    //    and NOTHING was ever auto-pressed again for the rest of the session — a silent soft lock
    //    with no symptom other than "the skip stopped working".
    if crate::ui_input::reap_busy() {
        note_recovery("ui_input::BUSY held past its deadline (click engine unblocked)");
    }

    // 2. The composite text-entry nesting guard. Stuck ⇒ every string forwards untranslated.
    if crate::loc_settext::reap_entry_guard() {
        note_recovery("loc_settext entry guard held past its deadline (translation unblocked)");
    }

    // 3. The race-result arm window. Stuck ⇒ auto-press keeps firing in unrelated menus.
    if crate::skip::result::reap_window() {
        note_recovery("race-result window open past its cap (auto-advance disarmed)");
    }

    // 4. Skip / training progression expectations — but ONLY while the player is actually looking
    //    at the game, plus a short grace after regaining focus. The three latches above are pure
    //    self-repair and always safe; this step can INVOKE a retry or stand a feature down, so it
    //    must not fire on a deadline that only expired because the game was in the background.
    if game_foreground() {
        let was = LAST_FOREGROUND_MS.swap(now, Ordering::Relaxed);
        let regained = now.saturating_sub(was) > 500; // gap since the last foreground frame
        if regained {
            // Push every pending deadline out so the catch-up frames aren't judged as a stall.
            crate::skip::defer_progress(REFOCUS_GRACE_MS);
        } else {
            crate::skip::poll_progress();
        }
    }
}

/// Frames the watchdog has ticked, and the process-clock ms of the last tick. A background reader
/// can compare the latter against wall-clock to tell a frozen main thread from a stalled view.
pub fn heartbeat() -> (u64, u64) {
    (TICKS.load(Ordering::Relaxed), LAST_TICK_MS.load(Ordering::Relaxed))
}

/// Watchdog helper for a set of gates — closes any that outlived their cap and reports each once.
pub fn reap_gate(g: &Gate) -> bool {
    let closed = g.reap();
    if closed {
        note_recovery(&format!("gate '{}' exceeded its lifetime and was disarmed", g.name()));
    }
    closed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_latch_cannot_stay_set() {
        // This is the entire point of the type: the deadline is part of the stored value, so a lost
        // `clear()` self-heals instead of silently disabling whatever gates on it.
        let l = Latch::new("test", 30);
        assert!(!l.get());
        l.arm();
        assert!(l.get());
        std::thread::sleep(std::time::Duration::from_millis(60));
        assert!(!l.get(), "an armed latch must read false past its deadline");
        assert!(l.leaked(), "and the leak must be visible to the watchdog");
        assert!(l.take_leak());
        assert!(!l.take_leak(), "a leak is reported exactly once");
    }

    #[test]
    fn clearing_a_latch_is_not_a_leak() {
        let l = Latch::new("test", 30);
        l.arm();
        l.clear();
        std::thread::sleep(std::time::Duration::from_millis(60));
        assert!(!l.get());
        assert!(!l.leaked(), "the normal path must never be reported as a leak");
    }

    #[test]
    fn a_gate_closes_itself_past_its_cap() {
        let g = Gate::new("test", 40);
        assert!(!g.is_open());
        g.arm();
        assert!(g.is_open());
        assert!(g.raw());
        std::thread::sleep(std::time::Duration::from_millis(70));
        assert!(!g.is_open(), "an over-age gate must read closed");
        assert!(g.raw(), "…while still being visibly stuck, so the watchdog can report it");
        assert!(g.reap());
        assert!(!g.raw());
        assert!(!g.reap(), "reaped once");
    }

    #[test]
    fn progress_escalates_retry_then_stand_down() {
        let p = Progress::new("test", 20);
        assert_eq!(p.poll(), Escalation::None, "nothing pending");
        p.expect();
        assert!(p.pending());
        assert_eq!(p.poll(), Escalation::None, "still inside the deadline");
        std::thread::sleep(std::time::Duration::from_millis(35));
        assert_eq!(p.poll(), Escalation::Retry);
        assert_eq!(p.poll(), Escalation::None, "one escalation per stage");
        std::thread::sleep(std::time::Duration::from_millis(35));
        assert_eq!(p.poll(), Escalation::StandDown);
        assert!(p.stood_down());
        assert_eq!(p.poll(), Escalation::None, "and it stops firing after standing down");
    }

    #[test]
    fn resolving_progress_cancels_the_escalation() {
        // The game moving on must fully disarm the ladder — otherwise a slow-but-successful skip
        // would be "recovered" from, and the leg stood down for no reason.
        let p = Progress::new("test", 20);
        p.expect();
        p.resolved();
        assert!(!p.pending());
        std::thread::sleep(std::time::Duration::from_millis(35));
        assert_eq!(p.poll(), Escalation::None);
        assert!(!p.stood_down());
    }
}
