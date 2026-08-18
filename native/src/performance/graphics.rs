//! Model quality unlock + the live graphics knobs (cosmetic / perf).
//!
//! `Gallop.GraphicSettings.ApplyGraphicsQuality(quality, force)` selects the character render
//! quality tier. We hook it to force the full-quality toon tier with `force = true`, so models
//! always render at full quality regardless of the device / menu cap.
//!
//! ## What can actually be changed on this build (all of it measured, not assumed)
//!
//! The knobs used to live inside that hook, which made them dead twice over:
//!
//!  1. **`ApplyGraphicsQuality` never fires** — `gfx_applies` stayed 0 for a whole session, so
//!     anything applied only from inside the hook could never run. The fix is [`pump`], called from
//!     the main-thread `TweenManager.Update` tick: it applies on change and re-asserts every ~2s,
//!     so the game's own quality pass can't stomp the user's choice either.
//!
//!  2. **IL2CPP stripped most of `QualitySettings`** — Unity drops what the game never calls. Only
//!     four setters survive in the binary: `set_antiAliasing`, `set_pixelLightCount`,
//!     `set_shadowDistance`, `set_enableLODCrossFade`. `anisotropicFiltering`, `lodBias`,
//!     `shadowResolution`, `shadows` and `masterTextureLimit` **do not exist** — they resolved MISS,
//!     so those options are not offered rather than shipped as silent no-ops.
//!
//! The `UniversalRenderPipelineAsset` looked like the way out (it's fully intact — msaa /
//! renderScale / HDR / shadow-res setters all resolve), but the game renders on Unity's **built-in
//! pipeline**: `GraphicsSettings.currentRenderPipeline`, `.defaultRenderPipeline` and
//! `QualitySettings.renderPipeline` all return NULL, so the asset is never used. That's why there's
//! no URP path here and no render-scale/HDR/shadow-res knob.
//!
//! Net: anti-aliasing, shadow distance (0 = shadows off), pixel-light count and LOD cross-fade are
//! real; everything else would be theatre. [`readback`] reads AA + shadow distance back out of the
//! engine so the UI can show what actually took, instead of just what was asked for.
//!
//! No gameplay effect, so this ships in every build.

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;

use retour::RawDetour;

use crate::il2cpp;

/// `GraphicsQuality` enum value we force. `ToonFull` (3) is the full-quality toon tier.
const QUALITY_FULL: i32 = 3;

static QUALITY_ON: AtomicBool = AtomicBool::new(false);
static LOW_SPEC: AtomicBool = AtomicBool::new(false);
/// How hard Low Resources mode pushes: 1 = Balanced, 2 = Aggressive, 3 = Extreme, 4 = Minimum.
///
/// The original mode wrote four values and stopped, which is why it barely moved the needle on a
/// weak machine. The tiers below add every remaining lever this build actually exposes, and the
/// biggest one by far — render scale — is not a QualitySettings knob at all but the resolution the
/// game renders at, which is applied through `performance::display`.
static LOW_LEVEL: AtomicI32 = AtomicI32::new(1);

// User overrides from the Performance page. Sentinel = "leave the game's own value": -1 for the
// i32s, negative for the float. Pushed by pump() and re-asserted, so a choice always wins.
static G_AA: AtomicI32 = AtomicI32::new(-1); // antiAliasing: 0/2/4/8 (MSAA sample count)
static G_SHADOWS: AtomicI32 = AtomicI32::new(-1); // 0 = off (forces shadowDistance 0), -1 = leave
static G_SHADOWDIST: AtomicU32 = AtomicU32::new(0xBF80_0000); // shadowDistance f32, -1 = leave

// NOTE: seven further QualitySettings setters were tried here for Low Resources — skinWeights,
// particleRaycastBudget, softParticles, realtimeReflectionProbes, maximumLODLevel, shadowCascades
// and asyncUploadTimeSlice. ALL SEVEN resolved MISS on this build (see `apply_now` for the verbatim
// log line), joining aniso / lodBias / shadowResolution / masterTextureLimit. They are not kept as
// dormant slots, because a resolved-but-absent setter is exactly how a knob ships broken: the write
// silently returns and the UI claims it took effect. The four below are the entire real surface.

