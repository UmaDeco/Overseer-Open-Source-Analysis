//! LocalizedData content store — the translation-pack backbone (ported from the upstream
//! localization framework's core content store). Holds the active pack's six lookup dicts + assets path + pack config behind
//! an `ArcSwap` for lock-free reads and hot-reload. Every localization hook (Localize::Get, the
//! SQLite DB hooks, the texture/atlas localizer, story, …) reads via `data()`.
//!
//! A pack is a folder next to the DLL — default `localized_data/` — containing:
//!   * `config.json`  → `LocalizedDataConfig` (which dict file is which, assets dir, font, …)
//!   * the dict JSON files it names (localize/hashed/text_data/character_system_text/race jikkyo)
//!   * an assets dir of replacement PNGs (+ optional `<name>.json` sidecars carrying `bundle_name`)
//! This is exactly the community translation-pack `localized_data` layout, so existing packs load unchanged.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use fnv::FnvHashMap;
use once_cell::sync::Lazy;
use serde::de::DeserializeOwned;
use serde::Deserialize;

fn default_one_f32() -> f32 {
    1.0
}

static DATA: Lazy<ArcSwap<LocalizedData>> = Lazy::new(|| ArcSwap::from_pointee(LocalizedData::default()));

fn log(m: &str) {
    crate::tools::log(m);
}

/// The active localized data (cheap Arc clone, lock-free). Hooks hold the returned Arc for the
/// duration of one lookup so a concurrent `reload()` can't free it mid-use.
pub fn data() -> Arc<LocalizedData> {
    DATA.load_full()
}

/// True if a translation pack is currently loaded.
pub fn is_loaded() -> bool {
    DATA.load().path.is_some()
}

/// The active pack directory next to the DLL, if present (default `localized_data/`).
fn active_pack_dir() -> Option<PathBuf> {
    let p = crate::paths::dll_dir().join("localized_data");
    if p.is_dir() {
        Some(p)
    } else {
        None
    }
}

/// (Re)load the active pack from disk and hot-swap it in. Safe to call from any thread at any time.
pub fn reload() {
    let new = match active_pack_dir() {
        Some(dir) => LocalizedData::load(&dir),
        None => LocalizedData::default(),
    };
    let (l, h, t) = new.stats();
    let has = new.path.is_some();
    DATA.store(Arc::new(new));
    // A pack swap may change the replacement font — drop the cached font handles so it reloads.
    crate::loc_font::invalidate();
    if has {
        log(&format!("[localize] pack loaded: {l} ui strings, {h} hashed, {t} db-text entries"));
    } else {
        log("[localize] no pack (drop a localized_data/ folder next to the DLL)");
    }
}

/// The active translation pack's content. Cloned-by-Arc per lookup batch.
#[derive(Default)]
pub struct LocalizedData {
    pub config: LocalizedDataConfig,
    path: Option<PathBuf>,
    assets_path: Option<PathBuf>,

    pub localize_dict: FnvHashMap<String, String>, // TextId-name → text
    pub hashed_dict: FnvHashMap<u64, String>,      // content hash → text
    pub text_data_dict: FnvHashMap<i32, FnvHashMap<i32, String>>, // category → index → text
    pub character_system_text_dict: FnvHashMap<i32, FnvHashMap<i32, String>>, // char → voice → text
    pub race_jikkyo_comment_dict: FnvHashMap<i32, String>, // id → text
    pub race_jikkyo_message_dict: FnvHashMap<i32, String>, // id → text

    // gettext plural/ordinal rules from the pack config (default = always form 0).
    pub plural_form: crate::plurals::Resolver,
    pub ordinal_form: crate::plurals::Resolver,
}

