//! Generic UI click/dialog engine — the reusable primitives the SuperSkip
//! (`crate::skip::result` and `crate::skip::shop`) drive game buttons/dialogs with.
//!
//! Resolved (methodPointer code, MethodInfo*) pairs are called DIRECTLY like the JS
//! NativeFunctions (no runtime_invoke), passing the trailing MethodInfo arg. The
//! result-specific gating (whitelist, win/loss, stuck-lift) lives in `crate::skip::result`;
//! this module only owns the mechanical click/close + the button-name cache.

#![allow(dead_code)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::il2cpp;
use crate::skip::clock;
use crate::skip::result::{
    is_advance_button, is_multi, is_press_target, lift_input_block, press_allowed, MULTI_MAX,
    PRESS_GAP_MS, RR_LIFT_GRACE_MS, RR_LIFT_THROTTLE_MS, RR_NEXT_LIFT, RR_PRESSES, WINDOW,
};

/// Append a line to the native engine log (race-result diagnostics).
fn rr_log(msg: &str) {
    crate::tools::log(msg);
}

// Single global busy flag (mirrors native_skip.js `busy`): set while WE invoke a
// button/dialog method so the Update/Push detours skip during our own calls.
//
// SOFT-LOCK CLASS: this used to be a bare `AtomicBool`. Every setter pairs a `store(true)` with a
// `store(false)` around ONE synchronous game call — but that call is `ButtonCommon.OnPointerClick`,
// i.e. arbitrary game code, which can tear the view down, push a dialog, or raise a managed
// exception that never returns through our frame. When it didn't return, BUSY stayed true forever
// and every later `auto_press` / `click_now` / `auto_close` bailed on its first line: the skip
// "just stopped working" with nothing in the log. It is now a self-healing latch — the deadline is
// part of the stored state, so a lost clear can only ever cost us `BUSY_TTL_MS`, and the watchdog
// reports the leak so it stops being invisible.
const BUSY_TTL_MS: u64 = 750; // a synthetic click is sub-millisecond; 750ms is pure safety margin
pub(crate) static BUSY: crate::guard::Latch = crate::guard::Latch::new("ui_input::BUSY", BUSY_TTL_MS);

/// RAII busy marker: sets the latch and clears it on drop, so an early `return` (or any non-panic
/// divergence in the guarded scope) can't leave it armed.
struct BusyGuard;
impl BusyGuard {
    fn enter() -> Self {
        BUSY.arm();
        BusyGuard
    }
}
impl Drop for BusyGuard {
    fn drop(&mut self) {
        BUSY.clear();
    }
}

/// Watchdog hook (see `guard::tick`): true once per leaked busy window.
pub(crate) fn reap_busy() -> bool {
    BUSY.take_leak()
}

/// Diagnostics: is the click engine currently mid-press?
pub fn busy() -> bool {
    BUSY.get()
}

// Resolved (methodPointer code, MethodInfo*) pairs — called DIRECTLY like the
// JS NativeFunctions (no runtime_invoke), passing the trailing MethodInfo arg.
pub(crate) static GETNAME_C: AtomicUsize = AtomicUsize::new(0);
pub(crate) static GETNAME_MI: AtomicUsize = AtomicUsize::new(0);
pub(crate) static OPC_C: AtomicUsize = AtomicUsize::new(0);
pub(crate) static OPC_MI: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ISLOCK_C: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ISLOCK_MI: AtomicUsize = AtomicUsize::new(0);
pub(crate) static CLOSE_C: AtomicUsize = AtomicUsize::new(0);
pub(crate) static CLOSE_MI: AtomicUsize = AtomicUsize::new(0);
pub(crate) static CUR_C: AtomicUsize = AtomicUsize::new(0);
pub(crate) static CUR_MI: AtomicUsize = AtomicUsize::new(0);
pub(crate) static CTOR_C: AtomicUsize = AtomicUsize::new(0);
pub(crate) static CTOR_MI: AtomicUsize = AtomicUsize::new(0);
pub(crate) static C_PED: AtomicUsize = AtomicUsize::new(0);

