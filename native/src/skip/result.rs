//! Race-result auto-advance (win/loss gating, whitelist, multi-press) +
//! SteamInputBlock lifter. Career-gated, default OFF unless `races_on`.
//!
//! ═══════════════════════════════════════════════════════════════════════════
//! B3b — RACE-RESULT AUTO-ADVANCE  (EXPERIMENTAL, default OFF)
//! Port of native_skip.js part 3. After "View Results" (+ the unavoidable 1st
//! tap), the result screens auto-press their own buttons to the next turn.
//! Untested in this native form → gated behind RACE_RESULT_ENABLED; enabling it
//! never touches the proven training/event core.
//! ═══════════════════════════════════════════════════════════════════════════

#![allow(dead_code)]

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::hooks::{in_overseer, ReentryGuard};
use crate::il2cpp;
use crate::skip::{
    call_orig, career_fresh, install_one, is_shop_enabled, mark_career, now_ms, rr_log,
    LAST_CAREER_MS, RESULT_PROGRESS,
};
use crate::ui_input::{auto_close, auto_press, button_name, click_now};

// Default ON in builds with feature `races_on`, OFF otherwise.
pub(crate) static RACE_RESULT_ENABLED: AtomicBool = AtomicBool::new(cfg!(feature = "races_on"));

// TEAM TRIALS guard (see the note in skip/mod.rs::set_in_team_trials). htt.rs sets this via
// crate::skip::set_in_team_trials; the career view-manager clears it. While set, result never fires.
pub(crate) static IN_TEAM_TRIALS: AtomicBool = AtomicBool::new(false);

/// Race-result auto-advance only fires when the player WON (finished 1st).
/// Anything else — lost, or placement not yet known — means NO auto-advance, so
/// the player handles it manually (e.g. to retry). Reset per race in race.rs
/// (ImportDirect), so a retry or the next race is re-evaluated from scratch.
fn rr_should_advance() -> bool {
    if !RACE_RESULT_ENABLED.load(Ordering::Relaxed) {
        return false;
    }
    // Never auto-advance during Team Trials (career-only feature).
    if IN_TEAM_TRIALS.load(Ordering::Relaxed) {
        return false;
    }
    // Positive career gate: only ever auto-press inside a career (single-mode) run, so a stale armed
    // window can't leak presses into other modes' menus.
    if !career_fresh() {
        return false;
    }
    // Auto-advance when the player WON (placement 1), OR when no race retries remain
    // (`available_continue_num` == 0): a retry isn't possible, so don't hold the result
    // screen on a loss. Placement + continues come from the response hooks (the response hook in
    // public, the response hook in private). continues == -1 means "unknown" → fall back
    // to the win-only gate. Both builds ship `raceread`.
    #[cfg(feature = "raceread")]
    {
        let won = crate::race::player_finish_order() == 1;
        let no_retries_left = crate::race::continues_available() == 0;
        won || no_retries_left
    }
}

// Spacing between the post-race result-screen presses. Same reasoning as HYPER_GAP_MS: a debounce
// against a per-frame Update, not a timing the game requires. With MULTI_MAX presses per screen this
// is paid in full every race, so it is real wall-clock cost that no frame rate can recover.
// 90 ms is ~15 frames at 170 fps. Raise it again if a result screen ever advances twice.
pub(crate) const PRESS_GAP_MS: u64 = 130;
pub(crate) const MULTI_MAX: u32 = 4;
// EXACT whitelist + exact match (substring matching caused mis-presses). The
// (substring matching caused mis-presses, so we match the exact button names).
pub(crate) fn is_press_target(name: &str) -> bool {
    [
        "ButtonCommon",
        "ButtonCenter",
        "NextButton",
        "ScreenTap",
        "SingleModeNextButton",
        "TouchSprite",
        // Debut / first-race completion (and other special result screens) advance via
        // a "Continue" button. Safe to press: auto_press only runs when rr_should_advance
        // (won, or no retries left), so a retry-eligible loss never auto-continues.
        "ContinueButton",
    ]
    .contains(&name)
}
pub(crate) fn is_multi(name: &str) -> bool {
    name == "ScreenTap"
        || name == "TouchSprite"
        || name == "ButtonCommon"
        || name == "ButtonCenter"
}

/// Gate the auto-press by context. When the skip is advancing a LOSS only because race
/// retries are exhausted (`continues == 0`), the result screen's retry is a paid
/// "buy an alarm clock" purchase — and the generic dialog taps (ButtonCenter / ScreenTap /
/// ButtonCommon / TouchSprite) or ContinueButton can land on that purchase confirmation /
/// the server round-trip, spending Carats and desyncing the game (bounce to title). In that
/// one case we press ONLY the explicit result-advance button. A win (or a loss with retries
/// still left) uses the full whitelist as before — those screens have no purchase dialog.
pub(crate) fn press_allowed(name: &str) -> bool {
    #[cfg(feature = "raceread")]
    {
        let won = crate::race::player_finish_order() == 1;
        let retries_exhausted = crate::race::continues_available() == 0;
        if !won && retries_exhausted {
            return name == "NextButton" || name == "SingleModeNextButton";
        }
    }
    is_press_target(name)
}

// Single global busy flag lives in ui_input (BUSY). Window/press counters below.
//
// SOFT-LOCK CLASS: `WINDOW_OPEN` used to be a bare AtomicBool armed on the in-race Skip button and
// cleared only by `ChangeMainView` or reaching Home. Every path that leaves a race WITHOUT passing
// through either of those — a network error bouncing to title, a soft reset, the game being closed
// and reopened to a different mode, an update prompt — left it armed forever. The `career_fresh()`
// heartbeat bounded the damage but not the state itself, so the auto-press engine stayed live,
// re-evaluating whitelisted button names in menus it had no business acting in. It is now a Gate:
// open-ness carries its own maximum lifetime, so it CANNOT outlive a plausible result sequence, and
// the watchdog reports (and closes) any window that does.
pub(crate) static WINDOW: crate::guard::Gate = crate::guard::Gate::new("race-result-window", 300_000);
pub(crate) static RR_PRESSES: AtomicU64 = AtomicU64::new(0);

/// Watchdog hook: close (and report) a result window that outlived its cap.
pub(crate) fn reap_window() -> bool {
    if WINDOW.raw() && !WINDOW.is_open() {
        WINDOW.disarm();
        clear_rr_caches();
        return true;
    }
    false
}

/// Force the window closed (used by the progression watchdog's stand-down).
pub(crate) fn force_disarm(why: &str) {
    if WINDOW.disarm() {
        clear_rr_caches();
        rr_log(&format!("[race-result] window force-disarmed ({why})"));
    }
    RESULT_PROGRESS.resolved();
}

/// Give every tracked button a fresh press budget. The press engine caps attempts per button so a
/// locked control can't be machine-gunned; on a watchdog retry that cap is exactly what stops the
/// recovery, so the retry path resets it once.
pub(crate) fn reset_press_budget() {
    crate::ui_input::clear_press_state();
}

/// The result flow moved on — clear the outstanding progression expectation.
pub(crate) fn note_progressed() {
    RESULT_PROGRESS.resolved();
}

pub(crate) fn clear_rr_caches() {
    crate::ui_input::clear_caches();
    clear_autoskip_clicked();
    RR_NEXT_LIFT.store(0, Ordering::Relaxed);
    // Warn-confirm tidiness: disarm + un-root the matched-warning Data on the arm/disarm paths
    // (RaceSkip arm, ChangeMainView, Home). The frontmost re-verify already prevents stale fires;
    // this just frees the GCHandle promptly. Every caller is a game-thread hook (attached), so the
    // handle drop here is safe.
    WARN_CONFIRM_UNTIL.store(0, Ordering::Relaxed);
    if let Ok(mut g) = WARN_DATA_H.lock() {
        *g = None;
    }
}

// Stuck-advance-button input-block lift. The victory-concert result screen (and similar) keeps a
// SteamInputBlock(Clone) up that never lifts on its own, leaving Next/Continue locked forever so
// the skip stalls. 0 = not timing yet; otherwise the earliest ms at which we may PlayClose. Reset
// on a successful press and on race end (clear_rr_caches).
pub(crate) static RR_NEXT_LIFT: AtomicU64 = AtomicU64::new(0);
pub(crate) const RR_LIFT_GRACE_MS: u64 = 600; // stay locked this long before lifting (don't fight normal transient locks)
pub(crate) const RR_LIFT_THROTTLE_MS: u64 = 400; // re-lift at most this often while still stuck

pub(crate) fn is_advance_button(name: &str) -> bool {
    name == "NextButton" || name == "SingleModeNextButton" || name == "ContinueButton"
}

/// Lift the SteamInputBlock the skipped result coroutine would have lifted (the same PlayClose the
/// shop-skip uses) so a stuck-locked advance button unlocks. No-op until SIB_MGR is captured.
pub(crate) fn lift_input_block() {
    use crate::skip::shop::{PLAY_CLOSE, SIB_MGR};
    let mgr = SIB_MGR.load(Ordering::Relaxed);
    if mgr == 0 {
        return;
    }
    if let Some(pc) = PLAY_CLOSE.get() {
        if pc.ok() {
            unsafe { pc.call_play_close(mgr as *mut c_void, true) };
            rr_log("[race-result] lifted stuck input block (advance button locked)");
        }
    }
}

// Detours for race-result.
type Void3 = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void);
type Push1 = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void;
type Push2 = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> *mut c_void;
crate::skip_hook_slot!(TR_UPDATE, D_UPDATE);
crate::skip_hook_slot!(TR_ONPC, D_ONPC);
crate::skip_hook_slot!(TR_CMV, D_CMV);
crate::skip_hook_slot!(TR_HOME, D_HOME); // HomeViewController.PlayInView — reached the lobby = left career
crate::skip_hook_slot!(TR_HOMEOUT, D_HOMEOUT); // HomeViewController.PlayOutView — left the lobby
// NOTE: these two slot comments used to name PlayInView/PlayOutView. They were WRONG — the code
// installs BeginView/InitializeView. A misleading name here is not cosmetic: it's what let the
// "Scout closed" release path look implemented when nothing releases the pin except the lobby.
crate::skip_hook_slot!(TR_GACHA, D_GACHA); // GachaMainViewController.BeginView — never observed firing
crate::skip_hook_slot!(TR_GACHAINIT, D_GACHAINIT); // GachaMainViewController.InitializeView (NOT a close hook)
crate::skip_hook_slot!(TR_PUSH1, D_PUSH1);
crate::skip_hook_slot!(TR_PUSH2, D_PUSH2);
// [rrprobe] hyper-native Phase-0 traces: the result scene's OWN skip entry points (passthrough).
crate::skip_hook_slot!(TR_RRSKIP0, D_RRSKIP0); // RaceResultSceneUI.OnPushSkip
crate::skip_hook_slot!(TR_RRSKIP1, D_RRSKIP1); // RaceResultSceneUI.Skip

