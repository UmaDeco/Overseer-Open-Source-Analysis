//! [inspire] Phase-0 discovery for the Inspiration ("Succession") auto-skip.
//!
//! DIAGNOSTICS ONLY — every detour here logs "[inspire] <Class.Method> ENTERED" and then calls
//! the original, nothing else. Zero behavior change even with the `skip_inspiration` toggle ON
//! (the toggle still only arms the [button]/[dialog] discovery logging elsewhere). This is the
//! project's hard-won discipline (the Scout/gtrace lesson): metadata existence — and even a
//! non-null il2cpp::method resolve, which WALKS BASE CLASSES — is NOT proof a method fires on
//! the live flow. One inspiration turn with these traces armed settles method ownership, call
//! order and arg counts; only THEN does Phase 1 wire the real invokes (OnClickSuccessionButton
//! via Invokable, SkipRuntime on the cut helper, etc.).
//!
//! Traced (whichever resolve as OWN methods of their class — never via the base-class walk):
//!   • SingleModeSuccessionEventViewController.SetupSuccessionButton / OnClickSuccessionButton
//!     (the GO screen: Setup says when a live controller exists; OnClick is the GO tap handler)
//!   • SingleModeChangeViewManager.ChangeViewSuccessionCutAndStory / ChangeSuccessionEventView
//!     (the career view transitions the GO tap triggers — ChangeMainView's siblings)
//!   • SingleModeSuccessionCutInHelper lifecycle: OnStartCutIn/OnPlayCutIn/OnPlayMainCutIn +
//!     any Play* the class declares (exact naming sibling of SingleModeTrainingCutInHelper,
//!     whose trio train.rs hooks and whose SkipRuntime it invokes)
//! Plus a RESOLVE-ONLY check (never invoked): does SkipRuntime/0 resolve on the succession
//! helper — Phase 2's native cut skip depends on it.

#![allow(dead_code)]

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::OnceLock;

use retour::RawDetour;

use crate::il2cpp;
use crate::skip::{install_one, rr_log};

// ── trace slots — one per traced method (a detour needs its own trampoline) ─────────────────────
// Which method lands in which slot is decided at install time (the Play* sweep is discovered at
// runtime), so the human-readable label lives in LABELS[idx], set BEFORE the detour is enabled.
const MAX_TRACES: usize = 12;

crate::skip_hook_slot!(TR_IT0, D_IT0);
crate::skip_hook_slot!(TR_IT1, D_IT1);
crate::skip_hook_slot!(TR_IT2, D_IT2);
crate::skip_hook_slot!(TR_IT3, D_IT3);
crate::skip_hook_slot!(TR_IT4, D_IT4);
crate::skip_hook_slot!(TR_IT5, D_IT5);
crate::skip_hook_slot!(TR_IT6, D_IT6);
crate::skip_hook_slot!(TR_IT7, D_IT7);
crate::skip_hook_slot!(TR_IT8, D_IT8);
crate::skip_hook_slot!(TR_IT9, D_IT9);
crate::skip_hook_slot!(TR_IT10, D_IT10);
crate::skip_hook_slot!(TR_IT11, D_IT11);

static LABELS: [OnceLock<String>; MAX_TRACES] = [
    OnceLock::new(), OnceLock::new(), OnceLock::new(), OnceLock::new(),
    OnceLock::new(), OnceLock::new(), OnceLock::new(), OnceLock::new(),
    OnceLock::new(), OnceLock::new(), OnceLock::new(), OnceLock::new(),
];
static HIT_COUNTS: [AtomicU32; MAX_TRACES] = [
    AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0),
    AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0),
    AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0),
];

/// Per-method log cap. gtrace never needed one because its four targets were one-shot lifecycle
/// methods; the Play* sweep here could land on something per-frame, and a flooded session log
/// would bury the very call-order evidence Phase 0 exists to collect.
const TRACE_LOG_CAP: u32 = 60;

