//! Overseer — UI tempo.
//!
//! The game animates almost its entire interface (menu opens, panel slides, screen
//! transitions, button feedback) through **DOTween**. We speed those up.
//!
//! We hook DOTween's per-frame driver `DG.Tweening.Core.TweenManager.Update(type,
//! deltaTime, independentTime)` and scale the time deltas it receives. This intercepts
//! the CONSUMER of the clock, so it's immune to anything that resets DOTween's global
//! `timeScale` mid-session — e.g. certain event popups (the "mood" event) reset it to 1,
//! which defeated the earlier approach of writing the global field directly.
//!
//!   tempo 1.0 = stock · >1 = snappier UI (10x ≈ near-instant).

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;

use retour::RawDetour;

use crate::il2cpp;

/// Highest UI speed we allow. Raised 5x -> 20x once the pump moved from delta-scaling to sub-stepping
/// (which can't overshoot). Also bounds the per-frame sub-step loop, so the cost stays predictable.
pub const MAX_TEMPO: f32 = 20.0;

// Requested tempo, stored as f32 bits. 1.0 = stock.
static TEMPO: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0f32

/// Frame-rate FLOOR the sub-step pump is allowed to spend down to, in microseconds per frame.
///
/// ## Why a floor and not a fraction
///
/// The previous cut budgeted a fixed *share* of the measured frame (20%). That is backwards, and
/// the telemetry showed it plainly: at 522 fps the frame is 2.64 ms, so a 20% share is **0.53 ms**
/// — six times LESS pump time than the old fixed 3 ms, on a machine with more headroom than it has
/// ever had. The faster the game ran, the harder the pump was throttled. Skips got slower.
///
/// What actually governs skip speed is *sub-steps per second*, not frame rate. With a per-frame
/// budget `B` and a per-step cost `c`, the pump lands `(B/c)` steps in a frame that now takes
/// `game + B`, i.e. `(B/c)/(game+B)` steps per second — **monotonically increasing in B**, and
/// saturating at `1/c`. There is no frame rate at which withholding budget makes a skip finish
/// sooner; high fps only ever helped because it came with more pump opportunities.
///
/// So the budget is set from what is LEFT after the game's own cost, down to a frame-rate floor:
///
/// ```text
/// budget = TARGET_FRAME_US - (measured_frame - what_the_pump_itself_spent)
/// ```
///
/// At 522 fps that yields ~6 ms instead of 0.53 ms (a ~3.7× improvement in steps/second) and settles
/// the frame rate at the floor. On a heavy scene where the game alone already exceeds the floor, the
/// subtraction goes negative and the pump backs off to [`BUDGET_MIN_US`] on its own — no tier, no
/// setting, no special case. That is the whole point of measuring the game's cost separately.
///
/// ## The floor follows the requested tempo
///
/// A single fixed floor can't serve both ends of the slider. Someone on 2× wants the game to stay
/// smooth; someone on 20× has explicitly asked for "as instant as it can get" and would rather spend
/// frame time than wait. So the floor slides with the request: [`FLOOR_FPS_MIN_TEMPO`] at the bottom
/// of the range down to [`FLOOR_FPS_MAX_TEMPO`] at the top.
///
/// Concretely, with the game itself costing ~5.5 ms/frame here: at 120 fps the pump gets 2.9 ms, at
/// 60 fps it gets 11 ms — for roughly **twice** the sub-steps per second. That is the trade the top
/// of the slider is asking for, and it is why the frame rate visibly dips during a fast skip. That
/// dip is the pump doing the work; it is not the fault the 200→18 fps collapse was (that was
/// synchronous file logging on the render thread, fixed separately).
const FLOOR_FPS_MIN_TEMPO: f32 = 144.0; // at 1–2× — stay smooth
const FLOOR_FPS_MAX_TEMPO: f32 = 60.0; // at 20× — spend the frame, finish the skip
const FLOOR_TEMPO_LO: f32 = 2.0;
const FLOOR_TEMPO_HI: f32 = 20.0;

/// Frame-time target for the CURRENT requested tempo, in microseconds.
fn target_frame_us() -> u64 {
    let t = tempo().clamp(FLOOR_TEMPO_LO, FLOOR_TEMPO_HI);
    let k = (t - FLOOR_TEMPO_LO) / (FLOOR_TEMPO_HI - FLOOR_TEMPO_LO);
    let fps = FLOOR_FPS_MIN_TEMPO + k * (FLOOR_FPS_MAX_TEMPO - FLOOR_FPS_MIN_TEMPO);
    (1_000_000.0 / fps.max(1.0)) as u64
}
/// Floor/ceiling on the budget, so a pathological frame time can't starve or blow out the pump.
const BUDGET_MIN_US: u64 = 250;
// Ceiling sized so it does not silently undercut the LOWEST floor: at 60 fps (16.7 ms) with a cheap
// scene the subtraction can legitimately ask for ~14 ms, and clamping that back to 8 would quietly
// reimpose a 120 fps floor at the top of the slider — the same "budget is smaller than it looks"
// mistake in a new place. The clamp exists only to catch a pathological frame-time measurement.
const BUDGET_MAX_US: u64 = 15_000;

