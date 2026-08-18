//! Main-thread work queue.
//!
//! IL2CPP and GPU calls must run on the game's render/main thread — doing them from a
//! background thread trips a GC "collect from unknown thread" crash, and off-render-thread
//! GPU calls (Blit/ReadPixels, QualitySettings) recreate GPU state and crash. So the web
//! server, translation workers, and any other non-main thread that needs to run managed
//! code POST a closure here; it is drained once per frame from the single
//! `ui_tempo::update_hook` (`TweenManager.Update`) pump, which runs on the main thread and
//! is IL2CPP-attached. This is the marshalling primitive the texture/DB/QualitySettings
//! ports all build on.

#![allow(dead_code)]

use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use windows_sys::Win32::System::Threading::GetCurrentThreadId;

type Job = Box<dyn FnOnce() + Send + 'static>;

static QUEUE: Lazy<Mutex<Vec<Job>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// Thread id of the main/render thread (the ui_tempo pump). Stamped each frame in `drain()`.
static MAIN_TID: AtomicU32 = AtomicU32::new(0);

/// Record the pump's thread id (idempotent, cheap). Called at the top of `drain()`.
#[inline]
fn mark_main() {
    MAIN_TID.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
}

/// True if the caller is on the game's main/render thread. False before the first frame pump has
/// run — callers then MUST defer via `post()`. Lets hooks that already fire on the main thread run
/// il2cpp/GPU work inline (no one-frame flash) and defer only when off-thread.
pub fn is_main() -> bool {
    let t = MAIN_TID.load(Ordering::Relaxed);
    t != 0 && t == unsafe { GetCurrentThreadId() }
}

/// Queue a closure to run on the next main-thread frame. Safe to call from ANY thread.
/// The closure runs il2cpp-attached, so it may call managed methods and GPU APIs.
pub fn post<F: FnOnce() + Send + 'static>(f: F) {
    if let Ok(mut q) = QUEUE.lock() {
        q.push(Box::new(f));
    }
}

/// Number of jobs waiting (diagnostics).
pub fn pending() -> usize {
    QUEUE.lock().map(|q| q.len()).unwrap_or(0)
}

/// Process-clock ms of the last drain — lets a fallback driver defer to the primary.
static LAST_DRAIN_MS: AtomicU64 = AtomicU64::new(0);

/// FALLBACK driver, called from the `ButtonCommon.Update` hook.
///
/// The primary `drain()` runs from the `TweenManager.Update` detour — but DOTween only ticks that
/// while at least one tween is ACTIVE. A screen sitting idle on a dialog has none, which is exactly
/// where deferred UI work (a queued dialog click) is posted from. Without a second driver such a job
/// could sit in the queue indefinitely: the skill-learning confirm would be queued and never
/// clicked. `ButtonCommon.Update` ticks whenever any button exists, which covers precisely the
/// dialog case the tween pump misses. Throttled so the usual (tween-active) path stays authoritative
/// and this costs one atomic compare per button per frame.
pub fn drain_fallback() {
    let now = crate::tools::now_ms();
    if now.saturating_sub(LAST_DRAIN_MS.load(Ordering::Relaxed)) < 30 {
        return;
    }
    drain();
}

/// Run and clear all queued jobs. Called from the ui_tempo main-thread pump (primary) and from the
/// button pump (fallback, see `drain_fallback`).
pub fn drain() {
    LAST_DRAIN_MS.store(crate::tools::now_ms(), Ordering::Relaxed);
    mark_main(); // stamp the main-thread id so is_main() works for this frame's callers
    // Take the batch under the lock, then run outside it: a job that posts more work
    // won't deadlock (its post() re-locks an empty queue) and won't run this frame —
    // it lands in the next drain, keeping each frame bounded.
    let jobs: Vec<Job> = match QUEUE.lock() {
        Ok(mut q) if !q.is_empty() => std::mem::take(&mut *q),
        _ => return,
    };
    for job in jobs {
        // A panic must never unwind through the extern-"C" tween detour (UB). release is
        // panic=abort anyway; in dev this degrades a bad job to a logged no-op.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
    }
}
