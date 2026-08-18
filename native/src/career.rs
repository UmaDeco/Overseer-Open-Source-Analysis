//! Career-run tracking — per-turn stat snapshots, inferred PLAYER actions, and completed-run history.
//!
//! Fed from the network response hook (`chara_info` each turn + `single_mode_finish_common` at career
//! end, plus the MANT extras: inventory / shop catalog / training options / race rewards) and served
//! to the web UI's Dashboard (run history + stats) and the Logs "Player Actions" tab.
//! Pure data + file I/O — NEVER touches IL2CPP, so it's safe to call from the response-parse path.
//!
//! Model mirrors Icarus (uma_api/client.py + career_bot/runner.py): the trainee's live state comes from
//! `chara_info` (speed/stamina/power/guts/wiz, skill_point, fans, motivation=mood, vital=energy, turn);
//! a run record (grade from rank_score) is captured from the finish payload's `trained_chara`.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

/// Live trainee state, refreshed every turn from `chara_info`.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct CareerState {
    pub turn: i64,
    pub speed: i64,
    pub stamina: i64,
    pub power: i64,
    pub guts: i64,
    pub wit: i64,
    pub skill_point: i64,
    pub fans: i64,
    pub mood: i64, // 1..5
    pub energy: i64,
    pub max_energy: i64,
    pub card_id: i64,
    pub active: bool, // a career is in progress
}

/// One turn's inferred player action + stat deltas.
#[derive(Clone, Serialize)]
pub struct TurnAction {
    pub turn: i64,
    pub time: String,   // HH:MM:SS
    pub action: String, // "Raced (+N fans)" / "Trained Speed (+15)" / "Learned skills (-N SP)" / "Rested" / "Turn N"
    pub kind: String,   // RACE / TRAIN / SKILL / REST / TURN — for the UI badge
    pub deltas: Vec<(String, i64)>,
    pub speed: i64,
    pub stamina: i64,
    pub power: i64,
    pub guts: i64,
    pub wit: i64,
    pub skill_point: i64,
    pub fans: i64,
}

/// One training tile as the game presents it (its own preview numbers — no advice on top).
/// `gains` serializes as `[["SPD",14],["Energy",-21]]`, the shape the dashboard renders directly.
///
/// `partners` and `hint_partners` come from the tile's `training_partner_array` /
/// `tips_event_partner_array`: the deck POSITIONS (1-6) standing on that training, and the subset of
/// them that will proc a skill hint. The hint roll is already decided server-side and shipped with
/// the tile — picking this training WILL yield the hint — which is why it is worth surfacing next to
/// the raw numbers rather than left as a maybe.
#[derive(Clone, Serialize)]
pub struct TrainingOpt {
    pub name: String,              // Speed / Stamina / Power / Guts / Wit
    pub gains: Vec<(String, i64)>, // summed per label: SPD/STA/PWR/GUT/WIT/Energy
    pub fail: i64,                 // failure_rate %
    pub partners: Vec<String>,     // support-card names standing on this training
    pub hint_partners: Vec<String>, // those that will give a skill hint (pre-rolled by the server)
    /// What that hint could be — the offering cards' pools, pooled and de-duplicated. A CANDIDATE
    /// SET, never a prediction: see `names::support_hint_pool`.
    pub hints: Vec<HintOption>,
}

/// One candidate skill behind a training's red [!].
#[derive(Clone, Serialize)]
pub struct HintOption {
    pub name: String,
    /// True when the trainee already holds a hint for this skill — a repeat is worth less than a
    /// new one, and that's decidable from her own `skill_tips_array` with no guessing involved.
    pub known: bool,
}

/// One race this career has on the books, from the career-start payload.
///
/// Two sources, distinguished by `reserved`: `reserved_race_array` is what the PLAYER has entered
/// on a deck, and `race_random_program_array` is the set of races this particular run rolled — the
/// schedule is not fixed across careers, so it is only knowable from the start response.
#[derive(Clone, Serialize)]
pub struct PlannedRace {
    pub year: i64,
    pub program_id: i64,
    /// "" when the program id isn't in the bundled race table.
    pub name: String,
    pub distance: i64,
    pub ground: &'static str,
    pub reserved: bool,
}

/// One race the trainee ran this career (for the Dashboard RACES panel).
#[derive(Clone, Serialize)]
struct RaceRow {
    turn: i64,
    name: String, // race::race_header() at the time — "" if the header wasn't captured
    place: i64,   // result_rank; 0 = unknown
    fans: i64,
}

/// A completed career run (persisted to `career-runs.json`, newest last).
#[derive(Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub ts: String, // local HH:MM:SS DD/MM
    pub card_id: i64,
    pub grade: String,
    pub rank_score: i64,
    pub fans: i64,
    pub speed: i64,
    pub stamina: i64,
    pub power: i64,
    pub guts: i64,
    pub wit: i64,
    pub skill_point: i64,
    pub wins: i64,
    pub race_count: i64,
    pub scenario_id: i64,
    pub final_turn: i64,
    // v2 fields — #[serde(default)] so a legacy career-runs.json (without them) still loads.
    #[serde(default)]
    pub epoch_ms: u64, // unix ms at finish; 0 = legacy record
    #[serde(default)]
    pub duration_s: u64, // run wall-clock seconds; 0 = unknown/legacy
    #[serde(default)]
    pub day: String, // LOCAL "YYYY-MM-DD" at record time — daily_fans / hall_of_fame key; "" = legacy
    // v3 fields — per-run action tallies from this run's ACTIONS timeline (TurnAction.kind), captured
    // at finish time. Feed the AI Brain page's observed-behavior stats. #[serde(default)] → 0 on
    // legacy records, so an old career-runs.json still loads.
    #[serde(default)]
    pub act_train: u32, // kind == "TRAIN"
    #[serde(default)]
    pub act_race: u32, // kind == "RACE"
    #[serde(default)]
    pub act_rest: u32, // kind == "REST"
    #[serde(default)]
    pub act_skill: u32, // kind == "SKILL"
    #[serde(default)]
    pub act_other: u32, // any other kind ("TURN" — event-only/unclassified turns)
}

/// A spark (inheritance "factor") carried by the finished trainee. Kept deliberately raw-plus-derived:
/// `id` is exactly what the server sent, `kind`/`name`/`stars` are Overseer's decoding of it. A
/// consumer that trusts our decoding reads the friendly fields; one that doesn't can re-derive from
/// `id`. See `legacy::decode_factor` for how the decoding is sourced and how it degrades.
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct Spark {
    pub id: i64,
    pub kind: String, // "character" | "stat" | "aptitude" | "unique" | "skill" | "race" | "scenario" | "unknown"
    pub name: String, // "" when we can't name it (the id is still there)
    pub stars: i64,   // 1..3, 0 = unknown
}

/// One purchased skill.
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct SkillBuy {
    pub id: i64,
    pub name: String,
    pub cost: i64,
    pub turn: i64,
}

/// A support card's end-of-career bond, when the payload carried it.
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct BondRow {
    pub id: i64,
    pub name: String,
    pub bond: i64,
    pub maxed: bool,
}