/// EMA of the real frame interval, microseconds. Seeded on the first frame.
static FRAME_US_EMA: AtomicU64 = AtomicU64::new(0);
/// Process-clock microseconds at the previous frame (for the EMA above).
static LAST_FRAME_US: AtomicU64 = AtomicU64::new(0);
/// EMA of what the sub-step loop itself spent, microseconds. Subtracted from the frame interval to
/// recover the GAME's own cost — without this the budget would chase its own tail, since every
/// microsecond the pump spends lands in the next frame's measurement and would look like game load.
static PUMP_US_EMA: AtomicU64 = AtomicU64::new(0);

/// Measured frame interval in ms and the current sub-step budget in ms — surfaced on the System
/// page so the pump's share of the frame is visible instead of inferred.
pub fn frame_stats() -> (f32, f32) {
    (
        FRAME_US_EMA.load(Ordering::Relaxed) as f32 / 1000.0,
        step_budget_us() as f32 / 1000.0,
    )
}

/// The sub-step budget for this frame, in microseconds.
fn step_budget_us() -> u64 {
    let frame = FRAME_US_EMA.load(Ordering::Relaxed);
    if frame == 0 {
        return BUDGET_MIN_US; // first frame: no measurement yet
    }
    // The game's own cost = what the frame took MINUS what we spent inside it. Saturating, because
    // the two EMAs move independently and the pump's can briefly exceed the frame's.
    let game = frame.saturating_sub(PUMP_US_EMA.load(Ordering::Relaxed));
    target_frame_us()
        .saturating_sub(game)
        .clamp(BUDGET_MIN_US, BUDGET_MAX_US)
}
/// Cap on carried-over sub-steps, so a sustained overload can't bank a huge backlog and then
/// fast-forward the UI in one visible burst when the load drops. Generous, because with a real
/// budget the debt now drains within a frame or two instead of being permanently saturated.
const MAX_DEBT_STEPS: f32 = 16.0;
/// Unspent sub-steps carried to the next frame (f32 bits).
static TEMPO_DEBT: AtomicU32 = AtomicU32::new(0);
static TRAMP: AtomicUsize = AtomicUsize::new(0);
static DETOUR: OnceLock<RawDetour> = OnceLock::new();

pub fn tempo() -> f32 {
    // Master switch off → stock speed (1.0), regardless of the stored tempo (which is preserved).
    if !crate::settings::bot_enabled() {
        return 1.0;
    }
    f32::from_bits(TEMPO.load(Ordering::Relaxed))
}

/// The user's CHOSEN tempo, ignoring the master-switch gate. For persistence and display only —
/// never for driving the tween pump. `save_current()` must use this, not `tempo()`: persisting the
/// gated value while the bot was off overwrote the user's stored speed with 1.0.
pub fn stored_tempo() -> f32 {
    f32::from_bits(TEMPO.load(Ordering::Relaxed))
}

// The main-menu (Home lobby) runs looping decorative animations — the character's "live" idle, pulsing
// badges, the event-banner shine. Speeding those up (even smoothly via sub-stepping) makes them loop
// several times a second, which reads as FLASHING. There's nothing to skip on the idle lobby, so we
// pin the effective tempo to 1x while it's on screen. Set true by HomeViewController.PlayInView and
// false by PlayOutView / a career's ChangeMainView (skip::result). tempo() itself is left alone so the
// UI still shows the user's chosen speed.
static HOME_ACTIVE: AtomicBool = AtomicBool::new(false);
pub fn set_home_active(on: bool) {
    HOME_ACTIVE.store(on, Ordering::Relaxed);
}
pub fn home_active() -> bool {
    HOME_ACTIVE.load(Ordering::Relaxed)
}

