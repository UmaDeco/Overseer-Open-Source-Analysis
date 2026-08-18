//! IL2CPP bindings for the texture-localization pipeline (Phase 3).
//!
//! Resolved once from `init()` (called by `assets::install`). Two ABI classes, kept strictly
//! separate because mixing them crashes:
//!   * MANAGED methods (resolved by `il2cpp::method`) take the hidden trailing `MethodInfo*` — we
//!     pass the resolved `Method` handle as the last arg (the convention proven in
//!     `performance/graphics.rs`).
//!   * ICALLs (resolved by `il2cpp::resolve_icall`) are the raw engine functions and take NO
//!     `MethodInfo*`.
//! Calling one as the other yields a garbage pointer and crashes, so each symbol is resolved by
//! its exact kind and `init()` fails loud if a hot-path symbol is missing.

#![allow(non_snake_case, dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::il2cpp::{self, Class, Method, Object};

// ── win64 il2cpp struct layouts ──────────────────────────────────────────────
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
} // 16 bytes → passed by hidden pointer on win64 (see ReadPixels)

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Color32 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
} // 4 bytes, tight

/// UnityEngine.TextureFormat.RGBA32.
pub const TEXTURE_FORMAT_RGBA32: i32 = 4;

// MANAGED slots hold the `Method` handle (so we can pass it as MethodInfo*).
static M_CTOR: AtomicUsize = AtomicUsize::new(0); // Texture2D..ctor(5)
static M_READPIXELS: AtomicUsize = AtomicUsize::new(0); // Texture2D.ReadPixels(3)
static M_GETTEMP: AtomicUsize = AtomicUsize::new(0); // RenderTexture.GetTemporary(2) STATIC
static M_LOADIMAGE: AtomicUsize = AtomicUsize::new(0); // ImageConversion.LoadImage(3) STATIC ext
static M_GETTEXTURE: AtomicUsize = AtomicUsize::new(0); // Sprite.get_texture(0)
// ICALL slots hold the raw code address (no MethodInfo).
static I_GETW: AtomicUsize = AtomicUsize::new(0); // Texture::GetDataWidth()
static I_GETH: AtomicUsize = AtomicUsize::new(0); // Texture::GetDataHeight()
static I_BLIT2: AtomicUsize = AtomicUsize::new(0); // Graphics::Blit2 STATIC
static I_GETACTIVE: AtomicUsize = AtomicUsize::new(0); // RenderTexture::GetActive() STATIC
static I_SETACTIVE: AtomicUsize = AtomicUsize::new(0); // RenderTexture::SetActive STATIC
static I_RELTEMP: AtomicUsize = AtomicUsize::new(0); // RenderTexture::ReleaseTemporary STATIC
static I_GETPIX32: AtomicUsize = AtomicUsize::new(0); // Texture2D::GetPixels32(System.Int32)
static C_TEX2D: AtomicUsize = AtomicUsize::new(0);
static C_BYTE: AtomicUsize = AtomicUsize::new(0);

pub fn texture2d_class() -> Class {
    C_TEX2D.load(Ordering::Relaxed) as Class
}
pub fn byte_class() -> Class {
    C_BYTE.load(Ordering::Relaxed) as Class
}

// ── ICALL wrappers (raw addr; NO MethodInfo) ─────────────────────────────────
pub fn GetDataWidth(this: Object) -> i32 {
    let p = I_GETW.load(Ordering::Relaxed);
    if p == 0 {
        return 0;
    }
    (unsafe { std::mem::transmute::<usize, extern "C" fn(Object) -> i32>(p) })(this)
}
pub fn GetDataHeight(this: Object) -> i32 {
    let p = I_GETH.load(Ordering::Relaxed);
    if p == 0 {
        return 0;
    }
    (unsafe { std::mem::transmute::<usize, extern "C" fn(Object) -> i32>(p) })(this)
}
pub fn Blit2(src: Object, dst: Object) {
    let p = I_BLIT2.load(Ordering::Relaxed);
    if p == 0 {
        return;
    }
    (unsafe { std::mem::transmute::<usize, extern "C" fn(Object, Object)>(p) })(src, dst)
}
pub fn GetActive() -> Object {
    let p = I_GETACTIVE.load(Ordering::Relaxed);
    if p == 0 {
        return std::ptr::null_mut();
    }
    (unsafe { std::mem::transmute::<usize, extern "C" fn() -> Object>(p) })()
}
pub fn SetActive(rt: Object) {
    let p = I_SETACTIVE.load(Ordering::Relaxed);
    if p == 0 {
        return;
    }
    (unsafe { std::mem::transmute::<usize, extern "C" fn(Object)>(p) })(rt)
}
pub fn ReleaseTemporary(rt: Object) {
    let p = I_RELTEMP.load(Ordering::Relaxed);
    if p == 0 {
        return;
    }
    (unsafe { std::mem::transmute::<usize, extern "C" fn(Object)>(p) })(rt)
}
pub fn GetPixels32(this: Object, mip: i32) -> Object {
    let p = I_GETPIX32.load(Ordering::Relaxed);
    if p == 0 {
        return std::ptr::null_mut();
    }
    (unsafe { std::mem::transmute::<usize, extern "C" fn(Object, i32) -> Object>(p) })(this, mip)
}

