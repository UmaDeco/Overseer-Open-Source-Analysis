//! Event/story skip + friendship "TAG" splash skip + deferred pump.

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;

use crate::hooks::{in_overseer, ReentryGuard};
use crate::skip::{
    call_orig, clock, is_enabled, is_event_enabled, now_ms, rr_log, Invokable, EVENT_SKIPS, TRAIN_SKIPS,
};

// Debounce only swallows a same-timeline re-fire of OnStartPlayingTimeline (a few frames). It must
// stay SHORT because it's WALL-clock, not game time — at high UI speed (e.g. 10x) distinct events
// compress into a small real-time window, and a long debounce was dropping the 2nd/3rd event ("some
// texts don't skip"). The IS_PLAYING check below is the real guard against re-skipping a finished one.
const EVENT_DEBOUNCE_MS: u64 = 100;

// Invokables (set in install).
pub(crate) static SKIP_STORY: OnceLock<Invokable> = OnceLock::new(); // events
pub(crate) static GET_TL: OnceLock<Invokable> = OnceLock::new(); // StoryViewController.get_TimelineController
pub(crate) static IS_PLAYING: OnceLock<Invokable> = OnceLock::new(); // StoryTimelineController.get_IsPlaying
pub(crate) static TRAIN_CUTT: OnceLock<Invokable> = OnceLock::new(); // get_IsPlayingOrWillPlayTrainingCutt

crate::skip_hook_slot!(TR_TIMELINE, D_TIMELINE);
crate::skip_hook_slot!(TR_TAGIN, D_TAGIN); // SingleModeMainViewTagTrainingCutInPlayer.PlayCutIn
crate::skip_hook_slot!(TR_TAGOUT, D_TAGOUT); // .PlayCutInOut

// ── Goal-Complete FREEZE guard ──────────────────────────────────────────────
// SkipStory on the Goal-Complete ("All goals achieved") story pushes the ConfirmComplete + Result
// dialogs into a circular stack → the game spins forever in DialogManager.GetParentType (z-order
// resolution) and HANGS. Root-caused 2026-07-05 from a live crashtrace (last mod action before the
// hang = SkipStory; matches the historical "Events OFF" workaround). We suppress the event skip for a
// short window after SingleModeConfirmCompleteViewController.BeginView (Goal Complete opening). It is
// TIME-BASED so it can NEVER stick (auto-expires), and Goal Complete → URA Finale races has no normal
// events in that window, so there is zero collateral on other events / other skips.
static GOAL_COMPLETE_UNTIL: AtomicU64 = AtomicU64::new(0);
/// Arm the end-career suppression. CRITICAL: callers must arm this BEFORE running the original method,
/// because the original ConfirmComplete flow is what HANGS (so a store placed after call_orig never
/// runs). 120s window (covers the whole graduation flow) + cleared at Home; time-based so it can't stick.
pub(crate) fn mark_goal_complete(src: &str) {
    if GOAL_COMPLETE_UNTIL.load(Ordering::Relaxed) < now_ms() + 100_000 {
        rr_log(&format!("[event] end-career detected ({src}) -> event-skip suppressed 120s (freeze guard)"));
    }
    GOAL_COMPLETE_UNTIL.store(now_ms() + 120_000, Ordering::Relaxed);
}
/// Cleared when the lobby (Home) is reached — a new career can skip events normally again.
pub(crate) fn clear_goal_complete() {
    if GOAL_COMPLETE_UNTIL.swap(0, Ordering::Relaxed) != 0 {
        rr_log("[event] Home reached -> end-career event-skip suppression cleared");
    }
}
// RegisterDownload = the EARLIEST safe touch (view asset preload, fires before the Goal-Complete story
// plays). Arm the guard here, before call_orig, so the corrupting SkipStory is suppressed in time.
type Void2 = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void);
crate::skip_hook_slot!(TR_GCREG, D_GCREG);
pub(crate) unsafe extern "C" fn on_goal_complete_register(this: *mut c_void, reg: *mut c_void, m: *mut c_void) {
    mark_goal_complete("RegisterDownload");
    let t = TR_GCREG.load(Ordering::Relaxed);
    if t != 0 {
        let f: Void2 = std::mem::transmute(t);
        f(this, reg, m);
    }
}
// BeginView backup — arm BEFORE call_orig (the original hangs, so a post-call store never runs).
crate::skip_hook_slot!(TR_GOALBEGIN, D_GOALBEGIN);
pub(crate) unsafe extern "C" fn on_goal_complete_begin(this: *mut c_void, m: *mut c_void) {
    mark_goal_complete("BeginView");
    call_orig(&TR_GOALBEGIN, this, m);
}

