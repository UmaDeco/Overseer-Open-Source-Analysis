//! Legacy / inheritance analysis — affinity matrix, parent recommendations, spark modelling and
//! the multi-career **legacy loop** planner.
//!
//! ## What this borrows from Umamusume-Legacy-Loops, and what it does differently
//!
//! The Legacy Loops guide (dadudeian.github.io/Umamusume-Legacy-Loops) makes one sharp
//! observation: inheritance is not a per-career decision, it is a *rotation*. If you pick four
//! characters and rotate which one is the trainee while the other three supply the legacy, you can
//! keep double-◎ affinity indefinitely instead of re-solving the problem every run. Its own tool is
//! a static four-slot rotation chart driven by a community compatibility table, and it explicitly
//! excludes the legacy-side affinity contribution because the author didn't know how it was
//! computed.
//!
//! Overseer is in a much better position on both counts, so this module adapts the *idea* rather
//! than the implementation:
//!
//! * **Exact numbers, not a community table.** Overseer already hooks the game's own
//!   `SingleModeUtils.CalcRelationPoint`, so every pairing the player evaluates on Legacy Select
//!   yields the value the game itself uses — including the grandparent chain and the win-saddle
//!   bonus that the reference tool had to leave out. Those observations are persisted, so the
//!   affinity matrix builds itself from play instead of being shipped and going stale.
//! * **Any loop size, scored not asserted.** The reference chart is a fixed four-character
//!   rotation. Here a loop is any 3–6 character set, every rotation is enumerated, and each is
//!   scored on the WEAKEST session it contains (a loop is only as good as its worst career) with a
//!   secondary term for the average. That surfaces loops the fixed chart can't express, and refuses
//!   to recommend one whose third session quietly drops to ○.
//! * **Honest coverage.** A planner over a partially-observed matrix must say so. Every result
//!   reports which pairings are measured, which are estimated from an optional community prior, and
//!   which are unknown — plus exactly which Legacy Select screens would fill the gaps.
//!
//! ## Data sources, in priority order
//!
//! 1. **Live captures** (`note_affinity`) — the game's own value. Authoritative.
//! 2. **Optional prior** — `<dll_dir>/data/legacy_affinity.json`, a `{"<a>-<b>": points}` map a user
//!    can drop in (this is where a community compatibility export goes). Used only where no live
//!    capture exists, and always labelled `estimated`.
//! 3. **Unknown** — reported as such; never guessed.
//!
//! Everything here is pure data + file I/O. It NEVER touches IL2CPP, so it is safe to call from the
//! affinity hook, the response-parse thread, and the web server alike.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

fn log(m: &str) {
    crate::tools::log(m);
}

// ── affinity ranks ──────────────────────────────────────────────────────────────────────────────

/// In-game succession-affinity bands. The game shows ◎ / ○ / △ / ✕ rather than the raw number;
/// these are the thresholds that map one to the other. Kept as named constants (not inline magic)
/// because a future game rebalance changes exactly these three numbers and nothing else.
pub const AFFINITY_DOUBLE_CIRCLE: i64 = 151; // ◎
pub const AFFINITY_CIRCLE: i64 = 51; // ○
pub const AFFINITY_TRIANGLE: i64 = 1; // △

/// Rank symbol for an affinity total. Negative (unknown) yields "".
pub fn affinity_rank(points: i64) -> &'static str {
    match points {
        p if p < 0 => "",
        p if p >= AFFINITY_DOUBLE_CIRCLE => "◎",
        p if p >= AFFINITY_CIRCLE => "○",
        p if p >= AFFINITY_TRIANGLE => "△",
        _ => "✕",
    }
}

/// A 0..1 quality score for an affinity value, used to rank loops. Deliberately NOT linear in the
/// raw points: the game's own reward curve is banded, and the difference between 150 (○) and 151
/// (◎) matters far more than between 151 and 300. Each band maps onto its own sub-range so a
/// rotation that holds ◎ everywhere always outranks one that dips to ○, whatever the raw totals.
pub fn affinity_score(points: i64) -> f64 {
    if points < 0 {
        return 0.0;
    }
    let (base, lo, hi) = match points {
        p if p >= AFFINITY_DOUBLE_CIRCLE => (0.75, AFFINITY_DOUBLE_CIRCLE as f64, 400.0),
        p if p >= AFFINITY_CIRCLE => (0.45, AFFINITY_CIRCLE as f64, AFFINITY_DOUBLE_CIRCLE as f64),
        p if p >= AFFINITY_TRIANGLE => (0.15, AFFINITY_TRIANGLE as f64, AFFINITY_CIRCLE as f64),
        _ => return 0.0,
    };
    let span = (hi - lo).max(1.0);
    let within = (((points as f64) - lo) / span).clamp(0.0, 1.0);
    // Each band is 0.25 wide, so the floor of a band always beats the ceiling of the one below it.
    (base + within * 0.25).min(1.0)
}

// ── the observed affinity matrix ────────────────────────────────────────────────────────────────

/// One measured pairing. `trainee` and `parent` are the game's character ids (NOT card ids — a
/// character's affinity does not depend on which outfit you trained).
#[derive(Clone, Serialize, Deserialize)]
pub struct Observation {
    pub trainee: i64,
    pub parent: i64,
    /// The value the game returned for `CalcRelationPoint(trainee, parent, null)` — the parent's
    /// whole chain (itself + its two grandparents + win bonuses).
    pub chain_points: i64,
    /// Unix ms of the most recent sighting.
    pub seen_ms: u64,
    /// How many times this exact pairing has been observed (confidence signal).
    pub samples: u32,
    /// The lowest chain value ever seen for the pair. With a barren parent (no grandparents, no
    /// win saddles) this converges on the pair's own base relation, which is what a loop plan needs
    /// — a parent's chain bonus follows the parent, not the pairing.
    pub floor_points: i64,
}