impl LocalizedData {
    fn load(dir: &Path) -> LocalizedData {
        let config: LocalizedDataConfig = match std::fs::read_to_string(dir.join("config.json")) {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(c) => c,
                Err(e) => {
                    log(&format!("[localize] bad config.json: {e}"));
                    LocalizedDataConfig::default()
                }
            },
            Err(_) => {
                log("[localize] pack has no config.json");
                LocalizedDataConfig::default()
            }
        };
        let assets_path = config.assets_dir.as_ref().map(|d| dir.join(d));
        LocalizedData {
            localize_dict: load_dict(dir, config.localize_dict.as_deref()),
            hashed_dict: load_dict(dir, config.hashed_dict.as_deref()),
            text_data_dict: load_dict(dir, config.text_data_dict.as_deref()),
            character_system_text_dict: load_dict(dir, config.character_system_text_dict.as_deref()),
            race_jikkyo_comment_dict: load_dict(dir, config.race_jikkyo_comment_dict.as_deref()),
            race_jikkyo_message_dict: load_dict(dir, config.race_jikkyo_message_dict.as_deref()),
            assets_path,
            plural_form: parse_plural(config.plural_form.as_deref()),
            ordinal_form: parse_plural(config.ordinal_form.as_deref()),
            path: Some(dir.to_path_buf()),
            config,
        }
    }

    // ── lookups (borrow into the Arc held by the caller) ──
    pub fn localize(&self, key: &str) -> Option<&str> {
        self.localize_dict.get(key).map(String::as_str)
    }
    pub fn hashed(&self, hash: u64) -> Option<&str> {
        self.hashed_dict.get(&hash).map(String::as_str)
    }
    pub fn text_data(&self, category: i32, index: i32) -> Option<&str> {
        self.text_data_dict.get(&category).and_then(|m| m.get(&index)).map(String::as_str)
    }
    pub fn character_system_text(&self, char_id: i32, voice_id: i32) -> Option<&str> {
        self.character_system_text_dict.get(&char_id).and_then(|m| m.get(&voice_id)).map(String::as_str)
    }
    pub fn race_jikkyo_comment(&self, id: i32) -> Option<&str> {
        self.race_jikkyo_comment_dict.get(&id).map(String::as_str)
    }
    pub fn race_jikkyo_message(&self, id: i32) -> Option<&str> {
        self.race_jikkyo_message_dict.get(&id).map(String::as_str)
    }

    /// Absolute path of a replacement asset under the pack's assets dir (None if no pack/assets dir).
    pub fn get_assets_path<P: AsRef<Path>>(&self, rel: P) -> Option<PathBuf> {
        self.assets_path.as_ref().map(|p| p.join(rel))
    }

    /// Absolute path of a file in the pack's ROOT dir (None if no pack). Used for the extra font
    /// asset-bundle (config.extra_asset_bundle).
    pub fn get_data_path<P: AsRef<Path>>(&self, rel: P) -> Option<PathBuf> {
        self.path.as_ref().map(|p| p.join(rel))
    }

    /// Load a typed JSON sidecar from the pack's assets dir (silent on missing file). Used by the
    /// StoryTimeline localizer (`<base>.json` → StoryTimelineDataDict).
    pub fn load_assets_dict<T: DeserializeOwned, P: AsRef<Path>>(&self, rel: P) -> Option<T> {
        let base = self.assets_path.as_ref()?;
        let p = base.join(rel);
        std::fs::read_to_string(&p).ok().and_then(|s| serde_json::from_str(&s).ok())
    }

    /// Load a per-asset info sidecar (`<assets>/<rel>.json`) carrying `windows` metadata + a typed
    /// `data` payload (Flash AnRootData / FlashActionPlayerData). Default if missing.
    pub fn load_asset_info<T: DeserializeOwned, P: AsRef<Path>>(&self, rel: P) -> AssetInfo<T> {
        let mut p = rel.as_ref().to_path_buf();
        p.set_extension("json");
        self.load_assets_dict(&p).unwrap_or_default()
    }

    /// Per-asset sidecar metadata (`<assets>/<rel>.json` → `windows.bundle_name`) — for the texture
    /// localizer's bundle verification guard.
    pub fn load_asset_metadata<P: AsRef<Path>>(&self, rel: P) -> AssetMetadata {
        self.load_asset_info::<serde_json::Value, _>(rel).windows
    }

    /// (ui strings, hashed entries, total db-text entries) — for status/diagnostics.
    pub fn stats(&self) -> (usize, usize, usize) {
        (
            self.localize_dict.len(),
            self.hashed_dict.len(),
            self.text_data_dict.values().map(|m| m.len()).sum(),
        )
    }
}

/// Parse a gettext plural-form expression from the pack config into a resolver (default = form 0).
fn parse_plural(expr: Option<&str>) -> crate::plurals::Resolver {
    expr.and_then(|s| crate::plurals::Ast::parse(s).ok())
        .map(crate::plurals::Resolver::Expr)
        .unwrap_or_default()
}

