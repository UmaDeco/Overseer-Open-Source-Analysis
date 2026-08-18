//! Broad, instant translation via the game's TEXT SETTERS — the coverage engine ported from
//! Overseer's `translation_agent.js` (`hookSetter` on TMP_Text / UI.Text / TextCommon /
//! TextMeshProUguiCommon).
//!
//! `Gallop.Localize::Get` (loc_ui.rs) only covers the game's *static* localized UI keys — a small
//! slice of on-screen text. The overwhelming majority of strings (dynamic labels, result screens,
//! menus, tooltips, story-adjacent UI) are pushed straight into a text component via its `set_text`
//! property setter, never touching Localize. Overseer got its broad, ZERO-delay coverage by hooking
//! those setters and swapping the incoming `System.String`; we do the same, in-process.
//!
//! ABI: `set_text` is an INSTANCE property setter — `void set_text(this, System.String value,
//! MethodInfo*)`, arity 1. We detour each of the four component types, read the argument string,
//! run it through the active-language glossary, and call the ORIGINAL setter with a fresh
//! `il2cpp_string_new` of the translation (so the game's own layout/mesh regen runs on our text).
//!
//! Feedback-loop guard: the game re-feeds our own output back through `set_text` (result logs
//! persist displayed strings). `translate()` is memoized + deterministic so a re-fed translation
//! resolves to "no change", but we also keep a bounded set of emitted outputs and skip them outright
//! — matching Overseer's `emitted` guard — so a translation that happens to contain an English
//! game-term substring can never be double-substituted.
//!
//! Threading: Unity mutates text only on the main thread, so `read_string`/`new_string` here are
//! always on the IL2CPP main thread. Zero overhead when no language is selected (`glossary::active`
//! is a single ArcSwap load) — we just forward to the original.

#![allow(dead_code)]

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use once_cell::sync::Lazy;
use retour::RawDetour;

use crate::il2cpp::{self, Method, Object};

fn log(m: &str) {
    crate::tools::log(m);
}

/// `void set_text(this, System.String, MethodInfo*)`.
type SetFn = unsafe extern "C" fn(Object, Object, Method);

/// Translation OUTPUTS we've emitted, so the game re-displaying our own text never gets re-translated
/// (which would garble it — target-language text re-fed as a source). Two generations: when the
/// current set fills, it ages into `prev` and a fresh `cur` starts, so recent outputs are never
/// forgotten wholesale (a plain clear-at-N briefly reopened the feedback loop). Bounded at 2×CAP.
struct Emitted {
    cur: HashSet<String>,
    prev: HashSet<String>,
}
// 40k × two generations = up to 80 000 owned `String`s purely to recognise our own output. That
// duty is now carried by `mtl::is_own_output`, a permanent set of 8-byte hashes over the SAME data
// — so this generational set only needs to cover the very recent window that hash set can't
// (strings produced this session but not yet in the cache). 8k is generous for that.
const EMITTED_CAP: usize = 8_000;
static EMITTED: Lazy<Mutex<Emitted>> = Lazy::new(|| {
    Mutex::new(Emitted {
        cur: HashSet::new(),
        prev: HashSet::new(),
    })
});

fn is_emitted(s: &str) -> bool {
    EMITTED
        .lock()
        .map(|e| e.cur.contains(s) || e.prev.contains(s))
        .unwrap_or(false)
}
/// Record a translation output so the game re-displaying it is forwarded untouched. `pub(crate)` so
/// the MTL re-apply pump (mtl::pump) can mark a late translation before re-issuing set_text.
pub(crate) fn record_emitted(s: &str) {
    if let Ok(mut e) = EMITTED.lock() {
        if e.cur.len() >= EMITTED_CAP {
            e.prev = std::mem::take(&mut e.cur); // age out, keep the last generation
        }
        e.cur.insert(s.to_string());
    }
    // Also record it PERMANENTLY (glossary/cache hits are our output too, not just worker inserts) so
    // it's recognised as ours forever, long after it ages out of the generational set above.
    crate::mtl::note_output_pub(s);
}

/// One (trampoline, detour-keepalive) pair per hooked setter. Each detour fn is a thin shim that
/// names its own trampoline and defers to `dispatch`.
macro_rules! setter {
    // $reapply = whether to async-re-apply the arrived translation on-screen for this component class.
    // TRUE for every component INCLUDING TextCommon (story / event / journal dialogue): first-view
    // free-form text (scenario descriptions, event dialogue, event CHOICES) isn't in the pre-translated
    // cache, so a synchronous swap can't cover it — it must be re-applied a beat later once NMT returns.
    // The pump's current-text guard (re-apply only while the component STILL shows the exact source) +
    // the class-validity guard keep this from disturbing the story timeline's reveal. Re-apply was NOT
    // the old soft-lock (that was loc_story's field-pokes, now disabled; the lock persisted even with
    // TextCommon unhooked), so re-applying it is safe and gives day-one dialogue/choice translation.
    ($tr:ident, $keep:ident, $detour:ident, $reapply:expr) => {
        static $tr: AtomicUsize = AtomicUsize::new(0);
        static $keep: std::sync::OnceLock<RawDetour> = std::sync::OnceLock::new();
        unsafe extern "C" fn $detour(this: Object, value: Object, mi: Method) {
            dispatch(this, value, mi, &$tr, $reapply);
        }
    };
}

setter!(TR_TMP, D_TMP, h_tmp, true); // TMPro.TMP_Text
setter!(TR_UI, D_UI, h_ui, true); // UnityEngine.UI.Text
setter!(TR_TC, D_TC, h_tc, true); // Gallop.TextCommon — story/event/choice text; re-apply (guarded)
setter!(TR_TMPU, D_TMPU, h_tmpu, true); // Gallop.TextMeshProUguiCommon

