//! Overseer Plan B — native IL2CPP binding layer.
//!
//! Replaces frida-il2cpp-bridge: resolve the `il2cpp_*` C API exported by
//! GameAssembly.dll via GetProcAddress, then walk domain → assemblies → image →
//! class → method/field entirely in-process. No Frida, no Python.
//!
//! FOOTGUN: every Overseer-owned thread that touches managed memory MUST call
//! `thread_attach(domain())` once, or IL2CPP crashes. The game's render thread
//! is already attached; our loader/worker thread is not.

#![allow(dead_code)]

use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use retour::RawDetour;

use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

// ── Opaque IL2CPP pointer aliases (we never deref the structs directly, except
//    MethodInfo+0 = methodPointer, handled in `method_pointer`). ─────────────
pub type Domain = *mut c_void;
pub type Thread = *mut c_void;
pub type Assembly = *mut c_void;
pub type Image = *mut c_void;
pub type Class = *mut c_void;
pub type Method = *const c_void; // MethodInfo*
pub type Field = *mut c_void;
pub type Object = *mut c_void;

// ── Exported function signatures (C ABI). ───────────────────────────────────
type FnDomainGet = unsafe extern "C" fn() -> Domain;
type FnThreadAttach = unsafe extern "C" fn(Domain) -> Thread;
type FnThreadDetach = unsafe extern "C" fn(Thread);
type FnDomainGetAssemblies = unsafe extern "C" fn(Domain, *mut usize) -> *const Assembly;
type FnAssemblyGetImage = unsafe extern "C" fn(Assembly) -> Image;
type FnImageGetName = unsafe extern "C" fn(Image) -> *const c_char;
type FnClassFromName = unsafe extern "C" fn(Image, *const c_char, *const c_char) -> Class;
type FnClassGetMethodFromName = unsafe extern "C" fn(Class, *const c_char, i32) -> Method;
type FnClassGetFieldFromName = unsafe extern "C" fn(Class, *const c_char) -> Field;
type FnFieldGetOffset = unsafe extern "C" fn(Field) -> usize;
type FnRuntimeInvoke =
    unsafe extern "C" fn(Method, Object, *mut *mut c_void, *mut Object) -> Object;
type FnObjectNew = unsafe extern "C" fn(Class) -> Object;
type FnObjectGetClass = unsafe extern "C" fn(Object) -> Class;
type FnStringChars = unsafe extern "C" fn(Object) -> *const u16;
type FnStringLength = unsafe extern "C" fn(Object) -> i32;
type FnClassGetName = unsafe extern "C" fn(Class) -> *const c_char;
type FnClassGetStaticFieldData = unsafe extern "C" fn(Class) -> *mut c_void;
type FnClassGetType = unsafe extern "C" fn(Class) -> *mut c_void;
type FnTypeGetObject = unsafe extern "C" fn(*mut c_void) -> Object;
type FnClassGetMethods = unsafe extern "C" fn(Class, *mut *mut c_void) -> Method;
type FnMethodGetName = unsafe extern "C" fn(Method) -> *const c_char;
type FnMethodGetParamCount = unsafe extern "C" fn(Method) -> u32;
type FnMethodGetParam = unsafe extern "C" fn(Method, u32) -> *mut c_void; // Il2CppType*
type FnTypeGetName = unsafe extern "C" fn(*mut c_void) -> *mut c_char;
type FnStringNew = unsafe extern "C" fn(*const c_char) -> Object;
type FnMethodGetFlags = unsafe extern "C" fn(Method, *mut u32) -> u32;
type FnClassGetNestedTypes = unsafe extern "C" fn(Class, *mut *mut c_void) -> Class;
// Allocate a managed array of `len` elements of `element_class`. Returns Il2CppArray* (data @0x20).
type FnArrayNew = unsafe extern "C" fn(Class, usize) -> Object;
// GC write barrier: store `value` into `*field` of managed object `obj` (keeps the ref reachable).
type FnGcWbarrierSetField = unsafe extern "C" fn(Object, *mut *mut c_void, *mut c_void);
// GC handle: pin/track a managed object across frames/threads by an integer handle.
type FnGcHandleNew = unsafe extern "C" fn(Object, bool) -> u32;
type FnGcHandleGetTarget = unsafe extern "C" fn(u32) -> Object;
type FnGcHandleFree = unsafe extern "C" fn(u32);
// Il2CppType* → its Il2CppTypeEnum tag (for overload resolution by parameter type).
type FnTypeGetType = unsafe extern "C" fn(*mut c_void) -> i32;
// Enumerate a class's fields + read a field's name (for discovering *MotionTrackData fields).
type FnClassGetFields = unsafe extern "C" fn(Class, *mut *mut c_void) -> Field;
type FnFieldGetName = unsafe extern "C" fn(Field) -> *const c_char;

