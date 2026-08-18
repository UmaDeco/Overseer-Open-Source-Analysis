//! Overseer Plan B — B6: in-process name enrichment.
//!
//! In Plan A the Python host (DataDB) mapped ids → names. Plan B has no Python,
//! so we embed the curated lookup tables straight into the DLL with include_str!
//! and parse them once. Paths are relative to this source file
//! (native/src/names.rs → ../../data/).

#![allow(dead_code)]

use once_cell::sync::Lazy;
use std::collections::HashMap;

use serde::Deserialize;
use serde_json::Value;

const CHARA_JSON: &str = include_str!("../../data/chara_list.json");
const SUPPORT_JSON: &str = include_str!("../../data/support_list.json");
const SKILL_JSON: &str = include_str!("../../data/skill_data.json");
// Slim projections of Icarus's data/race_map.json and data/factor_map.json — regenerate them from
// there when the game adds races or sparks. Bundled rather than read from disk for the same reason
// the tables above are: the DLL is the only thing guaranteed to be installed.
const RACE_PROGRAMS_JSON: &str = include_str!("../assets/race_programs.json");
const FACTOR_NAMES_JSON: &str = include_str!("../assets/factor_names.json");
// support_card_id -> the skills it can give a hint for (Icarus data/support_hint_skills.json).
const SUPPORT_HINTS_JSON: &str = include_str!("../assets/support_hints.json");
// Overseer's own skill table, re-included here for the NAME → group reverse lookup (career.rs owns
// the forward id → name direction). `include_str!` of the same file twice costs nothing.
const SKILL_NAMES_JSON: &str = include_str!("../assets/skill_names.json");

// chara_list.json:  { "100101": "Special Week", ... }
static CHARA: Lazy<HashMap<String, String>> =
    Lazy::new(|| serde_json::from_str(CHARA_JSON).unwrap_or_default());
// support_list.json: { "10001": { "name": "...", "rarity": "R", "type": "Guts" } }
static SUPPORT: Lazy<HashMap<String, Value>> =
    Lazy::new(|| serde_json::from_str(SUPPORT_JSON).unwrap_or_default());
// skill_data.json: { "10071": { "name": "...", ... } }
static SKILL: Lazy<HashMap<String, Value>> =
    Lazy::new(|| serde_json::from_str(SKILL_JSON).unwrap_or_default());