fn trace_entered(idx: usize) {
    let n = HIT_COUNTS[idx].fetch_add(1, Ordering::Relaxed);
    if n >= TRACE_LOG_CAP {
        return;
    }
    let label = LABELS[idx].get().map(|s| s.as_str()).unwrap_or("?");
    if n + 1 == TRACE_LOG_CAP {
        rr_log(&format!("[inspire] {label} ENTERED (cap reached — further entries suppressed)"));
    } else {
        rr_log(&format!("[inspire] {label} ENTERED"));
    }
}

// Pure log-and-passthrough (the gtrace pattern, generalized): every arg is forwarded through the
// four integer argument registers (this + up to 2 params + the trailing MethodInfo*) and the
// return value is forwarded from rax — so refs/ints/bools/enums and void/pointer returns all pass
// through untouched at any arity ≤ 2 (a hidden struct-return pointer also just rides along). A
// FLOAT param would ride an XMM register instead and would NOT survive the logging call, which is
// why arm_trace refuses arity > 2 and this stays pointed at lifecycle/UI methods — this family
// takes only references/ints in this game. NOTHING else is done here: no toggle checks, no state,
// no game calls beyond the original.
type Fwd4 = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> *mut c_void;
macro_rules! itrace {
    ($fname:ident, $idx:expr, $slot:ident) => {
        unsafe extern "C" fn $fname(
            a: *mut c_void,
            b: *mut c_void,
            c: *mut c_void,
            d: *mut c_void,
        ) -> *mut c_void {
            trace_entered($idx);
            let t = $slot.load(Ordering::Relaxed);
            if t != 0 {
                let f: Fwd4 = std::mem::transmute(t);
                f(a, b, c, d)
            } else {
                std::ptr::null_mut()
            }
        }
    };
}
itrace!(it0, 0, TR_IT0);
itrace!(it1, 1, TR_IT1);
itrace!(it2, 2, TR_IT2);
itrace!(it3, 3, TR_IT3);
itrace!(it4, 4, TR_IT4);
itrace!(it5, 5, TR_IT5);
itrace!(it6, 6, TR_IT6);
itrace!(it7, 7, TR_IT7);
itrace!(it8, 8, TR_IT8);
itrace!(it9, 9, TR_IT9);
itrace!(it10, 10, TR_IT10);
itrace!(it11, 11, TR_IT11);

type Slot = (*const (), &'static AtomicUsize, &'static OnceLock<RawDetour>);

/// Arity of `name` among the class's OWN methods. class_get_methods does NOT walk base classes
/// (unlike il2cpp::method), so a hit here is real ownership — the distinction the Gacha BeginView
/// trap made load-bearing. None = the class itself declares no such method. First overload wins
/// (fine for diagnostics; the [inspire] armed line includes the arity so ambiguity is visible).
fn own_method_argc(k: il2cpp::Class, name: &str) -> Option<i32> {
    for entry in il2cpp::class_methods(k) {
        if let Some((n, pc)) = entry.rsplit_once('/') {
            if n == name {
                return pc.parse::<i32>().ok();
            }
        }
    }
    None
}

/// Try to arm one passthrough trace on `cls_label.name`. Logs every miss flavor distinctly:
/// not-declared vs base-walk-only (called out, NOT hooked — a base detour would blanket every
/// sibling class, e.g. the training cut-in helper) vs arity-too-wide vs install failure. On
/// success the slot's label is set BEFORE the detour goes live so ENTERED lines are always named.
fn arm_trace(
    k: il2cpp::Class,
    cls_label: &str,
    name: &str,
    slots: &[Slot; MAX_TRACES],
    next: &mut usize,
    armed: &mut Vec<String>,
    notes: &mut String,
) {
    let label = format!("{cls_label}.{name}");
    let argc = match own_method_argc(k, name) {
        Some(a) => a,
        None => {
            if !il2cpp::method(k, name, -1).is_null() {
                rr_log(&format!(
                    "[inspire] {label}: base-class resolve ONLY — not hooked (a base detour would blanket every sibling)"
                ));
            } else {
                rr_log(&format!("[inspire] {label}: miss"));
            }
            return;
        }
    };
    if argc > 2 {
        rr_log(&format!(
            "[inspire] {label}/{argc}: exists but too many args for the 4-reg passthrough — logged only, not hooked"
        ));
        return;
    }
    if *next >= MAX_TRACES {
        rr_log(&format!("[inspire] {label}/{argc}: exists but all {MAX_TRACES} trace slots are used"));
        return;
    }
    let (f, tr, d) = slots[*next];
    let _ = LABELS[*next].set(format!("{label}/{argc}"));
    *next += 1; // slot is burned even on install failure (its OnceLock label is already set)
    match unsafe { install_one(k, name, argc, f, tr, d) } {
        Ok(()) => armed.push(format!("{label}/{argc}")),
        Err(e) => notes.push_str(&format!("inspire {e}; ")),
    }
}

