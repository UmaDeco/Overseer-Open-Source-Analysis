//! Pre-race FIELD — who you are actually running against, decoded at the entry screen.
//!
//! `race_start_info.race_horse_data` is a complete dossier on every runner: exact speed / stamina /
//! power / guts / wit, running style, the ten aptitude grades, mood, popularity and learned skills.
//! The server sends it at **`race_entry`** — 14/14 times in a full career capture — a median 4.4 s
//! (min 2.6 s) before `race_start`, and `race_scenario` (the outcome, decoded by `race_reveal`) is
//! NOT in that response. So there is a real window where you know the field but not yet the result:
//! the point at which backing out or changing running style still means something.
//!
//! `race_reveal` answers "where do I finish". This answers "against whom, and on what" — the two are
//! deliberately separate modules because they decode different payloads at different moments.
//!
//! Everything here is arithmetic on numbers the server volunteered. Nothing is predicted or modelled:
//! a runner's stat total is a sum, and the rank is a sort. No request is ever sent.

#![allow(dead_code)]

use std::sync::Mutex;

use once_cell::sync::Lazy;
use rmpv::Value;
use serde::Serialize;

use crate::msgpack::{as_arr, find_key, map_get};

/// One runner, as the server describes it before the gates open.
#[derive(Clone, Serialize)]
pub struct Runner {
    /// Post position (1-based). The result table in `race_reveal` keys on `frame_order - 1`.
    pub frame: i64,
    /// Character name; "" for the anonymous mob runners that fill out a small field.
    pub name: String,
    pub is_player: bool,
    pub speed: i64,
    pub stamina: i64,
    pub power: i64,
    pub guts: i64,
    pub wit: i64,
    /// The five stats summed — the only ranking input, and deliberately a plain sum: weighting them
    /// per distance would be a model, and a model is exactly what this module refuses to be.
    pub total: i64,
    pub style: &'static str,
    pub popularity: i64,
    /// 1..5 (Awful..Great).
    pub mood: i64,
    /// Aptitude letters for THIS race's surface and distance, and for the style this runner uses.
    pub apt_ground: &'static str,
    pub apt_distance: &'static str,
    pub apt_style: &'static str,
    pub skills: usize,
}

#[derive(Clone, Serialize, Default)]
pub struct Field {
    pub program_id: i64,
    /// "" when the program id isn't in the bundled table (a new race, or a scenario-only one).
    pub race_name: String,
    pub distance: i64,
    pub distance_label: &'static str,
    pub ground: &'static str,
    pub weather: &'static str,
    pub condition: &'static str,
    /// Sorted by `total`, strongest first.
    pub runners: Vec<Runner>,
    pub field_size: usize,
    pub player_frame: i64,
    /// The player's 1-based position when the field is ranked by stat total; 0 if not identified.
    pub player_rank: usize,
}

static LATEST: Lazy<Mutex<Option<Field>>> = Lazy::new(|| Mutex::new(None));

/// The field for the race on screen, or None outside one.
pub fn latest() -> Option<Field> {
    LATEST.lock().ok().and_then(|f| f.clone())
}

/// Drop the field (the race finished, or the career ended).
pub fn clear() {
    if let Ok(mut f) = LATEST.lock() {
        *f = None;
    }
}

/// Aptitude grade. 1=G .. 8=S, per the scale Icarus documents alongside `data/card_aptitudes.json`.
fn grade(v: i64) -> &'static str {
    match v {
        8 => "S",
        7 => "A",
        6 => "B",
        5 => "C",
        4 => "D",
        3 => "E",
        2 => "F",
        1 => "G",
        _ => "?",
    }
}

/// 1 nige / 2 senko / 3 sashi / 4 oikomi — the same mapping Icarus uses in `skill_categories.py`.
fn style_name(v: i64) -> &'static str {
    match v {
        1 => "Front",
        2 => "Pace",
        3 => "Late",
        4 => "End",
        _ => "?",
    }
}

// Weather and track condition are the game's documented 1..4 scales. Only 1/1 occurs in the capture
// this module was built from, so the other three labels come from the published scale rather than
// from observed traffic; an out-of-range code prints as a number instead of guessing.
fn weather_name(v: i64) -> &'static str {
    match v {
        1 => "Sunny",
        2 => "Cloudy",
        3 => "Rainy",
        4 => "Snowy",
        _ => "?",
    }
}
fn condition_name(v: i64) -> &'static str {
    match v {
        1 => "Firm",
        2 => "Good",
        3 => "Soft",
        4 => "Heavy",
        _ => "?",
    }
}

