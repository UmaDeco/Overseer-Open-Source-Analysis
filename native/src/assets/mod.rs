//! The single Overseer-owned AssetBundle dispatcher (Phase 3 substrate).
//!
//! Every asset load funnels through one set of detours; localizers register a callback per asset
//! CLASS instead of each re-hooking AssetBundle. This is the choke point the whole
//! localization hook tree hangs off — Phase 4 registers Texture2D / AtlasReference / StoryTimelineData /
//! font / … localizers via `register_asset_localizer`, and the dispatch stays O(1) (class-ptr →
//! callback). With nothing registered it is a pure pass-through, so it is safe to arm before any
//! localizer exists.
//!
//! Hook kinds (kept strict — see bindings.rs): the four `AssetBundle::*_Internal` + `Unload` are
//! engine ICALLs (raw RawDetour, no MethodInfo); `AssetBundleRequest::GetResult` is a MANAGED method
//! (via `il2cpp::hook_method`, which cedes if already detoured). Async loads carry the requested
//! name across the `LoadAssetAsync → GetResult` boundary via a GC-pinned side map.

#![allow(dead_code)]

pub mod bindings;
pub mod texture;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use once_cell::sync::Lazy;
use retour::RawDetour;

use crate::il2cpp::{self, Class, GCHandle, Method, Object};

/// Bundle path prefix stripped when mapping an asset name to its localized-data file.
pub const ASSET_PATH_PREFIX: &str = "assets/_gallopresources/bundle/resources/";

fn log(m: &str) {
    crate::tools::log(m);
}

// ── registration table + dispatch ────────────────────────────────────────────
/// A Phase-4 localizer. Runs on whatever thread the load happened on; GPU work must go through
/// `texture::replace_*` (which auto-defers off the main thread).
pub type OnLoadAsset = fn(bundle: Object, asset: Object, name: &str);

static REGISTRY: Lazy<Mutex<HashMap<usize, OnLoadAsset>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// THE registration API. `class_ptr` = `il2cpp::class("...")` of the asset type to localize, e.g.
/// `register_asset_localizer(il2cpp::class("UnityEngine.Texture2D"), texture_cb)`.
pub fn register_asset_localizer(class_ptr: Class, f: OnLoadAsset) {
    if class_ptr.is_null() {
        return;
    }
    if let Ok(mut r) = REGISTRY.lock() {
        r.insert(class_ptr as usize, f);
    }
}

/// The single choke point: O(1) class-ptr lookup → registered callback. Unregistered classes pass
/// through untouched.
fn on_load_asset(bundle: Object, asset: Object, name: Object) {
    if asset.is_null() {
        return;
    }
    let class = il2cpp::object_class(asset);
    if class.is_null() {
        return;
    }
    let cb = REGISTRY.lock().ok().and_then(|r| r.get(&(class as usize)).copied());
    if let Some(cb) = cb {
        let name_str = il2cpp::read_string(name); // copy to owned Rust String while `name` is live
        cb(bundle, asset, &name_str);
    }
}

// ── side maps (pointer-keyed; entries REMOVED on teardown so a reused ptr can't mis-dispatch) ──
static BUNDLE_PATHS: Lazy<Mutex<HashMap<usize, GCHandle>>> = Lazy::new(|| Mutex::new(HashMap::new()));

struct RequestInfo {
    name: GCHandle,
    bundle: usize,
}
static REQUEST_INFOS: Lazy<Mutex<HashMap<usize, RequestInfo>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// bundle → on-disk path (for the localizer's bundle-name verification guard).
pub fn get_bundle_path(bundle: Object) -> Option<String> {
    let g = BUNDLE_PATHS.lock().ok()?;
    let s = g.get(&(bundle as usize))?.target();
    if s.is_null() {
        None
    } else {
        Some(il2cpp::read_string(s))
    }
}

