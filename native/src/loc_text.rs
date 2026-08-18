//! Wave 6 — TextGenerator hashed-dict localization (Tier 1). Hooks
//! UnityEngine.TextGenerator::PopulateWithErrors (managed, argc 3, instance):
//!   bool PopulateWithErrors(string str, TextGenerationSettings settings, GameObject context)
//! `settings` is 96 bytes -> win64 passes it BY HIDDEN POINTER (like Rect in assets/bindings.rs).
//! We never read/mutate it, so it is an OPAQUE 96-byte aligned blob: size+align alone give the
//! correct by-value ABI without depending on the interior field order. Swap the string on a
//! hashed_dict hit; forward everything (incl. settings + trailing MethodInfo*) verbatim.
#![allow(non_snake_case, dead_code)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use retour::RawDetour;
use crate::il2cpp::{self, Method, Object};

/// Opaque mirror of TextGenerationSettings. VERIFY sizeof==96 against the target il2cpp dump before
/// enabling — a wrong size corrupts the forwarded call. Named layout for Tier 2 is in the spec.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
struct TextGenerationSettings_t { _bytes: [u8; 96] }

type PopulateFn =
    unsafe extern "C" fn(Object, Object, TextGenerationSettings_t, Object, Method) -> bool;
static TR: AtomicUsize = AtomicUsize::new(0);
static D: OnceLock<RawDetour> = OnceLock::new();

unsafe extern "C" fn h_populate(
    this: Object, str_: Object, settings: TextGenerationSettings_t, context: Object, mi: Method,
) -> bool {
    let orig = |s: Object| -> bool {
        let t = TR.load(Ordering::Relaxed);
        if t == 0 { return false; }
        let f: PopulateFn = std::mem::transmute(t);
        f(this, s, settings, context, mi) // settings forwarded verbatim (Copy blob)
    };
    let ld = crate::localize::data();
    if ld.hashed_dict.is_empty() { return orig(str_); } // no pack hashes -> zero overhead
    let h = il2cpp::string_fnv_hash(str_);
    if let Some(text) = ld.hashed_dict.get(&h) {
        let s = il2cpp::new_string(text);
        if !s.is_null() { return orig(s); }
    }
    orig(str_)
}

/// INTENTIONALLY DISARMED. The opaque `[u8;96]` `settings` blob is only ABI-safe if
/// `sizeof(TextGenerationSettings)==96` on the shipping build — which needs an in-game il2cpp dump to
/// confirm — and this hook fires on EVERY uGUI text generation (a wrong size crashes constantly). Its
/// value (hashed_dict full-string replacements) is minor since Localize.Get + the DB hooks already
/// cover the bulk of UI text. To enable after verifying sizeof==96, restore the commented body.
pub fn install() -> Result<(), String> {
    // let tg = il2cpp::class("UnityEngine.TextGenerator");
    // if tg.is_null() { return Err("UnityEngine.TextGenerator not found".into()); }
    // unsafe { il2cpp::hook_method(tg, "PopulateWithErrors", 3, h_populate as *const (), &TR, &D) }
    Err("disabled pending TextGenerationSettings size verification".into())
}