/// Resolved bundle of the IL2CPP exports we use. Built once in `init`.
struct Api {
    domain_get: FnDomainGet,
    thread_attach: FnThreadAttach,
    thread_detach: FnThreadDetach,
    domain_get_assemblies: FnDomainGetAssemblies,
    assembly_get_image: FnAssemblyGetImage,
    image_get_name: FnImageGetName,
    class_from_name: FnClassFromName,
    class_get_method_from_name: FnClassGetMethodFromName,
    class_get_field_from_name: FnClassGetFieldFromName,
    field_get_offset: FnFieldGetOffset,
    runtime_invoke: FnRuntimeInvoke,
    object_new: FnObjectNew,
    object_get_class: FnObjectGetClass,
    string_chars: FnStringChars,
    string_length: FnStringLength,
    class_get_name: FnClassGetName,
    class_get_static_field_data: FnClassGetStaticFieldData,
    class_get_type: FnClassGetType,
    type_get_object: FnTypeGetObject,
    class_get_methods: FnClassGetMethods,
    method_get_name: FnMethodGetName,
    method_get_param_count: FnMethodGetParamCount,
    method_get_param: FnMethodGetParam,
    type_get_name: FnTypeGetName,
    string_new: FnStringNew,
    method_get_flags: FnMethodGetFlags,
    class_get_nested_types: FnClassGetNestedTypes,
    array_new: FnArrayNew,
    gc_wbarrier_set_field: FnGcWbarrierSetField,
    gchandle_new: FnGcHandleNew,
    gchandle_get_target: FnGcHandleGetTarget,
    gchandle_free: FnGcHandleFree,
    type_get_type: FnTypeGetType,
    class_get_fields: FnClassGetFields,
    field_get_name: FnFieldGetName,
}
// SAFETY: the resolved code lives for the process lifetime; pointers are read-only.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

static API: OnceLock<Api> = OnceLock::new();

/// Get the loaded GameAssembly.dll module handle, or null if not yet loaded.
fn game_module() -> HMODULE {
    // "GameAssembly.dll" as a UTF-16, null-terminated wide string.
    let wide: Vec<u16> = "GameAssembly.dll\0".encode_utf16().collect();
    unsafe { GetModuleHandleW(wide.as_ptr()) }
}

/// True once GameAssembly.dll is present in the process.
pub fn game_loaded() -> bool {
    !game_module().is_null()
}

/// Resolve one exported symbol to a typed fn pointer. Returns None if absent.
unsafe fn resolve<T>(module: HMODULE, name: &[u8]) -> Option<T> {
    // name must be a null-terminated byte string.
    debug_assert_eq!(*name.last().unwrap(), 0, "resolve() name must be NUL-terminated");
    let proc = GetProcAddress(module, name.as_ptr());
    proc.map(|p| std::mem::transmute_copy::<_, T>(&p))
}

macro_rules! load {
    ($m:expr, $sym:literal) => {
        match resolve($m, concat!($sym, "\0").as_bytes()) {
            Some(f) => f,
            None => return Err(concat!("missing export: ", $sym)),
        }
    };
}