// ── the 5 detours ────────────────────────────────────────────────────────────
static TR_LA: AtomicUsize = AtomicUsize::new(0);
static D_LA: OnceLock<RawDetour> = OnceLock::new();
static TR_LAA: AtomicUsize = AtomicUsize::new(0);
static D_LAA: OnceLock<RawDetour> = OnceLock::new();
static TR_LFF: AtomicUsize = AtomicUsize::new(0);
static D_LFF: OnceLock<RawDetour> = OnceLock::new();
static TR_UL: AtomicUsize = AtomicUsize::new(0);
static D_UL: OnceLock<RawDetour> = OnceLock::new();
static TR_GR: AtomicUsize = AtomicUsize::new(0);
static D_GR: OnceLock<RawDetour> = OnceLock::new();

// (1) ICALL, INSTANCE: (this, name:String*, type:Type*) -> asset*  NO MethodInfo
type LoadAssetFn = unsafe extern "C" fn(Object, Object, Object) -> Object;
unsafe extern "C" fn h_LoadAsset(this: Object, name: Object, ty: Object) -> Object {
    let t = TR_LA.load(Ordering::Relaxed);
    let asset = if t != 0 {
        let f: LoadAssetFn = std::mem::transmute(t);
        f(this, name, ty)
    } else {
        std::ptr::null_mut()
    };
    on_load_asset(this, asset, name); // sync: everything available now
    asset
}

// (2) ICALL, INSTANCE: (this, name, type) -> AssetBundleRequest*  stash keyed by request ptr
unsafe extern "C" fn h_LoadAssetAsync(this: Object, name: Object, ty: Object) -> Object {
    let t = TR_LAA.load(Ordering::Relaxed);
    let req = if t != 0 {
        let f: LoadAssetFn = std::mem::transmute(t);
        f(this, name, ty)
    } else {
        std::ptr::null_mut()
    };
    if !req.is_null() {
        if let Ok(mut m) = REQUEST_INFOS.lock() {
            m.insert(req as usize, RequestInfo { name: GCHandle::new(name, false), bundle: this as usize });
        }
    }
    req
}

// (3) ICALL, STATIC: (path:String*, crc:u32, offset:u64) -> bundle*  NO this, NO MethodInfo
type LoadFromFileFn = unsafe extern "C" fn(Object, u32, u64) -> Object;
unsafe extern "C" fn h_LoadFromFile(path: Object, crc: u32, offset: u64) -> Object {
    let t = TR_LFF.load(Ordering::Relaxed);
    let bundle = if t != 0 {
        let f: LoadFromFileFn = std::mem::transmute(t);
        f(path, crc, offset)
    } else {
        std::ptr::null_mut()
    };
    if !bundle.is_null() {
        if let Ok(mut m) = BUNDLE_PATHS.lock() {
            m.insert(bundle as usize, GCHandle::new(path, false));
        }
    }
    bundle
}

// (4) ICALL, INSTANCE: (this, unloadAll: bool)  bool = 1 byte
type UnloadFn = unsafe extern "C" fn(Object, bool);
unsafe extern "C" fn h_Unload(this: Object, all: bool) {
    if let Ok(mut m) = BUNDLE_PATHS.lock() {
        m.remove(&(this as usize)); // free the path GCHandle; avoid stale-ptr reuse
    }
    let t = TR_UL.load(Ordering::Relaxed);
    if t != 0 {
        let f: UnloadFn = std::mem::transmute(t);
        f(this, all);
    }
}

// (5) MANAGED, INSTANCE argc=0: real ABI (this, MethodInfo*). Declare + forward mi.
type GetResultFn = unsafe extern "C" fn(Object, Method) -> Object;
unsafe extern "C" fn h_GetResult(this: Object, mi: Method) -> Object {
    let t = TR_GR.load(Ordering::Relaxed);
    let asset = if t != 0 {
        let f: GetResultFn = std::mem::transmute(t);
        f(this, mi)
    } else {
        std::ptr::null_mut()
    };
    let info = REQUEST_INFOS.lock().ok().and_then(|mut m| m.remove(&(this as usize)));
    if let Some(info) = info {
        on_load_asset(info.bundle as Object, asset, info.name.target());
    }
    asset
}

