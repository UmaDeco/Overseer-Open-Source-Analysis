//! Wave 6 — replacement-font swap (ported from the upstream localization framework's
//! font-swap hooks). One Font is loaded from the pack's
//! extra_asset_bundle + replacement_font_name, cached behind a GCHandle, revalidated each Awake,
//! and applied as each text component wakes:
//!   Gallop.TextCommon.Awake            -> UnityEngine.UI.Text::set_font(this, Font)
//!   Gallop.TextMeshProUguiCommon.Awake -> TMPro.TMP_Text::set_font(this, TMP_FontAsset)
//! ABI: managed helpers pass the trailing hidden MethodInfo* exactly like assets/bindings.rs; the
//! Awake detours declare the trailing `mi: Method` and forward it (the upstream implementation does NOT strip it either — see
//! loc_ui::h_get). Runs entirely on the main (attached) thread at Awake time.
//!
//! Non-Latin MTL extension: when NO pack font is configured but a target language (settings::tl_lang)
//! is set, these same Awake hooks (a) remediate OVERFLOW on every text component (UI.Text wrap +
//! best-fit, TMP word-wrap + shrink-to-fit — never truncating/hiding) and (b) swap in a Windows OS
//! DYNAMIC font (Font.CreateDynamicFontFromOSFont) chosen per script so Korean/Chinese/Thai/Cyrillic/…
//! glyphs actually render. A pack font/overflow still win where configured. Every added helper is
//! resolved NON-FATALLY (IL2CPP may strip it) and every call is null-guarded — a stripped method just
//! degrades that one knob, it never crashes.
#![allow(non_snake_case, dead_code)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use once_cell::sync::Lazy;
use retour::RawDetour;
use crate::il2cpp::{self, GCHandle, Method, Object};
use crate::localize::LocalizedData;

const HIDE_FLAGS_DONT_UNLOAD_UNUSED_ASSET: i32 = 32;
// Overflow tuning: never grow text, only shrink toward a floor so short labels are untouched.
const UI_TEXT_MIN_SIZE: i32 = 12;   // UI.Text best-fit shrink floor (px)
const TMP_MIN_RATIO: f32 = 0.6;     // TMP auto-size floor = 60% of the component's current size…
const TMP_MIN_SIZE: f32 = 12.0;     // …but never below this absolute floor (px)
fn log(m: &str) { crate::tools::log(m); }

// Resolved Method handles (managed helpers) + Font Type object.
static M_TEXT_SET_FONT: AtomicUsize = AtomicUsize::new(0); // UI.Text::set_font(1) inst
static M_TEXT_SET_HO:   AtomicUsize = AtomicUsize::new(0); // UI.Text::set_horizontalOverflow(1) inst
static M_TEXT_SET_VO:   AtomicUsize = AtomicUsize::new(0); // UI.Text::set_verticalOverflow(1) inst
static M_TMP_SET_FONT:  AtomicUsize = AtomicUsize::new(0); // TMP_Text::set_font(1) inst
static M_CREATE_FA:     AtomicUsize = AtomicUsize::new(0); // TMP_FontAsset::CreateFontAsset(1) STATIC
static M_SET_HIDEFLAGS: AtomicUsize = AtomicUsize::new(0); // Object::set_hideFlags(1) inst
static M_IS_ALIVE:      AtomicUsize = AtomicUsize::new(0); // Object::IsNativeObjectAlive(1) STATIC
static FONT_TYPE_OBJECT: AtomicUsize = AtomicUsize::new(0); // System.Type of UnityEngine.Font

// OS dynamic font (Part 2). Font::CreateDynamicFontFromOSFont(string,int) — a STRIP CANDIDATE (the
// game never calls it), so resolved + probed at install() and gated on OS_FONT_OK/TMP_OS_OK.
static M_CREATE_OSFONT: AtomicUsize = AtomicUsize::new(0); // Font::CreateDynamicFontFromOSFont(string,int) STATIC
// UI.Text and TMP are tracked SEPARATELY: a Font may build (UI ok) while CreateFontAsset(Font) yields
// an empty TMP atlas (TMP then degrades to overflow-only).
static OS_FONT_OK: AtomicBool = AtomicBool::new(false); // Font.CreateDynamicFontFromOSFont usable
static TMP_OS_OK:  AtomicBool = AtomicBool::new(false); // TMP asset from an OS Font usable