/// Resolve the IL2CPP API. Call once GameAssembly.dll is loaded. Idempotent.
pub fn init() -> Result<(), &'static str> {
    if API.get().is_some() {
        return Ok(());
    }
    let m = game_module();
    if m.is_null() {
        return Err("GameAssembly.dll not loaded yet");
    }
    let api = unsafe {
        Api {
            domain_get: load!(m, "il2cpp_domain_get"),
            thread_attach: load!(m, "il2cpp_thread_attach"),
            thread_detach: load!(m, "il2cpp_thread_detach"),
            domain_get_assemblies: load!(m, "il2cpp_domain_get_assemblies"),
            assembly_get_image: load!(m, "il2cpp_assembly_get_image"),
            image_get_name: load!(m, "il2cpp_image_get_name"),
            class_from_name: load!(m, "il2cpp_class_from_name"),
            class_get_method_from_name: load!(m, "il2cpp_class_get_method_from_name"),
            class_get_field_from_name: load!(m, "il2cpp_class_get_field_from_name"),
            field_get_offset: load!(m, "il2cpp_field_get_offset"),
            runtime_invoke: load!(m, "il2cpp_runtime_invoke"),
            object_new: load!(m, "il2cpp_object_new"),
            object_get_class: load!(m, "il2cpp_object_get_class"),
            string_chars: load!(m, "il2cpp_string_chars"),
            string_length: load!(m, "il2cpp_string_length"),
            class_get_name: load!(m, "il2cpp_class_get_name"),
            class_get_static_field_data: load!(m, "il2cpp_class_get_static_field_data"),
            class_get_type: load!(m, "il2cpp_class_get_type"),
            type_get_object: load!(m, "il2cpp_type_get_object"),
            class_get_methods: load!(m, "il2cpp_class_get_methods"),
            method_get_name: load!(m, "il2cpp_method_get_name"),
            method_get_param_count: load!(m, "il2cpp_method_get_param_count"),
            method_get_param: load!(m, "il2cpp_method_get_param"),
            type_get_name: load!(m, "il2cpp_type_get_name"),
            string_new: load!(m, "il2cpp_string_new"),
            method_get_flags: load!(m, "il2cpp_method_get_flags"),
            class_get_nested_types: load!(m, "il2cpp_class_get_nested_types"),
            array_new: load!(m, "il2cpp_array_new"),
            gc_wbarrier_set_field: load!(m, "il2cpp_gc_wbarrier_set_field"),
            gchandle_new: load!(m, "il2cpp_gchandle_new"),
            gchandle_get_target: load!(m, "il2cpp_gchandle_get_target"),
            gchandle_free: load!(m, "il2cpp_gchandle_free"),
            type_get_type: load!(m, "il2cpp_type_get_type"),
            class_get_fields: load!(m, "il2cpp_class_get_fields"),
            field_get_name: load!(m, "il2cpp_field_get_name"),
        }
    };
    let _ = API.set(api);
    Ok(())
}

fn api() -> &'static Api {
    API.get().expect("il2cpp::init() not called / failed")
}

/// True once the IL2CPP API has been resolved (init succeeded). Cheap guard for callers
/// that may run before the runtime is up — they must bail instead of panicking in `api()`.
pub fn ready() -> bool {
    API.get().is_some()
}

// ── High-level helpers ──────────────────────────────────────────────────────

pub fn domain() -> Domain {
    unsafe { (api().domain_get)() }
}

/// Attach the CURRENT thread to the IL2CPP domain. Call once per Overseer thread
/// before touching managed memory.
pub fn attach_current_thread() -> Thread {
    unsafe { (api().thread_attach)(domain()) }
}

/// Detach a previously-attached thread (unregister it from the GC). Do this
/// before the thread exits, so it isn't left dangling for the shutdown GC.
pub fn detach_thread(th: Thread) {
    if !th.is_null() {
        unsafe { (api().thread_detach)(th) }
    }
}

/// Find a class by "Namespace.Name" (or bare "Name") across every loaded
/// assembly image. Returns null if not found.
pub fn class(full_name: &str) -> Class {
    let (ns, name) = match full_name.rfind('.') {
        Some(i) => (&full_name[..i], &full_name[i + 1..]),
        None => ("", full_name),
    };
    let ns_c = to_cstring(ns);
    let name_c = to_cstring(name);
    unsafe {
        let dom = domain();
        let mut count: usize = 0;
        let asms = (api().domain_get_assemblies)(dom, &mut count);
        if asms.is_null() {
            return std::ptr::null_mut();
        }
        for i in 0..count {
            let asm = *asms.add(i);
            if asm.is_null() {
                continue;
            }
            let img = (api().assembly_get_image)(asm);
            if img.is_null() {
                continue;
            }
            let k = (api().class_from_name)(img, ns_c.as_ptr(), name_c.as_ptr());
            if !k.is_null() {
                return k;
            }
        }
        std::ptr::null_mut()
    }
}

/// Look up a NESTED class by its outer class full name + the nested simple name.
/// `class_from_name` can't see nested types, so we enumerate the outer's nested types.
pub fn nested_class(outer_full: &str, nested_name: &str) -> Class {
    nested_in(class(outer_full), nested_name)
}

/// Find a nested type by simple name inside an already-resolved outer Class. Use this to walk
/// DOUBLE-nested types (a coroutine state machine inside a nested controller): resolve each level
/// with nested_in. Null-safe.
pub fn nested_in(outer: Class, nested_name: &str) -> Class {
    if outer.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        let mut iter: *mut c_void = std::ptr::null_mut();
        loop {
            let k = (api().class_get_nested_types)(outer, &mut iter);
            if k.is_null() {
                break;
            }
            if class_name(k) == nested_name {
                return k;
            }
        }
    }
    std::ptr::null_mut()
}