static NAME_CACHE: OnceLock<Mutex<HashMap<usize, String>>> = OnceLock::new();
static PRESS_STATE: OnceLock<Mutex<HashMap<usize, (u32, u64)>>> = OnceLock::new();
static DONE_DLG: OnceLock<Mutex<std::collections::HashSet<usize>>> = OnceLock::new();
static LOGGED_NAMES: OnceLock<Mutex<std::collections::HashMap<String, u64>>> = OnceLock::new();
fn name_cache() -> &'static Mutex<HashMap<usize, String>> {
    NAME_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}
fn press_state() -> &'static Mutex<HashMap<usize, (u32, u64)>> {
    PRESS_STATE.get_or_init(|| Mutex::new(HashMap::new()))
}
fn done_dlg() -> &'static Mutex<std::collections::HashSet<usize>> {
    DONE_DLG.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}
fn logged_names() -> &'static Mutex<std::collections::HashMap<String, u64>> {
    LOGGED_NAMES.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}
/// Clear the click-engine caches (name / press-state / done-dialog / logged-names).
/// Called by crate::skip::result::clear_rr_caches on arm/disarm/race-end.
pub(crate) fn clear_caches() {
    if let Ok(mut m) = name_cache().lock() { m.clear(); }
    if let Ok(mut m) = press_state().lock() { m.clear(); }
    if let Ok(mut m) = done_dlg().lock() { m.clear(); }
    if let Ok(mut m) = logged_names().lock() { m.clear(); }
}

// MEMORY: every one of these maps is keyed by a raw pointer to a game object that the game
// destroys and reallocates constantly. `clear_caches()` only runs on race-result arm/disarm, so on
// any session that never opens a race result they grew for the whole run — the name cache alone
// accumulated an entry (plus a heap `String`) per button instance the game ever created. Caps turn
// them into bounded working sets: at the limit we drop the whole map, which costs one re-resolve
// per live button and reclaims everything the game has since freed.
const NAME_CACHE_CAP: usize = 2048;
const PRESS_STATE_CAP: usize = 512;
const DONE_DLG_CAP: usize = 512;
const LOGGED_NAMES_CAP: usize = 256;

/// Drop the per-button press budget so a watchdog retry gets a fresh attempt at a control whose
/// allowance was already spent (see `skip::result::reset_press_budget`).
pub(crate) fn clear_press_state() {
    if let Ok(mut m) = press_state().lock() {
        m.clear();
    }
}

/// Current cache sizes, for the memory-report endpoint.
pub fn cache_sizes() -> (usize, usize, usize, usize) {
    (
        name_cache().lock().map(|m| m.len()).unwrap_or(0),
        press_state().lock().map(|m| m.len()).unwrap_or(0),
        done_dlg().lock().map(|m| m.len()).unwrap_or(0),
        logged_names().lock().map(|m| m.len()).unwrap_or(0),
    )
}

// Direct-call ABIs (this, …, MethodInfo*).
type RetPtr = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void; // get_name
type RetBool = unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool; // IsLock
type Void2 = unsafe extern "C" fn(*mut c_void, *mut c_void); // Close
type Click = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void); // OnPointerClick(this, ped, mi)
type CurStatic = unsafe extern "C" fn(*mut c_void) -> *mut c_void; // EventSystem.get_current(mi)
type Ctor1 = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void); // ctor(this, es, mi)

