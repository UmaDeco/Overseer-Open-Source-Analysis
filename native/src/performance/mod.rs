//! Performance / graphics tweak modules.
//!
//! Groups the four single-responsibility knobs that ship in every build:
//! - [`fps`]      — FPS unlock (targetFrameRate/vSyncCount clamp hooks)
//! - [`graphics`] — model-quality tier + the live QualitySettings knobs (AA / shadows, low-spec)
//! - [`cyspring`] — cloth physics (CySpringController.Init UpdateMode, low-spec 60fps cap)
//! - [`display`]  — window/resolution QoL (always-on-top, SetResolution, UI-scale)

pub mod fps;
pub mod graphics;
pub mod cyspring;
pub mod display;

/// Low-resources "potato" master mode: fan out to every subsystem that reacts to it.
/// (display has no low-spec behaviour — see its module docs.)
pub fn set_low_spec(on: bool) {
    graphics::set_low_spec(on);
    cyspring::set_low_spec(on);
}

/// Low Resources tier (1 Balanced / 2 Aggressive / 3 Extreme).
pub fn set_low_level(v: i32) {
    graphics::set_low_level(v);
}
pub fn low_level() -> i32 {
    graphics::low_level()
}

/// Nothing here fans out to `display` any more. The tiers used to push a render scale through
/// `display::set_render_scale`, which has been an explicit no-op since 2026-07-17 (it corrupted the
/// display and persisted in the Unity registry) — so that call chain drove nothing at all. The
/// remaining tier work is entirely inside `graphics::apply_now` and `cyspring`.

/// Apply persisted settings to the whole performance domain at boot.
pub fn apply(s: &crate::settings::Settings) {
    fps::set_cap(s.fps);
    cyspring::set_enabled(s.cyspring_uncap);
    graphics::set_quality_unlocked(s.gfx_quality);
    graphics::set_aa(s.gfx_aa);
    graphics::set_shadows(s.gfx_shadows);
    graphics::set_shadow_distance(s.gfx_shadow_dist);
    display::set_block_minimize(s.block_minimize);
    display::set_display_mode(s.display_mode);
    display::set_render_scale(s.render_scale);
    display::set_ui_scale(s.ui_scale);
    display::set_always_on_top(s.always_on_top);
    // Tier BEFORE the master toggle: `set_low_spec` pushes the tier's render scale, so it has to
    // already know which tier it is on (otherwise boot applies tier 1's scale and the saved tier
    // only takes effect on the next change).
    graphics::set_low_level(s.low_level);
    set_low_spec(s.low_spec);
}