/// Everything Overseer knows about a finished run, in one place. Built at finish time from the
/// server payload plus the per-turn state Overseer accumulated during the run, and consumed by the
/// webhook sender and the web UI. Every field has a defined "unknown" value (0 / "" / empty) so a
/// consumer never has to test for presence.
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct CareerSummary {
    // ── career information ──
    pub trainee: String,
    pub card_id: i64,
    pub scenario_id: i64,
    pub scenario: String,
    pub difficulty: String,
    pub running_style: String,
    pub distance: String,
    pub started_at: String, // ISO-8601 local
    pub ended_at: String,   // ISO-8601 local
    pub duration_s: u64,
    pub final_turn: i64,
    /// False when the run ended early (retired / abandoned) rather than reaching the finish screen.
    pub completed: bool,
    // ── final stats ──
    pub speed: i64,
    pub stamina: i64,
    pub power: i64,
    pub guts: i64,
    pub wisdom: i64,
    pub skill_points_remaining: i64,
    // ── sparks ──
    pub character_spark: Option<Spark>,
    pub stat_sparks: Vec<Spark>,
    pub skill_sparks: Vec<Spark>,
    pub race_sparks: Vec<Spark>,
    pub other_sparks: Vec<Spark>,
    /// Sparks inherited FROM the two legacy parents, when the payload exposed them.
    pub inherited_sparks: Vec<Spark>,
    /// The succession affinity the run started with (captured on Legacy Select), -1 = unknown.
    pub inheritance_affinity: i64,
    // ── skills ──
    pub skills_purchased: Vec<SkillBuy>,
    pub total_skills_purchased: i64,
    pub skill_points_spent: i64,
    // ── racing ──
    pub races_entered: i64,
    pub wins: i64,
    pub losses: i64,
    pub g1_wins: i64,
    pub fans_gained: i64,
    pub fans: i64,
    // ── training ──
    /// Facility → number of turns spent there ("Speed", "Stamina", "Power", "Guts", "Wit", "Rest", …).
    pub training_distribution: std::collections::BTreeMap<String, i64>,
    pub friendships_completed: i64,
    pub support_bonds: Vec<BondRow>,
    pub training_failures: i64,
    // ── summary ──
    pub grade: String,
    pub rank_score: i64,
    pub achievements: Vec<String>,
    /// Scenario-specific end-of-run values the payload carried (name → value), free-form.
    pub scenario_results: std::collections::BTreeMap<String, i64>,
    /// Any other numeric field from the server's trained-chara block we didn't model, so the report
    /// stays useful across game updates without a code change. Bounded.
    pub raw: std::collections::BTreeMap<String, i64>,
    /// Which sections we could actually fill from this payload (a game update that renames a key
    /// shows up here as `false` rather than as silently-zero statistics).
    pub sections_resolved: Vec<String>,
}

static STATE: Lazy<Mutex<CareerState>> = Lazy::new(|| Mutex::new(CareerState::default()));
static ACTIONS: Lazy<Mutex<Vec<TurnAction>>> = Lazy::new(|| Mutex::new(Vec::new()));
static RUNS: Lazy<Mutex<Vec<RunRecord>>> = Lazy::new(|| Mutex::new(Vec::new()));
static LOADED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
// MANT/dashboard extras, fed by the response hook. Carried forward between responses (a partial
// response never blanks a panel) — the hook only calls the setters when the source array is present.
static ITEMS: Lazy<Mutex<Vec<String>>> = Lazy::new(|| Mutex::new(Vec::new()));
static CATALOG: Lazy<Mutex<Vec<String>>> = Lazy::new(|| Mutex::new(Vec::new()));
static TRAINING: Lazy<Mutex<Vec<TrainingOpt>>> = Lazy::new(|| Mutex::new(Vec::new()));
static RACES: Lazy<Mutex<Vec<RaceRow>>> = Lazy::new(|| Mutex::new(Vec::new()));
/// This run's race plan, from the career-start payload — see `set_plan`.
static PLAN: Lazy<Mutex<Vec<PlannedRace>>> = Lazy::new(|| Mutex::new(Vec::new()));
/// What the server said did NOT go up this turn — see `set_blocked`.
static BLOCKED: Lazy<Mutex<Vec<String>>> = Lazy::new(|| Mutex::new(Vec::new()));
// Buffered one-shot race result (`race_reward_info`): the reward arrives in the race-flow response,
// but the turn only ADVANCES on the next `chara_info` — update() attaches it to that RACE action.
static PENDING_RACE: Lazy<Mutex<Option<(i64, i64)>>> = Lazy::new(|| Mutex::new(None)); // (rank, fans)
// Unix ms when the current career started (new-career branch of update()); 0 = unknown.
static RUN_START_MS: AtomicU64 = AtomicU64::new(0);
// Local ISO timestamp of the current run's start (webhook "career start time"). "" = unknown.
static RUN_START_ISO: Lazy<Mutex<String>> = Lazy::new(|| Mutex::new(String::new()));

// ── per-run accumulators for the completion report ──────────────────────────────────────────────
// These are derived from what Overseer already observes each turn; they exist because the server's
// finish block does NOT carry them (it reports the END STATE, not what the player did to get there).
// All are cleared by the new-career branch of `update()`.
/// Facility label → turns spent. Fed by the inferred-action classifier in `update()`.
static TRAINING_MIX: Lazy<Mutex<HashMap<String, i64>>> = Lazy::new(|| Mutex::new(HashMap::new()));
/// Skill purchases seen this run (id/name/cost/turn). Fed from the skill-learning payload.
static SKILL_BUYS: Lazy<Mutex<Vec<SkillBuy>>> = Lazy::new(|| Mutex::new(Vec::new()));
/// Support-card bonds at the last observed turn (id → (name, bond)).
static BONDS: Lazy<Mutex<Vec<BondRow>>> = Lazy::new(|| Mutex::new(Vec::new()));
/// Training failures observed this run (a turn where energy dropped with no stat gain at all).
static TRAIN_FAILS: AtomicU64 = AtomicU64::new(0);
/// G1 wins observed this run (race rows whose captured header names a G1).
static G1_WINS: AtomicU64 = AtomicU64::new(0);
/// Fans at the first turn we saw this run — `fans_gained` is the delta against the finish value.
static START_FANS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);
/// Succession affinity captured when this career's Legacy Select was confirmed (-1 = unknown).
static RUN_AFFINITY: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);

const SKILL_BUYS_CAP: usize = 200;
const BONDS_CAP: usize = 12;
const RAW_FIELDS_CAP: usize = 96;

/// Record a skill purchase (id/name/cost) at the current turn. Bounded.
pub fn note_skill_purchase(id: i64, name: &str, cost: i64) {
    let turn = STATE.lock().map(|s| s.turn).unwrap_or(0);
    if let Ok(mut v) = SKILL_BUYS.lock() {
        if v.iter().any(|s| s.id == id && id != 0) {
            return; // the learn payload can repeat; one row per skill
        }
        if v.len() >= SKILL_BUYS_CAP {
            return;
        }
        v.push(SkillBuy { id, name: name.to_string(), cost, turn });
    }
}