/// GameObject/component name of a button (cached), via direct get_name call.
pub(crate) fn button_name(this: *mut c_void) -> String {
    let key = this as usize;
    if let Ok(m) = name_cache().lock() {
        if let Some(n) = m.get(&key) {
            return n.clone();
        }
    }
    let code = GETNAME_C.load(Ordering::Relaxed);
    let name = if code != 0 {
        unsafe {
            let f: RetPtr = std::mem::transmute(code);
            let s = f(this, GETNAME_MI.load(Ordering::Relaxed) as *mut c_void);
            il2cpp::read_string(s)
        }
    } else {
        String::new()
    };
    if let Ok(mut m) = name_cache().lock() {
        if m.len() >= NAME_CACHE_CAP {
            m.clear(); // bounded working set — see NAME_CACHE_CAP
        }
        m.insert(key, name.clone());
    }
    // Diagnostic: log each distinct button name seen while the result window is open, OR during a
    // career while an Auto-Skip extra that still needs its button identified is on (inspiration /
    // warnings / skill-learning). This captured the Inspiration "GO!" button, the warning-dialog OK
    // button and the skill screen's Learn/OK/Back buttons so they could be wired in precisely.
    //
    // PERF — this WAS a headline FPS cost, and the description that used to sit here is now wrong
    // in a way that cost two later investigations time, so it is corrected rather than deleted.
    //
    // The old claim was that each line is "an open + create_dir_all + write + close on the render
    // thread", making this the clearest contributor to the 200 → 18 fps collapse. That writer no
    // longer exists: `tools::log_to` pushes a formatted line into a queue and a background thread
    // does the file I/O. Per-line render-thread cost is now single-digit microseconds.
    //
    // Measured on a 22,582-line field log: 5,439 `[button] seen` lines (24% of the file) across
    // 1,040 distinct seconds — peak 72 in one second, but a MEAN of 0.058 lines/s in the session
    // under investigation. It is a burst at each view change, not a per-frame cost, because the
    // block is only reached on a name-cache MISS (see the early return above).
    //
    // It is still verbose-only, which is correct — but it is not the FPS bug, and looking here again
    // is a dead end. The logging that genuinely did cost main-thread time was the ungated
    // `[dialog-dump]` reflection walk in `skip/result.rs`.
    if !crate::tools::log::trace_enabled() {
        return name;
    }
    let diag = WINDOW.raw()
        || (crate::skip::career_fresh()
            && (crate::skip::is_inspiration_enabled()
                || crate::skip::is_warnings_enabled()
                || crate::skip::is_skill_learn_enabled()));
    if diag && !name.is_empty() {
        // Once-ever per name normally; while the INSPIRATION checkbox is on, re-log every 30s per
        // name — a once-ever entry from minutes earlier otherwise hides which buttons are actually
        // PRESENT at the inspiration moment. (The inspiration button itself IS identified now — it
        // is `SuccessionButton`, handled in `skip/inspire.rs`; the re-log is kept because the
        // surrounding buttons are still what tell us what state that screen is in.)
        let now = crate::skip::now_ms();
        let relog = crate::skip::is_inspiration_enabled();
        if let Ok(mut s) = logged_names().lock() {
            let due = match s.get(&name) {
                None => true,
                Some(&t) => relog && now.saturating_sub(t) >= 30_000,
            };
            if due {
                if s.len() >= LOGGED_NAMES_CAP {
                    s.clear();
                }
                s.insert(name.clone(), now);
                // `whitelisted`, NOT "the button accepted/rejected input". It reports membership of
                // `result::is_press_target`, the race-result auto-press list — nothing more. It has
                // been misread as input-state evidence in two separate investigations, so it is named
                // for what it is. Note even `true` does not mean the button was pressed:
                // `press_allowed` narrows further, and `auto_press` only runs inside an open window.
                crate::tools::trace(&format!(
                    "[button] seen: \"{name}\" whitelisted={}",
                    is_press_target(&name)
                ));
            }
        }
    }
    name
}

/// Synthetic PointerEventData(EventSystem.current), via direct ctor call.
pub(crate) unsafe fn make_pointer_event() -> *mut c_void {
    let ctor = CTOR_C.load(Ordering::Relaxed);
    let ped_cls = C_PED.load(Ordering::Relaxed);
    if ctor == 0 || ped_cls == 0 {
        return std::ptr::null_mut();
    }
    let cur_c = CUR_C.load(Ordering::Relaxed);
    let es = if cur_c != 0 {
        let f: CurStatic = std::mem::transmute(cur_c);
        f(CUR_MI.load(Ordering::Relaxed) as *mut c_void)
    } else {
        std::ptr::null_mut()
    };
    let obj = il2cpp::object_new(ped_cls as il2cpp::Class);
    if obj.is_null() {
        return std::ptr::null_mut();
    }
    let f: Ctor1 = std::mem::transmute(ctor);
    f(obj, es, CTOR_MI.load(Ordering::Relaxed) as *mut c_void);
    obj
}