// The Scout/Gacha view gets an UNCONDITIONAL 1x pin: its banner intro runs one-shot tween state
// machines synchronized against real-time VP9 movie playback + async loads. At 10-20x the tween side
// finishes far ahead of the movie/load events it's married to (an input-unblock callback fires before
// its listener registers), which SOFT-LOCKS the screen — the user-reported "game soft locks going to
// Scout".
//
// STATUS: THE PIN FIRES. Measured over 36 sessions of field logs (2026-07-26): both hooks report
// 19 engagements each, one per scout-open. An earlier comment here insisted it had "NEVER FIRED"
// based on a log where `InitializeView` simply had no log line of its own; that claim was wrong and
// sent two investigations hunting for an entry point that had already been found. The SOFT-LOCK
// theory the pin defends against is still untested (nobody has run Scout at 20x with the pin
// removed), but the mechanism is live and cheap, so it stays.
// Cleared when the lobby is reached (home_in). A 60s cap is a hard safety so the pin can NEVER get
// stuck ON (which would trap the whole game at 1x) — keep it until a real view-close release exists.
static GACHA_ACTIVE: AtomicBool = AtomicBool::new(false);
static GACHA_SINCE_MS: AtomicU64 = AtomicU64::new(0);
const GACHA_PIN_CAP_MS: u64 = 60_000;
pub fn set_gacha_active(on: bool) {
    GACHA_ACTIVE.store(on, Ordering::Relaxed);
    if on {
        GACHA_SINCE_MS.store(crate::tools::now_ms(), Ordering::Relaxed);
    }
}
pub fn gacha_active() -> bool {
    GACHA_ACTIVE.load(Ordering::Relaxed)
        && crate::tools::now_ms().saturating_sub(GACHA_SINCE_MS.load(Ordering::Relaxed)) < GACHA_PIN_CAP_MS
}

// ── STORY / EVENT ceiling ────────────────────────────────────────────────────────────────────────
// The real, MEASURED soft-lock (unlike the gacha pin above): a career/post-race event's story +
// choice flow is a tween-driven state machine synchronized with async loads. At 10-20x the tween side
// outruns the events it's married to and the CHOICE UI never becomes interactive — no auto-choice can
// fire and the screen stalls (fps collapses, main thread still ticks).
//
// This USED to hold tempo at 1x here, which cost the whole slider across every career event and made
// the build noticeably slower than the 3.5.16 public one it grew out of. It is now a CEILING instead
// (STORY_MAX_TEMPO) — see that constant for the value it holds, and for what to change if the
// desync ever appears below it.
//
// PRECISE, not time-windowed. The first cut used a 6s re-stamped window; that OSCILLATED tempo
// (1x→20x→1x) as intermittent lobby events armed/expired it, which snapped the journal scroll (a
// DOTween view) and felt slow. This tracks the ACTUAL live state instead:
//   * `event::story_playing()` — the captured StoryViewController's timeline IsPlaying (1x only while
//     a timeline truly plays; the lobby/journal, where nothing plays, run at full tempo → no snap).
//   * `result::choice_pending()` — a choice is open and unselected (edge-set on choice-setup, cleared
//     on select/teardown, hard-capped at 10s). Covers the story→choice gap where the desync forms.
pub fn story_active() -> bool {
    crate::skip::event::story_playing() || crate::skip::result::choice_pending()
}

/// Ceiling applied during the story/event/choice flow — NOT a pin to 1x.
///
/// The soft-lock described above is real and measured, but it was measured at 10-20x, and 20x is a
/// range only this build can reach: the 3.5.16 public build ran the SAME tween pump with NO story
/// gate at all and a slider that stopped at 10x, and its event flow is the one players ask for back.
/// Slamming to 1x therefore gave up the entire slider across every career event — which is most of a
/// career — to defend a failure that only appears above where the proven-good build could even go.
///
/// So: run the user's chosen tempo during story up to this ceiling, and clamp above it. Rather than
/// collapse to stock, it degrades to the fastest speed we have reason to trust.
///
/// 13x is deliberately a STEP INTO the untested band. The desync was reported for "10-20x" without
/// isolating where in that range it starts, and 10x (the 3.5.16 slider maximum) was the last value
/// with evidence behind it. This trades a known-safe 10x for ~30% more speed in the flow that
/// dominates a career, on the basis that the failure is loud and instantly recognisable rather than
/// silent: **the choice screen never becomes interactive** — the buttons render but do not respond,
/// fps drops while the game keeps ticking. If that happens even once, put this back to 10.0.
const STORY_MAX_TEMPO: f32 = 10.0;

