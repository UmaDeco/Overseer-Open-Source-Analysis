//! Skill-learning auto-confirm + native cutscene/result skip (`skip_skill_learn`, default OFF).
//!
//! A live discovery session CONFIRMED the fingerprints this module wires against:
//!   • View controller  `Gallop.SingleModeSkillLearningViewController`
//!       BeginView/0, RegisterDownload/1, PlayOutView/0, OnClickBackButton/0, Setup/0, ...
//!   • Confirm dialog #1 ("learn these skills?"):  Title="Confirmation", FormType="BIG_TWO_BUTTON",
//!       LeftButtonText="Annuler", RightButtonText="Apprendre"   →  auto-OK ButtonRight.
//!   • Acquisition cutscene = a StoryViewController timeline (log: "[event] SkipStory suppressed
//!       (dialog open)" while the confirm closes, then "SkipStory()" once it's gone).
//!   • Result popup #2:  Title="Compétences acquises", FormType="SMALL_ONE_BUTTON",
//!       CenterButtonText="Fermer"  →  one-button dialog, GameObject "ButtonCenter"  →  auto-Close.
//!   • Lobby return:  invoke OnClickBackButton on the captured VC.
//!
//! MONEY-SAFETY (structural, layered): [1] positive view gate — nothing arms outside the career
//! skill-learning view (`SKILL_VIEW_ACTIVE`), so shop/gacha/carat contexts can never trip it;
//! [2] the confirm auto-OK matches the RightButtonText against a LEARN-verb set AND hard-rejects any
//! purchase verb (the shop dialog is Title="Boutique" R="Boutique" — never matches); learning spends
//! skill POINTS, never carats; [3] every window is a wall-clock ONE-SHOT re-verified against the
//! rooted, still-frontmost dialog Data — worst case is "the player taps one thing by hand", never a
//! soft-lock and never a spend. Every auto-click uses the game's own OnPointerClick path (`click_now`)
//! so callbacks + AutoClose run exactly as a manual tap. Every cached instance is a strong GCHandle
//! re-fetched with `target()` and cleared on `unity_object_destroyed` (the crate's #1 crash class).

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::hooks::{in_overseer, ReentryGuard};
use crate::htt_il2cpp as h;
use crate::il2cpp;
use crate::skip::{call_orig, install_one, mark_career, now_ms, resolve, rr_log, Invokable};

// ── view-active gate ─────────────────────────────────────────────────────────────────────────────
// Set true while the career skill-learning view is up (BeginView/RegisterDownload); cleared on
// PlayOutView, ChangeMainView (lobby reached), HomeViewController.PlayInView, and a hard 10-min cap.
// Every auto-action requires this true, so nothing here can ever fire in another screen's context.
static SKILL_VIEW_ACTIVE: AtomicBool = AtomicBool::new(false);
static SKILL_VIEW_MS: AtomicU64 = AtomicU64::new(0);
const SKILL_VIEW_CAP_MS: u64 = 600_000; // 10-min hard cap so the gate can never stick

// The live SingleModeSkillLearningViewController, rooted via a STRONG GCHandle (mirrors STORY_CTRL_H /
// FF_UI_H): the GC can't see Rust statics, so a raw pointer would dangle once the view tears down.
// Used to invoke OnClickBackButton for the lobby return. Re-fetched with target(), cleared on Destroy.
static SKILL_VC_H: Mutex<Option<il2cpp::GCHandle>> = Mutex::new(None);
static SKILL_BACK: OnceLock<Invokable> = OnceLock::new(); // OnClickBackButton/0