// Overflow-remediation helpers (Part 1). ALL non-fatal: a null (IL2CPP-stripped) handle is skipped at
// runtime, so the component just keeps whatever wrap/size it had — never a crash. Logged in install().
static M_TEXT_SET_BESTFIT: AtomicUsize = AtomicUsize::new(0); // UI.Text::set_resizeTextForBestFit(1) bool
static M_TEXT_SET_MINSIZE: AtomicUsize = AtomicUsize::new(0); // UI.Text::set_resizeTextMinSize(1) i32
static M_TMP_SET_AUTOSIZE: AtomicUsize = AtomicUsize::new(0); // TMP_Text::set_enableAutoSizing(1) bool
static M_TMP_SET_WORDWRAP: AtomicUsize = AtomicUsize::new(0); // TMP_Text::set_enableWordWrapping(1) bool (older TMP)
static M_TMP_SET_WRAPMODE: AtomicUsize = AtomicUsize::new(0); // TMP_Text::set_textWrappingMode(1) enum i32 (TMP 3.x)
static M_TMP_SET_OVERFLOW: AtomicUsize = AtomicUsize::new(0); // TMP_Text::set_overflowMode(1) enum i32
static M_TMP_SET_FSMIN:    AtomicUsize = AtomicUsize::new(0); // TMP_Text::set_fontSizeMin(1) f32
static M_TMP_SET_FSMAX:    AtomicUsize = AtomicUsize::new(0); // TMP_Text::set_fontSizeMax(1) f32
static M_TMP_GET_FONTSIZE: AtomicUsize = AtomicUsize::new(0); // TMP_Text::get_fontSize(0) -> f32

// GCHandle caches (one shared Font; TMP asset derived from it).
static EXTRA_BUNDLE:  Lazy<Mutex<Option<GCHandle>>> = Lazy::new(|| Mutex::new(None));
static FONT_HANDLE:   Lazy<Mutex<Option<GCHandle>>> = Lazy::new(|| Mutex::new(None));
static TMP_HANDLE:    Lazy<Mutex<Option<GCHandle>>> = Lazy::new(|| Mutex::new(None));
// Language the cached OS Font/TMP handles were built for; a mismatch triggers a rebuild (see load_os_font).
static OS_FONT_LANG:  Lazy<Mutex<Option<String>>>   = Lazy::new(|| Mutex::new(None));

// ── managed helper wrappers (deref methodPointer, pass Method as trailing MethodInfo*) ──
fn text_set_font(this: Object, v: Object) { call_ov(M_TEXT_SET_FONT.load(Ordering::Relaxed) as Method, this, v); }
fn tmp_set_font(this: Object, v: Object)  { call_ov(M_TMP_SET_FONT.load(Ordering::Relaxed) as Method, this, v); }
fn set_hide_flags(this: Object, v: i32)   { call_oi(M_SET_HIDEFLAGS.load(Ordering::Relaxed) as Method, this, v); }
fn text_set_ho(this: Object, v: i32)      { call_oi(M_TEXT_SET_HO.load(Ordering::Relaxed) as Method, this, v); }
fn text_set_vo(this: Object, v: i32)      { call_oi(M_TEXT_SET_VO.load(Ordering::Relaxed) as Method, this, v); }
// Overflow knobs (each a no-op when its handle is null/stripped).
fn text_set_bestfit(this: Object, v: bool) { call_ob(M_TEXT_SET_BESTFIT.load(Ordering::Relaxed) as Method, this, v); }
fn text_set_minsize(this: Object, v: i32)  { call_oi(M_TEXT_SET_MINSIZE.load(Ordering::Relaxed) as Method, this, v); }
fn tmp_set_autosize(this: Object, v: bool) { call_ob(M_TMP_SET_AUTOSIZE.load(Ordering::Relaxed) as Method, this, v); }
fn tmp_set_wordwrap(this: Object, v: bool) { call_ob(M_TMP_SET_WORDWRAP.load(Ordering::Relaxed) as Method, this, v); }
fn tmp_set_wrapmode(this: Object, v: i32)  { call_oi(M_TMP_SET_WRAPMODE.load(Ordering::Relaxed) as Method, this, v); }
fn tmp_set_overflow(this: Object, v: i32)  { call_oi(M_TMP_SET_OVERFLOW.load(Ordering::Relaxed) as Method, this, v); }
fn tmp_set_fsmin(this: Object, v: f32)     { call_of(M_TMP_SET_FSMIN.load(Ordering::Relaxed) as Method, this, v); }
fn tmp_set_fsmax(this: Object, v: f32)     { call_of(M_TMP_SET_FSMAX.load(Ordering::Relaxed) as Method, this, v); }
fn tmp_get_fontsize(this: Object) -> f32   { call_o_getf(M_TMP_GET_FONTSIZE.load(Ordering::Relaxed) as Method, this) }