// ── EVENTS: SkipStory on OnStartPlayingTimeline (guarded + debounced). ──────
static LAST_EVENT_SKIP_MS: AtomicU64 = AtomicU64::new(0);
fn try_event_skip(this: *mut c_void) {
    // The general Events toggle gate; every OTHER guard lives in the shared core so a different
    // enable gate (the skill-learning cutscene retry) can reuse the exact same guarded body.
    // A stood-down leg (a previous SkipStory never advanced, even after a retry) plays the event
    // normally for a short cool-down so the screen stays interactive.
    if !is_event_enabled() || crate::skip::event_stood_down() {
        return;
    }
    try_event_skip_core(this);
}

/// The guarded SkipStory core, WITHOUT the `is_event_enabled` toggle gate — the CALLER supplies its
/// own enable gate (the general Events toggle via `try_event_skip`, OR the skill-learning cutscene
/// retry via `retry_captured_skip`). Keeps EVERY other guard intact and in the same order: re-entry
/// (`in_overseer`), null `this`, the Goal-Complete freeze window, `dialog_open()` (the DialogManager
/// z-order freeze class — never weaken it), TRAIN_CUTT, the timeline IS_PLAYING check, the wall-clock
/// debounce, and the ReentryGuard. Returns true iff SkipStory actually fired.
pub(crate) fn try_event_skip_core(this: *mut c_void) -> bool {
    if in_overseer() || this.is_null() {
        return false;
    }
    // Never SkipStory during the Goal-Complete window (it hangs the game — see above).
    if now_ms() < GOAL_COMPLETE_UNTIL.load(Ordering::Relaxed) {
        crate::tools::trace("[event] SkipStory SUPPRESSED (Goal Complete freeze guard)");
        return false;
    }
    // Never SkipStory while ANY dialog is open/opening — the general form of the same freeze: the
    // fan-class promotion dialog ("Umamusume is a class") hung the game 2026-07-14 with SkipStory as
    // the last mod action, exactly like Goal-Complete. Worst case the player taps through one event
    // by hand; there is no safe way to skip under a live dialog.
    if crate::skip::dialog_open() {
        crate::tools::trace("[event] SkipStory suppressed (dialog open)");
        return false;
    }
    let now = clock().elapsed().as_millis() as u64;
    let dt = now.wrapping_sub(LAST_EVENT_SKIP_MS.load(Ordering::Relaxed));
    // Guard the whole critical section: any re-entry into our hooks passes thru.
    let _g = ReentryGuard::enter();
    unsafe {
        // Don't skip while a training cut-in is (or will be) playing.
        if let Some(tc) = TRAIN_CUTT.get() {
            if tc.ok() && tc.call_bool(this) {
                crate::tools::trace("[event] ignored (training cut-in playing)");
                return false;
            }
        }
        // Resolve the playing timeline (its pointer is the dedup key we log, so we can tell a
        // genuine distinct event from a same-timeline re-fire).
        let mut tl_key = 0usize;
        if let (Some(gtl), Some(isp)) = (GET_TL.get(), IS_PLAYING.get()) {
            if gtl.ok() && isp.ok() {
                let tl = gtl.call_ptr(this);
                if tl.is_null() || !isp.call_bool(tl) {
                    crate::tools::trace("[event] ignored (no timeline / not playing)");
                    return false;
                }
                tl_key = tl as usize;
            }
        }
        // Short re-fire debounce: swallow OnStartPlayingTimeline firing twice for the SAME timeline
        // start (a few render frames). MUST stay short — it's wall-clock, so a long window dropped
        // distinct back-to-back events at high UI speed (the "some texts don't skip" report).
        if dt < EVENT_DEBOUNCE_MS {
            if crate::tools::log::trace_enabled() { crate::tools::trace(&format!("[event] ignored (debounce {dt}ms) tl={tl_key:#x}")); }
            return false;
        }
        if let Some(ss) = SKIP_STORY.get() {
            if ss.ok() {
                ss.call_void(this);
                LAST_EVENT_SKIP_MS.store(now, Ordering::Relaxed);
                EVENT_SKIPS.fetch_add(1, Ordering::Relaxed);
                // Progression expectation: SkipStory should end the timeline within seconds. If it
                // doesn't, the story is wedged (the classic "event never advances" soft lock) and
                // the watchdog retries once, then stands the leg down so the player can tap through.
                crate::skip::EVENT_PROGRESS.expect();
                crate::tools::trace(&format!("[event] SkipStory() tl={tl_key:#x} dt={dt}ms"));
                return true;
            }
        }
    }
    false
}