/// Look up a method on a class by name + argument count (-1 = any count).
pub fn method(klass: Class, name: &str, argc: i32) -> Method {
    if klass.is_null() {
        return std::ptr::null();
    }
    let n = to_cstring(name);
    unsafe { (api().class_get_method_from_name)(klass, n.as_ptr(), argc) }
}

// Il2CppTypeEnum tags used to disambiguate overloads by parameter type.
pub const IL2CPP_TYPE_BOOLEAN: i32 = 0x02; // System.Boolean
pub const IL2CPP_TYPE_I4: i32 = 0x08; // System.Int32
pub const IL2CPP_TYPE_STRING: i32 = 0x0e; // System.String
pub const IL2CPP_TYPE_VALUETYPE: i32 = 0x11; // a struct/enum value type
pub const IL2CPP_TYPE_CLASS: i32 = 0x12; // a reference type (e.g. System.Type)

/// Resolve a method overload by EXACT parameter types (an Il2CppTypeEnum per parameter). `method()`
/// matches by arity only, so this is needed to pick e.g. `Localize.Get(TextId /*valuetype*/)` over a
/// sibling `Get(string)`, or `Enum.ToObject(Type, Int32)` among several 2-arg `ToObject` overloads.
/// Null if no overload of `name` matches the given parameter type tags.
pub fn method_overload(klass: Class, name: &str, param_type_enums: &[i32]) -> Method {
    if klass.is_null() {
        return std::ptr::null();
    }
    unsafe {
        let mut iter: *mut c_void = std::ptr::null_mut();
        loop {
            let m = (api().class_get_methods)(klass, &mut iter);
            if m.is_null() {
                break;
            }
            let np = (api().method_get_name)(m);
            if np.is_null() || cstr_to_string(np) != name {
                continue;
            }
            let pc = (api().method_get_param_count)(m);
            if pc as usize != param_type_enums.len() {
                continue;
            }
            let mut ok = true;
            for i in 0..pc {
                let t = (api().method_get_param)(m, i);
                if t.is_null() || (api().type_get_type)(t) != param_type_enums[i as usize] {
                    ok = false;
                    break;
                }
            }
            if ok {
                return m;
            }
        }
    }
    std::ptr::null()
}

/// The compiled native entry point of a method = MethodInfo->methodPointer,
/// which is the first pointer-sized field of the MethodInfo struct. This is the
/// address we detour (retour) and the address we cast to call the original.
pub fn method_pointer(m: Method) -> *const c_void {
    if m.is_null() {
        return std::ptr::null();
    }
    unsafe { *(m as *const *const c_void) }
}

/// Heuristic: is this compiled code already hooked (its prologue is a jmp)? A real IL2CPP
/// method prologue never starts with a jump, so `E9`/`EB` (rel jmp) or `FF 25` (abs jmp) means
/// another overlay detoured it first. Used to avoid double-detouring the same address.
pub unsafe fn is_detoured(code: *const c_void) -> bool {
    if code.is_null() {
        return false;
    }
    let p = code as *const u8;
    let b0 = std::ptr::read_unaligned(p);
    b0 == 0xE9 || b0 == 0xEB || (b0 == 0xFF && std::ptr::read_unaligned(p.add(1)) == 0x25)
}

/// Resolve `klass.name` (argc args), detour its compiled code to `detour`, and stash the
/// trampoline (the original entry point) in `tramp` + keep the detour alive in `keep`.
/// The detour forwards the trailing hidden MethodInfo* it receives, so callers don't need
/// it separately. Returns Err with the method name on any miss.
pub unsafe fn hook_method(
    klass: Class,
    name: &str,
    argc: i32,
    detour: *const (),
    tramp: &AtomicUsize,
    keep: &OnceLock<RawDetour>,
) -> Result<(), String> {
    let m = method(klass, name, argc);
    if m.is_null() {
        return Err(format!("{name}: not found"));
    }
    hook_at(m, name, detour, tramp, keep)
}

