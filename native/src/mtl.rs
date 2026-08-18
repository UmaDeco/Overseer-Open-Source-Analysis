//! Async machine-translation layer — the engine-agnostic plumbing that makes free-form dialogue
//! (which the synchronous glossary can't cover) translate without blocking the game.
//!
//! Ported from Overseer's translation_manager.py worker + translation_agent.js tracked/reapply loop,
//! hardened for in-process Rust:
//!   * `lookup(src)`   — one in-memory cache read on the hot text path → INSTANT for repeats.
//!   * `request(src)`  — eligibility-gate + enqueue a glossary-miss for the background worker.
//!   * `on_miss(...)`  — request + register the on-screen component (via a GC handle) for re-apply.
//!   * worker thread   — drains a batch, calls the NLLB engine (nllb.rs), writes results into the
//!                       persistent per-language `mtl.json` cache, and queues them for re-apply.
//!   * `pump()`        — runs on the main-thread tween pump; re-applies arrived translations to the
//!                       still-live components so already-painted screens (result Log, choices)
//!                       update themselves without reopening.
//!
//! HARD INVARIANT: the worker thread NEVER touches IL2CPP (no new_string/read_string/GCHandle::target
//! /setter calls) — those happen only on the main thread (in the setter hook or `pump()`). A stray
//! attached background thread during a GC crashes the game (see boot.rs shutdown notes). The worker
//! deals only in owned Rust `String`s and file I/O.
//!
//! SOURCE DIRECTION: the game's own text is English on the Global/Steam client and Japanese on the JP
//! client; `source_flores()` switches the NLLB source token accordingly, so JP→target works too.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use arc_swap::ArcSwap;
use fnv::FnvHashMap;
use once_cell::sync::Lazy;

use crate::il2cpp::{self, GCHandle, Method, Object};

fn log(m: &str) {
    crate::tools::log(m);
}

/// `void set_text(this, System.String, MethodInfo*)` — the setter trampoline signature (matches
/// loc_settext.rs) used to re-apply a late translation to a component.
type SetFn = unsafe extern "C" fn(Object, Object, Method);

// ─── source/target language direction (FLORES-200 codes) ───────────────────────────────────────

/// NLLB source code = the game's own script: Japanese on the JP client, English on Global/Steam.
fn source_flores() -> &'static str {
    if crate::loc_ui::is_jp_client() {
        "jpn_Jpan"
    } else {
        "eng_Latn"
    }
}

/// FLORES-200 target for a supported UI language code, or None if we don't translate to it.
/// (Burmese `my` / Khmer `km` intentionally dropped per project scope; `en` present so JP→English
/// works.)
fn target_flores(code: &str) -> Option<&'static str> {
    Some(match code {
        "en" => "eng_Latn",
        "es" => "spa_Latn",
        "fr" => "fra_Latn",
        "de" => "deu_Latn",
        "pt" => "por_Latn",
        "it" => "ita_Latn",
        "ru" => "rus_Cyrl",
        "ja" => "jpn_Jpan",
        "zh" => "zho_Hans",
        "ko" => "kor_Hang",
        "nl" => "nld_Latn",
        "pl" => "pol_Latn",
        "tr" => "tur_Latn",
        "ar" => "arb_Arab",
        "hi" => "hin_Deva",
        "id" => "ind_Latn",
        "vi" => "vie_Latn",
        "uk" => "ukr_Cyrl",
        "cs" => "ces_Latn",
        "sv" => "swe_Latn",
        "ro" => "ron_Latn",
        "th" => "tha_Thai",
        "tl" => "tgl_Latn",
        "ms" => "zsm_Latn",
        "lo" => "lao_Laoo",
        _ => return None,
    })
}

// ─── state ──────────────────────────────────────────────────────────────────────────────────────

struct Inner {
    queue: VecDeque<String>,  // bulk lane (pre-warm, non-tracked) — FIFO
    pqueue: VecDeque<String>, // PRIORITY lane (text on a live component) — newest-first LIFO
    pending: HashSet<String>, // in either queue or in flight — dedups the same line seen every frame
    epoch: u64,               // language generation; stamped on results so stale ones are dropped
}

static REQ: Lazy<Mutex<Inner>> = Lazy::new(|| {
    Mutex::new(Inner {
        queue: VecDeque::new(),
        pqueue: VecDeque::new(),
        pending: HashSet::new(),
        epoch: 0,
    })
});
static CV: Condvar = Condvar::new();

/// Sources the engine already tried and returned identity/failure for — never retried (Overseer
/// `_attempted`). Bounded.
static ATTEMPTED: Lazy<Mutex<HashSet<String>>> = Lazy::new(|| Mutex::new(HashSet::new()));

/// Positive cache: source → translation (the persistent mtl.json, in memory). A `lookup` hit is
/// served synchronously by the setter hook, same speed as a glossary hit.
static CACHE: Lazy<Mutex<FnvHashMap<String, String>>> = Lazy::new(|| Mutex::new(FnvHashMap::default()));

/// Hashes of every translation OUTPUT we've ever produced (the cache's values). The feedback-loop
/// guard the EMITTED set can't be: EMITTED is generational and ages out, after which our own French
/// gets re-fed as a "source" and re-translated into garble ("Indice Lvl 1" => "L'indice Lvl est de
/// 1.", "Until 15:00…" => "Je suis d'accord avec…"). This set is populated from the whole cache at
/// load and on every worker insert, so "is this string something WE made?" is answerable in O(1)
/// forever, not just for two generations. RwLock: hot paths only read.
static OUT_HASHES: Lazy<std::sync::RwLock<HashSet<u64>>> =
    Lazy::new(|| std::sync::RwLock::new(HashSet::new()));

fn out_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Is `s` one of our own translation outputs? If yes, it must be forwarded untouched, never
/// re-translated. Checked by every text hook right after the (generational) `is_emitted`.
pub fn is_own_output(s: &str) -> bool {
    OUT_HASHES.read().map(|h| h.contains(&out_hash(s))).unwrap_or(false)
}

fn note_output(s: &str) {
    if let Ok(mut h) = OUT_HASHES.write() {
        h.insert(out_hash(s));
    }
}
/// Public wrapper: loc_settext::record_emitted feeds glossary/cache hits (our output too) in here.
pub fn note_output_pub(s: &str) {
    note_output(s);
}

// ─── recent-translations feed (web UI "what just translated" / click-to-fix) ────────────────────

/// Cap on the recent-translations ring buffer — oldest entries drop off the back past this.
const RECENT_CAP: usize = 150;

/// Ring buffer of the most recent (source, output) pairs actually swapped onto on-screen text,
/// NEWEST at the FRONT. Feeds the web UI's click-to-fix list (GET /api/translation/recent). Written
/// on the game's text-hook thread at the synchronous-swap chokepoint (loc_settext::dispatch) — NOT
/// per-frame — so the cost is one lock + two small allocations only when a genuine, novel
/// translation is applied.
static RECENT: Mutex<VecDeque<(String, String)>> = Mutex::new(VecDeque::new());

/// Record a translation just applied to on-screen text: `src` (original) → `dst` (shown output).
/// Skips the entries that would only add noise to the click-to-fix feed:
///   * `src` empty, or `dst` empty,
///   * identity (`src == dst`) — a pass-through, nothing was translated,
///   * `is_own_output(src)` — the SOURCE is our own prior output the game re-fed back through
///     set_text (a feedback re-paint, not a real original→target swap),
///   * an exact duplicate of the current FRONT entry — the same line re-painted (dedup consecutive).
/// Cheap: one lock + the two `to_string()`s, and only on a real new swap.
///
/// This MUST test `src`, not `dst`: the caller (`loc_settext::dispatch`) runs `record_emitted(dst)`
/// immediately before calling us, which permanently marks `dst` as our own output — so testing
/// `is_own_output(dst)` was true for EVERY real translation and silently dropped the whole feed
/// (the bug that left "Recently translated" always empty).
pub fn note_recent(src: &str, dst: &str) {
    if src.is_empty() || dst.is_empty() || src == dst || is_own_output(src) {
        return;
    }
    if let Ok(mut q) = RECENT.lock() {
        if let Some((fs, fd)) = q.front() {
            if fs == src && fd == dst {
                return; // consecutive duplicate — the same line painted again
            }
        }
        q.push_front((src.to_string(), dst.to_string()));
        while q.len() > RECENT_CAP {
            q.pop_back();
        }
    }
}

/// Up to `limit` newest (source, output) pairs, FRONT (newest) first — for the web UI feed.
pub fn recent(limit: usize) -> Vec<(String, String)> {
    RECENT
        .lock()
        .map(|q| q.iter().take(limit).cloned().collect())
        .unwrap_or_default()
}

/// Which language `CACHE`/`DIRTY` belong to (so `flush()` writes to the right file even after
/// `set_tl_lang` has already advanced to a new language).
static CACHE_LANG: Lazy<Mutex<String>> = Lazy::new(|| Mutex::new(String::new()));

struct Tracked {
    handle: GCHandle, // keep-alive for the target component; target() re-fetches or null-if-collected
    src: String,
    tramp: usize,     // original set_text trampoline for this component's class
    mi: usize,        // MethodInfo* for that setter
    class_ptr: usize, // the component's IL2CPP Class* captured when it was live (validity check)
    getter: usize,    // the component's get_text Method* (0 = unresolved) — to verify current text
    set_maxvis: usize, // set_maxVisibleCharacters(i32) Method* (0 = not a TMP-derived component)
    ts: Instant,
    epoch: u64,
}

/// Untranslated components awaiting their translation, keyed by the raw `this` value (used ONLY as a
/// dedup/eviction key — never dereferenced; liveness comes from the GCHandle).
static TRACKED: Lazy<Mutex<HashMap<usize, Tracked>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Components whose (longer-than-source) translation we've applied and must keep FULLY revealed against
/// the story timeline's English-sized typewriter, which re-drives maxVisibleCharacters each frame and
/// would otherwise clip the translation's tail. Each frame the pump re-asserts REVEAL_ALL while the
/// component still shows our translation; the entry is released when the line advances (text changed),
/// the component is collected, or HOLD_SECS elapses. Keyed by raw `this` (dedup only; liveness = the
/// GCHandle). Never cleared off-thread (GCHandle::drop is IL2CPP) — the pump evicts it on the main
/// thread.
struct Hold {
    handle: GCHandle,
    class_ptr: usize,
    getter: usize,
    set_maxvis: usize,
    dst: String,
    until: Instant,
}
static REVEAL_HOLD: Lazy<Mutex<HashMap<usize, Hold>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Worker → main-thread handoff: finished (src, dst, epoch) awaiting re-apply on the pump.
static RESULTS: Lazy<Mutex<Vec<(String, String, u64)>>> = Lazy::new(|| Mutex::new(Vec::new()));

static IN_REAPPLY: AtomicBool = AtomicBool::new(false);
static WORKER_STARTED: AtomicBool = AtomicBool::new(false);
/// Worker power state. False = the NMT worker parks on its condvar and every enqueue is refused, so
/// "translation disabled" (or "Overseer disabled") really does stop the background thread instead of
/// leaving it spinning through a queue nobody will read. Written by `runtime::fan_out`.
static WORKER_ACTIVE: AtomicBool = AtomicBool::new(true);
/// Process-clock ms of the last real translation activity — drives the idle model unload.
static LAST_ACTIVITY_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Suspend or resume the translation worker. Idempotent; safe from any thread.
pub fn set_worker_active(on: bool) {
    if WORKER_ACTIVE.swap(on, Ordering::Relaxed) == on {
        return;
    }
    if !on {
        // Drop everything the worker was going to do and release the queue memory. TRACKED is left
        // alone deliberately: dropping a GCHandle is an IL2CPP call and this runs off the main
        // thread — the main-thread pump's eviction sweep frees them on the next frame.
        if let Ok(mut inner) = REQ.lock() {
            inner.queue.clear();
            inner.pqueue.clear();
            inner.pending.clear();
            inner.queue.shrink_to_fit();
            inner.pqueue.shrink_to_fit();
            inner.pending.shrink_to_fit();
        }
        if let Ok(mut r) = RESULTS.lock() {
            r.clear();
            r.shrink_to_fit();
        }
        flush(); // don't lose what has already been learned
    }
    CV.notify_all();
}