// instance, (this, Object value, MethodInfo*)
fn call_ov(m: Method, this: Object, v: Object) {
    if m.is_null() { return; }
    let p = il2cpp::method_pointer(m);
    if p.is_null() { return; }
    let f: extern "C" fn(Object, Object, Method) = unsafe { std::mem::transmute(p) };
    f(this, v, m);
}
// instance, (this, i32 value, MethodInfo*)
fn call_oi(m: Method, this: Object, v: i32) {
    if m.is_null() { return; }
    let p = il2cpp::method_pointer(m);
    if p.is_null() { return; }
    let f: extern "C" fn(Object, i32, Method) = unsafe { std::mem::transmute(p) };
    f(this, v, m);
}
// instance, (this, bool value, MethodInfo*) — an il2cpp bool marshals as a 1-byte u8
fn call_ob(m: Method, this: Object, v: bool) {
    if m.is_null() { return; }
    let p = il2cpp::method_pointer(m);
    if p.is_null() { return; }
    let f: extern "C" fn(Object, u8, Method) = unsafe { std::mem::transmute(p) };
    f(this, v as u8, m);
}
// instance, (this, f32 value, MethodInfo*)
fn call_of(m: Method, this: Object, v: f32) {
    if m.is_null() { return; }
    let p = il2cpp::method_pointer(m);
    if p.is_null() { return; }
    let f: extern "C" fn(Object, f32, Method) = unsafe { std::mem::transmute(p) };
    f(this, v, m);
}
// instance getter, (this, MethodInfo*) -> f32; 0.0 when stripped (callers treat 0.0 as "unknown")
fn call_o_getf(m: Method, this: Object) -> f32 {
    if m.is_null() { return 0.0; }
    let p = il2cpp::method_pointer(m);
    if p.is_null() { return 0.0; }
    let f: extern "C" fn(Object, Method) -> f32 = unsafe { std::mem::transmute(p) };
    f(this, m)
}
// STATIC, (Object font, MethodInfo*) -> TMP_FontAsset*
fn create_font_asset(font: Object) -> Object {
    let m = M_CREATE_FA.load(Ordering::Relaxed) as Method;
    if m.is_null() { return std::ptr::null_mut(); }
    let p = il2cpp::method_pointer(m);
    if p.is_null() { return std::ptr::null_mut(); }
    let f: extern "C" fn(Object, Method) -> Object = unsafe { std::mem::transmute(p) };
    f(font, m) // no `this` — static
}
// STATIC, (string name, i32 size, MethodInfo*) -> Font*  [Font.CreateDynamicFontFromOSFont(string,int)]
fn create_os_font(name: Object, size: i32) -> Object {
    let m = M_CREATE_OSFONT.load(Ordering::Relaxed) as Method;
    if m.is_null() { return std::ptr::null_mut(); }
    let p = il2cpp::method_pointer(m);
    if p.is_null() { return std::ptr::null_mut(); }
    let f: extern "C" fn(Object, i32, Method) -> Object = unsafe { std::mem::transmute(p) };
    f(name, size, m) // no `this` — static
}
// STATIC, (Object obj, MethodInfo*) -> bool
fn is_native_object_alive(obj: Object) -> bool {
    let m = M_IS_ALIVE.load(Ordering::Relaxed) as Method;
    if m.is_null() { return false; }
    let p = il2cpp::method_pointer(m);
    if p.is_null() { return false; }
    let f: extern "C" fn(Object, Method) -> bool = unsafe { std::mem::transmute(p) };
    f(obj, m)
}