/// The distance bracket a length falls in, which selects which of the four distance aptitudes
/// actually applies to this race.
fn distance_bracket(m: i64) -> &'static str {
    match m {
        0 => "?",
        d if d <= 1400 => "Sprint",
        d if d <= 1800 => "Mile",
        d if d <= 2400 => "Medium",
        _ => "Long",
    }
}

/// Parse `race_start_info` and publish the field. Called for any response carrying one; a payload
/// with no `race_horse_data` (the empty `race_start_info` that rides along on ordinary turn
/// responses) is ignored rather than blanking a live field.
pub fn note(val: &Value) {
    let mut hits: Vec<&Value> = Vec::new();
    find_key(val, "race_start_info", &mut hits);
    let Some(rsi) = hits.into_iter().find(|v| {
        map_get(v, "race_horse_data").and_then(as_arr).map(|a| !a.is_empty()).unwrap_or(false)
    }) else {
        return;
    };
    let Some(horses) = map_get(rsi, "race_horse_data").and_then(as_arr) else { return };

    let gi = |v: &Value, k: &str| map_get(v, k).and_then(|x| x.as_i64()).unwrap_or(0);
    let program_id = gi(rsi, "program_id");
    let (race_name, ground_code, distance) = crate::names::race_program(program_id);
    let bracket = distance_bracket(distance);

    // The account's own viewer_id identifies the player's row; every NPC carries 0.
    let player_vid = {
        let mut ids: Vec<&Value> = Vec::new();
        find_key(val, "viewer_id", &mut ids);
        ids.into_iter().find_map(|v| v.as_i64()).filter(|&x| x != 0).unwrap_or(0)
    };

    let mut runners: Vec<Runner> = Vec::with_capacity(horses.len());
    for h in horses {
        let style = gi(h, "running_style");
        // Which of the ten aptitudes matter depends on the race and on this runner's own style, so
        // resolve them here rather than shipping all ten to the UI.
        let apt_ground = grade(match ground_code {
            2 => gi(h, "proper_ground_dirt"),
            _ => gi(h, "proper_ground_turf"),
        });
        let apt_distance = grade(match bracket {
            "Sprint" => gi(h, "proper_distance_short"),
            "Mile" => gi(h, "proper_distance_mile"),
            "Medium" => gi(h, "proper_distance_middle"),
            "Long" => gi(h, "proper_distance_long"),
            _ => 0,
        });
        let apt_style = grade(match style {
            1 => gi(h, "proper_running_style_nige"),
            2 => gi(h, "proper_running_style_senko"),
            3 => gi(h, "proper_running_style_sashi"),
            4 => gi(h, "proper_running_style_oikomi"),
            _ => 0,
        });
        let (speed, stamina, power, guts, wit) = (
            gi(h, "speed"),
            gi(h, "stamina"),
            gi(h, "pow"), // the wire spells power "pow" here and "power" on chara_info
            gi(h, "guts"),
            gi(h, "wiz"),
        );
        let vid = gi(h, "viewer_id");
        runners.push(Runner {
            frame: gi(h, "frame_order"),
            // Mob runners share chara_id 1 and have a mob_id instead; they have no name to show.
            name: match gi(h, "chara_id") {
                id if id > 1 => crate::names::chara_name_by_chara_id(id),
                _ => String::new(),
            },
            is_player: vid != 0 && vid == player_vid,
            speed,
            stamina,
            power,
            guts,
            wit,
            total: speed + stamina + power + guts + wit,
            style: style_name(style),
            popularity: gi(h, "popularity"),
            mood: gi(h, "motivation"),
            apt_ground,
            apt_distance,
            apt_style,
            skills: map_get(h, "skill_array").and_then(as_arr).map(|a| a.len()).unwrap_or(0),
        });
    }
    runners.sort_by(|a, b| b.total.cmp(&a.total));

    let player_rank = runners.iter().position(|r| r.is_player).map(|i| i + 1).unwrap_or(0);
    let player_frame = runners.iter().find(|r| r.is_player).map(|r| r.frame).unwrap_or(0);

    let field = Field {
        program_id,
        race_name,
        distance,
        distance_label: bracket,
        ground: if ground_code == 2 { "Dirt" } else { "Turf" },
        weather: weather_name(gi(rsi, "weather")),
        condition: condition_name(gi(rsi, "ground_condition")),
        field_size: runners.len(),
        player_frame,
        player_rank,
        runners,
    };
    // A DIFFERENT race is being entered → any decoded result on screen belongs to the previous one.
    // `race_scenario` (the result) only arrives at `race_start`, so between entering a race and it
    // loading there is genuinely no result yet, and showing the last one is worse than showing none.
    let is_new = LATEST
        .lock()
        .ok()
        .map(|l| l.as_ref().map(|f| f.program_id) != Some(field.program_id))
        .unwrap_or(true);
    if is_new {
        crate::ipc::clear_reveal();
    }
    if let Ok(mut l) = LATEST.lock() {
        *l = Some(field);
    }
}

