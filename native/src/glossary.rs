//! Phase 5 — Overseer's dynamic glossary translation layer (ported from `translation_manager.py`
//! `_translate_string` / `_load_glossary`, the glossary-only path — argos MTL is deferred to a local
//! HTTP backend).
//!
//! Wired as a MISS-FALLBACK under the translation pack inside `Localize::Get`: when the pack has no entry
//! for a UI string, the game's own English text is run through the active language's two-tier
//! glossary — exact-line UI labels (`glossary_ui.json`) then word-boundary substring game-terms
//! (`glossary.json`, longest-first) — giving instant in-process translation into any of Overseer's
//! 26 languages, with NO pack required. Results are memoized (each unique English string is processed
//! once) so the hot path amortizes to a hashmap lookup.
//!
//! Data: `<dll_dir>/glossary/<lang>/{glossary_ui.json, glossary.json}`. Active language = the
//! `tl_lang` setting (empty/None → layer disabled, zero overhead).

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use fnv::FnvHashMap;
use once_cell::sync::Lazy;
use std::sync::Mutex;

/// Letters that count as "inside a word" for boundary checks (ASCII + Spanish accents), matching
/// Overseer's `_LETTER`. Fixed-width lookbehind/ahead over this class = word boundary.
const LETTER: &str = "A-Za-z\u{c1}\u{c9}\u{cd}\u{d3}\u{da}\u{dc}\u{d1}\u{e1}\u{e9}\u{ed}\u{f3}\u{fa}\u{fc}\u{f1}";

static DATA: Lazy<ArcSwap<Glossary>> = Lazy::new(|| ArcSwap::from_pointee(Glossary::default()));
/// English source → translated (or None = no translation). Cleared on reload.
static MEMO: Lazy<Mutex<FnvHashMap<String, Option<String>>>> = Lazy::new(|| Mutex::new(FnvHashMap::default()));

fn log(m: &str) {
    crate::tools::log(m);
}

#[derive(Default)]
pub struct Glossary {
    active: bool,
    lang: String,
    exact: HashMap<String, String>, // trimmed whole-line UI label → translation
    // Substring game-terms compiled into ONE word-boundary alternation regex (longest-first, so the
    // longest term wins) + a matched-term → replacement map. This makes the hot Localize::Get path a
    // single regex pass with O(1) lookups instead of one pass PER term (which stalled the main thread).
    sub_re: Option<fancy_regex::Regex>,
    sub_map: HashMap<String, String>,
}

/// The target languages we support (code → English name). Burmese/Khmer dropped per project scope.
pub const LANGUAGES: &[(&str, &str)] = &[
    ("es", "Spanish"), ("fr", "French"), ("de", "German"), ("pt", "Portuguese"),
    ("it", "Italian"), ("ru", "Russian"), ("ja", "Japanese"), ("zh", "Chinese"),
    ("ko", "Korean"), ("nl", "Dutch"), ("pl", "Polish"), ("tr", "Turkish"),
    ("ar", "Arabic"), ("hi", "Hindi"), ("id", "Indonesian"), ("vi", "Vietnamese"),
    ("uk", "Ukrainian"), ("cs", "Czech"), ("sv", "Swedish"), ("ro", "Romanian"),
    ("th", "Thai"), ("tl", "Filipino"), ("ms", "Malay"), ("lo", "Lao"),
];

/// True if a glossary is loaded for the active language.
pub fn active() -> bool {
    DATA.load().active
}

/// The active language code, or "" if disabled.
pub fn lang() -> String {
    DATA.load().lang.clone()
}

/// English display name of the active language, if any.
pub fn active_name() -> Option<&'static str> {
    let l = lang();
    LANGUAGES.iter().find(|(c, _)| *c == l).map(|(_, n)| *n)
}

/// (ui-label count, term count) currently loaded.
pub fn counts() -> (usize, usize) {
    let g = DATA.load();
    (g.exact.len(), g.sub_map.len())
}

/// Whether a glossary data folder exists next to the DLL for `code`.
pub fn has_data(code: &str) -> bool {
    let base = crate::paths::dll_dir().join("glossary").join(code);
    base.join("glossary.json").exists() || base.join("glossary_ui.json").exists()
}