/// Highest multiplier driven by plain DELTA-SCALING (one call, both deltas multiplied) before the
/// pump switches to sub-stepping.
///
/// MUST be >= `STORY_MAX_TEMPO`, and the two are raised together. If this were left at 10 while the
/// story ceiling went to 13, every career event would land on the sub-stepping path — the one that
/// costs Nx the tween work and truncates under load — and raising the ceiling would make events
/// SLOWER, not faster. That interaction is the whole reason they are documented as a pair.
///
/// The baseline was the 10x the 3.5.16 public build shipped with, since that build used
/// delta-scaling across its whole slider; 13 follows the story ceiling into the untested band.
///
/// Delta-scaling was abandoned here for a real reason — at high multipliers a tween jumps N x its
/// normal step, OVERSHOOTS its target, and DOTween snaps it back, which strobes looping animations
/// and can skip completion callbacks. What changed is that the strobe half is now handled
/// independently: `set_looper_scale` cancels the speed-up on infinite-loop tweens on EVERY screen,
/// and it runs on both paths. So within the range 3.5.16 proved, delta-scaling is cheap AND calm.
///
/// Above this, sub-stepping takes over: 10-20x is beyond anything delta-scaling has been shown to
/// survive, and there the smoothness is worth paying N x the tween work for.
///
/// Note this pairs with `STORY_MAX_TEMPO`: story/event/choice is clamped to the same value, so the
/// flow that dominates a career always takes the cheap, never-throttled path.
const DELTA_SCALE_MAX: f32 = 10.0;

// ── main-thread heartbeat (read by the watchdog on a BACKGROUND thread) ─────────────────────────
// Bumped at the top of update_hook, i.e. once per game frame. A background reader can compare
// LAST_TICK_MS against wall-clock to tell the two failure modes apart — which look identical to a
// player, and need completely different fixes:
//   ticks FROZEN  -> HANG:  the main thread is blocked/spinning inside something (crashlog step names it)
//   ticks RUNNING -> STALL: the engine is fine; some view is waiting on an event that already passed
static MAIN_TICKS: AtomicU64 = AtomicU64::new(0);
static LAST_TICK_MS: AtomicU64 = AtomicU64::new(0);

/// (frames ticked since boot, wall-clock ms of the last tick). 0 ticks = the tween pump never armed.
pub fn heartbeat() -> (u64, u64) {
    (MAIN_TICKS.load(Ordering::Relaxed), LAST_TICK_MS.load(Ordering::Relaxed))
}

/// Monotonic frame id (one per main-thread tick). Lets a hook that fires once per OBJECT — e.g.
/// `ButtonCommon.Update`, which runs for every button on screen every frame — run its frame-global
/// work exactly once instead of once per object.
#[inline]
pub fn frame_id() -> u64 {
    MAIN_TICKS.load(Ordering::Relaxed)
}
// (effective_tempo_now was removed: it exposed effective_tempo() — which lazily binds DOTween via
// IL2CPP — to the background watchdog thread, a fatal off-thread GC call. effective_tempo() is now
// ONLY reachable from update_hook, the main thread.)

/// The tempo actually applied to the tween pump this frame.
///
/// Normally just the user's tempo: decorative-loop exclusion (below) keeps looping animations at 1x on
/// EVERY screen, so there's nothing left to strobe and even the lobby can run at full speed. The
/// idle-lobby 1x pin is kept only as a FALLBACK for when that exclusion couldn't arm (e.g. a DOTween
/// update renamed its internals) — then at least the main menu, the screen you sit on longest, is calm.
fn effective_tempo() -> f32 {
    // Scout/Gacha: ALWAYS 1x while the view is up — its one-shot intro tweens are synchronized with
    // real-time movie playback, and outrunning them soft-locks the screen (see GACHA_ACTIVE above).
    // Unconditional on purpose: the decorative-loop exclusion only protects LOOPING tweens.
    if gacha_active() {
        return 1.0;
    }
    // Story / event / choice flow — CLAMPED, not pinned. See STORY_MAX_TEMPO.
    if story_active() {
        return tempo().min(STORY_MAX_TEMPO);
    }
    if home_active() && dotween_bind().is_none() {
        return 1.0;
    }
    tempo()
}

// ── decorative-loop exclusion ───────────────────────────────────────────────────────────────────
//
// THE general fix for "high UI speed makes things flash". Sub-stepping removed the overshoot flash,
// but a tween that LOOPS FOREVER (a card's "Recommended!" glow, a pulsing badge, the lobby character's
// live idle) is *meant* to repeat — running it Nx faster just makes it repeat N times a second, which
// reads as a strobe. Those decorations gain nothing from being sped up; only one-shot transitions do.
//
// So each frame we walk DOTween's active-tween list and give every INFINITE-loop tween a per-tween
// timeScale of 1/N. The pump then advances the world by N steps, and those tweens net out at N*(1/N) =
// 1x — exactly their designed speed — while one-shot tweens (timeScale 1) still get the full Nx. This
// is screen-independent, so it fixes every page at once instead of pinning them one by one.
//
// Reads DOTween's own statics (DG.Tweening is a third-party lib, so its field names aren't obfuscated).
// If anything fails to resolve we simply no-op and keep today's behaviour.
struct DoTweenBind {
    active_tweens_addr: usize, // &_activeTweens (Tween[])
    tot_active_addr: usize,    // &totActiveTweens (i32)
    loops_off: usize,          // Tween.loops (i32; < 0 == infinite)
    timescale_off: usize,      // Tween.timeScale (f32)
}
static DOTWEEN: OnceLock<Option<DoTweenBind>> = OnceLock::new();
/// True once we've counter-scaled loopers, so we know to restore them when the speed-up is turned off.
static LOOPERS_SCALED: AtomicBool = AtomicBool::new(false);