pub fn worker_active() -> bool {
    WORKER_ACTIVE.load(Ordering::Relaxed)
}

#[inline]
fn mark_activity() {
    LAST_ACTIVITY_MS.store(crate::tools::now_ms(), Ordering::Relaxed);
}

/// Milliseconds since the last translation the pipeline actually did (u64::MAX = never).
pub fn idle_ms() -> u64 {
    let t = LAST_ACTIVITY_MS.load(Ordering::Relaxed);
    if t == 0 {
        u64::MAX
    } else {
        crate::tools::now_ms().saturating_sub(t)
    }
}
/// Throttle for the re-apply "text moved on" diagnostic (ms process clock).
static GUARD_MISS_LOG_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// ── memory budget ───────────────────────────────────────────────────────────────────────────────
// Every one of these caps is a MEMORY decision, so they are sized against what they actually hold
// rather than against a round number:
//
//  * QUEUE_CAP    — owned `String`s awaiting translation. 600 short lines is a few hundred KB.
//  * TRACK_CAP    — each entry pins a **GCHandle**, which keeps a managed text component (and
//                   everything it references: its mesh, its material, its GameObject subtree) alive
//                   in the IL2CPP heap. 900 pinned components was by far the most expensive number
//                   here — it is managed memory the game cannot collect, and it is invisible in any
//                   native profiler. 250 still covers every component on the busiest screen.
//  * ATTEMPTED    — a negative cache of failures; it only ever grows and is pure overhead past a
//                   few thousand entries because failures repeat quickly.
//  * CACHE_CAP    — the translation cache. Entries are two heap strings each, so 60k averaged tens
//                   of megabytes resident on top of the same data on disk; 25k covers a very
//                   heavily-played language and the rest reloads from `mtl.json` on demand.
const QUEUE_CAP: usize = 600;
const TRACK_CAP: usize = 250;
const ATTEMPTED_CAP: usize = 6_000;
const CACHE_CAP: usize = 25_000;
// Small batches ON PURPOSE: a priority-lane (on-screen) line can only jump ahead of the NEXT batch,
// so batch duration IS the first-view latency floor. At 32 a skip-flooded queue made live story
// lines wait 30-60s — translated long after the line advanced (guard discards it → "story never
// translates live"). Priority batches are smaller still: latency beats throughput for visible text.
const BATCH: usize = 8; // bulk lane (pre-warm/background)
const PRIORITY_BATCH: usize = 4; // on-screen lane — ~a few seconds end-to-end
const REAPPLY_PER_FRAME: usize = 40;
// A translation older than this is likely for a line the player already advanced past — drop it
// rather than risk re-applying stale text onto a moved-on component (the current-text guard in the
// pump is the primary defense; this bounds how stale a candidate can even be).
const REAPPLY_MAX_AGE_SECS: u64 = 15;
const FLUSH_EVERY: usize = 24;
// Longest source we send to NMT. Raised from 400 → 700 so full scenario descriptions and multi-clause
// event dialogue (which can run past 400 chars) still translate rather than being silently dropped.
// Still well under NLLB's token ceiling, and genuine UI text never approaches it.
const MAX_SRC_LEN: usize = 700;
// TMP's "show every glyph" sentinel (its own default for maxVisibleCharacters). After re-applying a
// translation that's LONGER than the source, we force maxVisibleCharacters to this so the story
// timeline's English-sized typewriter reveal can't clip the translation's tail. It's purely a DISPLAY
// property — it never drives the timeline clock, so it can't desync story/voice timing; it only ever
// reveals MORE text, which the advance gate reads as "reveal complete" (so it can never block).
const REVEAL_ALL: i32 = 99_999;
const HOLD_CAP: usize = 48; // max concurrently held (re-asserted) components — each pins a GCHandle
const HOLD_SECS: u64 = 5; // how long to keep re-asserting a line's full reveal before releasing it

// ─── eligibility (ports of translation_manager.py _is_junk / _has_english) ──────────────────────

/// Pure numbers / placeholders / too-short tokens aren't translation targets. Latin text needs a
/// 3+ letter run; CJK (JP client) menu labels are frequently 1-2 ideographs, so any CJK char with
/// total length >= 2 qualifies.
fn is_junk(s: &str) -> bool {
    let t = s.trim();
    if t.chars().count() < 2 {
        return true;
    }
    if has_cjk(t) {
        return false; // a real CJK label
    }
    let mut run = 0usize;
    for c in t.chars() {
        if c.is_alphabetic() {
            run += 1;
            if run >= 3 {
                return false; // has a real word → not junk
            }
        } else {
            run = 0;
        }
    }
    true
}

/// Digit-heavy strings ("+1200", "3/10", "12,345 pts") are stats, not prose — skip NMT.
fn digit_heavy(s: &str) -> bool {
    let (mut digits, mut letters) = (0usize, 0usize);
    for c in s.chars() {
        if c.is_ascii_digit() {
            digits += 1;
        } else if c.is_alphabetic() {
            letters += 1;
        }
    }
    digits > 0 && digits * 2 >= letters // at least a third of the alphanumerics are digits
}

/// Distinctly-English words — if any survives the glossary, the line is free-form English that needs
/// NMT. (Deliberately excludes words that are also common in target languages.) Only used on the
/// Global (English-source) client.
static EN_WORDS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "the", "you", "your", "yours", "went", "will", "would", "with", "without", "this", "that",
        "these", "those", "have", "has", "had", "having", "don't", "isn't", "won't", "can't",
        "didn't", "doesn't", "i'll", "you'll", "we'll", "it's", "that's", "need", "needs", "want",
        "wants", "wanted", "going", "gonna", "wanna", "stay", "yourself", "myself", "himself",
        "herself", "lose", "understand", "behind", "and", "but", "not", "from", "about", "when",
        "what", "which", "while", "should", "could", "because", "there", "their", "they", "them",
        "his", "her", "she", "we", "our", "been", "being", "more", "very", "just", "know", "think",
        "like", "make", "made", "take", "come", "back", "only", "over", "after", "before", "again",
        "still", "here", "now", "then", "too", "really", "something", "someone", "everyone",
        "nothing", "please", "thank", "thanks", "sorry", "hello", "okay", "yeah", "let's",
        "everything", "maybe", "already", "always", "never", "everybody", "anybody", "whatever",
        "whenever", "ourselves", "is", "are", "was", "were", "am", "it", "its", "isn", "aren",
        "wasn", "won", "can", "cannot", "get", "got", "see", "say", "said", "tell", "told", "good",
        "great", "win", "race", "training", "today", "tomorrow", "i'm", "we're", "they're",
        "you're", "there's", "here's", "let", "us",
    ]
    .into_iter()
    .collect()
});

fn has_english(s: &str) -> bool {
    let mut word = String::new();
    for c in s.chars() {
        if c.is_ascii_alphabetic() || c == '\'' {
            word.push(c.to_ascii_lowercase());
        } else if !word.is_empty() {
            if EN_WORDS.contains(word.as_str()) {
                return true;
            }
            word.clear();
        }
    }
    !word.is_empty() && EN_WORDS.contains(word.as_str())
}