/// (Re)load the glossary for the `tl_lang` setting and clear the memo. Safe from any thread.
pub fn reload() {
    let lang = crate::settings::tl_lang().unwrap_or_default();
    let g = if lang.is_empty() { Glossary::default() } else { Glossary::load(&lang) };
    if g.active {
        log(&format!("[glossary] {} loaded: {} UI labels, {} terms", lang, g.exact.len(), g.sub_map.len()));
    }
    DATA.store(Arc::new(g));
    if let Ok(mut m) = MEMO.lock() {
        m.clear();
    }
}

/// Translate a game English string via the active glossary, or None if disabled / no change.
pub fn translate(s: &str) -> Option<String> {
    if s.is_empty() {
        return None;
    }
    if let Ok(m) = MEMO.lock() {
        if let Some(r) = m.get(s) {
            return r.clone();
        }
    }
    let g = DATA.load();
    let r = if g.active { g.translate_inner(s) } else { None };
    if let Ok(mut m) = MEMO.lock() {
        // Bound the memo so a flood of unique strings can't grow it without limit.
        if m.len() > 20_000 {
            m.clear();
        }
        m.insert(s.to_string(), r.clone());
    }
    r
}

impl Glossary {
    fn load(lang: &str) -> Glossary {
        let dir = crate::paths::dll_dir().join("glossary").join(lang);

        // Exact-line UI labels: keep only k != v, trimmed key.
        let exact: HashMap<String, String> = read_json(&dir.join("glossary_ui.json"))
            .into_iter()
            .filter_map(|(k, v)| {
                let k = k.trim().to_string();
                if !k.is_empty() && !v.is_empty() && k != v {
                    Some((k, v))
                } else {
                    None
                }
            })
            .collect();

        // Substring terms → ONE combined alternation regex (longest-first so longer terms win in
        // fancy-regex's leftmost-first matching) + a term→replacement map.
        let sub = read_json(&dir.join("glossary.json"));
        let mut keys: Vec<String> = sub.keys().filter(|k| !k.is_empty() && !sub[*k].is_empty()).cloned().collect();
        keys.sort_by(|a, b| b.chars().count().cmp(&a.chars().count()));
        let mut sub_map = HashMap::with_capacity(keys.len());
        let mut alts = Vec::with_capacity(keys.len());
        for k in &keys {
            alts.push(regex_escape(k));
            sub_map.insert(k.clone(), sub[k].clone());
        }
        let sub_re = if alts.is_empty() {
            None
        } else {
            let pat = format!("(?<![{LETTER}])(?:{})(?![{LETTER}])", alts.join("|"));
            match fancy_regex::Regex::new(&pat) {
                Ok(r) => Some(r),
                Err(e) => {
                    log(&format!("[glossary] combined regex failed: {e}"));
                    None
                }
            }
        };

        let active = !exact.is_empty() || sub_re.is_some();
        Glossary { active, lang: lang.to_string(), exact, sub_re, sub_map }
    }

    fn translate_inner(&self, s: &str) -> Option<String> {
        // 1. whole-line UI label (preserve surrounding whitespace).
        let key = s.trim();
        if let Some(v) = self.exact.get(key) {
            let lead = &s[..s.len() - s.trim_start().len()];
            let trail = &s[s.trim_end().len()..];
            return Some(format!("{lead}{v}{trail}"));
        }
        // 2. substring game-terms — ONE combined-regex pass. Skip very long free-form strings (not
        //    glossary territory; that's MTL) so a huge dialogue line never triggers a slow scan.
        let re = self.sub_re.as_ref()?;
        if s.len() > 2000 {
            return None;
        }
        let mut out = String::with_capacity(s.len());
        let mut last = 0usize;
        let mut changed = false;
        let mut iter = re.find_iter(s);
        while let Some(Ok(m)) = iter.next() {
            out.push_str(&s[last..m.start()]);
            match self.sub_map.get(m.as_str()) {
                Some(v) => {
                    out.push_str(v);
                    changed = true;
                }
                None => out.push_str(m.as_str()),
            }
            last = m.end();
        }
        if !changed {
            return None;
        }
        out.push_str(&s[last..]);
        Some(out)
    }
}

/// Read a `{string: string}` JSON map (empty on any error).
fn read_json(path: &Path) -> HashMap<String, String> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

/// Escape regex metacharacters in a literal glossary key.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if "\\.+*?()|[]{}^$#-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