/// Serial for the frame-GLOBAL half of `on_button_update`. `ButtonCommon.Update` fires once per
/// button per frame — dozens of invocations on a busy screen — but most of the work below is
/// per-FRAME, not per-button, so it runs once and the rest of the buttons skip it. That removes the
/// largest per-frame cost Overseer adds to a menu-heavy screen.
///
/// It deliberately does NOT key on `ui_tempo::frame_id()`. That counter only advances when DOTween
/// ticks, and DOTween does not tick while no tween is active — which is exactly the idle-dialog case
/// these drivers exist for. Keying on it would silently stop the auto-choice, the fast-forward press
/// and the MTL fallback pump on precisely the screens that need them. A monotonic serial per
/// ButtonCommon.Update BATCH is what's actually wanted: `this` cycles through the live buttons, so a
/// repeat of the first-seen pointer marks a new frame.
static GLOBAL_ANCHOR: AtomicUsize = AtomicUsize::new(0);
static GLOBAL_LAST_MS: AtomicU64 = AtomicU64::new(0);

/// True at most once per `ButtonCommon.Update` sweep. Anchors on the first button of the sweep and
/// returns true when that same button comes round again; a **100 ms** floor bounds it if the anchor
/// button is destroyed mid-sweep (the anchor then re-seeds on the next distinct button).
///
/// That floor is not free: for up to 100 ms after every view change, each per-frame driver gated on
/// this function is starved. It is the deliberate trade for never re-running a sweep-scoped action
/// several times in one sweep, but if a driver ever needs sub-100 ms responsiveness right after a
/// view change, this is the constant that is stopping it.
fn once_per_sweep(this: *mut c_void) -> bool {
    let anchor = GLOBAL_ANCHOR.load(Ordering::Relaxed);
    let now = now_ms();
    if anchor == 0 {
        GLOBAL_ANCHOR.store(this as usize, Ordering::Relaxed);
        GLOBAL_LAST_MS.store(now, Ordering::Relaxed);
        return true;
    }
    if anchor == this as usize {
        GLOBAL_LAST_MS.store(now, Ordering::Relaxed);
        return true;
    }
    // The anchor button went away (view change) — re-seed rather than starve the drivers.
    if now.saturating_sub(GLOBAL_LAST_MS.load(Ordering::Relaxed)) >= 100 {
        GLOBAL_ANCHOR.store(this as usize, Ordering::Relaxed);
        GLOBAL_LAST_MS.store(now, Ordering::Relaxed);
        return true;
    }
    false
}

unsafe extern "C" fn on_button_update(this: *mut c_void, m: *mut c_void) {
    call_orig(&TR_UPDATE, this, m);
    // Suspended → this hook is a pure pass-through. It is the single most frequently executed piece
    // of Overseer code, so gating it here is what makes "Overseer disabled" actually free.
    //
    // One exception: translation can be configured to keep running while Overseer is off, and its
    // FALLBACK re-apply pump lives here (DOTween's own Update doesn't tick on an idle story screen,
    // which is exactly where a late translation needs to land). Drive that, then leave.
    if !crate::runtime::active(crate::runtime::Subsystem::Skip) {
        if once_per_sweep(this) {
            // Translation (which can be kept alive independently) still needs its fallback pump,
            // and any deferred main-thread job must still drain.
            crate::mainthread::drain_fallback();
            if crate::mtl::translation_active() {
                crate::mtl::pump_fallback();
            }
        }
        return;
    }
    // WATCHDOG: this pump runs on the game thread OUTSIDE any Overseer-initiated call, so the re-entry
    // guard must be false here. A leaked guard (a managed exception unwound past our frame, so `Drop`
    // never ran) used to disable every skip until a game restart. `in_overseer()` now expires the
    // hold on its own, so this is the tidy-up + the place the recovery is counted — and it checks
    // `guard_leaked()` rather than `in_overseer()`, which by then already reads false.
    if crate::hooks::guard_leaked() {
        crate::hooks::clear_in_overseer();
        crate::guard::note_recovery("re-entry guard leaked (managed exception unwound past us) — cleared");
    }
    // ── frame-global half: once per ButtonCommon.Update sweep, whichever button gets there first ──
    if once_per_sweep(this) {
        crate::skip::event::pump_pending_tag_cb(); // deferred friendship-splash onDone, clean frame
        // Deferred main-thread jobs (queued dialog clicks). The tween pump is the primary driver but
        // does not tick on an idle dialog screen, which is where those jobs are posted from.
        crate::mainthread::drain_fallback();
        // MTL re-apply FALLBACK: same reasoning — DOTween skips its Update entirely when no tween is
        // active, true on idle story/event-choice screens, precisely where first-view translation
        // swaps matter. Throttled internally.
        crate::mtl::pump_fallback();
        drive_result_skip();
        drive_event_choice();
        drive_race_ff();
        drive_inspiration();
    }
    crate::followers::pump(this); // auto-unfollow: drive one step of the bulk-remove flow (default off)

    // ── per-button half ──────────────────────────────────────────────────────────────────────────
    // PERF: everything below wants this button's NAME, and each site used to resolve it separately —
    // `button_name` takes a mutex on every call, so a screen with 50 buttons paid ~250 lock/unlock
    // pairs per frame just to ask the same question five times. Resolve once, and only when one of
    // the windows that could act is actually armed (the common case is that none are, and then we do
    // no work at all).
    let now = now_ms();
    let want_shop = now < crate::skip::shop::SHOP_PRESS_BACK_UNTIL.load(Ordering::Relaxed) && is_shop_enabled();
    let want_warn = now < WARN_CONFIRM_UNTIL.load(Ordering::Relaxed);
    let want_skill = crate::skip::skill::wants_button();
    let name: Option<String> = if (want_shop || want_warn || want_skill) && !in_overseer() {
        Some(button_name(this))
    } else {
        None
    };

    // After a buy auto-closes its dialog, auto-press the shop "BackButton" so the player lands
    // where their manual Back would (this Update fires per ButtonCommon, so we catch BackButton
    // when it's this one). Frame-windowed + shop-gated so it never fires elsewhere.
    if want_shop && !in_overseer() {
        if name.as_deref() == Some("BackButton") {
            let _g = ReentryGuard::enter();
            if click_now(this) {
                crate::skip::shop::SHOP_PRESS_BACK_UNTIL.store(0, Ordering::Relaxed);
                rr_log(&format!("[shop {}ms] auto-pressed BackButton", now_ms()));
            }
        }
    }
    if rr_should_advance() && WINDOW.is_open() && !in_overseer() {
        auto_press(this);
    }
    hyper_tick(this);
    // (drive_result_skip / drive_event_choice / drive_race_ff / drive_inspiration moved into the
    // per-sweep block above — none of them looks at `this`, so running them per button was pure
    // duplication.)
    // Skill-learning auto-confirm + native cutscene/result skip (default OFF; self-gated on the
    // SKILL_VIEW_ACTIVE positive view gate). Runs per-button so its ButtonRight/ButtonCenter/BackButton
    // matches ride the same pump as warn-confirm; it takes the already-resolved name.
    crate::skip::skill::pump(this, name.as_deref());
    // Auto-OK a matched advisory WARNING (e.g. the consecutive-race notice): click its ButtonRight
    // (OK) through the game's OWN click path — runs the button callback + AutoClose exactly as the
    // player would, so no dialog is left dangling. Windowed (6s) after a signature match
    // (try_arm_warning_confirm) AND re-verified every frame against the ROOTED Data
    // (warn_target_still_frontmost), so it can only ever fire while THAT warning is still the
    // frontmost dialog — never a stray button, and never a purchase/race-details dialog (those
    // never arm the window, and the re-verify rejects them even inside an open window). The wider
    // window rides out an input-lock/animate-in that outlives the old 2.5s (a live-proven miss).
    if want_warn
        && !in_overseer()
        && name.as_deref() == Some("ButtonRight")
        && warn_target_still_frontmost()
    {
        let _g = ReentryGuard::enter();
        if click_now(this) {
            WARN_CONFIRM_UNTIL.store(0, Ordering::Relaxed);
            if let Ok(mut g) = WARN_DATA_H.lock() {
                *g = None; // un-root — the wrapper can now be collected
            }
            rr_log("[warn-confirm] auto-clicked OK (ButtonRight) on advisory warning");
        }
    }
}

// ── Auto-pick top event choice (controller-driven) ──────────────────────────────────────────────
// The button GAMEOBJECT is "StoryViewChoiceButton01", but its CLASS is Gallop.StoryChoiceButton and
// the selection is handled by Gallop.StoryChoiceController — NOT by ButtonCommon.OnPointerClick (the
// probe proved this: OnPointerClick returned "success" but the choice never registered, so the old
// code re-clicked the same pooled button every frame forever). Fix: capture the live controller when
// a choice opens, and select the TOP choice through the game's OWN `ChoiceButton(0)`, gated on
// `IsWaitSelect` — which flips false once selected, so it fires exactly once and can't loop.
/// The live StoryChoiceController from the last choice-open — polled by `drive_event_choice`.
/// Held via a STRONG GCHandle (same fix as STORY_CTRL_H): the GC can't see Rust statics, so a raw
/// usize cache here was a use-after-free — once the story view tore the controller down, the pump's
/// m_CachedPtr destroyed-check ITSELF read freed memory. The handle keeps the managed wrapper alive,
/// so that check reads live memory and correctly detects the native Destroy (which then clears it).
static CHOICE_CTRL_H: Mutex<Option<crate::il2cpp::GCHandle>> = Mutex::new(None);
static CHOICE_ISWAIT: AtomicUsize = AtomicUsize::new(0); // get_IsWaitSelect() code
static CHOICE_ISWAIT_MI: AtomicUsize = AtomicUsize::new(0);
static CHOICE_SELECT: AtomicUsize = AtomicUsize::new(0); // ChoiceButton(i32) code
static CHOICE_SELECT_MI: AtomicUsize = AtomicUsize::new(0);
static AUTOSKIP_LAST_MS: AtomicU64 = AtomicU64::new(0);
const AUTOSKIP_GAP_MS: u64 = 250;
crate::skip_hook_slot!(TR_CHOPEN, D_CHOPEN); // StoryChoiceController.SetupOpenChoiceButton capture

/// Deprecated no-op kept for the clear_rr_caches call site — the controller path is stateless
/// (IsWaitSelect gates it), so there's nothing per-instance to reset anymore.
pub(crate) fn clear_autoskip_clicked() {}

/// ONE-SHOT layout dump of StoryChoiceController, to find the field holding the choice buttons.
///
/// Drawing a ✓/✗ on the actual choice buttons needs their screen rects, and the only handle we have
/// is this controller — `ChoiceButton(i32)` is a method that SELECTS, not a list we can read. Rather
/// than guess a field name (the mistake behind the affinity offset that read garbage for months),
/// this asks the running game for its own layout and logs it once per session.
///
/// Runs on the IL2CPP hook thread, which is where field enumeration is legal. Once per session, so
/// it cannot become a per-choice cost. Delete this once the field is known and wired.
fn dump_choice_layout(this: *mut c_void) {
    use std::sync::atomic::AtomicBool;
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::Relaxed) || this.is_null() {
        return;
    }
    let klass = crate::il2cpp::object_class(this as crate::il2cpp::Object);
    if klass.is_null() {
        return;
    }
    let fields = crate::il2cpp::class_fields(klass);
    rr_log(&format!(
        "[choice-layout] {} — {} field(s)",
        crate::il2cpp::object_class_name(this as crate::il2cpp::Object),
        fields.len()
    ));
    for (name, off) in &fields {
        rr_log(&format!("[choice-layout]   {name} @ 0x{off:X}"));
    }
    // Methods too: a `GetChoiceButton(i)` style accessor would be a cleaner route than a raw field.
    for m in crate::il2cpp::class_methods(klass) {
        rr_log(&format!("[choice-layout]   fn {m}"));
    }
}