/// Count DISTINCT English function words in `s`. Used with a >=2 threshold by the degenerate-output
/// checks: several EN_WORDS entries are also real words in target languages ("but"/"race"/"us" in
/// French), so a SINGLE hit must never reject a correct translation — only the multi-word English
/// leak (the actual mixed-language garble) should.
/// The set of `{n}` placeholder indices a format string uses, e.g. "{0}: {1:D2}" → {"0","1"}.
fn placeholder_indices(s: &str) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'{' {
            let mut j = i + 1;
            let mut idx = String::new();
            while j < b.len() && b[j].is_ascii_digit() {
                idx.push(b[j] as char);
                j += 1;
            }
            // A placeholder is {N} or {N:fmt} or {N,pad}. Anything else (incl. the literal "{}") isn't.
            if !idx.is_empty() && j < b.len() && (b[j] == b'}' || b[j] == b':' || b[j] == b',') {
                out.insert(idx);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// Word count, for the short-source hallucination guard.
fn word_count(s: &str) -> usize {
    s.split_whitespace().filter(|w| w.chars().any(|c| c.is_alphanumeric())).count()
}

/// Is `dst` a SAFE translation of `src` — i.e. can we hand it to the game without breaking it?
///
/// This is a CORRECTNESS gate, not a quality one. A translation that reads badly is a cosmetic wart;
/// one that violates these rules makes the game THROW. Both rules come from real, measured failures:
///
/// 1. **Placeholder integrity.** The game passes many of these strings to `String.Format`. NLLB
///    happily drops or invents `{n}` slots — the cache contained
///    `"{0} <color=#FF6D26>+{1}</color>"` → `"+{1} {0} {}"`, and that bare `{}` throws
///    FormatException every single time. An index the args don't supply throws too. So the
///    translation must use EXACTLY the same index set as the source, and introduce no stray braces.
///
/// 2. **Short-source hallucination.** This is the Scout soft-lock. `Localize.Get(954)` = `"Jan"` came
///    back as `"Je suis d'accord avec Jan"` ("I agree with Jan") — the model took the month for a
///    person's name and invented a sentence. The game parsed that back as a date and threw
///    `FormatException: Format_UnknownDateTimeWord`, killing the gacha view silently. The existing
///    degenerate filter could never catch it: it only looks for mixed-language garble and EXEMPTS
///    short sources outright. Short strings fail the other way — by growing. A genuine translation of
///    a 1-3 word label stays about that length ("OK" → "D'accord", "Team Rank" → "Rang de l'équipe");
///    a 1-word source becoming 5 words is invention, not translation.
///
/// Month/day tokens are ALSO protected upstream by `is_protected_name`, which is the real fix for
/// dates; this is the general net that catches the same class everywhere else.
pub fn translation_safe(src: &str, dst: &str) -> bool {
    if placeholder_indices(src) != placeholder_indices(dst) {
        return false;
    }
    // A brace that isn't part of a well-formed placeholder (the literal "{}" case) is a throw waiting
    // to happen — but only judge the OUTPUT for braces the INPUT didn't have.
    if dst.matches('{').count() != src.matches('{').count()
        || dst.matches('}').count() != src.matches('}').count()
    {
        return false;
    }
    let (sw, dw) = (word_count(src), word_count(dst));
    if sw <= 3 && dw > sw + 2 {
        return false; // hallucinated expansion (the "Jan" case)
    }
    // Degenerate word repetition — NMT sometimes stutters a short input: "Focus" => "Focus Focus",
    // "Mar" => "Mar Mar Mar". Reject when the source is short and the output is just one token echoed.
    if sw <= 2 {
        let toks: Vec<&str> = dst.split_whitespace().collect();
        if toks.len() >= 2 && toks.iter().all(|t| t.eq_ignore_ascii_case(toks[0])) {
            return false;
        }
    }
    // ── Hallucination patterns on SHORT sources (labels / item names / menu entries) ──
    // A label or item name is a NOUN PHRASE. NMT, fed a short one, sometimes DEFINES it instead of
    // translating it — turning it into a full "X is a Y" clause. Real, from the inventory page:
    //   "Held"                   => "Il est tenu"                            (Held-count label, on EVERY item)
    //   "Pleasing Parfait"       => "Il est parfaitement agréable"          (item name)
    //   "Outer Post Raffle Ball" => "Le Raffle Ball est un ballon de raffle." (item name → definition)
    //   "Options"                => "Options d'options"                     (source echoed + padded)
    //   "Other"                  => "Other autre"                           (source kept, French appended)
    // Prose (long source) is exempt: its clauses and repetition are legitimate. The prior guards were
    // off by one word ("Held" 1→3 needs `>3`; "Pleasing Parfait" 2→4 needs `>4`), so calibrate by
    // PATTERN, not length.
    if sw <= 5 {
        let dl = dst.to_lowercase();
        let sl = src.to_lowercase();
        // (a) Definitional clause in the OUTPUT that the SOURCE didn't have. A source that is itself a
        //     clause ("It's a bonus" → "C'est un bonus") is fine, so bail out if the source already
        //     carries a copula.
        const DEFN: [&str; 9] = [
            "il est ", "il s'agit", "c'est ", "ce sont ", "elle est ",
            " est un ", " est une ", " sont des ", "this is ",
        ];
        const SRC_COPULA: [&str; 9] =
            [" is ", " are ", " am ", "it's ", "that's ", "'s a ", " est ", " sont ", " es "];
        let dst_defn = DEFN.iter().any(|p| dl.starts_with(p) || dl.contains(p));
        let src_has_copula = SRC_COPULA.iter().any(|p| sl.contains(p))
            || sl.starts_with("is ")
            || sl.starts_with("are ");
        if dst_defn && !src_has_copula {
            return false;
        }
        // (b) Source echoed then padded: the model kept the source verbatim at the front and appended
        //     rather than translating. "Options" => "Options d'options", "Other" => "Other autre". The
        //     trailing space avoids the prefix trap ("Convert" => "Convertir" is NOT "convert " + more).
        if dw > sw && dl.starts_with(&format!("{sl} ")) {
            return false;
        }
    }
    true
}

fn english_word_hits(s: &str) -> usize {
    let mut hits: HashSet<&'static str> = HashSet::new();
    let mut word = String::new();
    for c in s.chars() {
        if c.is_ascii_alphabetic() || c == '\'' {
            word.push(c.to_ascii_lowercase());
        } else if !word.is_empty() {
            if let Some(k) = EN_WORDS.get(word.as_str()) {
                hits.insert(k);
            }
            word.clear();
        }
    }
    if !word.is_empty() {
        if let Some(k) = EN_WORDS.get(word.as_str()) {
            hits.insert(k);
        }
    }
    hits.len()
}

/// Choice-button-sized fragments ("The beach.", "A new town."). Exempt from the English-leak output
/// check — one function word in a 3-word string is statistically meaningless (the filter exists for
/// long mixed-language garble), and NLLB needs the copy-retry below for these, not a blacklist.
/// CJK has no spaces (whole sentences = one "word") and packs a sentence into few chars, so it gets
/// its own, much stricter char bound — otherwise the JP client would bypass the quality gate entirely.
fn short_source(s: &str) -> bool {
    if has_cjk(s) {
        s.chars().count() < 8
    } else {
        s.chars().count() < 25 || s.split_whitespace().count() <= 3
    }
}

/// Re-wrap a translation to the SOURCE's box shape. When the game handed the setter PRE-WRAPPED text
/// (embedded \n — the LineHeadWrap form sized to the dialogue box), applying an UNWRAPPED (and often
/// longer) translation overflows the box horizontally — the reported "text spills out of the box".
/// Greedy space-wrap at the source's widest visible line; single-line sources pass through (box
/// width unknown — never guess). Public: used by the re-apply pump AND the synchronous cache tier.
pub fn fit_to_source(dst: &str, src: &str) -> String {
    if !src.contains('\n') || dst.contains('\n') {
        return dst.to_string();
    }
    let width = src.lines().map(visible_char_count).max().unwrap_or(0);
    if width < 8 {
        return dst.to_string();
    }
    let mut out = String::with_capacity(dst.len() + 8);
    let mut line = 0usize;
    for word in dst.split(' ') {
        let w = word.chars().count();
        if line == 0 {
            out.push_str(word);
            line = w;
        } else if line + 1 + w <= width {
            out.push(' ');
            out.push_str(word);
            line += 1 + w;
        } else {
            out.push('\n');
            out.push_str(word);
            line = w;
        }
    }
    out
}

/// Cache lookup + box-fit in one step (the synchronous setter tier): a cached translation keyed by a
/// WRAPPED source must be re-wrapped to that source's box or it spills.
pub fn lookup_fitted(src: &str) -> Option<String> {
    lookup(src).map(|t| fit_to_source(&t, src))
}

/// Should a GLOSSARY translation be accepted as FINAL? The glossary does word-boundary substring
/// replacement, which is perfect for labels/terms but on free-form PROSE a generic entry ("was" →
/// "était") produces franglais ("I était checking my interview…") — and a tier-1 hit used to return
/// immediately, so the line NEVER reached NMT and stayed mixed forever. (English target: final.)
///
/// Two tests, both needed:
/// * `english_word_hits(dst) >= 2` — distinctly-English FUNCTION words survive → prose, not final.
/// * **Surviving content words** — the function-word test alone waved through
///   `"Slightly increase velocity on a straight. (Long)"` → `"… (Longue)"`: one word swapped, the
///   entire English sentence intact, zero function-word hits ("slightly"/"increase"/"velocity" are
///   content words the EN_WORDS set doesn't list). If any substantial source word (≥4 alpha chars)
///   survives verbatim into the result, the glossary only PARTIALLY covered the string → not final,
///   fall through with the ORIGINAL so NMT translates the whole line. A fully-covered label
///   ("Race" → "Course") has no survivors and stays an instant hit.
pub fn glossary_result_final(src: &str, dst: &str) -> bool {
    if crate::settings::tl_lang().as_deref() == Some("en") {
        return true;
    }
    if english_word_hits(dst) >= 2 {
        return false;
    }
    // Any source word of >=4 letters that appears verbatim in the output marks a partial — EXCEPT
    // protected names, which are SUPPOSED to survive ("Tokai Teio", "Air Groove", "Copano Rickey" in
    // a French line are correct, not a leak). Mask names out of both sides first (same NAME_RE the
    // reload purge uses), so only genuine untranslated words ("velocity", "straight") count.
    let (src_m, dst_m) = match NAME_RE.load().as_ref() {
        Some(re) => (re.replace_all(src, " ").into_owned(), re.replace_all(dst, " ").into_owned()),
        None => (src.to_string(), dst.to_string()),
    };
    let dst_words: HashSet<&str> = dst_m
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|w| w.len() >= 4)
        .collect();
    !src_m
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|w| w.len() >= 4)
        .any(|w| dst_words.contains(w))
}

/// Uppercase the first character (re-case a lowercased retry translation to the source's shape).
fn upper_first(s: &str) -> String {
    let mut cs = s.chars();
    match cs.next() {
        Some(c) => c.to_uppercase().collect::<String>() + cs.as_str(),
        None => String::new(),
    }
}

/// Japanese source detection (Hiragana / Katakana / CJK ideographs) — the JP-client analog of
/// `has_english`.
fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c,
            '\u{3040}'..='\u{30ff}' |   // Hiragana + Katakana
            '\u{3400}'..='\u{4dbf}' |   // CJK Ext A
            '\u{4e00}'..='\u{9fff}' |   // CJK Unified
            '\u{ff66}'..='\u{ff9d}')    // Halfwidth Katakana
    })
}