// ─── TextCommon COMPOSITE entry points ───────────────────────────────────────────────────────────
//
// The boot probe showed Gallop.TextCommon exposes a family of text-entry methods beyond the
// `set_text` property — SetTextFromController, SetTextWithLineHeadWrap(+WithColorTag),
// SetTextWithCustomTag(+MultiLine/+IfChanged) — and some screen elements are written ONLY through
// them (the StoryView event-title banner displayed English while its exact translation sat in the
// cache). We hook each one and substitute the incoming string BEFORE the method runs, so the game's
// own wrapping/tag parsing operates on the TRANSLATED text (better line breaks than translating
// pre-wrapped English). Tier behavior: glossary/cache hits swap synchronously; a miss enqueues the
// UNWRAPPED source (priority lane) and passes the original through — the next sighting hits the
// cache. No pump re-apply for these (extra args can't be replayed), but when the method funnels into
// set_text internally the inner hook still tracks the wrapped form exactly as before.
//
// Nesting guard: after WE substitute a translation, the method's internal set_text must forward it
// untouched (the wrapped form won't match the emitted-set, and target-language text re-fed as source
// would garble). Depth-counted so nested composite calls can't clear the flag early. When we DIDN'T
// translate (miss), the inner set_text is left alone — the wrapped-English path keeps working.
//
// TWO fixes over the original `static AtomicUsize`:
//
//  1. **Thread-local.** Unity sets text on the main thread, but the guard is a *call-scope*
//     property, not a process one — a global counter meant any other thread that reached a hooked
//     setter (the MTL re-apply pump running while a composite entry was mid-flight) saw `in_entry()`
//     true and silently forwarded its text untranslated. A thread-local matches what the flag
//     actually means.
//  2. **Self-healing deadline.** A composite entry can call arbitrary game code that never returns
//     through our frame; the `Drop` then never runs and the counter is stuck > 0 forever, so EVERY
//     string on that thread forwards untranslated for the rest of the session ("translation just
//     stopped"). The depth now carries a deadline; past it `in_entry()` reads false and the
//     watchdog reports the leak.
const ENTRY_TTL_MS: u64 = 1000;
thread_local! {
    static ENTRY_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ENTRY_UNTIL: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
/// Cross-thread mirror so the (main-thread) watchdog can SEE a leak on the text thread. Only ever
/// stamped when a guard is taken; the actual gating uses the thread-local.
static ENTRY_LEAK_UNTIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn in_entry() -> bool {
    ENTRY_DEPTH.with(|d| d.get() > 0) && ENTRY_UNTIL.with(|u| crate::tools::now_ms() < u.get())
}

struct EntryGuard;
impl EntryGuard {
    fn new() -> Self {
        let until = crate::tools::now_ms() + ENTRY_TTL_MS;
        ENTRY_DEPTH.with(|d| d.set(d.get() + 1));
        ENTRY_UNTIL.with(|u| u.set(until));
        ENTRY_LEAK_UNTIL.store(until, Ordering::Relaxed);
        EntryGuard
    }
}
impl Drop for EntryGuard {
    fn drop(&mut self) {
        ENTRY_DEPTH.with(|d| {
            let n = d.get().saturating_sub(1);
            d.set(n);
            if n == 0 {
                ENTRY_UNTIL.with(|u| u.set(0));
                ENTRY_LEAK_UNTIL.store(0, Ordering::Relaxed);
            }
        });
    }
}

/// Watchdog hook (`guard::tick`): true once per leaked composite-entry guard.
pub(crate) fn reap_entry_guard() -> bool {
    let until = ENTRY_LEAK_UNTIL.load(Ordering::Relaxed);
    if until != 0 && crate::tools::now_ms() >= until {
        ENTRY_LEAK_UNTIL.store(0, Ordering::Relaxed);
        return true;
    }
    false
}

/// set_text (Method*, trampoline) per hooked base class — the REPLAY route the pump uses to swap a
/// late translation onto a component that was originally written through a composite/method entry
/// point (those have extra args the pump can't replay, but plain set_text works on any of them).
static MI_TC: AtomicUsize = AtomicUsize::new(0); // Gallop.TextCommon.set_text MethodInfo*
static MI_TMP: AtomicUsize = AtomicUsize::new(0); // TMPro.TMP_Text.set_text MethodInfo*

/// Front half shared by every composite entry hook: translate `value` through glossary + MTL cache,
/// or enqueue it (priority lane) on a miss — and, when a replay route is provided, TRACK the
/// component so the pump swaps the translation in on FIRST view (not just the next sighting).
/// Returns the string to pass through and whether it was translated (→ caller raises the nesting
/// guard around the original call).
unsafe fn entry_translate(value: Object, track: Option<(Object, usize, usize)>) -> (Object, bool) {
    if in_entry() || !crate::mtl::translation_active() || value.is_null() {
        return (value, false);
    }
    let s = il2cpp::read_string(value);
    if s.is_empty() || is_emitted(&s) || crate::mtl::is_own_output(&s) || crate::mtl::is_protected_name(&s) {
        return (value, false);
    }
    // USER CONTENT fields pass through verbatim on this path too — every composite hook routes its
    // component in `track`, so the check covers SetTextFromController/LineHeadWrap/CustomTag writes.
    if let Some((this, _, _)) = track {
        if let Some(kind) = user_content_component(this) {
            if kind == UserField::Name {
                crate::mtl::note_user_name(&s);
            }
            return (value, false);
        }
    }
    // Glossary hits are only final for fully-translated results — franglais prose falls through to
    // the cache/NMT tiers (same rule as dispatch tier-1).
    let t = match crate::glossary::translate(&s).filter(|t| crate::mtl::glossary_result_final(&s, t)) {
        Some(t) => Some(t),
        None => crate::mtl::lookup(&s),
    };
    match t {
        Some(t) => {
            record_emitted(&t);
            let ns = il2cpp::new_string(&t);
            if ns.is_null() {
                (value, false)
            } else {
                (ns, true)
            }
        }
        None => {
            match track {
                // Re-apply via the base class's ORIGINAL set_text once the translation arrives. The
                // pump's current-text guard still gates the swap (only while the component displays
                // this exact string), so a wrapped/transformed display form simply never matches and
                // ages out — the inner set_text hook covers those instead.
                Some((this, tramp, mi)) if tramp != 0 && mi != 0 => {
                    crate::mtl::on_miss(this, &s, tramp, mi);
                }
                _ => crate::mtl::request(&s),
            }
            (value, false)
        }
    }
}

/// One-shot "this entry point actually carries traffic" log, so a stubborn untranslated element can
/// be attributed to its write path from the log alone.
fn log_first_traffic(fired: &'static AtomicBool, label: &str) {
    if !fired.swap(true, Ordering::Relaxed) {
        log(&format!("[loc/settext] first traffic: {label}"));
    }
}

/// `(this, String, MethodInfo*)` composite entries (SetTextFromController, 1-arg MultiLine, TMP
/// SetText(String)). `$rt`/`$rm` name the replay-route statics (set_text trampoline + MethodInfo).
macro_rules! entry1 {
    ($tr:ident, $keep:ident, $detour:ident, $label:literal, $rt:ident, $rm:ident) => {
        static $tr: AtomicUsize = AtomicUsize::new(0);
        static $keep: std::sync::OnceLock<RawDetour> = std::sync::OnceLock::new();
        unsafe extern "C" fn $detour(this: Object, value: Object, mi: Method) {
            let tramp = $tr.load(Ordering::Relaxed);
            if tramp == 0 {
                return;
            }
            static FIRED: AtomicBool = AtomicBool::new(false);
            log_first_traffic(&FIRED, $label);
            let route = Some((this, $rt.load(Ordering::Relaxed), $rm.load(Ordering::Relaxed)));
            let (v, translated) = entry_translate(value, route);
            let _g = translated.then(EntryGuard::new);
            let f: SetFn = std::mem::transmute(tramp);
            f(this, v, mi);
        }
    };
}

/// `(this, String, i32, MethodInfo*)` composite entries (LineHeadWrap / +WithColorTag).
macro_rules! entry2i {
    ($tr:ident, $keep:ident, $detour:ident, $label:literal, $rt:ident, $rm:ident) => {
        static $tr: AtomicUsize = AtomicUsize::new(0);
        static $keep: std::sync::OnceLock<RawDetour> = std::sync::OnceLock::new();
        unsafe extern "C" fn $detour(this: Object, value: Object, a: i32, mi: Method) {
            let tramp = $tr.load(Ordering::Relaxed);
            if tramp == 0 {
                return;
            }
            static FIRED: AtomicBool = AtomicBool::new(false);
            log_first_traffic(&FIRED, $label);
            let route = Some((this, $rt.load(Ordering::Relaxed), $rm.load(Ordering::Relaxed)));
            let (v, translated) = entry_translate(value, route);
            let _g = translated.then(EntryGuard::new);
            let f: unsafe extern "C" fn(Object, Object, i32, Method) = std::mem::transmute(tramp);
            f(this, v, a, mi);
        }
    };
}

/// `(this, String, u8/bool, MethodInfo*)` entry (TMP SetText(String, Boolean)).
macro_rules! entry_sb {
    ($tr:ident, $keep:ident, $detour:ident, $label:literal, $rt:ident, $rm:ident) => {
        static $tr: AtomicUsize = AtomicUsize::new(0);
        static $keep: std::sync::OnceLock<RawDetour> = std::sync::OnceLock::new();
        unsafe extern "C" fn $detour(this: Object, value: Object, a: u8, mi: Method) {
            let tramp = $tr.load(Ordering::Relaxed);
            if tramp == 0 {
                return;
            }
            static FIRED: AtomicBool = AtomicBool::new(false);
            log_first_traffic(&FIRED, $label);
            let route = Some((this, $rt.load(Ordering::Relaxed), $rm.load(Ordering::Relaxed)));
            let (v, translated) = entry_translate(value, route);
            let _g = translated.then(EntryGuard::new);
            let f: unsafe extern "C" fn(Object, Object, u8, Method) = std::mem::transmute(tramp);
            f(this, v, a, mi);
        }
    };
}

/// `(this, String, f32, i32, MethodInfo*)` composite entry (SetTextWithCustomTag).
macro_rules! entry_f_i {
    ($tr:ident, $keep:ident, $detour:ident, $label:literal, $rt:ident, $rm:ident) => {
        static $tr: AtomicUsize = AtomicUsize::new(0);
        static $keep: std::sync::OnceLock<RawDetour> = std::sync::OnceLock::new();
        unsafe extern "C" fn $detour(this: Object, value: Object, a: f32, b: i32, mi: Method) {
            let tramp = $tr.load(Ordering::Relaxed);
            if tramp == 0 {
                return;
            }
            static FIRED: AtomicBool = AtomicBool::new(false);
            log_first_traffic(&FIRED, $label);
            let route = Some((this, $rt.load(Ordering::Relaxed), $rm.load(Ordering::Relaxed)));
            let (v, translated) = entry_translate(value, route);
            let _g = translated.then(EntryGuard::new);
            let f: unsafe extern "C" fn(Object, Object, f32, i32, Method) =
                std::mem::transmute(tramp);
            f(this, v, a, b, mi);
        }
    };
}

/// `(this, String, Object, MethodInfo*)` composite entries (MultiLine/IfChanged with a tag parser).
macro_rules! entry_obj {
    ($tr:ident, $keep:ident, $detour:ident, $label:literal, $rt:ident, $rm:ident) => {
        static $tr: AtomicUsize = AtomicUsize::new(0);
        static $keep: std::sync::OnceLock<RawDetour> = std::sync::OnceLock::new();
        unsafe extern "C" fn $detour(this: Object, value: Object, a: Object, mi: Method) {
            let tramp = $tr.load(Ordering::Relaxed);
            if tramp == 0 {
                return;
            }
            static FIRED: AtomicBool = AtomicBool::new(false);
            log_first_traffic(&FIRED, $label);
            let route = Some((this, $rt.load(Ordering::Relaxed), $rm.load(Ordering::Relaxed)));
            let (v, translated) = entry_translate(value, route);
            let _g = translated.then(EntryGuard::new);
            let f: unsafe extern "C" fn(Object, Object, Object, Method) =
                std::mem::transmute(tramp);
            f(this, v, a, mi);
        }
    };
}

entry1!(TR_FC, D_FC, h_fc, "TC.SetTextFromController", TR_TC, MI_TC);
entry1!(TR_ML1, D_ML1, h_ml1, "TC.SetTextWithCustomTagMultiLine/1", TR_TC, MI_TC);
entry1!(TR_LOCW, D_LOCW, h_locw, "TC.SetSystemTextWithLineHeadWrapLocalize", TR_TC, MI_TC);
entry2i!(TR_LHW, D_LHW, h_lhw, "TC.SetTextWithLineHeadWrap", TR_TC, MI_TC);
entry2i!(TR_LHWC, D_LHWC, h_lhwc, "TC.SetTextWithLineHeadWrapWithColorTag", TR_TC, MI_TC);
entry_f_i!(TR_CT, D_CT, h_ct, "TC.SetTextWithCustomTag", TR_TC, MI_TC);
entry_obj!(TR_ML2, D_ML2, h_ml2, "TC.SetTextWithCustomTagMultiLine/2", TR_TC, MI_TC);
entry_obj!(TR_MLC, D_MLC, h_mlc, "TC.SetTextWithCustomTagMultiLineIfChanged", TR_TC, MI_TC);
// TMP_Text.SetText METHOD overloads — NOT the set_text property: TMP writes its backing text
// directly here, so the property hook never fires for SetText callers (the story event-choice
// button labels stayed English while their translations sat in the cache — cached via the journal
// sighting, never swapped on the button).
entry1!(TR_ST1, D_ST1, h_st1, "TMP.SetText(String)", TR_TMP, MI_TMP);
entry_sb!(TR_ST2, D_ST2, h_st2, "TMP.SetText(String,Boolean)", TR_TMP, MI_TMP);

/// Shared body: translate `value` through the layered pipeline, then invoke the original setter.
/// Tiers: (1) glossary — instant, exact+substring; (2) MTL cache — instant, prior NMT output;
/// (3) async NMT — show the source now, re-apply the translation a beat later via mtl::pump.
unsafe fn dispatch(this: Object, value: Object, mi: Method, tr: &AtomicUsize, reapply: bool) {
    let tramp = tr.load(Ordering::Relaxed);
    let call_orig = |v: Object| {
        if tramp != 0 {
            let f: SetFn = std::mem::transmute(tramp);
            f(this, v, mi);
        }
    };
    // Diagnostic trace (off by default, flipped from /api/mod/tracetext). Logs every string this hook
    // touches and what we hand back to the game. Bisecting proved the Scout soft-lock is caused by
    // translation being ACTIVE (bot on + tempo 1x + tl_lang=fr locks; tl_lang=none works), and the
    // Scout view never constructs — so the last strings we rewrite before the freeze are the lead.
    // Log each DISTINCT source once. The first cut logged every call and drowned in per-frame timer
    // and percentage churn (4000 lines in one second, cap blown, freeze invisible). What we need is
    // the SEQUENCE of unique strings the flow touches, not proof that a clock ticks.
    let traced = trace_on() && !value.is_null() && {
        let src = il2cpp::read_string(value);
        trace_first_time(&src)
    };
    if traced {
        let cls = crate::il2cpp::object_class_name(this);
        let src = il2cpp::read_string(value);
        // Also log the GameObject name + its parent — user-content fields (trainer name, club name,
        // profile comment) share the TextCommon class with everything else, so the CLASS can't tell
        // them apart. The GameObject/hierarchy name is what identifies "this is the club label".
        let go = go_hierarchy(this);
        trace_log(&format!("[ttrace] {cls} [{go}] <- {src:?} (reapply={reapply})"));
    }

    // A composite entry hook (SetTextWithLineHeadWrap etc.) already translated this call's text and
    // is invoking the original method, which funnels into set_text internally — forward untouched.
    // (The wrapped form of our translation wouldn't match the emitted-set, and target-language text
    // re-fed as a source would garble.)
    if in_entry() {
        return call_orig(value);
    }
    // Fast out: no target language selected → forward untouched. (Gated on tl_lang, NOT glossary
    // activity, so the JP client — where the English glossary is inactive but NMT still applies —
    // reaches the MTL tiers below.)
    if !crate::mtl::translation_active() {
        return call_orig(value);
    }
    if value.is_null() {
        return call_orig(value);
    }
    let s = il2cpp::read_string(value);
    // is_emitted is the GENERATIONAL guard (last two batches); is_own_output is the PERMANENT one
    // (every translation we ever produced). Both: once our French ages out of is_emitted, re-feeding
    // it as a source re-translates our own output into garble ("Indice Lvl 1" => "L'indice Lvl est
    // de 1."). is_own_output closes that window for good.
    if s.is_empty() || is_emitted(&s) || crate::mtl::is_own_output(&s) {
        return call_orig(value);
    }
    // Protected proper names (skill / character / race names) stay English — skip all translation.
    if crate::mtl::is_protected_name(&s) {
        return call_orig(value);
    }
    // USER CONTENT (trainer name / club name / profile comment) — identified by the component's
    // GameObject, not by the text: player-typed strings look exactly like ordinary prose, so content
    // matching can never separate them. Forward verbatim; don't enqueue, don't track (the pump can
    // then never re-apply onto these either). Name fields also FEED the embedded-mask set, so the
    // learned name survives inside prose sent to NMT.
    if let Some(kind) = user_content_component(this) {
        if kind == UserField::Name {
            crate::mtl::note_user_name(&s);
        }
        if traced {
            trace_log(&format!("[ttrace]   -> SKIP user-content field ({s:?} stays verbatim)"));
        }
        return call_orig(value);
    }

    // 1. Glossary (instant). Inactive glossary returns None (e.g. JP client) → fall through.
    //    A glossary hit is only FINAL for fully-translated results (labels/terms): word-substitution
    //    on free-form prose leaves franglais ("I était checking…") — those fall through to the
    //    cache/NMT tiers with the ORIGINAL text instead of being locked in by the early return.
    if let Some(t) = crate::glossary::translate(&s) {
        if crate::mtl::glossary_result_final(&s, &t) {
            record_emitted(&t);
            crate::mtl::note_recent(&s, &t); // recent-translations feed (click-to-fix)
            if traced {
                trace_log(&format!("[ttrace]   -> GLOSSARY {s:?} => {t:?}"));
            }
            let ns = il2cpp::new_string(&t);
            return call_orig(if ns.is_null() { value } else { ns });
        }
    }
    // 2. MTL cache (instant) — a translation we (or a prior session) already computed, re-wrapped
    //    to this source's box shape (a wrapped-source key + unwrapped value would spill the box).
    if let Some(t) = crate::mtl::lookup_fitted(&s) {
        record_emitted(&t);
        crate::mtl::note_recent(&s, &t); // recent-translations feed (click-to-fix)
        if traced {
            trace_log(&format!("[ttrace]   -> CACHE {s:?} => {t:?}"));
        }
        let ns = il2cpp::new_string(&t);
        return call_orig(if ns.is_null() { value } else { ns });
    }
    // 3. Async NMT. Enqueue + track so the pump swaps in the translation a beat later (once NLLB
    //    returns), while the game shows the source in the meantime. TextCommon (reapply=true now)
    //    goes through this path too, so scenario descriptions / event dialogue / event choices get
    //    translated on FIRST view — the pump's current-text guard prevents timeline desync. The rare
    //    class with reapply=false would only enqueue for a later synchronous swap.
    if traced {
        trace_log(&format!("[ttrace]   -> MISS {s:?} (enqueued, reapply={reapply})"));
    }
    if reapply {
        crate::mtl::on_miss(this, &s, tramp, mi as usize);
    } else {
        crate::mtl::request(&s);
    }
    call_orig(value);
}

// ── diagnostic text trace ───────────────────────────────────────────────────────────────────────
// Off by default; POST /api/mod/tracetext {"on":true} arms it. Capped so a runaway trace can never
// fill the disk or slow the main thread to a crawl — this is a debugging aid, not telemetry.
static TRACE: AtomicBool = AtomicBool::new(false);
static TRACE_LEFT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
const TRACE_CAP: i64 = 4000;

/// Sources already logged this trace session — so each distinct string appears exactly once.
static TRACE_SEEN: std::sync::Mutex<Option<std::collections::HashSet<String>>> = std::sync::Mutex::new(None);
fn trace_first_time(src: &str) -> bool {
    match TRACE_SEEN.lock() {
        Ok(mut g) => g.get_or_insert_with(std::collections::HashSet::new).insert(src.to_string()),
        Err(_) => false,
    }
}

pub fn set_trace(on: bool) {
    if on {
        TRACE_LEFT.store(TRACE_CAP, Ordering::Relaxed);
        if let Ok(mut g) = TRACE_SEEN.lock() {
            *g = Some(std::collections::HashSet::new());
        }
    }
    TRACE.store(on, Ordering::Relaxed);
    crate::tools::log(if on {
        "[ttrace] text trace ARMED — every translated string will be logged (capped)"
    } else {
        "[ttrace] text trace off"
    });
}
pub fn trace_on() -> bool {
    TRACE.load(Ordering::Relaxed)
}

// Cached Method handles for the GameObject-name walk. get_gameObject is on UnityEngine.Component and
// get_name on UnityEngine.Object — both base classes, so class_get_method_from_name (which walks
// parents) returns the same MethodInfo* regardless of which TextCommon-derived class we resolve from.
static M_GET_GO: AtomicUsize = AtomicUsize::new(0);
static M_GET_NAME: AtomicUsize = AtomicUsize::new(0);
static M_GET_TF: AtomicUsize = AtomicUsize::new(0);
static M_GET_PARENT: AtomicUsize = AtomicUsize::new(0);

unsafe fn call_obj0(this: Object, slot: &AtomicUsize, cls: il2cpp::Class, name: &str) -> Object {
    let mut m = slot.load(Ordering::Relaxed) as il2cpp::Method;
    if m.is_null() {
        m = il2cpp::method(cls, name, 0);
        slot.store(m as usize, Ordering::Relaxed);
    }
    if m.is_null() {
        return std::ptr::null_mut();
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return std::ptr::null_mut();
    }
    let f: extern "C" fn(Object, *const std::ffi::c_void) -> Object = std::mem::transmute(p);
    f(this, m as *const std::ffi::c_void)
}

/// GameObject-name tokens that mark a component as USER CONTENT — player-typed data, not game text:
/// trainer names, club (circle) names, profile comments/greetings. These must NEVER be translated:
/// a name is an identity (translating "Mama Yuurai" into a French sentence is nonsense), and the
/// comment field is EDITABLE — a translated value sitting in it can get saved back to the server as
/// the player's actual profile text. Captured live from the profile screen with the hierarchy trace:
/// `TrainerNameText < Up`, `CircleNameText < Circle`, `CommentText < Comment`. The other tokens are
/// the game's standard naming for the same data on other screens (friends list, race opponents).
/// Substring match, so `TrainerNameText`, `PartsTrainerName(Clone)` etc. all hit.
/// What kind of user content a field holds. `Name` fields (trainer/club names) additionally FEED the
/// embedded-mask set — a string displayed in a name field is a real name by construction, so learning
/// it protects its occurrences inside prose too. `Prose` fields (the profile comment) are skipped but
/// never learned: masking whole sentences would wreck the NMT of any text quoting them.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum UserField {
    /// Player-typed identity (trainer/club name). Skipped AND learned into the embedded-mask set.
    Name,
    /// Player-typed prose (profile comment). Skipped, never learned (masking sentences breaks NMT).
    Prose,
    /// Game proper noun displayed in a dedicated name slot (skill names). Skipped, never learned:
    /// skill names are often common words ("Focus", "Early Lead"), and masking those inside prose
    /// would silently degrade the translation of every sentence that happens to use the word.
    Term,
}