/// Detour an ALREADY-RESOLVED method (e.g. one from `method_overload`), with the same
/// cede-on-double-detour + arbiter accounting as `hook_method`. `label` is used for logging /
/// arbiter only. The detour forwards the trailing hidden MethodInfo* it receives.
pub unsafe fn hook_at(
    m: Method,
    label: &str,
    detour: *const (),
    tramp: &AtomicUsize,
    keep: &OnceLock<RawDetour>,
) -> Result<(), String> {
    if m.is_null() {
        return Err(format!("{label}: not found"));
    }
    let target = method_pointer(m);
    if target.is_null() {
        return Err(format!("{label}: null ptr"));
    }
    // If the method's prologue is already a jmp, another overlay has detoured it first.
    // Double-detouring the same address with a different hook engine corrupts the
    // trampolines and freezes the game — so we yield this hook instead of stacking on it.
    if is_detoured(target) {
        #[cfg(feature = "overseer")]
        crate::arbiter::record(label, crate::arbiter::Owner::External);
        return Err(format!("{label}: already detoured (skipped)"));
    }
    let d = RawDetour::new(target as *const (), detour).map_err(|e| format!("{label}: {e}"))?;
    // Publish the trampoline BEFORE enabling the detour: `trampoline()` is valid pre-enable, and the
    // instant the detour is live the hook can fire and read this — a zero here would transmute to a
    // null fn pointer and crash. Release-store pairs with the hook's Relaxed load on the same thread
    // (detour fires on the game thread) and is correct for any other reader.
    tramp.store(d.trampoline() as *const () as usize, Ordering::Release);
    d.enable().map_err(|e| format!("{label} enable: {e}"))?;
    let _ = keep.set(d);
    #[cfg(feature = "overseer")]
    crate::arbiter::record(label, crate::arbiter::Owner::Overseer);
    Ok(())
}

/// Byte offset of an instance field on a class (for the ObscuredInt reads etc.).
pub fn field_offset(klass: Class, name: &str) -> Option<usize> {
    if klass.is_null() {
        return None;
    }
    let n = to_cstring(name);
    unsafe {
        let f = (api().class_get_field_from_name)(klass, n.as_ptr());
        if f.is_null() {
            None
        } else {
            Some((api().field_get_offset)(f))
        }
    }
}

/// True if a UnityEngine.Object-derived MANAGED object has been DESTROYED on the native side
/// (its `m_CachedPtr` is 0). A strong GCHandle keeps the managed wrapper alive indefinitely, so
/// `handle.target()` can NEVER observe destruction — but invoking a method (set_text) on a
/// destroyed component raises a managed MissingReferenceException that unwinds through our native
/// frames (catch_unwind can't catch foreign exceptions → abort). Pure field read, never calls into
/// managed code. Conservative: returns false ("alive") when the offset can't be resolved.
pub fn unity_object_destroyed(obj: Object) -> bool {
    if obj.is_null() {
        return true;
    }
    static OFF: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let off = *OFF.get_or_init(|| {
        let k = class("UnityEngine.Object");
        if k.is_null() {
            return 0;
        }
        field_offset(k, "m_CachedPtr").unwrap_or(0)
    });
    if off == 0 {
        return false;
    }
    unsafe { *((obj as *const u8).add(off) as *const usize) == 0 }
}

/// Raw address of a STATIC field's storage (base of the class's static data + the field's
/// offset). Lets us read/write a static field directly (e.g. DOTween.timeScale) with a
/// plain memory store — no runtime call, safe from any thread. Null if not found.
pub fn static_field_addr(klass: Class, name: &str) -> *mut c_void {
    if klass.is_null() {
        return std::ptr::null_mut();
    }
    let n = to_cstring(name);
    unsafe {
        let f = (api().class_get_field_from_name)(klass, n.as_ptr());
        if f.is_null() {
            return std::ptr::null_mut();
        }
        let base = (api().class_get_static_field_data)(klass);
        if base.is_null() {
            return std::ptr::null_mut();
        }
        let off = (api().field_get_offset)(f);
        (base as *mut u8).add(off) as *mut c_void
    }
}

/// The `System.Type` managed object for a class (e.g. to pass `typeof(Component)` as a
/// method argument). Null if the class is null.
pub fn type_object(klass: Class) -> Object {
    if klass.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        let t = (api().class_get_type)(klass);
        if t.is_null() {
            return std::ptr::null_mut();
        }
        (api().type_get_object)(t)
    }
}

