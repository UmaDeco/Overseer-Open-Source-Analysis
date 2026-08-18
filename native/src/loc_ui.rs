//! Wave 2 — builtin UI-string localization via `Gallop.Localize::Get` (ported from the upstream
//! localization framework's Localize/TextId hooks).
//!
//! `Localize::Get(TextId id)` is the game's lookup for almost all builtin UI text. The devs reorder
//! the `TextId` enum between versions, so numeric ids are volatile — we map each id to its STABLE
//! enum-member NAME (via `Enum.ToObject` → `Enum.ToString`) and serve `localize_dict[name]` from the
//! active pack. This is a classic single static-method detour; it does NOT touch the AssetBundle
//! dispatcher.
//!
//! ABI (verified): `Get` is STATIC → detour is `(i32 id, MethodInfo*)` with NO `this`; a bogus
//! `this` would shift every arg and crash. It is resolved by the `[VALUETYPE]` overload (not by
//! arity, which could grab a sibling `Get`), on the JP nested class for JP clients. `Enum.ToObject`
//! is the `[Type, Int32]` overload; all managed calls pass the trailing `MethodInfo*`.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use fnv::FnvHashMap;
use once_cell::sync::Lazy;
use retour::RawDetour;

use crate::il2cpp::{self, Method, Object};

fn log(m: &str) {
    crate::tools::log(m);
}

/// JP client detection by the game executable name (mirrors the upstream region gate): the JP build is
/// `umamusume.exe` / `…_jpn.exe`; the Global/Steam build is `UmamusumePrettyDerby.exe`. Determines
/// which `Localize.Get` is the live one (JP localizes via the nested `Localize.JP`). Cached — the
/// exe path never changes, and this is read on the per-set_text hot path (MTL source direction).
pub(crate) fn is_jp_client() -> bool {
    static JP: OnceLock<bool> = OnceLock::new();
    *JP.get_or_init(|| {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_ascii_lowercase()))
            .unwrap_or_default();
        exe == "umamusume.exe" || exe.contains("jpn")
    })
}

static TR_GET: AtomicUsize = AtomicUsize::new(0);
static D_GET: OnceLock<RawDetour> = OnceLock::new();

// TextId → name resolution (resolved at install()).
static TEXTID_TYPE_OBJECT: AtomicUsize = AtomicUsize::new(0); // Il2CppObject* System.Type of TextId
static M_TOOBJECT: AtomicUsize = AtomicUsize::new(0); // Enum.ToObject(Type, Int32) — static
static M_TOSTRING: AtomicUsize = AtomicUsize::new(0); // Enum.ToString() — instance, argc 0

/// id → enum-member name. `Get` is main-thread-only, but a Mutex keeps this sound at negligible
/// (uncontended) cost rather than relying on the invariant.
static NAME_CACHE: Lazy<Mutex<FnvHashMap<i32, String>>> = Lazy::new(|| Mutex::new(FnvHashMap::default()));

type GetFn = unsafe extern "C" fn(i32, Method) -> Object;

/// `Enum.ToObject(TextId_type, id)` — box the i32 as its TextId enum value.
unsafe fn enum_to_object(type_obj: Object, id: i32) -> Object {
    let m = M_TOOBJECT.load(Ordering::Relaxed) as Method;
    if m.is_null() {
        return std::ptr::null_mut();
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return std::ptr::null_mut();
    }
    let f: extern "C" fn(Object, i32, Method) -> Object = std::mem::transmute(p);
    f(type_obj, id, m)
}

/// `boxed.ToString()` — the enum member name as a managed string.
unsafe fn enum_to_string(boxed: Object) -> Object {
    let m = M_TOSTRING.load(Ordering::Relaxed) as Method;
    if m.is_null() {
        return std::ptr::null_mut();
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return std::ptr::null_mut();
    }
    let f: extern "C" fn(Object, Method) -> Object = std::mem::transmute(p);
    f(boxed, m)
}