fn user_content_kind_of(name: &str) -> Option<UserField> {
    const NAME_TOKENS: [&str; 6] =
        ["TrainerName", "CircleName", "NickName", "Nickname", "UserName", "PlayerName"];
    if NAME_TOKENS.iter().any(|t| name.contains(t)) {
        return Some(UserField::Name);
    }
    if name.contains("CommentText") {
        return Some(UserField::Prose);
    }
    None
}

// PERF: `user_content_component` ran SIX il2cpp virtual calls (gameObject → name → transform →
// parent → gameObject → name, plus two managed-string decodes) on EVERY set_text — and set_text is
// the single hottest hook Overseer owns, firing for every label on every screen, every frame a
// counter ticks. Memoize it per component. The cache is verified on each hit against the
// component's CURRENT gameObject pointer, so a recycled component slot re-resolves instead of
// inheriting the previous occupant's classification (which would either translate a player's name
// or silently skip a real label). That verification is one virtual call, so a hit costs 1/6th of a
// miss with no loss of accuracy.
struct UserFieldCache {
    go: usize,               // the gameObject this verdict was computed for
    kind: Option<UserField>, // None = ordinary game text
}
static UF_CACHE: Lazy<Mutex<std::collections::HashMap<usize, UserFieldCache>>> =
    Lazy::new(|| Mutex::new(std::collections::HashMap::new()));