/// A measured PAIR total (both parents at once), kept separately: it is what the player actually
/// sees on Legacy Select and the only direct evidence for a full session.
#[derive(Clone, Serialize, Deserialize)]
pub struct PairObservation {
    pub trainee: i64,
    pub p1: i64,
    pub p2: i64,
    pub total: i64,
    pub seen_ms: u64,
}

#[derive(Clone, Serialize, Deserialize, Default)]
struct Store {
    #[serde(default)]
    observations: Vec<Observation>,
    #[serde(default)]
    pairs: Vec<PairObservation>,
    /// Character id → display name, learned as we see them.
    #[serde(default)]
    names: BTreeMap<i64, String>,
    /// Sparks seen on each character's trained record (character id → spark ids), so the planner
    /// can report what a legacy actually carries, not just how compatible it is.
    #[serde(default)]
    sparks: BTreeMap<i64, Vec<i64>>,
}

const MAX_OBSERVATIONS: usize = 4000;
const MAX_PAIRS: usize = 1500;

static STORE: Lazy<Mutex<Store>> = Lazy::new(|| Mutex::new(Store::default()));
static LOADED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static DIRTY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Last full pair total confirmed on Legacy Select — stamped onto the career summary.
static LAST_CONFIRMED_AFFINITY: AtomicI64 = AtomicI64::new(-1);
static LAST_SAVE_MS: AtomicU64 = AtomicU64::new(0);

fn store_path() -> std::path::PathBuf {
    crate::paths::local_file("overseer-legacy.json")
}

fn ensure_loaded() {
    if LOADED.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Ok(s) = std::fs::read_to_string(store_path()) {
        if let Ok(v) = serde_json::from_str::<Store>(s.trim_start_matches('\u{feff}')) {
            if let Ok(mut g) = STORE.lock() {
                *g = v;
            }
        }
    }
}

/// Mark the store dirty. The actual write happens on a background thread.
///
/// This matters because `note_affinity` runs INSIDE the `CalcRelationPoint` detour — on the game's
/// main thread, while the player is scrolling the Legacy Select candidate list. Writing a JSON file
/// there would put disk latency (and any antivirus filter driver) directly into a frame, on the one
/// screen this feature is used from. The saver thread below owns all of that instead.
fn save_throttled() {
    DIRTY.store(true, Ordering::Relaxed);
    start_saver();
}

static SAVER_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn start_saver() {
    if SAVER_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    if std::thread::Builder::new()
        .name("overseer-legacy".into())
        .spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            write_store();
        })
        .is_err()
    {
        SAVER_STARTED.store(false, Ordering::Release);
    }
}

/// Serialise + atomically replace the store file, if anything changed. Background thread only,
/// except for `save_now()` which is an explicit user/lifecycle action.
fn write_store() {
    if !DIRTY.swap(false, Ordering::Relaxed) {
        return;
    }
    LAST_SAVE_MS.store(crate::tools::now_ms(), Ordering::Relaxed);
    let snapshot = match STORE.lock() {
        Ok(g) => g.clone(),
        Err(_) => return,
    };
    if let Ok(json) = serde_json::to_string(&snapshot) {
        let path = store_path();
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Force a write now (career start/finish, and the web UI). Called from background/web threads.
pub fn save_now() {
    write_store();
}

/// Record a single-parent chain observation (the exact value the game computed).
pub fn note_affinity(trainee: i64, parent: i64, chain_points: i64) {
    if !crate::settings::legacy_capture() || trainee <= 0 || parent <= 0 || chain_points < 0 {
        return;
    }
    ensure_loaded();
    let now = crate::tools::now_ms();
    if let Ok(mut g) = STORE.lock() {
        match g
            .observations
            .iter_mut()
            .find(|o| o.trainee == trainee && o.parent == parent)
        {
            Some(o) => {
                o.chain_points = chain_points;
                o.floor_points = o.floor_points.min(chain_points);
                o.samples = o.samples.saturating_add(1);
                o.seen_ms = now;
            }
            None => {
                if g.observations.len() >= MAX_OBSERVATIONS {
                    // Evict the least recently seen — the matrix is a working set, not an archive.
                    if let Some(i) = g
                        .observations
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, o)| o.seen_ms)
                        .map(|(i, _)| i)
                    {
                        g.observations.remove(i);
                    }
                }
                g.observations.push(Observation {
                    trainee,
                    parent,
                    chain_points,
                    seen_ms: now,
                    samples: 1,
                    floor_points: chain_points,
                });
            }
        }
    }
    save_throttled();
}

/// Record the full pair total the player is looking at right now.
pub fn note_pair(trainee: i64, p1: i64, p2: i64, total: i64) {
    if total >= 0 {
        LAST_CONFIRMED_AFFINITY.store(total, Ordering::Relaxed);
    }
    // The trio matrix needs BOTH parent ids to be meaningful; a partial read still gave us the
    // live total above, which is all the career report needs.
    if !crate::settings::legacy_capture() || trainee <= 0 || p1 <= 0 || p2 <= 0 || total < 0 {
        return;
    }
    ensure_loaded();
    let now = crate::tools::now_ms();
    if let Ok(mut g) = STORE.lock() {
        match g
            .pairs
            .iter_mut()
            .find(|o| o.trainee == trainee && o.p1 == p1 && o.p2 == p2)
        {
            Some(o) => {
                o.total = total;
                o.seen_ms = now;
            }
            None => {
                if g.pairs.len() >= MAX_PAIRS {
                    if let Some(i) = g.pairs.iter().enumerate().min_by_key(|(_, o)| o.seen_ms).map(|(i, _)| i) {
                        g.pairs.remove(i);
                    }
                }
                g.pairs.push(PairObservation { trainee, p1, p2, total, seen_ms: now });
            }
        }
    }
    save_throttled();
}

/// The most recent confirmed Legacy Select total (-1 = none this session). Stamped onto the career
/// summary so a completed run reports what inheritance it actually started from.
pub fn last_confirmed_affinity() -> i64 {
    LAST_CONFIRMED_AFFINITY.load(Ordering::Relaxed)
}