static TR_AGQ: AtomicUsize = AtomicUsize::new(0);
static D_AGQ: OnceLock<RawDetour> = OnceLock::new();

// Resolved `UnityEngine.QualitySettings` static setters (Method handles). ONLY these four exist in
// this build — see the module docs. Anything else you might want (aniso, lodBias, shadowResolution,
// shadows, masterTextureLimit) was stripped by IL2CPP and is not resolvable at any address.
static QS_ANTIALIAS: AtomicUsize = AtomicUsize::new(0); // set_antiAliasing(int)
static QS_PIXELLIGHTS: AtomicUsize = AtomicUsize::new(0); // set_pixelLightCount(int)
static QS_SHADOWDIST: AtomicUsize = AtomicUsize::new(0); // set_shadowDistance(float)
static QS_LODCROSSFADE: AtomicUsize = AtomicUsize::new(0); // set_enableLODCrossFade(bool)
pub fn shadow_distance() -> f32 {
    f32::from_bits(G_SHADOWDIST.load(Ordering::Relaxed))
}
pub fn set_shadow_distance(v: f32) {
    G_SHADOWDIST.store(v.to_bits(), Ordering::Relaxed);
    DIRTY.store(true, Ordering::Relaxed);
}
/// Call a resolved STATIC QualitySettings setter taking a single bool.
unsafe fn set_bool_qs(slot: &AtomicUsize, value: bool) {
    let m = slot.load(Ordering::Relaxed) as il2cpp::Method;
    if m.is_null() {
        return;
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return;
    }
    let f: extern "C" fn(bool, *const c_void) = std::mem::transmute(p);
    f(value, m as *const c_void);
}

// ── Read-back: proof a write actually landed ─────────────────────────────────────────────────────
// FPS is too indirect to trust as evidence (it's CPU-bound here, and characters render to a
// fixed-size RT), so instead we read the values straight back OUT of the engine on the main-thread
// pump and surface them in /api/mod/performance. If the UI says 8× and the engine says 0, the write
// didn't take — no guessing.
static QS_GET_AA: AtomicUsize = AtomicUsize::new(0); // get_antiAliasing
static QS_GET_SHADOWDIST: AtomicUsize = AtomicUsize::new(0); // get_shadowDistance
static RB_AA: AtomicI32 = AtomicI32::new(-1);
static RB_SHADOWDIST: AtomicU32 = AtomicU32::new(0xBF80_0000); // -1.0 = never read (getter stripped)
static PUMP_LOGGED: AtomicBool = AtomicBool::new(false);
/// What the ENGINE currently reports back (antiAliasing, shadowDistance) — proof our writes landed.
pub fn readback() -> (i32, f32) {
    (RB_AA.load(Ordering::Relaxed), f32::from_bits(RB_SHADOWDIST.load(Ordering::Relaxed)))
}

/// Set by every knob setter → the main-thread pump re-applies. Also re-asserted periodically so the
/// game's own quality pass can't silently stomp our values.
static DIRTY: AtomicBool = AtomicBool::new(true);
static LAST_APPLY_MS: AtomicU64 = AtomicU64::new(0);

fn log(msg: &str) {
    crate::tools::log(msg);
}

/// Resolve-report helper: a slot is either `ok` or a `MISS` we must not silently build on.
fn ok_miss(slot: usize) -> &'static str {
    if slot == 0 { "MISS" } else { "ok" }
}

pub fn set_quality_unlocked(on: bool) {
    QUALITY_ON.store(on, Ordering::Relaxed);
    DIRTY.store(true, Ordering::Relaxed);
}

pub fn low_spec() -> bool {
    LOW_SPEC.load(Ordering::Relaxed)
}
pub fn set_low_spec(on: bool) {
    LOW_SPEC.store(on, Ordering::Relaxed);
    DIRTY.store(true, Ordering::Relaxed);
}

/// Low Resources tier (1..3). Higher = lighter + uglier.
pub fn low_level() -> i32 {
    LOW_LEVEL.load(Ordering::Relaxed)
}
pub fn set_low_level(v: i32) {
    LOW_LEVEL.store(v.clamp(1, 3), Ordering::Relaxed);
    DIRTY.store(true, Ordering::Relaxed);
}