const UF_CACHE_CAP: usize = 4096;

/// Current size of the user-content classification cache (memory report).
pub fn uf_cache_len() -> usize {
    UF_CACHE.lock().map(|m| m.len()).unwrap_or(0)
}

/// Drop the memoized classifications (language switch / explicit cache trim).
pub fn clear_uf_cache() {
    if let Ok(mut m) = UF_CACHE.lock() {
        m.clear();
    }
}

/// Does this text component sit on a user-content GameObject? (See [`user_content_kind_of`].)
/// Memoized per component (see `UF_CACHE`); falls back to `None` on any miss so a stripped getter
/// can only ever mean "translate as before", never a crash.
pub(crate) unsafe fn user_content_component(this: Object) -> Option<UserField> {
    if this.is_null() {
        return None;
    }
    let cls = il2cpp::object_class(this);
    let go = call_obj0(this, &M_GET_GO, cls, "get_gameObject");
    if go.is_null() {
        return None;
    }
    // Fast path: same component, same GameObject → reuse the verdict.
    if let Ok(m) = UF_CACHE.lock() {
        if let Some(e) = m.get(&(this as usize)) {
            if e.go == go as usize {
                return e.kind;
            }
        }
    }
    let kind = user_content_component_slow(go);
    if let Ok(mut m) = UF_CACHE.lock() {
        if m.len() >= UF_CACHE_CAP {
            m.clear(); // bounded working set; a clear costs one re-resolve per live component
        }
        m.insert(this as usize, UserFieldCache { go: go as usize, kind });
    }
    kind
}