// ── one-shot wall-clock windows (all cleared by clear_all) ─────────────────────────────────────────
static SKILL_CONFIRM_UNTIL: AtomicU64 = AtomicU64::new(0); // auto-OK the confirm (ButtonRight)
static SKILL_FLOW_UNTIL: AtomicU64 = AtomicU64::new(0); // post-confirm phase (cutscene skip + result arm)
static SKILL_RESULT_UNTIL: AtomicU64 = AtomicU64::new(0); // auto-Close the result popup (ButtonCenter)
static SKILL_BACK_UNTIL: AtomicU64 = AtomicU64::new(0); // lobby-return (OnClickBackButton / BackButton)
static SKILL_STORY_LAST_MS: AtomicU64 = AtomicU64::new(0); // throttle the cutscene-skip retry
/// A deferred click is queued and hasn't reported back yet. Stops the pump queueing a second click
/// for the same dialog on the next frame, WITHOUT throwing away the one-shot window — so if the
/// click's re-verify fails (dialog still animating in, button not yet interactable) the window is
/// still armed and the next frame tries again. Losing the window on a failed attempt is why the
/// confirm sometimes never landed at all.
static SKILL_CLICK_INFLIGHT_MS: AtomicU64 = AtomicU64::new(0);
const SKILL_INFLIGHT_MS: u64 = 250;

const SKILL_CONFIRM_WINDOW_MS: u64 = 6000; // matches the warn-confirm window (rides out a slow input-lock)
const SKILL_FLOW_WINDOW_MS: u64 = 20_000; // cutscene + result-popup phase after the confirm OK lands
const SKILL_RESULT_WINDOW_MS: u64 = 6000;
const SKILL_BACK_WINDOW_MS: u64 = 5000;
const SKILL_STORY_THROTTLE_MS: u64 = 120;

// The matched confirm / result Data, rooted so the click-time re-verify reads LIVE memory (same fix as
// WARN_DATA_H): re-verify keeps the click bound to the still-frontmost matched dialog.
static SKILL_CONFIRM_DATA_H: Mutex<Option<il2cpp::GCHandle>> = Mutex::new(None);
static SKILL_RESULT_DATA_H: Mutex<Option<il2cpp::GCHandle>> = Mutex::new(None);

// ── verb sets (read AFTER Overseer's own localisation, so cover the shipped locales) ───────────────
// The confirm's RIGHT button ("learn") — spends skill POINTS. Broaden as locales are ever missed.
const LEARN_VERBS: &[&str] = &[
    "Apprendre", "Learn", "習得", "習得する", "学ぶ", "覚える", "Aprender", "Lernen", "Impara",
    "Imparare", "배우기", "익히기", "学习", "學習",
];
// HARD-REJECT set: any purchase/exchange verb on EITHER button vetoes the auto-click (defence in depth
// vs the shop's Title="Boutique" R="Boutique" dialog and any carat spend).
const PURCHASE_VERBS: &[&str] = &[
    "Boutique", "Buy", "Purchase", "Acheter", "Kaufen", "Comprar", "Compra", "Acquista", "Acquistare",
    "購入", "交換", "구매", "교환", "购买", "購買", "兑换", "Exchange", "Échanger", "Cambiar", "Tauschen",
];

fn is_verb(s: &str, set: &[&str]) -> bool {
    let t = s.trim();
    // eq_ignore_ascii_case folds ONLY ASCII letters (latin case variants) and is exact for CJK bytes.
    !t.is_empty() && set.iter().any(|&v| t.eq_ignore_ascii_case(v))
}

/// Read up to `names` string fields off a DialogCommon.Data in ONE class walk. Same plausibility gate
/// as `result::warning_data_matches` (the 2-arg push path can hand us a by-value enum/bool slot that
/// would be a wild pointer). Returns None if the object isn't a readable managed ref.
fn read_data_fields(data: *mut c_void, names: &[&str]) -> Option<Vec<String>> {
    if data.is_null() || (data as usize) < 0x10000 || (data as usize) & 0x7 != 0 {
        return None;
    }
    let v = match unsafe { h::val_of(data as *mut h::RawObject) } {
        Some(v) => v,
        None => return None,
    };
    let mut out = Vec::with_capacity(names.len());
    for n in names {
        // Type-checked (see htt_il2cpp::read_string_field): reading a non-string field as a string
        // is an access violation, and the field names here give no hint which is which.
        out.push(unsafe { h::read_string_field(&v, n).unwrap_or_default() });
    }
    Some(out)
}