/// Replace the observed support-card bond rows (called whenever a response carried them).
pub fn set_bonds(rows: Vec<BondRow>) {
    if let Ok(mut b) = BONDS.lock() {
        *b = rows.into_iter().take(BONDS_CAP).collect();
    }
}

/// Note that a race this run was a G1 win (called by the race-result consumer when the grade is known).
pub fn note_g1_win() {
    G1_WINS.fetch_add(1, Ordering::Relaxed);
}

const MAX_ACTIONS: usize = 400; // one career is < ~80 turns; bounded for safety
const MAX_RACES: usize = 100; // ~20 races per career in practice; bounded for safety
const MAX_RUNS: usize = 1000;

fn runs_path() -> std::path::PathBuf {
    crate::paths::local_file("career-runs.json")
}

/// Grade from rank_score (Icarus GRADE_TABLE).
fn grade_from_score(score: i64) -> &'static str {
    const T: &[(i64, &str)] = &[
        (19200, "SS+"), (17500, "SS"), (15900, "S+"), (14500, "S"), (12100, "A+"), (10000, "A"),
        (8200, "B+"), (6500, "B"), (4900, "C+"), (3500, "C"), (2900, "D+"), (2300, "D"), (1800, "E+"),
        (1300, "E"), (900, "F+"), (600, "F"), (300, "G+"), (0, "G"),
    ];
    for (thresh, g) in T {
        if score >= *thresh {
            return g;
        }
    }
    "G"
}

fn stamp() -> String {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    format!("{:02}:{:02}:{:02} {:02}/{:02}", st.wHour, st.wMinute, st.wSecond, st.wDay, st.wMonth)
}