/// A run of >=2 ASCII letters = a real word (vs. pure numbers/symbols). We translate ANY English
/// label with a real word (menu buttons, Missions, Presents, Clubs, skill descriptions…), not just
/// free-form sentences: the glossary runs first for known terms, the emitted-guard + cache absorb
/// repeats, and protected names are excluded — so broad coverage is safe. (`has_english` remains for
/// reference; broad coverage is what the user wants for menus.)
fn has_latin_word(s: &str) -> bool {
    let mut run = 0u32;
    for c in s.chars() {
        if c.is_ascii_alphabetic() {
            run += 1;
            if run >= 2 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// Source text worth sending to NMT (source-direction aware): Japanese chars on the JP client, any
/// real Latin word on the Global (English) client.
fn needs_mtl(src: &str) -> bool {
    if crate::loc_ui::is_jp_client() {
        has_cjk(src)
    } else {
        has_latin_word(src)
    }
}

/// Full eligibility gate shared by `request` and `on_miss`: MTL on, engine ready, a target language,
/// real translatable text, and NOT a protected proper name (skill/character/race names stay English).
fn eligible(src: &str) -> bool {
    crate::settings::mtl_enabled()
        && crate::nllb::ready()
        && crate::settings::tl_lang().is_some()
        && !is_junk(src)
        && !is_protected_name(src)
        && needs_mtl(src)
}

/// Proper nouns that must stay verbatim (skill names, character names, race names) — Overseer's
/// `names.json`. Language-independent, so loaded once from `<dll_dir>/glossary/names.json`.
static NAMES: Lazy<ArcSwap<HashSet<String>>> = Lazy::new(|| ArcSwap::from_pointee(HashSet::new()));
/// The same names compiled into ONE word-bounded alternation regex (longest-first) so occurrences
/// EMBEDDED in a sentence (e.g. a skill description that mentions another skill) can be masked before
/// NMT and restored after — keeping those names English too. Built from NAMES ∪ USER_NAMES.
static NAME_RE: Lazy<ArcSwap<Option<fancy_regex::Regex>>> = Lazy::new(|| ArcSwap::from_pointee(None));
/// USER-supplied protected names — the player's own trainer name and anything else they want kept in
/// the original language. Kept SEPARATE from the bundled names.json set so the user list can be edited
/// live from the web UI without reloading the whole names.json. Both sets feed is_protected_name and
/// the embedded-masking NAME_RE.
static USER_NAMES: Lazy<ArcSwap<HashSet<String>>> = Lazy::new(|| ArcSwap::from_pointee(HashSet::new()));

/// Escape regex metacharacters in a literal name.
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

/// True if the whole (trimmed) string is a protected proper name — keep it English, never translate.
/// Checks BOTH the bundled names.json set AND the user's own protected list (their trainer name etc.),
/// plus the machine-readable date tokens below.
pub fn is_protected_name(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() {
        return false;
    }
    is_date_token(t)
        || NAMES.load().contains(t)
        || USER_NAMES.load().contains(t)
        || AUTO_NAMES.load().contains(t)
}

/// Month / day / meridiem tokens — NEVER translate these. They are not prose: the game feeds them
/// straight to `DateTime` parsing, so a translated one is a hard crash, not a cosmetic wart.
///
/// This is the proven root cause of the Scout (gacha) soft-lock, caught 2026-07-15 with the exception
/// tracer. The chain, from one log excerpt:
///
/// ```text
/// click -> FooterButtonBase(Clone)                        (user taps Scout)
/// Localize.Get(954) = "Jan"                               (game wants the month abbreviation)
///   -> CACHE(954) "Jan" => "Je suis d'accord avec Jan"    (NMT read "Jan" as a PERSON and
///                                                          hallucinated "I agree with Jan")
/// !! FormatException: Format_UnknownDateTimeWord          (DateTime.Parse hits "Je" and throws)
/// ```
///
/// The throw happened inside the gacha view's construction, so the view never built and the tab
/// silently did nothing — engine still at 115 FPS, no crash, nothing in any log (the game suppresses
/// Unity's logger). "Jan"/"May"/"Mar"/"Sun" are exactly the strings an NMT model is most likely to
/// mistake for names, which is why this bites here and not on prose.
fn is_date_token(t: &str) -> bool {
    const MONTHS: [&str; 24] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        "January", "February", "March", "April", "June", "July", "August", "September", "October",
        "November", "December", "Sept",
    ];
    const DAYS: [&str; 14] = [
        "Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun", "Monday", "Tuesday", "Wednesday",
        "Thursday", "Friday", "Saturday", "Sunday",
    ];
    const MERIDIEM: [&str; 4] = ["AM", "PM", "am", "pm"];
    MONTHS.iter().any(|m| m.eq_ignore_ascii_case(t))
        || DAYS.iter().any(|d| d.eq_ignore_ascii_case(t))
        || MERIDIEM.contains(&t)
}

/// Names LEARNED automatically from the UI: whenever the game writes a string into a component whose
/// GameObject marks it as a name field (TrainerNameText, CircleNameText, … — see
/// `loc_settext::user_content_component`), that string IS a real trainer/club name, by construction.
/// Feeding it back here protects its EMBEDDED occurrences too — prose like "Sorry to disturb you,
/// Trainer Mama Yuurai…" masks the name before NMT and restores it after. This replaces the manual
/// "keep names untranslated" textarea: the list builds itself from what the game actually displays
/// (your name, friends, opponents), which a hand-typed list could never keep up with.
static AUTO_NAMES: Lazy<ArcSwap<HashSet<String>>> = Lazy::new(|| ArcSwap::from_pointee(HashSet::new()));

/// Learn a name seen in a name-kind UI field. Cheap when already known (one set lookup); rebuilds the
/// masking regex only on a genuinely new name (rare after the first screens). In-memory only — the
/// set repopulates naturally as screens display names, so persisting it would only risk staleness.
pub fn note_user_name(s: &str) {
    let t = s.trim();
    // Names are short. A length cap keeps a mis-tagged prose field from ever bloating the mask regex.
    if t.is_empty() || t.len() > 40 || AUTO_NAMES.load().contains(t) {
        return;
    }
    let mut set: HashSet<String> = AUTO_NAMES.load().as_ref().clone();
    set.insert(t.to_string());
    AUTO_NAMES.store(Arc::new(set));
    rebuild_name_re();
    log(&format!("[mtl] learned user name from UI field: {t:?} (kept in original language)"));
}

/// Rebuild the embedded-name masking regex from NAMES ∪ USER_NAMES ∪ AUTO_NAMES (longest-first
/// alternation, word-bounded on ASCII letters). Called after any of the sets changes.
fn rebuild_name_re() {
    let base = NAMES.load();
    let user = USER_NAMES.load();
    let auto = AUTO_NAMES.load();
    let mut names: Vec<&String> = base.iter().chain(user.iter()).chain(auto.iter()).collect();
    names.sort_by(|a, b| b.chars().count().cmp(&a.chars().count()));
    names.dedup();
    let alts: Vec<String> = names.iter().map(|s| regex_escape(s)).collect();
    let re = if alts.is_empty() {
        None
    } else {
        fancy_regex::Regex::new(&format!(r"(?<![A-Za-z])(?:{})(?![A-Za-z])", alts.join("|"))).ok()
    };
    NAME_RE.store(Arc::new(re));
}

/// The user's current protected names (for the web UI to show/edit).
pub fn user_names() -> Vec<String> {
    let mut v: Vec<String> = USER_NAMES.load().iter().cloned().collect();
    v.sort();
    v
}

/// Replace the user's protected-name list (their trainer name etc.). Takes effect immediately — a
/// protected name is checked BEFORE glossary/cache/NMT, so it stops translating on the next set_text
/// even if it was previously cached. Persisted by the caller (settings).
pub fn set_user_names(list: Vec<String>) {
    let set: HashSet<String> = list
        .into_iter()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect();
    let n = set.len();
    USER_NAMES.store(Arc::new(set));
    rebuild_name_re();
    log(&format!("[mtl] {n} user-protected names set (kept in original language)"));
}

/// Load the protected-names list. Language-independent; call once at boot. Missing file = no
/// protection (non-fatal).
pub fn load_names() {
    let path = crate::paths::dll_dir().join("glossary").join("names.json");
    let set: HashSet<String> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .map(|v| {
            v.into_iter()
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let n = set.len();
    NAMES.store(Arc::new(set));
    rebuild_name_re(); // includes any already-set USER_NAMES
    if n > 0 {
        log(&format!("[mtl] loaded {n} protected names (kept English)"));
    }
}

// ─── hot-path API (main thread) ─────────────────────────────────────────────────────────────────

/// True if a target language is selected — the setter hook should run the translation pipeline even
/// when the (English) glossary is inactive (e.g. on the JP client, where only NMT applies).
///
/// PERF: this is the FIRST thing every hooked `set_text` asks, so it must not allocate. The old
/// form called `settings::tl_lang()`, which clones the `Option<String>` out of an ArcSwap — a heap
/// allocation per on-screen string, per frame. `tl_lang_set()` answers the same question with a
/// pointer read. It also now consults the runtime gate, so "Translation disabled" (or "Overseer
/// disabled", unless translation was explicitly kept alive) really does stop the pipeline instead
/// of only hiding the language.
#[inline]
pub fn translation_active() -> bool {
    crate::runtime::active(crate::runtime::Subsystem::Translation)
        && crate::settings::tl_lang_set()
}

/// Cached translation for `src`, if any. One map read; served synchronously by the setter hook.
pub fn lookup(src: &str) -> Option<String> {
    CACHE.lock().ok().and_then(|c| c.get(src).cloned())
}

/// How many translations are currently learned (in the active language's cache). Shown in the UI so
/// the accumulated, auto-saved learning is visible.
pub fn cache_count() -> usize {
    CACHE.lock().map(|c| c.len()).unwrap_or(0)
}

/// Pending translation requests (priority + bulk), for the memory report.
pub fn queue_depth() -> usize {
    REQ.lock().map(|i| i.pqueue.len() + i.queue.len()).unwrap_or(0)
}

/// Components currently tracked for a late re-apply (each pins a GCHandle).
pub fn tracked_len() -> usize {
    TRACKED.lock().map(|t| t.len()).unwrap_or(0)
}

/// Drop every reclaimable translation cache. The on-disk `mtl.json` is flushed first, so nothing
/// learned is lost — the in-memory copy simply reloads on the next language activation.
///
/// TRACKED / REVEAL_HOLD are deliberately NOT touched here: dropping a `GCHandle` is an IL2CPP call
/// and this can run on the web thread. The main-thread pump's eviction sweep releases them.
pub fn trim() {
    flush();
    if let Ok(mut a) = ATTEMPTED.lock() {
        a.clear();
        a.shrink_to_fit();
    }
    if let Ok(mut q) = RECENT.lock() {
        q.clear();
        q.shrink_to_fit();
    }
    if let Ok(mut r) = RESULTS.lock() {
        r.shrink_to_fit();
    }
    if let Ok(mut c) = CACHE.lock() {
        c.shrink_to_fit();
    }
    log("[mtl] caches trimmed on request");
}

/// Force-persist the cache now (e.g. from the web UI "Save" button). The worker already flushes
/// every ~20s + on language switch; this lets the user snapshot immediately.
pub fn save_now() {
    flush();
}

/// Enqueue a glossary-miss for the background worker (idempotent, non-blocking). Bulk lane —
/// pre-warm lists and non-tracked components join the BACK of the queue.
pub fn request(src: &str) {
    enqueue(src, false);
}

/// Queue-full / staleness evictions since the last queue-status log. A hyper-skip career floods
/// hundreds of story lines through the setters faster than NLLB drains them; silently dropping the
/// overflow was how ON-SCREEN event choices vanished without a trace — so drops are counted and the
/// worker reports them.
static QUEUE_DROPPED: AtomicUsize = AtomicUsize::new(0);

fn enqueue(src: &str, on_screen: bool) {
    // Suspended pipeline → refuse the work outright. Without this the queue kept filling while the
    // worker was parked, so re-enabling translation dumped a minutes-deep backlog of text that had
    // long since left the screen.
    if !WORKER_ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    if !eligible(src) || src.len() > MAX_SRC_LEN || digit_heavy(src) {
        return;
    }
    if CACHE.lock().map(|c| c.contains_key(src)).unwrap_or(false) {
        return;
    }
    if ATTEMPTED.lock().map(|a| a.contains(src)).unwrap_or(false) {
        return;
    }
    let mut inner = match REQ.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if inner.pending.contains(src) {
        // Already queued. If it's now ON SCREEN, promote it to the FRONT OF THE PRIORITY LANE — an
        // earlier sighting (skip-flood, or a bulk pre-warm) may have it buried, and the player is
        // looking at it NOW. Not found in either queue = already in the worker's current batch.
        if on_screen {
            if let Some(pos) = inner.pqueue.iter().position(|q| q == src) {
                if pos > 0 {
                    if let Some(item) = inner.pqueue.remove(pos) {
                        inner.pqueue.push_front(item);
                    }
                }
            } else if let Some(pos) = inner.queue.iter().position(|q| q == src) {
                if let Some(item) = inner.queue.remove(pos) {
                    inner.pqueue.push_front(item);
                }
            }
        }
        return;
    }
    if on_screen {
        // PRIORITY LANE (newest-first): text on a live component. The worker drains this lane first
        // in SMALL batches, so what the player is looking at translates within seconds even when a
        // skip-flood built a minutes-deep backlog. Older on-screen lines drift backward — if they've
        // scrolled past, the pump's current-text guard discards their result anyway, and they
        // re-enqueue on their next sighting. At cap, the STALEST priority entry is evicted.
        if inner.pqueue.len() + inner.queue.len() >= QUEUE_CAP {
            if let Some(evicted) = inner.pqueue.pop_back().or_else(|| inner.queue.pop_back()) {
                inner.pending.remove(&evicted);
                QUEUE_DROPPED.fetch_add(1, Ordering::Relaxed);
            }
        }
        inner.pending.insert(src.to_string());
        inner.pqueue.push_front(src.to_string());
    } else {
        if inner.pqueue.len() + inner.queue.len() >= QUEUE_CAP {
            QUEUE_DROPPED.fetch_add(1, Ordering::Relaxed);
            return;
        }
        inner.pending.insert(src.to_string());
        inner.queue.push_back(src.to_string());
    }
    drop(inner);
    CV.notify_one();
}

/// Diagnostic: log the runtime class of each distinct component that reaches the async-NMT path once
/// (throttled to 40 classes). If some on-screen element still won't translate, this reveals whether
/// it flows through a component type we don't hook (so we can add it) vs. one we already cover.
static LOGGED_CLASSES: Lazy<Mutex<HashSet<String>>> = Lazy::new(|| Mutex::new(HashSet::new()));
fn log_long_class(this: Object, src: &str) {
    if this.is_null() {
        return;
    }
    let name = il2cpp::object_class_name(this);
    if name.is_empty() {
        return;
    }
    if let Ok(mut s) = LOGGED_CLASSES.lock() {
        if s.len() < 40 && s.insert(name.clone()) {
            // Also report whether this class exposes set_maxVisibleCharacters — i.e. whether it's a
            // TMP-derived component whose translated (longer) text we can keep fully revealed. If a
            // component that truncates translations logs "maxVisibleCharacters: no", its reveal lives
            // on an internal TMP field we'd need to reach instead. The first-seen text snippet ties
            // the class to the on-screen element it renders (which UI a stubborn class belongs to).
            let cls = il2cpp::object_class(this);
            let has_maxvis = !il2cpp::method(cls, "set_maxVisibleCharacters", 1).is_null();
            let snip: String = src.chars().take(48).collect();
            log(&format!(
                "[mtl] async-translate component class: {name} (maxVisibleCharacters setter: {}) first text: {snip:?}",
                if has_maxvis { "yes" } else { "no" }
            ));
        }
    }
}

/// Called from the setter hook's glossary-miss arm. Enqueues the string for background translation
/// AND registers the on-screen component so the main-thread pump can re-apply the arrived translation
/// a beat later (once NLLB returns) — the game shows the source in the meantime.
///
/// Async on-screen re-apply is ENABLED for every hooked component, including the story/event/choice
/// text component (Gallop.TextCommon). This is what gives first-view translation of free-form text
/// (scenario descriptions, in-event dialogue, event CHOICE buttons) that isn't in the pre-translated
/// cache. Re-setting text out of band was once suspected of soft-locking the story timeline, but the
/// real culprit was loc_story's field-pokes (now disabled) — the lock persisted even with TextCommon
/// unhooked. The pump's current-text guard (only re-apply while the component STILL shows the exact
/// source) + the class-validity guard prevent any desync of a line that has since advanced.
pub fn on_miss(this: Object, src: &str, tramp: usize, mi: usize) {
    // Nothing translatable → don't even queue it. This guard used to sit BELOW the enqueue, so every
    // countdown tick and percentage went into the PRIORITY lane: a trace measured "60.0%" enqueued
    // 846 times and "00:00"/"0:00:00" ~500 times in seconds, all ahead of real text. The fast lane
    // was permanently full of clock digits, which is why prose translated late or not at all.
    let junk = !eligible(src) || src.len() > MAX_SRC_LEN || digit_heavy(src);
    if !junk {
        enqueue(src, true); // on-screen → priority lane (front of the queue)
    }
    // Diagnostic: record which component class each async-miss line belongs to (once per class,
    // throttled) so any element that still won't translate can be traced to an un-hooked component.
    log_long_class(this, src);
    // Track for on-screen re-apply. The pump's current-text guard skips anything that changed
    // (recycled cells, advanced dialogue lines), so tracking dialogue components is safe.
    if this.is_null() || tramp == 0 || junk {
        return;
    }
    let epoch = REQ.lock().map(|i| i.epoch).unwrap_or(0);
    let handle = GCHandle::new(this, false);
    let cls = il2cpp::object_class(this);
    let getter = il2cpp::method(cls, "get_text", 0) as usize;
    // TMP-derived components expose set_maxVisibleCharacters(i32); resolve it once (searches base
    // classes too) so the pump can keep a longer translation fully revealed. 0 for UI.Text / any
    // non-TMP component — the reveal-hold simply doesn't apply to those (they don't truncate).
    let set_maxvis = il2cpp::method(cls, "set_maxVisibleCharacters", 1) as usize;
    let entry = Tracked {
        handle,
        src: src.to_string(),
        tramp,
        mi,
        class_ptr: cls as usize,
        getter,
        set_maxvis,
        ts: Instant::now(),
        epoch,
    };
    if let Ok(mut t) = TRACKED.lock() {
        if t.len() >= TRACK_CAP {
            if let Some(k) = t.iter().min_by_key(|(_, v)| v.ts).map(|(k, _)| *k) {
                t.remove(&k);
            }
        }
        t.insert(this as usize, entry);
    }
}

// ─── main-thread re-apply pump (called every frame from ui_tempo) ───────────────────────────────

/// A translation ready to swap onto a still-live component this frame (collected under TRACKED, then
/// applied OUTSIDE the lock — see pump_inner).
struct ReApply {
    handle: GCHandle,
    src: String,
    dst: String,
    tramp: usize,
    mi: usize,
    class_ptr: usize,
    getter: usize,
    set_maxvis: usize,
}

/// Read a text component's current string via its resolved get_text Method* (0 = unavailable).
/// MAIN THREAD ONLY. get_text is not one of our hooked setters, so this can't re-enter the pipeline.
unsafe fn read_text(comp: Object, getter: usize) -> Option<String> {
    if getter == 0 {
        return None;
    }
    let m = getter as Method;
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return None;
    }
    let g: unsafe extern "C" fn(Object, Method) -> Object = std::mem::transmute(p);
    let cur = g(comp, m);
    if cur.is_null() {
        return None;
    }
    Some(il2cpp::read_string(cur))
}

/// Formatting-insensitive comparison basis for the current-text guard: rich-text tags stripped and
/// all whitespace collapsed. Some components RE-FORMAT the exact string they were handed before it
/// lands in m_Text (the Flash-imported story labels insert line breaks/markup), which made the
/// exact-match guard read "moved on" for text that is still the very same line — the translation
/// was cached and tracked but never swapped in. A genuinely different (advanced) line still
/// normalizes differently, so the stale-re-apply protection is intact.
fn norm_for_guard(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    let mut last_space = true;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if in_tag => {}
            c if c.is_whitespace() => {
                if !last_space {
                    out.push(' ');
                    last_space = true;
                }
            }
            c => {
                out.push(c);
                last_space = false;
            }
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Count VISIBLE glyphs — chars OUTSIDE `<...>` rich-text spans — the basis TMP's maxVisibleCharacters
/// uses (parsed tags don't consume a visible slot). We must compare visible-vs-visible when deciding
/// if a translation is longer than the source it replaces: `dst` is already tag-free (protect() strips
/// tags and never restores them), but the raw `src` still carries its `<color=…>` / ruby / `<b>` tags,
/// so a raw `chars().count()` would OVER-count the source and let a genuinely-longer translation slip
/// past the reveal-hold and get its tail clipped by the source-sized typewriter. Mirrors protect()'s
/// naive `<`→next-`>` stripping so the two sides use the same basis.
fn visible_char_count(s: &str) -> usize {
    let mut n = 0usize;
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => n += 1,
            _ => {}
        }
    }
    n
}

/// Call an `(this, i32, MethodInfo*)` value-type property setter via its resolved Method* (0 = skip).
/// MAIN THREAD ONLY. Used for set_maxVisibleCharacters — not a hooked method, so no re-entry.
unsafe fn set_i32_prop(comp: Object, setter: usize, value: i32) {
    if setter == 0 {
        return;
    }
    let m = setter as Method;
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return;
    }
    let f: unsafe extern "C" fn(Object, i32, Method) = std::mem::transmute(p);
    f(comp, value, m);
}

/// Process-clock timestamp of the last pump run — lets the fallback driver defer to the primary.
static LAST_PUMP_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Re-apply arrived translations to their still-live components. Runs on the main thread, IL2CPP-
/// attached. `catch_unwind` because it executes inside the extern-"C" tween detour.
pub fn pump() {
    LAST_PUMP_MS.store(crate::tools::time::now_ms(), Ordering::Relaxed);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(pump_inner));
}