/// True iff `data` is the skill-learning CONFIRM dialog (#1). STRICT so it can never fire on a spend:
/// requires SKILL_VIEW_ACTIVE, RightButtonText ∈ LEARN verbs, a non-empty LeftButtonText (the two-
/// button Cancel — never a one-button spend), and HARD-rejects any purchase verb on either button.
fn confirm_data_matches(data: *mut c_void) -> bool {
    if !SKILL_VIEW_ACTIVE.load(Ordering::Relaxed) {
        return false;
    }
    let f = match read_data_fields(data, &["RightButtonText", "LeftButtonText"]) {
        Some(f) => f,
        None => return false,
    };
    let (right, left) = (f[0].trim(), f[1].trim());
    is_verb(right, LEARN_VERBS)
        && !left.is_empty()
        && !is_verb(right, PURCHASE_VERBS)
        && !is_verb(left, PURCHASE_VERBS)
}

/// True iff `data` is the "skills acquired" RESULT popup (#2): a one-button form — detected purely
/// from the button-text fields (non-empty Center, empty Left AND Right) — whose Center text isn't a
/// purchase verb. Requires SKILL_VIEW_ACTIVE; the arm site additionally gates it to the post-confirm
/// flow window, so this only ever collapses the result popup — clicking a one-button Close spends
/// nothing regardless.
/// NB: do NOT read `FormType` here — it is an il2cpp ENUM (i32), not a managed String, so reading it
/// through `read_string` dereferences the enum value as a pointer (the "read at 0x11" crash on the
/// confirm path). Only ever read genuine System.String fields via read_data_fields.
fn result_data_matches(data: *mut c_void) -> bool {
    if !SKILL_VIEW_ACTIVE.load(Ordering::Relaxed) {
        return false;
    }
    let f = match read_data_fields(data, &["CenterButtonText", "LeftButtonText", "RightButtonText"]) {
        Some(f) => f,
        None => return false,
    };
    let (center, left, right) = (f[0].trim(), f[1].trim(), f[2].trim());
    let one_button = left.is_empty() && right.is_empty();
    !center.is_empty() && one_button && !is_verb(center, PURCHASE_VERBS)
}

// ── arm on dialog push (called from result.rs on_push1/on_push2) ───────────────────────────────────
/// If `data` is the skill confirm (#1) or result popup (#2), arm the matching one-shot window and root
/// the Data for the click-time re-verify. Returns true if it armed (so the 2-arg push can stop probing
/// the other slot). Gated on the feature toggle + the positive view gate.
pub(crate) fn try_arm_on_push(data: *mut c_void) -> bool {
    if !crate::skip::is_skill_learn_enabled() || !SKILL_VIEW_ACTIVE.load(Ordering::Relaxed) {
        return false;
    }
    if confirm_data_matches(data) {
        if let Ok(mut g) = SKILL_CONFIRM_DATA_H.lock() {
            *g = Some(il2cpp::GCHandle::new(data, false));
        }
        SKILL_CONFIRM_UNTIL.store(now_ms() + SKILL_CONFIRM_WINDOW_MS, Ordering::Relaxed);
        mark_career(); // refresh the career heartbeat (a career-only view just pushed this)
        rr_log("[skill] confirm dialog matched (Learn/Cancel) -> auto-OK (ButtonRight) armed");
        return true;
    }
    // Result popup only arms during the post-confirm flow window (so a stray one-button dialog on the
    // skill screen BEFORE a learn confirm can't be auto-closed).
    if now_ms() < SKILL_FLOW_UNTIL.load(Ordering::Relaxed) && result_data_matches(data) {
        if let Ok(mut g) = SKILL_RESULT_DATA_H.lock() {
            *g = Some(il2cpp::GCHandle::new(data, false));
        }
        SKILL_RESULT_UNTIL.store(now_ms() + SKILL_RESULT_WINDOW_MS, Ordering::Relaxed);
        rr_log("[skill] result popup matched (one-button Close) -> auto-close (ButtonCenter) armed");
        return true;
    }
    false
}

/// The matched CONFIRM Data must still read as the learn confirm AND still be the frontmost dialog, so
/// we can never OK a different dialog's ButtonRight after the original closed (mirrors warn-confirm).
fn confirm_target_ok() -> bool {
    let data = match SKILL_CONFIRM_DATA_H.lock() {
        Ok(g) => g.as_ref().map_or(std::ptr::null_mut(), |h| h.target()),
        Err(_) => std::ptr::null_mut(),
    };
    !data.is_null() && confirm_data_matches(data) && crate::skip::result::data_still_frontmost(data)
}