// NOTE: a tier-driven FRAME-RATE CAP was written here and removed before it shipped. It is a real,
// large saving — but it is the wrong lever for THIS mode. Low Resource Mode is described on its own
// panel as "the fastest option for Super Skip", and skipping is frame-driven: every skip step is
// pumped from a per-frame hook, so capping the frame rate directly slows down the thing the mode
// exists to speed up. A cap belongs on the Frame Rate panel, where the user chooses it explicitly.

// ══════════════ The game's OWN quality tier — the one large lever that is real ══════════════════
//
// `GraphicSettings.ApplyGraphicsQuality(quality, force)` selects the whole character/scene render
// preset. It is the game's device-tier system, so it is designed to be switched at runtime and is
// worth far more than any individual QualitySettings flag.
//
// The catch, and the reason it has never done anything: our detour on it has fired ZERO times in
// every field log ("gfx_applies": 0), so the tier chosen inside `on_apply_quality` is unreachable.
// The class probe (see `install`) settled the rest — `static=false`, i.e. it needs an instance, and
// the class exposes no `Instance` accessor among its 140 methods. So we go and find the live one
// through `UnityEngine.Object.FindObjectOfType(typeof(GraphicSettings))`, hold it with a strong
// GCHandle, and drive the method ourselves from the main-thread pump.
static OBJ_FIND: AtomicUsize = AtomicUsize::new(0); // UnityEngine.Object.FindObjectOfType(Type)
static GFX_TYPE: AtomicUsize = AtomicUsize::new(0); // typeof(Gallop.GraphicSettings), pinned
static AGQ_M: AtomicUsize = AtomicUsize::new(0); // ApplyGraphicsQuality (Method)
static GETQ_M: AtomicUsize = AtomicUsize::new(0); // GetQuality (Method)
/// The live `GraphicSettings`, once found. Strong handle: a raw pointer here would be a
/// use-after-free the moment the scene unloads.
static GFX_INST: std::sync::Mutex<Option<il2cpp::GCHandle>> = std::sync::Mutex::new(None);
/// The quality tier WE last pushed, or -1 for "we have pushed none". Drives change-detection so the
/// preset is applied on transitions only, never re-asserted every 2s like the QualitySettings flags.
static APPLIED_Q: AtomicI32 = AtomicI32::new(-1);
/// The game's own tier, read once via `GetQuality` before we first override it.
static ORIG_Q: AtomicI32 = AtomicI32::new(-1);
/// Throttle for the scene scan — `FindObjectOfType` walks live objects and is not free.
static LAST_FIND_MS: AtomicU64 = AtomicU64::new(0);

/// The live `GraphicSettings` instance, or null. Cached; re-acquired if the scene destroyed it.
unsafe fn gfx_instance() -> *mut c_void {
    if let Ok(g) = GFX_INST.lock() {
        if let Some(h) = g.as_ref() {
            let p = h.target();
            // Non-null is NOT enough: our own handle keeps the managed wrapper alive after Unity has
            // torn down the native side, so the destroyed check is the load-bearing one.
            if !p.is_null() && !il2cpp::unity_object_destroyed(p) {
                return p;
            }
        }
    }
    let now = crate::tools::now_ms();
    if now.saturating_sub(LAST_FIND_MS.load(Ordering::Relaxed)) < 3000 {
        return std::ptr::null_mut();
    }
    LAST_FIND_MS.store(now, Ordering::Relaxed);
    let m = OBJ_FIND.load(Ordering::Relaxed) as il2cpp::Method;
    let ty = GFX_TYPE.load(Ordering::Relaxed) as *mut c_void;
    if m.is_null() || ty.is_null() {
        return std::ptr::null_mut();
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return std::ptr::null_mut();
    }
    let f: extern "C" fn(*mut c_void, *const c_void) -> *mut c_void = std::mem::transmute(p);
    let found = f(ty, m as *const c_void);
    if found.is_null() {
        return std::ptr::null_mut();
    }
    if let Ok(mut g) = GFX_INST.lock() {
        *g = Some(il2cpp::GCHandle::new(found, false));
    }
    log("[graphics] found the live GraphicSettings — the game's own quality tier is now drivable");
    found
}