/// The story/event flow moved on (a new timeline started, the view changed, the turn advanced).
pub(crate) fn note_progressed() {
    crate::skip::EVENT_PROGRESS.resolved();
}

/// Retry the guarded event-skip on the CAPTURED StoryViewController (`STORY_CTRL_H`, set by
/// `on_start_timeline`). The skill-learning pump calls this to skip the skill-ACQUISITION cutscene,
/// whose one-shot `on_start_timeline` skip is suppressed by `dialog_open()` at the exact moment the
/// "Learn these skills?" confirm is closing — so the pump retries it, through the same guards, until
/// the dialog is gone and the timeline is genuinely playing. The CALLER applies the feature gate
/// (`is_skill_learn_enabled`). GC-safe: re-fetches the live controller through the strong handle and
/// clears it on Destroy. Returns true iff SkipStory actually fired.
pub(crate) fn retry_captured_skip() -> bool {
    let ctrl = match STORY_CTRL_H.lock() {
        Ok(g) => g.as_ref().map_or(std::ptr::null_mut(), |h| h.target()),
        Err(_) => std::ptr::null_mut(),
    };
    if ctrl.is_null() {
        return false;
    }
    if unsafe { crate::il2cpp::unity_object_destroyed(ctrl) } {
        if let Ok(mut g) = STORY_CTRL_H.lock() {
            *g = None; // frees the handle; the wrapper can now be collected
        }
        return false;
    }
    try_event_skip_core(ctrl)
}
pub(crate) unsafe extern "C" fn on_start_timeline(this: *mut c_void, m: *mut c_void) {
    // Capture the StoryViewController so `story_playing()` can poll its live play-state — that PRECISE
    // signal (1x only while a timeline is actually playing) replaced a 6s time window that oscillated
    // tempo in the lobby (snapping the journal scroll, feeling slow). At 20x the post-race choice UI
    // desyncs; keeping the story itself at 1x lets the choice build cleanly.
    if let Ok(mut g) = STORY_CTRL_H.lock() {
        *g = Some(crate::il2cpp::GCHandle::new(this, false)); // old handle freed on drop
    }
    // A NEW timeline starting is proof the previous one finished — the strongest "the game moved
    // on" signal this leg has, and it comes from the game rather than from a timer.
    note_progressed();
    call_orig(&TR_TIMELINE, this, m);
    try_event_skip(this);
}

/// The StoryViewController from the most recent timeline start — polled by `refresh_story_state()`.
/// Held via a STRONG GCHandle: a raw usize cache here was a use-after-free — the GC can't see Rust
/// statics, so once the scene dropped the controller the object was collected and the per-frame poll
/// ran get_Timeline/IsPlaying on reused memory (AV "read at 0xffffffffffffffff" in GameAssembly; the
/// m_CachedPtr destroyed-check reads the same freed memory, so it can't guard this). The handle keeps
/// the managed wrapper alive until the native side dies (destroyed-check then clears it) or the next
/// timeline replaces it.
static STORY_CTRL_H: std::sync::Mutex<Option<crate::il2cpp::GCHandle>> = std::sync::Mutex::new(None);
/// Cached play-state, refreshed ONLY on the main thread by `refresh_story_state()`. `story_playing()`
/// reads this atomic — never IL2CPP — so it is safe to call from ANY thread.
static STORY_PLAYING: AtomicBool = AtomicBool::new(false);