/// First static field that resolves out of `names` (DOTween's internals differ slightly by version).
fn first_static_addr(k: il2cpp::Class, names: &[&str]) -> *mut c_void {
    for n in names {
        let a = il2cpp::static_field_addr(k, n);
        if !a.is_null() {
            return a;
        }
    }
    std::ptr::null_mut()
}

fn dotween_bind() -> Option<&'static DoTweenBind> {
    DOTWEEN
        .get_or_init(|| {
            let tm = il2cpp::class("DG.Tweening.Core.TweenManager");
            let tw = il2cpp::class("DG.Tweening.Tween");
            if tm.is_null() || tw.is_null() {
                crate::tools::log("[tempo] DOTween classes not found — decorative-loop exclusion off");
                return None;
            }
            let a = first_static_addr(tm, &["_activeTweens", "activeTweens"]);
            let c = first_static_addr(tm, &["totActiveTweens", "_totActiveTweens"]);
            // `loops`/`timeScale` live on the Tween base class, so the offsets are valid for every
            // Tweener/Sequence subclass stored in the active list.
            let lo = il2cpp::field_offset(tw, "loops");
            let ts = il2cpp::field_offset(tw, "timeScale");
            match (a.is_null(), c.is_null(), lo, ts) {
                (false, false, Some(loops_off), Some(timescale_off)) => {
                    crate::tools::log("[tempo] decorative-loop exclusion armed (looping tweens stay 1x)");
                    Some(DoTweenBind {
                        active_tweens_addr: a as usize,
                        tot_active_addr: c as usize,
                        loops_off,
                        timescale_off,
                    })
                }
                _ => {
                    crate::tools::log("[tempo] DOTween fields not resolved — loop exclusion off (decor may flash at high speed)");
                    None
                }
            }
        })
        .as_ref()
}

/// Set every INFINITE-loop active tween's timeScale to `scale` (1/N to cancel the speed-up, 1.0 to
/// restore). Plain i32/f32 field reads+writes on the main thread — no runtime calls, no GC barrier.
unsafe fn set_looper_scale(scale: f32) {
    let Some(b) = dotween_bind() else { return };
    let arr = *(b.active_tweens_addr as *const il2cpp::Object);
    if arr.is_null() {
        return;
    }
    let tot = *(b.tot_active_addr as *const i32);
    if tot <= 0 {
        return;
    }
    // Il2CppArray: length at +0x18, elements at +0x20 (same layout freecam.rs walks). Clamp against
    // BOTH the array's real length and the active count so a stale count can't run us off the end.
    let arr_len = *((arr as *const u8).add(0x18) as *const i32);
    let n = (tot.min(arr_len).max(0) as usize).min(4096);
    let elems = (arr as *const u8).add(0x20) as *const il2cpp::Object;
    for i in 0..n {
        let t = *elems.add(i);
        if t.is_null() {
            continue;
        }
        if *((t as *const u8).add(b.loops_off) as *const i32) < 0 {
            *((t as *mut u8).add(b.timescale_off) as *mut f32) = scale;
        }
    }
}

/// Set the UI tempo (clamped to 1..=MAX_TEMPO).
pub fn set_tempo(t: f32) {
    // The old 5x ceiling existed ONLY because tempo was applied by delta-SCALING, which made tweens
    // overshoot (flashing) and skip past completion callbacks (soft-locks) at high multipliers.
    // Above `DELTA_SCALE_MAX` the pump SUB-STEPS instead (see update_hook), so nothing overshoots
    // there — each step is a normal delta. That is what lets the slider go to 20x at all. At or below
    // that ceiling it still delta-scales, because it is cheap and never throttles.
    //
    // What still bounds it is REAL time, not tweens: we only accelerate DOTween. The game's real-time
    // systems (asset loads, network requests, WaitForSeconds coroutines) run at wall-clock speed, so at
    // very high tempo the UI can finish its animations well ahead of the data behind them. That's a
    // "wait a beat on a loading screen" annoyance, not a corruption — but it's why 10x+ is experimental.
    TEMPO.store(t.clamp(1.0, MAX_TEMPO).to_bits(), Ordering::Relaxed);
}