/// Learn a character's display name (from whatever screen exposed it).
pub fn note_name(chara: i64, name: &str) {
    if chara <= 0 || name.trim().is_empty() {
        return;
    }
    ensure_loaded();
    let changed = match STORE.lock() {
        Ok(mut g) => {
            if g.names.get(&chara).map(|n| n != name).unwrap_or(true) {
                g.names.insert(chara, name.trim().to_string());
                true
            } else {
                false
            }
        }
        Err(_) => false,
    };
    if changed {
        save_throttled(); // marks dirty AND makes sure the saver thread is running
    }
}

/// Record which sparks a character's trained record carries (used by the planner's legacy value).
pub fn note_sparks(chara: i64, ids: &[i64]) {
    if chara <= 0 || ids.is_empty() {
        return;
    }
    ensure_loaded();
    let changed = match STORE.lock() {
        Ok(mut g) => {
            // Only mark dirty on a REAL change: this is fed from the response hook, so the same
            // roster arrives again on every screen refresh, and an unconditional insert would have
            // the saver rewriting an identical file every few seconds for the whole session.
            if g.sparks.get(&chara).map(|v| v.as_slice() == ids).unwrap_or(false) {
                false
            } else {
                g.sparks.insert(chara, ids.to_vec());
                true
            }
        }
        Err(_) => false,
    };
    if changed {
        save_throttled();
    }
}

// ── end-of-career spark OFFER ───────────────────────────────────────────────────────────────────
// The sparks the run is offering you, sent in full before you choose: `factor_select` returns the
// pool (9 in the observed career) and `factor_lottery` the re-rolled one (10), each under
// `factor_select_info_array[].factor_info_array[].factor_id`, with `lottery_remain_num` counting the
// re-rolls left. Deliberately NOT routed through `note_sparks`: that records what a trainee OWNS,
// and these are candidates the player may well not take — writing them into the roster would poison
// the parent-suggestion pool with sparks nobody ever picked.
//
// Held in memory only. The offer is meaningless once the screen closes, and it is cleared when the
// career ends.
static OFFER: Lazy<Mutex<Option<SparkOffer>>> = Lazy::new(|| Mutex::new(None));

/// The spark pool on the end-of-career selection screen.
#[derive(Clone, Serialize, Default)]
pub struct SparkOffer {
    pub sparks: Vec<crate::career::Spark>,
    /// Re-rolls still available (`lottery_remain_num`).
    pub rerolls_left: i64,
    /// How many re-rolls have been spent (`lottery_count`).
    pub rerolls_used: i64,
}

/// Record the offered pool. `ids` is taken verbatim from the wire; naming happens here.
pub fn note_spark_offer(ids: &[i64], rerolls_left: i64, rerolls_used: i64) {
    if ids.is_empty() {
        return;
    }
    let mut sparks: Vec<crate::career::Spark> = ids.iter().map(|id| decode_factor(*id)).collect();
    // Best first, by star rating then kind, so the panel reads top-down without the user sorting it.
    sparks.sort_by(|a, b| b.stars.cmp(&a.stars).then_with(|| a.kind.cmp(&b.kind)));
    if let Ok(mut g) = OFFER.lock() {
        *g = Some(SparkOffer { sparks, rerolls_left, rerolls_used });
    }
}

/// The live spark offer, or None when the selection screen isn't up.
pub fn spark_offer() -> Option<SparkOffer> {
    OFFER.lock().ok().and_then(|g| g.clone())
}

/// Drop the offer (career finished).
pub fn clear_spark_offer() {
    if let Ok(mut g) = OFFER.lock() {
        *g = None;
    }
}

/// Called when a career completes — snapshot the finished trainee as a future legacy candidate.
pub fn note_career_finished(s: &crate::career::CareerSummary) {
    let chara = chara_of_card(s.card_id);
    if chara > 0 {
        note_name(chara, &s.trainee);
        let ids: Vec<i64> = s
            .stat_sparks
            .iter()
            .chain(s.skill_sparks.iter())
            .chain(s.race_sparks.iter())
            .chain(s.character_spark.iter())
            .map(|k| k.id)
            .filter(|i| *i != 0)
            .collect();
        if !ids.is_empty() {
            note_sparks(chara, &ids);
        }
    }
    save_now();
}

/// Character id from a card id. The game's card ids are `<chara><outfit>` (e.g. 100101 → chara
/// 1001), so the character is the value without its two trailing outfit digits. Small ids are
/// already character ids.
pub fn chara_of_card(card_id: i64) -> i64 {
    if card_id > 9999 {
        card_id / 100
    } else {
        card_id
    }
}

// ── the optional community prior ────────────────────────────────────────────────────────────────

/// `<dll_dir>/data/legacy_affinity.json`: `{"1001-1002": 63, …}` (character-id pair → base relation
/// points, order-insensitive). This is the slot for a community compatibility export — the same
/// class of data the Legacy Loops reference tool uses. Purely optional: absent it, the planner runs
/// on live captures alone and simply reports lower coverage.
static PRIOR: Lazy<HashMap<(i64, i64), i64>> = Lazy::new(|| {
    let path = crate::paths::dll_dir_ref().join("data").join("legacy_affinity.json");
    let mut out = HashMap::new();
    let Ok(text) = std::fs::read_to_string(&path) else { return out };
    let Ok(map) = serde_json::from_str::<BTreeMap<String, i64>>(&text) else {
        log("[legacy] data/legacy_affinity.json is present but not a {\"a-b\": points} map — ignored");
        return out;
    };
    for (k, v) in map {
        let mut it = k.split(['-', '_', ',']);
        if let (Some(a), Some(b)) = (it.next(), it.next()) {
            if let (Ok(a), Ok(b)) = (a.trim().parse::<i64>(), b.trim().parse::<i64>()) {
                out.insert((a.min(b), a.max(b)), v);
            }
        }
    }
    if !out.is_empty() {
        log(&format!("[legacy] loaded {} community affinity priors", out.len()));
    }
    out
});