/// Is anything still tracked/held for re-apply? Lets the frame pump skip its whole body when
/// translation is off AND there is nothing left to release.
pub fn has_tracked() -> bool {
    TRACKED.lock().map(|t| !t.is_empty()).unwrap_or(false)
        || REVEAL_HOLD.lock().map(|h| !h.is_empty()).unwrap_or(false)
        || RESULTS.lock().map(|r| !r.is_empty()).unwrap_or(false)
}

/// FALLBACK pump driver — called from the ButtonCommon.Update hook. The primary driver sits in the
/// TweenManager.Update detour, but DOTween only calls that while at least one tween is ACTIVE: a
/// story/event-choice screen can idle with ZERO tweens, stalling re-apply exactly where first-view
/// translation matters most (the event-choice translations were cached seconds after display but
/// never swapped onto the buttons). Runs only when the primary hasn't run recently — the established
/// after-tween timing is preserved whenever tweens ARE active — and the 50ms gate also collapses
/// ButtonCommon.Update's once-per-button-per-frame fan-out to at most ~20 pumps/s.
pub fn pump_fallback() {
    let now = crate::tools::time::now_ms();
    if now.saturating_sub(LAST_PUMP_MS.load(Ordering::Relaxed)) > 50 {
        pump();
    }
}

fn pump_inner() {
    if IN_REAPPLY.swap(true, Ordering::AcqRel) {
        return; // re-apply calls set_text → re-enters our hook; don't recurse
    }
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            IN_REAPPLY.store(false, Ordering::Release);
        }
    }
    let _reset = Reset;

    let cur_epoch = REQ.lock().map(|i| i.epoch).unwrap_or(0);

    // PHASE 1 — collect the re-apply actions to run this frame, and ALWAYS run the eviction sweep
    // (even when no results arrived) so stale-epoch / timed-out GCHandles are freed and dead text
    // components stop being pinned. We must NOT call set_text while holding TRACKED: the game's
    // setter can synchronously set text on another component, re-entering our hook → on_miss →
    // TRACKED.lock() on THIS SAME THREAD → self-deadlock (the freeze). So collect now, apply after
    // the lock is released. (Mirrors mainthread::drain's take-then-run pattern.)
    let results: Vec<(String, String, u64)> = RESULTS
        .lock()
        .map(|mut r| std::mem::take(&mut *r))
        .unwrap_or_default();
    let mut actions: Vec<ReApply> = Vec::new();
    let mut leftover: Vec<(String, String, u64)> = Vec::new();
    // Tracked entries at their age limit: (key, component, getter, src) — the "is it still showing
    // the untranslated source?" read happens after TRACKED is released (managed call, see below).
    let mut expiry_checks: Vec<(usize, Object, usize, String)> = Vec::new();
    {
        let mut tracked = match TRACKED.lock() {
            Ok(t) => t,
            Err(_) => return,
        };
        for (src, dst, ep) in results.into_iter() {
            if ep != cur_epoch {
                continue; // stale-language result
            }
            if actions.len() >= REAPPLY_PER_FRAME {
                leftover.push((src, dst, ep));
                continue;
            }
            let keys: Vec<usize> = tracked
                .iter()
                .filter(|(_, t)| t.src == src && t.epoch == cur_epoch)
                .map(|(k, _)| *k)
                .collect();
            let mut ran_out = false;
            for k in keys {
                if actions.len() >= REAPPLY_PER_FRAME {
                    ran_out = true;
                    break;
                }
                if let Some(t) = tracked.remove(&k) {
                    actions.push(ReApply {
                        handle: t.handle,
                        src: src.clone(),
                        dst: dst.clone(),
                        tramp: t.tramp,
                        mi: t.mi,
                        class_ptr: t.class_ptr,
                        getter: t.getter,
                        set_maxvis: t.set_maxvis,
                    });
                }
            }
            if ran_out {
                leftover.push((src, dst, ep));
            }
        }
        // Time-out / stale-epoch eviction sweep (frees GCHandles) — runs every frame. An entry at
        // its age limit is NOT evicted while its component is alive and STILL showing the exact
        // untranslated source: during a queue backlog the translation can take minutes, and hard
        // eviction at 15s meant the result arrived to nobody (the on-screen line stayed English
        // forever). The current-text guard in PHASE 2 remains the correctness gate — refreshing the
        // timestamp only extends how long we're willing to wait, never what we're willing to apply.
        // The get_text read itself happens OUTSIDE this lock (below, with the other managed calls):
        // a managed call under TRACKED could self-deadlock if it ever re-entered a hooked setter.
        // Cost is bounded: each surviving entry is checked once per REAPPLY_MAX_AGE_SECS.
        let now = Instant::now();
        tracked.retain(|k, t| {
            if t.epoch != cur_epoch {
                return false;
            }
            if now.duration_since(t.ts).as_secs() < REAPPLY_MAX_AGE_SECS {
                return true;
            }
            let comp = t.handle.target();
            if comp.is_null() || il2cpp::object_class(comp) as usize != t.class_ptr {
                return false; // collected or the slot was reused
            }
            expiry_checks.push((*k, comp, t.getter, t.src.clone()));
            true // still-on-screen? decided outside the lock
        });
    } // TRACKED released HERE — before any set_text call, so re-entry can't self-deadlock.

    // PHASE 2 — apply OUTSIDE every lock. set_text may now safely re-enter our hook.
    let now = Instant::now(); // shared by the hold deadlines below + the PHASE 3 sweep

    // Resolve the deferred age-limit checks: an entry whose component STILL shows the exact source
    // keeps waiting (timestamp refreshed — the translation is just slow, e.g. a deep queue); one
    // whose text moved on ages out exactly as before. read_text runs lock-free here.
    if !expiry_checks.is_empty() {
        let mut keep: Vec<usize> = Vec::new();
        for (k, comp, getter, src) in &expiry_checks {
            // Destroyed components must AGE OUT here: their managed wrapper still holds the old
            // source string (get_text is a plain field read that "succeeds" after destruction), so
            // without the m_CachedPtr check a closed story view's components would be refreshed as
            // "still waiting" forever — pinned zombies awaiting an eventual crash-on-apply.
            if il2cpp::unity_object_destroyed(*comp) {
                continue;
            }
            let waiting = match unsafe { read_text(*comp, *getter) } {
                Some(cur) => cur == *src || norm_for_guard(&cur) == norm_for_guard(src),
                None => false,
            };
            if waiting {
                keep.push(*k);
            }
        }
        if let Ok(mut tracked) = TRACKED.lock() {
            for (k, ..) in &expiry_checks {
                if keep.contains(k) {
                    if let Some(t) = tracked.get_mut(k) {
                        t.ts = now;
                    }
                } else {
                    tracked.remove(k);
                }
            }
        }
    }
    let mut new_holds: Vec<(usize, Hold)> = Vec::new();
    for a in actions {
        let comp = a.handle.target();
        // A strong GCHandle keeps the managed wrapper reachable forever, so null means corruption,
        // not collection — the REAL liveness signal is Unity's m_CachedPtr (0 = the component was
        // Destroyed). set_text on a destroyed component raises a managed exception that unwinds
        // through our native frames (abort), so this check is load-bearing, not an optimization.
        if comp.is_null() || il2cpp::unity_object_destroyed(comp) {
            continue;
        }
        // Validity guard: the component's class must equal the one captured when it was tracked
        // (the GCHandle already rules out ABA, so this only rejects genuine corruption; it accepts
        // the real runtime subclass — e.g. TextMeshProUGUI — which an exact-name whitelist wrongly
        // dropped, silently breaking re-apply for the most common text type).
        if il2cpp::object_class(comp) as usize != a.class_ptr {
            continue;
        }
        // CRITICAL for story stability: only re-apply if the component STILL shows the source we
        // translated — exactly, or up to formatting (tags/whitespace: Flash-imported labels re-format
        // the string they were handed). If the game has since changed its text — the dialogue advanced
        // to the next line, or a shared component was reused — re-setting our (now stale) translation
        // desyncs the StoryTimeline clip → no text box + can't advance (soft-lock). When we can't read
        // the current text (no getter), skip to be safe rather than risk a stale re-apply.
        let cur = unsafe { read_text(comp, a.getter) };
        let still_ours = match &cur {
            Some(c) => *c == a.src || norm_for_guard(c) == norm_for_guard(&a.src),
            None => false,
        };
        if !still_ours {
            // Throttled diagnostic: a swap dying HERE is otherwise invisible — the translation is
            // cached AND tracked, yet the on-screen element never flips.
            let nowms = crate::tools::time::now_ms();
            if nowms.saturating_sub(GUARD_MISS_LOG_MS.load(Ordering::Relaxed)) > 2000 {
                GUARD_MISS_LOG_MS.store(nowms, Ordering::Relaxed);
                let s: String = a.src.chars().take(40).collect();
                let c: String = cur.as_deref().unwrap_or("<unreadable>").chars().take(40).collect();
                log(&format!("[mtl] re-apply skipped: text moved on (was {s:?}, now {c:?})"));
            }
            continue;
        }
        // Fit the translation to the source's wrapped box shape (else long French spills out), and
        // use the FITTED form consistently below — it's what the game will re-feed and re-read.
        let dst = fit_to_source(&a.dst, &a.src);
        crate::loc_settext::record_emitted(&dst); // so the re-set isn't re-translated
        let ns = il2cpp::new_string(&dst);
        if ns.is_null() {
            continue;
        }
        unsafe {
            let f: SetFn = std::mem::transmute(a.tramp);
            f(comp, ns, a.mi as Method);
        }
        // If the translation is LONGER than the source, the timeline's English-sized typewriter would
        // clip its tail (maxVisibleCharacters only animates up to the English count). Force the full
        // reveal now and keep re-asserting it (PHASE 3) until the line advances. Only for TMP-derived
        // components (set_maxvis != 0); shorter/equal translations and non-TMP components never
        // truncate, so their normal typewriter reveal is left completely untouched. Compare VISIBLE
        // glyphs on both sides (src still carries rich-text tags; dst is already tag-free) so a tagged
        // source can't over-count and hide a genuinely-longer translation from the hold.
        if a.set_maxvis != 0 && visible_char_count(&dst) > visible_char_count(&a.src) {
            unsafe { set_i32_prop(comp, a.set_maxvis, REVEAL_ALL) };
            new_holds.push((
                comp as usize,
                Hold {
                    handle: a.handle,
                    class_ptr: a.class_ptr,
                    getter: a.getter,
                    set_maxvis: a.set_maxvis,
                    dst,
                    until: now + std::time::Duration::from_secs(HOLD_SECS),
                },
            ));
        }
        // a.handle dropped here (unless moved into a Hold above) → GC keep-alive released
    }
    if !new_holds.is_empty() {
        if let Ok(mut hold) = REVEAL_HOLD.lock() {
            for (k, h) in new_holds {
                if hold.len() >= HOLD_CAP {
                    // bound memory: evict the soonest-to-expire (its GCHandle frees here, main thread)
                    if let Some(old) = hold.iter().min_by_key(|(_, v)| v.until).map(|(k, _)| *k) {
                        hold.remove(&old);
                    }
                }
                hold.insert(k, h);
            }
        }
    }

    if !leftover.is_empty() {
        if let Ok(mut r) = RESULTS.lock() {
            r.extend(leftover);
        }
    }

    // PHASE 3 — reveal-hold: re-assert the full reveal on longer translations against the story
    // timeline's per-frame maxVisibleCharacters re-drive. Runs every frame. Every call here is
    // main-thread and touches only NON-hooked members (get_text / set_maxVisibleCharacters), so it
    // can't re-enter the setter pipeline — safe under the lock. `retain` releases finished entries and
    // frees their GCHandle on this (main) thread.
    if let Ok(mut hold) = REVEAL_HOLD.lock() {
        hold.retain(|_, h| {
            if now >= h.until {
                return false; // hold window elapsed
            }
            let comp = h.handle.target();
            if comp.is_null()
                || il2cpp::object_class(comp) as usize != h.class_ptr
                || il2cpp::unity_object_destroyed(comp)
            {
                return false; // collected, slot reused, or the component was Destroyed
            }
            match unsafe { read_text(comp, h.getter) } {
                Some(cur) if cur == h.dst => {
                    unsafe { set_i32_prop(comp, h.set_maxvis, REVEAL_ALL) };
                    true // still our line → keep it fully revealed
                }
                _ => false, // advanced / changed → release (never fight the next line's reveal)
            }
        });
    }
}

