//! In-process neural MT engine — NLLB-200-distilled-600M (int8) via CTranslate2 (`ct2rs`).
//!
//! This is the engine behind `mtl.rs`'s `engine_translate` seam. It covers all supported target
//! languages offline, from a single ~600 MB model shipped next to the DLL — no Python, no sidecar,
//! keeping the bot plug-and-play. The model + tokenizer live in `<dll_dir>/nllb/`.
//!
//! Built only with the `nllb` cargo feature (which pulls in ct2rs + the vendored CTranslate2 C++).
//! Without the feature this is an inert stub, so the async pipeline still compiles and the
//! glossary + mtl.json cache keep working.
//!
//! DIRECTION: NLLB needs a source-language and a forced target-language token. The bundled
//! tokenizer.json fixes the SOURCE to `eng_Latn` (Global/Steam client). The TARGET is supplied per
//! request as the decoder `target_prefix` (FLORES code from mtl.rs). JP-client source (`jpn_Jpan`)
//! would need a tokenizer configured for that src_lang — a documented follow-up; English source
//! (the Global client this bot targets) works today.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};

static READY: AtomicBool = AtomicBool::new(false);
/// Engine power state. False = don't hold (or load) the model at all. Written by `runtime::fan_out`
/// when translation is switched off, so the biggest allocation Overseer owns is released rather
/// than sitting resident for a feature nobody is using.
static ENGINE_ACTIVE: AtomicBool = AtomicBool::new(true);
/// True while the model is resident, so the idle-unload path can tell "released" from "never loaded".
static RESIDENT: AtomicBool = AtomicBool::new(false);

fn log(m: &str) {
    crate::tools::log(m);
}

/// True once the model is loaded and inference is available. `mtl::request` gates on this so we
/// never enqueue work the engine can't yet serve.
pub fn ready() -> bool {
    READY.load(Ordering::Relaxed)
}

/// Is the model currently held in RAM? (Memory report / web UI.)
pub fn resident() -> bool {
    RESIDENT.load(Ordering::Relaxed)
}

/// Suspend or resume the translation engine.
///
/// MEMORY: this is the single biggest lever Overseer has. The NLLB-200 600M int8 model is roughly
/// 0.7 GB resident once CTranslate2 has mapped and staged it; the optional 1.3B build is 2 GB+.
/// Before this the model was loaded at boot and held for the whole session whether or not a target
/// language was ever selected, which is most of the reported footprint. Now it follows the
/// translation switch, and (see `maybe_unload_idle`) is released after a configurable idle period.
pub fn set_engine_active(on: bool) {
    if ENGINE_ACTIVE.swap(on, Ordering::Relaxed) == on {
        return;
    }
    if on {
        load_async();
    } else {
        unload("translation suspended");
    }
}

pub fn engine_active() -> bool {
    ENGINE_ACTIVE.load(Ordering::Relaxed)
}

/// Release the model if the translation pipeline has been idle past the configured window.
/// Called from the MTL worker's park loop (so it only ever runs off the game thread).
pub fn maybe_unload_idle() {
    if !RESIDENT.load(Ordering::Relaxed) {
        return;
    }
    let window = crate::settings::mtl_idle_unload_s();
    if window == 0 {
        return; // user chose to keep it resident
    }
    if crate::mtl::idle_ms() >= window.saturating_mul(1000) {
        unload(&format!("idle for {window}s"));
    }
}

/// Directory holding the active CTranslate2 model: `<dll_dir>/nllb-1.3b/` in high-quality mode,
/// else `<dll_dir>/nllb/` (the 600M model). Chosen by the `mtl_hq` setting.
pub fn model_dir() -> std::path::PathBuf {
    let base = crate::paths::dll_dir();
    if crate::settings::mtl_hq() {
        base.join("nllb-1.3b")
    } else {
        base.join("nllb")
    }
}

/// Whether the active model's files are present (so the web UI can report "install the model").
pub fn model_present() -> bool {
    let d = model_dir();
    d.join("model.bin").exists() && d.join("tokenizer.json").exists()
}

/// Whether the high-quality 1.3B model is installed on disk (for the UI to enable its toggle).
pub fn hq_model_present() -> bool {
    let d = crate::paths::dll_dir().join("nllb-1.3b");
    d.join("model.bin").exists() && d.join("tokenizer.json").exists()
}

// ─── real engine (feature = "nllb") ─────────────────────────────────────────────────────────────

#[cfg(feature = "nllb")]
mod engine {
    use super::{log, model_dir, model_present, READY};
    use arc_swap::ArcSwap;
    use ct2rs::tokenizers::auto::Tokenizer as AutoTokenizer;
    use ct2rs::{Config, Translator};
    use once_cell::sync::Lazy;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    type Tr = Translator<AutoTokenizer>;

    // Hot-swappable model handle: the 600M/1.3B toggle rebuilds the Translator on a background thread
    // and swaps it in. CTranslate2's Translator is Send+Sync; the single MTL worker borrows it.
    static TRANSLATOR: Lazy<ArcSwap<Option<Arc<Tr>>>> = Lazy::new(|| ArcSwap::from_pointee(None));
    static LOADING: AtomicBool = AtomicBool::new(false);

    /// Load the active model on a dedicated background thread (inference is 0.4-1.5s/line — never on
    /// the game main thread). Idempotent while a load is already in flight.
    pub fn load_async() {
        spawn_load();
    }

    /// Unload the current model and load the (possibly different) active one — used by the HQ toggle.
    /// Routed through `unload` so the RESIDENT flag stays truthful (a direct `TRANSLATOR.store(None)`
    /// left it claiming the model was in RAM, which then suppressed the idle-unload path forever).
    pub fn reload_model() {
        unload("switching model");
        spawn_load();
    }