/// Push the game's own quality preset for the current tier, or restore its original.
///
/// Called from `apply_now`, i.e. the main thread. Only acts on a CHANGE: this reconfigures render
/// state, so re-asserting it on every 2s pump would mean repeatedly rebuilding quality settings
/// underneath the game for no benefit.
unsafe fn apply_game_quality(want: i32) {
    if want == APPLIED_Q.load(Ordering::Relaxed) {
        return;
    }
    let inst = gfx_instance();
    if inst.is_null() {
        return; // not in a scene that has one yet — try again on a later pump
    }
    // Snapshot the game's own tier before the first override, so "off" restores rather than guesses.
    if ORIG_Q.load(Ordering::Relaxed) < 0 {
        let gm = GETQ_M.load(Ordering::Relaxed) as il2cpp::Method;
        if !gm.is_null() {
            let gp = il2cpp::method_pointer(gm);
            if !gp.is_null() {
                let g: extern "C" fn(*mut c_void, *const c_void) -> i32 = std::mem::transmute(gp);
                ORIG_Q.store(g(inst, gm as *const c_void), Ordering::Relaxed);
            }
        }
    }
    let target = if want >= 0 { want } else { ORIG_Q.load(Ordering::Relaxed) };
    if target < 0 {
        return; // nothing to restore to — leave the game alone rather than guess a tier
    }
    let am = AGQ_M.load(Ordering::Relaxed) as il2cpp::Method;
    if am.is_null() {
        return;
    }
    let ap = il2cpp::method_pointer(am);
    if ap.is_null() {
        return;
    }
    // Through the TRAMPOLINE, not the detour: `on_apply_quality` would re-apply our tier choice on
    // top of this call and turn a restore back into an override.
    let t = TR_AGQ.load(Ordering::Relaxed);
    let entry = if t != 0 { t as *const c_void } else { ap };
    let f: extern "C" fn(*mut c_void, i32, bool, *const c_void) = std::mem::transmute(entry);
    let _g = crate::hooks::ReentryGuard::enter();
    f(inst, target, true, am as *const c_void);
    APPLIED_Q.store(want, Ordering::Relaxed);
    log(&format!("[graphics] game quality tier -> {target} (Low Resources tier {})", low_level()));
}


// ── granular knob getters/setters ───────────────────────────────────────────────────────────────
// Store only; [`pump`] pushes them on the main thread. -1 / negative = "leave the game's value".
// Only knobs whose setter SURVIVED IL2CPP stripping exist here — aniso, texture limit, LOD bias and
// shadow resolution are absent from the binary (verified MISS at resolve time), so they're left out
// rather than shipped as switches that silently do nothing. Shadows on/off goes through shadow
// DISTANCE (set_shadows is stripped too; distance 0 == no shadows on the built-in pipeline).
pub fn aa() -> i32 { G_AA.load(Ordering::Relaxed) }
pub fn set_aa(v: i32) { G_AA.store(v, Ordering::Relaxed); DIRTY.store(true, Ordering::Relaxed); }
pub fn shadows() -> i32 { G_SHADOWS.load(Ordering::Relaxed) }
pub fn set_shadows(v: i32) { G_SHADOWS.store(v, Ordering::Relaxed); DIRTY.store(true, Ordering::Relaxed); }

/// Call a resolved static `QualitySettings` setter that takes a single i32.
unsafe fn set_i32(slot: &AtomicUsize, value: i32) {
    let m = slot.load(Ordering::Relaxed) as il2cpp::Method;
    if m.is_null() {
        return;
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return;
    }
    let f: extern "C" fn(i32, *const c_void) = std::mem::transmute(p);
    f(value, m as *const c_void);
}

/// Call a resolved static `QualitySettings` setter that takes a single f32.
unsafe fn set_f32(slot: &AtomicUsize, value: f32) {
    let m = slot.load(Ordering::Relaxed) as il2cpp::Method;
    if m.is_null() {
        return;
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return;
    }
    let f: extern "C" fn(f32, *const c_void) = std::mem::transmute(p);
    f(value, m as *const c_void);
}