/// LOCAL calendar day key "YYYY-MM-DD" (same GetLocalTime source as `stamp` — no chrono dep).
/// Stored on each RunRecord at record time so daily_fans / hall_of_fame never re-derive timezones.
fn day_key() -> String {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    format!("{:04}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay)
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Unix-ms when THIS mod/game session started (set once at boot). Lets the Dashboard scope its stat
/// tiles to "this session" — runs whose epoch_ms >= this. 0 until marked.
static SESSION_START_MS: AtomicU64 = AtomicU64::new(0);
/// Call once from boot (the boot thread) so "this session" anchors to launch, not to first web poll.
pub fn mark_session_start() {
    let _ = SESSION_START_MS.compare_exchange(0, now_unix_ms(), Ordering::SeqCst, Ordering::Relaxed);
}
fn session_start_ms() -> u64 {
    let v = SESSION_START_MS.load(Ordering::Relaxed);
    if v != 0 { v } else { now_unix_ms() }
}

/// Trainee display name for a card_id ("" unknown). Backed by assets/chara_list.json (copied from
/// Icarus data/chara_list.json), compiled in via include_str! and parsed once on first use.
pub fn trainee_name(card_id: i64) -> String {
    static NAMES: OnceLock<HashMap<i64, String>> = OnceLock::new();
    let map = NAMES.get_or_init(|| {
        serde_json::from_str::<HashMap<String, String>>(include_str!("../assets/chara_list.json"))
            .map(|m| {
                m.into_iter()
                    .filter_map(|(k, v)| k.parse::<i64>().ok().map(|id| (id, v)))
                    .collect()
            })
            .unwrap_or_default()
    });
    map.get(&card_id).cloned().unwrap_or_default()
}

/// Skill display name for a skill id ("" when unknown).
///
/// Backed by `assets/skill_names.json` — a slimmed id→name projection of the project's
/// `data/skill_data.json`, compiled in with `include_str!` and parsed once on first use. It is
/// bundled rather than read from disk because only the DLL itself is guaranteed to be installed;
/// a webhook that reported bare numeric skill ids would be markedly less useful. An id the table
/// doesn't know yields "" so the caller can fall back to the id.
pub fn skill_name(id: i64) -> String {
    static NAMES: OnceLock<HashMap<i64, String>> = OnceLock::new();
    let map = NAMES.get_or_init(|| {
        serde_json::from_str::<HashMap<String, String>>(include_str!("../assets/skill_names.json"))
            .map(|m| {
                m.into_iter()
                    .filter_map(|(k, v)| k.parse::<i64>().ok().map(|id| (id, v)))
                    .collect()
            })
            .unwrap_or_default()
    });
    map.get(&id).cloned().unwrap_or_default()
}

/// Item display name for a MANT item id, or None. Ported EXACTLY from Icarus
/// career_bot/items.py ITEM_NAMES — keep the two in sync.
const fn item_name_raw(id: i64) -> Option<&'static str> {
    match id {
        1001 => Some("Speed Notepad"),
        1002 => Some("Stamina Notepad"),
        1003 => Some("Power Notepad"),
        1004 => Some("Guts Notepad"),
        1005 => Some("Wit Notepad"),
        1101 => Some("Speed Manual"),
        1102 => Some("Stamina Manual"),
        1103 => Some("Power Manual"),
        1104 => Some("Guts Manual"),
        1105 => Some("Wit Manual"),
        1201 => Some("Speed Scroll"),
        1202 => Some("Stamina Scroll"),
        1203 => Some("Power Scroll"),
        1204 => Some("Guts Scroll"),
        1205 => Some("Wit Scroll"),
        2001 => Some("Vita 20"),
        2002 => Some("Vita 40"),
        2003 => Some("Vita 65"),
        2101 => Some("Royal Kale Juice"),
        2201 => Some("Energy Drink MAX"),
        2202 => Some("Energy Drink MAX EX"),
        2301 => Some("Plain Cupcake"),
        2302 => Some("Berry Sweet Cupcake"),
        3001 => Some("Yummy Cat Food"),
        3101 => Some("Grilled Carrots"),
        4001 => Some("Pretty Mirror"),
        4002 => Some("Reporter's Binoculars"),
        4003 => Some("Master Practice Guide"),
        4004 => Some("Scholar's Hat"),
        4101 => Some("Fluffy Pillow"),
        4102 => Some("Pocket Planner"),
        4103 => Some("Rich Hand Cream"),
        4104 => Some("Smart Scale"),
        4105 => Some("Aroma Diffuser"),
        4106 => Some("Practice Drills DVD"),
        4201 => Some("Miracle Cure"),
        5001 => Some("Speed Training Application"),
        5002 => Some("Stamina Training Application"),
        5003 => Some("Power Training Application"),
        5004 => Some("Guts Training Application"),
        5005 => Some("Wit Training Application"),
        7001 => Some("Reset Whistle"),
        8001 => Some("Coaching Megaphone"),
        8002 => Some("Motivating Megaphone"),
        8003 => Some("Empowering Megaphone"),
        9001 => Some("Speed Ankle Weights"),
        9002 => Some("Stamina Ankle Weights"),
        9003 => Some("Power Ankle Weights"),
        9004 => Some("Guts Ankle Weights"),
        10001 => Some("Good-Luck Charm"),
        11001 => Some("Artisan Cleat Hammer"),
        11002 => Some("Master Cleat Hammer"),
        11003 => Some("Glow Sticks"),
        _ => None,
    }
}

/// Item display name; unknown ids fall back to "Item {id}" so nothing renders blank.
pub fn item_name(id: i64) -> String {
    match item_name_raw(id) {
        Some(n) => n.to_string(),
        None => format!("Item {id}"),
    }
}

/// Replace the held-items panel ("Name xN"). Only called when the response CARRIED the array.
pub fn set_items(items: Vec<String>) {
    if let Ok(mut i) = ITEMS.lock() {
        *i = items;
    }
    touch();
}

/// Replace the shop-catalog panel ("Name (cost)"). Only called when the response CARRIED the array.
pub fn set_catalog(catalog: Vec<String>) {
    if let Ok(mut c) = CATALOG.lock() {
        *c = catalog;
    }
    touch();
}

/// Replace this turn's training tiles. Only called when the response CARRIED command_info_array.
pub fn set_training(training: Vec<TrainingOpt>) {
    if let Ok(mut t) = TRAINING.lock() {
        *t = training;
    }
    touch();
}

/// This turn's training tiles, for the in-game prediction HUD (the web dashboard reads them out of
/// `snapshot_json` instead). Empty outside a career.
pub fn training_snapshot() -> Vec<TrainingOpt> {
    TRAINING.lock().map(|t| t.clone()).unwrap_or_default()
}

/// Replace this run's race plan. Only called from the career-start payload, which is the only place
/// the rolled schedule appears — so it is NOT cleared per turn, only when the next career starts.
pub fn set_plan(plan: Vec<PlannedRace>) {
    if let Ok(mut p) = PLAN.lock() {
        *p = plan;
    }
    touch();
}
pub fn plan_snapshot() -> Vec<PlannedRace> {
    PLAN.lock().map(|p| p.clone()).unwrap_or_default()
}

/// Replace this turn's "did not go up" notices (`not_up_parameter_info`).
///
/// Every array in that block was EMPTY for the whole career this was built from, so there is no
/// observed sample of a populated one. It is therefore reported literally — the stats it names, and
/// the bare count for the arrays whose id space we have not seen used — rather than being dressed up
/// as advice we cannot stand behind. Empty (the normal case) shows nothing at all.
pub fn set_blocked(blocked: Vec<String>) {
    let changed = BLOCKED.lock().map(|b| *b != blocked).unwrap_or(false);
    if let Ok(mut b) = BLOCKED.lock() {
        *b = blocked;
    }
    if changed {
        touch();
    }
}
pub fn blocked_snapshot() -> Vec<String> {
    BLOCKED.lock().map(|b| b.clone()).unwrap_or_default()
}

/// Buffer a race result (`race_reward_info`) until the turn advances — one-shot, latest wins.
pub fn note_race_result(rank: i64, fans: i64) {
    if let Ok(mut p) = PENDING_RACE.lock() {
        *p = Some((rank, fans));
    }
}

fn ensure_loaded() {
    if LOADED.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    if let Ok(s) = std::fs::read_to_string(runs_path()) {
        if let Ok(v) = serde_json::from_str::<Vec<RunRecord>>(&s) {
            if let Ok(mut runs) = RUNS.lock() {
                *runs = v;
            }
        }
    }
}

/// Refresh the live state from a parsed `chara_info` turn snapshot. When the turn ADVANCES, record the
/// inferred player action for the turn that just ended (stat deltas → what they did).
pub fn update(new: CareerState) {
    ensure_loaded();
    let prev = STATE.lock().map(|s| s.clone()).unwrap_or_default();
    // New career? (turn reset to a low number, or we weren't active) → clear the per-run timeline
    // and the carried-forward panels, and stamp the run start for duration_s.
    if !prev.active || new.turn < prev.turn {
        if let Ok(mut a) = ACTIONS.lock() {
            a.clear();
        }
        if let Ok(mut i) = ITEMS.lock() {
            i.clear();
        }
        if let Ok(mut c) = CATALOG.lock() {
            c.clear();
        }
        if let Ok(mut t) = TRAINING.lock() {
            t.clear();
        }
        if let Ok(mut r) = RACES.lock() {
            r.clear();
        }
        if let Ok(mut p) = PENDING_RACE.lock() {
            *p = None;
        }
        RUN_START_MS.store(now_unix_ms(), Ordering::Relaxed);
        if let Ok(mut s) = RUN_START_ISO.lock() {
            *s = crate::webhook::iso_now();
        }
        // Reset the completion-report accumulators for the new run.
        if let Ok(mut m) = TRAINING_MIX.lock() {
            m.clear();
        }
        if let Ok(mut v) = SKILL_BUYS.lock() {
            v.clear();
        }
        if let Ok(mut b) = BONDS.lock() {
            b.clear();
        }
        TRAIN_FAILS.store(0, Ordering::Relaxed);
        G1_WINS.store(0, Ordering::Relaxed);
        START_FANS.store(new.fans, Ordering::Relaxed);
        RUN_AFFINITY.store(crate::legacy::last_confirmed_affinity(), Ordering::Relaxed);
    } else if new.turn > prev.turn {
        // A TURN ADVANCE is the strongest possible "the game moved on" evidence, and it comes from
        // the server rather than from any of our own timers — so it resolves every outstanding skip
        // progression expectation. Without this the watchdog could escalate a training/event skip
        // that had, in fact, worked perfectly.
        crate::skip::train::note_progressed();
        crate::skip::event::note_progressed();
        crate::skip::result::note_progressed();
        // The turn advanced — infer what happened between prev and new.
        let d_fans = new.fans - prev.fans;
        let stats = [
            ("Speed", new.speed - prev.speed),
            ("Stamina", new.stamina - prev.stamina),
            ("Power", new.power - prev.power),
            ("Guts", new.guts - prev.guts),
            ("Wit", new.wit - prev.wit),
        ];
        let mut deltas: Vec<(String, i64)> = stats
            .iter()
            .filter(|(_, d)| *d != 0)
            .map(|(n, d)| (n.to_string(), *d))
            .collect();
        let sp_d = new.skill_point - prev.skill_point;
        if d_fans != 0 {
            deltas.push(("Fans".into(), d_fans));
        }
        if sp_d != 0 {
            deltas.push(("SP".into(), sp_d));
        }
        let stat_sum: i64 = stats.iter().map(|(_, d)| d.max(&0)).sum();
        let energy_up = new.energy > prev.energy && stat_sum == 0 && d_fans == 0;
        // Precedence: RACE > TRAIN > SKILL > REST > TURN. A skill purchase never raises a stat and
        // burns SP, so "SP dropped with no stat gain" can only be the skill screen.
        let (action, kind) = if d_fans > 0 {
            (format!("Raced (+{d_fans} fans)"), "RACE")
        } else if stat_sum > 0 {
            let (best, bd) = stats.iter().max_by_key(|(_, d)| *d).unwrap();
            (format!("Trained {best} (+{bd})"), "TRAIN")
        } else if sp_d < 0 {
            (format!("Learned skills ({sp_d} SP)"), "SKILL")
        } else if energy_up {
            ("Rested".to_string(), "REST")
        } else {
            (format!("Turn {}", prev.turn), "TURN")
        };
        // ── completion-report accumulators ──
        // Training distribution: one tally per turn, bucketed by what the turn actually was. A
        // TRAIN turn is attributed to the facility that gained the most (the same inference the
        // action label uses), so the mix mirrors what the player sees in their own history.
        {
            let bucket = match kind {
                "TRAIN" => stats
                    .iter()
                    .max_by_key(|(_, d)| *d)
                    .map(|(n, _)| n.to_string())
                    .unwrap_or_else(|| "Training".into()),
                "REST" => "Rest".into(),
                "RACE" => "Race".into(),
                "SKILL" => "Skills".into(),
                _ => "Other".into(),
            };
            if let Ok(mut m) = TRAINING_MIX.lock() {
                *m.entry(bucket).or_insert(0) += 1;
            }
        }
        // Training failure heuristic: energy dropped (so a facility WAS used) but nothing was
        // gained — no stat, no fans, no skill points. The server doesn't flag failures in
        // `chara_info`, so this is the only signal available without a dedicated hook; it is
        // reported as an observation, never as an authoritative count.
        if new.energy < prev.energy && stat_sum == 0 && d_fans == 0 && sp_d <= 0 && !energy_up {
            TRAIN_FAILS.fetch_add(1, Ordering::Relaxed);
        }
        if START_FANS.load(Ordering::Relaxed) < 0 {
            START_FANS.store(prev.fans, Ordering::Relaxed);
        }
        // A RACE turn consumes the buffered race_reward_info (place + exact fans) for the RACES panel.
        if kind == "RACE" {
            let pending = PENDING_RACE.lock().ok().and_then(|mut p| p.take());
            let (place, gained) = pending.unwrap_or((0, 0));
            let row = RaceRow {
                turn: prev.turn,
                name: crate::race::race_header(), // may be "" if the race header wasn't captured
                place,
                // The reward payload is exact; the fan DELTA can include event fans — prefer the reward.
                fans: if gained > 0 { gained } else { d_fans },
            };
            if let Ok(mut r) = RACES.lock() {
                r.push(row);
                let over = r.len().saturating_sub(MAX_RACES);
                if over > 0 {
                    r.drain(0..over);
                }
            }
        }
        let act = TurnAction {
            turn: prev.turn,
            time: stamp(),
            action,
            kind: kind.into(),
            deltas,
            speed: new.speed,
            stamina: new.stamina,
            power: new.power,
            guts: new.guts,
            wit: new.wit,
            skill_point: new.skill_point,
            fans: new.fans,
        };
        if let Ok(mut a) = ACTIONS.lock() {
            a.push(act);
            let over = a.len().saturating_sub(MAX_ACTIONS);
            if over > 0 {
                a.drain(0..over);
            }
        }
    }
    if let Ok(mut s) = STATE.lock() {
        *s = new;
    }
    touch();
}

/// Everything the response hook could extract from `single_mode_finish_common`. Only the scalars
/// Overseer previously used are required; everything else is optional so a game update that renames
/// or removes a key degrades that ONE field instead of the whole record.
#[derive(Default, Clone)]
pub struct FinishPayload {
    pub card_id: i64,
    pub rank_score: i64,
    pub fans: i64,
    pub speed: i64,
    pub stamina: i64,
    pub power: i64,
    pub guts: i64,
    pub wit: i64,
    pub skill_point: i64,
    pub wins: i64,
    pub race_count: i64,
    pub scenario_id: i64,
    /// Raw spark (factor) ids carried by the finished trainee.
    pub factor_ids: Vec<i64>,
    /// Raw spark ids that came from the two legacy parents, if the payload separated them.
    pub inherited_factor_ids: Vec<i64>,
    /// (skill_id, level) pairs the trainee finished with.
    pub skill_ids: Vec<i64>,
    /// Support-card bonds at the end of the run.
    pub bonds: Vec<BondRow>,
    /// Running-style / distance aptitude codes the payload exposed (0 = unknown).
    pub running_style: i64,
    pub proper_distance: i64,
    pub difficulty: i64,
    /// Scenario-specific end-of-run numbers, verbatim.
    pub scenario_results: std::collections::BTreeMap<String, i64>,
    /// Any other integer field on the trained-chara block (bounded) — future-proofing.
    pub raw: std::collections::BTreeMap<String, i64>,
    /// Which of the optional sections actually resolved.
    pub resolved: Vec<String>,
}

/// Record a completed run from the RICH finish payload: persists the historical `RunRecord` exactly
/// as before, then builds and dispatches the completion summary (web UI + webhook).
pub fn record_finish_full(p: FinishPayload) {
    let replayed = record_finish(
        p.card_id,
        p.rank_score,
        p.fans,
        p.speed,
        p.stamina,
        p.power,
        p.guts,
        p.wit,
        p.skill_point,
        p.wins,
        p.race_count,
        p.scenario_id,
    );
    if replayed {
        return; // duplicate finish block (relog / screen revisit) — never re-report it
    }
    let summary = build_summary(&p);
    if let Ok(mut g) = LAST_SUMMARY.lock() {
        *g = Some(summary.clone());
    }
    crate::webhook::career_completed(&summary);
    crate::legacy::note_career_finished(&summary);
}

/// The most recent completed-career summary (served to the web UI and re-sendable as a webhook).
static LAST_SUMMARY: Lazy<Mutex<Option<CareerSummary>>> = Lazy::new(|| Mutex::new(None));

/// The last completed run's full summary, if this session has seen one.
pub fn last_summary() -> Option<CareerSummary> {
    LAST_SUMMARY.lock().ok().and_then(|g| g.clone())
}

/// Re-deliver the last completed career to the configured webhooks (web UI "resend" button).
pub fn resend_last() -> bool {
    match last_summary() {
        Some(s) => {
            crate::webhook::career_completed(&s);
            true
        }
        None => false,
    }
}

/// Assemble the completion report from the server payload + the per-run accumulators.
fn build_summary(p: &FinishPayload) -> CareerSummary {
    let (final_turn, live_sp) = STATE.lock().map(|s| (s.turn, s.skill_point)).unwrap_or((0, 0));
    let start_ms = RUN_START_MS.load(Ordering::Relaxed);
    let now_ms = now_unix_ms();
    let races = RACES.lock().map(|r| r.clone()).unwrap_or_default();
    let wins_seen = races.iter().filter(|r| r.place == 1).count() as i64;
    let races_entered = if p.race_count > 0 { p.race_count } else { races.len() as i64 };
    let wins = if p.wins > 0 { p.wins } else { wins_seen };
    let start_fans = START_FANS.load(Ordering::Relaxed).max(0);
    let skills: Vec<SkillBuy> = SKILL_BUYS.lock().map(|v| v.clone()).unwrap_or_default();
    let sp_spent: i64 = skills.iter().map(|s| s.cost).sum();
    let mix: std::collections::BTreeMap<String, i64> = TRAINING_MIX
        .lock()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), *v)).collect())
        .unwrap_or_default();
    let bonds = if !p.bonds.is_empty() {
        p.bonds.clone()
    } else {
        BONDS.lock().map(|b| b.clone()).unwrap_or_default()
    };
    let friendships = bonds.iter().filter(|b| b.maxed).count() as i64;

    // Sparks: decode each raw id, then bucket by kind so the report reads the way the game presents
    // them (one character spark, the stat sparks, the skill sparks, the race sparks).
    let decoded: Vec<Spark> = p.factor_ids.iter().map(|id| crate::legacy::decode_factor(*id)).collect();
    let mut character_spark = None;
    let (mut stat_sparks, mut skill_sparks, mut race_sparks, mut other_sparks) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for s in decoded {
        match s.kind.as_str() {
            "character" | "unique" => {
                if character_spark.is_none() {
                    character_spark = Some(s);
                } else {
                    other_sparks.push(s);
                }
            }
            "stat" => stat_sparks.push(s),
            "skill" => skill_sparks.push(s),
            "race" => race_sparks.push(s),
            _ => other_sparks.push(s),
        }
    }
    let inherited_sparks: Vec<Spark> = p
        .inherited_factor_ids
        .iter()
        .map(|id| crate::legacy::decode_factor(*id))
        .collect();

    let mut achievements: Vec<String> = Vec::new();
    if wins > 0 && wins == races_entered {
        achievements.push(format!("Undefeated — {wins}/{races_entered}"));
    }
    let g1 = G1_WINS.load(Ordering::Relaxed) as i64;
    if g1 > 0 {
        achievements.push(format!("{g1} G1 victories"));
    }
    let grade = grade_from_score(p.rank_score).to_string();
    if matches!(grade.as_str(), "SS+" | "SS" | "S+" | "S") {
        achievements.push(format!("Reached grade {grade}"));
    }
    if let Some(cs) = &character_spark {
        if cs.stars >= 3 {
            achievements.push(format!("3★ character spark: {}", cs.name));
        }
    }

    CareerSummary {
        trainee: trainee_name(p.card_id),
        card_id: p.card_id,
        scenario_id: p.scenario_id,
        scenario: crate::legacy::scenario_name(p.scenario_id),
        difficulty: crate::legacy::difficulty_name(p.difficulty),
        running_style: crate::legacy::running_style_name(p.running_style),
        distance: crate::legacy::distance_name(p.proper_distance),
        started_at: RUN_START_ISO.lock().map(|s| s.clone()).unwrap_or_default(),
        ended_at: crate::webhook::iso_now(),
        duration_s: if start_ms > 0 && now_ms > start_ms { (now_ms - start_ms) / 1000 } else { 0 },
        final_turn,
        completed: true,
        speed: p.speed,
        stamina: p.stamina,
        power: p.power,
        guts: p.guts,
        wisdom: p.wit,
        skill_points_remaining: if p.skill_point > 0 { p.skill_point } else { live_sp },
        character_spark,
        stat_sparks,
        skill_sparks,
        race_sparks,
        other_sparks,
        inherited_sparks,
        inheritance_affinity: RUN_AFFINITY.load(Ordering::Relaxed),
        total_skills_purchased: skills.len() as i64,
        skill_points_spent: sp_spent,
        skills_purchased: skills,
        races_entered,
        wins,
        losses: (races_entered - wins).max(0),
        g1_wins: g1,
        fans_gained: (p.fans - start_fans).max(0),
        fans: p.fans,
        training_distribution: mix,
        friendships_completed: friendships,
        support_bonds: bonds,
        training_failures: TRAIN_FAILS.load(Ordering::Relaxed) as i64,
        grade,
        rank_score: p.rank_score,
        achievements,
        scenario_results: p.scenario_results.clone(),
        raw: p.raw.clone(),
        sections_resolved: p.resolved.clone(),
    }
}

