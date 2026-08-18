//! Texture / atlas localization via the PNG-diff pipeline (port of the upstream localization framework's
//! `replace_texture_with_diff_ex`). Solves the GPU-only/DXT5 dead end the old Python pipeline hit: instead of
//! reading the compressed non-CPU-readable texture bytes directly (impossible), it GPU-Blits the
//! texture into a temp RenderTexture, ReadPixels into a fresh RGBA32 CPU texture, composites a PNG
//! "diff" over those pixels, and re-uploads via ImageConversion.LoadImage.
//!
//! Diff rules (per pixel): alpha==0 → keep the original pixel (readback is bottom-up, so flip);
//! opaque #FF00FF → force transparent; otherwise use the diff pixel. The composited full PNG is
//! cached next to the source so subsequent loads skip the GPU readback entirely.
//!
//! ALL il2cpp/GPU work here MUST run on the main/render thread — `replace_ex` runs inline when
//! already on it, else defers onto the `mainthread` pump with the input texture GC-pinned.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use crate::assets::bindings::{self, Color32, Rect, TEXTURE_FORMAT_RGBA32};
use crate::il2cpp::{self, GCHandle, Object};

fn log(m: &str) {
    crate::tools::log(m);
}

/// Blit a (possibly DXT5/GPU-only) texture into a temp RGBA RenderTexture and ReadPixels it back
/// into a CPU RGBA32 Texture2D. Backs up / restores the game's active RenderTexture. MAIN-THREAD ONLY.
fn render_to_texture(this: Object) -> Object {
    let w = bindings::GetDataWidth(this);
    let h = bindings::GetDataHeight(this);
    if w <= 0 || h <= 0 {
        return std::ptr::null_mut();
    }
    let rt = bindings::GetTemporary(w, h);
    if rt.is_null() {
        return std::ptr::null_mut();
    }
    bindings::Blit2(this, rt); // GPU decode DXT5 → RGBA
    let prev = bindings::GetActive();
    bindings::SetActive(rt);
    let out = bindings::new_texture(w, h, TEXTURE_FORMAT_RGBA32, false, false);
    if !out.is_null() {
        bindings::ReadPixels(out, Rect { x: 0.0, y: 0.0, width: w as f32, height: h as f32 }, 0, 0);
    }
    bindings::SetActive(prev); // restore the game's active RT
    bindings::ReleaseTemporary(rt);
    out // held live on the Rust stack (the conservative GC scans it)
}

/// The composite core (== the upstream `replace_texture_with_diff_ex`). MAIN-THREAD ONLY.
unsafe fn do_replace(texture: Object, path: &Path, diff_path: &Path, mark: bool, allow_fallback: bool) -> bool {
    if texture.is_null() {
        return false;
    }
    // G1: no diff file → optional direct full-PNG fallback, else bail.
    let diff_is_file = std::fs::metadata(diff_path).map(|m| m.is_file()).unwrap_or(false);
    if !diff_is_file {
        return if allow_fallback { load_image_file(texture, path, mark) } else { false };
    }
    // G2: freshness — if the cached base image is newer than the diff, direct-load it (skip readback).
    if let (Ok(im), Ok(dm)) = (std::fs::metadata(path), std::fs::metadata(diff_path)) {
        if let (Ok(it), Ok(dt)) = (im.modified(), dm.modified()) {
            if dt < it && load_image_file(texture, path, mark) {
                return true;
            }
        }
    }
    // G3: load the diff strictly as RGBA8 (reject anything else).
    let (mut pixels, dw, dh) = match load_rgba_png(diff_path) {
        Some(v) => v,
        None => {
            log(&format!("[texture] diff not RGBA8: {}", diff_path.display()));
            return false;
        }
    };
    // G4: size guard vs REAL gpu dims (a mismatch would read out of bounds).
    let width = bindings::GetDataWidth(texture) as usize;
    let height = bindings::GetDataHeight(texture) as usize;
    if width == 0 || height == 0 || width != dw as usize || height != dh as usize {
        log(&format!("[texture] size mismatch: gpu {width}x{height} vs diff {dw}x{dh}"));
        return false;
    }
    // G5: GPU readback.
    let new_tex = render_to_texture(texture);
    if new_tex.is_null() {
        return false;
    }
    // G6: GetPixels32 → Color32[] slice (element data at +0x20, bottom-up).
    let arr = bindings::GetPixels32(new_tex, 0);
    if arr.is_null() {
        return false;
    }
    let orig = std::slice::from_raw_parts((arr as *const u8).add(0x20) as *const Color32, width * height);
    // G7: composite into the diff buffer in place.
    for y in 0..height {
        for x in 0..width {
            let s = (y * width + x) * 4;
            let px = &mut pixels[s..s + 4];
            if px[3] == 0 {
                // transparent diff → keep ORIGINAL (flip: readback is bottom-up vs top-down PNG)
                let o = &orig[(height - y - 1) * width + x];
                px[0] = o.r;
                px[1] = o.g;
                px[2] = o.b;
                px[3] = o.a;
            } else if px[0] == 255 && px[1] == 0 && px[2] == 255 && px[3] == 255 {
                // opaque #FF00FF → erase
                px.fill(0);
            } // else: keep the diff pixel
        }
    }
    // G8: encode full RGBA8 PNG (Fast).
    let png = match encode_png(&pixels, width as u32, height as u32) {
        Some(b) => b,
        None => return false,
    };
    // G9: cache next to the source so next time G2 hits and skips the readback.
    if let Some(par) = path.parent() {
        let _ = std::fs::create_dir_all(par);
    }
    let _ = std::fs::write(path, &png);
    // G10: build a managed byte[] and re-upload.
    upload_png(texture, &png, mark)
}

