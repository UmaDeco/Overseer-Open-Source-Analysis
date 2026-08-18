//! Managed-exception tracer — makes invisible C# exceptions visible.
//!
//! ## Why
//!
//! The Scout (gacha) tab does nothing when translation is active. Bisected live: translation is
//! necessary and sufficient; UI tempo is irrelevant; the other footer tabs work; the click reaches
//! `ButtonCommon.OnPointerClick`; the gacha view is never constructed; the main thread never stops
//! ticking (115 FPS throughout). A silently-swallowed managed exception fits every one of those facts
//! — and we cannot see it, because the game suppresses Unity's logger and `Player.log` stays 0 bytes.
//!
//! ## How (and why it's done THIS way)
//!
//! We hook `System.Exception..ctor`, NOT `il2cpp_raise_exception`. That matters:
//!
//! * `il2cpp_raise_exception` is `noreturn` — it throws a C++ exception. Detouring it would force a
//!   foreign exception to unwind through our Rust frame AND through retour's trampoline, which is
//!   heap-allocated code with no `RUNTIME_FUNCTION` unwind data. That risks crashing the game
//!   outright, which is a worse bug than the one we're chasing.
//! * A constructor, by contrast, RETURNS NORMALLY. The object is built, we log it, we return, and the
//!   throw happens afterwards in the caller. Nothing unwinds through us. No new failure mode.
//!
//! Every exception type chains to one of `Exception`'s base constructors, so hooking those catches
//! the whole hierarchy — including the `FormatException` we expect from a mangled format string.
//!
//! Pure observation: read two fields, write a log line, call the original. Off unless armed, capped
//! so it can never flood, and deduped so a per-frame thrower can't drown the interesting one.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use crate::il2cpp::{self, Method, Object};
// AtomicUsize / OnceLock / RawDetour are referenced only inside the `skip_hook_slot!` expansion
// (fully qualified there), so they need no `use` here.

static ARMED: AtomicBool = AtomicBool::new(false);
static LEFT: AtomicI64 = AtomicI64::new(0);
const CAP: i64 = 300;

crate::skip_hook_slot!(TR_EX0, D_EX0); // Exception..ctor()
crate::skip_hook_slot!(TR_EX1, D_EX1); // Exception..ctor(string)
crate::skip_hook_slot!(TR_EX2, D_EX2); // Exception..ctor(string, Exception)

/// Seen (type, message) pairs — a thrower inside a per-frame loop would otherwise bury everything.
static SEEN: std::sync::Mutex<Option<std::collections::HashSet<String>>> = std::sync::Mutex::new(None);

pub fn set_armed(on: bool) {
    if on {
        LEFT.store(CAP, Ordering::Relaxed);
        if let Ok(mut g) = SEEN.lock() {
            *g = Some(std::collections::HashSet::new());
        }
    }
    ARMED.store(on, Ordering::Relaxed);
    crate::tools::log(if on {
        "[extrace] managed-exception tracer ARMED — every distinct C# exception will be logged"
    } else {
        "[extrace] managed-exception tracer off"
    });
}

fn report(this: Object, msg: Object) {
    if !ARMED.load(Ordering::Relaxed) {
        return;
    }
    let ty = il2cpp::object_class_name(this);
    let text = if msg.is_null() { String::new() } else { il2cpp::read_string(msg) };
    let key = format!("{ty}|{text}");
    match SEEN.lock() {
        Ok(mut g) => {
            if !g.get_or_insert_with(std::collections::HashSet::new).insert(key) {
                return; // already reported this exact exception
            }
        }
        Err(_) => return,
    }
    if LEFT.fetch_sub(1, Ordering::Relaxed) <= 0 {
        ARMED.store(false, Ordering::Relaxed);
        crate::tools::log("[extrace] cap reached — tracer disabled");
        return;
    }
    crate::tools::log(&format!("[extrace] !! {ty}: {text}"));
}

unsafe extern "C" fn on_ctor0(this: Object, mi: Method) {
    report(this, std::ptr::null_mut());
    let t = TR_EX0.load(Ordering::Relaxed);
    if t != 0 {
        let f: unsafe extern "C" fn(Object, Method) = std::mem::transmute(t);
        f(this, mi);
    }
}
unsafe extern "C" fn on_ctor1(this: Object, msg: Object, mi: Method) {
    report(this, msg);
    let t = TR_EX1.load(Ordering::Relaxed);
    if t != 0 {
        let f: unsafe extern "C" fn(Object, Object, Method) = std::mem::transmute(t);
        f(this, msg, mi);
    }
}
unsafe extern "C" fn on_ctor2(this: Object, msg: Object, inner: Object, mi: Method) {
    report(this, msg);
    let t = TR_EX2.load(Ordering::Relaxed);
    if t != 0 {
        let f: unsafe extern "C" fn(Object, Object, Object, Method) = std::mem::transmute(t);
        f(this, msg, inner, mi);
    }
}

/// Hook the `System.Exception` base constructors. Non-fatal: a miss just means less visibility.
pub fn install() -> Result<(), String> {
    let ex = il2cpp::class("System.Exception");
    if ex.is_null() {
        return Err("System.Exception not found".into());
    }
    let mut armed: Vec<&str> = Vec::new();
    unsafe {
        let m0 = il2cpp::method(ex, ".ctor", 0);
        if !m0.is_null() && il2cpp::hook_at(m0, "Exception..ctor()", on_ctor0 as *const (), &TR_EX0, &D_EX0).is_ok() {
            armed.push("()");
        }
        let m1 = il2cpp::method(ex, ".ctor", 1);
        if !m1.is_null() && il2cpp::hook_at(m1, "Exception..ctor(string)", on_ctor1 as *const (), &TR_EX1, &D_EX1).is_ok() {
            armed.push("(string)");
        }
        let m2 = il2cpp::method(ex, ".ctor", 2);
        if !m2.is_null() && il2cpp::hook_at(m2, "Exception..ctor(string,Exception)", on_ctor2 as *const (), &TR_EX2, &D_EX2).is_ok() {
            armed.push("(string,Exception)");
        }
    }
    crate::tools::log(&format!("[extrace] Exception ctors hooked: {}", armed.join(" ")));
    Ok(())
}