fn name_field(map: &HashMap<String, Value>, id: i64) -> String {
    map.get(&id.to_string())
        .and_then(|v| v.get("name"))
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Character display name for a card/chara id ("" if unknown).
pub fn chara_name(id: i64) -> String {
    CHARA.get(&id.to_string()).cloned().unwrap_or_default()
}

/// Character display name for a BARE chara id (`1014`), as the event-reward tables use — the table
/// above is keyed by 6-digit card ids (`101401` = the default outfit, `101402` = an alt), so this
/// folds every outfit of a character onto `card_id / 100` and takes the lowest card id, which is
/// always the base outfit and therefore the plain character name. Built once, on first use.
pub fn chara_name_by_chara_id(chara_id: i64) -> String {
    static BY_CHARA: Lazy<HashMap<i64, String>> = Lazy::new(|| {
        let mut best: HashMap<i64, (i64, String)> = HashMap::new();
        for (k, v) in CHARA.iter() {
            let Ok(card) = k.parse::<i64>() else { continue };
            let e = best.entry(card / 100).or_insert((card, v.clone()));
            if card < e.0 {
                *e = (card, v.clone());
            }
        }
        best.into_iter().map(|(c, (_, n))| (c, n)).collect()
    });
    BY_CHARA.get(&chara_id).cloned().unwrap_or_default()
}

/// Support card display name ("" if unknown).
pub fn support_name(id: i64) -> String {
    name_field(&SUPPORT, id)
}

/// Race details for a `program_id` → `(name, ground, distance_m)`, where ground is 1 turf / 2 dirt.
/// An unknown program yields `("", 0, 0)` so the caller shows the id rather than a wrong race.
pub fn race_program(program_id: i64) -> (String, i64, i64) {
    // [name, ground, distance] — an array, because at 745 rows the key names would be most of the file.
    static PROGRAMS: Lazy<HashMap<String, (String, i64, i64)>> = Lazy::new(|| {
        serde_json::from_str::<HashMap<String, (String, i64, i64)>>(RACE_PROGRAMS_JSON).unwrap_or_default()
    });
    PROGRAMS
        .get(&program_id.to_string())
        .cloned()
        .unwrap_or_else(|| (String::new(), 0, 0))
}

/// The skills a support card can give a hint for — the pool behind the red [!] on a training tile.
///
/// The tile itself names no skill (all 3,932 observed carry the same eight fields, none of them a
/// skill id), so which one fires is only revealed by the follow-up event. This is therefore a
/// CANDIDATE SET, and callers must present it as one. It was validated against a full career: of the
/// 11 [!] trainings that produced a hint, the skill was in the offering card's pool **11 times**.
///
/// Empty for a card the table doesn't list (it covers 216; a brand-new release won't be there yet).
pub fn support_hint_pool(card_id: i64) -> Vec<String> {
    static POOLS: Lazy<HashMap<String, Vec<String>>> =
        Lazy::new(|| serde_json::from_str(SUPPORT_HINTS_JSON).unwrap_or_default());
    POOLS.get(&card_id.to_string()).cloned().unwrap_or_default()
}

/// Skill display name → its hint GROUP id, so a pool entry can be checked against the trainee's
/// `skill_tips_array` (which is keyed by group, not by skill id: every rarity variant of a skill —
/// the ○ and ◎ forms — shares one group, and `group = skill_id / 10`).
///
/// All 168 distinct skills across the hint pools resolve through this.
pub fn skill_group_of_name(name: &str) -> Option<i64> {
    static BY_NAME: Lazy<HashMap<String, i64>> = Lazy::new(|| {
        serde_json::from_str::<HashMap<String, String>>(SKILL_NAMES_JSON)
            .map(|m| {
                m.into_iter()
                    .filter_map(|(id, nm)| id.parse::<i64>().ok().map(|i| (nm, i / 10)))
                    .collect()
            })
            .unwrap_or_default()
    });
    BY_NAME.get(name).copied()
}

/// Inheritance-spark details for a `factor_id` → `(kind, name, stars)`, or None when unlisted.
///
/// `kind` uses the same vocabulary as `career::Spark` (stat / aptitude / unique / race / skill /
/// scenario / other). This is the BUNDLED baseline; `legacy.rs` still prefers an on-disk
/// `data/factor_data.json` when the user ships one, and falls back to its id heuristic for anything
/// neither table knows.
pub fn factor(factor_id: i64) -> Option<(String, String, i64)> {
    #[derive(Deserialize)]
    struct Row {
        #[serde(default)]
        kind: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        stars: i64,
    }
    static FACTORS: Lazy<HashMap<i64, (String, String, i64)>> = Lazy::new(|| {
        serde_json::from_str::<HashMap<String, Row>>(FACTOR_NAMES_JSON)
            .map(|m| {
                m.into_iter()
                    .filter_map(|(k, v)| k.parse::<i64>().ok().map(|id| (id, (v.kind, v.name, v.stars))))
                    .collect()
            })
            .unwrap_or_default()
    });
    FACTORS.get(&factor_id).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The [!] pool is only useful if its names can be checked against the trainee's own held hints,
    /// and `skill_tips_array` is keyed by GROUP. Any pool entry that failed to resolve would silently
    /// render as "you don't have this" forever, which is the one way this display can mislead.
    #[test]
    fn every_hint_pool_name_resolves_to_a_group() {
        let pools: HashMap<String, Vec<String>> =
            serde_json::from_str(SUPPORT_HINTS_JSON).expect("bundled pool table parses");
        assert!(pools.len() >= 200, "expected the full card table, got {}", pools.len());

        let unresolved: Vec<&String> = pools
            .values()
            .flatten()
            .filter(|n| skill_group_of_name(n).is_none())
            .collect();
        assert!(unresolved.is_empty(), "pool skills with no group: {unresolved:?}");
    }

    /// A spot check on the deck from the capture this was validated against: Kitasan Black's SSR
    /// offers 8 skills, and an unlisted card yields an empty pool rather than a wrong one.
    #[test]
    fn a_known_card_yields_its_pool_and_an_unknown_one_yields_nothing() {
        let kitasan = support_hint_pool(30028);
        assert_eq!(kitasan.len(), 8);
        assert!(kitasan.iter().all(|s| skill_group_of_name(s).is_some()));
        // 30052 was in the capture's deck but is not in the table — must degrade, not invent.
        assert!(support_hint_pool(30052).is_empty());
        assert!(support_hint_pool(0).is_empty());
    }

    /// Race and factor tables: the ids the capture actually used must resolve, and an unlisted id
    /// must come back blank so the caller falls back to showing the raw number.
    #[test]
    fn the_bundled_race_and_factor_tables_resolve_real_ids() {
        assert_eq!(race_program(1070), ("Junior Make Debut".into(), 1, 1600));
        assert_eq!(race_program(81).0, "Arima Kinen");
        assert_eq!(race_program(1106), (String::new(), 0, 0)); // scenario-only, not in the table

        assert_eq!(factor(403), Some(("stat".into(), "Guts".into(), 3)));
        assert_eq!(factor(10060101).map(|f| f.1), Some("Triumphant Pulse".into()));
        assert_eq!(factor(0), None);
    }

    /// A bare 4-digit chara id resolves through the 6-digit card table to the base outfit's name.
    #[test]
    fn chara_ids_resolve_without_their_outfit_suffix() {
        assert_eq!(chara_name_by_chara_id(1006), "Oguri Cap");
        assert_eq!(chara_name_by_chara_id(1014), "El Condor Pasa");
        assert_eq!(chara_name_by_chara_id(9008), "", "scenario group ids are not characters");
    }
}