// ── load chain (GCHandle caches + IsNativeObjectAlive revalidation) ──
fn load_extra_bundle(ld: &LocalizedData) -> Object {
    let mut h = EXTRA_BUNDLE.lock().unwrap();
    if let Some(g) = h.as_ref() {
        let b = g.target();
        if !b.is_null() { return b; }
        *h = None;
    }
    let Some(rel) = ld.config.extra_asset_bundle.get() else { return std::ptr::null_mut(); };
    let Some(path) = ld.get_data_path(rel) else { return std::ptr::null_mut(); };
    let Some(path_str) = path.to_str() else { log("[loc_font] bad extra bundle path"); return std::ptr::null_mut(); };
    let bundle = crate::assets::load_from_file_orig(il2cpp::new_string(path_str), 0, 0);
    if bundle.is_null() { log("[loc_font] failed to load extra asset bundle"); return std::ptr::null_mut(); }
    *h = Some(GCHandle::new(bundle, false));
    bundle
}

fn load_replacement_font(ld: &LocalizedData) -> Object {
    let mut h = FONT_HANDLE.lock().unwrap();
    if let Some(g) = h.as_ref() {
        let font = g.target();
        if !font.is_null() && is_native_object_alive(font) { return font; }
        log("[loc_font] font destroyed; reloading");
        *h = None;
    }
    let Some(name) = ld.config.replacement_font_name.as_deref() else { return std::ptr::null_mut(); };
    let bundle = load_extra_bundle(ld);
    if bundle.is_null() { return std::ptr::null_mut(); }
    let ty = FONT_TYPE_OBJECT.load(Ordering::Relaxed) as Object;
    let font = crate::assets::load_asset_orig(bundle, il2cpp::new_string(name), ty);
    if font.is_null() { log("[loc_font] failed to load replacement font"); return std::ptr::null_mut(); }
    set_hide_flags(font, HIDE_FLAGS_DONT_UNLOAD_UNUSED_ASSET);
    *h = Some(GCHandle::new(font, false));
    font
}

fn load_tmp_replacement_font(ld: &LocalizedData) -> Object {
    let mut h = TMP_HANDLE.lock().unwrap();
    if let Some(g) = h.as_ref() {
        let t = g.target();
        if !t.is_null() && is_native_object_alive(t) { return t; }
        log("[loc_font] TMP font destroyed; reloading");
        *h = None;
    }
    let font = load_replacement_font(ld);
    if font.is_null() { return std::ptr::null_mut(); }
    let tmp = create_font_asset(font);
    if tmp.is_null() { log("[loc_font] CreateFontAsset failed"); return std::ptr::null_mut(); }
    set_hide_flags(font, HIDE_FLAGS_DONT_UNLOAD_UNUSED_ASSET);
    *h = Some(GCHandle::new(tmp, false));
    tmp
}

// ── OS dynamic font (Part 2): no pack, script-driven glyph coverage ──────────────────────────────

/// Map a target-language code to a Windows OS font family covering its script, or None when the
/// game's own font already renders it correctly (Japanese) — in which case no swap is applied.
/// `lang` is the settings::tl_lang() code (currently 2-letter; the region checks are future-proofing).
fn os_font_for_lang(lang: &str) -> Option<&'static str> {
    match lang.split(['-', '_']).next().unwrap_or(lang) {
        "ko" => Some("Malgun Gothic"),                       // Hangul
        "zh" => Some(if lang.contains("Hant") || lang.contains("TW") || lang.contains("HK") {
            "Microsoft JhengHei"                             // Traditional
        } else {
            "Microsoft YaHei"                                // Simplified
        }),
        "th" => Some("Leelawadee UI"),                       // Thai
        "ja" => None,                                        // game font already correct → no swap
        // English is Latin — EVERY game font (JP or Global) already covers it, so DON'T swap: on the
        // JP client "en" is a valid target, and swapping to a CJK-less font would tofu any
        // not-yet-translated Japanese on the same components. No swap keeps untranslated JP intact.
        "en" => None,
        // Cyrillic / Greek / Arabic-script / Latin are all in Segoe UI's glyph set.
        // (ar/fa/he: glyphs present, but contextual shaping + RTL/BiDi are NOT solved here.)
        "ru" | "uk" | "bg" | "sr" | "be" | "mk"              // Cyrillic
        | "el"                                               // Greek
        | "ar" | "fa" | "he"                                 // RTL scripts (glyphs only)
        | "es" | "fr" | "de" | "it" | "pt" | "pl" | "tr" | "vi" => Some("Segoe UI"),
        _ => Some("Segoe UI"),                               // safe default: broad Latin+Cyrillic+Greek
    }
}