// ── MANAGED wrappers (deref methodPointer, pass `m` as trailing MethodInfo*) ──
pub fn new_texture(w: i32, h: i32, fmt: i32, mip: bool, linear: bool) -> Object {
    let m = M_CTOR.load(Ordering::Relaxed) as Method;
    if m.is_null() {
        return std::ptr::null_mut();
    }
    let this = il2cpp::object_new(texture2d_class());
    if this.is_null() {
        return std::ptr::null_mut();
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return std::ptr::null_mut();
    }
    let f: extern "C" fn(Object, i32, i32, i32, bool, bool, Method) =
        unsafe { std::mem::transmute(p) };
    f(this, w, h, fmt, mip, linear, m); // bools are 1 byte; MethodInfo* trailing
    this
}
pub fn ReadPixels(this: Object, source: Rect, dx: i32, dy: i32) {
    let m = M_READPIXELS.load(Ordering::Relaxed) as Method;
    if m.is_null() {
        return;
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return;
    }
    // Rect (16B) declared BY VALUE — Rust's extern "C" lowers it to the win64 hidden-pointer form,
    // matching il2cpp's compiled ReadPixels(this, Rect, int, int, MethodInfo*). Never flatten to 4
    // f32s (they'd shift dx/dy/MethodInfo into the wrong slots).
    let f: extern "C" fn(Object, Rect, i32, i32, Method) = unsafe { std::mem::transmute(p) };
    f(this, source, dx, dy, m);
}
pub fn GetTemporary(w: i32, h: i32) -> Object {
    let m = M_GETTEMP.load(Ordering::Relaxed) as Method;
    if m.is_null() {
        return std::ptr::null_mut();
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return std::ptr::null_mut();
    }
    let f: extern "C" fn(i32, i32, Method) -> Object = unsafe { std::mem::transmute(p) };
    f(w, h, m) // STATIC: no `this`
}
pub fn LoadImage(tex: Object, data: Object, mark_non_readable: bool) -> bool {
    let m = M_LOADIMAGE.load(Ordering::Relaxed) as Method;
    if m.is_null() {
        return false;
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return false;
    }
    let f: extern "C" fn(Object, Object, bool, Method) -> bool = unsafe { std::mem::transmute(p) };
    f(tex, data, mark_non_readable, m) // STATIC extension method; MethodInfo* trailing
}
pub fn get_texture(sprite: Object) -> Object {
    let m = M_GETTEXTURE.load(Ordering::Relaxed) as Method;
    if m.is_null() {
        return std::ptr::null_mut();
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return std::ptr::null_mut();
    }
    let f: extern "C" fn(Object, Method) -> Object = unsafe { std::mem::transmute(p) };
    f(sprite, m)
}

/// Resolve every texture symbol once. Returns Err (dispatcher stays disarmed) if a hot-path symbol
/// is missing — all validation happens here, before any GPU call runs.
pub fn init() -> Result<(), String> {
    let tex2d = il2cpp::class("UnityEngine.Texture2D");
    let tex = il2cpp::class("UnityEngine.Texture");
    let rt = il2cpp::class("UnityEngine.RenderTexture");
    let imgconv = il2cpp::class("UnityEngine.ImageConversion");
    let sprite = il2cpp::class("UnityEngine.Sprite");
    let byte = il2cpp::class("System.Byte");
    if tex2d.is_null() || tex.is_null() || rt.is_null() || imgconv.is_null() || sprite.is_null() || byte.is_null() {
        return Err("texture classes not resolved".into());
    }
    C_TEX2D.store(tex2d as usize, Ordering::Relaxed);
    C_BYTE.store(byte as usize, Ordering::Relaxed);
    M_CTOR.store(il2cpp::method(tex2d, ".ctor", 5) as usize, Ordering::Relaxed);
    M_READPIXELS.store(il2cpp::method(tex2d, "ReadPixels", 3) as usize, Ordering::Relaxed);
    M_GETTEMP.store(il2cpp::method(rt, "GetTemporary", 2) as usize, Ordering::Relaxed);
    M_LOADIMAGE.store(il2cpp::method(imgconv, "LoadImage", 3) as usize, Ordering::Relaxed);
    M_GETTEXTURE.store(il2cpp::method(sprite, "get_texture", 0) as usize, Ordering::Relaxed);
    I_GETW.store(il2cpp::resolve_icall("UnityEngine.Texture::GetDataWidth()") as usize, Ordering::Relaxed);
    I_GETH.store(il2cpp::resolve_icall("UnityEngine.Texture::GetDataHeight()") as usize, Ordering::Relaxed);
    I_BLIT2.store(
        il2cpp::resolve_icall("UnityEngine.Graphics::Blit2(UnityEngine.Texture,UnityEngine.RenderTexture)") as usize,
        Ordering::Relaxed,
    );
    I_GETACTIVE.store(il2cpp::resolve_icall("UnityEngine.RenderTexture::GetActive()") as usize, Ordering::Relaxed);
    I_SETACTIVE.store(
        il2cpp::resolve_icall("UnityEngine.RenderTexture::SetActive(UnityEngine.RenderTexture)") as usize,
        Ordering::Relaxed,
    );
    I_RELTEMP.store(
        il2cpp::resolve_icall("UnityEngine.RenderTexture::ReleaseTemporary(UnityEngine.RenderTexture)") as usize,
        Ordering::Relaxed,
    );
    I_GETPIX32.store(
        il2cpp::resolve_icall("UnityEngine.Texture2D::GetPixels32(System.Int32)") as usize,
        Ordering::Relaxed,
    );
    for (n, v) in [
        ("ctor", &M_CTOR),
        ("readpixels", &M_READPIXELS),
        ("gettemp", &M_GETTEMP),
        ("loadimage", &M_LOADIMAGE),
        ("getw", &I_GETW),
        ("geth", &I_GETH),
        ("blit2", &I_BLIT2),
        ("getpix32", &I_GETPIX32),
    ] {
        if v.load(Ordering::Relaxed) == 0 {
            return Err(format!("texture symbol missing: {n}"));
        }
    }
    Ok(())
}