    /// Drop the resident model. The `ArcSwap` store is the release point: the previous `Arc<Tr>` has
    /// no other strong holders once the current batch (if any) finishes, so CTranslate2's buffers go
    /// back to the allocator. Safe to call at any time — a translate in flight keeps its own `Arc`.
    pub fn unload(reason: &str) {
        if !super::RESIDENT.swap(false, Ordering::AcqRel) {
            return;
        }
        super::READY.store(false, Ordering::Release);
        TRANSLATOR.store(Arc::new(None));
        log(&format!("[nllb] model released ({reason}) — it reloads automatically on the next translation"));
    }

    fn spawn_load() {
        if !super::ENGINE_ACTIVE.load(Ordering::Relaxed) {
            return; // translation is switched off — don't pay for the model
        }
        if LOADING.swap(true, Ordering::AcqRel) {
            return; // a load is already running
        }
        if !model_present() {
            log("[nllb] model not installed — MTL fallback off; glossary + cache still active");
            LOADING.store(false, Ordering::Release);
            return;
        }
        let _ = std::thread::Builder::new()
            .name("nllb-load".into())
            .spawn(|| {
                let dir = model_dir();
                log(&format!("[nllb] loading model from {} …", dir.display()));
                let t0 = std::time::Instant::now();
                // Cap CTranslate2's CPU pool: default 0 = all cores, which would starve the game's
                // render thread during inference. 2 keeps latency reasonable while staying responsive.
                let cfg = Config {
                    num_threads_per_replica: 2,
                    ..Default::default()
                };
                match Translator::new(&dir, &cfg) {
                    Ok(t) => {
                        TRANSLATOR.store(Arc::new(Some(Arc::new(t))));
                        super::RESIDENT.store(true, Ordering::Release);
                        READY.store(true, Ordering::Release);
                        log(&format!(
                            "[nllb] model ready in {:.1}s — MTL active",
                            t0.elapsed().as_secs_f32()
                        ));
                        crate::mtl::prewarm(); // seed the cache with common UI strings

                    }
                    Err(e) => log(&format!("[nllb] model load failed: {e}")),
                }
                LOADING.store(false, Ordering::Release);
            });
    }

    /// Translate `texts` to FLORES target `tgt`. Source token comes from the tokenizer (eng_Latn);
    /// the target token is forced via `target_prefix`. One output per input (None = empty/failed).
    pub fn translate_batch(_src: &str, tgt: &str, texts: &[String]) -> Vec<Option<String>> {
        let guard = TRANSLATOR.load();
        let Some(t) = guard.as_ref().as_ref() else {
            // Released while idle (or never loaded) and work has arrived → bring it back. The batch
            // that triggered this returns un-translated and is re-queued naturally on the next
            // sighting, which is the same behaviour as "the model hasn't finished loading yet".
            if super::ENGINE_ACTIVE.load(Ordering::Relaxed) && !super::RESIDENT.load(Ordering::Relaxed) {
                spawn_load();
            }
            return vec![None; texts.len()];
        };
        let prefixes: Vec<Vec<String>> = vec![vec![tgt.to_string()]; texts.len()];
        match t.translate_batch_with_target_prefix(texts, &prefixes, &Default::default(), None) {
            Ok(res) => res.into_iter().map(|(r, _)| clean(r, tgt)).collect(),
            Err(e) => {
                log(&format!("[nllb] translate failed: {e}"));
                vec![None; texts.len()]
            }
        }
    }

    /// Clean NLLB detokenized output: drop a leading target-language token, and strip stray special
    /// tokens (NLLB emits a literal `<unk>` for glyphs it can't represent — usually trailing — plus
    /// the odd `</s>`/`<pad>`). Then trim.
    fn clean(s: String, tgt: &str) -> Option<String> {
        let mut t = s.trim();
        if let Some(r) = t.strip_prefix(tgt) {
            t = r.trim();
        }
        let cleaned = t
            .replace("<unk>", "")
            .replace("</s>", "")
            .replace("<pad>", "");
        let cleaned = cleaned.trim();
        if cleaned.is_empty() {
            None
        } else {
            Some(cleaned.to_string())
        }
    }
}

#[cfg(feature = "nllb")]
pub fn load_async() {
    engine::load_async();
}

/// Reload the model (called when the HQ/600M toggle changes). Swaps in the newly-selected model.
#[cfg(feature = "nllb")]
pub fn reload_model() {
    engine::reload_model();
}

#[cfg(feature = "nllb")]
pub fn translate_batch(src: &str, tgt: &str, texts: &[String]) -> Vec<Option<String>> {
    engine::translate_batch(src, tgt, texts)
}

/// Release the resident model.
#[cfg(feature = "nllb")]
fn unload(reason: &str) {
    engine::unload(reason);
}

// ─── stub (feature off) ─────────────────────────────────────────────────────────────────────────

#[cfg(not(feature = "nllb"))]
pub fn reload_model() {}

#[cfg(not(feature = "nllb"))]
pub fn load_async() {
    log("[nllb] built without the `nllb` feature — MTL fallback disabled (glossary + cache only)");
}

#[cfg(not(feature = "nllb"))]
pub fn translate_batch(_src: &str, _tgt: &str, texts: &[String]) -> Vec<Option<String>> {
    texts.iter().map(|_| None).collect()
}

#[cfg(not(feature = "nllb"))]
fn unload(_reason: &str) {
    RESIDENT.store(false, Ordering::Relaxed);
    READY.store(false, Ordering::Relaxed);
}