// ─── background worker (pure Rust — NEVER touches IL2CPP) ────────────────────────────────────────

fn worker_main() {
    let mut since_flush = 0usize;
    let mut last_flush = Instant::now();
    let mut last_depth_log: Option<Instant> = None;
    loop {
        // Wait for work, then drain a batch.
        let (batch, epoch) = {
            let mut inner = match REQ.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            // Parked (translation or Overseer disabled): wait on the condvar rather than exiting,
            // so re-enabling is instant and we never leak a thread per toggle. Also the natural
            // place to release the NLLB model once we've been idle long enough.
            while !WORKER_ACTIVE.load(Ordering::Relaxed) {
                crate::nllb::maybe_unload_idle();
                inner = match CV.wait_timeout(inner, std::time::Duration::from_secs(5)) {
                    Ok((g, _)) => g,
                    Err(_) => return,
                };
            }
            while inner.pqueue.is_empty() && inner.queue.is_empty() {
                // An idle queue is also when the resident model should go, if the user asked for it.
                crate::nllb::maybe_unload_idle();
                if !WORKER_ACTIVE.load(Ordering::Relaxed) {
                    break;
                }
                if since_flush > 0 {
                    // Unsaved results + an idle queue: park with a TIMEOUT and persist the session
                    // tail if still idle. An indefinite wait here lost everything translated after
                    // the last periodic flush whenever the game exited from a quiet screen.
                    inner = match CV.wait_timeout(inner, std::time::Duration::from_secs(6)) {
                        Ok((g, _)) => g,
                        Err(_) => return,
                    };
                    if inner.pqueue.is_empty() && inner.queue.is_empty() {
                        drop(inner); // flush touches CACHE/FLUSH_LOCK only — never under REQ
                        since_flush = 0;
                        last_flush = Instant::now();
                        flush();
                        inner = match REQ.lock() {
                            Ok(g) => g,
                            Err(_) => return,
                        };
                    }
                } else {
                    // Bounded wait (not `CV.wait`): the idle-unload check above has to keep running
                    // while nothing is queued — that IS the idle case it exists for.
                    inner = match CV.wait_timeout(inner, std::time::Duration::from_secs(10)) {
                        Ok((g, _)) => g,
                        Err(_) => return,
                    };
                }
            }
            let epoch = inner.epoch;
            // Priority lane first, in EXTRA-SMALL batches: on-screen text's first-view latency =
            // in-flight batch + own batch, so smaller = the visible line lands in a few seconds.
            // Bulk (pre-warm) only runs when no on-screen text is waiting.
            let batch: Vec<String> = if !inner.pqueue.is_empty() {
                let n = inner.pqueue.len().min(PRIORITY_BATCH);
                (0..n).filter_map(|_| inner.pqueue.pop_front()).collect()
            } else {
                let n = inner.queue.len().min(BATCH);
                (0..n).filter_map(|_| inner.queue.pop_front()).collect()
            };
            (batch, epoch)
        };

        if batch.is_empty() {
            continue; // woken by a suspend/resume, not by work
        }
        mark_activity(); // real work → the model is in use, so the idle-unload clock restarts
        // Resolve the direction for this batch.
        let lang = crate::settings::tl_lang().unwrap_or_default();
        let tgt = match target_flores(&lang) {
            Some(t) => t,
            None => {
                mark_attempted_and_unpend(&batch);
                continue;
            }
        };
        let src = source_flores();

        // Protect placeholders/tags, translate, then restore them (all off the game thread).
        let (protected, maps): (Vec<String>, Vec<Vec<String>>) =
            batch.iter().map(|s| protect(s)).unzip();
        let mut outs: Vec<Option<String>> = engine_translate(src, tgt, &protected)
            .into_iter()
            .zip(batch.iter().zip(maps.iter()))
            .map(|(o, (s, m))| {
                // Quality gate: reject a DEGENERATE, mostly-copied NMT output. For a non-English
                // target a real translation contains no distinctly-English function words — if they
                // survive (checked on the pre-restore output, so masked names can't interfere), the
                // model barely translated (the "English mixed with French" garble). Drop it (→ None,
                // marked attempted) so the line stays cleanly in the source language instead of a mix.
                // Two calibrations so the gate can't nuke GOOD translations: (1) >=2 DISTINCT English
                // words required — "but"/"race"/"us" are real French words, one hit proves nothing;
                // (2) short choice-sized fragments are exempt entirely (they get the copy-retry below,
                // and a 3-word string can't exhibit "mixed-language garble" anyway).
                o.filter(|t| tgt == "eng_Latn" || short_source(s) || english_word_hits(t) < 2)
                    .map(|t| restore(&t, m))
                    // CORRECTNESS gate, applied AFTER restore so it judges what the game will really
                    // receive (placeholders are masked during translation and put back by restore).
                    // Unlike the degenerate check above, this one has NO short-source exemption —
                    // short sources are precisely where the fatal hallucination happens ("Jan" →
                    // "Je suis d'accord avec Jan" → the game's DateTime.Parse throws).
                    .filter(|t| {
                        let ok = translation_safe(s, t);
                        if !ok {
                            crate::tools::trace(&format!("[mtl] REJECTED unsafe translation {s:?} => {t:?} (placeholder/expansion guard)"));
                        }
                        ok
                    })
            })
            .collect();

        // Copy-retry for short fragments. NLLB tends to COPY 1-4 word Title-Case inputs verbatim
        // ("The beach." → "The beach."), and the identity check below would then blacklist them for
        // the whole session — the reason no event-choice string ever reached the cache. Lowercasing
        // and dropping the terminal punctuation usually breaks the copy behavior; the output is then
        // re-cased and re-punctuated to match the source's shape.
        let retry_idx: Vec<usize> = batch
            .iter()
            .enumerate()
            .filter(|(i, s)| {
                short_source(s)
                    && match outs.get(*i) {
                        Some(Some(d)) => d.is_empty() || d == *s,
                        Some(None) => true,
                        None => false, // engine returned short — nothing to retry into
                    }
            })
            .map(|(i, _)| i)
            .collect();
        if !retry_idx.is_empty() {
            let variants: Vec<String> = retry_idx
                .iter()
                .map(|&i| {
                    protected[i]
                        .trim_end_matches(['.', '!', '?', '…', '。', '！', '？', '．'])
                        .trim()
                        .to_lowercase()
                })
                .collect();
            let retry_outs = engine_translate(src, tgt, &variants);
            for (k, &i) in retry_idx.iter().enumerate() {
                let Some(Some(t)) = retry_outs.get(k).cloned() else {
                    continue;
                };
                if t.is_empty() || t == variants[k] {
                    continue; // copied again → genuinely untranslatable, let it fail
                }
                let mut fixed = restore(&t, &maps[i]);
                // Re-case: if the source starts uppercase, so should the translation.
                if batch[i].chars().next().is_some_and(|c| c.is_uppercase()) {
                    fixed = upper_first(&fixed);
                }
                // Carry the source's terminal punctuation back over if the variant stripped it —
                // unless the translation already ends in ANY terminal punctuation (its own script's
                // included: a Japanese output ending "。" must not gain a trailing ASCII period).
                if let Some(tail) = batch[i].chars().last().filter(|c| ".!?…".contains(*c)) {
                    if !fixed.ends_with(['.', '!', '?', '…', '。', '！', '？', '．', '।']) {
                        fixed.push(tail);
                    }
                }
                // The retry result must pass the SAME correctness gate as the first attempt. This
                // path used to insert unguarded, which silently un-did the guard: the first attempt's
                // hallucination got REJECTED (→ None), `Some(None) => true` above queued a retry, and
                // the retry's hallucination went straight into the cache. That is exactly how
                // "Mama Yuurai" => "Maman yuurai est une mère yuurai." got cached AFTER the guard
                // shipped — rejected at the front door, re-admitted through the back.
                if !translation_safe(&batch[i], &fixed) {
                    crate::tools::trace(&format!("[mtl] REJECTED unsafe retry {:?} => {fixed:?} (placeholder/expansion guard)", batch[i]));
                    continue;
                }
                if let Some(slot) = outs.get_mut(i) {
                    *slot = Some(fixed);
                }
            }
        }

        // Language-switch race: if reload() bumped the epoch while we were translating, these
        // results belong to the OLD language — inserting them would poison the new language's
        // in-memory cache and get flushed into the wrong mtl.json. Drop the whole batch (reload
        // already cleared pending/ATTEMPTED, so nothing needs unwinding).
        if REQ.lock().map(|i| i.epoch != epoch).unwrap_or(true) {
            continue;
        }

        let mut new_results: Vec<(String, String, u64)> = Vec::new();
        let mut produced = 0usize;
        let mut rejected_lines: Vec<String> = Vec::new(); // logged AFTER the locks release (log = file I/O)
        {
            let mut pend = REQ.lock().ok();
            let mut att = ATTEMPTED.lock().ok();
            let mut cache = CACHE.lock().ok();
            for (s, o) in batch.iter().zip(outs.into_iter()) {
                if let Some(p) = pend.as_mut() {
                    p.pending.remove(s);
                }
                match o {
                    Some(dst) if !dst.is_empty() && dst != *s => {
                        // Success → CACHE (which request() already checks first, so it needn't also
                        // go into ATTEMPTED — keeping ATTEMPTED purely a negative cache of failures).
                        if let Some(c) = cache.as_mut() {
                            if c.len() >= CACHE_CAP {
                                c.clear();
                            }
                            c.insert(s.clone(), dst.clone());
                        }
                        note_output(&dst); // permanent feedback-loop guard (see OUT_HASHES)
                        new_results.push((s.clone(), dst, epoch));
                        produced += 1;
                    }
                    _ => {
                        // Failure/identity → negative cache so it's never retried this session — and
                        // LOGGED (capped per batch): a silent rejection is indistinguishable from
                        // "never queued" when debugging coverage gaps.
                        if rejected_lines.len() < 4 {
                            let snip: String = s.chars().take(60).collect();
                            rejected_lines.push(format!(
                                "[mtl] gave up on {snip:?} (NMT copied the source or leaked English)"
                            ));
                        }
                        if let Some(a) = att.as_mut() {
                            if a.len() >= ATTEMPTED_CAP {
                                a.clear();
                            }
                            a.insert(s.clone());
                        }
                    }
                }
            }
        }
        for line in &rejected_lines {
            crate::tools::trace(line); // per-string coverage detail — verbose level
        }
        if !new_results.is_empty() {
            if let Ok(mut r) = RESULTS.lock() {
                r.extend(new_results);
            }
        }
        since_flush += produced;
        let queue_len = REQ.lock().map(|i| i.pqueue.len() + i.queue.len()).unwrap_or(0);
        // Deep-queue visibility: a skip-flood can outrun NLLB by minutes. Say so (throttled to one
        // line per 30s) instead of letting "queue saturated" look identical to "pipeline broken".
        if queue_len > QUEUE_CAP / 2
            && last_depth_log.is_none_or(|t| t.elapsed().as_secs() >= 30)
        {
            last_depth_log = Some(Instant::now());
            let dropped = QUEUE_DROPPED.swap(0, Ordering::Relaxed);
            log(&format!(
                "[mtl] queue deep: {queue_len} waiting, {dropped} evicted since last report \
                 (skip-flood backlog; on-screen text is prioritized)"
            ));
        }
        // Flush when enough new entries have accumulated AND at most every ~20s — so a busy screen
        // doesn't rewrite the whole cache file dozens of times a second. (The session TAIL — results
        // finished after the last periodic flush — is covered by the idle-timeout flush in the wait
        // loop above, which fires ~6s after the queue drains.)
        if since_flush >= FLUSH_EVERY && last_flush.elapsed().as_secs() >= 20 {
            since_flush = 0;
            last_flush = Instant::now();
            flush();
        }
    }
}