/// Where a number came from — surfaced so the UI never presents an estimate as a measurement.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The game's own CalcRelationPoint, captured live.
    Measured,
    /// The optional community prior file.
    Estimated,
    /// No data at all.
    Unknown,
}

/// A single trainee↔parent affinity lookup result.
#[derive(Clone, Serialize)]
pub struct Affinity {
    pub trainee: i64,
    pub parent: i64,
    pub points: i64,
    pub rank: String,
    pub source: Source,
    pub samples: u32,
}

/// Best available affinity for a (trainee, parent) character pair.
pub fn lookup(trainee: i64, parent: i64) -> Affinity {
    ensure_loaded();
    let mk = |points: i64, source: Source, samples: u32| Affinity {
        trainee,
        parent,
        points,
        rank: affinity_rank(points).to_string(),
        source,
        samples,
    };
    if let Ok(g) = STORE.lock() {
        if let Some(o) = g
            .observations
            .iter()
            .find(|o| o.trainee == trainee && o.parent == parent)
        {
            // The FLOOR is the better planning number: a parent's chain bonus travels with the
            // parent, so the minimum ever seen is the closest thing we have to the pair's own base
            // relation — and a loop plan must not credit a bonus the next rotation won't have.
            return mk(o.floor_points, Source::Measured, o.samples);
        }
        // Relation is symmetric in the game's own model, so an observation the other way round is
        // still evidence — flagged with its own sample count.
        if let Some(o) = g
            .observations
            .iter()
            .find(|o| o.trainee == parent && o.parent == trainee)
        {
            return mk(o.floor_points, Source::Measured, o.samples);
        }
    }
    if let Some(p) = PRIOR.get(&(trainee.min(parent), trainee.max(parent))) {
        return mk(*p, Source::Estimated, 0);
    }
    mk(-1, Source::Unknown, 0)
}

/// How many pairings the affinity matrix holds (memory report / coverage display).
pub fn observation_count() -> usize {
    ensure_loaded();
    STORE.lock().map(|g| g.observations.len()).unwrap_or(0)
}

/// Display name for a character id, if we've learned one.
pub fn name_of(chara: i64) -> String {
    ensure_loaded();
    if let Ok(g) = STORE.lock() {
        if let Some(n) = g.names.get(&chara) {
            return n.clone();
        }
    }
    // Fall back to the bundled card list (card id = chara*100 + outfit; try the common outfits).
    for outfit in 1..=9 {
        let n = crate::career::trainee_name(chara * 100 + outfit);
        if !n.is_empty() {
            return n;
        }
    }
    String::new()
}

// ── parent recommendations ──────────────────────────────────────────────────────────────────────

/// One ranked parent suggestion for a given trainee.
#[derive(Clone, Serialize)]
pub struct ParentSuggestion {
    pub chara: i64,
    pub name: String,
    pub affinity: i64,
    pub rank: String,
    pub source: Source,
    /// Sparks this legacy is known to carry (raw ids decoded).
    pub sparks: Vec<crate::career::Spark>,
    /// 0..1 — the blended ranking score (affinity band, spark value, evidence).
    pub score: f64,
    /// Why it ranks where it does, in plain words (shown in the UI as a tooltip).
    pub reason: String,
}

/// Rank every character we have ANY data for as a legacy parent for `trainee`.
///
/// Score = affinity band (dominant) + spark value + a small confidence term. Affinity dominates on
/// purpose: a 3★ speed spark on a ✕-affinity parent is worth far less than a plain parent at ◎,
/// because affinity gates how much of ANY spark actually transfers.
pub fn recommend_parents(trainee: i64, limit: usize) -> Vec<ParentSuggestion> {
    ensure_loaded();
    let mut candidates: HashSet<i64> = HashSet::new();
    if let Ok(g) = STORE.lock() {
        for o in &g.observations {
            if o.trainee == trainee {
                candidates.insert(o.parent);
            }
            if o.parent == trainee {
                candidates.insert(o.trainee);
            }
        }
        for c in g.sparks.keys() {
            candidates.insert(*c);
        }
        for c in g.names.keys() {
            candidates.insert(*c);
        }
    }
    for (a, b) in PRIOR.keys() {
        if *a == trainee {
            candidates.insert(*b);
        } else if *b == trainee {
            candidates.insert(*a);
        }
    }
    candidates.remove(&trainee); // a character can't be its own legacy

    let mut out: Vec<ParentSuggestion> = candidates
        .into_iter()
        .map(|c| {
            let a = lookup(trainee, c);
            let sparks = sparks_of(c);
            let spark_value = spark_value(&sparks);
            let confidence = match a.source {
                Source::Measured => 1.0,
                Source::Estimated => 0.8,
                Source::Unknown => 0.35,
            };
            // 70% affinity / 30% sparks, then scaled by how much we trust the affinity number.
            let score = (affinity_score(a.points) * 0.7 + spark_value * 0.3) * confidence;
            let reason = describe(&a, &sparks);
            ParentSuggestion {
                chara: c,
                name: name_of(c),
                affinity: a.points,
                rank: a.rank.clone(),
                source: a.source,
                sparks,
                score: (score * 1000.0).round() / 1000.0,
                reason,
            }
        })
        .collect();
    out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(limit.clamp(1, 200));
    out
}

fn describe(a: &Affinity, sparks: &[crate::career::Spark]) -> String {
    let aff = match a.source {
        Source::Measured => format!("affinity {} {} (measured)", a.points, a.rank),
        Source::Estimated => format!("affinity ~{} {} (community estimate)", a.points, a.rank),
        Source::Unknown => "affinity unknown — evaluate this pairing on Legacy Select".to_string(),
    };
    if sparks.is_empty() {
        return aff;
    }
    let top: Vec<String> = sparks
        .iter()
        .take(3)
        .map(|s| format!("{} {}★", if s.name.is_empty() { s.kind.clone() } else { s.name.clone() }, s.stars))
        .collect();
    format!("{aff}; carries {}", top.join(", "))
}