/// Refresh the cached story play-state by polling the captured StoryViewController's timeline
/// `IsPlaying`. **MAIN THREAD ONLY** — it invokes managed IL2CPP methods; the tween pump calls it
/// once per frame. This is deliberately the ONLY place that touches IL2CPP for the story pin, because
/// `story_playing()` is read from a BACKGROUND thread (the watchdog's freeze report) and calling
/// managed code from an unattached thread is a fatal GC error ("Collecting from unknown thread").
pub fn refresh_story_state() {
    // Re-fetch the live pointer through the GC handle each frame — null once collected, so the
    // calls below can never run on freed memory.
    let ctrl = match STORY_CTRL_H.lock() {
        Ok(g) => g.as_ref().map_or(std::ptr::null_mut(), |h| h.target()),
        Err(_) => std::ptr::null_mut(),
    };
    if ctrl.is_null() {
        STORY_PLAYING.store(false, Ordering::Relaxed);
        return;
    }
    let playing = unsafe {
        if crate::il2cpp::unity_object_destroyed(ctrl) {
            if let Ok(mut g) = STORY_CTRL_H.lock() {
                *g = None; // frees the handle; the wrapper can now be collected
            }
            false
        } else if let (Some(gtl), Some(isp)) = (GET_TL.get(), IS_PLAYING.get()) {
            if gtl.ok() && isp.ok() {
                let tl = gtl.call_ptr(ctrl);
                !tl.is_null() && isp.call_bool(tl)
            } else {
                false
            }
        } else {
            false
        }
    };
    // Falling edge: the timeline was playing and now isn't — the story finished, which is the
    // resolution signal the expectation was missing.
    if STORY_PLAYING.swap(playing, Ordering::Relaxed) && !playing {
        note_progressed();
    }
}

/// Is a story/event timeline playing? Reads the cached state only (no IL2CPP) → safe from any thread.
pub fn story_playing() -> bool {
    STORY_PLAYING.load(Ordering::Relaxed)
}

// ── TAG (friendship/rainbow) TRAINING cut-in splash ─────────────────────────
// The "FRIENDSHIP TRAINING!" splash is SingleModeMainViewTagTrainingCutInPlayer.
// PlayCutIn(List<SupportCardData>, Action onDone). We skip the ~1.5s animation by
// firing the onDone callback immediately (so the turn proceeds with no splash).
// Gated by the training-skip toggle.
type TagInFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void);
type TagOutFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void);

// Strategy: let the ORIGINAL PlayCutIn run (so its setup + the later PlayCutInOut work,
// no freeze) but fire its onDone callback EARLY (deferred to the next frame, on a clean
// stack) so the flow advances immediately instead of waiting ~1.1s for the splash. The
// original's own later onDone re-fires — assumed idempotent (it just unblocks the
// execute coroutine). Net: friendship training skips ~as fast as a normal one.
pub(crate) static ACTION_INVOKE: OnceLock<Invokable> = OnceLock::new(); // System.Action.Invoke
pub(crate) static SET_ACTIVE: OnceLock<Invokable> = OnceLock::new(); // GameObject.SetActive(bool)
// The deferred onDone, held via a STRONG GCHandle: the Action is a per-call closure and the GC
// can't see Rust statics — a raw usize cache was a use-after-free (nothing rooted the delegate
// between capture and the next-frame Invoke). The handle roots it until after the deferred fire.
// No destroyed-check: delegates aren't UnityEngine.Objects; the strong root alone guarantees validity.
static PENDING_TAG_CB: std::sync::Mutex<Option<crate::il2cpp::GCHandle>> = std::sync::Mutex::new(None);
const O_TAG_ROOT: usize = 0x60; // SingleModeMainViewTagTrainingCutInPlayer._tagCutInRootObject

// GameObject.SetActive takes a bool arg → fn(this, bool, MethodInfo*).
type SetActiveFn = unsafe extern "C" fn(*mut c_void, bool, *mut c_void);

unsafe fn hide_tag_visual(this: *mut c_void) {
    if this.is_null() {
        return;
    }
    let go = *((this as usize + O_TAG_ROOT) as *const *mut c_void);
    if go.is_null() {
        return;
    }
    if let Some(sa) = SET_ACTIVE.get() {
        if sa.ok() {
            let f: SetActiveFn = std::mem::transmute(sa.code);
            f(go, false, sa.mi as *mut c_void);
        }
    }
}