/// The uncached classification: walk the GameObject (and its parent) names.
unsafe fn user_content_component_slow(go: Object) -> Option<UserField> {
    let go_cls = il2cpp::object_class(go);
    let name = il2cpp::read_string(call_obj0(go, &M_GET_NAME, go_cls, "get_name"));
    if let Some(k) = user_content_kind_of(&name) {
        return Some(k);
    }
    // Skill list rows: the NAME label and the DESCRIPTION both sit under a parent named "NameRoot" —
    // the name label's own GameObject carries a designer placeholder (Japanese text), so the PARENT
    // is the only reliable marker. Under NameRoot, everything except SkillDesc is the skill's NAME:
    // "only descriptions must be translated" (user requirement). Captured live from the skills page:
    //   [ttrace] TextCommon [シューティングスター (1) < NameRoot] <- "Focus"        (name — skip)
    //   [ttrace] TextCommon [SkillDesc < NameRoot]              <- "Slightly …"  (description — translate)
    // This is structural, so it covers ALL skills — the bundled names.json list (383 entries) only
    // ever protected the skills someone had typed in, and "Focus" → "Focus Focus" slipped past it.
    if !name.contains("SkillDesc") {
        let tf = call_obj0(go, &M_GET_TF, go_cls, "get_transform");
        if !tf.is_null() {
            let p = call_obj0(tf, &M_GET_PARENT, il2cpp::object_class(tf), "get_parent");
            if !p.is_null() {
                let p_go = call_obj0(p, &M_GET_GO, il2cpp::object_class(p), "get_gameObject");
                let parent =
                    il2cpp::read_string(call_obj0(p_go, &M_GET_NAME, il2cpp::object_class(p_go), "get_name"));
                if parent == "NameRoot" {
                    return Some(UserField::Term);
                }
            }
        }
    }
    None
}