// ── The game's own values, captured ONCE before we overwrite anything ────────────────────────────
// Without this, "Default" is a lie: clearing an override just stops us writing, leaving our last
// value latched in the engine until something else happens to reset it. We snapshot on the first
// pump (via the getters) so Default can put the original back. If a getter is stripped we simply
// have no original to restore — in that case Default keeps the last value and says so in the UI,
// rather than pretending.
/// True while any value WE wrote is still latched in the engine (so a clear-to-Default still owes a
/// restore pass). Cleared once that restore has been pushed.
static TOUCHED: AtomicBool = AtomicBool::new(false);
/// Is any knob currently overridden (i.e. not "Default")?
fn has_overrides() -> bool {
    LOW_SPEC.load(Ordering::Relaxed)
        || G_AA.load(Ordering::Relaxed) >= 0
        || G_SHADOWS.load(Ordering::Relaxed) >= 0
        || f32::from_bits(G_SHADOWDIST.load(Ordering::Relaxed)) > 0.0
}

static ORIG_TAKEN: AtomicBool = AtomicBool::new(false);
static ORIG_AA: AtomicI32 = AtomicI32::new(-1);
static ORIG_SHADOWDIST: AtomicU32 = AtomicU32::new(0xBF80_0000); // -1.0 = unknown

unsafe fn read_i32(slot: &AtomicUsize) -> Option<i32> {
    let m = slot.load(Ordering::Relaxed) as il2cpp::Method;
    if m.is_null() {
        return None;
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return None;
    }
    let f: extern "C" fn(*const c_void) -> i32 = std::mem::transmute(p);
    Some(f(m as *const c_void))
}
unsafe fn read_f32(slot: &AtomicUsize) -> Option<f32> {
    let m = slot.load(Ordering::Relaxed) as il2cpp::Method;
    if m.is_null() {
        return None;
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return None;
    }
    let f: extern "C" fn(*const c_void) -> f32 = std::mem::transmute(p);
    Some(f(m as *const c_void))
}
/// Capture the game's own values on the first pump, before any override lands.
unsafe fn snapshot_originals() {
    if ORIG_TAKEN.swap(true, Ordering::Relaxed) {
        return;
    }
    if let Some(v) = read_i32(&QS_GET_AA) {
        ORIG_AA.store(v, Ordering::Relaxed);
    }
    if let Some(v) = read_f32(&QS_GET_SHADOWDIST) {
        ORIG_SHADOWDIST.store(v.to_bits(), Ordering::Relaxed);
    }
    log(&format!(
        "[graphics] captured game defaults: aa={} shadowDistance={}",
        ORIG_AA.load(Ordering::Relaxed),
        f32::from_bits(ORIG_SHADOWDIST.load(Ordering::Relaxed))
    ));
}
fn original_aa() -> Option<i32> {
    let v = ORIG_AA.load(Ordering::Relaxed);
    if ORIG_TAKEN.load(Ordering::Relaxed) && v >= 0 { Some(v) } else { None }
}
fn original_shadow_dist() -> Option<f32> {
    let v = f32::from_bits(ORIG_SHADOWDIST.load(Ordering::Relaxed));
    if ORIG_TAKEN.load(Ordering::Relaxed) && v >= 0.0 { Some(v) } else { None }
}