/// Install every Phase-0 trace that resolves as an OWN method of its class, and log the full
/// resolve map. Called from skip::install(); every miss is a log/notes line, never a failure —
/// the game runs identically whether zero or all traces armed.
pub(crate) fn install(notes: &mut String) {
    let slots: [Slot; MAX_TRACES] = [
        (it0 as *const (), &TR_IT0, &D_IT0),
        (it1 as *const (), &TR_IT1, &D_IT1),
        (it2 as *const (), &TR_IT2, &D_IT2),
        (it3 as *const (), &TR_IT3, &D_IT3),
        (it4 as *const (), &TR_IT4, &D_IT4),
        (it5 as *const (), &TR_IT5, &D_IT5),
        (it6 as *const (), &TR_IT6, &D_IT6),
        (it7 as *const (), &TR_IT7, &D_IT7),
        (it8 as *const (), &TR_IT8, &D_IT8),
        (it9 as *const (), &TR_IT9, &D_IT9),
        (it10 as *const (), &TR_IT10, &D_IT10),
        (it11 as *const (), &TR_IT11, &D_IT11),
    ];
    let mut next = 0usize;
    let mut armed: Vec<String> = Vec::new();
    // PHASE 0 IS ANSWERED. The live traces settled the flow (see the Phase-1 banner below), so they
    // are no longer armed by default: each was a permanent detour producing nothing, and two of them
    // sat on the exact methods Phase 1 must hook (`install_one` refuses an already-detoured address,
    // so leaving them armed would silently disable the feature). They stay behind verbose logging
    // for the next time this flow changes.
    let phase0 = crate::tools::log::trace_enabled();

    // GO screen: the controller that owns the "SuccessionButton" GameObject the [button] diagnostic
    // already identified. Setup tells us when a live instance exists; OnClick is the handler a
    // Phase-1 controller-driven invoke would fire.
    let evc = il2cpp::class("Gallop.SingleModeSuccessionEventViewController");
    if evc.is_null() {
        rr_log("[inspire] Gallop.SingleModeSuccessionEventViewController: class miss");
        notes.push_str("inspire evc miss; ");
    } else if phase0 {
        for name in ["SetupSuccessionButton", "OnClickSuccessionButton"] {
            arm_trace(evc, "SingleModeSuccessionEventViewController", name, &slots, &mut next, &mut armed, notes);
        }
    }

    // The career view transitions the GO tap triggers — same manager whose ChangeMainView
    // result.rs already hooks (different methods, so no detour collision; install_one refuses
    // an already-detoured address anyway).
    let cvm = il2cpp::class("Gallop.SingleModeChangeViewManager");
    if cvm.is_null() {
        rr_log("[inspire] Gallop.SingleModeChangeViewManager: class miss");
        notes.push_str("inspire cvm miss; ");
    } else if phase0 {
        for name in ["ChangeViewSuccessionCutAndStory", "ChangeSuccessionEventView"] {
            arm_trace(cvm, "SingleModeChangeViewManager", name, &slots, &mut next, &mut armed, notes);
        }
    }

    // Succession cut-in helper — exact naming sibling of SingleModeTrainingCutInHelper (whose
    // OnStartCutIn/OnPlayCutIn/OnPlayMainCutIn train.rs hooks and whose SkipRuntime it invokes).
    // Probe the same lifecycle trio + any Play* the class declares, and record whether SkipRuntime
    // resolves. RESOLVE-ONLY on SkipRuntime: it is never invoked here.
    let helper = il2cpp::class("Gallop.SingleModeSuccessionCutInHelper");
    if helper.is_null() {
        rr_log("[inspire] Gallop.SingleModeSuccessionCutInHelper: class miss");
        notes.push_str("inspire helper miss; ");
    } else if phase0 {
        let fixed = ["OnStartCutIn", "OnPlayCutIn", "OnPlayMainCutIn"];
        for name in fixed {
            arm_trace(helper, "SingleModeSuccessionCutInHelper", name, &slots, &mut next, &mut armed, notes);
        }
        // Play* sweep — own methods only, deduped (overload names repeat in class_methods).
        let mut seen: HashSet<String> = HashSet::new();
        for entry in il2cpp::class_methods(helper) {
            let name = entry.split('/').next().unwrap_or("").to_string();
            if !name.starts_with("Play") || !seen.insert(name.clone()) {
                continue;
            }
            arm_trace(helper, "SingleModeSuccessionCutInHelper", &name, &slots, &mut next, &mut armed, notes);
        }
        // Both views matter: il2cpp::method walks base classes (so HIT may be a base-class
        // blanket), own_method is real ownership. Phase 2's native cut skip hangs on this answer.
        let walk = !il2cpp::method(helper, "SkipRuntime", 0).is_null();
        let own = own_method_argc(helper, "SkipRuntime") == Some(0);
        rr_log(&format!(
            "[inspire] SingleModeSuccessionCutInHelper.SkipRuntime/0: resolve={} own_method={own}",
            if walk { "HIT" } else { "miss" }
        ));
    }

    crate::tools::trace(&format!(
        "[inspire] Phase-0 traces armed on: {}",
        if armed.is_empty() { "(none)".to_string() } else { armed.join(" ") }
    ));

    // ── Phase 1 ──────────────────────────────────────────────────────────────────────────────────
    install_phase1(evc, notes);
}