/// "goName < parentName" for a component — the Unity hierarchy that identifies WHICH field this text
/// is. Best-effort: any missing link just shortens the string. Diagnostic only.
unsafe fn go_hierarchy(this: Object) -> String {
    if this.is_null() {
        return String::new();
    }
    let cls = il2cpp::object_class(this);
    let go = call_obj0(this, &M_GET_GO, cls, "get_gameObject");
    if go.is_null() {
        return String::new();
    }
    let go_cls = il2cpp::object_class(go);
    let name = il2cpp::read_string(call_obj0(go, &M_GET_NAME, go_cls, "get_name"));
    // Parent GameObject name via transform.parent — cheap context ("TrainerName" under "ProfileTop").
    let tf = call_obj0(go, &M_GET_TF, go_cls, "get_transform");
    let parent = if tf.is_null() {
        String::new()
    } else {
        let tf_cls = il2cpp::object_class(tf);
        let p = call_obj0(tf, &M_GET_PARENT, tf_cls, "get_parent");
        if p.is_null() {
            String::new()
        } else {
            let p_go = call_obj0(p, &M_GET_GO, il2cpp::object_class(p), "get_gameObject");
            il2cpp::read_string(call_obj0(p_go, &M_GET_NAME, il2cpp::object_class(p_go), "get_name"))
        }
    };
    if parent.is_empty() { name } else { format!("{name} < {parent}") }
}
fn trace_log(msg: &str) {
    if TRACE_LEFT.fetch_sub(1, Ordering::Relaxed) <= 0 {
        TRACE.store(false, Ordering::Relaxed);
        crate::tools::log("[ttrace] trace cap reached — trace disabled");
        return;
    }
    crate::tools::log(msg);
}