/// Queue a click for the NEXT main-thread pump instead of performing it inline.
///
/// HARD-FREEZE CLASS (measured). Every synthetic click Overseer makes is issued from inside
/// `ButtonCommon.Update` — i.e. we call `OnPointerClick` *from within Unity's own UI update loop*,
/// re-entering the EventSystem while it is mid-iteration. For a self-contained dialog (the advisory
/// OK) that survives: 46 such clicks in the field log, no incident. For a click whose handler tears
/// down a dialog stack and fires a network request — the skill-learning "Learn" button — it does
/// not: of 4 attempts in the log, 2 froze the game outright. The signature is unmistakable, because
/// the log goes *completely* silent afterwards (no translation lines, no button lines, nothing) for
/// 92 s and 1348 s respectively, until the user killed the process. Total silence means the game's
/// main thread itself wedged, which is the known `DialogManager` z-order deadlock this codebase has
/// already hit twice from `SkipStory`.
///
/// The fix is to stop clicking from that stack. `mainthread::post` runs on the tween pump, which is
/// still the main thread (so the click is legal) but is *outside* the UI update — a clean stack,
/// exactly like the deferred friendship-splash callback. The click lands one frame later, which is
/// imperceptible, and the re-verify the callers do (dialog still frontmost, still matching) is
/// re-checked at that point rather than being trusted from the previous frame.
/// `verify` re-checks at click time; `done` is called with whether the click actually landed, so the
/// caller can consume its one-shot window on success and leave it armed for a retry on failure.
pub(crate) fn click_deferred(
    this: *mut c_void,
    verify: impl Fn() -> bool + Send + 'static,
    done: impl Fn(bool) + Send + 'static,
) {
    if this.is_null() || BUSY.get() {
        return;
    }
    let addr = this as usize;
    crate::mainthread::post(move || {
        // Re-verify on the frame we actually click: a dialog can close between the two frames, and
        // clicking a button that has since been recycled is how you press something unrelated.
        if !verify() {
            done(false);
            return;
        }
        let clicked = unsafe {
            let _g = crate::hooks::ReentryGuard::enter();
            click_now(addr as *mut c_void)
        };
        done(clicked);
    });
}

/// Raw click on a ButtonCommon (no whitelist/dedup) — reuses the race-result click primitives to
/// auto-press the shop "BackButton" after a buy. Returns true once it actually clicked.
pub(crate) unsafe fn click_now(this: *mut c_void) -> bool {
    if this.is_null() || BUSY.get() {
        return false;
    }
    let il = ISLOCK_C.load(Ordering::Relaxed);
    if il != 0 {
        let f: RetBool = std::mem::transmute(il);
        if f(this, ISLOCK_MI.load(Ordering::Relaxed) as *mut c_void) {
            return false; // locked → retry next frame
        }
    }
    let opc = OPC_C.load(Ordering::Relaxed);
    if opc == 0 {
        return false;
    }
    let ped = make_pointer_event();
    if ped.is_null() {
        return false;
    }
    {
        let _busy = BusyGuard::enter();
        let f: Click = std::mem::transmute(opc);
        f(this, ped, OPC_MI.load(Ordering::Relaxed) as *mut c_void);
    }
    true
}