/// Drop the three GCHandle caches (bundle + Font + TMP asset) so the next Awake rebuilds. Does NOT
/// touch OS_FONT_LANG — callers manage that separately (load_os_font holds its lock; invalidate()
/// clears it after). Kept distinct from invalidate() to avoid re-locking OS_FONT_LANG under itself.
fn clear_handle_caches() {
    for cache in [&*EXTRA_BUNDLE, &*FONT_HANDLE, &*TMP_HANDLE] {
        if let Ok(mut g) = cache.lock() { *g = None; }
    }
}

/// Build (or revalidate) the OS dynamic Font for `lang`, cached in FONT_HANDLE and keyed by
/// OS_FONT_LANG so a live language switch rebuilds. None-mapped langs (ja) return null = no swap.
fn load_os_font(lang: &str) -> Object {
    // Rebuild when the requested lang differs from the cached one. Compare + store under the
    // OS_FONT_LANG lock, then clear the handle caches OUTSIDE it (clear_handle_caches locks
    // FONT/TMP; nesting those under OS_FONT_LANG — or calling invalidate() here — would deadlock).
    let changed = {
        let mut cur = OS_FONT_LANG.lock().unwrap();
        if cur.as_deref() != Some(lang) { *cur = Some(lang.to_string()); true } else { false }
    };
    if changed { clear_handle_caches(); }

    let Some(name) = os_font_for_lang(lang) else { return std::ptr::null_mut(); };
    let mut h = FONT_HANDLE.lock().unwrap();
    if let Some(g) = h.as_ref() {
        let font = g.target();
        if !font.is_null() && is_native_object_alive(font) { return font; }
        log("[loc_font] OS font destroyed; reloading");
        *h = None;
    }
    let font = create_os_font(il2cpp::new_string(name), 48); // 48 = base sampling; UI.Text re-rasters per size
    if font.is_null() { log("[loc_font] CreateDynamicFontFromOSFont returned null"); return std::ptr::null_mut(); }
    set_hide_flags(font, HIDE_FLAGS_DONT_UNLOAD_UNUSED_ASSET);
    *h = Some(GCHandle::new(font, false));
    font
}

/// TMP_FontAsset built from the OS dynamic Font, cached in TMP_HANDLE. The Font is resolved FIRST
/// (its own lock released) BEFORE locking TMP_HANDLE, so a lang-change clear inside load_os_font can
/// never re-enter this lock.
fn load_os_tmp_font(lang: &str) -> Object {
    let font = load_os_font(lang);
    if font.is_null() { return std::ptr::null_mut(); }
    let mut h = TMP_HANDLE.lock().unwrap();
    if let Some(g) = h.as_ref() {
        let t = g.target();
        if !t.is_null() && is_native_object_alive(t) { return t; }
        log("[loc_font] OS TMP font destroyed; reloading");
        *h = None;
    }
    let tmp = create_font_asset(font);
    if tmp.is_null() { log("[loc_font] CreateFontAsset(OS font) returned null"); return std::ptr::null_mut(); }
    *h = Some(GCHandle::new(tmp, false));
    tmp
}

// ── Overflow remediation (Part 1): wrap + shrink-to-fit, never truncate/hide ─────────────────────