/// Push the user's graphics choices to `QualitySettings`. MAIN THREAD ONLY (Unity API).
///
/// Only the statics that survived IL2CPP stripping are touched — see the module docs for why that's
/// the whole surface on this build. Re-applied every pump so a game-side quality pass can't quietly
/// revert the user's choice.
unsafe fn apply_now() {
    snapshot_originals();
    // Latch whether any of OUR values are currently sitting in the engine, so pump() knows it still
    // owes a restore pass after the last override is cleared (and can then stop writing entirely).
    TOUCHED.store(has_overrides(), Ordering::Relaxed);
    let low = LOW_SPEC.load(Ordering::Relaxed);
    if low {
        // ── What Low Resources can ACTUALLY do on this build ────────────────────────────────────
        // An earlier cut of the tiers wrote seven more QualitySettings values (skinWeights,
        // particleRaycastBudget, softParticles, realtimeReflectionProbes, maximumLODLevel,
        // shadowCascades, asyncUploadTimeSlice) and drove a render-scale slider. Every one of those
        // is dead here, and the field log says so outright:
        //
        //   [graphics] QualitySettings setters: aa=ok pixellights=ok shadowdist=ok lodcrossfade=ok
        //     skinweights=MISS particleraycast=MISS softparticles=MISS reflectionprobes=MISS
        //     maxlod=MISS shadowcascades=MISS asyncupload=MISS
        //
        // — IL2CPP stripped all seven, so `set_i32`/`set_bool_qs` returned immediately; and render
        // scaling is a deliberate no-op (`display::set_render_scale`, removed 2026-07-17 for
        // corrupting the display). Higher tiers were therefore doing literally nothing extra.
        // They're gone, per this module's own rule: a knob nobody can prove works does not ship.
        set_i32(&QS_ANTIALIAS, 0); // no MSAA
        set_i32(&QS_PIXELLIGHTS, 0); // no per-pixel lights
        set_bool_qs(&QS_LODCROSSFADE, false); // no cross-fade blending between LODs
        let level = LOW_LEVEL.load(Ordering::Relaxed);
        // Tier 2+: shadows OFF entirely (distance 0 == no shadows on the built-in pipeline).
        set_f32(&QS_SHADOWDIST, if level >= 2 { 0.0 } else { 20.0 });
        // Tier 3 · Extreme — drop the GAME's own quality preset to its lowest tier. This is the one
        // large, genuinely effective reduction available on this build, and it is the game's own
        // device-tier switch rather than anything we invented.
        apply_game_quality(if level >= 3 { 0 } else { -1 });
        return;
    }
    // Low Resources off → hand the game's own quality tier back (no-op if we never touched it).
    apply_game_quality(-1);
    // "Default" (-1) means give the game its OWN value back. Just leaving it alone isn't enough: our
    // last write is still in the engine, so Default would silently keep whatever we set last.
    let aa = G_AA.load(Ordering::Relaxed);
    if aa >= 0 {
        set_i32(&QS_ANTIALIAS, aa); // 0/2/4/8
    } else if let Some(o) = original_aa() {
        set_i32(&QS_ANTIALIAS, o);
    }
    // Shadows-off wins over a distance: on the built-in pipeline, distance 0 == no shadows at all
    // (QualitySettings.shadows itself was stripped, so distance is the only lever).
    let sd = f32::from_bits(G_SHADOWDIST.load(Ordering::Relaxed));
    let sh = G_SHADOWS.load(Ordering::Relaxed);
    if sh == 0 {
        set_f32(&QS_SHADOWDIST, 0.0);
    } else if sd > 0.0 {
        set_f32(&QS_SHADOWDIST, sd);
    } else if let Some(o) = original_shadow_dist() {
        set_f32(&QS_SHADOWDIST, o);
    }
    // NOTE: no URP branch. Every render-pipeline getter (GraphicsSettings.currentRenderPipeline /
    // .defaultRenderPipeline / QualitySettings.renderPipeline) returns NULL on this build — the game
    // renders with Unity's BUILT-IN pipeline, so the (fully-intact) UniversalRenderPipelineAsset is
    // unused dead code. QualitySettings above is the only real surface.
}