/// Apply persisted settings to the UI-tempo module at boot.
pub fn apply(s: &crate::settings::Settings) {
    set_tempo(s.ui_tempo);
}

/// Kept for the overlay's per-frame call site — the hook now does the work, so this is a
/// no-op (no global field to re-assert).
pub fn enforce() {}

// TweenManager.Update is a STATIC method → (updateType, deltaTime, independentTime, MethodInfo*).
type UpdateFn = unsafe extern "C" fn(i32, f32, f32, *mut c_void);

unsafe extern "C" fn update_hook(update_type: i32, dt: f32, idt: f32, mi: *mut c_void) {
    // MAIN-THREAD HEARTBEAT — stamped FIRST, before anything that could wedge. TweenManager.Update is
    // the game's per-frame main-thread tick, so "this counter stopped moving" == "the main thread is
    // stuck", which the watchdog (background thread) reads to tell a HANG from a STALL. Must stay at
    // the very top: stamping it later would make a hang inside our own pumps look like a healthy tick.
    MAIN_TICKS.fetch_add(1, Ordering::Relaxed);
    LAST_TICK_MS.store(crate::tools::now_ms(), Ordering::Relaxed);
    // Real frame interval, in microseconds — `now_ms()` is far too coarse at 160–200 fps (a whole
    // frame is 5–6 ms). This drives the adaptive sub-step budget below, so the pump's share of the
    // frame is measured rather than assumed.
    {
        let now_us = crate::tools::clock().elapsed().as_micros() as u64;
        let prev = LAST_FRAME_US.swap(now_us, Ordering::Relaxed);
        if prev != 0 && now_us > prev {
            let dt = (now_us - prev).min(100_000); // ignore loading-screen gaps
            let ema = FRAME_US_EMA.load(Ordering::Relaxed);
            // 1/8 EMA: settles in a few frames, ignores single-frame spikes.
            FRAME_US_EMA.store(if ema == 0 { dt } else { ema - ema / 8 + dt / 8 }, Ordering::Relaxed);
        }
    }
    let t = TRAMP.load(Ordering::Relaxed);
    if t == 0 {
        return;
    }
    // This is the SINGLE detour on TweenManager.Update (a static, main-thread per-frame tick) and the
    // one main-thread driver for ALL Overseer pumps. hunter previously stacked a SECOND RawDetour on this
    // exact method for the same pumps — two detours on one 5-byte prologue corrupt each other's
    // trampolines and produced the intermittent AV/C++-exception crash stamped "after-tween". Now
    // consolidated here: one detour, every pump. All pumps are idempotent/guarded and safe on the main
    // thread (the only place RequestBase.Send / SoftwareReset may run).
    let pump_t0 = std::time::Instant::now();
    // Soft-lock watchdog FIRST: it only reads and clears atomics (never calls into the game), and
    // running it ahead of the pumps means a leaked latch is already cleared by the time the pumps
    // that depend on it run this frame — recovery within one frame instead of the next.
    crate::crashlog::step("tween:guard-tick");
    crate::guard::tick();
    // The remaining pumps are Overseer BEHAVIOUR, so they follow the master switch. Previously they
    // ran unconditionally — which is precisely why "Overseer disabled" still cost frame time and
    // still let the hunter's auto-roll, the padder and the graphics re-apply act in the background.
    // `mainthread::drain` is deliberately OUTSIDE the gate: it must keep draining or work posted
    // just before the switch flipped would leak (and its jobs are already individually gated).
    let active = crate::runtime::overseer_enabled();
    if active {
        crate::crashlog::step("tween:hunter-pump");
        crate::hunter::frame_pump(); // opponent-hunter jittered auto-roll
        crate::crashlog::step("tween:padder-pump");
        crate::padder::pump(); // Team Trials team-edit apply (main thread = safe for RequestBase.Send)
        crate::crashlog::step("tween:reset-pump");
        crate::reset::poll(); // soft-reset main-thread execution point (guarded, no-op if idle)
        crate::crashlog::step("tween:affinity-poll");
        crate::affinity::poll(); // Legacy Select screen-gate + affinity sampling (main-thread only)
        crate::crashlog::step("tween:gfx-pump");
        crate::performance::graphics::pump(); // push graphics knobs onto the live URP asset
    }
    crate::crashlog::step("tween:mainthread-drain");
    crate::mainthread::drain(); // run il2cpp/GPU work posted from background threads (texture, QS, web)
    // Diagnostic: how long Overseer's own main-thread pumps take this frame (proves we're not the
    // stall). Opt-in now — `note()` takes a mutex and formats a detail string, per frame, forever.
    if crate::loadprof::is_enabled() {
        crate::loadprof::pump(pump_t0.elapsed().as_secs_f64() * 1000.0);
    }
    crate::crashlog::step("tween:story-refresh");
    // Refresh the cached story play-state HERE, on the main thread — it invokes IL2CPP, and it is the
    // ONLY place allowed to. effective_tempo() (and the watchdog's read of it) then only read a cached
    // atomic, so nothing off-thread ever touches managed code (fatal "Collecting from unknown thread").
    // Skipped while suspended: it makes two managed calls per frame purely to drive the story
    // ceiling, and the master switch already forces stock speed, so there is nothing to decide.
    if active {
        crate::skip::event::refresh_story_state();
    }
    crate::crashlog::step("tween:scale-orig");
    let f: UpdateFn = std::mem::transmute(t);
    let s = effective_tempo();
    // Cancel the speed-up for decorative INFINITE-loop tweens so they don't strobe (see above). Only
    // walk when we're actually speeding up — plus one restore pass when the speed-up goes away, so a
    // looper can never be left stuck at a fractional timeScale.
    if s > 1.0 {
        // Throttled: this walks DOTween's ENTIRE active-tween list (up to 4096 entries, each a
        // pointer deref plus two field reads) and the value it writes is idempotent, so re-running
        // it every frame was pure overhead. Every ~4 frames still catches a newly created looping
        // tween within a few milliseconds — far below the point where a strobe is visible.
        if MAIN_TICKS.load(Ordering::Relaxed) % 4 == 0 {
            set_looper_scale(1.0 / s);
        }
        LOOPERS_SCALED.store(true, Ordering::Relaxed);
    } else if LOOPERS_SCALED.swap(false, Ordering::Relaxed) {
        set_looper_scale(1.0);
    }
    if s <= 1.0 {
        f(update_type, dt, idt, mi);
        // Stock speed costs the pump nothing, so decay its cost estimate toward zero. Without this
        // the EMA would stay latched at whatever the last skip cost, and the first frames after the
        // tempo comes back up would budget against a phantom load.
        let prev = PUMP_US_EMA.load(Ordering::Relaxed);
        if prev != 0 {
            PUMP_US_EMA.store(prev - prev / 8, Ordering::Relaxed);
        }
    } else if s <= DELTA_SCALE_MAX {
        // ── DELTA-SCALING (the 3.5.16 path) ─────────────────────────────────────────────────────
        // One call, both deltas multiplied. Its virtue is that the cost does NOT scale with the
        // multiplier, so the requested speed is ALWAYS actually delivered: no budget to exhaust, no
        // debt to defer, and no degradation exactly when the screen is busiest. Sub-stepping below
        // is the opposite — it costs N x the tween work per frame, and under a skip flood the budget
        // truncates the loop and carries the remainder, so a nominal 10x lands well short of 10x.
        // That shortfall is what made this build feel slower than 3.5.16 even after the story pin
        // became a ceiling.
        //
        // Scale BOTH clocks: many menu/transition/loader tweens advance on the timeScale-INDEPENDENT
        // delta, and leaving that at real time makes high speed feel only half applied.
        f(update_type, dt * s, idt * s, mi);
        // One call, so there is no meaningful pump cost to charge the budget with — decay it like the
        // stock path, and drop any debt the sub-step path may have carried in before the tempo fell.
        let prev = PUMP_US_EMA.load(Ordering::Relaxed);
        if prev != 0 {
            PUMP_US_EMA.store(prev - prev / 8, Ordering::Relaxed);
        }
        TEMPO_DEBT.store(0f32.to_bits(), Ordering::Relaxed);
    } else {
        // Fast UI via SUB-STEPPING, not delta-scaling. Multiplying the delta by Nx in one call makes
        // every tween jump Nx its normal step and OVERSHOOT its target; DOTween then clamps/snaps back,
        // and looping/idle animations (e.g. the Home character's live idle) strobe — the "flashing".
        // Instead, advance the SAME total time in normal-sized steps: run the original update floor(N)
        // times at the REAL delta, plus one partial step for the fractional part. Identical speed-up,
        // but each tween interpolates smoothly through its keyframes, so nothing overshoots.
        //
        // ── FRAME BUDGET ────────────────────────────────────────────────────────────────────────
        // Sub-stepping costs N × the tween work, every frame. That is fine on a menu with a handful
        // of tweens and catastrophic during a skip flood, which is exactly when the multiplier is
        // turned up: at 20x on a screen with hundreds of active tweens the pump alone can eat tens of
        // milliseconds, and the frame rate collapses — the reported 200 → 18 fps. Worse, it is
        // self-defeating: the skip *feels* slower, because skipping is driven by frames.
        //
        // So the loop is now time-bounded. We advance in normal-sized steps only while we are inside
        // a slice of the frame, and any time we could not spend is carried in `TEMPO_DEBT` and
        // applied on the following frames. Total advanced time is unchanged over any window longer
        // than a frame or two — the UI still runs at the requested multiple — but no single frame can
        // be blown out by the pump. Skips stay fast *because* the frame rate stays high.
        let budget = std::time::Duration::from_micros(step_budget_us());
        let t0 = std::time::Instant::now();
        // Time still owed from previous frames, expressed in whole steps of `dt`.
        let debt = f32::from_bits(TEMPO_DEBT.load(Ordering::Relaxed));
        let want = s + debt;
        let mut steps = want.floor() as i32;
        let mut done = 0i32;
        while done < steps {
            f(update_type, dt, idt, mi);
            done += 1;
            // Always give at least ONE extra step beyond the plain 1x update, so a tight budget can
            // never stall the UI below stock speed.
            if done >= 2 && t0.elapsed() >= budget {
                break;
            }
        }
        // Fractional remainder only if we actually finished the whole-step run.
        if done >= steps {
            let frac = want - steps as f32;
            if frac > 0.001 {
                f(update_type, dt * frac, idt * frac, mi);
            }
            steps = want.ceil() as i32;
        }
        // Carry what we couldn't afford, capped so a sustained overload can't accumulate a
        // multi-second backlog that would then fast-forward the UI in one burst.
        let carried = (want - done as f32).max(0.0).min(MAX_DEBT_STEPS);
        TEMPO_DEBT.store(if done >= steps { 0f32.to_bits() } else { carried.to_bits() }, Ordering::Relaxed);
        // Feed the pump-cost EMA that `step_budget_us` subtracts to recover the game's own frame
        // cost. Measured here rather than derived, because the sub-step count varies per frame.
        let spent = t0.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        let prev = PUMP_US_EMA.load(Ordering::Relaxed);
        PUMP_US_EMA.store(if prev == 0 { spent } else { prev - prev / 8 + spent / 8 }, Ordering::Relaxed);
    }
    // Re-apply MTL translations AFTER the tween update ran this frame (LateUpdate-like timing, as
    // Overseer did). Doing it BEFORE the tween update re-set story-dialogue text while the timeline
    // clip was still mid-reveal, which desynced the "clip finished" gate → no text box + can't
    // advance (soft-lock). After the update, the clip state for this frame is settled first.
    //
    // Gated on translation being live: `pump_inner` takes three mutexes and walks the tracked-
    // component map EVERY frame even with nothing pending, which is pure overhead when translation
    // is off. One run is still allowed right after a switch flip so the eviction sweep frees the
    // GCHandles it is holding (those must be dropped on this thread).
    crate::crashlog::step("tween:mtl-reapply");
    if crate::mtl::translation_active() || crate::mtl::has_tracked() {
        crate::mtl::pump();
    }
    crate::crashlog::step("idle:after-tween");
}