impl CareerSummary {
    /// Render to the webhook body, honouring the per-section include flags.
    pub fn to_json(&self, sec: &crate::webhook::WebhookSections) -> serde_json::Value {
        let mut o = serde_json::Map::new();
        // Identity is never optional — a consumer must always be able to tell runs apart.
        o.insert("completed".into(), serde_json::json!(self.completed));
        o.insert("sections_resolved".into(), serde_json::json!(self.sections_resolved));
        if sec.career_info {
            o.insert(
                "career_info".into(),
                serde_json::json!({
                    "character": self.trainee,
                    "card_id": self.card_id,
                    "scenario": self.scenario,
                    "scenario_id": self.scenario_id,
                    "difficulty": self.difficulty,
                    "running_style": self.running_style,
                    "distance": self.distance,
                    "started_at": self.started_at,
                    "ended_at": self.ended_at,
                    "duration_seconds": self.duration_s,
                    "duration_human": crate::webhook::hms(self.duration_s),
                    "final_turn": self.final_turn,
                }),
            );
        }
        if sec.final_stats {
            o.insert(
                "final_stats".into(),
                serde_json::json!({
                    "speed": self.speed,
                    "stamina": self.stamina,
                    "power": self.power,
                    "guts": self.guts,
                    "wisdom": self.wisdom,
                    "skill_points_remaining": self.skill_points_remaining,
                    "total": self.speed + self.stamina + self.power + self.guts + self.wisdom,
                }),
            );
        }
        if sec.sparks {
            o.insert(
                "sparks".into(),
                serde_json::json!({
                    "character": self.character_spark,
                    "stats": self.stat_sparks,
                    "skills": self.skill_sparks,
                    "races": self.race_sparks,
                    "other": self.other_sparks,
                    "inheritance": {
                        "affinity": if self.inheritance_affinity < 0 {
                            serde_json::Value::Null
                        } else {
                            serde_json::json!(self.inheritance_affinity)
                        },
                        "rank": crate::legacy::affinity_rank(self.inheritance_affinity),
                        "inherited": self.inherited_sparks,
                    },
                }),
            );
        }
        if sec.skills {
            o.insert(
                "skills".into(),
                serde_json::json!({
                    "purchased": self.skills_purchased,
                    "total_purchased": self.total_skills_purchased,
                    "skill_points_spent": self.skill_points_spent,
                    "skill_points_remaining": self.skill_points_remaining,
                }),
            );
        }
        if sec.racing {
            o.insert(
                "racing".into(),
                serde_json::json!({
                    "races_entered": self.races_entered,
                    "wins": self.wins,
                    "losses": self.losses,
                    "g1_wins": self.g1_wins,
                    "fans_gained": self.fans_gained,
                    "final_fan_count": self.fans,
                }),
            );
        }
        if sec.training {
            o.insert(
                "training".into(),
                serde_json::json!({
                    "distribution": self.training_distribution,
                    "friendships_completed": self.friendships_completed,
                    "support_bonds": self.support_bonds,
                    "failures": self.training_failures,
                }),
            );
        }
        if sec.summary {
            o.insert(
                "summary".into(),
                serde_json::json!({
                    "final_rank": self.grade,
                    "evaluation_score": self.rank_score,
                    "achievements": self.achievements,
                    "scenario_results": self.scenario_results,
                }),
            );
        }
        if sec.timeline {
            let actions = ACTIONS.lock().map(|a| a.clone()).unwrap_or_default();
            o.insert("timeline".into(), serde_json::json!(actions));
        }
        if !self.raw.is_empty() {
            o.insert("raw".into(), serde_json::json!(self.raw));
        }
        serde_json::Value::Object(o)
    }