/// UI.Text: wrap horizontally, then shrink-to-fit down to a floor if best-fit survived stripping,
/// else at least let wrapped lines overflow vertically instead of clipping.
fn apply_ui_overflow(this: Object) {
    text_set_ho(this, 0); // HorizontalWrapMode.Wrap (0) — opposite of the pack's 1=Overflow
    if M_TEXT_SET_BESTFIT.load(Ordering::Relaxed) != 0 {
        if M_TEXT_SET_MINSIZE.load(Ordering::Relaxed) != 0 { text_set_minsize(this, UI_TEXT_MIN_SIZE); }
        text_set_bestfit(this, true); // shrink to fit, down to the floor
    } else {
        text_set_vo(this, 1); // no best-fit → VerticalWrapMode.Overflow (1) so wrapped lines aren't clipped
    }
}

/// TMP: word-wrap, then auto-size DOWN only (clamp max to the current size so short text never grows).
/// overflowMode is left at its default (Overflow) so text is never hidden.
fn apply_tmp_overflow(this: Object) {
    // word wrap: prefer the older bool API, fall back to the TMP 3.x enum API
    if M_TMP_SET_WORDWRAP.load(Ordering::Relaxed) != 0 {
        tmp_set_wordwrap(this, true);
    } else if M_TMP_SET_WRAPMODE.load(Ordering::Relaxed) != 0 {
        tmp_set_wrapmode(this, 1); // TextWrappingModes.Normal
    }
    // auto-size DOWN to a floor, never up — only when the whole knob set survived stripping
    if M_TMP_SET_AUTOSIZE.load(Ordering::Relaxed) != 0
        && M_TMP_SET_FSMIN.load(Ordering::Relaxed) != 0
        && M_TMP_GET_FONTSIZE.load(Ordering::Relaxed) != 0
    {
        let cur = tmp_get_fontsize(this);
        if cur > 0.0 {
            if M_TMP_SET_FSMAX.load(Ordering::Relaxed) != 0 { tmp_set_fsmax(this, cur); } // never grow
            tmp_set_fsmin(this, (cur * TMP_MIN_RATIO).max(TMP_MIN_SIZE));
            tmp_set_autosize(this, true);
        }
    }
}

// ── the two Awake detours (managed, argc 0, instance; forward trailing MethodInfo*) ──
static TR_TC: AtomicUsize = AtomicUsize::new(0);
static D_TC: OnceLock<RawDetour> = OnceLock::new();
static TR_TMP: AtomicUsize = AtomicUsize::new(0);
static D_TMP: OnceLock<RawDetour> = OnceLock::new();
type AwakeFn = unsafe extern "C" fn(Object, Method);

unsafe extern "C" fn h_text_common_awake(this: Object, mi: Method) {
    let t = TR_TC.load(Ordering::Relaxed);
    if t != 0 { let f: AwakeFn = std::mem::transmute(t); f(this, mi); } // orig FIRST
    let ld = crate::localize::data();
    // ── PACK tier-0 wins: an explicit replacement font is applied verbatim, and its allow_overflow
    //    toggle still takes precedence over MTL wrap/best-fit for UI.Text (unchanged behaviour). ──
    if ld.config.replacement_font_name.is_some() {
        let font = load_replacement_font(&ld);
        if !font.is_null() { text_set_font(this, font); }
        if ld.config.text_common_allow_overflow {
            text_set_ho(this, 1); // HorizontalWrapMode.Overflow
            text_set_vo(this, 1); // VerticalWrapMode.Overflow
        }
        return;
    }
    // MTL (no-pack) global-Awake swap REVERTED 2026-07-21. Applying a font + overflow to EVERY
    // component here also restyled Latin/numeric UI (wrong font AND size) — the swap must follow the
    // TRANSLATED non-Latin text, not blanket every component. The per-translation path lives in
    // loc_settext (apply only where a non-Latin translation is actually set). Zero-overhead otherwise.
}
unsafe extern "C" fn h_tmp_common_awake(this: Object, mi: Method) {
    let t = TR_TMP.load(Ordering::Relaxed);
    if t != 0 { let f: AwakeFn = std::mem::transmute(t); f(this, mi); }
    let ld = crate::localize::data();
    // PACK tier-0 wins (the TMP path has no allow_overflow branch historically — keep as-is).
    if ld.config.replacement_font_name.is_some() {
        let tmp = load_tmp_replacement_font(&ld);
        if !tmp.is_null() { tmp_set_font(this, tmp); }
        return;
    }
    // MTL (no-pack) global-Awake swap REVERTED 2026-07-21 — see h_text_common_awake. Blanketing every
    // TMP component restyled Latin text; the correct swap is per-translation (loc_settext).
}