/// Record WHICH button the player is on, from the live controller.
///
/// `_selectedIndex` is a plain i32 field (offset resolved by NAME from the runtime, never
/// hard-coded). It matters because the row-selection rule cannot be derived without knowing which
/// option produced the outcome — inferring it from the row that fired is circular, which is exactly
/// the mistake that made an earlier analysis look like it agreed with itself.
///
/// This used to also drive an in-game highlight drawn over the choice buttons. That is gone: see
/// `HANDOFF.md` for what was learned about the canvas geometry, and why it was dropped.
unsafe fn note_selected_button(ctrl: *mut c_void) {
    if ctrl.is_null() {
        return;
    }
    let klass = crate::il2cpp::object_class(ctrl as crate::il2cpp::Object);
    if klass.is_null() {
        return;
    }
    if let Some(off) = crate::il2cpp::field_offset(klass, "_selectedIndex") {
        let idx = crate::il2cpp::get_field_i32(ctrl as crate::il2cpp::Object, off);
        crate::event_reveal::note_selected_index(idx);
    }
}

/// Capture the live StoryChoiceController the moment its choices are set up, so `drive_event_choice`
/// can drive selection through the game's own methods. Refreshed every time choices open.
unsafe extern "C" fn on_choice_setup(this: *mut c_void, m: *mut c_void) {
    if let Ok(mut g) = CHOICE_CTRL_H.lock() {
        *g = Some(crate::il2cpp::GCHandle::new(this, false)); // old handle freed on drop
    }
    dump_choice_layout(this);
    // A choice is opening → pin 1x until it's selected, so the choice UI syncs and drive_event_choice
    // can pick it. (The post-race soft-lock was here, at 20x.) Edge-triggered (set here, cleared on
    // select/teardown) — NOT re-stamped by lobby events, so it can't oscillate tempo. A one-shot
    // timestamp backs a hard safety cap so a never-selected choice (auto-choice off) can't stick 1x.
    CHOICE_PENDING.store(true, Ordering::Relaxed);
    CHOICE_PENDING_MS.store(now_ms(), Ordering::Relaxed);
    CHOICE_SELECTED_FOR.store(0, Ordering::Relaxed); // a NEW choice → allow exactly one selection
    let t = TR_CHOPEN.load(Ordering::Relaxed);
    if t != 0 {
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void) = std::mem::transmute(t);
        f(this, m);
    }
}

/// Auto-pick the top event choice. Runs from the ButtonCommon.Update pump (fires every frame). Uses
/// the captured controller + the game's ChoiceButton(0); IsWaitSelect makes it a single clean select.
unsafe fn drive_event_choice() {
    // Selected-index sampling FIRST, before the auto-choice gate: it must run for MANUAL players
    // too. It used to sit below the gate, so with auto-choice off (i.e. for anyone actually
    // choosing) it never ran and the outcome recorder's selected_index stayed -1 — precisely the
    // field the row-selection analysis needs.
    {
        let ctrl = match CHOICE_CTRL_H.lock() {
            Ok(g) => g.as_ref().map_or(std::ptr::null_mut(), |h| h.target()),
            Err(_) => std::ptr::null_mut(),
        };
        if !ctrl.is_null() && !crate::il2cpp::unity_object_destroyed(ctrl) {
            note_selected_button(ctrl);
        }
    }
    if !crate::skip::is_event_choice_enabled() || in_overseer() {
        return;
    }
    // AFTER-RACE EVENTS. On the Steam build the event's choice branches are shown in a SIDE panel
    // (`DialogType`/`DispStackType` "SteamRight", titled "Choix" — 47 of them in one field log). It
    // is a DialogCommon, so a blanket `dialog_open()` bail meant the auto-choice stood down for the
    // entire event and nothing advanced: the panel explaining the choices prevented the choices from
    // being taken.
    //
    // The guard still matters — the 2026-07-14 freeze came from acting under a dialog — so it is
    // narrowed rather than dropped, and by the GAME's own signal rather than by trying to classify
    // dialogs (an earlier attempt at that read an enum field as a string and crashed the process):
    // `IsWaitSelect` on the live `StoryChoiceController` is the game stating that it is blocked
    // waiting for exactly this selection. If it is waiting, a dialog being on screen is not a reason
    // to refuse — we drive the controller's own `ChoiceButton`, not any dialog's button.
    //
    // `choice_ready` is re-derived below from the live controller, so this is a two-stage check: the
    // dialog bail only applies while the controller is NOT waiting.
    let dialog_up = crate::skip::dialog_open();
    // Re-fetch the live pointer through the GC handle each frame — null once collected, so the
    // calls below can never run on freed memory.
    let ctrl = match CHOICE_CTRL_H.lock() {
        Ok(g) => g.as_ref().map_or(std::ptr::null_mut(), |h| h.target()),
        Err(_) => std::ptr::null_mut(),
    };
    let sel = CHOICE_SELECT.load(Ordering::Relaxed);
    let iw = CHOICE_ISWAIT.load(Ordering::Relaxed);
    if ctrl.is_null() || sel == 0 || iw == 0 {
        return;
    }
    let now = now_ms();
    if now.saturating_sub(AUTOSKIP_LAST_MS.load(Ordering::Relaxed)) < AUTOSKIP_GAP_MS {
        return;
    }
    // The controller is a StoryView component; the game Destroys it on view teardown. The strong
    // handle keeps the managed wrapper alive, so this m_CachedPtr read is on LIVE memory (with the
    // old raw cache this very check was the first use-after-free read) and reliably detects Destroy.
    if crate::il2cpp::unity_object_destroyed(ctrl) {
        if let Ok(mut g) = CHOICE_CTRL_H.lock() {
            *g = None; // frees the handle; the wrapper can now be collected
        }
        CHOICE_PENDING.store(false, Ordering::Relaxed); // view torn down → release the 1x pin
        return;
    }
    // Only when the controller is actually WAITING for a selection (not mid-animation / already picked).
    // Do NOT release the pin here on !iswait — the choice is still ANIMATING IN then, and that is
    // exactly the fragile window that must stay 1x. The pin releases only on select or teardown below.
    let iswait: unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool = std::mem::transmute(iw);
    if !iswait(ctrl, CHOICE_ISWAIT_MI.load(Ordering::Relaxed) as *mut c_void) {
        return;
    }
    // The narrowed dialog guard (see the note at the top of this function): a dialog on screen only
    // blocks us while the controller is NOT waiting. Now that `IsWaitSelect` is true, the game is
    // explicitly blocked on this selection, so the Steam side panel — or any other dialog that
    // happens to be up — must not stop it.
    let _ = dialog_up;
    // ONE selection per opened choice.
    //
    // `IsWaitSelect` is the game's own "still waiting" flag, but it does not flip synchronously —
    // the selection is queued and the flag clears a few frames later. With only the 250 ms throttle
    // in front of it, that window let us call `ChoiceButton(0)` again, and again: the logs show 54
    // separate choices each selected THREE times (three invocations inside ~750 ms). Firing a
    // choice handler three times pushes duplicate story continuations, which is precisely the shape
    // of the post-choice freeze — one session in the log wedged immediately after such a burst.
    //
    // The dedup keys on the controller pointer: a genuinely new choice arrives on a fresh
    // `SetupOpenChoiceButton` (which resets this), so a real second choice is never blocked.
    if CHOICE_SELECTED_FOR.swap(ctrl as usize, Ordering::Relaxed) == ctrl as usize {
        return;
    }
    AUTOSKIP_LAST_MS.store(now, Ordering::Relaxed);
    let _g = ReentryGuard::enter();
    let select: unsafe extern "C" fn(*mut c_void, i32, *mut c_void) = std::mem::transmute(sel);
    select(ctrl, 0, CHOICE_SELECT_MI.load(Ordering::Relaxed) as *mut c_void); // index 0 = TOP option
    CHOICE_PENDING.store(false, Ordering::Relaxed); // selected → release the 1x pin
    crate::skip::event::note_progressed(); // a taken choice IS the event advancing
    rr_log("[autoskip] selected top event choice (StoryChoiceController.ChoiceButton 0)");
}

/// The controller instance we have already selected on — see the dedup note in `drive_event_choice`.
/// Reset whenever a new choice opens, so each choice gets exactly one selection.
static CHOICE_SELECTED_FOR: AtomicUsize = AtomicUsize::new(0);

/// True while an event choice is open and unselected — ui_tempo holds 1x so the choice UI syncs.
/// Edge-cleared on select/teardown; the timestamp bounds it (10s) so a manually-handled choice
/// (auto-choice off, no select event) can never leave tempo stuck at 1x.
static CHOICE_PENDING: AtomicBool = AtomicBool::new(false);
static CHOICE_PENDING_MS: AtomicU64 = AtomicU64::new(0);
const CHOICE_PIN_CAP_MS: u64 = 10_000;
pub fn choice_pending() -> bool {
    CHOICE_PENDING.load(Ordering::Relaxed)
        && now_ms().saturating_sub(CHOICE_PENDING_MS.load(Ordering::Relaxed)) < CHOICE_PIN_CAP_MS
}

// ── Auto Fast-Forward (in-race accelerated playback) ─────────────────────────────────────────────
// Gallop.RaceUIFastForward is the in-race FF button; `OnPushFastForward()` is its OWN click handler
// (cycles the FF mode + applies the RaceManager delta-timescale correctly). We capture the live
// instance at Awake and, when the button is visible and race_ff is on, press it ONCE per race —
// engaging Fast-Forward without the mode-cycle pitfalls of pressing repeatedly. Unlike RaceSkip, FF
// is available on races you can't skip (forced-watch, first win at a grade), so it never soft-locks.
/// The live RaceUIFastForward — polled by `drive_race_ff`. Held via a STRONG GCHandle (same fix as
/// STORY_CTRL_H): the GC can't see Rust statics, so a raw usize cache here dangled once the race
/// scene freed the button — the pump's destroyed-check ITSELF read freed memory (and the
/// is_race_ff_enabled() early return could skip the clear indefinitely). The handle keeps the
/// wrapper alive until the destroyed-check clears it or the next capture replaces it.
static FF_UI_H: Mutex<Option<crate::il2cpp::GCHandle>> = Mutex::new(None);
static FF_SHOWN: AtomicBool = AtomicBool::new(false); // last SetVisible(bool) state
static FF_PRESSED_FOR: AtomicUsize = AtomicUsize::new(0); // instance we've already engaged
static FF_PUSH: AtomicUsize = AtomicUsize::new(0); // OnPushFastForward() code
static FF_PUSH_MI: AtomicUsize = AtomicUsize::new(0);
static FF_VIS: AtomicUsize = AtomicUsize::new(0); // IsVisible()->bool code (secondary gate)
static FF_VIS_MI: AtomicUsize = AtomicUsize::new(0);
static FF_LAST_MS: AtomicU64 = AtomicU64::new(0);
crate::skip_hook_slot!(TR_FFAWAKE, D_FFAWAKE);
crate::skip_hook_slot!(TR_FFVIS, D_FFVIS); // SetVisible(bool) — the reliable capture + shown-state signal