    /// A representative summary used by the webhook "Send test" button, so a user can verify their
    /// endpoint (and their downstream parsing) without finishing a career first.
    pub fn sample() -> CareerSummary {
        let mut mix = std::collections::BTreeMap::new();
        mix.insert("Speed".to_string(), 24);
        mix.insert("Stamina".to_string(), 11);
        mix.insert("Power".to_string(), 14);
        mix.insert("Guts".to_string(), 3);
        mix.insert("Wit".to_string(), 8);
        mix.insert("Rest".to_string(), 7);
        mix.insert("Race".to_string(), 12);
        CareerSummary {
            trainee: "Special Week".into(),
            card_id: 100101,
            scenario_id: 1,
            scenario: crate::legacy::scenario_name(1),
            difficulty: "Normal".into(),
            running_style: "Front Runner".into(),
            distance: "Medium".into(),
            started_at: crate::webhook::iso_now(),
            ended_at: crate::webhook::iso_now(),
            duration_s: 3 * 3600 + 12 * 60,
            final_turn: 78,
            completed: true,
            speed: 1180,
            stamina: 742,
            power: 903,
            guts: 402,
            wisdom: 611,
            skill_points_remaining: 145,
            character_spark: Some(Spark { id: 0, kind: "character".into(), name: "Special Week".into(), stars: 3 }),
            stat_sparks: vec![
                Spark { id: 0, kind: "stat".into(), name: "Speed".into(), stars: 3 },
                Spark { id: 0, kind: "stat".into(), name: "Power".into(), stars: 2 },
            ],
            skill_sparks: vec![Spark { id: 0, kind: "skill".into(), name: "Professor of Curvature".into(), stars: 1 }],
            race_sparks: vec![Spark { id: 0, kind: "race".into(), name: "Japanese Derby".into(), stars: 2 }],
            other_sparks: Vec::new(),
            inherited_sparks: Vec::new(),
            inheritance_affinity: 187,
            skills_purchased: vec![
                SkillBuy { id: 200331, name: "Professor of Curvature".into(), cost: 170, turn: 62 },
                SkillBuy { id: 200041, name: "Straightaway Recovery".into(), cost: 160, turn: 66 },
            ],
            total_skills_purchased: 2,
            skill_points_spent: 330,
            races_entered: 12,
            wins: 10,
            losses: 2,
            g1_wins: 5,
            fans_gained: 84_200,
            fans: 91_450,
            training_distribution: mix,
            friendships_completed: 5,
            support_bonds: vec![BondRow { id: 30028, name: "Kitasan Black (SSR)".into(), bond: 100, maxed: true }],
            training_failures: 3,
            grade: "A+".into(),
            rank_score: 12_640,
            achievements: vec!["5 G1 victories".into(), "Reached grade A+".into()],
            scenario_results: std::collections::BTreeMap::new(),
            raw: std::collections::BTreeMap::new(),
            sections_resolved: vec!["sample".into()],
        }
    }
}