// ════════════════════════════════════════════════════════════════════════════════════════════════
// PHASE 1 — auto-advance the Inspiration ("Succession") event.
//
// The Phase-0 discipline was: wire nothing until ENTERED lines appear in a live run. They have
// (field log 2026-07-26, two separate inspiration turns), and they settled the flow exactly:
//
//     [inspire] SingleModeChangeViewManager.ChangeSuccessionEventView/0   ENTERED
//     [inspire] SingleModeSuccessionEventViewController.SetupSuccessionButton/0 ENTERED
//     [inspire] SingleModeSuccessionEventViewController.OnClickSuccessionButton/0 ENTERED
//
// So: the manager swaps to the succession view, the controller sets up its GO button, and the
// player's tap lands on `OnClickSuccessionButton`. That last one is the game's OWN handler, which is
// what we invoke — the same "press the button the player would press, through the game's own code
// path" approach as `RaceUIFastForward.OnPushFastForward` and `StoryChoiceController.ChoiceButton`,
// and deliberately NOT a synthetic UI click (those re-enter the EventSystem, which is what froze the
// skill-learning flow).
//
// The same probe also settled the OTHER half of the question, negatively:
//     SingleModeSuccessionCutInHelper.SkipRuntime/0: resolve=miss own_method=false
// That class declares no SkipRuntime of its own, so there is no training-cut-in-style native skip
// for the inspiration cut. The cut is instead collapsed by the ordinary event path: the story
// timeline it plays goes through `StoryViewController`, which the Events skip already handles. That
// is why the user saw "only the stats are fast-forwarded" — the GO tap was never automated, so the
// whole flow waited on a manual press before the event skip could do its part.
// ════════════════════════════════════════════════════════════════════════════════════════════════

