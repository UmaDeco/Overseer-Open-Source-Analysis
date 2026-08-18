//! Wave 3 — texture / atlas image localization (ported from the upstream
//! localization framework's Texture2D/AtlasReference hooks).
//!
//! Pure registration onto the Phase-3 AssetBundle dispatcher — no new game hooks. Each callback maps
//! a loaded asset's resource path to a replacement PNG under the pack's assets dir and drives the
//! diff pipeline (`assets::texture`). This is the baked-in-image translation the old Python pipeline's
//! post-upload DXT5 swap could never do; here the texture is decoded via GPU Blit/ReadPixels and re-uploaded.
//!
//! The callbacks fire on the asset-load thread (Unity main / loader coroutine — IL2CPP-attached), so
//! the managed `Sprite.get_texture` call is safe; the GPU work inside `texture::replace_*` auto-defers
//! to the main-thread pump.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::assets::{self, bindings, texture};
use crate::il2cpp::{self, Object};
use crate::localize::AssetMetadata;

/// Byte offset of `Cute.UI.AtlasReference.sprites` (resolved at install; 0 = unresolved).
static ATLAS_SPRITES_OFF: AtomicUsize = AtomicUsize::new(0);

fn log(m: &str) {
    crate::tools::log(m);
}

/// Drop everything after the last `.` (e.g. `atlas/foo.asset` → `atlas/foo`).
fn path_basename(s: &str) -> &str {
    match s.rfind('.') {
        Some(i) => &s[..i],
        None => s,
    }
}

/// The final path component (after the last `/` or `\`).
fn path_filename(s: &str) -> &str {
    match s.rfind(['/', '\\']) {
        Some(i) => &s[i + 1..],
        None => s,
    }
}

/// Only replace an atlas when the pack's sidecar `bundle_name` (if any) matches the bundle the asset
/// actually loaded from — guards against patching a same-named atlas in the wrong bundle. Passes when
/// no constraint is set, or the bundle path is unknown (matches the upstream behavior).
fn check_asset_bundle_name(bundle: Object, metadata: &AssetMetadata) -> bool {
    if let Some(want) = &metadata.bundle_name {
        if let Some(path) = assets::get_bundle_path(bundle) {
            if path_filename(&path) != want.as_str() {
                return false;
            }
        }
    }
    true
}

/// Texture2D localizer: `<pack>/assets/textures/<orig>` (+ petit common-diff special case).
fn tex_cb(_bundle: Object, this: Object, name: &str) {
    if !name.starts_with(assets::ASSET_PATH_PREFIX) {
        return;
    }
    let orig_path = &name[assets::ASSET_PATH_PREFIX.len()..];
    let ld = crate::localize::data();
    let rel = format!("textures/{orig_path}");
    let Some(replace_path) = ld.get_assets_path(&rel) else {
        return;
    };

    // Petit (home-screen chibi) buttons share a "common" diff. `chara/chrXXXX/petit/petit_chr_...`.
    if orig_path.len() == 50
        && orig_path.is_ascii() // guarantees byte indices == char boundaries for the slices below
        && orig_path.starts_with("chara/chr")
        && &orig_path[13..30] == "/petit/petit_chr_"
    {
        let petit_type = &orig_path[42..46];
        if petit_type == "0070" || petit_type == "0071" {
            // The upstream implementation tries the per-texture diff first and falls back to the common diff by reading a
            // sync bool. The upstream replace_ex returns ()/auto-defers, so branch on file existence:
            // per-texture diff present → use it (no fallback); else → common diff (with fallback).
            let per_texture_diff = texture::texture_diff_path(&replace_path);
            if std::fs::metadata(&per_texture_diff).map(|m| m.is_file()).unwrap_or(false) {
                texture::replace_ex(this, replace_path, per_texture_diff, true, false);
            } else if let Some(common_diff) =
                ld.get_assets_path(format!("textures/chara/_chr/petit/petit_chr_{petit_type}.diff.png"))
            {
                texture::replace_ex(this, replace_path, common_diff, true, true);
            }
            return;
        }
    }

    texture::replace_texture_with_diff(this, replace_path, true);
}

/// AtlasReference localizer: all sprites share one texture, so patch `sprites[0]`'s texture once.
fn atlas_cb(bundle: Object, this: Object, name: &str) {
    if !name.starts_with(assets::ASSET_PATH_PREFIX) {
        return;
    }
    let base_path = path_basename(&name[assets::ASSET_PATH_PREFIX.len()..]);
    if !base_path.starts_with("atlas/") {
        return;
    }
    let rel = format!("{base_path}.png");
    let ld = crate::localize::data();
    let Some(replace_path) = ld.get_assets_path(&rel) else {
        return;
    };
    let metadata = ld.load_asset_metadata(&rel);
    if !check_asset_bundle_name(bundle, &metadata) {
        return;
    }

    let off = ATLAS_SPRITES_OFF.load(Ordering::Relaxed);
    if off == 0 {
        return;
    }
    unsafe {
        let sprites = *((this as *const u8).add(off) as *const Object);
        if sprites.is_null() {
            return;
        }
        // il2cpp array: max_length (element count) @ +0x18, element data @ +0x20.
        let len = *((sprites as *const u8).add(0x18) as *const usize);
        if len == 0 {
            return;
        }
        let sprite0 = *((sprites as *const u8).add(0x20) as *const Object);
        if sprite0.is_null() {
            return;
        }
        let tex = bindings::get_texture(sprite0);
        if tex.is_null() {
            return;
        }
        texture::replace_texture_with_diff(tex, replace_path, true);
    }
}

/// Register the texture + atlas localizers on the dispatcher. Call AFTER `assets::install` (so the
/// dispatcher is armed and `bindings::init` has resolved `Sprite.get_texture`). Non-fatal on misses.
pub fn install() -> Result<(), String> {
    let tex2d = il2cpp::class("UnityEngine.Texture2D");
    if tex2d.is_null() {
        return Err("Texture2D not found".into());
    }
    assets::register_asset_localizer(tex2d, tex_cb);

    let atlas = il2cpp::class("Cute.UI.AtlasReference");
    if !atlas.is_null() {
        if let Some(off) = il2cpp::field_offset(atlas, "sprites") {
            ATLAS_SPRITES_OFF.store(off, Ordering::Relaxed);
            assets::register_asset_localizer(atlas, atlas_cb);
        } else {
            log("[loc/tex] AtlasReference.sprites offset missing — atlas localization off");
        }
    } else {
        log("[loc/tex] Cute.UI.AtlasReference not found — atlas localization off");
    }
    Ok(())
}