/// Resolve (and cache) the stable enum-member name for `id`.
fn text_id_name(id: i32) -> Option<String> {
    if let Ok(cache) = NAME_CACHE.lock() {
        if let Some(n) = cache.get(&id) {
            return Some(n.clone());
        }
    }
    let type_obj = TEXTID_TYPE_OBJECT.load(Ordering::Relaxed) as Object;
    if type_obj.is_null() {
        return None;
    }
    let name = unsafe {
        let boxed = enum_to_object(type_obj, id);
        if boxed.is_null() {
            return None;
        }
        let name_str = enum_to_string(boxed);
        if name_str.is_null() {
            return None;
        }
        il2cpp::read_string(name_str)
    };
    if let Ok(mut cache) = NAME_CACHE.lock() {
        cache.insert(id, name.clone());
    }
    Some(name)
}

unsafe extern "C" fn h_get(id: i32, mi: Method) -> Object {
    let tr = TR_GET.load(Ordering::Relaxed);
    let orig = |id: i32, mi: Method| -> Object {
        if tr != 0 {
            let f: GetFn = std::mem::transmute(tr);
            f(id, mi)
        } else {
            std::ptr::null_mut()
        }
    };

    // The pack tier is a TRANSLATION surface, so it follows the translation switch exactly like the
    // MTL tier below. Without this, turning translation off still swapped every packed UI string —
    // which is precisely the "the toggle doesn't really turn it off" complaint.
    if !crate::runtime::active(crate::runtime::Subsystem::Translation) {
        return orig(id, mi);
    }
    let ld = crate::localize::data();
    // 1. Translation pack: map the volatile id → stable TextId name → localize_dict. Exit through
    //    sql::pack_string so pack text gets its $(plural/ordinal/month) templates expanded and is
    //    marked as our own output (same rules as every other pack surface).
    if !ld.localize_dict.is_empty() {
        if let Some(name) = text_id_name(id) {
            // Diagnostic (rides the text trace): log id -> TextId name so a pack author can harvest
            // the exact keys this build asks for — localize_dict is keyed by these names.
            if crate::loc_settext::trace_on() {
                crate::tools::log(&format!("[ptrace] TextId {id} = {name:?}"));
            }
            if let Some(text) = ld.localize(&name) {
                let s = crate::sql::pack_string(text);
                if !s.is_null() {
                    return s;
                }
            }
        }
    }
    // 2. Dynamic translation fallback (works with or without a pack). Gated on tl_lang (not glossary
    //    activity) so the JP client reaches the MTL cache too. Zero overhead when no language is set.
    let orig_str = orig(id, mi);
    if !orig_str.is_null() && crate::mtl::translation_active() {
        let english = il2cpp::read_string(orig_str);
        // Diagnostic (armed with the text trace): Localize.Get is the OTHER translation-gated hook,
        // and it runs during view SETUP — before any set_text. The Scout view dies without setting a
        // single string, so if translation kills it, it most likely dies on a value we returned here.
        let traced = crate::loc_settext::trace_on();
        if traced {
            crate::tools::log(&format!("[ltrace] Localize.Get({id}) = {english:?}"));
        }
        // Protected proper names (skill/character/race names) stay English.
        if crate::mtl::is_protected_name(&english) {
            return orig_str;
        }
        // 2a. Glossary (instant). record_emitted so the set_text hook doesn't re-translate our own
        //     output (the same string is later pushed into a text component) — target-language text
        //     re-fed as a source would garble.
        //
        //     FINAL results only — the same rule dispatch tier-1 has. Without it, a word-substitution
        //     partial ("Skip Settings" → "Passer Settings": glossary knew "Skip", not "Settings") was
        //     returned to the game AND record_emitted, which LOCKED the franglais in: every set_text
        //     hook saw it as our own output and forwarded it untouched, so it could never reach NLLB.
        //     That is exactly the frozen half-translated career Options dialog ("Passer First-Time
        //     Events", half-English explanation paragraphs). A non-final partial now falls through to
        //     the cache/NMT tiers with the ORIGINAL text, and the set_text hook tracks the component
        //     for re-apply once the full translation arrives.
        if let Some(translated) =
            crate::glossary::translate(&english).filter(|t| crate::mtl::glossary_result_final(&english, t))
        {
            if traced {
                crate::tools::log(&format!("[ltrace]   -> GLOSSARY({id}) {english:?} => {translated:?}"));
            }
            let s = il2cpp::new_string(&translated);
            if !s.is_null() {
                crate::loc_settext::record_emitted(&translated);
                return s;
            }
        }
        // 2b. MTL cache (instant); else seed the worker. Localize.Get owns no component, so it only
        //     seeds — the set_text path performs the actual on-screen re-apply.
        if let Some(t) = crate::mtl::lookup(&english) {
            if traced {
                crate::tools::log(&format!("[ltrace]   -> CACHE({id}) {english:?} => {t:?}"));
            }
            let s = il2cpp::new_string(&t);
            if !s.is_null() {
                crate::loc_settext::record_emitted(&t);
                return s;
            }
        } else {
            crate::mtl::request(&english);
        }
    }
    orig_str
}