pub(crate) unsafe extern "C" fn on_tag_play_cutin(this: *mut c_void, cards: *mut c_void, cb: *mut c_void, m: *mut c_void) {
    let t = TR_TAGIN.load(Ordering::Relaxed);
    if t != 0 {
        let f: TagInFn = std::mem::transmute(t);
        f(this, cards, cb, m); // full original flow — keeps state valid for PlayCutInOut
    }
    if is_enabled() && !in_overseer() && !cb.is_null() {
        hide_tag_visual(this); // hide the "FRIENDSHIP TRAINING!" splash content (no flicker)
        if let Ok(mut g) = PENDING_TAG_CB.lock() {
            *g = Some(crate::il2cpp::GCHandle::new(cb, false)); // fire onDone early next frame; old handle freed on drop
        }
        TRAIN_SKIPS.fetch_add(1, Ordering::Relaxed);
    }
}
pub(crate) unsafe extern "C" fn on_tag_play_cutin_out(this: *mut c_void, cb: *mut c_void, m: *mut c_void) {
    let t = TR_TAGOUT.load(Ordering::Relaxed);
    if t != 0 {
        let f: TagOutFn = std::mem::transmute(t);
        f(this, cb, m);
    }
    if is_enabled() && !in_overseer() && !cb.is_null() {
        if let Ok(mut g) = PENDING_TAG_CB.lock() {
            *g = Some(crate::il2cpp::GCHandle::new(cb, false)); // old handle freed on drop
        }
    }
}

pub(crate) unsafe fn fire_action(cb: usize) {
    if cb == 0 {
        return;
    }
    if let Some(inv) = ACTION_INVOKE.get() {
        if inv.ok() {
            let _g = ReentryGuard::enter();
            inv.call_void(cb as *mut c_void);
        }
    }
}

/// Fire all deferred callbacks (friendship onDone + shop perf callbacks) on a clean
/// main-thread frame (from the ButtonCommon.Update tick), avoiding re-entrancy.
pub(crate) fn pump_pending_tag_cb() {
    use crate::skip::shop;
    // Re-fetch the delegate through its strong handle (rooted since capture, so never freed
    // memory); the handle drops — un-rooting the closure — only AFTER the Invoke.
    if let Some(h) = PENDING_TAG_CB.lock().ok().and_then(|mut g| g.take()) {
        unsafe { fire_action(h.target() as usize) };
    }
    // Close the skipped use-item performance's dialogs BEFORE its callbacks reopen the list.
    if shop::SHOP_TEARDOWN.swap(false, Ordering::Relaxed) {
        let _g = ReentryGuard::enter();
        shop::shop_teardown();
    }
    for h in shop::drain_pending() {
        unsafe { fire_action(h.target() as usize) }; // each handle drops right after its Invoke
    }
    // After a use-item performance was driven to completion (coroutine), auto-close the item
    // list so the player lands on the career screen instead of back on the list.
    if shop::SHOP_CLOSE_LIST.swap(false, Ordering::Relaxed) {
        let _g = ReentryGuard::enter();
        let _ = shop::shop_close_item_list();
        // A NORMAL-item buy shows the exchange-complete dialog via this same use-performance
        // coroutine, so it's THIS path (not the buy-path) that dismisses the dialog. If a buy is
        // pending, arm Back right now instead of letting the buy-path wait out its timeout.
        if shop::SHOP_CLOSE_BUY_UNTIL.swap(0, Ordering::Relaxed) != 0 {
            shop::SHOP_PRESS_BACK_UNTIL.store(now_ms() + 5000, Ordering::Relaxed);
            rr_log(&format!("[shop {}ms] dialog dismissed during buy; back-press armed", now_ms()));
        }
    }
    // After a BUY (exchange), the "Exchange Complete / choose how many" dialog appears a few
    // frames LATER — so we retry closing the forefront dialog over a short window until it shows.
    let close_until = shop::SHOP_CLOSE_BUY_UNTIL.load(Ordering::Relaxed);
    if close_until != 0 {
        if now_ms() < close_until {
            // Normal item: the exchange-complete dialog appears → close it, then arm Back.
            let _g = ReentryGuard::enter();
            if shop::shop_close_item_list() {
                shop::SHOP_CLOSE_BUY_UNTIL.store(0, Ordering::Relaxed);
                shop::SHOP_PRESS_BACK_UNTIL.store(now_ms() + 5000, Ordering::Relaxed);
                rr_log(&format!("[shop {}ms] buy dialog closed; back-press armed", now_ms()));
            }
        } else {
            // Window expired with no dialog ever appearing → it was an AUTO-REDEEM item (stat
            // books etc. apply instantly, no "choose how many" dialog). Just arm Back so the
            // player still lands where their manual Back would.
            shop::SHOP_CLOSE_BUY_UNTIL.store(0, Ordering::Relaxed);
            shop::SHOP_PRESS_BACK_UNTIL.store(now_ms() + 5000, Ordering::Relaxed);
            rr_log(&format!("[shop {}ms] auto-redeem (no dialog); back-press armed", now_ms()));
        }
    }
}