fn mark_attempted_and_unpend(batch: &[String]) {
    if let Ok(mut p) = REQ.lock() {
        for s in batch {
            p.pending.remove(s);
        }
    }
    if let Ok(mut a) = ATTEMPTED.lock() {
        for s in batch {
            a.insert(s.clone());
        }
    }
}

/// The ONLY engine-specific point. Everything else (queue/cache/re-apply) is engine-agnostic.
fn engine_translate(src: &str, tgt: &str, texts: &[String]) -> Vec<Option<String>> {
    crate::nllb::translate_batch(src, tgt, texts)
}

// ─── placeholder / rich-text protection (port of Overseer's sentinel trick) ─────────────────────
//
// NMT mangles `{0}`-style placeholders and `<color=…>` rich-text tags (it "translates" or drops
// them), which corrupts the on-screen string and its layout. Before translating we replace each such
// token with an ASCII sentinel `NMZ<n>ZMN` (padded with spaces so the tokenizer keeps it separate);
// these survive NMT intact. Afterwards we splice the original tokens back — tolerantly, since NMT may
// alter the spacing/case around the sentinel. Runs on the worker thread (never the game thread).

static SENTINEL_RE: Lazy<fancy_regex::Regex> =
    Lazy::new(|| fancy_regex::Regex::new(r"(?i)NMZ\s*(\d+)\s*ZMN").unwrap());