/// List all methods of a class as "name/paramCount" strings (for RE/diagnostics).
pub fn class_methods(klass: Class) -> Vec<String> {
    let mut out = Vec::new();
    if klass.is_null() {
        return out;
    }
    unsafe {
        let mut iter: *mut c_void = std::ptr::null_mut();
        loop {
            let m = (api().class_get_methods)(klass, &mut iter);
            if m.is_null() {
                break;
            }
            let np = (api().method_get_name)(m);
            if np.is_null() {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(np).to_string_lossy().into_owned();
            let pc = (api().method_get_param_count)(m);
            out.push(format!("{name}/{pc}"));
        }
    }
    out
}

/// Parameter type names of all overloads of `name` on `klass` (e.g. ["String, Boolean", "Int32"]).
/// One string per overload matching the name. For RE'ing a method's exact signature.
pub fn method_param_types(klass: Class, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    if klass.is_null() {
        return out;
    }
    let target = name;
    unsafe {
        let mut iter: *mut c_void = std::ptr::null_mut();
        loop {
            let m = (api().class_get_methods)(klass, &mut iter);
            if m.is_null() {
                break;
            }
            let np = (api().method_get_name)(m);
            if np.is_null() {
                continue;
            }
            let nm = std::ffi::CStr::from_ptr(np).to_string_lossy();
            if nm != target {
                continue;
            }
            let pc = (api().method_get_param_count)(m);
            let mut parts = Vec::new();
            for i in 0..pc {
                let t = (api().method_get_param)(m, i);
                if t.is_null() {
                    parts.push("?".to_string());
                    continue;
                }
                let tn = (api().type_get_name)(t);
                if tn.is_null() {
                    parts.push("?".to_string());
                } else {
                    parts.push(std::ffi::CStr::from_ptr(tn).to_string_lossy().into_owned());
                }
            }
            out.push(parts.join(", "));
        }
    }
    out
}

// `il2cpp_resolve_icall(const char* signature)` returns the native function backing an
// [InternalCall] method. The freecam hooks these (Transform set_position_Injected,
// Internal_LookAt_Injected). Resolved lazily so a missing export never breaks init.
type FnResolveIcall = unsafe extern "C" fn(*const c_char) -> *const c_void;
static RESOLVE_ICALL: OnceLock<Option<FnResolveIcall>> = OnceLock::new();

/// Resolve an engine internal call by full signature, e.g.
/// `"UnityEngine.Transform::set_position_Injected(UnityEngine.Vector3&)"`. Null if absent.
pub fn resolve_icall(signature: &str) -> *const c_void {
    let f = RESOLVE_ICALL.get_or_init(|| {
        let m = game_module();
        if m.is_null() {
            return None;
        }
        unsafe { resolve::<FnResolveIcall>(m, b"il2cpp_resolve_icall\0") }
    });
    match f {
        Some(func) => {
            let sig = to_cstring(signature);
            unsafe { func(sig.as_ptr()) }
        }
        None => std::ptr::null(),
    }
}

/// Allocate a new managed object of `klass` (e.g. a PointerEventData).
pub fn object_new(klass: Class) -> Object {
    if klass.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { (api().object_new)(klass) }
}

/// Allocate a managed array of `len` elements of `element_class`. Element storage starts at
/// offset 0x20; for reference-type elements each slot is an 8-byte pointer. Returns null on error.
pub fn array_new(element_class: Class, len: usize) -> Object {
    if element_class.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { (api().array_new)(element_class, len) }
}

/// GC write barrier: store reference `value` into the field at `field_addr` (which lives inside the
/// managed object `owner`). Use this whenever writing a managed reference into managed memory from
/// native code, so the GC keeps `value` reachable and doesn't collect it mid-operation.
pub unsafe fn wbarrier_set(owner: Object, field_addr: *mut c_void, value: Object) {
    (api().gc_wbarrier_set_field)(owner, field_addr as *mut *mut c_void, value);
}

/// A GC handle: pins/tracks a managed object so it survives a GC and can be safely carried
/// across frames or threads (a raw `Object` pointer cannot — the conservative GC does not scan
/// native heap/boxed closures, so a captured pointer can be freed before it's used again).
/// Wraps only a u32 GC-table index, so it is `Send`; re-fetch the live pointer with `target()`
/// on an il2cpp-attached thread (the main-thread pump always is). Frees on drop.
#[repr(transparent)]
pub struct GCHandle(u32);

impl GCHandle {
    /// Track `obj` (returns an empty handle for a null object). `pinned` also fixes the object's
    /// address (only needed for raw interop; pass `false` for "keep alive + re-fetch").
    pub fn new(obj: Object, pinned: bool) -> GCHandle {
        if obj.is_null() {
            return GCHandle(0);
        }
        GCHandle(unsafe { (api().gchandle_new)(obj, pinned) })
    }
    /// The live object, or null if the handle is empty / the object was collected.
    pub fn target(&self) -> Object {
        if self.0 == 0 {
            return std::ptr::null_mut();
        }
        unsafe { (api().gchandle_get_target)(self.0) }
    }
}

impl Drop for GCHandle {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe { (api().gchandle_free)(self.0) };
        }
    }
}

// SAFETY: wraps only a u32 GC-table index (no raw managed pointer). `target()` must be called on
// an attached thread — our only cross-thread consumer is the main-thread pump, which is attached.
unsafe impl Send for GCHandle {}