/// Resolve helpers + install both Awake hooks. Call from boot AFTER assets::install() (needs the
/// AssetBundle trampolines). Non-fatal per hook — cede if pre-detoured (hook_method already does).
pub fn install() -> Result<(), String> {
    let font_cls = il2cpp::class("UnityEngine.Font");
    let font_type = il2cpp::type_object(font_cls);
    if font_type.is_null() { return Err("UnityEngine.Font type object null".into()); }
    FONT_TYPE_OBJECT.store(font_type as usize, Ordering::Relaxed);

    let text  = il2cpp::class("UnityEngine.UI.Text");
    let tmp   = il2cpp::class("TMPro.TMP_Text");
    let tmpfa = il2cpp::class("TMPro.TMP_FontAsset");
    let obj   = il2cpp::class("UnityEngine.Object");
    if text.is_null() || tmp.is_null() || tmpfa.is_null() || obj.is_null() {
        return Err("font helper classes not resolved".into());
    }
    M_TEXT_SET_FONT.store(il2cpp::method(text,  "set_font", 1) as usize, Ordering::Relaxed);
    M_TEXT_SET_HO.store(il2cpp::method(text,    "set_horizontalOverflow", 1) as usize, Ordering::Relaxed);
    M_TEXT_SET_VO.store(il2cpp::method(text,    "set_verticalOverflow", 1) as usize, Ordering::Relaxed);
    M_TMP_SET_FONT.store(il2cpp::method(tmp,    "set_font", 1) as usize, Ordering::Relaxed);
    M_CREATE_FA.store(il2cpp::method(tmpfa,     "CreateFontAsset", 1) as usize, Ordering::Relaxed);
    M_SET_HIDEFLAGS.store(il2cpp::method(obj,   "set_hideFlags", 1) as usize, Ordering::Relaxed);
    M_IS_ALIVE.store(il2cpp::method(obj,        "IsNativeObjectAlive", 1) as usize, Ordering::Relaxed);
    for (n, v) in [("Text.set_font", &M_TEXT_SET_FONT), ("TMP.set_font", &M_TMP_SET_FONT),
                   ("CreateFontAsset", &M_CREATE_FA), ("set_hideFlags", &M_SET_HIDEFLAGS),
                   ("IsNativeObjectAlive", &M_IS_ALIVE)] {
        if v.load(Ordering::Relaxed) == 0 { return Err(format!("font helper missing: {n}")); }
    }

    // ── Part 1: overflow helpers — NON-FATAL. IL2CPP strips methods no managed code references, so
    //    any of these can be null on the live build; each call site is null-guarded. Log one line per
    //    handle so the actual stripping surface is visible in overseer-native.log. ──
    M_TEXT_SET_BESTFIT.store(il2cpp::method(text, "set_resizeTextForBestFit", 1) as usize, Ordering::Relaxed);
    M_TEXT_SET_MINSIZE.store(il2cpp::method(text, "set_resizeTextMinSize", 1) as usize, Ordering::Relaxed);
    M_TMP_SET_AUTOSIZE.store(il2cpp::method(tmp,  "set_enableAutoSizing", 1) as usize, Ordering::Relaxed);
    M_TMP_SET_WORDWRAP.store(il2cpp::method(tmp,  "set_enableWordWrapping", 1) as usize, Ordering::Relaxed);
    M_TMP_SET_WRAPMODE.store(il2cpp::method(tmp,  "set_textWrappingMode", 1) as usize, Ordering::Relaxed);
    M_TMP_SET_OVERFLOW.store(il2cpp::method(tmp,  "set_overflowMode", 1) as usize, Ordering::Relaxed);
    M_TMP_SET_FSMIN.store(il2cpp::method(tmp,     "set_fontSizeMin", 1) as usize, Ordering::Relaxed);
    M_TMP_SET_FSMAX.store(il2cpp::method(tmp,     "set_fontSizeMax", 1) as usize, Ordering::Relaxed);
    M_TMP_GET_FONTSIZE.store(il2cpp::method(tmp,  "get_fontSize", 0) as usize, Ordering::Relaxed);
    for (n, v) in [
        ("UI.Text.set_resizeTextForBestFit", &M_TEXT_SET_BESTFIT),
        ("UI.Text.set_resizeTextMinSize",    &M_TEXT_SET_MINSIZE),
        ("TMP.set_enableAutoSizing",         &M_TMP_SET_AUTOSIZE),
        ("TMP.set_enableWordWrapping",       &M_TMP_SET_WORDWRAP),
        ("TMP.set_textWrappingMode",         &M_TMP_SET_WRAPMODE),
        ("TMP.set_overflowMode",             &M_TMP_SET_OVERFLOW),
        ("TMP.set_fontSizeMin",              &M_TMP_SET_FSMIN),
        ("TMP.set_fontSizeMax",              &M_TMP_SET_FSMAX),
        ("TMP.get_fontSize",                 &M_TMP_GET_FONTSIZE),
    ] {
        log(&format!("[loc_font] overflow helper {n} -> {}",
            if v.load(Ordering::Relaxed) != 0 { "resolved" } else { "NULL (stripped)" }));
    }

    // ── Part 2: OS dynamic font. Resolve the (string,int) overload EXPLICITLY to avoid the string[]
    //    sibling; both may be stripped (the game never calls either). If present, do a functional
    //    probe on this attached boot thread: a throwaway Segoe UI Font (→ OS_FONT_OK) and a TMP asset
    //    from it (→ TMP_OS_OK, tracked separately so UI.Text can arm while TMP degrades). ──
    let os_m = il2cpp::method_overload(font_cls, "CreateDynamicFontFromOSFont",
                                       &[il2cpp::IL2CPP_TYPE_STRING, il2cpp::IL2CPP_TYPE_I4]);
    if os_m.is_null() {
        OS_FONT_OK.store(false, Ordering::Relaxed);
        TMP_OS_OK.store(false, Ordering::Relaxed);
        log("[loc_font] CreateDynamicFontFromOSFont(string,int) -> NULL (stripped); OS dynamic font disabled");
        crate::diag::record_install("loc_font_os", "stripped (CreateDynamicFontFromOSFont absent)");
    } else {
        M_CREATE_OSFONT.store(os_m as usize, Ordering::Relaxed);
        let probe = create_os_font(il2cpp::new_string("Segoe UI"), 48);
        let ui_ok = !probe.is_null();
        let tmp_ok = ui_ok && !create_font_asset(probe).is_null();
        OS_FONT_OK.store(ui_ok, Ordering::Relaxed);
        TMP_OS_OK.store(tmp_ok, Ordering::Relaxed);
        let msg = if !ui_ok { "probe: OS Font null; disabled" }
                  else if tmp_ok { "armed (OS dynamic font; TMP ok)" }
                  else { "armed (UI.Text only, TMP null)" };
        log(&format!("[loc_font] OS dynamic font probe -> {msg}"));
        crate::diag::record_install("loc_font_os", msg);
    }

    let tc = il2cpp::class("Gallop.TextCommon");
    if !tc.is_null() {
        unsafe { let _ = il2cpp::hook_method(tc, "Awake", 0, h_text_common_awake as *const (), &TR_TC, &D_TC); }
    }
    let tmpc = il2cpp::class("Gallop.TextMeshProUguiCommon");
    if !tmpc.is_null() {
        unsafe { let _ = il2cpp::hook_method(tmpc, "Awake", 0, h_tmp_common_awake as *const (), &TR_TMP, &D_TMP); }
    }
    Ok(())
}

/// Drop the cached font handles so the next Awake reloads. Call on pack hot-reload (localize::reload)
/// or a target-language change — IsNativeObjectAlive only catches native destruction, not a config
/// change to replacement_font_name or tl_lang. Also clears OS_FONT_LANG so the OS font rebuilds.
pub fn invalidate() {
    clear_handle_caches();
    if let Ok(mut g) = OS_FONT_LANG.lock() { *g = None; }
}