/// Decoded sparks known for a character.
pub fn sparks_of(chara: i64) -> Vec<crate::career::Spark> {
    ensure_loaded();
    STORE
        .lock()
        .ok()
        .and_then(|g| g.sparks.get(&chara).cloned())
        .unwrap_or_default()
        .into_iter()
        .map(decode_factor)
        .collect()
}

/// 0..1 value of a spark set. Stars dominate (a 3★ is worth much more than three 1★s, because the
/// inheritance roll is per-spark and higher stars raise both the odds and the magnitude), and
/// character/unique sparks count double since they carry the whole unique-skill inheritance.
fn spark_value(sparks: &[crate::career::Spark]) -> f64 {
    if sparks.is_empty() {
        return 0.0;
    }
    let mut total = 0.0;
    for s in sparks {
        let weight = match s.kind.as_str() {
            "character" | "unique" => 2.0,
            "stat" | "aptitude" => 1.0,
            "skill" => 0.8,
            "race" => 0.6,
            _ => 0.4,
        };
        // 1★ = 1, 2★ = 2.5, 3★ = 5 — superlinear, matching how the game rewards concentration.
        let stars = match s.stars {
            3 => 5.0,
            2 => 2.5,
            1 => 1.0,
            _ => 0.5,
        };
        total += weight * stars;
    }
    // 30 points ≈ a very strong parent; saturate there so one monster doesn't flatten the ranking.
    (total / 30.0_f64).min(1.0)
}

// ── legacy loop planner ─────────────────────────────────────────────────────────────────────────

/// One career inside a planned loop.
#[derive(Clone, Serialize)]
pub struct LoopSession {
    pub index: usize,
    pub trainee: i64,
    pub trainee_name: String,
    pub parents: [i64; 2],
    pub parent_names: [String; 2],
    /// Best available affinity for each parent, and the pair estimate.
    pub parent_points: [i64; 2],
    pub parent_ranks: [String; 2],
    pub estimated_total: i64,
    pub estimated_rank: String,
    pub source: Source,
}

/// A complete rotation over the chosen characters.
#[derive(Clone, Serialize)]
pub struct LoopPlan {
    pub characters: Vec<i64>,
    pub character_names: Vec<String>,
    pub sessions: Vec<LoopSession>,
    /// Score of the WEAKEST session — a loop is only as good as its worst career.
    pub weakest: f64,
    /// Mean session score (tie-breaker).
    pub average: f64,
    /// 0..1 — how much of the plan rests on measured data rather than estimates.
    pub coverage: f64,
    /// Pairings with no data at all: open Legacy Select with these to complete the picture.
    pub missing: Vec<[i64; 2]>,
    /// Plain-language verdict.
    pub verdict: String,
}

/// Plan a legacy loop over `chars` (3–6 characters).
///
/// The rotation model follows the reference tool: in each session ONE character is the trainee and
/// the others are the legacy pool; over a full cycle every character trains once, so the loop is
/// self-sustaining. Rather than assert a single fixed rotation, we enumerate every assignment of a
/// parent PAIR to each session from the non-trainee pool and keep the best, then score the plan on
/// its weakest session.
///
/// Complexity is bounded by construction: with n ≤ 6 there are at most 6 sessions and C(5,2) = 10
/// pair choices each, so the whole search is trivial — no heuristics or cutoffs needed.
pub fn plan_loop(chars: &[i64]) -> Option<LoopPlan> {
    let uniq: Vec<i64> = {
        let mut seen = HashSet::new();
        chars.iter().copied().filter(|c| *c > 0 && seen.insert(*c)).collect()
    };
    if uniq.len() < 3 || uniq.len() > 6 {
        return None;
    }
    let mut sessions: Vec<LoopSession> = Vec::new();
    let mut missing: Vec<[i64; 2]> = Vec::new();
    let mut measured = 0usize;
    let mut total_pairings = 0usize;

    for (i, &trainee) in uniq.iter().enumerate() {
        let pool: Vec<i64> = uniq.iter().copied().filter(|c| *c != trainee).collect();
        // Best parent pair for this trainee out of the pool.
        let mut best: Option<(f64, [i64; 2], [Affinity; 2])> = None;
        for a in 0..pool.len() {
            for b in (a + 1)..pool.len() {
                let (pa, pb) = (pool[a], pool[b]);
                let (fa, fb) = (lookup(trainee, pa), lookup(trainee, pb));
                // A session's quality is driven by the PAIR total the game shows, which is the sum
                // of the two chains. Unknown branches contribute 0 rather than being invented.
                let total = fa.points.max(0) + fb.points.max(0);
                let s = affinity_score(total);
                if best.as_ref().map(|(bs, _, _)| s > *bs).unwrap_or(true) {
                    best = Some((s, [pa, pb], [fa, fb]));
                }
            }
        }
        let (_, parents, affs) = best?;
        for f in &affs {
            total_pairings += 1;
            match f.source {
                Source::Measured => measured += 1,
                Source::Estimated => {}
                Source::Unknown => missing.push([trainee, f.parent]),
            }
        }
        // Prefer a directly-observed pair total when the player has actually evaluated this exact
        // trio — it includes whatever the sum-of-branches model can't see.
        let observed = pair_total(trainee, parents[0], parents[1]);
        let estimated_total = observed.unwrap_or_else(|| affs[0].points.max(0) + affs[1].points.max(0));
        let source = if observed.is_some() {
            Source::Measured
        } else if affs.iter().any(|f| f.source == Source::Unknown) {
            Source::Unknown
        } else if affs.iter().all(|f| f.source == Source::Measured) {
            Source::Measured
        } else {
            Source::Estimated
        };
        sessions.push(LoopSession {
            index: i + 1,
            trainee,
            trainee_name: name_of(trainee),
            parents,
            parent_names: [name_of(parents[0]), name_of(parents[1])],
            parent_points: [affs[0].points, affs[1].points],
            parent_ranks: [affs[0].rank.clone(), affs[1].rank.clone()],
            estimated_total,
            estimated_rank: affinity_rank(estimated_total).to_string(),
            source,
        });
    }

    let scores: Vec<f64> = sessions.iter().map(|s| affinity_score(s.estimated_total)).collect();
    let weakest = scores.iter().cloned().fold(f64::INFINITY, f64::min);
    let average = if scores.is_empty() { 0.0 } else { scores.iter().sum::<f64>() / scores.len() as f64 };
    let coverage = if total_pairings == 0 { 0.0 } else { measured as f64 / total_pairings as f64 };
    missing.sort();
    missing.dedup();

    let verdict = if !missing.is_empty() {
        format!(
            "Incomplete: {} of {} pairings have never been evaluated. Open Legacy Select with them once each and this plan becomes exact.",
            missing.len(),
            total_pairings
        )
    } else if weakest >= 0.75 {
        "Viable loop — every session holds ◎ affinity, so it sustains indefinitely.".to_string()
    } else if weakest >= 0.45 {
        format!(
            "Workable, but the weakest session only reaches {}. Swapping one character usually fixes it.",
            affinity_rank(sessions
                .iter()
                .min_by_key(|s| s.estimated_total)
                .map(|s| s.estimated_total)
                .unwrap_or(0))
        )
    } else {
        "Not recommended — at least one session drops to poor affinity, which breaks the rotation."
            .to_string()
    };

    Some(LoopPlan {
        character_names: uniq.iter().map(|c| name_of(*c)).collect(),
        characters: uniq,
        sessions,
        weakest: (weakest * 1000.0).round() / 1000.0,
        average: (average * 1000.0).round() / 1000.0,
        coverage: (coverage * 1000.0).round() / 1000.0,
        missing,
        verdict,
    })
}