/// Capture the live RaceUIFastForward at Awake (a fresh instance per race). Kept as a FALLBACK — the
/// primary capture is SetVisible, because Awake alone was not landing the press (0 engagements).
unsafe extern "C" fn on_ff_awake(this: *mut c_void, m: *mut c_void) {
    if let Ok(mut g) = FF_UI_H.lock() {
        *g = Some(crate::il2cpp::GCHandle::new(this, false)); // old handle freed on drop
    }
    let t = TR_FFAWAKE.load(Ordering::Relaxed);
    if t != 0 {
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void) = std::mem::transmute(t);
        f(this, m);
    }
}

/// `RaceUIFastForward.SetVisible(bool)` — the game shows/hides the FF button through this. It is our
/// PRIMARY capture: it hands us the live instance AND whether the button is on screen, so we never
/// have to guess with Awake timing or interpret IsVisible ourselves. On a show (true) we clear the
/// per-instance press dedup so a new race re-engages. The actual press happens on the pump (calling
/// OnPushFastForward from inside this hook would re-enter the game's own SetVisible flow).
unsafe extern "C" fn on_ff_setvisible(this: *mut c_void, shown: bool, m: *mut c_void) {
    if let Ok(mut g) = FF_UI_H.lock() {
        *g = Some(crate::il2cpp::GCHandle::new(this, false)); // old handle freed on drop
    }
    FF_SHOWN.store(shown, Ordering::Relaxed);
    if shown && FF_PRESSED_FOR.load(Ordering::Relaxed) != this as usize {
        FF_PRESSED_FOR.store(0, Ordering::Relaxed); // re-arm for this (freshly shown) instance
    }
    let t = TR_FFVIS.load(Ordering::Relaxed);
    if t != 0 {
        let f: unsafe extern "C" fn(*mut c_void, bool, *mut c_void) = std::mem::transmute(t);
        f(this, shown, m);
    }
}

/// Auto-engage in-race Fast-Forward once the FF button is shown. Runs from the ButtonCommon.Update
/// pump. Presses once per race (per-instance dedup); the game applies the accelerated timescale.
unsafe fn drive_race_ff() {
    if !crate::skip::is_race_ff_enabled() || in_overseer() {
        return;
    }
    // Re-fetch the live pointer through the GC handle each frame — null once collected, so the
    // calls below can never run on freed memory. FF_PRESSED_FOR keys on the pointer VALUE only
    // (dedup compare, never dereferenced), so keeping it a usize is fine.
    let ui = match FF_UI_H.lock() {
        Ok(g) => g.as_ref().map_or(std::ptr::null_mut(), |h| h.target()),
        Err(_) => std::ptr::null_mut(),
    };
    let push = FF_PUSH.load(Ordering::Relaxed);
    if ui.is_null() || push == 0 || FF_PRESSED_FOR.load(Ordering::Relaxed) == ui as usize {
        return;
    }
    // The strong handle keeps the managed wrapper alive, so this m_CachedPtr read is on LIVE memory
    // (with the old raw cache this very check was the first use-after-free read).
    if crate::il2cpp::unity_object_destroyed(ui) {
        if let Ok(mut g) = FF_UI_H.lock() {
            *g = None; // frees the handle; the wrapper can now be collected
        }
        FF_SHOWN.store(false, Ordering::Relaxed);
        return;
    }
    // Gate on the shown state SetVisible gave us; fall back to IsVisible() only if we never saw a
    // SetVisible call (belt-and-braces for a build where the game toggles display another way).
    if !FF_SHOWN.load(Ordering::Relaxed) {
        let vis = FF_VIS.load(Ordering::Relaxed);
        if vis == 0 {
            return;
        }
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool = std::mem::transmute(vis);
        if !f(ui, FF_VIS_MI.load(Ordering::Relaxed) as *mut c_void) {
            return;
        }
    }
    let now = now_ms();
    if now.saturating_sub(FF_LAST_MS.load(Ordering::Relaxed)) < 300 {
        return;
    }
    FF_LAST_MS.store(now, Ordering::Relaxed);
    FF_PRESSED_FOR.store(ui as usize, Ordering::Relaxed);
    let _g = ReentryGuard::enter();
    let f: unsafe extern "C" fn(*mut c_void, *mut c_void) = std::mem::transmute(push);
    f(ui, FF_PUSH_MI.load(Ordering::Relaxed) as *mut c_void);
    rr_log("[race] auto Fast-Forward engaged (RaceUIFastForward.OnPushFastForward)");
}

// ── Native result-scene skip (hyper-native Phase 1) ─────────────────────────────────────────────
// Gallop.RaceResultSceneUI is the whole race-result presentation; OnPushSkip() is its OWN skip-button
// handler (it runs the game's guard logic — the same reasoning that chose OnPushFastForward over raw
// mode-sets), with Skip() as the fallback. We capture a live instance at a lifecycle hook (Setup /
// PlayFinishOrder) into a STRONG GCHandle (RES_UI_H — the FF_UI_H pattern) and, once the result window
// is armed for a WON race, invoke it ONCE, collapsing the screens instantly. Layered ON TOP of the
// auto_press engine: if the native skip fires, the whitelisted buttons never appear and the press
// engine naturally no-ops; if the method misses on a future build, behaviour degrades to today's
// proven press flow. Deliberately NOT fired on the loss/no-retries path (collapsing a loss result
// risks landing on the paid retry-purchase) — hence the STRICT finish_order==1 gate below.
static RES_UI_H: Mutex<Option<crate::il2cpp::GCHandle>> = Mutex::new(None);
static RES_SKIP: AtomicUsize = AtomicUsize::new(0); // OnPushSkip() code (preferred)
static RES_SKIP_MI: AtomicUsize = AtomicUsize::new(0);
static RES_SKIP_FB: AtomicUsize = AtomicUsize::new(0); // Skip() code (fallback)
static RES_SKIP_FB_MI: AtomicUsize = AtomicUsize::new(0);
static RES_SKIPPED_FOR: AtomicUsize = AtomicUsize::new(0); // instance already skipped (per-instance dedup)
static RES_LAST_MS: AtomicU64 = AtomicU64::new(0);
pub(crate) static RR_NATIVE_SKIPS: AtomicU64 = AtomicU64::new(0);
crate::skip_hook_slot!(TR_RRSETUP, D_RRSETUP); // RaceResultSceneUI.Setup — capture
crate::skip_hook_slot!(TR_RRFINISH, D_RRFINISH); // RaceResultSceneUI.PlayFinishOrder — capture

/// STRICT won gate: 1st place only. Race telemetry lives behind `raceread` (always on in shipping
/// builds); without it we never native-skip (behaviour falls back to the press engine).
#[cfg(feature = "raceread")]
fn player_won_strict() -> bool {
    crate::race::player_finish_order() == 1
}
#[cfg(not(feature = "raceread"))]
fn player_won_strict() -> bool {
    false
}

/// (Re)capture the live RaceResultSceneUI + re-arm the per-instance dedup for a genuinely new scene.
/// Logs once per new instance (Setup + PlayFinishOrder on the same scene log only once).
fn capture_result_ui(this: *mut c_void, src: &str) {
    let prev = if let Ok(mut g) = RES_UI_H.lock() {
        let prev = g.as_ref().map_or(std::ptr::null_mut(), |h| h.target());
        *g = Some(crate::il2cpp::GCHandle::new(this, false)); // old handle freed on drop
        prev
    } else {
        std::ptr::null_mut()
    };
    if prev != this {
        RES_SKIPPED_FOR.store(0, Ordering::Relaxed); // fresh instance → allow one native skip
        rr_log(&format!("[race-result] RaceResultSceneUI captured ({src})"));
    }
}

unsafe extern "C" fn on_rr_setup(this: *mut c_void, m: *mut c_void) {
    capture_result_ui(this, "Setup");
    call_orig(&TR_RRSETUP, this, m);
}
unsafe extern "C" fn on_rr_finish(this: *mut c_void, m: *mut c_void) {
    capture_result_ui(this, "PlayFinishOrder");
    call_orig(&TR_RRFINISH, this, m);
}

/// Fire the game's OWN result-scene skip ONCE for a WON result. Gated identically to auto_press
/// (WINDOW_OPEN + rr_should_advance + career + !team-trials + !overseer) PLUS the stricter won==1
/// requirement, so it never collapses a loss / no-retries result. Per-instance dedup + 300ms throttle
/// + Destroy staleness + ReentryGuard, exactly like drive_race_ff.
/// Press GO on a live Inspiration ("Succession") event. Thin wrapper so the per-sweep block reads
/// like its siblings; all the logic (capture, dedup, guards) lives in `skip::inspire`.
unsafe fn drive_inspiration() {
    crate::skip::inspire::drive();
}

unsafe fn drive_result_skip() {
    if in_overseer()
        || crate::skip::result_stood_down()
        || !WINDOW.is_open()
        || IN_TEAM_TRIALS.load(Ordering::Relaxed)
        || !career_fresh()
        || !rr_should_advance()
        || !player_won_strict()
    {
        return;
    }
    let ui = match RES_UI_H.lock() {
        Ok(g) => g.as_ref().map_or(std::ptr::null_mut(), |h| h.target()),
        Err(_) => std::ptr::null_mut(),
    };
    // Prefer OnPushSkip (the button's own handler + guard); fall back to Skip.
    let (code, mi, which) = {
        let s = RES_SKIP.load(Ordering::Relaxed);
        if s != 0 {
            (s, RES_SKIP_MI.load(Ordering::Relaxed), "OnPushSkip")
        } else {
            (RES_SKIP_FB.load(Ordering::Relaxed), RES_SKIP_FB_MI.load(Ordering::Relaxed), "Skip")
        }
    };
    if ui.is_null() || code == 0 || RES_SKIPPED_FOR.load(Ordering::Relaxed) == ui as usize {
        return;
    }
    // The strong handle keeps the wrapper alive, so this m_CachedPtr read is on LIVE memory.
    if crate::il2cpp::unity_object_destroyed(ui) {
        if let Ok(mut g) = RES_UI_H.lock() {
            *g = None; // frees the handle; the wrapper can now be collected
        }
        return;
    }
    let now = now_ms();
    if now.saturating_sub(RES_LAST_MS.load(Ordering::Relaxed)) < 300 {
        return;
    }
    RES_LAST_MS.store(now, Ordering::Relaxed);
    RES_SKIPPED_FOR.store(ui as usize, Ordering::Relaxed); // one native skip per instance
    let _g = ReentryGuard::enter();
    let f: unsafe extern "C" fn(*mut c_void, *mut c_void) = std::mem::transmute(code);
    f(ui, mi as *mut c_void);
    RR_NATIVE_SKIPS.fetch_add(1, Ordering::Relaxed);
    rr_log(&format!("[race-result] native skip ({which}) — RaceResultSceneUI (won)"));
}