/// The field as the web UI consumes it (`null` when no race is loaded).
pub fn json() -> serde_json::Value {
    match latest() {
        Some(f) => serde_json::to_value(f).unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LATEST is process-global (one game, one race); cargo runs tests on parallel threads.
    static TEST_LOCK: Mutex<()> = Mutex::new(());
    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        let g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        g
    }

    fn mp(json: &str) -> Value {
        fn conv(v: &serde_json::Value) -> Value {
            match v {
                serde_json::Value::Null => Value::Nil,
                serde_json::Value::Bool(b) => Value::Boolean(*b),
                serde_json::Value::Number(n) => n
                    .as_i64()
                    .map(Value::from)
                    .or_else(|| n.as_f64().map(Value::from))
                    .unwrap_or(Value::Nil),
                serde_json::Value::String(s) => Value::from(s.as_str()),
                serde_json::Value::Array(a) => Value::Array(a.iter().map(conv).collect()),
                serde_json::Value::Object(o) => {
                    Value::Map(o.iter().map(|(k, v)| (Value::from(k.as_str()), conv(v))).collect())
                }
            }
        }
        conv(&serde_json::from_str::<serde_json::Value>(json).expect("fixture is valid json"))
    }

    /// The Junior Make Debut from a real capture (program 1070, 1600 m turf), trimmed to three of
    /// the nine runners. Stats and aptitudes are verbatim; only the viewer ids are substituted —
    /// the dumper redacts EVERY id to the same string, while the wire gives the player a real one
    /// and NPCs 0, which is the distinction `race_reveal` has always keyed on.
    const DEBUT: &str = r#"{"race_start_info":{
      "program_id":1070,"weather":1,"ground_condition":1,"race_horse_data":[
      {"frame_order":8,"viewer_id":7777,"chara_id":1006,"speed":222,"stamina":137,"pow":261,
       "guts":146,"wiz":114,"running_style":3,"popularity":1,"motivation":4,
       "proper_ground_turf":7,"proper_ground_dirt":6,"proper_distance_short":3,
       "proper_distance_mile":7,"proper_distance_middle":7,"proper_distance_long":7,
       "proper_running_style_nige":4,"proper_running_style_senko":7,
       "proper_running_style_sashi":7,"proper_running_style_oikomi":4,
       "skill_array":[{"skill_id":100061,"level":1}]},
      {"frame_order":4,"viewer_id":0,"chara_id":1,"speed":77,"stamina":57,"pow":104,"guts":94,
       "wiz":100,"running_style":2,"popularity":8,"motivation":1,
       "proper_ground_turf":6,"proper_ground_dirt":1,"proper_distance_short":4,
       "proper_distance_mile":5,"proper_distance_middle":4,"proper_distance_long":2,
       "proper_running_style_nige":5,"proper_running_style_senko":6,
       "proper_running_style_sashi":5,"proper_running_style_oikomi":4,"skill_array":[]},
      {"frame_order":7,"viewer_id":0,"chara_id":1,"speed":86,"stamina":80,"pow":77,"guts":93,
       "wiz":37,"running_style":1,"popularity":9,"motivation":1,
       "proper_ground_turf":6,"proper_ground_dirt":1,"proper_distance_short":2,
       "proper_distance_mile":4,"proper_distance_middle":5,"proper_distance_long":4,
       "proper_running_style_nige":6,"proper_running_style_senko":5,
       "proper_running_style_sashi":4,"proper_running_style_oikomi":3,"skill_array":[]}]}}"#;

    #[test]
    fn decodes_a_real_entry_screen_field() {
        let _g = exclusive();
        note(&mp(DEBUT));
        let f = latest().expect("a field was published");

        assert_eq!(f.program_id, 1070);
        assert_eq!(f.race_name, "Junior Make Debut", "program id resolved via the bundled table");
        assert_eq!((f.distance, f.distance_label, f.ground), (1600, "Mile", "Turf"));
        assert_eq!((f.weather, f.condition), ("Sunny", "Firm"));
        assert_eq!(f.field_size, 3);

        // Strongest first: 880 / 432 / 373.
        let totals: Vec<i64> = f.runners.iter().map(|r| r.total).collect();
        assert_eq!(totals, vec![880, 432, 373]);

        let me = &f.runners[0];
        assert!(me.is_player);
        assert_eq!(me.name, "Oguri Cap");
        assert_eq!((f.player_rank, f.player_frame), (1, 8));
        assert_eq!(me.style, "Late", "running_style 3 is sashi");
        assert_eq!(me.skills, 1);
        assert_eq!(me.mood, 4);
    }

    /// The three aptitude columns must be picked for THIS race and THIS runner, not printed blindly:
    /// turf (ground 1), Mile (1600 m), and whichever style each runner is actually using.
    #[test]
    fn aptitudes_are_selected_for_the_race_and_the_runner() {
        let _g = exclusive();
        note(&mp(DEBUT));
        let f = latest().unwrap();

        // Player: turf 7=A, mile 7=A, sashi 7=A (NOT dirt 6, short 3 or nige 4).
        assert_eq!(
            (f.runners[0].apt_ground, f.runners[0].apt_distance, f.runners[0].apt_style),
            ("A", "A", "A")
        );
        // Second runner uses style 2 (senko) → its senko grade 6=B, turf 6=B, mile 5=C.
        assert_eq!(
            (f.runners[1].apt_ground, f.runners[1].apt_distance, f.runners[1].apt_style),
            ("B", "C", "B")
        );
        // Third uses style 1 (nige) → nige 6=B, mile 4=D.
        assert_eq!(
            (f.runners[2].apt_ground, f.runners[2].apt_distance, f.runners[2].apt_style),
            ("B", "D", "B")
        );
        // Mob runners share chara_id 1 and have no name to show.
        assert_eq!(f.runners[1].name, "");
        assert!(!f.runners[1].is_player);
    }

    /// An empty `race_start_info` rides along on ordinary turn responses. Treating it as a field
    /// would blank the panel the moment the entry screen refreshed anything.
    #[test]
    fn an_empty_race_start_info_does_not_clear_a_live_field() {
        let _g = exclusive();
        note(&mp(DEBUT));
        assert!(latest().is_some());

        note(&mp(r#"{"race_start_info":{"race_horse_data":[]},"chara_info":{"turn":12}}"#));
        note(&mp(r#"{"chara_info":{"turn":13}}"#));
        assert_eq!(latest().map(|f| f.program_id), Some(1070), "the live field survived");

        clear();
        assert!(latest().is_none(), "race_end retires it");
    }

    /// Entering a new race must retire the previous race's decoded RESULT. The result only exists
    /// from `race_start`; between entry and load there is none, and the panel used to keep showing
    /// the last race's finish — which is how a "1st" was displayed for a race that came 9th.
    #[test]
    fn entering_a_new_race_retires_the_previous_result() {
        let _g = exclusive();
        crate::ipc::set_reveal("{\"player_finish_label\":\"1st of 17\"}".into());
        note(&mp(DEBUT));
        assert!(
            crate::ipc::advice_cache().reveal_json.is_none(),
            "a new race must clear the old result"
        );

        // A fresh result for THIS race, then the same race re-served (the entry screen refreshes):
        // that must NOT wipe a result which genuinely belongs to it.
        crate::ipc::set_reveal("{\"player_finish_label\":\"9th of 18\"}".into());
        note(&mp(DEBUT));
        assert!(
            crate::ipc::advice_cache().reveal_json.is_some(),
            "re-serving the same race must keep its own result"
        );
        crate::ipc::clear_reveal();
    }

    /// A program the bundled table doesn't list (scenario-only races — 1 of the 11 in the capture)
    /// must still produce a usable field, with the id left for the caller to show.
    #[test]
    fn an_unlisted_program_still_yields_a_field() {
        let _g = exclusive();
        note(&mp(
            r#"{"race_start_info":{"program_id":1106,"weather":9,"ground_condition":9,
              "race_horse_data":[{"frame_order":1,"viewer_id":7777,"chara_id":1006,"speed":100,
              "stamina":100,"pow":100,"guts":100,"wiz":100,"running_style":1,"popularity":1,
              "motivation":3,"proper_ground_turf":8}]}}"#,
        ));
        let f = latest().expect("still published");
        assert_eq!(f.race_name, "");
        assert_eq!((f.distance, f.distance_label), (0, "?"));
        // Unknown weather/condition codes print as "?" rather than being guessed at.
        assert_eq!((f.weather, f.condition), ("?", "?"));
        assert_eq!(f.runners[0].total, 500);
        assert_eq!(f.runners[0].apt_distance, "?", "no distance means no distance aptitude");
    }
}