/// Load one dict JSON file named in the pack config into a fast FNV map. Missing file → empty map;
/// a parse error is logged and treated as empty (never fatal).
fn load_dict<K, V>(dir: &Path, rel: Option<&str>) -> FnvHashMap<K, V>
where
    K: std::cmp::Eq + std::hash::Hash + serde::de::DeserializeOwned,
    V: serde::de::DeserializeOwned,
{
    let Some(rel) = rel else {
        return FnvHashMap::default();
    };
    let path = dir.join(rel);
    match std::fs::read_to_string(&path) {
        Ok(s) => match serde_json::from_str(&s) {
            Ok(m) => m,
            Err(e) => {
                log(&format!("[localize] parse {}: {e}", path.display()));
                FnvHashMap::default()
            }
        },
        Err(_) => FnvHashMap::default(),
    }
}

/// The pack's `config.json`. Unknown fields are ignored, so richer real packs load fine; fields not
/// yet consumed by a ported hook are kept so later waves (wrap/fit, font, story) have them.
#[derive(Deserialize, Clone, Default)]
pub struct LocalizedDataConfig {
    pub localize_dict: Option<String>,
    pub hashed_dict: Option<String>,
    pub text_data_dict: Option<String>,
    pub character_system_text_dict: Option<String>,
    pub race_jikkyo_comment_dict: Option<String>,
    pub race_jikkyo_message_dict: Option<String>,
    pub assets_dir: Option<String>,
    pub replacement_font_name: Option<String>,
    #[serde(default)]
    pub extra_asset_bundle: OsOption,
    pub line_width_multiplier: Option<f32>,
    #[serde(default)]
    pub use_text_wrapper: bool,
    #[serde(default)]
    pub remove_ruby: bool,
    #[serde(default)]
    pub auto_adjust_story_clip_length: bool,
    pub story_line_count_offset: Option<i32>,
    pub plural_form: Option<String>,
    pub ordinal_form: Option<String>,
    #[serde(default)]
    pub ordinal_types: Vec<String>,
    #[serde(default)]
    pub months: Vec<String>,
    pub month_text_format: Option<String>,

    // Flags consumed by later-wave hooks (folded onto the pack config so every hook reads one
    // ArcSwap snapshot). The auto-TL / translator / IPC flags default off — Sugoi MTL + IPC are
    // non-core and stubbed for now.
    #[serde(default)]
    pub text_common_allow_overflow: bool,
    #[serde(default)]
    pub disable_skill_name_translation: bool,
    pub text_frame_font_size_multiplier: Option<f32>,
    #[serde(default = "default_one_f32")]
    pub story_tcps_multiplier: f32,
    #[serde(default)]
    pub auto_translate_stories: bool,
    #[serde(default)]
    pub auto_translate_localize: bool,
    #[serde(default)]
    pub translator_mode: bool,
    #[serde(default)]
    pub enable_ipc: bool,
}

/// Overseer's per-OS option shape (`{ "windows": "...", "android": "..." }`); we read the windows arm.
#[derive(Deserialize, Clone, Default)]
pub struct OsOption {
    #[serde(default)]
    pub windows: Option<String>,
}

impl OsOption {
    pub fn get(&self) -> Option<&str> {
        self.windows.as_deref()
    }
}

/// Per-asset sidecar (`{ "windows": { "bundle_name": "..." }, "data": {...} }`). `T` is the typed
/// `data` payload (e.g. Flash AnRootData); use `serde_json::Value` / `()` when only metadata matters.
#[derive(Deserialize)]
pub struct AssetInfo<T> {
    #[serde(default)]
    pub windows: AssetMetadata,
    pub data: Option<T>,
}

impl<T> Default for AssetInfo<T> {
    fn default() -> Self {
        Self { windows: AssetMetadata::default(), data: None }
    }
}

impl<T> AssetInfo<T> {
    pub fn metadata(self) -> AssetMetadata {
        self.windows
    }
    pub fn metadata_ref(&self) -> &AssetMetadata {
        &self.windows
    }
}

#[derive(Deserialize, Clone, Default)]
pub struct AssetMetadata {
    pub bundle_name: Option<String>,
}