/// Resolve + hook `<class>.set_text(string)`. Missing types/methods are non-fatal (that component
/// simply isn't present in this build) — we skip and report how many armed.
unsafe fn hook_setter(
    full_class: &str,
    label: &str,
    detour: unsafe extern "C" fn(Object, Object, Method),
    tr: &AtomicUsize,
    keep: &std::sync::OnceLock<RawDetour>,
) -> bool {
    let klass = il2cpp::class(full_class);
    if klass.is_null() {
        return false;
    }
    let m = il2cpp::method(klass, "set_text", 1);
    if m.is_null() {
        return false;
    }
    match il2cpp::hook_at(m, label, detour as *const (), tr, keep) {
        Ok(()) => true,
        Err(e) => {
            log(&format!("[loc/settext] {label}: {e}"));
            false
        }
    }
}

/// Arm all four text setters. Returns Err only if NONE could be armed (translation would be
/// Localize.Get-only). Call from boot on the IL2CPP main thread.
pub fn install() -> Result<(), String> {
    let mut armed = 0;
    unsafe {
        armed += hook_setter("TMPro.TMP_Text", "TMP_Text.set_text", h_tmp, &TR_TMP, &D_TMP) as i32;
        armed += hook_setter("UnityEngine.UI.Text", "UI.Text.set_text", h_ui, &TR_UI, &D_UI) as i32;
        // Gallop.TextCommon = the story-dialogue / event-choice / journal-log text component. Hooked
        // WITH re-apply (reapply=true) now that the real soft-lock cause (loc_story's field pokes) is
        // gone — the earlier lock persisted even with this UNhooked, proving it wasn't the culprit.
        // Re-applying it is what gives day-one translation of scenario descriptions, in-event dialogue,
        // and the event CHOICE buttons (none of which are in the pre-translated master.mdb cache). The
        // pump's current-text guard + class-validity guard + throttling keep the async re-apply from
        // disturbing a component whose line has since advanced.
        armed += hook_setter("Gallop.TextCommon", "TextCommon.set_text", h_tc, &TR_TC, &D_TC) as i32;
        armed += hook_setter(
            "Gallop.TextMeshProUguiCommon",
            "TMPUguiCommon.set_text",
            h_tmpu,
            &TR_TMPU,
            &D_TMPU,
        ) as i32;
    }
    if armed == 0 {
        return Err("no text setters resolved (TMP_Text/UI.Text/TextCommon/TMPUguiCommon)".into());
    }
    log(&format!("[loc/settext] armed {armed} text setters (incl. TextCommon)"));

    // Composite TextCommon entry points (probe-discovered). Resolved by name + argc — the two
    // SetTextWithCustomTagMultiLine overloads disambiguate on arity. Missing methods are non-fatal.
    let mut extra = 0;
    unsafe {
        let tc = il2cpp::class("Gallop.TextCommon");
        if !tc.is_null() {
            let mut arm = |name: &str,
                           argc: i32,
                           label: &str,
                           detour: *const (),
                           tr: &AtomicUsize,
                           keep: &std::sync::OnceLock<RawDetour>| {
                let m = il2cpp::method(tc, name, argc);
                if m.is_null() {
                    return 0;
                }
                match unsafe { il2cpp::hook_at(m, label, detour, tr, keep) } {
                    Ok(()) => 1,
                    Err(e) => {
                        log(&format!("[loc/settext] {label}: {e}"));
                        0
                    }
                }
            };
            // Replay route: the pump re-applies late translations through set_text's ORIGINAL.
            MI_TC.store(il2cpp::method(tc, "set_text", 1) as usize, Ordering::Relaxed);
            extra += arm("SetTextFromController", 1, "TC.SetTextFromController", h_fc as *const (), &TR_FC, &D_FC);
            extra += arm("SetTextWithLineHeadWrap", 2, "TC.SetTextWithLineHeadWrap", h_lhw as *const (), &TR_LHW, &D_LHW);
            extra += arm("SetTextWithLineHeadWrapWithColorTag", 2, "TC.SetTextWithLineHeadWrapWithColorTag", h_lhwc as *const (), &TR_LHWC, &D_LHWC);
            extra += arm("SetTextWithCustomTag", 3, "TC.SetTextWithCustomTag", h_ct as *const (), &TR_CT, &D_CT);
            extra += arm("SetTextWithCustomTagMultiLine", 1, "TC.SetTextWithCustomTagMultiLine/1", h_ml1 as *const (), &TR_ML1, &D_ML1);
            extra += arm("SetTextWithCustomTagMultiLine", 2, "TC.SetTextWithCustomTagMultiLine/2", h_ml2 as *const (), &TR_ML2, &D_ML2);
            extra += arm("SetTextWithCustomTagMultiLineIfChanged", 2, "TC.SetTextWithCustomTagMultiLineIfChanged", h_mlc as *const (), &TR_MLC, &D_MLC);
            extra += arm("SetSystemTextWithLineHeadWrapLocalize", 1, "TC.SetSystemTextWithLineHeadWrapLocalize", h_locw as *const (), &TR_LOCW, &D_LOCW);
        }
        // TMP_Text.SetText METHOD overloads (type-checked — argc-only matching could grab the
        // StringBuilder/char[] overloads, whose first arg must NOT be read as a string).
        let tmp = il2cpp::class("TMPro.TMP_Text");
        if !tmp.is_null() {
            MI_TMP.store(il2cpp::method(tmp, "set_text", 1) as usize, Ordering::Relaxed);
            let mut arm_overload = |types: &[i32],
                                    label: &str,
                                    detour: *const (),
                                    tr: &AtomicUsize,
                                    keep: &std::sync::OnceLock<RawDetour>| {
                let m = il2cpp::method_overload(tmp, "SetText", types);
                if m.is_null() {
                    return 0;
                }
                match unsafe { il2cpp::hook_at(m, label, detour, tr, keep) } {
                    Ok(()) => 1,
                    Err(e) => {
                        log(&format!("[loc/settext] {label}: {e}"));
                        0
                    }
                }
            };
            extra += arm_overload(
                &[il2cpp::IL2CPP_TYPE_STRING],
                "TMP.SetText(String)",
                h_st1 as *const (),
                &TR_ST1,
                &D_ST1,
            );
            extra += arm_overload(
                &[il2cpp::IL2CPP_TYPE_STRING, il2cpp::IL2CPP_TYPE_BOOLEAN],
                "TMP.SetText(String,Boolean)",
                h_st2 as *const (),
                &TR_ST2,
                &D_ST2,
            );
        }
    }
    log(&format!("[loc/settext] armed {extra} composite/method text entry points"));
    probe_text_entry_points();
    Ok(())
}