fn result_target_ok() -> bool {
    let data = match SKILL_RESULT_DATA_H.lock() {
        Ok(g) => g.as_ref().map_or(std::ptr::null_mut(), |h| h.target()),
        Err(_) => std::ptr::null_mut(),
    };
    !data.is_null() && result_data_matches(data) && crate::skip::result::data_still_frontmost(data)
}

// ── per-frame pump (called from result.rs on_button_update, once per ButtonCommon this frame) ───────
/// Drive every armed skill-learning auto-action. `this` is one button being updated this frame; the
/// dialog auto-clicks match on its GameObject name exactly like the warn-confirm pump, while the
/// cutscene-skip retry and the lobby-return invoke ignore `this` (they act on captured instances).
/// Does this module currently want a button NAME resolved? Lets the caller skip the (mutex-taking)
/// name lookup entirely when no skill window is armed — the overwhelmingly common case.
pub(crate) fn wants_button() -> bool {
    if !crate::skip::is_skill_learn_enabled() {
        return false;
    }
    let now = now_ms();
    now < SKILL_CONFIRM_UNTIL.load(Ordering::Relaxed)
        || now < SKILL_RESULT_UNTIL.load(Ordering::Relaxed)
        || now < SKILL_BACK_UNTIL.load(Ordering::Relaxed)
}