/// A directly-observed pair total for this exact trio, in either parent order.
fn pair_total(trainee: i64, p1: i64, p2: i64) -> Option<i64> {
    ensure_loaded();
    let g = STORE.lock().ok()?;
    g.pairs
        .iter()
        .find(|o| {
            o.trainee == trainee
                && ((o.p1 == p1 && o.p2 == p2) || (o.p1 == p2 && o.p2 == p1))
        })
        .map(|o| o.total)
}

/// Search for the best loop among a candidate pool, returning the top `limit` plans.
/// Combinations are enumerated over `size`-subsets of `pool` — bounded by capping the pool.
pub fn suggest_loops(pool: &[i64], size: usize, limit: usize) -> Vec<LoopPlan> {
    let size = size.clamp(3, 6);
    // C(24,4) = 10626 plans, each a trivial search — comfortably fast, and the cap keeps a huge
    // roster from turning this into a combinatorial explosion.
    let pool: Vec<i64> = pool.iter().copied().take(24).collect();
    if pool.len() < size {
        return Vec::new();
    }
    let mut plans: Vec<LoopPlan> = Vec::new();
    let mut idx: Vec<usize> = (0..size).collect();
    loop {
        let combo: Vec<i64> = idx.iter().map(|i| pool[*i]).collect();
        if let Some(p) = plan_loop(&combo) {
            plans.push(p);
        }
        // Next combination in lexicographic order.
        let mut i = size;
        loop {
            if i == 0 {
                plans.sort_by(|a, b| {
                    b.weakest
                        .partial_cmp(&a.weakest)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(b.average.partial_cmp(&a.average).unwrap_or(std::cmp::Ordering::Equal))
                        .then(b.coverage.partial_cmp(&a.coverage).unwrap_or(std::cmp::Ordering::Equal))
                });
                plans.truncate(limit.clamp(1, 25));
                return plans;
            }
            i -= 1;
            if idx[i] < pool.len() - (size - i) {
                idx[i] += 1;
                for j in i + 1..size {
                    idx[j] = idx[j - 1] + 1;
                }
                break;
            }
        }
    }
}

// ── spark (factor) decoding ─────────────────────────────────────────────────────────────────────

/// Optional decoding table: `<dll_dir>/data/factor_data.json`, `{"<factor_id>": {"kind": "...",
/// "name": "...", "stars": N}}`. Exported from master.mdb by the dashboard app, or hand-maintained.
/// Present ⇒ exact names; absent ⇒ the heuristic below, which still gets kind + stars right for the
/// common families and always preserves the raw id.
static FACTOR_TABLE: Lazy<HashMap<i64, (String, String, i64)>> = Lazy::new(|| {
    let path = crate::paths::dll_dir_ref().join("data").join("factor_data.json");
    let mut out = HashMap::new();
    let Ok(text) = std::fs::read_to_string(&path) else { return out };
    #[derive(Deserialize)]
    struct Row {
        #[serde(default)]
        kind: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        stars: i64,
    }
    if let Ok(map) = serde_json::from_str::<BTreeMap<String, Row>>(&text) {
        for (k, v) in map {
            if let Ok(id) = k.trim().parse::<i64>() {
                out.insert(id, (v.kind, v.name, v.stars));
            }
        }
    }
    if !out.is_empty() {
        log(&format!("[legacy] loaded {} spark definitions from data/factor_data.json", out.len()));
    }
    out
});