/// Invoke a method via the runtime (boxed args). For perf-critical hot paths,
/// prefer casting `method_pointer` to a typed fn and calling directly.
pub unsafe fn runtime_invoke(m: Method, this: Object, args: &mut [*mut c_void]) -> Object {
    let mut exc: Object = std::ptr::null_mut();
    let argp = if args.is_empty() { std::ptr::null_mut() } else { args.as_mut_ptr() };
    (api().runtime_invoke)(m, this, argp, &mut exc)
}

/// True if a method is static (no implicit `this` first arg). METHOD_ATTRIBUTE_STATIC = 0x10.
pub fn method_is_static(m: Method) -> bool {
    if m.is_null() {
        return false;
    }
    unsafe { ((api().method_get_flags)(m, std::ptr::null_mut()) & 0x10) != 0 }
}

/// Allocate a managed System.String from a Rust &str (UTF-8 → IL2CPP string).
/// The GC owns the result; only call from an attached thread. Null on failure.
pub fn new_string(s: &str) -> Object {
    let c = to_cstring(s);
    unsafe { (api().string_new)(c.as_ptr()) }
}

/// Read a managed System.String into a Rust String (UTF-16 → UTF-8).
pub fn read_string(s: Object) -> String {
    if s.is_null() {
        return String::new();
    }
    unsafe {
        let len = (api().string_length)(s);
        if len <= 0 {
            return String::new();
        }
        let chars = (api().string_chars)(s);
        if chars.is_null() {
            return String::new();
        }
        let slice = std::slice::from_raw_parts(chars, len as usize);
        String::from_utf16_lossy(slice)
    }
}

/// Zero-copy view of a managed System.String's UTF-16 body: (chars_ptr, len_in_code_units).
/// (null, 0) for a null string. Unlike `read_string()`, this does NOT transcode — the returned
/// pointer ALIASES the live il2cpp string buffer, so it is only valid while `s` is alive and must
/// never feed a UTF-8 consumer. This is the accessor the FNV-over-UTF16 content hash requires.
pub fn string_utf16(s: Object) -> (*const u16, usize) {
    if s.is_null() {
        return (std::ptr::null(), 0);
    }
    unsafe {
        let len = (api().string_length)(s);
        let chars = (api().string_chars)(s);
        (chars, if len > 0 { len as usize } else { 0 })
    }
}

/// FNV-1a-64 over the string's RAW UTF-16LE bytes (length*2 bytes, NUL excluded) — the key type of
/// `LocalizedData::hashed_dict`. `FnvHasher::default()` = offset basis 0xcbf2_9ce4_8422_2325, prime
/// 0x0000_0100_0000_01b3, per-byte FNV-1a, `finish()` unmixed. NO normalization/re-encoding — must
/// match the offline tool that built hashed_dict or every lookup misses.
pub fn string_fnv_hash(s: Object) -> u64 {
    use std::hash::Hasher;
    let (ptr, len) = string_utf16(s);
    let mut h = fnv::FnvHasher::default();
    if !ptr.is_null() && len > 0 {
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len * 2) };
        h.write(bytes);
    } else {
        h.write(&[]); // empty string → bare offset basis, matching the offline hashed_dict tool's convention
    }
    h.finish()
}

/// Runtime class of a managed object (null if obj is null). Use this to resolve methods on NESTED
/// types (`il2cpp_class_from_name` can't find `Outer.Nested` by namespace) — get the class from a
/// live instance instead.
pub fn object_class(obj: Object) -> Class {
    if obj.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { (api().object_get_class)(obj) }
}

/// Runtime class name of a managed object ("" if null). For type checks.
pub fn object_class_name(obj: Object) -> String {
    if obj.is_null() {
        return String::new();
    }
    unsafe {
        let k = (api().object_get_class)(obj);
        class_name(k)
    }
}

/// Class name as a Rust string (for runtime type checks).
pub fn class_name(klass: Class) -> String {
    if klass.is_null() {
        return String::new();
    }
    unsafe {
        let p = (api().class_get_name)(klass);
        cstr_to_string(p)
    }
}

// ── raw field access + List<T> view (Wave-5 story field pokes) ───────────────

/// Read a 32-bit value-type field at byte `offset` inside managed object `obj` (0 if `obj` null).
/// `offset` = `field_offset(klass, name)` (already includes the 0x10 header). Plain load, any thread.
pub fn get_field_i32(obj: Object, offset: usize) -> i32 {
    if obj.is_null() {
        return 0;
    }
    unsafe { *((obj as *const u8).add(offset) as *const i32) }
}

/// Write a 32-bit value-type field at `offset`. Plain store — NO GC barrier (value types only:
/// ClipLength/StartFrame/BlockLength/Length/TypewriteCountPerSecond/…).
pub fn set_field_i32(obj: Object, offset: usize, value: i32) {
    if obj.is_null() {
        return;
    }
    unsafe { *((obj as *mut u8).add(offset) as *mut i32) = value }
}