pub fn install() -> Result<&'static str, String> {
    let k = il2cpp::class("DG.Tweening.Core.TweenManager");
    if k.is_null() {
        return Err("TweenManager not found".into());
    }
    let m = il2cpp::method(k, "Update", 3);
    if m.is_null() {
        return Err("Update: not found".into());
    }
    let target = il2cpp::method_pointer(m);
    if target.is_null() {
        return Err("Update: null ptr".into());
    }
    unsafe {
        // Overseer OWNS the UI tempo. If a co-resident mod (e.g. a localization loader) detoured
        // Update FIRST, we CHAIN on top instead of yielding: our detour scales dt/idt, then its
        // trampoline jumps to whatever hooked Update before us, which finally calls the real
        // Update. So the tempo works regardless of load order — no external config needed for
        // Overseer to win the speed. retour relocates the existing jmp prologue into our trampoline.
        let chained = il2cpp::is_detoured(target);
        let d = RawDetour::new(target as *const (), update_hook as *const ())
            .map_err(|e| format!("Update: {e}"))?;
        d.enable().map_err(|e| format!("Update enable: {e}"))?;
        TRAMP.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
        let _ = DETOUR.set(d);
        Ok(if chained {
            "OK (chained on top — Overseer owns speed)"
        } else {
            "OK"
        })
    }
}