/// Live `SingleModeSuccessionEventViewController`, captured at `SetupSuccessionButton`. Strong
/// GCHandle for the same reason every other captured controller uses one: the GC cannot see Rust
/// statics, so a raw pointer here would be a use-after-free the moment the view tore down.
static EVC_H: std::sync::Mutex<Option<il2cpp::GCHandle>> = std::sync::Mutex::new(None);
/// `OnClickSuccessionButton` — (code, MethodInfo*).
static ON_CLICK: AtomicUsize = AtomicUsize::new(0);
static ON_CLICK_MI: AtomicUsize = AtomicUsize::new(0);
/// Controller instance we have already pressed GO on (per-instance dedup).
static PRESSED_FOR: AtomicUsize = AtomicUsize::new(0);
static LAST_PRESS_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// When the CURRENT controller was captured. `drive` waits [`SETTLE_MS`] past this before pressing.
static CAPTURED_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// How long the inspiration view must have been up before we touch its button.
///
/// `SetupSuccessionButton` runs while the view is still being built, and the click handler it wires
/// tears that same view down. Pressing too early is a way to leave the screen half-initialised with
/// an inert button — i.e. exactly the "can't click the Inspiration button" lock. Waiting costs a
/// third of a second on a flow that already takes several, and removes the whole failure class.
const SETTLE_MS: u64 = 350;
crate::skip_hook_slot!(TR_SETUP, D_SETUP);

/// `SetupSuccessionButton()` — the GO screen is live. Capture the controller and re-arm the dedup.
unsafe extern "C" fn on_setup_button(this: *mut c_void, m: *mut c_void) {
    if !this.is_null() {
        if let Ok(mut g) = EVC_H.lock() {
            *g = Some(il2cpp::GCHandle::new(this, false)); // old handle freed on drop
        }
        if PRESSED_FOR.load(Ordering::Relaxed) != this as usize {
            PRESSED_FOR.store(0, Ordering::Relaxed); // fresh instance → one press allowed
            // Start the settle clock for THIS instance — see `drive`.
            CAPTURED_MS.store(crate::skip::now_ms(), Ordering::Relaxed);
        }
    }
    crate::skip::call_orig(&TR_SETUP, this, m);
}

/// Press GO on the captured inspiration controller, once per instance.
///
/// Driven from the per-sweep block in `result.rs` (not from inside `SetupSuccessionButton`): invoking
/// the click handler from within the setup call would re-enter the controller mid-initialisation.
pub(crate) unsafe fn drive() {
    if !crate::skip::is_inspiration_enabled() || crate::hooks::in_overseer() {
        return;
    }
    // Never act under a dialog — the same hands-off rule every other auto-action follows.
    if crate::skip::dialog_open() {
        return;
    }
    let code = ON_CLICK.load(Ordering::Relaxed);
    if code == 0 {
        return;
    }
    let ctrl = match EVC_H.lock() {
        Ok(g) => g.as_ref().map_or(std::ptr::null_mut(), |h| h.target()),
        Err(_) => std::ptr::null_mut(),
    };
    if ctrl.is_null() || PRESSED_FOR.load(Ordering::Relaxed) == ctrl as usize {
        return;
    }
    if il2cpp::unity_object_destroyed(ctrl) {
        if let Ok(mut g) = EVC_H.lock() {
            *g = None;
        }
        return;
    }
    // ── THE PRESS IS DISABLED ────────────────────────────────────────────────────────────────────
    //
    // What used to be here: `OnClickSuccessionButton` invoked directly on the captured controller.
    // It is gone, and the reasons are worth keeping, because every one of them was a guess that
    // looked reasonable at the time.
    //
    //  * **It runs inside `ButtonCommon.Update`.** `drive()` is called from the per-sweep block at
    //    `result.rs:294`, which is inside that detour. So the invoke re-entered the game's UI update
    //    from within the game's UI update — the same re-entrant stack this codebase had already
    //    identified as a hard-freeze class and moved every comparable click away from.
    //  * **Correlation in the field log.** All three presses in the deployed build (09:36:56,
    //    09:38:02, 11:19:39) are each the LAST main-thread line their session emitted; the last one
    //    was followed by nine minutes of silence and `[guard] recovered: event skip did not advance`.
    //    Every *manual* tap of the same method advanced the flow normally.
    //  * **The readiness gate never worked.** `SETTLE_MS` was stamped in `on_setup_button` BEFORE
    //    `call_orig`, so it measured from before the view had even begun building. Observed
    //    human Setup->click latency on the same screen is 2-5 seconds; we pressed at +350 ms.
    //  * **The press was one-shot, unverified and unrecoverable.** `PRESSED_FOR` was consumed
    //    before the call and success was logged unconditionally, so a press that killed the screen
    //    was indistinguishable from one that worked — and turning the feature off afterwards could
    //    not un-stick the view.
    //
    // Honest limit on the evidence: sessions after 11:34 contain zero presses and the screen was
    // still reported stuck, so this is the best single candidate, NOT a proven cause. That is
    // precisely why the fix is a removal — it costs nothing if the diagnosis is wrong.
    //
    // What is deliberately NOT done: routing the invoke through `mainthread::post` /
    // `click_deferred`. `drain_fallback()` runs eight lines above the caller, inside the same
    // detour, so a posted closure lands on the identical stack; and we hold a controller handle,
    // not a `ButtonCommon`, so it would mean synthesising an EventSystem click — the very thing
    // `ui_input.rs` blames for the real freezes.
    //
    // The capture hook, the resolve and the toggle all stay, so re-enabling is a one-function change
    // once probe P1 (`[ctrace] click ->`, armed via the text trace) shows whether a player's tap
    // even reaches this button.
    let _ = (code, ctrl);
    if PRESSED_FOR.swap(ctrl as usize, Ordering::Relaxed) != ctrl as usize {
        rr_log("[inspire] succession view is up — auto-press is disabled, tap GO! yourself");
    }
}