/// Direct full-PNG load (fallback / cache-hit). Reads the file in RUST (not managed
/// File.ReadAllBytes) to avoid an uncatchable C# exception on a missing/locked file.
unsafe fn load_image_file(texture: Object, path: &Path, mark: bool) -> bool {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return false,
    };
    upload_png(texture, &bytes, mark)
}

/// Copy PNG bytes into a managed `byte[]` and call ImageConversion.LoadImage.
unsafe fn upload_png(texture: Object, png: &[u8], mark: bool) -> bool {
    let arr = il2cpp::array_new(bindings::byte_class(), png.len());
    if arr.is_null() {
        return false;
    }
    std::ptr::copy_nonoverlapping(png.as_ptr(), (arr as *mut u8).add(0x20), png.len());
    bindings::LoadImage(texture, arr, mark)
}

fn load_rgba_png(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
    let f = std::fs::File::open(path).ok()?;
    let mut r = png::Decoder::new(std::io::BufReader::new(f)).read_info().ok()?;
    let (ct, bd, w, h) = {
        let i = r.info();
        (i.color_type, i.bit_depth, i.width, i.height)
    };
    if ct != png::ColorType::Rgba || bd != png::BitDepth::Eight {
        return None; // strict RGBA8
    }
    let mut buf = vec![0u8; r.output_buffer_size()];
    let info = r.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    Some((buf, w, h))
}

fn encode_png(px: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut e = png::Encoder::new(&mut out, w, h);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        e.set_compression(png::Compression::Fast);
        let mut wr = e.write_header().ok()?;
        wr.write_image_data(px).ok()?;
    }
    Some(out)
}

/// Diff path = base path with a `.diff.png` extension (foo.png → foo.diff.png), matching the upstream diff naming.
pub fn texture_diff_path(p: &Path) -> PathBuf {
    p.with_extension("diff.png")
}

/// Replace `texture` using `<path>` (cached full PNG) + `<path>.diff.png`. Marshals onto the main
/// thread if needed. `mark` = markNonReadable passed to LoadImage.
pub fn replace_texture_with_diff(texture: Object, path: PathBuf, mark: bool) {
    let diff = texture_diff_path(&path);
    replace_ex(texture, path, diff, mark, true);
}

/// General entry: run inline when already on the main thread (no one-frame flash), else defer with
/// the input texture GC-pinned across the frame.
pub fn replace_ex(texture: Object, path: PathBuf, diff_path: PathBuf, mark: bool, allow_fallback: bool) {
    if crate::mainthread::is_main() {
        unsafe {
            do_replace(texture, &path, &diff_path, mark, allow_fallback);
        }
    } else {
        let h = GCHandle::new(texture, false); // PIN: the closure escapes the GC-scanned stack
        crate::mainthread::post(move || {
            let t = h.target(); // re-fetch the live pointer on the main thread
            if !t.is_null() {
                unsafe {
                    do_replace(t, &path, &diff_path, mark, allow_fallback);
                }
            }
            // h drops here → gchandle_free
        });
    }
}