// ── Hyper Skip ────────────────────────────────────────────────────────────────────────────────
// Throttle so a per-frame button Update doesn't machine-gun clicks.
//
// This is a DEBOUNCE, not a measured requirement — nothing in the game demands a particular spacing,
// it only stops one button being clicked many times before the UI has reacted. That matters because
// it is pure wall-clock time and it dominates: at ~170 fps a frame is 6 ms, so 220 ms is ~37 frames
// of doing nothing, and a chain of five taps spent over a second waiting rather than skipping. Raw
// frame rate cannot touch this — the gate is the floor.
//
// 120 ms is still ~20 frames of settling, which is more than any of these taps needs, while nearly
// halving the chain. If a tap ever double-fires (content skipped twice, a prompt dismissed before it
// renders), this is the first number to put back up.
pub(crate) static HYPER_LAST_MS: AtomicU64 = AtomicU64::new(0);
pub(crate) const HYPER_GAP_MS: u64 = 220;
/// Gap for the race-skip button specifically — see the `gap` selection in `hyper_tick`. Deliberately
/// far larger than the tap gap: one click should be enough, and the cost of an extra one is a stray
/// unlock dialog rather than a slightly late tap.
pub(crate) const RACESKIP_GAP_MS: u64 = 600;
/// Distinct button names the auto-tap has seen, for the trace-armed diagnostic above. Bounded so an
/// armed trace left on for a whole session cannot grow without limit.
static TAP_SEEN: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);
const TAP_SEEN_CAP: usize = 200;
// Last time Hyper clicked the RaceSkip button specifically. A normal skip click SKIPS (no dialog); if a
// dialog pops up right after, the skip was BLOCKED — you haven't unlocked skipping this grade yet
// ("forced to watch a race you haven't won"). Only THAT case auto-disables Hyper (not any dialog).
pub(crate) static RACESKIP_LAST_MS: AtomicU64 = AtomicU64::new(0);

/// Auto-advance without user taps (experimental, opt-in). Runs from the ButtonCommon.Update pump,
/// so `this` is one button being updated this frame. Career-gated + Team-Trials-guarded (same as the
/// race-result skip) so it can't leak into other modes. Only presses buttons the player could press:
///   • the in-race Skip button (RaceSkipButton) the moment it's live  → INSTANT RACES
///   • pure tap-to-continue prompts (ScreenTap / TouchSprite)         → AUTO-TAP
/// It deliberately does NOT press generic/confirm buttons (ButtonCommon/Next/Continue/purchase
/// dialogs) — those are left to the careful, win/retry-gated race-result path so nothing spends
/// resources or desyncs.
unsafe fn hyper_tick(this: *mut c_void) {
    // Diagnostic FIRST, above every gate. With the text trace armed, name every button this pump
    // sees — deduped, capped, one line each. It must not depend on Hyper being ENABLED: the whole
    // reason to run it is to find out what a button is called so it can be added as a tap target,
    // and Hyper disables itself (locked race skip), which would silently take the diagnostic with
    // it. The `[ctrace]` logger only names buttons the player CLICKS, which cannot answer "what is
    // that button called" for one we want to click FOR them. GameObject names are asset strings, not
    // C# identifiers, so they are not in global-metadata.dat — they have to be observed live.
    if crate::loc_settext::trace_on() {
        let n = button_name(this);
        if !n.is_empty() {
            if let Ok(mut g) = TAP_SEEN.lock() {
                let seen = g.get_or_insert_with(std::collections::HashSet::new);
                if seen.len() < TAP_SEEN_CAP && seen.insert(n.clone()) {
                    crate::tools::log(&format!("[hyper] saw button: {n}"));
                }
            }
        }
    }
    if !crate::skip::is_hyper_enabled()
        || in_overseer()
        || IN_TEAM_TRIALS.load(Ordering::Relaxed)
        || !career_fresh()
    {
        return;
    }
    let now = now_ms();
    let name = button_name(this);
    // The race-skip button gets its OWN, much longer gap. One click is meant to be enough — the
    // result window then arms and `!WINDOW.is_open()` stops us — but the window takes a moment to
    // appear, and at the general 120 ms tap gap that gap was filled with SEVENTEEN clicks (observed:
    // "+16 repeats suppressed"). On an unlocked race that is merely wasteful; on a locked one every
    // click spawns an unlock dialog, so the burst is fired straight into the failure case. Tapping
    // sprites want to be quick; this one wants to be patient.
    let gap = if name.contains("RaceSkipButton") { RACESKIP_GAP_MS } else { HYPER_GAP_MS };
    if now.saturating_sub(HYPER_LAST_MS.load(Ordering::Relaxed)) < gap {
        return;
    }
    let act = if name.contains("RaceSkipButton") {
        // Instant races: click the Skip button once (until the result window arms via OnPointerClick),
        // exactly as if the player pressed it — the proven result-advance flow takes it from there.
        !WINDOW.is_open()
    } else {
        // ONLY the two pure "tap to continue" sprites — inert overlays with no side effects.
        //
        // DO NOT add advance buttons here. NextButton / SingleModeNextButton were added and caused a
        // soft-lock on the post-race win screen: a managed exception unwound through the click hook
        // ("[guard] recovered: re-entry guard leaked") and the result screen then refused to advance
        // ("[skip] race result did not advance"). The cause is a CONFLICT, not the buttons — the
        // result screen already has a presser for exactly those two (`auto_press` via
        // `press_allowed`), gated on rr_should_advance, capped at MULTI_MAX, and aware of when a
        // purchase dialog may be underneath. This pump has none of that context and fires every
        // HYPER_GAP_MS, so the two of them clicked the same button and raced each other.
        //
        // Anything that advances a screen belongs in the press path, which knows WHICH screen it is
        // on; this one only knows a button's name. CheckScreenTap and TapButton were also added here
        // from names alone, with no idea which screens they belong to — same mistake, removed too.
        name == "ScreenTap" || name == "TouchSprite"
    };
    // Never advance out from under an open event choice. Nothing should present a Next button while
    // one is up, but this pump sees every button on screen with no idea which screen it is, and
    // clicking past a choice would skip the decision the predictions exist to inform.
    if act && choice_pending() && !name.contains("RaceSkipButton") {
        return;
    }
    if !act {
        return;
    }
    let _g = ReentryGuard::enter();
    if click_now(this) {
        HYPER_LAST_MS.store(now, Ordering::Relaxed);
        if name.contains("RaceSkipButton") {
            RACESKIP_LAST_MS.store(now, Ordering::Relaxed);
            rr_log("[hyper] auto-clicked RaceSkipButton (instant race)");
        }
    }
}
unsafe extern "C" fn on_pointer_click(this: *mut c_void, evt: *mut c_void, m: *mut c_void) {
    // Diagnostic (armed with the text trace): name every button the player actually clicks. This
    // answers the question the Scout investigation is stuck on — whether the click REACHES the game
    // at all. If "Scout" appears here and nothing follows, the click lands and its handler dies; if
    // it never appears, input is being swallowed before the game sees it (a modal/raycast blocker).
    if crate::loc_settext::trace_on() {
        crate::tools::log(&format!("[ctrace] click -> {}", crate::ui_input::button_name(this)));
    }
    let t = TR_ONPC.load(Ordering::Relaxed);
    if t != 0 {
        let f: Void3 = std::mem::transmute(t);
        f(this, evt, m);
    }
    if !RACE_RESULT_ENABLED.load(Ordering::Relaxed) || in_overseer() {
        return;
    }
    // Only the in-race skip button is the anchor.
    if !button_name(this).contains("RaceSkipButton") {
        return;
    }
    // Log EVERY RaceSkipButton click + the gate decision, so the arm/disarm behaviour can be
    // verified from overseer-native.log without having to reproduce the rare menu-bug live.
    let tt = IN_TEAM_TRIALS.load(Ordering::Relaxed);
    let cf = career_fresh();
    let already = WINDOW.raw();
    let age = now_ms().saturating_sub(LAST_CAREER_MS.load(Ordering::Relaxed));
    if !already && !tt && cf {
        WINDOW.arm();
        // The result sequence has started; it must reach the career view within the watchdog's
        // window. If it doesn't, the auto-advance stalled (a locked advance button, a swallowed
        // press) and the watchdog lifts the input block, retries, then stands the leg down.
        RESULT_PROGRESS.expect();
        clear_rr_caches();
        #[cfg(feature = "raceread")]
        let fo = crate::race::player_finish_order();
        rr_log(&format!(
            "[race-result] RaceSkipButton -> ARMED (career age={age}ms, finish_order={fo} -> {})",
            if fo == 1 { "SKIP" } else { "MANUAL" }
        ));
    } else {
        crate::tools::trace(&format!(
            "[race-result] RaceSkipButton -> NOT armed (career_fresh={cf} age={age}ms, team_trials={tt}, already_open={already})"
        ));
    }
}
unsafe extern "C" fn on_change_main_view(this: *mut c_void, m: *mut c_void) {
    call_orig(&TR_CMV, this, m);
    // ChangeMainView is single-mode only → we're in career; refresh the heartbeat + clear TT guard.
    crate::ui_tempo::set_home_active(false); // in a career view now → restore full UI speed
    mark_career();
    // Reaching a career main view is the GAME telling us the previous flow completed — resolve every
    // outstanding progression expectation from evidence rather than from a timer.
    crate::skip::train::note_progressed();
    crate::skip::event::note_progressed();
    note_progressed();
    crate::skip::inspire::clear(); // left the succession view
    IN_TEAM_TRIALS.store(false, Ordering::Relaxed);
    // Back at the training lobby → the skill-learning flow (if any) is over: clear its view gate + all
    // one-shot windows (mirrors the WINDOW_OPEN disarm below).
    crate::skip::skill::clear_all("ChangeMainView");
    if WINDOW.disarm() {
        clear_rr_caches();
        rr_log("[race-result] back in career (ChangeMainView) -> window disarmed");
    }
}
// HomeViewController.PlayInView — the lobby (Home) is animating in, i.e. we've LEFT career. Hard-clear
// the career heartbeat + disarm the race-result skip, so it can never leak into non-career modes
// (you always pass through Home to reach Daily / Champions / Legend / Story). Returns the coroutine.
unsafe extern "C" fn on_home_in(this: *mut c_void, m: *mut c_void) -> *mut c_void {
    let t = TR_HOME.load(Ordering::Relaxed);
    let rv = if t != 0 {
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void = std::mem::transmute(t);
        f(this, m)
    } else {
        std::ptr::null_mut()
    };
    LAST_CAREER_MS.store(0, Ordering::Relaxed); // career heartbeat → stale immediately
    crate::skip::train::note_progressed(); // left career entirely — nothing is pending any more
    crate::skip::event::note_progressed();
    note_progressed();
    crate::skip::inspire::clear();
    crate::skip::event::clear_goal_complete(); // re-enable event skip for the next career
    crate::ui_tempo::set_home_active(true); // on the idle lobby → pin UI speed to 1x (no decor flashing)
    crate::ui_tempo::set_gacha_active(false); // back at the lobby → release the Scout/Gacha 1x pin
    crate::skip::skill::clear_all("Home"); // left career entirely → hard-clear the skill-learn flow
    let was_open = WINDOW.disarm();
    if was_open {
        clear_rr_caches();
    }
    rr_log(&format!("[race-result] Home reached -> gate CLOSED (heartbeat cleared, was_armed={was_open})"));
    rv
}