/// Resolve + hook `Localize::Get`. Non-fatal on any miss (returns Err → the wave is disabled, the
/// game runs untranslated). Call from boot on the IL2CPP-attached thread.
pub fn install() -> Result<(), String> {
    // TextId type object.
    let textid = il2cpp::class("Gallop.TextId");
    if textid.is_null() {
        return Err("Gallop.TextId not found".into());
    }
    let type_obj = il2cpp::type_object(textid);
    if type_obj.is_null() {
        return Err("TextId type object null".into());
    }
    TEXTID_TYPE_OBJECT.store(type_obj as usize, Ordering::Relaxed);

    // Enum.ToObject(Type, Int32) + Enum.ToString().
    let enum_cls = il2cpp::class("System.Enum");
    if enum_cls.is_null() {
        return Err("System.Enum not found".into());
    }
    let to_object = il2cpp::method_overload(enum_cls, "ToObject", &[il2cpp::IL2CPP_TYPE_CLASS, il2cpp::IL2CPP_TYPE_I4]);
    let to_string = il2cpp::method(enum_cls, "ToString", 0);
    if to_object.is_null() || to_string.is_null() {
        return Err("Enum.ToObject/ToString not resolved".into());
    }
    M_TOOBJECT.store(to_object as usize, Ordering::Relaxed);
    M_TOSTRING.store(to_string as usize, Ordering::Relaxed);

    // Localize.Get(TextId) — resolve by the single-valuetype overload; JP clients use nested Localize.JP.
    let localize = il2cpp::class("Gallop.Localize");
    if localize.is_null() {
        return Err("Gallop.Localize not found".into());
    }
    // Region gate (matches the upstream implementation): JP localizes via nested Localize.JP.Get; Global/Steam via the
    // outer Localize.Get. Resolve by the single-valuetype (TextId) overload — arity alone can't tell
    // it from a sibling Get(string)/Get(int).
    let get_m = if is_jp_client() {
        let jp = il2cpp::nested_class("Gallop.Localize", "JP");
        il2cpp::method_overload(jp, "Get", &[il2cpp::IL2CPP_TYPE_VALUETYPE])
    } else {
        il2cpp::method_overload(localize, "Get", &[il2cpp::IL2CPP_TYPE_VALUETYPE])
    };
    if get_m.is_null() {
        return Err("Localize.Get(valuetype) overload not found".into());
    }
    if !il2cpp::method_is_static(get_m) {
        // Our detour ABI assumes no `this`; refuse rather than crash.
        return Err("Localize.Get is not static (unexpected ABI)".into());
    }
    unsafe {
        il2cpp::hook_at(get_m, "Localize.Get", h_get as *const (), &TR_GET, &D_GET)?;
    }
    Ok(())
}