pub(crate) fn auto_press(this: *mut c_void) {
    if this.is_null() || BUSY.get() {
        return;
    }
    let name = button_name(this);
    if !press_allowed(&name) {
        return;
    }
    let key = this as usize;
    let now = clock().elapsed().as_millis() as u64;
    let max = if is_multi(&name) { MULTI_MAX } else { 1 };
    // Read-only check — do NOT consume an attempt yet (mirrors native_skip.js:
    // the count/lastPress only advance AFTER a successful click). Otherwise a
    // transiently-locked button (e.g. "Next" during the result reveal) burns its
    // single allowed press on a locked frame and never retries.
    {
        let st = match press_state().lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        let (cnt, last) = *st.get(&key).unwrap_or(&(0, 0));
        if cnt >= max || now.wrapping_sub(last) < PRESS_GAP_MS {
            return;
        }
    }
    unsafe {
        // Respect button lock — return WITHOUT consuming the attempt so we retry
        // next frame once it unlocks.
        let il = ISLOCK_C.load(Ordering::Relaxed);
        if il != 0 {
            let f: RetBool = std::mem::transmute(il);
            if f(this, ISLOCK_MI.load(Ordering::Relaxed) as *mut c_void) {
                // Button is locked. Normally it unlocks in a frame or two and we just retry — but
                // the victory-concert result screen leaves a SteamInputBlock up that never lifts on
                // its own, so the advance button (Next / SingleModeNext / Continue) stays locked
                // forever and the skip stalls (window_open=true, no advance). After a short grace
                // (so we never disturb a normal transient lock), lift that block ourselves — the
                // exact PlayClose the skipped flow would have run — so the button unlocks and our
                // next-frame click lands. Only for the explicit advance buttons, and auto_press
                // already runs only when rr_should_advance (won / no retries), so this is safe.
                if is_advance_button(&name) {
                    let next = RR_NEXT_LIFT.load(Ordering::Relaxed);
                    if next == 0 {
                        RR_NEXT_LIFT.store(now + RR_LIFT_GRACE_MS, Ordering::Relaxed);
                    } else if now >= next {
                        lift_input_block();
                        RR_NEXT_LIFT.store(now + RR_LIFT_THROTTLE_MS, Ordering::Relaxed);
                    }
                }
                return;
            }
        }
        let opc = OPC_C.load(Ordering::Relaxed);
        if opc == 0 {
            return;
        }
        let ped = make_pointer_event();
        if ped.is_null() {
            return;
        }
        let _busy = BusyGuard::enter();
        let f: Click = std::mem::transmute(opc);
        f(this, ped, OPC_MI.load(Ordering::Relaxed) as *mut c_void);
    }
    // Click succeeded → now consume the attempt + stamp the time.
    if let Ok(mut st) = press_state().lock() {
        let (cnt, _) = *st.get(&key).unwrap_or(&(0, 0));
        if st.len() >= PRESS_STATE_CAP && !st.contains_key(&key) {
            st.clear();
        }
        st.insert(key, (cnt + 1, now));
    }
    RR_PRESSES.fetch_add(1, Ordering::Relaxed);
    // A landed press IS progress — re-arm the expectation for the NEXT screen rather than leaving
    // the original deadline running (a long result sequence is many presses, not one).
    crate::skip::RESULT_PROGRESS.expect();
    // We advanced — restart the stuck-lift grace for whatever screen comes next.
    RR_NEXT_LIFT.store(0, Ordering::Relaxed);
}

/// Auto-close a pushed dialog (the JS DialogManager.Push*→Close path).
pub(crate) fn auto_close(dlg: *mut c_void) {
    if dlg.is_null() || BUSY.get() {
        return;
    }
    if !il2cpp::object_class_name(dlg).contains("DialogCommon") {
        return;
    }
    {
        let mut d = match done_dlg().lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        if d.len() >= DONE_DLG_CAP {
            d.clear();
        }
        if !d.insert(dlg as usize) {
            return;
        }
    }
    let code = CLOSE_C.load(Ordering::Relaxed);
    if code == 0 {
        return;
    }
    unsafe {
        let _busy = BusyGuard::enter();
        let f: Void2 = std::mem::transmute(code);
        f(dlg, CLOSE_MI.load(Ordering::Relaxed) as *mut c_void);
    }
}