/// `name` is this button's GameObject name, already resolved by the caller (None = not resolved,
/// because no window that needs it was armed).
pub(crate) unsafe fn pump(this: *mut c_void, name: Option<&str>) {
    if in_overseer() {
        return;
    }
    // Hard cap: a view-active gate older than 10 min is stale (e.g. the player wandered off) — clear it
    // so nothing armed can outlive the skill screen. Runs regardless of the toggle.
    if SKILL_VIEW_ACTIVE.load(Ordering::Relaxed)
        && now_ms().saturating_sub(SKILL_VIEW_MS.load(Ordering::Relaxed)) > SKILL_VIEW_CAP_MS
    {
        clear_all("expiry");
    }
    if !crate::skip::is_skill_learn_enabled() {
        return;
    }
    let now = now_ms();

    // (1) Confirm dialog auto-OK — click "Apprendre" (ButtonRight) via the game's own click path.
    //
    // DEFERRED, unlike the other auto-clicks: this button's handler tears down a dialog stack AND
    // fires a network request, and clicking it from inside `ButtonCommon.Update` (i.e. re-entering
    // Unity's EventSystem mid-iteration) hard-froze the game in half the attempts recorded in the
    // field log — the whole process, not just the skip. `click_deferred` runs it on the next
    // main-thread pump instead: still the main thread, but a clean stack outside the UI update.
    // The closure re-verifies the dialog at click time, so a frame of latency costs nothing.
    if now < SKILL_CONFIRM_UNTIL.load(Ordering::Relaxed)
        && name == Some("ButtonRight")
        && confirm_target_ok()
        && now.saturating_sub(SKILL_CLICK_INFLIGHT_MS.load(Ordering::Relaxed)) >= SKILL_INFLIGHT_MS
    {
        SKILL_CLICK_INFLIGHT_MS.store(now, Ordering::Relaxed);
        crate::ui_input::click_deferred(this, confirm_target_ok, |clicked| {
            if !clicked {
                return; // window still armed — the next frame retries
            }
            SKILL_CONFIRM_UNTIL.store(0, Ordering::Relaxed);
            if let Ok(mut g) = SKILL_CONFIRM_DATA_H.lock() {
                *g = None;
            }
            // The learn request fires the moment OK is pressed; open the post-confirm phase
            // (cutscene skip retry + result-popup arm eligibility) and start expecting progress.
            SKILL_FLOW_UNTIL.store(now_ms() + SKILL_FLOW_WINDOW_MS, Ordering::Relaxed);
            crate::skip::SKILL_PROGRESS.expect();
            rr_log("[skill] auto-clicked Learn (ButtonRight) — spends the SP the player selected");
        });
    }

    // (2) Acquisition cutscene — RETRY the guarded SkipStory once the confirm dialog is gone. The
    //     one-shot on_start_timeline skip is suppressed by dialog_open() at that instant, so we retry
    //     through event.rs's shared core (keeps ALL its guards — never skips under a live dialog).
    if now < SKILL_FLOW_UNTIL.load(Ordering::Relaxed)
        && crate::skip::event::story_playing()
        && now.saturating_sub(SKILL_STORY_LAST_MS.load(Ordering::Relaxed)) >= SKILL_STORY_THROTTLE_MS
    {
        SKILL_STORY_LAST_MS.store(now, Ordering::Relaxed);
        if crate::skip::event::retry_captured_skip() {
            rr_log("[skill] acquisition cutscene skipped (SkipStory retry through the guarded core)");
        }
    }

    // (3) Result popup auto-Close — click "Fermer" (ButtonCenter). One-button dialog → GameObject is
    //     "ButtonCenter" (two-button uses ButtonLeft/ButtonRight). Deferred for the same reason as
    //     the confirm above: this closes a dialog, and closing a dialog from inside the UI update is
    //     the exact operation that deadlocks DialogManager.
    if now < SKILL_RESULT_UNTIL.load(Ordering::Relaxed)
        && name == Some("ButtonCenter")
        && result_target_ok()
        && now.saturating_sub(SKILL_CLICK_INFLIGHT_MS.load(Ordering::Relaxed)) >= SKILL_INFLIGHT_MS
    {
        SKILL_CLICK_INFLIGHT_MS.store(now, Ordering::Relaxed);
        crate::ui_input::click_deferred(this, result_target_ok, |clicked| {
            if !clicked {
                return; // still armed — retry next frame
            }
            SKILL_RESULT_UNTIL.store(0, Ordering::Relaxed);
            if let Ok(mut g) = SKILL_RESULT_DATA_H.lock() {
                *g = None;
            }
            SKILL_BACK_UNTIL.store(now_ms() + SKILL_BACK_WINDOW_MS, Ordering::Relaxed);
            // Reaching the result popup IS progress — re-arm the expectation for the lobby return.
            crate::skip::SKILL_PROGRESS.expect();
            rr_log("[skill] auto-closed result popup (ButtonCenter) — lobby-return armed");
        });
    }

    // (4) Lobby return — invoke OnClickBackButton on the captured VC (preferred: the game's own back
    //     handler), falling back to a windowed exact-name "BackButton" press if the invoke can't run.
    if now < SKILL_BACK_UNTIL.load(Ordering::Relaxed) {
        if invoke_back() {
            SKILL_BACK_UNTIL.store(0, Ordering::Relaxed);
            crate::skip::SKILL_PROGRESS.resolved(); // the flow completed
            rr_log("[skill] returned to lobby (OnClickBackButton invoked on captured VC)");
        } else if name == Some("BackButton") {
            let _g = ReentryGuard::enter();
            if crate::ui_input::click_now(this) {
                SKILL_BACK_UNTIL.store(0, Ordering::Relaxed);
                crate::skip::SKILL_PROGRESS.resolved();
                rr_log("[skill] returned to lobby (BackButton pressed — invoke fallback)");
            }
        }
    }
}

/// Watchdog recovery: the learn flow was started but never reached the lobby. Drop every armed
/// window and lift any input block so the player's own taps land — the flow is the one path where
/// Overseer has already spent the player's skill points, so leaving them on a locked screen is the
/// worst possible outcome.
pub(crate) fn abandon_flow(reason: &str) {
    SKILL_CONFIRM_UNTIL.store(0, Ordering::Relaxed);
    SKILL_RESULT_UNTIL.store(0, Ordering::Relaxed);
    SKILL_BACK_UNTIL.store(0, Ordering::Relaxed);
    SKILL_FLOW_UNTIL.store(0, Ordering::Relaxed);
    if let Ok(mut g) = SKILL_CONFIRM_DATA_H.lock() {
        *g = None;
    }
    if let Ok(mut g) = SKILL_RESULT_DATA_H.lock() {
        *g = None;
    }
    crate::skip::result::lift_input_block();
    rr_log(&format!("[skill] flow abandoned ({reason}) — control handed back to the player"));
}