// GachaMainViewController lifecycle — the Scout (gacha) screen. Its intro runs one-shot tweens synced
// to real-time banner-movie playback + async loads; at 10-20x UI tempo the tween side outruns the
// events it's married to and the screen SOFT-LOCKS. Pin UI tempo to 1x while the view is coming up.
//
// SETTLED (field logs, 2026-07-23, 36 sessions): BeginView and InitializeView BOTH fire on every
// scout-open — 19 occurrences each. The pin engages. Earlier comments in this file claimed it had
// "NEVER ONCE ENGAGED" based on a log in which the InitializeView hook simply had no log line of its
// own; that claim was wrong and sent two separate investigations hunting for an entry point that had
// already been found. The [gtrace] scaffolding those investigations added is gone (see below): none
// of the four methods it traced ever fired, which is consistent — the two hooks here are the path.
unsafe extern "C" fn on_gacha_begin(this: *mut c_void, m: *mut c_void) {
    crate::ui_tempo::set_gacha_active(true);
    rr_log("[view] Scout/Gacha view begin -> UI tempo pinned 1x (soft-lock guard)");
    let t = TR_GACHA.load(Ordering::Relaxed);
    if t != 0 {
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void) = std::mem::transmute(t);
        f(this, m);
    }
}
unsafe extern "C" fn on_gacha_init(this: *mut c_void, m: *mut c_void) {
    crate::ui_tempo::set_gacha_active(true);
    // This log line was MISSING, which is exactly why nobody could tell whether the pin engaged: with
    // BeginView silent and InitializeView unlogged, "did the pin fire?" was unanswerable from the log.
    rr_log("[view] Scout/Gacha InitializeView -> UI tempo pinned 1x (soft-lock guard)");
    let t = TR_GACHAINIT.load(Ordering::Relaxed);
    if t != 0 {
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void) = std::mem::transmute(t);
        f(this, m);
    }
}

// [gtrace] REMOVED. Four extra detours on GachaMainViewController (SetupMenu, InitializeViewForChara,
// InitializeViewForSupport, InitializeImageEffect) existed only to discover which lifecycle method
// Scout actually runs. Across 36 sessions of field logs they logged ENTERED exactly zero times while
// the two pin hooks above logged 19 each — so the question they were asked is answered, and they were
// four permanent detours doing nothing. Removing them is the probe-then-remove workflow completing.

// HomeViewController.PlayOutView — leaving the lobby for another main view (Race / Enhance / Scout /
// Story / a career). Clear the Home flag so the UI speed-up resumes at full tempo off the lobby.
unsafe extern "C" fn on_home_out(this: *mut c_void, m: *mut c_void) -> *mut c_void {
    crate::ui_tempo::set_home_active(false);
    let t = TR_HOMEOUT.load(Ordering::Relaxed);
    if t != 0 {
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void = std::mem::transmute(t);
        f(this, m)
    } else {
        std::ptr::null_mut()
    }
}
// Diagnostic: log the class of each distinct dialog pushed while auto-skip-warnings is on in a career,
// so the advisory-WARNING dialog can be identified precisely (and safely auto-confirmed later — never
// a purchase dialog). Dedup by class pair.
static LOGGED_DIALOGS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
// DIAG (warnings-confirm bring-up): distinct dialog Data objects already dumped, keyed by a hash of
// the dump content so each distinct advisory dumps exactly once (not once-per-class — the
// consecutive-race warning shares DialogCommon with every other dialog, so a class-pair dedup would
// miss it if a different DialogCommon appeared first).
static DUMPED_DIALOGS: OnceLock<Mutex<HashSet<u64>>> = OnceLock::new();
fn log_dialog_once(arg: *mut c_void, dlg: *mut c_void) {
    if !(crate::skip::is_warnings_enabled()
        || crate::skip::is_hyper_enabled()
        || crate::skip::is_inspiration_enabled()
        || crate::skip::is_skill_learn_enabled())
        || !career_fresh()
    {
        return;
    }
    let an = if arg.is_null() { String::new() } else { il2cpp::object_class_name(arg) };
    let dn = if dlg.is_null() { String::new() } else { il2cpp::object_class_name(dlg) };
    let key = format!("{an}|{dn}");
    let set = LOGGED_DIALOGS.get_or_init(|| Mutex::new(HashSet::new()));
    if let Ok(mut s) = set.lock() {
        if s.insert(key) {
            rr_log(&format!("[dialog] pushed: arg=\"{an}\" dialog=\"{dn}\""));
        }
    }
    // Shallow-dump the dialog's Data object (its own fields incl. the message text) so a SPECIFIC
    // advisory — e.g. the consecutive-race warning — can be pinned down for a targeted auto-confirm
    // that never touches a purchase/spend dialog. One dump per distinct dialog (content-hash dedup).
    //
    // TRACE-GATED, and it should have been from the start. This is a three-level managed field walk
    // over a live `DialogCommon.Data` — through `_dialog._circleMask`, `m_Canvas`, `m_CachedMesh`
    // and raw delegate slots — plus a multi-KB JSON build, run ON THE MAIN THREAD inside the
    // `PushDialog` detour, immediately after `call_orig`, while the dialog is still mid-construction.
    // The content-hash dedup below only suppresses the log LINE; the walk itself ran on every push.
    //
    // Its stated safety net is void: `il2cpp_json` wraps the walk in `catch_unwind`, but this crate
    // builds with `panic = "abort"` (Cargo.toml), so there is nothing to catch. A field-log session
    // ends with a `[dialog-dump]` line as its final entry.
    //
    // It is a diagnostic that has already answered its question, so it now costs nothing unless
    // someone is actually debugging dialogs.
    if !arg.is_null() && crate::tools::log::trace_enabled() {
        let json: String = crate::il2cpp_json::dump_shallow(arg as usize).chars().take(3500).collect();
        use std::hash::{Hash, Hasher};
        let mut hh = std::collections::hash_map::DefaultHasher::new();
        json.hash(&mut hh);
        let dset = DUMPED_DIALOGS.get_or_init(|| Mutex::new(HashSet::new()));
        if let Ok(mut s) = dset.lock() {
            if s.insert(hh.finish()) {
                rr_log(&format!("[dialog-dump] {an} :: {json}"));
            }
        }
    }
}

// ── non-blocking dialogs: REMOVED (it crashed the game) ─────────────────────────────────────────
//
// The idea was right — the Steam event-choice SIDE panel should not make the skips stand down — but
// the identification was fatally wrong. It read `DialogType` / `DispStackType` off the dialog's Data
// with `read_ref_ptr`, and those are **enum value-type fields, not strings**: the read took 8 bytes
// of inline enum data as a pointer and `read_string` then dereferenced it. Live crash, first career
// resume after deploy:
//
//     code : 0xc0000005   at cri_mana_vpx.dll + 0x1f10f7
//     access violation: read at 0xffffffff00000010      (garbage_base + 0x10 = String::length)
//
// `htt_il2cpp::read_string_field` now refuses any field whose il2cpp type isn't System.String, so
// this specific mistake cannot be made again. The actual goal is met a better way — see
// `drive_event_choice`, which keys on the game's OWN "I am waiting for a selection" flag instead of
// trying to classify dialogs.

/// Armed until this ms timestamp: a matched advisory WARNING is on screen, so the button pump may
/// auto-click its OK (ButtonRight) ONCE. 0 = disarmed. Set only by `try_arm_warning_confirm`.
static WARN_CONFIRM_UNTIL: AtomicU64 = AtomicU64::new(0);
/// The MATCHED warning's Data, rooted via a STRONG GCHandle (same fix as CHOICE_CTRL_H: the GC
/// can't see Rust statics) so the click-time re-verify reads live memory. Set on arm, cleared on
/// click / disarm. Every touch point (arm, verify, clear) is a game-thread hook, which is attached.
static WARN_DATA_H: Mutex<Option<crate::il2cpp::GCHandle>> = Mutex::new(None);
/// Auto-OK window. Was an inline 2500 — the live log proved a real consecutive-race warning whose
/// ButtonRight stayed locked past 2.5s, so the click never landed. 6s is safe ONLY because the
/// click is re-gated per frame on the rooted Data still matching + still being frontmost.
const WARN_CONFIRM_WINDOW_MS: u64 = 6000;

/// True iff `data` (a `DialogCommon.Data`) is the advisory-WARNING class and NOT a purchase/detail.
/// Read at BOTH arm time and click time so a widened window can never fire on a different dialog.
/// Field names + values were captured live from the dialog-dump diagnostic. Reads are on the game
/// (attached) thread.
fn warning_data_matches(data: *mut c_void) -> bool {
    // Plausibility gate: the 2-arg push path probes BOTH args, and a slot that isn't a managed ref
    // (an enum/bool passed by value) would be a wild pointer here — reject unaligned/low values.
    if data.is_null() || (data as usize) < 0x10000 || (data as usize) & 0x7 != 0 {
        return false;
    }
    use crate::htt_il2cpp as h;
    let v = match unsafe { h::val_of(data as *mut h::RawObject) } {
        Some(v) => v,
        None => return false,
    };
    // Type-checked: a field that is NOT a System.String yields "" instead of a wild pointer read.
    let read = |name: &str| -> String {
        unsafe { h::read_string_field(&v, name).unwrap_or_default() }
    };
    let title = read("Title");
    let right = read("RightButtonText");
    let left = read("LeftButtonText");
    // Localised "Warning" header the game may render (the field is read AFTER localisation).
    // Broadened; add any locale that is ever missed. This stays the identity gate so we only ever
    // OK the WARNING class (never a generic "Cancel training?" confirm) — purchases/details are
    // excluded twice over (below).
    let is_warning = matches!(
        title.as_str(),
        "Warning" | "Caution" | "Notice"
            | "Avertissement" | "Attention"
            | "警告" | "警示" | "注意"
            | "Advertencia" | "Aviso"
            | "Warnung" | "Achtung"
            | "Avviso" | "Attenzione"
            | "경고" | "주의"
    );
    // RIGHT must be an OK-equivalent (purchases are Buy/Acheter/Boutique; details are Course) and a
    // non-empty LEFT (Cancel/Annuler) guarantees the two-button advisory form, never a one-button
    // spend.
    let right_is_ok = right.eq_ignore_ascii_case("ok") || right.eq_ignore_ascii_case("d'accord");
    is_warning && right_is_ok && !left.is_empty()
}

/// If `data` (a `DialogCommon.Data`) is an advisory WARNING with a plain OK/Cancel — the
/// consecutive-race notice and its kin — arm a window in which the button pump auto-clicks OK.
/// STRICT so it can NEVER fire on a purchase (titled Confirmation, right button Buy/Acheter) or the
/// race-details dialog (right button "Course", MIDDLE form): see `warning_data_matches`. Returns
/// true if it armed. Roots the Data via a strong GCHandle so the click-time re-verify reads live
/// memory (same pattern as CHOICE_CTRL_H).
fn try_arm_warning_confirm(data: *mut c_void) -> bool {
    if !crate::skip::is_warnings_enabled() || !career_fresh() {
        return false;
    }
    if !warning_data_matches(data) {
        return false;
    }
    if let Ok(mut g) = WARN_DATA_H.lock() {
        *g = Some(crate::il2cpp::GCHandle::new(data, false)); // old handle freed on drop
    }
    WARN_CONFIRM_UNTIL.store(now_ms() + WARN_CONFIRM_WINDOW_MS, Ordering::Relaxed);
    // Definitionally in-career (a career-only advisory just pushed) → refresh the heartbeat so a
    // long menu idle (>6 min since ChangeMainView) can't have staled career_fresh and blocked
    // future arms.
    mark_career();
    rr_log("[warn-confirm] armed (advisory WARNING, OK/Cancel) — auto-OK within window");
    true
}