/// Main-thread tick (from ui_tempo's TweenManager.Update pump). Applies on change, and re-asserts
/// every ~2s so a game-side quality pass can't quietly revert the user's choices.
pub fn pump() {
    let any = has_overrides();
    // !any && touched = the user just cleared everything back to Default — we still owe the engine
    // ONE pass to put its own values back (apply_now does that), after which we go quiet again.
    if !any && !TOUCHED.load(Ordering::Relaxed) {
        return; // nothing overridden, nothing of ours latched → never touch the game's own values
    }
    let now = crate::tools::now_ms();
    let dirty = DIRTY.swap(false, Ordering::Relaxed);
    if !dirty && now.saturating_sub(LAST_APPLY_MS.load(Ordering::Relaxed)) < 2000 {
        return;
    }
    LAST_APPLY_MS.store(now, Ordering::Relaxed);
    let _ = std::panic::catch_unwind(|| unsafe {
        if !PUMP_LOGGED.swap(true, Ordering::Relaxed) {
            log("[graphics] pump alive; applying QualitySettings every 2s");
        }
        apply_now();
        // Read the values straight back OUT of the engine — proves the writes actually landed.
        let g = QS_GET_AA.load(Ordering::Relaxed) as il2cpp::Method;
        if !g.is_null() {
            let p = il2cpp::method_pointer(g);
            if !p.is_null() {
                let f: extern "C" fn(*const c_void) -> i32 = std::mem::transmute(p);
                RB_AA.store(f(g as *const c_void), Ordering::Relaxed);
            }
        }
        let g = QS_GET_SHADOWDIST.load(Ordering::Relaxed) as il2cpp::Method;
        if !g.is_null() {
            let p = il2cpp::method_pointer(g);
            if !p.is_null() {
                let f: extern "C" fn(*const c_void) -> f32 = std::mem::transmute(p);
                RB_SHADOWDIST.store(f(g as *const c_void).to_bits(), Ordering::Relaxed);
            }
        }
    });
}

/// How many times the game has re-applied its graphics quality tier. Only the model-quality unlock
/// rides this hook now (the knobs are pushed by [`pump`] instead, precisely because this can sit at
/// 0 for a long time — it fires when a 3D scene loads, not at the title screen).
static APPLY_COUNT: AtomicU32 = AtomicU32::new(0);
pub fn apply_count() -> u32 {
    APPLY_COUNT.load(Ordering::Relaxed)
}

// ApplyGraphicsQuality(this, quality: i32, force: bool, MethodInfo*)
unsafe extern "C" fn on_apply_quality(this: *mut c_void, quality: i32, force: bool, method: *mut c_void) {
    crate::crashlog::crumb(21);
    let n = APPLY_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 5 {
        log(&format!(
            "[graphics] ApplyGraphicsQuality fired #{n} (quality={quality} force={force}) — knobs applied here"
        ));
    }
    let t = TR_AGQ.load(Ordering::Relaxed);
    if t == 0 {
        return;
    }
    let orig: unsafe extern "C" fn(*mut c_void, i32, bool, *mut c_void) = std::mem::transmute(t);
    // This hook now only chooses the model-quality TIER. The QualitySettings knobs are NOT applied
    // here — pump() owns them, and re-asserts every ~2s, which also undoes anything this pass
    // changed underneath them.
    if LOW_SPEC.load(Ordering::Relaxed) {
        orig(this, 0, true, method); // potato mode: force the lowest toon tier (Toon1280)
    } else if QUALITY_ON.load(Ordering::Relaxed) {
        orig(this, QUALITY_FULL, true, method); // force the full-quality tier
    } else {
        orig(this, quality, force, method);
    }
}