/// Invoke OnClickBackButton on the live captured VC. GC-safe: re-fetch through the handle, clear on
/// Destroy. Returns true iff the invoke actually ran (the caller then disarms the back window).
unsafe fn invoke_back() -> bool {
    let inv = match SKILL_BACK.get() {
        Some(i) if i.ok() => *i,
        _ => return false,
    };
    let vc = match SKILL_VC_H.lock() {
        Ok(g) => g.as_ref().map_or(std::ptr::null_mut(), |h| h.target()),
        Err(_) => std::ptr::null_mut(),
    };
    if vc.is_null() {
        return false;
    }
    if il2cpp::unity_object_destroyed(vc) {
        if let Ok(mut g) = SKILL_VC_H.lock() {
            *g = None;
        }
        return false;
    }
    let _g = ReentryGuard::enter();
    inv.call_void(vc);
    true
}

// ── lifecycle: view-active gate + VC capture ───────────────────────────────────────────────────────
crate::skip_hook_slot!(TR_SKILL_BEGIN, D_SKILL_BEGIN); // BeginView/0 — primary capture
crate::skip_hook_slot!(TR_SKILL_REG, D_SKILL_REG); // RegisterDownload/1 — backup capture
crate::skip_hook_slot!(TR_SKILL_OUT, D_SKILL_OUT); // PlayOutView/0 — leaving the view

/// Set the view-active gate + (re)capture the live VC into the strong handle. Called from BeginView
/// (primary) and RegisterDownload (backup) — either fires the moment the skill screen comes up.
fn activate_view(this: *mut c_void) {
    SKILL_VIEW_ACTIVE.store(true, Ordering::Relaxed);
    SKILL_VIEW_MS.store(now_ms(), Ordering::Relaxed);
    if let Ok(mut g) = SKILL_VC_H.lock() {
        *g = Some(il2cpp::GCHandle::new(this, false)); // old handle freed on drop
    }
    mark_career(); // the skill-learning view is career-only → refresh the heartbeat
}

pub(crate) unsafe extern "C" fn on_skill_begin(this: *mut c_void, m: *mut c_void) {
    rr_log("[skill] BeginView ENTERED -> skill view active"); // hook-fires visibility (hard rule)
    activate_view(this);
    call_orig(&TR_SKILL_BEGIN, this, m);
}

// RegisterDownload(reg, MethodInfo*) — backup capture in case the BeginView resolve was a base-class
// blanket that never fires (the Gacha BeginView trap). Non-fatal, passthrough.
pub(crate) unsafe extern "C" fn on_skill_register(this: *mut c_void, reg: *mut c_void, m: *mut c_void) {
    rr_log("[skill] RegisterDownload ENTERED -> skill view active (backup)");
    activate_view(this);
    let t = TR_SKILL_REG.load(Ordering::Relaxed);
    if t != 0 {
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) = std::mem::transmute(t);
        f(this, reg, m);
    }
}

pub(crate) unsafe extern "C" fn on_skill_playout(this: *mut c_void, m: *mut c_void) {
    rr_log("[skill] PlayOutView ENTERED -> clearing skill windows");
    clear_all("PlayOutView");
    call_orig(&TR_SKILL_OUT, this, m);
}

/// Clear the view gate + every armed window + rooted handle. Called on PlayOutView, ChangeMainView
/// (lobby reached — from result.rs), HomeViewController.PlayInView (left career — from result.rs), and
/// the 10-min expiry. Logs only the actual disarm transition so the log isn't spammed.
pub(crate) fn clear_all(src: &str) {
    let was = SKILL_VIEW_ACTIVE.swap(false, Ordering::Relaxed)
        || SKILL_CONFIRM_UNTIL.load(Ordering::Relaxed) != 0
        || SKILL_FLOW_UNTIL.load(Ordering::Relaxed) != 0
        || SKILL_RESULT_UNTIL.load(Ordering::Relaxed) != 0
        || SKILL_BACK_UNTIL.load(Ordering::Relaxed) != 0;
    SKILL_CONFIRM_UNTIL.store(0, Ordering::Relaxed);
    SKILL_FLOW_UNTIL.store(0, Ordering::Relaxed);
    SKILL_RESULT_UNTIL.store(0, Ordering::Relaxed);
    SKILL_BACK_UNTIL.store(0, Ordering::Relaxed);
    if let Ok(mut g) = SKILL_VC_H.lock() {
        *g = None;
    }
    if let Ok(mut g) = SKILL_CONFIRM_DATA_H.lock() {
        *g = None;
    }
    if let Ok(mut g) = SKILL_RESULT_DATA_H.lock() {
        *g = None;
    }
    if was {
        rr_log(&format!("[skill] cleared ({src}) -> view gate + all skill windows disarmed"));
    }
}