/// The armed WARNING Data must (1) still read as a warning and (2) still be the frontmost dialog,
/// so we can NEVER click a different dialog's ButtonRight after the original warning was dismissed.
/// Uses the Data `_dialog` back-reference (present in the live dump) compared against
/// DialogManager.GetForeFrontDialog().
fn warn_target_still_frontmost() -> bool {
    let data = match WARN_DATA_H.lock() {
        Ok(g) => g.as_ref().map_or(std::ptr::null_mut(), |h| h.target()),
        Err(_) => std::ptr::null_mut(),
    };
    if data.is_null() || !warning_data_matches(data) {
        return false;
    }
    if let Some(gf) = crate::skip::shop::GET_FOREFRONT.get() {
        if gf.ok() {
            let front = unsafe { gf.call_ptr_static() };
            let dlg = {
                use crate::htt_il2cpp as h;
                match unsafe { h::val_of(data as *mut h::RawObject) } {
                    Some(v) => unsafe { h::read_ref_ptr(&v, "_dialog") }
                        .map_or(std::ptr::null_mut(), |p| p as *mut c_void),
                    None => std::ptr::null_mut(),
                }
            };
            // Only OK the warning that is actually on top.
            return !front.is_null() && front == dlg;
        }
    }
    true // forefront accessor unavailable → fall back to the field match alone (still warning-gated)
}

/// True iff `data` (a rooted `DialogCommon.Data`) is still the frontmost dialog — its `_dialog`
/// back-reference equals `DialogManager.GetForeFrontDialog()`. The click-time re-verify for any
/// windowed auto-OK (warn-confirm reimplements this inline; the skill-learning confirm/result reuse
/// this) so a widened window can never click a DIFFERENT dialog's button after the matched one closed.
/// Falls back to `true` when the forefront accessor is unavailable (callers still gate on a field match).
pub(crate) fn data_still_frontmost(data: *mut c_void) -> bool {
    if data.is_null() {
        return false;
    }
    if let Some(gf) = crate::skip::shop::GET_FOREFRONT.get() {
        if gf.ok() {
            let front = unsafe { gf.call_ptr_static() };
            let dlg = {
                use crate::htt_il2cpp as h;
                match unsafe { h::val_of(data as *mut h::RawObject) } {
                    Some(v) => unsafe { h::read_ref_ptr(&v, "_dialog") }
                        .map_or(std::ptr::null_mut(), |p| p as *mut c_void),
                    None => std::ptr::null_mut(),
                }
            };
            return !front.is_null() && front == dlg;
        }
    }
    true
}

/// SAFETY: auto-disable Hyper Skip ONLY when the race SKIP was blocked — i.e. a dialog appears right
/// after Hyper clicked the RaceSkip button. A normal skip click just skips (no dialog); a dialog there
/// means "you haven't unlocked skipping this grade — watch the race", where Hyper would keep poking a
/// locked control and soft-lock. Gated on RACESKIP_LAST_MS (NOT any Hyper click) so ordinary dialogs
/// during the run — event/result/reward popups — never turn Hyper off. Persisted so it stays off.
fn hyper_safety_on_dialog() {
    // Tight window: the "unlock requirements" dialog appears essentially instantly when a locked skip
    // is clicked; a real race's result dialogs come seconds later (after the race resolves), so 1s
    // catches the block without ever catching a normal post-skip result popup.
    if crate::skip::is_hyper_enabled()
        && now_ms().saturating_sub(RACESKIP_LAST_MS.load(Ordering::Relaxed)) < 1000
    {
        crate::skip::set_hyper_enabled(false);
        crate::settings::save_current();
        rr_log("[hyper] AUTO-DISABLED — race skip is locked (you haven't won this grade yet, so the race must be watched). Re-enable Hyper Skip once it's unlocked.");
    }
}

unsafe extern "C" fn on_push1(this: *mut c_void, a: *mut c_void, m: *mut c_void) -> *mut c_void {
    // Arm the hands-off guard BEFORE the original runs (the freeze happens inside the dialog's own
    // flow, so a store placed after call_orig can be too late — same lesson as mark_goal_complete).
    crate::skip::DIALOG_PUSH_GUARD_UNTIL.store(now_ms() + 1500, Ordering::Relaxed);
    let t = TR_PUSH1.load(Ordering::Relaxed);
    let rv = if t != 0 {
        let f: Push1 = std::mem::transmute(t);
        f(this, a, m)
    } else {
        std::ptr::null_mut()
    };
    log_dialog_once(a, rv);
    try_arm_warning_confirm(a);
    // Skill-learning confirm (#1) / result popup (#2): arm the matching one-shot window if `a` is one
    // of them (self-gated on the feature + SKILL_VIEW_ACTIVE; its matchers reject non-skill dialogs).
    crate::skip::skill::try_arm_on_push(a);
    hyper_safety_on_dialog();
    if rr_should_advance() && WINDOW.is_open() && !in_overseer() {
        auto_close(rv);
    }
    rv
}
unsafe extern "C" fn on_push2(
    this: *mut c_void,
    a: *mut c_void,
    b: *mut c_void,
    m: *mut c_void,
) -> *mut c_void {
    crate::skip::DIALOG_PUSH_GUARD_UNTIL.store(now_ms() + 1500, Ordering::Relaxed);
    let t = TR_PUSH2.load(Ordering::Relaxed);
    let rv = if t != 0 {
        let f: Push2 = std::mem::transmute(t);
        f(this, a, b, m)
    } else {
        std::ptr::null_mut()
    };
    log_dialog_once(a, rv);
    // The 2-arg push (PushDialogSequence) can carry the Data in EITHER slot — the live log shows
    // pushes whose FIRST arg is NULL (the old code inspected only `a`, so those warnings never
    // armed). Probe both, first match wins; the matcher rejects anything that isn't advisory Data.
    if !try_arm_warning_confirm(a) {
        try_arm_warning_confirm(b);
    }
    // Skill-learning confirm/result can also arrive on either slot of the 2-arg push — probe both.
    if !crate::skip::skill::try_arm_on_push(a) {
        crate::skip::skill::try_arm_on_push(b);
    }
    hyper_safety_on_dialog();
    if rr_should_advance() && WINDOW.is_open() && !in_overseer() {
        auto_close(rv);
    }
    rv
}

// ── [rrprobe] hyper-native Phase-0 (diagnostics only) ───────────────────────────────────────────
// The plan is to replace the synthetic result-screen presses with the game's OWN result skip
// (RaceResultSceneUI.OnPushSkip / Skip — plus RaceMainViewController / RaceManager natives). Those
// names come from a since-removed boot probe recorded only in memory, and a resolved method is NOT
// proof it fires (il2cpp resolution walks base classes; the Gacha BeginView lesson). So Phase 0
// re-verifies at runtime: dump the candidate classes' method inventories, and trace the two Skip
// entry points log-and-passthrough. One skipped + one watched race then settles (a) when a live
// instance exists, (b) whether the game itself gates Skip until the reveal, (c) whether pressing
// it pushes a confirm dialog. NOTHING is invoked here — pure observation.
macro_rules! rrtrace {
    ($fname:ident, $slot:ident, $label:literal) => {
        unsafe extern "C" fn $fname(this: *mut c_void, m: *mut c_void) -> *mut c_void {
            rr_log(concat!("[rrprobe] RaceResultSceneUI.", $label, " ENTERED"));
            let t = $slot.load(Ordering::Relaxed);
            if t != 0 {
                // Forward the return value too (Skip may not be void) — pure passthrough.
                let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
                    std::mem::transmute(t);
                f(this, m)
            } else {
                std::ptr::null_mut()
            }
        }
    };
}
rrtrace!(on_rr_push_skip, TR_RRSKIP0, "OnPushSkip");
rrtrace!(on_rr_skip, TR_RRSKIP1, "Skip");

/// [rrprobe] boot probe: method inventory of every hyper-native candidate class
/// (loc_settext::probe_text_entry_points is the template) + the two passthrough traces above.
/// Diagnostics only — deleted once Phase 1 wires the confirmed names (probe-then-remove workflow).
fn probe_result_skip() {
    // Verbose-only: this dumps four full class method inventories on EVERY launch. It answers a
    // wiring question (are the names still right?), which matters when something breaks and is pure
    // noise otherwise — 125 lines per session in the field logs.
    if !crate::tools::log::trace_enabled() {
        return;
    }
    for cls_name in [
        "Gallop.RaceResultSceneUI",
        "Gallop.RaceMainViewController",
        "Gallop.RaceManager",
        "Gallop.SingleModeRaceResultViewController",
    ] {
        let k = il2cpp::class(cls_name);
        if k.is_null() {
            rr_log(&format!("[rrprobe] {cls_name}: not found"));
            continue;
        }
        // The list is long — log it whole (one line per class per boot): the wiring pass needs
        // the exact names + arities ("name/argc" pairs, own methods only — no base walk).
        rr_log(&format!("[rrprobe] {cls_name}: {}", il2cpp::class_methods(k).join(" ")));
    }
    // gtrace-style ENTERED traces on the result scene's own skip handlers, if they resolve at
    // arity 0 (the arity the removed probe recorded). A miss is only logged — behavior falls back
    // to today's proven press path, which these traces never touch anyway (passthrough).
    let ui = il2cpp::class("Gallop.RaceResultSceneUI");
    if !ui.is_null() {
        let mut armed: Vec<&str> = Vec::new();
        unsafe {
            if install_one(ui, "OnPushSkip", 0, on_rr_push_skip as *const (), &TR_RRSKIP0, &D_RRSKIP0).is_ok() {
                armed.push("OnPushSkip");
            }
            if install_one(ui, "Skip", 0, on_rr_skip as *const (), &TR_RRSKIP1, &D_RRSKIP1).is_ok() {
                armed.push("Skip");
            }
        }
        let list = if armed.is_empty() { "(none)".to_string() } else { armed.join(" ") };
        rr_log(&format!(
            "[rrprobe] RaceResultSceneUI trace armed on: {list} — armed != fires; watch for ENTERED"
        ));
    }
}