/// Replace `{…}` placeholders, `<…>` tags, AND embedded protected names with sentinels; return the
/// masked string + the token map (so skill/character names stay English even inside a description).
fn protect(s: &str) -> (String, Vec<String>) {
    let mut map: Vec<String> = Vec::new();

    // 1. `{…}` placeholders + `<…>` tags (char-scan).
    let out = if s.contains('{') || s.contains('<') {
        let mut out = String::with_capacity(s.len() + 8);
        let mut chars = s.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            let close = match c {
                '{' => Some('}'),
                '<' => Some('>'),
                _ => None,
            };
            if let Some(close) = close {
                if let Some(rel) = s[i..].find(close) {
                    let end = i + rel + close.len_utf8();
                    if c == '{' {
                        // {..} placeholder: mask so it survives NMT verbatim.
                        out.push_str(&format!(" NMZ{}ZMN ", map.len()));
                        map.push(s[i..end].to_string());
                    } else {
                        // <..> rich-text tag (color/size/b): STRIP it. Masking made NMT reorder the
                        // sentinels so the raw tags leaked as visible text; stripping keeps the content
                        // between the tags (a colored number just becomes an inline number).
                        out.push(' ');
                    }
                    while let Some(&(j, _)) = chars.peek() {
                        if j < end {
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    continue;
                }
            }
            out.push(c);
        }
        out
    } else {
        s.to_string()
    };

    // 2. Embedded protected names (skill/character/race names) → keep verbatim through NMT.
    let re_guard = NAME_RE.load();
    let Some(re) = re_guard.as_ref() else {
        return (out, map);
    };
    let mut masked = String::with_capacity(out.len());
    let mut last = 0usize;
    let mut iter = re.find_iter(&out);
    while let Some(Ok(m)) = iter.next() {
        masked.push_str(&out[last..m.start()]);
        masked.push_str(&format!(" NMZ{}ZMN ", map.len()));
        map.push(m.as_str().to_string());
        last = m.end();
    }
    masked.push_str(&out[last..]);
    (masked, map)
}

/// Splice protected tokens back into a translated string (tolerant of NMT-altered sentinel spacing).
fn restore(s: &str, map: &[String]) -> String {
    if map.is_empty() {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut last = 0usize;
    let mut iter = SENTINEL_RE.captures_iter(s);
    while let Some(Ok(caps)) = iter.next() {
        let whole = caps.get(0).unwrap();
        out.push_str(&s[last..whole.start()]);
        if let Some(tok) = caps
            .get(1)
            .and_then(|g| g.as_str().parse::<usize>().ok())
            .and_then(|idx| map.get(idx))
        {
            out.push_str(tok);
        }
        last = whole.end();
    }
    out.push_str(&s[last..]);
    out
}

// ─── lifecycle ──────────────────────────────────────────────────────────────────────────────────

/// Spawn the worker once + kick off engine model load. Idempotent. Called from boot.
pub fn spawn_worker() {
    if WORKER_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Err(e) = std::thread::Builder::new()
        .name("mtl-worker".into())
        .spawn(worker_main)
    {
        log(&format!("[mtl] worker spawn failed: {e}"));
        WORKER_STARTED.store(false, Ordering::Release);
        return;
    }
    crate::nllb::load_async();
}

/// Load the active language's mtl.json (+ manual.json overrides) into the cache and clear all
/// transient per-language state, bumping the epoch so in-flight old-language results are discarded.
pub fn reload() {
    let lang = crate::settings::tl_lang().unwrap_or_default();
    let mut new_cache: FnvHashMap<String, String> = FnvHashMap::default();
    if !lang.is_empty() {
        let dir = crate::paths::dll_dir().join("glossary").join(&lang);
        let mut dropped = 0usize;
        for file in ["mtl.json", "manual.json"] {
            // manual overrides mtl (loaded second)
            let is_manual = file == "manual.json";
            if let Ok(s) = std::fs::read_to_string(dir.join(file)) {
                if let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&s) {
                    for (k, v) in map {
                        if k.is_empty() || v.is_empty() {
                            continue;
                        }
                        // Purge entries that are UNSAFE to hand back to the game (mangled {n}
                        // placeholders, or a short source that hallucinated into a sentence). These
                        // were cached before the worker gained `translation_safe`, and they are not
                        // cosmetic: the persisted "Jan" => "Je suis d'accord avec Jan" is what made
                        // the Scout tab dead — the game parsed it as a date and threw. Purge even
                        // manual.json here: a hand-written entry that breaks String.Format/DateTime
                        // still crashes the game, and silently keeping it would be worse than
                        // dropping it.
                        //
                        // Date tokens go too, regardless of how innocent the value looks. `Feb` =>
                        // `Février` passes every structural check and is still fatal — DateTime.Parse
                        // wants the English token. `is_protected_name` already stops us SERVING these
                        // (it runs before the cache in both Localize.Get and set_text), so this is
                        // defence in depth: no live poison in the file, and no reliance on every
                        // future cache reader remembering to check protection first. The real cache
                        // held 'Mar' => 'Mar Mar Mar' and 'Sun' => 'Le Soleil'.
                        if !translation_safe(&k, &v) || is_date_token(k.trim()) {
                            dropped += 1;
                            continue;
                        }
                        // Drop DEGENERATE cached translations — mostly-copied NMT output where English
                        // function words survived (the "English mixed with French" garble that was
                        // cached before the worker gained this filter). Only the auto mtl.json cache;
                        // user manual.json overrides are trusted and never touched. English target keeps
                        // English text, so skip the check there. Same calibration as the worker gate:
                        // mask protected names first (a restored "Make Debut" inside a good French line
                        // is not an English leak), require >=2 DISTINCT English words, and exempt short
                        // values — otherwise valid entries were re-purged on every boot.
                        // Cheap word-count first; the (expensive, backtracking) name-mask regex only
                        // runs to EXONERATE a suspect — this loop covers 25k entries on boot.
                        if !is_manual && lang != "en" && !short_source(&v) && english_word_hits(&v) >= 2
                        {
                            let hits = match NAME_RE.load().as_ref() {
                                Some(re) => english_word_hits(&re.replace_all(&v, " ")),
                                None => 2, // no regex yet → trust the raw count
                            };
                            if hits >= 2 {
                                dropped += 1;
                                continue;
                            }
                        }
                        new_cache.insert(k, v);
                    }
                }
            }
        }
        // Second pass — drop TRANSLATION CHAINS: an entry whose KEY is another entry's OUTPUT is us
        // re-translating our own French ("Hint Lvl 1" => "Indice Lvl 1" then "Indice Lvl 1" =>
        // "L'indice Lvl est de 1."). The EMITTED set that should catch this live is generational and
        // ages out, so chains accumulated in the file. Keep the first link, drop the rest.
        let outputs: HashSet<u64> = new_cache.values().map(|v| out_hash(v)).collect();
        let before = new_cache.len();
        new_cache.retain(|k, v| k == v || !outputs.contains(&out_hash(k)));
        dropped += before - new_cache.len();
        if dropped > 0 {
            log(&format!("[mtl] dropped {dropped} degenerate cached translations (will re-translate cleanly or stay in the original)"));
        }
    }
    let n = new_cache.len();
    // Rebuild the own-output hash set — one entry per cache VALUE, so hot paths can ask "is this
    // string something WE produced?" in O(1) and forward it untouched instead of re-translating it.
    {
        let hashes: HashSet<u64> = new_cache.values().map(|v| out_hash(v)).collect();
        if let Ok(mut h) = OUT_HASHES.write() {
            *h = hashes;
        }
    }
    if let Ok(mut c) = CACHE.lock() {
        *c = new_cache;
    }
    if let Ok(mut l) = CACHE_LANG.lock() {
        *l = lang.clone();
    }
    if let Ok(mut inner) = REQ.lock() {
        inner.queue.clear();
        inner.pqueue.clear();
        inner.pending.clear();
        inner.epoch = inner.epoch.wrapping_add(1);
    }
    if let Ok(mut a) = ATTEMPTED.lock() {
        a.clear();
    }
    if let Ok(mut r) = RESULTS.lock() {
        r.clear();
    }
    // Deliberately do NOT clear TRACKED here: reload() runs on the webui/boot thread, and dropping a
    // GCHandle calls il2cpp_gchandle_free — an IL2CPP API that must not run on an unattached thread.
    // The epoch bump above makes every tracked entry stale; the main-thread pump's eviction sweep
    // (`retain` on epoch mismatch) drops them safely on the next frame.
    if !lang.is_empty() {
        log(&format!("[mtl] {lang} cache loaded: {n} entries"));
    }
    prewarm();
}

/// Pre-translate a bundled list of common UI strings (`<dll_dir>/glossary/common_ui.json`, a JSON
/// array of English labels) so the first time you hit a menu it's already French instead of showing
/// English then swapping. No-op unless MTL is on and the model is ready; uncached items only, and
/// `request()` already dedups + gates. Called on language activation and after the model loads.
pub fn prewarm() {
    if !crate::settings::mtl_enabled() || !crate::nllb::ready() {
        return;
    }
    let path = crate::paths::dll_dir().join("glossary").join("common_ui.json");
    if let Ok(s) = std::fs::read_to_string(&path) {
        if let Ok(list) = serde_json::from_str::<Vec<String>>(&s) {
            let mut n = 0;
            for item in list.iter().take(500) {
                request(item);
                n += 1;
            }
            if n > 0 {
                log(&format!("[mtl] pre-warming {n} common UI strings"));
            }
        }
    }
}

/// Serializes concurrent flushes (worker vs. a language-switch on the webui thread) so they can't
/// clobber each other's temp file or interleave writes.
static FLUSH_LOCK: Mutex<u64> = Mutex::new(0);

/// Persist the current in-memory CACHE snapshot to `<dll_dir>/glossary/<CACHE_LANG>/mtl.json`.
/// CACHE is already the merged truth (disk ∪ session), so we serialize it directly — no disk
/// read-merge (that was O(N²) and raced a second writer). Atomic write via a UNIQUE temp + rename.
/// Worker-thread / language-switch only — pure file I/O, never IL2CPP.
pub fn flush() {
    // Hold FLUSH_LOCK for the whole operation; its counter also makes the temp name unique.
    let mut seq = match FLUSH_LOCK.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let lang = CACHE_LANG.lock().map(|l| l.clone()).unwrap_or_default();
    if lang.is_empty() {
        return;
    }
    let map: HashMap<String, String> = match CACHE.lock() {
        Ok(c) if !c.is_empty() => c.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        _ => return,
    };
    let dir = crate::paths::dll_dir().join("glossary").join(&lang);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(json) = serde_json::to_string_pretty(&map) {
        *seq = seq.wrapping_add(1);
        let tmp = dir.join(format!("mtl.json.{seq}.tmp"));
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, dir.join("mtl.json"));
        }
    }
}

#[cfg(test)]
mod translation_safe_tests {
    use super::{glossary_result_final, placeholder_indices, translation_safe};

    #[test]
    fn rejects_the_scout_killer() {
        // The measured root cause: Localize.Get("Jan") came back as a hallucinated sentence, and the
        // game's DateTime.Parse threw Format_UnknownDateTimeWord, killing the gacha view.
        assert!(!translation_safe("Jan", "Je suis d'accord avec Jan"));
    }

    #[test]
    fn rejects_word_repetition_stutter() {
        // NMT stutter on short inputs, seen live in the skills page and cache.
        assert!(!translation_safe("Focus", "Focus Focus"));
        assert!(!translation_safe("Mar", "Mar Mar Mar"));
        assert!(translation_safe("Focus", "Concentration")); // a real translation is fine
    }

    #[test]
    fn rejects_inventory_page_garbage() {
        // Every one of these was live in the fr cache and visible on the inventory / menu pages.
        assert!(!translation_safe("Held", "Il est tenu")); // the Held-count label, on every item
        assert!(!translation_safe("Pleasing Parfait", "Il est parfaitement agréable"));
        assert!(!translation_safe("Outer Post Raffle Ball", "Le Raffle Ball est un ballon de raffle."));
        assert!(!translation_safe("Options", "Options d'options"));
        assert!(!translation_safe("Other", "Other autre"));
    }

    #[test]
    fn keeps_legit_short_labels_and_names() {
        // Must NOT be caught by the hallucination guards — these are correct.
        assert!(translation_safe("Sunshine Doll", "La poupée Sunshine"));
        assert!(translation_safe("Jelly Carrot Mini", "La carotte à gelée mini"));
        assert!(translation_safe("OK", "D'accord"));
        assert!(translation_safe("Team Rank", "Rang de l'équipe"));
        assert!(translation_safe("Convert", "Convertir")); // prefix-of-translation, not an echo
        assert!(translation_safe("Details", "Détails"));
        // A source that is ITSELF a clause may legitimately translate to one.
        assert!(translation_safe("It's a bonus", "C'est un bonus"));
    }

    #[test]
    fn glossary_partial_is_not_final() {
        // The skills-page leak: the glossary swapped ONE word and returned the rest of the English
        // sentence, which the finality check accepted (no function-word hits) and locked in. A partial
        // must fall through to NMT with the original, not be served as final.
        assert!(!glossary_result_final(
            "Slightly increase velocity on a straight. (Long)",
            "Slightly increase velocity on a straight. (Longue)"
        ));
        // A fully-covered label has no surviving source words → final, instant hit.
        assert!(glossary_result_final("Long Straightaways", "Longues Lignes droites"));
    }

    #[test]
    fn preserved_proper_names_are_not_a_partial() {
        // With no NAME_RE loaded in the test harness, an unmasked proper name that survives WOULD
        // read as a partial — that's the pre-mask behaviour. The real protection is the NAME_RE mask
        // (exercised live). Here we assert the shape the mask produces: once names are gone, a fully
        // translated line with only names surviving has no OTHER survivors.
        // "trophies"/"Career" are translated; only the (masked-in-prod) name would remain.
        assert!(glossary_result_final("all trophies", "tous les trophées"));
    }

    #[test]
    fn rejects_mangled_placeholders_found_in_the_real_cache() {
        assert!(!translation_safe("{0} <color=#FF6D26>+{1}</color>", "+{1} {0} {}")); // bare {} => throws
        assert!(!translation_safe("Obtained from {0} Day {1}.", "Obtenu à partir du jour {1}.")); // {0} dropped
        assert!(!translation_safe("Turns left: <color=#{1}>{0}</color>", "Il tourne à gauche: {0}")); // {1} dropped
    }

    #[test]
    fn keeps_good_translations() {
        // Real entries from the live cache that MUST survive — a correctness gate that eats these
        // would silently untranslate the game, which is its own kind of broken.
        assert!(translation_safe("OK", "D'accord"));
        assert!(translation_safe("Team Rank", "Rang de l'équipe"));
        assert!(translation_safe("Ver. {0}", "Voir {0}"));
        assert!(translation_safe("Until {0} (UTC)", "Jusqu'à ce que {0} (UTC)"));
        assert!(translation_safe("Trainer ID: {0}", "ID du formateur: {0}"));
        assert!(translation_safe("Level Uncap (Lvl {0})", "Niveau Uncap (Lvl {0})"));
        assert!(translation_safe(
            "Additional data ({0:0.00} {1}) needs to be downloaded.",
            "Des données supplémentaires ({0:0.00} {1}) doivent être téléchargées."
        ));
        // Long prose may legitimately expand — the word guard only applies to <=3-word sources.
        assert!(translation_safe(
            "Hey, quit zoning out! You're wasting time!",
            "Arrêtez de faire des zones, vous perdez du temps !"
        ));
    }

    #[test]
    fn date_tokens_are_protected_regardless_of_how_innocent_the_value_looks() {
        // Every one of these was live in the real cache and is fatal to DateTime.Parse. They pass the
        // structural guards (short, no placeholders), so ONLY the date-token protection stops them.
        for t in ["Jan", "Feb", "Mar", "May", "Jul", "Sep", "Oct", "Sun", "Mon", "AM", "PM", "January", "Sunday"] {
            assert!(super::is_protected_name(t), "{t} must never be translated");
        }
        // A plausible-looking translation is still fatal: the game wants the English token back.
        assert!(super::is_protected_name("Feb")); // cache had 'Feb' => 'Février'
        assert!(super::is_protected_name("Sun")); // cache had 'Sun' => 'Le Soleil'
        // Ordinary words that merely resemble nothing special must stay translatable.
        assert!(!super::is_protected_name("Race"));
        assert!(!super::is_protected_name("Training"));
    }

    #[test]
    fn placeholder_parsing() {
        assert_eq!(placeholder_indices("{0:D1}:{1:D2}:{2:D2}").len(), 3);
        assert_eq!(placeholder_indices("{}").len(), 0); // not a placeholder — and fatal to String.Format
        assert_eq!(placeholder_indices("no slots here").len(), 0);
    }
}