/// Resolve `GraphicSettings` + `QualitySettings` and install the quality hook.
pub fn install() -> Result<(), String> {
    let k = il2cpp::class("Gallop.GraphicSettings");
    if k.is_null() {
        return Err("GraphicSettings not found".into());
    }
    unsafe {
        il2cpp::hook_method(k, "ApplyGraphicsQuality", 2, on_apply_quality as *const (), &TR_AGQ, &D_AGQ)?;
    }
    // ── Probe: why the character-quality tier is not a Low Resources lever (yet) ─────────────────
    // Forcing a lower `GraphicsQuality` is the game's OWN device-tier system and would be by far the
    // biggest real reduction available — but `on_apply_quality` has never fired in a session (the
    // "ApplyGraphicsQuality fired #n" line is absent from every field log), so the tier we choose in
    // there is dead code. Before wiring a direct call we need to know what we'd be calling: whether
    // the method is static (the detour above assumes an instance `this`, which would be wrong), and
    // whether the class exposes a singleton or a plain setter to drive instead. Log it once and read
    // it off the next run — the same probe-then-wire order used for the inspiration flow.
    let agq = il2cpp::method(k, "ApplyGraphicsQuality", 2);
    // Everything `apply_game_quality` needs to drive the tier itself, since the detour never fires.
    AGQ_M.store(agq as usize, Ordering::Relaxed);
    GETQ_M.store(il2cpp::method(k, "GetQuality", 0) as usize, Ordering::Relaxed);
    let uobj = il2cpp::class("UnityEngine.Object");
    if !uobj.is_null() {
        OBJ_FIND.store(il2cpp::method(uobj, "FindObjectOfType", 1) as usize, Ordering::Relaxed);
    }
    // typeof(GraphicSettings), held for the lifetime of the process. A managed System.Type object
    // that nothing roots could be collected between resolve and use, so pin it with a GCHandle and
    // deliberately leak that handle — one object, alive exactly as long as the DLL.
    let ty = il2cpp::type_object(k);
    if !ty.is_null() {
        let h = il2cpp::GCHandle::new(ty, false);
        GFX_TYPE.store(h.target() as usize, Ordering::Relaxed);
        std::mem::forget(h);
    }
    let names = il2cpp::class_methods(k);
    let interesting: Vec<&String> = names
        .iter()
        .filter(|n| {
            let l = n.to_ascii_lowercase();
            l.contains("quality") || l.contains("instance") || l.contains("setting")
        })
        .collect();
    log(&format!(
        "[graphics] GraphicSettings probe: ApplyGraphicsQuality static={} of {} methods; candidates: {:?}",
        if agq.is_null() { "?".to_string() } else { il2cpp::method_is_static(agq).to_string() },
        names.len(),
        interesting
    ));
    log(&format!(
        "[graphics] game-quality driver: ApplyGraphicsQuality={} GetQuality={} FindObjectOfType={} typeof={}",
        ok_miss(AGQ_M.load(Ordering::Relaxed)),
        ok_miss(GETQ_M.load(Ordering::Relaxed)),
        ok_miss(OBJ_FIND.load(Ordering::Relaxed)),
        ok_miss(GFX_TYPE.load(Ordering::Relaxed))
    ));
    // Resolve the QualitySettings static setters. A setter that fails to resolve makes its knob a
    // SILENT no-op (set_i32/set_f32 just return), so report exactly which ones resolved — a silently
    // dead knob is indistinguishable from "the setting does nothing", which is how the old aniso /
    // texture / LOD switches shipped broken. Only knobs that report `ok` are exposed in the UI.
    let qs = il2cpp::class("UnityEngine.QualitySettings");
    if qs.is_null() {
        log("[graphics] QualitySettings class NOT FOUND — every graphics knob is a no-op!");
        return Ok(());
    }
    let mut report: Vec<String> = Vec::new();
    let mut resolve = |names: &[&str], slot: &AtomicUsize, label: &str| {
        for n in names {
            let m = il2cpp::method(qs, n, 1);
            if !m.is_null() {
                slot.store(m as usize, Ordering::Relaxed);
                report.push(format!("{label}=ok"));
                return;
            }
        }
        report.push(format!("{label}=MISS"));
    };
    resolve(&["set_antiAliasing"], &QS_ANTIALIAS, "aa");
    resolve(&["set_pixelLightCount"], &QS_PIXELLIGHTS, "pixellights");
    resolve(&["set_shadowDistance"], &QS_SHADOWDIST, "shadowdist");
    resolve(&["set_enableLODCrossFade"], &QS_LODCROSSFADE, "lodcrossfade");
    QS_GET_AA.store(il2cpp::method(qs, "get_antiAliasing", 0) as usize, Ordering::Relaxed);
    QS_GET_SHADOWDIST.store(il2cpp::method(qs, "get_shadowDistance", 0) as usize, Ordering::Relaxed);
    log(&format!("[graphics] QualitySettings setters: {}", report.join(" ")));
    // Getters are stripped independently of setters — a MISS here only blinds the read-back display,
    // it does NOT mean the matching setter failed.
    log(&format!(
        "[graphics] QualitySettings getters: aa={} shadowdist={}",
        if QS_GET_AA.load(Ordering::Relaxed) == 0 { "MISS" } else { "ok" },
        if QS_GET_SHADOWDIST.load(Ordering::Relaxed) == 0 { "MISS" } else { "ok" }
    ));
    Ok(())
}