/// Install race-result auto-advance (faithful port of native_skip.js part 3).
/// Independent of training/events. Returns Ok(note) describing what resolved.
pub(crate) fn install_race_result() -> Result<String, String> {
    use crate::ui_input::{
        C_PED, CLOSE_C, CLOSE_MI, CTOR_C, CTOR_MI, CUR_C, CUR_MI, GETNAME_C, GETNAME_MI, ISLOCK_C,
        ISLOCK_MI, OPC_C, OPC_MI,
    };
    use std::sync::atomic::AtomicUsize;
    let mut note = String::new();
    let btn = il2cpp::class("Gallop.ButtonCommon");
    if btn.is_null() {
        return Err("anchor miss".into());
    }
    let cvm = il2cpp::class("Gallop.SingleModeChangeViewManager");
    if cvm.is_null() {
        return Err("view-mgr miss".into());
    }
    let dm = il2cpp::class("Gallop.DialogManager");
    let dc = il2cpp::class("Gallop.DialogCommon");
    let obj = il2cpp::class("UnityEngine.Object");
    let es = il2cpp::class("UnityEngine.EventSystems.EventSystem");
    let ped = il2cpp::class("UnityEngine.EventSystems.PointerEventData");

    // Resolve (code, MethodInfo) pairs.
    let resolve = |cs: &AtomicUsize, ms: &AtomicUsize, k: il2cpp::Class, n: &str, argc: i32| -> bool {
        if k.is_null() {
            return false;
        }
        let m = il2cpp::method(k, n, argc);
        if m.is_null() {
            return false;
        }
        cs.store(il2cpp::method_pointer(m) as usize, Ordering::Relaxed);
        ms.store(m as usize, Ordering::Relaxed);
        true
    };
    if !resolve(&GETNAME_C, &GETNAME_MI, obj, "get_name", 0) {
        note.push_str("name miss; ");
    }
    if !resolve(&OPC_C, &OPC_MI, btn, "OnPointerClick", 1) {
        return Err("click miss".into());
    }
    resolve(&ISLOCK_C, &ISLOCK_MI, btn, "IsLock", 0);
    if !dc.is_null() {
        resolve(&CLOSE_C, &CLOSE_MI, dc, "Close", 0);
    }
    resolve(&CUR_C, &CUR_MI, es, "get_current", 0);
    if !ped.is_null() {
        C_PED.store(ped as usize, Ordering::Relaxed);
        resolve(&CTOR_C, &CTOR_MI, ped, ".ctor", 1);
    }
    if CTOR_C.load(Ordering::Relaxed) == 0 {
        note.push_str("evt ctor miss; ");
    }

    unsafe {
        install_one(btn, "Update", 0, on_button_update as *const (), &TR_UPDATE, &D_UPDATE)?;
        install_one(btn, "OnPointerClick", 1, on_pointer_click as *const (), &TR_ONPC, &D_ONPC)?;
        install_one(cvm, "ChangeMainView", 0, on_change_main_view as *const (), &TR_CMV, &D_CMV)?;
        // Home (lobby) hard-clear — non-fatal: if it misses, the 6-min heartbeat window still bounds it.
        let home = il2cpp::class("Gallop.HomeViewController");
        if home.is_null() {
            note.push_str("home view miss (heartbeat-only gate); ");
        } else if install_one(home, "PlayInView", 0, on_home_in as *const (), &TR_HOME, &D_HOME).is_err() {
            note.push_str("home hook miss; ");
        } else {
            // PlayOutView (leaving the lobby) → resume full UI tempo off the main menu. Non-fatal: if it
            // misses, the flag still clears when a career's ChangeMainView fires (lobby stays 1x until then).
            let _ = install_one(home, "PlayOutView", 0, on_home_out as *const (), &TR_HOMEOUT, &D_HOMEOUT);
        }
        // Scout/Gacha view → 1x tempo pin (theory: its one-shot intro tweens are synced to real-time
        // banner movies, and outrunning them soft-locks the screen).
        //
        // READ THIS BEFORE TOUCHING THIS BLOCK. The pin has NEVER ONCE ENGAGED. Measured over a 1.3MB
        // log spanning 31 boots: "gacha 1x pin armed" 31 times, on_gacha_begin's line 0 times — while
        // rr_log from this same file (on_home_in) fired 120 times, so the logging is fine. BeginView
        // simply is not called on the Scout path, and InitializeView is unknown because it had no log
        // line at all. Consequence: Scout has always run at full tempo and the soft-lock theory above
        // has never actually been tested.
        //
        // Two traps that already burned us, so don't repeat them:
        //  * il2cpp::class(...) non-null proves the class EXISTS in metadata — NOT that it is the
        //    controller Scout pushes. Same for il2cpp::method: it wraps class_get_method_from_name,
        //    which WALKS BASE CLASSES, so "BeginView resolved" may be ViewControllerBase's.
        //  * The older PlayInView hook LOOKED like it worked (its "opened" line appeared 21 times) but
        //    it was exactly that base-class blanket: 65 closes vs 21 opens, closes firing before any
        //    open. It pinned unrelated screens to 1x. It is NOT a working detector to revert to.
        //
        // So: stop guessing the entry point from an existence dump. GTRACE below detours every 0-arg
        // lifecycle method on the class and logs which one actually runs — one Scout open settles it.
        {
            let gk = il2cpp::class("Gallop.GachaMainViewController");
            if gk.is_null() {
                note.push_str("gacha view miss (no scout 1x pin); ");
            } else {
                let a = install_one(gk, "BeginView", 0, on_gacha_begin as *const (), &TR_GACHA, &D_GACHA).is_ok();
                let b = install_one(gk, "InitializeView", 0, on_gacha_init as *const (), &TR_GACHAINIT, &D_GACHAINIT).is_ok();
                if a || b {
                    rr_log(&format!("[view] gacha 1x pin armed (begin={a} init={b}) — confirmed firing in the field logs"));
                } else {
                    note.push_str("gacha 1x pin hook miss; ");
                }
            }
        }
        if dm.is_null() {
            note.push_str("dialog-mgr miss (no auto-close); ");
        } else {
            if install_one(dm, "PushDialog", 1, on_push1 as *const (), &TR_PUSH1, &D_PUSH1).is_err() {
                note.push_str("push1 miss; ");
            }
            if install_one(dm, "PushDialogSequence", 2, on_push2 as *const (), &TR_PUSH2, &D_PUSH2).is_err() {
                note.push_str("push2 miss; ");
            }
        }
        // Auto Fast-Forward: capture the in-race FF button + resolve its press/visibility methods.
        {
            let ff = il2cpp::class("Gallop.RaceUIFastForward");
            if ff.is_null() {
                note.push_str("race FF class miss; ");
            } else {
                // SetVisible(bool) is the PRIMARY capture (Awake alone never landed the press).
                let sv = install_one(ff, "SetVisible", 1, on_ff_setvisible as *const (), &TR_FFVIS, &D_FFVIS).is_ok();
                let aw = install_one(ff, "Awake", 0, on_ff_awake as *const (), &TR_FFAWAKE, &D_FFAWAKE).is_ok();
                let push = il2cpp::method(ff, "OnPushFastForward", 0);
                let vis = il2cpp::method(ff, "IsVisible", 0);
                if !push.is_null() {
                    FF_PUSH.store(il2cpp::method_pointer(push) as usize, Ordering::Relaxed);
                    FF_PUSH_MI.store(push as usize, Ordering::Relaxed);
                }
                if !vis.is_null() {
                    FF_VIS.store(il2cpp::method_pointer(vis) as usize, Ordering::Relaxed);
                    FF_VIS_MI.store(vis as usize, Ordering::Relaxed);
                }
                rr_log(&format!(
                    "[race] auto Fast-Forward wired (setvisible={sv} awake={aw} push={} vis={})",
                    !push.is_null(),
                    !vis.is_null()
                ));
                if push.is_null() {
                    note.push_str("race FF push miss; ");
                }
                if !sv {
                    note.push_str("race FF SetVisible hook miss; ");
                }
            }
        }

        // Auto-pick top event choice: capture the StoryChoiceController + resolve its real selection
        // API (ChoiceButton(int) + get_IsWaitSelect). The GameObject name is "StoryViewChoiceButton01"
        // but selection is NOT ButtonCommon.OnPointerClick — it's this controller (probe-verified).
        {
            let scc = il2cpp::class("Gallop.StoryChoiceController");
            if scc.is_null() {
                note.push_str("choice controller miss (no auto event-choice); ");
            } else {
                let _ = install_one(scc, "SetupOpenChoiceButton", 0, on_choice_setup as *const (), &TR_CHOPEN, &D_CHOPEN);
                let iw = il2cpp::method(scc, "get_IsWaitSelect", 0);
                let cb = il2cpp::method(scc, "ChoiceButton", 1);
                if !iw.is_null() {
                    CHOICE_ISWAIT.store(il2cpp::method_pointer(iw) as usize, Ordering::Relaxed);
                    CHOICE_ISWAIT_MI.store(iw as usize, Ordering::Relaxed);
                }
                if !cb.is_null() {
                    CHOICE_SELECT.store(il2cpp::method_pointer(cb) as usize, Ordering::Relaxed);
                    CHOICE_SELECT_MI.store(cb as usize, Ordering::Relaxed);
                }
                rr_log(&format!(
                    "[view] auto event-choice wired (iswait={} select={})",
                    !iw.is_null(),
                    !cb.is_null()
                ));
                if iw.is_null() || cb.is_null() {
                    note.push_str("choice select/iswait miss; ");
                }
            }
        }

        // Native result-scene skip (hyper-native Phase 1): capture RaceResultSceneUI (Setup /
        // PlayFinishOrder) + resolve its OWN skip handler (OnPushSkip preferred, Skip fallback). Only
        // resolves method pointers here — the [rrprobe] passthrough traces below still own the
        // OnPushSkip/Skip detours (invoking the resolved pointer runs through them, so a native skip
        // also logs "[rrprobe] ... ENTERED", confirming the invoke landed). Layered ON TOP of the
        // press engine, so a miss degrades to today's proven flow.
        {
            let rui = il2cpp::class("Gallop.RaceResultSceneUI");
            if rui.is_null() {
                note.push_str("race-result scene class miss (press-only); ");
            } else {
                let setup =
                    install_one(rui, "Setup", 0, on_rr_setup as *const (), &TR_RRSETUP, &D_RRSETUP).is_ok();
                let finish = install_one(rui, "PlayFinishOrder", 0, on_rr_finish as *const (), &TR_RRFINISH, &D_RRFINISH)
                    .is_ok();
                let sk = il2cpp::method(rui, "OnPushSkip", 0);
                let sk_fb = il2cpp::method(rui, "Skip", 0);
                if !sk.is_null() {
                    RES_SKIP.store(il2cpp::method_pointer(sk) as usize, Ordering::Relaxed);
                    RES_SKIP_MI.store(sk as usize, Ordering::Relaxed);
                }
                if !sk_fb.is_null() {
                    RES_SKIP_FB.store(il2cpp::method_pointer(sk_fb) as usize, Ordering::Relaxed);
                    RES_SKIP_FB_MI.store(sk_fb as usize, Ordering::Relaxed);
                }
                rr_log(&format!(
                    "[race-result] native skip wired (setup={setup} finish={finish} onpushskip={} skip={})",
                    !sk.is_null(),
                    !sk_fb.is_null()
                ));
                if sk.is_null() && sk_fb.is_null() {
                    note.push_str("race-result skip method miss (press-only); ");
                }
                if !setup && !finish {
                    note.push_str("race-result capture hook miss (press-only); ");
                }
            }
        }
    }
    // PHASE-0 hyper-native re-verification (diagnostics only; see probe_result_skip above).
    probe_result_skip();
    Ok(note)
}