/// Clear the capture when the flow is over (career view change / lobby).
pub(crate) fn clear() {
    if let Ok(mut g) = EVC_H.lock() {
        *g = None;
    }
    PRESSED_FOR.store(0, Ordering::Relaxed);
    CAPTURED_MS.store(0, Ordering::Relaxed);
}

/// Resolve `OnClickSuccessionButton` and hook `SetupSuccessionButton` for the capture.
fn install_phase1(evc: il2cpp::Class, notes: &mut String) {
    if evc.is_null() {
        return;
    }
    // OWN method only — `il2cpp::method` walks base classes, and a base-class handler would be the
    // wrong instance entirely. The probe confirmed both of these are declared on this class.
    if own_method_argc(evc, "OnClickSuccessionButton") != Some(0) {
        notes.push_str("inspire: OnClickSuccessionButton not own/0-arg; ");
        return;
    }
    let m = il2cpp::method(evc, "OnClickSuccessionButton", 0);
    let p = il2cpp::method_pointer(m);
    if m.is_null() || p.is_null() {
        notes.push_str("inspire: OnClickSuccessionButton ptr miss; ");
        return;
    }
    ON_CLICK.store(p as usize, Ordering::Relaxed);
    ON_CLICK_MI.store(m as usize, Ordering::Relaxed);

    // OWN method only, same rule as above: `install_one` resolves through `il2cpp::method`, which
    // walks base classes, and detouring a BASE `SetupSuccessionButton` would fire for every sibling
    // view controller that inherits it — a broad, hard-to-attribute way to break unrelated buttons.
    // The probe confirmed it is declared on this class; this makes that a checked precondition
    // rather than a remembered one.
    if own_method_argc(evc, "SetupSuccessionButton") != Some(0) {
        notes.push_str("inspire: SetupSuccessionButton not own/0-arg; ");
        rr_log("[inspire] auto-advance NOT wired (SetupSuccessionButton is not an own 0-arg method)");
        return;
    }
    // The capture hook. `install_one` refuses an address that is already detoured, so if the
    // Phase-0 trace took this method first we simply keep the resolve and lose the capture — hence
    // Phase 0's traces are now verbose-only (they answered their question and no longer arm here).
    let hooked = unsafe {
        crate::skip::install_one(
            evc,
            "SetupSuccessionButton",
            0,
            on_setup_button as *const (),
            &TR_SETUP,
            &D_SETUP,
        )
    }
    .is_ok();
    if !hooked {
        notes.push_str("inspire: SetupSuccessionButton capture miss; ");
    }
    rr_log(&format!(
        "[inspire] auto-advance wired (capture={hooked} onclick=true) — default OFF"
    ));
}