/// Call the ORIGINAL `AssetBundle.LoadFromFile_Internal` via its trampoline (bypasses the
/// dispatcher). Null if the dispatcher isn't armed. Used by the font loader to load the extra bundle.
pub fn load_from_file_orig(path: Object, crc: u32, offset: u64) -> Object {
    let t = TR_LFF.load(Ordering::Relaxed);
    if t == 0 {
        return std::ptr::null_mut();
    }
    unsafe {
        let f: LoadFromFileFn = std::mem::transmute(t);
        f(path, crc, offset)
    }
}

/// Call the ORIGINAL `AssetBundle.LoadAsset_Internal` via its trampoline (bypasses the dispatcher).
/// Null if the dispatcher isn't armed. Used by the font loader to load the replacement font asset.
pub fn load_asset_orig(bundle: Object, name: Object, ty: Object) -> Object {
    let t = TR_LA.load(Ordering::Relaxed);
    if t == 0 {
        return std::ptr::null_mut();
    }
    unsafe {
        let f: LoadAssetFn = std::mem::transmute(t);
        f(bundle, name, ty)
    }
}

// ── install ──────────────────────────────────────────────────────────────────
unsafe fn hook_icall(sig: &str, det: *const (), tr: &AtomicUsize, keep: &OnceLock<RawDetour>) -> Result<(), String> {
    let addr = il2cpp::resolve_icall(sig);
    if addr.is_null() {
        return Err(format!("icall not found: {sig}"));
    }
    let d = RawDetour::new(addr as *const (), det).map_err(|e| format!("{sig}: {e}"))?;
    d.enable().map_err(|e| format!("{sig} enable: {e}"))?;
    tr.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
    let _ = keep.set(d);
    Ok(())
}

/// Resolve the texture bindings, then install all 5 AssetBundle detours. Safe to arm before any
/// localizer registers — the dispatcher is a pure pass-through until `register_asset_localizer` is
/// called. Returns Err (dispatcher disarmed) if any symbol is missing.
pub fn install() -> Result<(), String> {
    bindings::init()?; // resolve texture il2cpp bindings first (fail loud if any missing)
    unsafe {
        // Sync path + bundle tracking — the core. Required; a miss disarms the dispatcher.
        hook_icall(
            "UnityEngine.AssetBundle::LoadAsset_Internal(System.String,System.Type)",
            h_LoadAsset as *const (),
            &TR_LA,
            &D_LA,
        )?;
        hook_icall(
            "UnityEngine.AssetBundle::LoadFromFile_Internal(System.String,System.UInt32,System.UInt64)",
            h_LoadFromFile as *const (),
            &TR_LFF,
            &D_LFF,
        )?;
        hook_icall("UnityEngine.AssetBundle::Unload(System.Boolean)", h_Unload as *const (), &TR_UL, &D_UL)?;

        // Async path: only arm LoadAssetAsync if its CONSUMER (GetResult) also hooks — otherwise the
        // RequestInfo entries LoadAssetAsync stashes would never be consumed (a leak). Non-fatal: the
        // sync path above still localizes; GetResult is MANAGED so hook_method cedes if pre-detoured.
        let abr = il2cpp::class("UnityEngine.AssetBundleRequest");
        let get_result_ok = !abr.is_null()
            && il2cpp::hook_method(abr, "GetResult", 0, h_GetResult as *const (), &TR_GR, &D_GR).is_ok();
        if get_result_ok {
            hook_icall(
                "UnityEngine.AssetBundle::LoadAssetAsync_Internal(System.String,System.Type)",
                h_LoadAssetAsync as *const (),
                &TR_LAA,
                &D_LAA,
            )?;
        } else {
            log("[assets] async localization disabled (AssetBundleRequest.GetResult unavailable) — sync path still active");
        }
    }
    Ok(())
}
