//! Overseer — re-entrancy guard for Overseer-initiated calls into methods we also hook.
//!
//! Native retour detours don't have Frida's "breakpoint re-entrancy" crash, but we still
//! guard against LOGICAL recursion (our hook calling a method it itself hooks) with a
//! thread-local flag. Any re-entered Overseer hook checks `in_overseer()` and passes straight
//! through to the original.

use std::cell::Cell;

/// How long a guard may plausibly be held. Every Overseer-initiated call under this guard is a
/// synchronous UI action that completes in well under a frame; anything still "held" a second later
/// is a leak, not a long call.
const GUARD_TTL_MS: u64 = 1000;

thread_local! {
    /// Set while WE are invoking a method we also hook → the re-entered hook
    /// body sees this and passes straight through to the original.
    static IN_OVERSEER: Cell<bool> = const { Cell::new(false) };
    /// Deadline for the CURRENT hold. See the leak note on `in_overseer()`.
    static GUARD_UNTIL: Cell<u64> = const { Cell::new(0) };
}

/// Count of leaks detected by the deadline (surfaced through the health endpoint).
static LEAKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// RAII guard: set IN_OVERSEER for the duration of an Overseer-initiated call into a
/// method we also hook. Shared by skip.rs etc. — any re-entered Overseer hook sees
/// `in_overseer()` and passes straight through to the original.
///
/// Nesting-safe: it SAVES the previous value and RESTORES it on drop (not a blind reset to false).
/// The old version reset to false, so an inner guard dropping mid-way through an outer guard's call
/// re-enabled the hooks early → a skip could fire re-entrantly inside another skip and corrupt state.
pub struct ReentryGuard(bool, u64);
impl ReentryGuard {
    pub fn enter() -> Self {
        let prev_until = GUARD_UNTIL.with(|f| f.replace(crate::tools::now_ms() + GUARD_TTL_MS));
        ReentryGuard(IN_OVERSEER.with(|f| f.replace(true)), prev_until)
    }
}
impl Drop for ReentryGuard {
    fn drop(&mut self) {
        IN_OVERSEER.with(|f| f.set(self.0));
        GUARD_UNTIL.with(|f| f.set(self.1));
    }
}

/// Are we inside an Overseer-initiated call right now?
///
/// LEAK CLASS (measured: 15 occurrences in a four-day log, every single one straddling a race).
/// This crate builds with `panic = "abort"`, so Rust emits no landing pads — which means a `Drop`
/// only runs when the guarded call RETURNS to us. A managed C# exception raised inside the game code
/// we invoked does not return: IL2CPP unwinds it with SEH straight past our Rust frame, and the
/// guard is left set forever on that thread.
///
/// There was already a rescue for this, but it lived in the `ButtonCommon.Update` pump — and during
/// a race there are no ButtonCommon instances updating, so a guard that leaked going INTO a race
/// stayed set for the whole race and every skip was silently dead until buttons reappeared. That is
/// exactly the window the logs show.
///
/// So the deadline is now part of the hold itself: past `GUARD_TTL_MS` this reads false no matter
/// what, on any thread, with no pump involved. The pump's `clear_in_overseer` remains as the tidy-up
/// (and as the place the recovery is counted).
pub fn in_overseer() -> bool {
    if !IN_OVERSEER.with(|f| f.get()) {
        return false;
    }
    let until = GUARD_UNTIL.with(|f| f.get());
    if until != 0 && crate::tools::now_ms() >= until {
        // Expired hold — treat as not-held. Deliberately does NOT clear the cell: a guard that is
        // still on the stack must still restore the previous value when it eventually drops.
        return false;
    }
    true
}

/// True if the current thread is holding a guard that has outlived its deadline — i.e. a real leak
/// the watchdog should report. Reading it does not change any state.
pub fn guard_leaked() -> bool {
    IN_OVERSEER.with(|f| f.get())
        && GUARD_UNTIL.with(|f| f.get()) != 0
        && crate::tools::now_ms() >= GUARD_UNTIL.with(|f| f.get())
}

/// Number of leaked guards detected so far this session.
pub fn leak_count() -> u64 {
    LEAKS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Force-clear the re-entry guard on the current thread. Safety net for the leak described above:
/// the per-frame button pump calls this when it detects the guard held outside any Overseer call, so
/// the flag is tidied up (and the recovery counted) even though `in_overseer()` already ignores it.
pub fn clear_in_overseer() {
    LEAKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    IN_OVERSEER.with(|f| f.set(false));
    GUARD_UNTIL.with(|f| f.set(0));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nesting_restores_the_outer_hold() {
        assert!(!in_overseer());
        let outer = ReentryGuard::enter();
        assert!(in_overseer());
        {
            let _inner = ReentryGuard::enter();
            assert!(in_overseer());
        }
        // The inner guard dropping must NOT re-enable the hooks while the outer call is still
        // running — that was an old bug where a skip could fire re-entrantly inside another skip.
        assert!(in_overseer());
        drop(outer);
        assert!(!in_overseer());
    }

    #[test]
    fn a_leaked_guard_expires_instead_of_disabling_every_skip() {
        // Simulate the measured failure: a managed exception unwinds past our frame, so `Drop`
        // never runs and the guard is left set. `std::mem::forget` reproduces exactly that state.
        assert!(!in_overseer());
        std::mem::forget(ReentryGuard::enter());
        assert!(in_overseer(), "still held immediately after");
        assert!(!guard_leaked(), "not a leak yet — it is inside its deadline");
        std::thread::sleep(std::time::Duration::from_millis(GUARD_TTL_MS + 60));
        assert!(!in_overseer(), "past the deadline the hold must be ignored");
        assert!(guard_leaked(), "and the watchdog must be able to SEE it as a leak");
        let before = leak_count();
        clear_in_overseer();
        assert_eq!(leak_count(), before + 1);
        assert!(!guard_leaked());
    }
}