// ── install ────────────────────────────────────────────────────────────────────────────────────────
/// Wire the skill-learning skip against the CONFIRMED view controller. Non-fatal: every miss is a
/// note, and the dialog-driven auto-clicks work even if a lifecycle hook silently misses (a missed
/// view gate just means nothing arms — the safe direction). Also runs the discovery probe so a game
/// update that renames these methods is visible in the boot log (method-drift is an institutional
/// lesson).
pub(crate) fn install(notes: &mut String) {
    probe(notes);
    let vc = il2cpp::class("Gallop.SingleModeSkillLearningViewController");
    if vc.is_null() {
        notes.push_str("skill vc class miss (no skill-learn skip); ");
        return;
    }
    // Resolve OnClickBackButton for the lobby-return invoke (the game's own back handler).
    let _ = SKILL_BACK.set(resolve(vc, "OnClickBackButton", 0));
    if !SKILL_BACK.get().map(|i| i.ok()).unwrap_or(false) {
        notes.push_str("skill OnClickBackButton miss (BackButton-press fallback only); ");
    }
    unsafe {
        if let Err(e) = install_one(vc, "BeginView", 0, on_skill_begin as *const (), &TR_SKILL_BEGIN, &D_SKILL_BEGIN) {
            notes.push_str(&format!("skill BeginView: {e}; "));
        }
        // RegisterDownload(1) backup capture — non-fatal.
        let _ = install_one(vc, "RegisterDownload", 1, on_skill_register as *const (), &TR_SKILL_REG, &D_SKILL_REG);
        if let Err(e) = install_one(vc, "PlayOutView", 0, on_skill_playout as *const (), &TR_SKILL_OUT, &D_SKILL_OUT) {
            notes.push_str(&format!("skill PlayOutView: {e}; "));
        }
    }
    rr_log(&format!(
        "[skill] skill-learning skip wired (BeginView + RegisterDownload capture, PlayOutView clear, back={})",
        SKILL_BACK.get().map(|i| i.ok()).unwrap_or(false)
    ));
}

/// Boot probe: log each candidate class's OWN method inventory as one [skill-probe] line
/// (class_methods "name/argc" format — loc_settext::probe_text_entry_points is the template). Kept
/// after Phase-1 wiring so a game update that renames the skill-learning methods surfaces in the log.
pub(crate) fn probe(notes: &mut String) {
    let mut hits = 0usize;
    for cls_name in [
        // The likeliest controller names (the SingleMode* family every career view uses)…
        "Gallop.SingleModeSkillLearningViewController",
        "Gallop.SkillLearningViewController",
        "Gallop.SingleModeSkillLearningView",
        // …and the Parts* widget variants the plan flags (list / row / confirm panels).
        "Gallop.PartsSingleModeSkillLearning",
        "Gallop.PartsSingleModeSkillLearningList",
        "Gallop.PartsSingleModeSkillLearningListItem",
        "Gallop.PartsSingleModeSkillLearningConfirm",
    ] {
        let k = il2cpp::class(cls_name);
        if k.is_null() {
            rr_log(&format!("[skill-probe] {cls_name}: not found"));
            continue;
        }
        hits += 1;
        // The method list may be long — log it whole: confirms the wired names + arities still exist.
        rr_log(&format!("[skill-probe] {cls_name}: {}", il2cpp::class_methods(k).join(" ")));
    }
    if hits == 0 {
        notes.push_str("skill-probe: every candidate class miss; ");
    }
}