/// Decode a raw factor (spark) id into kind / name / stars.
///
/// The authoritative mapping lives in the game's master.mdb `succession_factor` table, which this
/// DLL does not read. Two fallbacks, in order:
///
/// 1. `data/factor_data.json` if the user (or the companion dashboard) has exported it — exact.
/// 2. A structural heuristic. Factor ids in this game are `<group><rarity>`: the last digit is the
///    star level (1–3) and the leading digits identify the factor. The low groups are the five stat
///    ("blue") sparks and the aptitude ("pink") sparks, which are stable and worth naming; anything
///    higher is a skill/race/scenario spark whose NAME we can't know without master data, so we
///    report the kind and the id and leave the name empty rather than invent one.
///
/// The raw id is always preserved on the `Spark`, so a consumer with better data can re-decode.
pub fn decode_factor(id: i64) -> crate::career::Spark {
    if id <= 0 {
        return crate::career::Spark { id, kind: "unknown".into(), name: String::new(), stars: 0 };
    }
    // A user-supplied data/factor_data.json wins (it can be newer than the shipped build), then the
    // table bundled into the DLL, then the id heuristic below. The heuristic gets kind + stars right
    // for the common families but cannot name a character or skill spark, which is most of them.
    if let Some((kind, name, stars)) = FACTOR_TABLE.get(&id) {
        return crate::career::Spark {
            id,
            kind: if kind.is_empty() { "unknown".into() } else { kind.clone() },
            name: name.clone(),
            stars: *stars,
        };
    }
    if let Some((kind, name, stars)) = crate::names::factor(id) {
        return crate::career::Spark {
            id,
            kind: if kind.is_empty() { "unknown".into() } else { kind },
            name,
            stars,
        };
    }
    let stars = (id % 10).clamp(0, 3);
    let group = id / 10;
    let (kind, name) = match group {
        1 => ("stat", "Speed"),
        2 => ("stat", "Stamina"),
        3 => ("stat", "Power"),
        4 => ("stat", "Guts"),
        5 => ("stat", "Wit"),
        // 6xx–9xx are the surface/distance/style aptitude ("pink") sparks.
        6 => ("aptitude", "Turf"),
        7 => ("aptitude", "Dirt"),
        8 => ("aptitude", "Sprint"),
        9 => ("aptitude", "Mile"),
        10 => ("aptitude", "Medium"),
        11 => ("aptitude", "Long"),
        12 => ("aptitude", "Front Runner"),
        13 => ("aptitude", "Pace Chaser"),
        14 => ("aptitude", "Late Surger"),
        15 => ("aptitude", "End Closer"),
        _ => {
            // Character/unique sparks live in the character-id range; skill sparks above it.
            let kind = if (1000..=2999).contains(&group) {
                "character"
            } else if group >= 20000 {
                "skill"
            } else if group >= 3000 {
                "race"
            } else {
                "unknown"
            };
            return crate::career::Spark { id, kind: kind.into(), name: String::new(), stars };
        }
    };
    crate::career::Spark { id, kind: kind.into(), name: name.into(), stars }
}

// ── display-name helpers used by the career report ──────────────────────────────────────────────

/// Scenario name for a scenario id (the report reads better than a bare number).
pub fn scenario_name(id: i64) -> String {
    match id {
        1 => "URA Finale",
        2 => "Aoharu Cup",
        3 => "Make a New Track",
        4 => "Grand Live",
        5 => "Grand Masters",
        6 => "Project L'Arc",
        7 => "U.A.F. Ready Go",
        8 => "Great Food Festival",
        9 => "Run! Mecha Umamusume",
        10 => "Legend Race",
        _ => return if id > 0 { format!("Scenario {id}") } else { String::new() },
    }
    .to_string()
}

/// Difficulty label. 0/unknown yields "".
pub fn difficulty_name(id: i64) -> String {
    match id {
        1 => "Easy",
        2 => "Normal",
        3 => "Hard",
        4 => "Very Hard",
        5 => "Expert",
        _ => return String::new(),
    }
    .to_string()
}

/// Running-style label for the game's style code (1..4). 0/unknown yields "".
pub fn running_style_name(id: i64) -> String {
    match id {
        1 => "Front Runner",
        2 => "Pace Chaser",
        3 => "Late Surger",
        4 => "End Closer",
        _ => return String::new(),
    }
    .to_string()
}

/// Distance category for the game's distance code. 0/unknown yields "".
pub fn distance_name(id: i64) -> String {
    match id {
        1 => "Sprint",
        2 => "Mile",
        3 => "Medium",
        4 => "Long",
        _ => return String::new(),
    }
    .to_string()
}

// ── web-UI payload ──────────────────────────────────────────────────────────────────────────────