/// One-shot boot probe: list plausible text-ENTRY methods (with parameter types) on the story/Flash
/// text classes beyond the four hooked `set_text` setters. Some elements reach the screen without
/// ever passing a hooked setter — the StoryView event-title banner displays English even when its
/// exact string is in the MTL cache — so this reveals which unhooked method (SetText overload /
/// Flash import / direct field write path) to hook next.
fn probe_text_entry_points() {
    // Verbose-only — same reasoning as `skip::result::probe_result_skip`. This one was the single
    // largest source of boot noise (1460 [loc/settext] lines across the field log).
    if !crate::tools::log::trace_enabled() {
        return;
    }
    for cls_name in [
        "Gallop.TextCommon",
        "Gallop.FlashToUguiText",
        "FlashToUguiText",
        "Gallop.Flash.FlashToUguiText",
        "Gallop.BitmapTextCommon",
        "BitmapTextCommon",
        "Gallop.StoryViewText",
        "StoryViewText",
        "Gallop.StoryViewController",
    ] {
        let k = il2cpp::class(cls_name);
        if k.is_null() {
            log(&format!("[loc/settext] probe: class not found under {cls_name:?}"));
            continue;
        }
        let mut seen: HashSet<String> = HashSet::new();
        for entry in il2cpp::class_methods(k) {
            let name = entry.split('/').next().unwrap_or("").to_string();
            let lower = name.to_lowercase();
            if !(lower.contains("text") || lower.contains("import"))
                || lower.starts_with("get_")
                || name == "set_text"
                || !seen.insert(name.clone())
            {
                continue;
            }
            let sigs = il2cpp::method_param_types(k, &name);
            if sigs.is_empty() {
                continue;
            }
            log(&format!(
                "[loc/settext] probe {cls_name}.{name}({})",
                sigs.join(" | ")
            ));
        }
    }
    // TMP_Text: tight filter (SetText*/SetCharArray only — the class has dozens of text-adjacent
    // methods and only the direct text-entry family matters for coverage gaps).
    let tmp = il2cpp::class("TMPro.TMP_Text");
    if !tmp.is_null() {
        let mut seen: HashSet<String> = HashSet::new();
        for entry in il2cpp::class_methods(tmp) {
            let name = entry.split('/').next().unwrap_or("").to_string();
            let l = name.to_lowercase();
            if !(l.starts_with("settext") || l.contains("chararray")) || !seen.insert(name.clone())
            {
                continue;
            }
            let sigs = il2cpp::method_param_types(tmp, &name);
            log(&format!(
                "[loc/settext] probe TMPro.TMP_Text.{name}({})",
                sigs.join(" | ")
            ));
        }
    }
}

/// Drop the emitted-output guard (call on language change so a new language starts clean).
pub fn reset_emitted() {
    if let Ok(mut e) = EMITTED.lock() {
        e.cur.clear();
        e.prev.clear();
    }
}