/// Record a completed run from the finish payload, persist it, and end the current run.
/// Returns TRUE when the payload was a replay of the previous record (and nothing was stored), so
/// the caller can suppress duplicate reporting.
#[allow(clippy::too_many_arguments)]
pub fn record_finish(
    card_id: i64,
    rank_score: i64,
    fans: i64,
    speed: i64,
    stamina: i64,
    power: i64,
    guts: i64,
    wit: i64,
    skill_point: i64,
    wins: i64,
    race_count: i64,
    scenario_id: i64,
) -> bool {
    ensure_loaded();
    let (final_turn, live_sp) = STATE
        .lock()
        .map(|s| (s.turn, s.skill_point))
        .unwrap_or((0, 0));
    let now_ms = now_unix_ms();
    let start_ms = RUN_START_MS.load(Ordering::Relaxed);
    // Tally this run's inferred actions by kind (the ACTIONS timeline is per-run: cleared when the
    // next career starts, so at finish time it holds exactly this run's turns).
    let (mut act_train, mut act_race, mut act_rest, mut act_skill, mut act_other) =
        (0u32, 0u32, 0u32, 0u32, 0u32);
    if let Ok(a) = ACTIONS.lock() {
        for act in a.iter() {
            match act.kind.as_str() {
                "TRAIN" => act_train += 1,
                "RACE" => act_race += 1,
                "REST" => act_rest += 1,
                "SKILL" => act_skill += 1,
                _ => act_other += 1,
            }
        }
    }
    let rec = RunRecord {
        ts: stamp(),
        card_id,
        grade: grade_from_score(rank_score).to_string(),
        rank_score,
        fans,
        speed,
        stamina,
        power,
        guts,
        wit,
        // The finish roster entry carries no skill_point (Icarus tracks it live too) — fall back to
        // the last per-turn value we tracked for this run.
        skill_point: if skill_point > 0 { skill_point } else { live_sp },
        wins,
        race_count,
        scenario_id,
        final_turn,
        epoch_ms: now_ms,
        duration_s: if start_ms > 0 && now_ms > start_ms { (now_ms - start_ms) / 1000 } else { 0 },
        day: day_key(),
        act_train,
        act_race,
        act_rest,
        act_skill,
        act_other,
    };
    if let Ok(mut runs) = RUNS.lock() {
        // Replay guard: the server can resend the same finish block (relog, screen revisit) — an
        // identical record to the last one is a replay, not a new run.
        if let Some(last) = runs.last() {
            if last.card_id == rec.card_id
                && last.rank_score == rec.rank_score
                && last.fans == rec.fans
                && last.speed == rec.speed
                && last.stamina == rec.stamina
                && last.power == rec.power
                && last.guts == rec.guts
                && last.wit == rec.wit
                && last.wins == rec.wins
                && last.race_count == rec.race_count
            {
                crate::tools::log("[career] duplicate finish payload ignored (replay)");
                return true;
            }
        }
        runs.push(rec);
        let over = runs.len().saturating_sub(MAX_RUNS);
        if over > 0 {
            runs.drain(0..over);
        }
        // Persist atomically (tmp + rename).
        if let Ok(json) = serde_json::to_string_pretty(&*runs) {
            let path = runs_path();
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
    // End the run.
    if let Ok(mut s) = STATE.lock() {
        s.active = false;
    }
    touch();
    crate::tools::log(&format!(
        "[career] run recorded: grade {} · {rank_score} pts · {fans} fans · {wins}W/{race_count}R",
        grade_from_score(rank_score)
    ));
    false
}

/// Snapshot of the completed-run history (oldest first, as stored on disk). For endpoint-layer
/// aggregation across runs (AI Brain observed stats) — same clone snapshot_json takes internally.
pub fn runs_snapshot() -> Vec<RunRecord> {
    ensure_loaded();
    RUNS.lock().map(|r| r.clone()).unwrap_or_default()
}

/// The web-UI payload (dashboard v2 contract): live state + this run's panels (items / catalog /
/// training / races / actions) + the completed-run history with the derived aggregates
/// (summary / leaderboard / hall_of_fame). Every field is ALWAYS present — empty/0/"" when absent.
// PERF: `snapshot_json` clones the ENTIRE run history (up to 1000 records), re-derives the
// leaderboard and the 12-month hall of fame, and serialises the lot. The web UI polls it every
// 2.5 s forever, on a thread the server spawns per request — so on a long-lived install this was a
// steady background CPU + allocation load for data that only changes when a turn advances. Cache
// the rendered string against a version counter every mutator bumps; a poll on unchanged data is
// now a clone of an already-built String.
static DATA_VERSION: AtomicU64 = AtomicU64::new(1);
static SNAPSHOT_CACHE: Lazy<Mutex<(u64, String)>> = Lazy::new(|| Mutex::new((0, String::new())));

/// Mark the career data as changed (invalidates the dashboard snapshot cache).
fn touch() {
    DATA_VERSION.fetch_add(1, Ordering::Relaxed);
}

pub fn snapshot_json() -> String {
    ensure_loaded();
    let want = DATA_VERSION.load(Ordering::Relaxed);
    if let Ok(c) = SNAPSHOT_CACHE.lock() {
        if c.0 == want && !c.1.is_empty() {
            return c.1.clone();
        }
    }
    let json = build_snapshot_json();
    if let Ok(mut c) = SNAPSHOT_CACHE.lock() {
        *c = (want, json.clone());
    }
    json
}

fn build_snapshot_json() -> String {
    let state = STATE.lock().map(|s| s.clone()).unwrap_or_default();
    let actions = ACTIONS.lock().map(|a| a.clone()).unwrap_or_default();
    let runs = RUNS.lock().map(|r| r.clone()).unwrap_or_default();
    let items = ITEMS.lock().map(|i| i.clone()).unwrap_or_default();
    let catalog = CATALOG.lock().map(|c| c.clone()).unwrap_or_default();
    let training = TRAINING.lock().map(|t| t.clone()).unwrap_or_default();
    let races = RACES.lock().map(|r| r.clone()).unwrap_or_default();
    // Aggregate dashboard stats.
    let total_runs = runs.len();
    let total_fans: i64 = runs.iter().map(|r| r.fans).sum();
    let best_score = runs.iter().map(|r| r.rank_score).max().unwrap_or(0);
    let best_grade = grade_from_score(best_score);
    let total_wins: i64 = runs.iter().map(|r| r.wins).sum();
    let total_races: i64 = runs.iter().map(|r| r.race_count).sum();
    // Daily fans: LOCAL-date compare via the stored day key (legacy rows have day == "" and today's
    // key is never empty, so they fall out naturally).
    let today = day_key();
    let daily_fans: i64 = runs.iter().filter(|r| r.day == today).map(|r| r.fans).sum();
    // Run rows for the UI: newest first, with the trainee name resolved server-side from card_id
    // (works for legacy rows too — the name is never stored).
    let run_rows: Vec<serde_json::Value> = runs
        .iter()
        .rev()
        .map(|r| {
            let mut v = serde_json::to_value(r).unwrap_or_else(|_| serde_json::json!({}));
            if let Some(o) = v.as_object_mut() {
                o.insert("trainee".into(), serde_json::json!(trainee_name(r.card_id)));
            }
            v
        })
        .collect();
    // All-time leaderboard: top 10 by rank_score, competition ranking (ties share a rank, the next
    // distinct score skips: 1,2,2,4). epoch_ms AND ts both emitted so legacy rows still get a "When".
    let mut by_score: Vec<&RunRecord> = runs.iter().collect();
    by_score.sort_by(|a, b| b.rank_score.cmp(&a.rank_score));
    let mut leaderboard: Vec<serde_json::Value> = Vec::new();
    let mut rank = 0usize;
    let mut prev_score = i64::MIN;
    for (i, r) in by_score.iter().take(10).enumerate() {
        if r.rank_score != prev_score {
            rank = i + 1;
            prev_score = r.rank_score;
        }
        leaderboard.push(serde_json::json!({
            "rank": rank,
            "trainee": trainee_name(r.card_id),
            "grade": r.grade,
            "rank_score": r.rank_score,
            "fans": r.fans,
            "scenario_id": r.scenario_id,
            "epoch_ms": r.epoch_ms,
            "ts": r.ts,
        }));
    }
    // Hall of fame: month buckets keyed on the day key's "YYYY-MM" prefix, newest first, max 12.
    // Legacy rows (day == "") carry no local date, so they are skipped rather than mis-bucketed.
    let cur_month = &today[..7];
    let mut buckets: HashMap<String, Vec<&RunRecord>> = HashMap::new();
    for r in runs.iter().filter(|r| r.day.len() >= 7) {
        buckets.entry(r.day[..7].to_string()).or_default().push(r);
    }
    let mut months: Vec<String> = buckets.keys().cloned().collect();
    months.sort_by(|a, b| b.cmp(a)); // "YYYY-MM" sorts lexicographically == chronologically
    let hall_of_fame: Vec<serde_json::Value> = months
        .iter()
        .take(12)
        .map(|m| {
            let rows = &buckets[m];
            let month_fans: i64 = rows.iter().map(|r| r.fans).sum();
            let mut podium: Vec<&&RunRecord> = rows.iter().collect();
            podium.sort_by(|a, b| b.rank_score.cmp(&a.rank_score));
            serde_json::json!({
                "month": m,
                "live": m.as_str() == cur_month,
                "runs": rows.len(),
                "total_fans": month_fans,
                "podium": podium
                    .iter()
                    .take(3)
                    .map(|r| serde_json::json!({
                        "trainee": trainee_name(r.card_id),
                        "grade": r.grade,
                        "rank_score": r.rank_score,
                    }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    // Trainee header: only meaningful while a career is live — "" when idle/unknown.
    let trainee = if state.active { trainee_name(state.card_id) } else { String::new() };
    serde_json::json!({
        "state": state,
        "trainee": trainee,
        "items_held": items,
        "catalog": catalog,
        "training": training,
        "races": races,
        "actions": actions,
        "runs": run_rows,
        "session_start_ms": session_start_ms(),
        "summary": {
            "total_runs": total_runs,
            "total_fans": total_fans,
            "best_grade": best_grade,
            "best_score": best_score,
            "total_wins": total_wins,
            "total_races": total_races,
            "daily_fans": daily_fans,
        },
        "leaderboard": leaderboard,
        "hall_of_fame": hall_of_fame,
    })
    .to_string()
}