/// Read a managed-reference field at `offset` (null if `obj` null). For Il2CppString*/List<T>/track refs.
pub fn get_field_object(obj: Object, offset: usize) -> Object {
    if obj.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { *((obj as *const u8).add(offset) as *const Object) }
}

/// Store managed reference `value` into the field at `offset` of `owner`, THROUGH the GC write
/// barrier. MANDATORY for every object-field store from native code (Il2CppString* pokes) — a plain
/// store misses the card table and the new string can be collected / the write lost.
pub fn set_field_object(owner: Object, offset: usize, value: Object) {
    if owner.is_null() {
        return;
    }
    unsafe {
        let field_addr = (owner as *mut u8).add(offset) as *mut c_void;
        wbarrier_set(owner, field_addr, value);
    }
}

/// Byte offsets of every INSTANCE field declared on `klass` whose name ends with `suffix`. Used to
/// discover the many unnamed-per-instance `*MotionTrackData` fields on StoryTimelineCharaTrackData.
/// Every field on `klass` as `(name, offset)`, for one-shot layout discovery.
///
/// Writing IL2CPP field reads blind is how this codebase has historically shipped crashes (see the
/// affinity module, whose offset drifted between game versions and read garbage), so a new field
/// read starts by having the RUNNING game state its own layout rather than by guessing a name.
/// Diagnostic only — never call this on a hot path; it walks the whole field table.
pub fn class_fields(klass: Class) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    if klass.is_null() {
        return out;
    }
    unsafe {
        let mut iter: *mut c_void = std::ptr::null_mut();
        loop {
            let f = (api().class_get_fields)(klass, &mut iter);
            if f.is_null() {
                break;
            }
            let np = (api().field_get_name)(f);
            if np.is_null() {
                continue;
            }
            out.push((cstr_to_string(np), (api().field_get_offset)(f)));
        }
    }
    out
}

pub fn field_offsets_ending_with(klass: Class, suffix: &str) -> Vec<usize> {
    let mut out = Vec::new();
    if klass.is_null() {
        return out;
    }
    unsafe {
        let mut iter: *mut c_void = std::ptr::null_mut();
        loop {
            let f = (api().class_get_fields)(klass, &mut iter);
            if f.is_null() {
                break;
            }
            let np = (api().field_get_name)(f);
            if np.is_null() {
                continue;
            }
            if cstr_to_string(np).ends_with(suffix) {
                out.push((api().field_get_offset)(f));
            }
        }
    }
    out
}

/// Read-only view over a managed `System.Collections.Generic.List<T>` of REFERENCE-type elements.
/// Reads the private `_items` backing array + `_size` count via their resolved offsets (identical
/// across every List<ref> instantiation), then indexes element k at `_items + 0x20 + k*8`. Bounds by
/// `_size` (NOT the array capacity, which can exceed it). Klass-agnostic and calls NO managed method,
/// so there is no MethodInfo* ABI concern (unlike get_Item/get_Count).
pub struct IList {
    items: Object, // backing T[] array object
    size: usize,   // logical element count (List._size)
}

impl IList {
    /// Wrap a live `List<T>`. None if `list` is null, its `_items`/`_size` layout can't be resolved,
    /// or the backing array is null.
    pub fn new(list: Object) -> Option<IList> {
        if list.is_null() {
            return None;
        }
        let klass = object_class(list);
        let items_off = field_offset(klass, "_items")?;
        let size_off = field_offset(klass, "_size")?;
        let items = get_field_object(list, items_off);
        if items.is_null() {
            return None;
        }
        let size = get_field_i32(list, size_off).max(0) as usize;
        Some(IList { items, size })
    }
    pub fn count(&self) -> usize {
        self.size
    }
    /// Element k (a managed reference), or None past `_size`. Element data @ +0x20, 8-byte slots.
    pub fn get(&self, i: usize) -> Option<Object> {
        if i >= self.size {
            return None;
        }
        unsafe {
            let base = (self.items as *const u8).add(0x20) as *const Object;
            Some(*base.add(i))
        }
    }
    pub fn iter(&self) -> impl Iterator<Item = Object> + '_ {
        (0..self.size).filter_map(move |i| self.get(i))
    }
}

// ── small utils ──────────────────────────────────────────────────────────────
fn to_cstring(s: &str) -> Vec<c_char> {
    let mut v: Vec<c_char> = s.bytes().map(|b| b as c_char).collect();
    v.push(0);
    v
}
unsafe fn cstr_to_string(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}