/// Everything the Legacy page renders: the live Legacy Select readout, the observed matrix, the
/// spark inventory, and the coverage report.
pub fn state_json() -> serde_json::Value {
    ensure_loaded();
    let (aff_total, aff_p1, aff_p2) = crate::affinity::values()
        .map(|(t, a, b)| (t as i64, a as i64, b as i64))
        .unwrap_or((-1, -1, -1));
    let g = STORE.lock();
    let (obs, pairs, names, _sparks) = match &g {
        Ok(s) => (s.observations.clone(), s.pairs.clone(), s.names.clone(), s.sparks.clone()),
        Err(_) => (Vec::new(), Vec::new(), BTreeMap::new(), BTreeMap::new()),
    };
    drop(g);

    // Which characters we know anything about (the planner's candidate pool).
    let mut pool: Vec<i64> = names.keys().copied().collect();
    for o in &obs {
        if !pool.contains(&o.trainee) {
            pool.push(o.trainee);
        }
        if !pool.contains(&o.parent) {
            pool.push(o.parent);
        }
    }
    pool.sort_unstable();
    pool.dedup();

    let roster: Vec<serde_json::Value> = pool
        .iter()
        .map(|c| {
            serde_json::json!({
                "chara": c,
                "name": name_of(*c),
                "sparks": sparks_of(*c),
                "measured_partners": obs.iter().filter(|o| o.trainee == *c || o.parent == *c).count(),
            })
        })
        .collect();

    // Coverage: how many of the possible pairings in the known pool have been measured.
    let n = pool.len();
    let possible = if n >= 2 { n * (n - 1) / 2 } else { 0 };
    let mut seen: HashSet<(i64, i64)> = HashSet::new();
    for o in &obs {
        seen.insert((o.trainee.min(o.parent), o.trainee.max(o.parent)));
    }

    let _ = (&pairs, &_sparks); // (kept for future per-pair drill-down in the UI)
    serde_json::json!({
        "live": {
            "on_screen": crate::affinity::active(),
            "total": if aff_total < 0 { serde_json::Value::Null } else { serde_json::json!(aff_total) },
            "rank": affinity_rank(aff_total),
            "p1": if aff_p1 < 0 { serde_json::Value::Null } else { serde_json::json!(aff_p1) },
            "p1_rank": affinity_rank(aff_p1),
            "p2": if aff_p2 < 0 { serde_json::Value::Null } else { serde_json::json!(aff_p2) },
            "p2_rank": affinity_rank(aff_p2),
        },
        "thresholds": {
            "double_circle": AFFINITY_DOUBLE_CIRCLE,
            "circle": AFFINITY_CIRCLE,
            "triangle": AFFINITY_TRIANGLE,
        },
        "roster": roster,
        "observations": obs.len(),
        "pair_observations": pairs.len(),
        "coverage": {
            "characters": n,
            "measured_pairs": seen.len(),
            "possible_pairs": possible,
            "fraction": if possible == 0 { 0.0 } else { (seen.len() as f64 / possible as f64 * 1000.0).round() / 1000.0 },
            "has_prior": !PRIOR.is_empty(),
        },
        "capture_enabled": crate::settings::legacy_capture(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_match_the_in_game_bands() {
        assert_eq!(affinity_rank(-1), ""); // unknown, not ✕
        assert_eq!(affinity_rank(0), "✕");
        assert_eq!(affinity_rank(1), "△");
        assert_eq!(affinity_rank(50), "△");
        assert_eq!(affinity_rank(51), "○");
        assert_eq!(affinity_rank(150), "○");
        assert_eq!(affinity_rank(151), "◎");
        assert_eq!(affinity_rank(400), "◎");
    }

    #[test]
    fn banding_dominates_raw_points() {
        // THE load-bearing property of the loop scorer: the WORST ◎ must still beat the BEST ○.
        // A linear score would rank a 150 (○) above a 155 (◎) whenever the spread is wide, and the
        // planner would then happily recommend a rotation that drops out of ◎.
        assert!(affinity_score(AFFINITY_DOUBLE_CIRCLE) > affinity_score(AFFINITY_DOUBLE_CIRCLE - 1));
        assert!(affinity_score(AFFINITY_CIRCLE) > affinity_score(AFFINITY_CIRCLE - 1));
        assert!(affinity_score(151) > affinity_score(150));
        // Monotonic within a band.
        assert!(affinity_score(300) > affinity_score(200));
        // Unknown scores zero, never "probably fine".
        assert_eq!(affinity_score(-1), 0.0);
        assert_eq!(affinity_score(0), 0.0);
    }

    #[test]
    fn card_ids_reduce_to_characters() {
        // Outfit variants of one character must collapse to the same id — affinity is per
        // CHARACTER, so treating 100101 and 100102 as different legacies would split the matrix.
        assert_eq!(chara_of_card(100101), 1001);
        assert_eq!(chara_of_card(100102), 1001);
        assert_eq!(chara_of_card(1001), 1001); // already a character id
        assert_eq!(chara_of_card(0), 0);
    }

    #[test]
    fn spark_decoding_is_structural_and_preserves_the_id() {
        let s = decode_factor(13); // group 1 (Speed), 3★
        assert_eq!(s.kind, "stat");
        assert_eq!(s.name, "Speed");
        assert_eq!(s.stars, 3);
        assert_eq!(s.id, 13);
        // An id we can't name still carries its kind, its stars and — critically — the raw id, so a
        // consumer with better data can re-decode it.
        let u = decode_factor(200311);
        assert_eq!(u.id, 200311);
        assert!(u.stars <= 3);
        assert!(!u.kind.is_empty());
        // Garbage in, defined value out.
        assert_eq!(decode_factor(0).kind, "unknown");
        assert_eq!(decode_factor(-5).stars, 0);
    }

    #[test]
    fn a_loop_needs_three_to_six_distinct_characters() {
        assert!(plan_loop(&[]).is_none());
        assert!(plan_loop(&[1001, 1002]).is_none());
        assert!(plan_loop(&[1001, 1001, 1001]).is_none()); // dedup leaves one
        assert!(plan_loop(&[1001, 1002, 1003, 1004, 1005, 1006, 1007]).is_none());
        let p = plan_loop(&[1001, 1002, 1003, 1004]).expect("4 characters is a valid loop");
        // One session per character: every character trains exactly once per cycle.
        assert_eq!(p.sessions.len(), 4);
        for s in &p.sessions {
            assert_ne!(s.parents[0], s.trainee, "a character can't be its own legacy");
            assert_ne!(s.parents[1], s.trainee);
            assert_ne!(s.parents[0], s.parents[1]);
        }
    }

    #[test]
    fn an_unmeasured_loop_reports_itself_as_unmeasured() {
        // With no observations and no prior, the planner must NOT present a confident answer.
        let p = plan_loop(&[900001, 900002, 900003]).unwrap();
        assert_eq!(p.coverage, 0.0);
        assert!(!p.missing.is_empty());
        assert!(p.verdict.contains("Incomplete"), "verdict was: {}", p.verdict);
        assert_eq!(p.weakest, 0.0);
    }

    #[test]
    fn suggest_loops_is_bounded_and_ordered() {
        let pool: Vec<i64> = (900100..900110).collect();
        let plans = suggest_loops(&pool, 4, 3);
        assert!(plans.len() <= 3);
        // Best-first on the weakest session.
        for w in plans.windows(2) {
            assert!(w[0].weakest >= w[1].weakest);
        }
        // Too small a pool yields nothing rather than a partial loop.
        assert!(suggest_loops(&[900200, 900201], 4, 3).is_empty());
    }

    #[test]
    fn display_names_degrade_to_empty_not_to_lies() {
        assert_eq!(scenario_name(0), "");
        assert_eq!(scenario_name(1), "URA Finale");
        assert_eq!(scenario_name(999), "Scenario 999"); // unknown but real → say so
        assert_eq!(difficulty_name(0), "");
        assert_eq!(running_style_name(0), "");
        assert_eq!(running_style_name(1), "Front Runner");
        assert_eq!(distance_name(4), "Long");
    }
}
