//! Event REVEAL — what every choice gives you, decoded BEFORE you pick it.
//!
//! The server tells the client the outcome of every option up front. Two responses carry it:
//!
//!   1. any career response → `unchecked_event_array[0]` = the event about to play, carrying its
//!      `choice_array`. Each entry's `gain_select_id_index` is the value `check_event` expects back
//!      as `choice_number` — NOT `select_index`, which is not the button position (Icarus documents
//!      the trap in career_bot/events.py: story 400002445 has five choices all with select_index=2).
//!   2. `get_choice_reward` → `choice_reward_array` = the full reward table, one entry per option.
//!
//! (2) arrives ALONE — no chara_info, no home_info, not even the event id (35/35 in a full career
//! capture) — so the event identity has to come from the (1) we buffered. That capture never showed
//! an `unchecked_event_array` longer than one entry, nor more than one entry with choices, so "the
//! pending event that has choices" is unambiguous; and no response ever landed between the table and
//! the player's pick, so consuming the buffer on the next empty array cannot clear a live reveal.
//!
//! Timing from the same capture: the table lands a median 1.9 s before the pick, 0.66 s at the
//! fastest — and that was a player skipping at speed, so a decode has to be effectively free.
//!
//! Both entry points run on the game's NETWORK thread, out of `response_hook`'s single decoded-tree
//! fan-out, alongside `parse_account` and friends. That is affordable because neither one copies the
//! payload or touches the filesystem: they walk a handful of small arrays and publish behind a
//! mutex. The reward table is a response of a few hundred bytes, and the pending-event walk reads
//! one array of one entry. Nothing here may grow into work that belongs on the "ov-capture" worker —
//! if it does, hand it off the way `race_reveal` does.
//!
//! What this does NOT know: the table lists a button's outcome once per possible RESULT, and for
//! the 14% of options that have more than one, nothing on the wire identifies which will fire —
//! `gain_select_id_index` is the answer token, not a branch selector (replaying six such picks
//! against their observed stat deltas, the winner was the first listed row only twice). Those
//! options carry `certain: false` and are presented as a list of possibilities. The other 86% are
//! exact.
//!
//! Effect wording comes from the GAME's own choice-effect templates (text ids 1-40), so a row reads
//! the way the game would phrase it rather than the way we guessed. Ids 1/2/4/5/6 are additionally
//! confirmed against observed `chara_info` deltas; the rest render their template verbatim, and an
//! id we have no template for degrades to a plain "Effect #N" row instead of being dropped.

#![allow(dead_code)]

use std::sync::Mutex;

use once_cell::sync::Lazy;
use rmpv::Value;
use serde::Serialize;

use crate::msgpack::{as_arr, find_key, map_get};

/// The choice event the client is about to play, buffered so the (identity-free) reward table can be
/// attached to it.
#[derive(Clone, Default)]
struct Pending {
    event_id: i64,
    story_id: i64,
    support_card_id: i64,
    /// `gain_select_id_index` per option, in the order the game lists them (index 0 = button 1).
    answers: Vec<i64>,
    /// The whole `event_contents_info`, verbatim, for live inspection.
    raw: serde_json::Value,
    /// Trainee state as the event opened, so the outcome can be diffed when it closes.
    before: Snap,
    /// The reward table, kept so the resolved outcome can be matched back to a row.
    ///
    /// `(select_index, effects)` per row. The select_index is recorded because the row-selection
    /// rule is NOT yet settled: indexing rows by `gain_select_id_index` predicted correctly twice
    /// and wrongly once (story 830101001, where a 3-row table with gsii 2 resolved to row 3, not
    /// row 2). Without each row's own select_index that failure can't be diagnosed — which is
    /// exactly the mistake this field exists to stop repeating.
    table: Vec<(i64, Vec<(i64, i64, i64)>)>,
    /// `(select_index, gain_select_id_index)` per choice_array entry, verbatim.
    wire: Vec<(i64, i64)>,
}

/// The trainee fields an event can move. Compared before/after to identify which row fired.
#[derive(Clone, Default, PartialEq, Serialize)]
struct Snap {
    speed: i64,
    stamina: i64,
    power: i64,
    guts: i64,
    wiz: i64,
    vital: i64,
    motivation: i64,
    skill_point: i64,
    /// `group_id -> level` from `skill_tips_array`.
    tips: std::collections::BTreeMap<i64, i64>,
    /// `chara_effect_id_array` — the conditions she currently has.
    effects: Vec<i64>,
}

fn snap_of(ci: &Value) -> Snap {
    let gi = |k: &str| map_get(ci, k).and_then(|v| v.as_i64()).unwrap_or(0);
    Snap {
        speed: gi("speed"),
        stamina: gi("stamina"),
        power: gi("power"),
        guts: gi("guts"),
        wiz: gi("wiz"),
        vital: gi("vital"),
        motivation: gi("motivation"),
        skill_point: gi("skill_point"),
        tips: map_get(ci, "skill_tips_array")
            .and_then(as_arr)
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| {
                        Some((
                            map_get(r, "group_id").and_then(|v| v.as_i64())?,
                            map_get(r, "level").and_then(|v| v.as_i64()).unwrap_or(0),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        effects: map_get(ci, "chara_effect_id_array")
            .and_then(as_arr)
            .map(|rows| rows.iter().filter_map(|v| v.as_i64()).collect())
            .unwrap_or_default(),
    }
}

/// Find the trainee block in a response, if it carries one.
fn chara_of(val: &Value) -> Option<Snap> {
    let mut hits: Vec<&Value> = Vec::new();
    find_key(val, "chara_info", &mut hits);
    hits.into_iter().next().map(snap_of)
}

static PENDING: Lazy<Mutex<Option<Pending>>> = Lazy::new(|| Mutex::new(None));
static LATEST: Lazy<Mutex<Option<Reveal>>> = Lazy::new(|| Mutex::new(None));

/// One decoded outcome line ("Speed +15", "Sunny Days ○ hint lvl +1", …).
#[derive(Clone, Serialize)]
pub struct Effect {
    pub text: String,
    /// Coarse class for colouring: stat | energy | mood | sp | fans | bond | hint | condition | other.
    pub kind: &'static str,
    /// Signed magnitude for numeric effects (drives +/- colouring), 0 when the effect isn't a number.
    pub value: i64,
}

/// One possible result of taking an option.
#[derive(Clone, Serialize)]
pub struct Outcome {
    pub effects: Vec<Effect>,
    /// Speed/Stamina/Power/Guts/Wit + skill points, summed.
    pub stat_total: i64,
}

/// One option, as the player sees it on screen.
#[derive(Clone, Serialize)]
pub struct Opt {
    /// 1-based button position, top to bottom.
    pub position: usize,
    /// The wire answer (`gain_select_id_index`) `check_event` receives for this option.
    pub answer: i64,
    /// Every result the table lists for this button.
    pub outcomes: Vec<Outcome>,
    /// True when this button's group holds exactly ONE row — nothing was branched, so the outcome
    /// is flatly the table's.
    ///
    /// When false the button DOES branch and the server has already picked: `variant` names which
    /// of `variant_count` rows will fire. That is still a single prediction, just one resting on
    /// the within-group half of the rule rather than the (separately proven) grouping half.
    pub certain: bool,
    /// 1-based row within this button's group that the server selected, and the group's size.
    /// `variant == 0` means the option could not be resolved at all.
    pub variant: usize,
    pub variant_count: usize,
    /// Worst-case stat+SP total across `outcomes` (== the only total when `certain`). The ranking
    /// input, because a floor is the one comparison that cannot overpromise.
    pub stat_floor: i64,
    /// Best case, for showing the spread on a branching option.
    pub stat_ceiling: i64,
    /// Effects present in EVERY listed outcome — what taking this option gets you no matter how the
    /// game rolls. Identical to the single outcome's effects when `certain`.
    pub guaranteed: Vec<Effect>,
    /// Effects present in only SOME outcomes — the part that is actually at stake. Empty when
    /// `certain`. This is the useful half of a branching option: the branches of a status event
    /// typically differ by exactly one line (a condition gained or dodged), and a raw branch list
    /// buries it among the parts that never change.
    pub variable: Vec<Effect>,
}

#[derive(Clone, Serialize, Default)]
pub struct Reveal {
    pub event_id: i64,
    pub story_id: i64,
    /// Support card that owns the event ("" for trainee/scenario events).
    pub support_card: String,
    pub options: Vec<Opt>,
    /// Position of the single highest `stat_floor`; 0 when it's a tie or there's nothing to compare.
    pub best_position: usize,
    /// True when every option has exactly one listed outcome — i.e. the whole event is decided.
    pub all_certain: bool,
    /// The two source payloads, VERBATIM, for inspection on the Predictions page.
    ///
    /// Present because "which branch fires" is still unresolved and arguing about it from an old
    /// capture is slow: with these, any event on screen can be read field-by-field live. If a branch
    /// selector exists anywhere in what the server sends, it is in one of these two blobs.
    pub raw_choice_array: serde_json::Value,
    pub raw_reward_table: serde_json::Value,
}

/// msgpack → JSON, structure preserved, for the verbatim debug blobs above.
fn to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Nil => serde_json::Value::Null,
        Value::Boolean(b) => serde_json::Value::Bool(*b),
        Value::Integer(i) => i
            .as_i64()
            .map(serde_json::Value::from)
            .or_else(|| i.as_u64().map(serde_json::Value::from))
            .unwrap_or(serde_json::Value::Null),
        Value::F32(f) => serde_json::Value::from(*f),
        Value::F64(f) => serde_json::Value::from(*f),
        Value::String(s) => serde_json::Value::from(s.as_str().unwrap_or_default()),
        Value::Binary(b) => serde_json::Value::from(format!("<{} bytes>", b.len())),
        Value::Array(a) => serde_json::Value::Array(a.iter().map(to_json).collect()),
        Value::Map(m) => serde_json::Value::Object(
            m.iter()
                .map(|(k, val)| {
                    (
                        k.as_str().map(str::to_string).unwrap_or_else(|| format!("{k}")),
                        to_json(val),
                    )
                })
                .collect(),
        ),
        Value::Ext(t, b) => serde_json::Value::from(format!("<ext {t}, {} bytes>", b.len())),
    }
}

/// The live reveal for the event on screen, or None when no choice event is pending.
pub fn latest() -> Option<Reveal> {
    LATEST.lock().ok().and_then(|l| l.clone())
}

/// `StoryChoiceController._selectedIndex`, pushed from the IL2CPP pump while a choice is up.
///
/// The recorder needs to know WHICH option produced an outcome. Deriving that from the row that
/// fired is circular — it assumes the very rule under test — so the button index is taken from the
/// game directly. -1 until a choice screen sets it.
static SELECTED: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

pub fn note_selected_index(i: i32) {
    SELECTED.store(i, std::sync::atomic::Ordering::Relaxed);
}

/// Drop any pending event + published reveal (career end, or the master switch going off).
pub fn clear() {
    if let Ok(mut p) = PENDING.lock() {
        *p = None;
    }
    if let Ok(mut l) = LATEST.lock() {
        *l = None;
    }
}

// ── param targets ───────────────────────────────────────────────────────────────────────────────
// `effect_value_0` for the param templates. Matches `target_type` in the training tiles'
// `params_inc_dec_info_array`, except that training reports energy as 30 and skill points aren't in
// it at all — so the two tables are kept separate rather than shared.
fn param_label(target: i64) -> Option<(&'static str, &'static str)> {
    Some(match target {
        1 => ("Speed", "stat"),
        2 => ("Stamina", "stat"),
        3 => ("Power", "stat"),
        4 => ("Guts", "stat"),
        5 => ("Wit", "stat"),
        10 => ("Energy", "energy"),
        20 => ("Mood", "mood"),
        30 => ("Skill points", "sp"),
        _ => return None,
    })
}

/// Conditions we can name. Anything else prints its id — a wrong name would be worse than a number
/// here, because the whole point of the panel is that you can trust what it says.
///
/// 1-6 are the bad ones, from Icarus's `BAD_EFFECT_NAMES`. 10 is derived from two independent
/// sources agreeing: the capture shows "Get Well Soon!" granting condition 10, and a screenshot of
/// that same event's branch list shows its good outcome as "Become Practice Perfect ○" — and the
/// capture also shows a later run of the event replacing 10 with 6 (Practice Poor), which is exactly
/// the good/bad pairing you would expect.
fn condition_label(id: i64) -> String {
    match id {
        1 => "Night Owl".into(),
        2 => "Slacker".into(),
        3 => "Skin Outbreak".into(),
        4 => "Slow Metabolism".into(),
        5 => "Migraine".into(),
        6 => "Practice Poor".into(),
        10 => "Practice Perfect ○".into(),
        n => format!("condition #{n}"),
    }
}

/// A character name for a bond/recreation slot: bare chara id first, then the scenario group ids
/// (9002/9008 and friends) which have no character behind them at all.
fn who(id: i64) -> String {
    let n = crate::names::chara_name_by_chara_id(id);
    if !n.is_empty() {
        return n;
    }
    let n = crate::names::chara_name(id);
    if !n.is_empty() {
        return n;
    }
    format!("#{id}")
}

/// Render ONE `gain_param_array` row using the game's own choice-effect template for its
/// `display_id`. `v0`/`v1`/`v2` are `effect_value_0..2` verbatim.
///
/// The template set is the game's text ids 1-40. Where a slot holds an id rather than a number it is
/// resolved to a name (stat, character, skill, condition); where it holds a number it is formatted
/// as the game formats it. An unmodelled id still produces a row — the panel must never imply an
/// option is free of effects just because we don't recognise one of them.
fn effect(display_id: i64, v0: i64, v1: i64, _v2: i64) -> Effect {
    let plain = |text: String, kind: &'static str, value: i64| Effect { text, kind, value };
    match display_id {
        // "{0} +{1}" / "{0} -{1}" — the workhorses: a param and a signed amount.
        1 | 2 | 23 => {
            let sign = if display_id == 2 { -1 } else { 1 };
            match param_label(v0) {
                Some((label, kind)) => {
                    plain(format!("{label} {:+}", sign * v1), kind, sign * v1)
                }
                None => plain(format!("param #{v0} {:+}", sign * v1), "other", sign * v1),
            }
        }
        // "+{0} Fans"
        3 => plain(format!("Fans +{v0}"), "fans", v0),
        // "Friendship with {0} +{1}" (5 adds the not-in-deck caveat; 35/36 are the -{1} forms).
        4 | 5 | 35 | 36 => {
            let sign = if display_id >= 35 { -1 } else { 1 };
            plain(format!("Bond with {} {:+}", who(v0), sign * v1), "bond", sign * v1)
        }
        // "hint lvl +{0}" — the subject is the skill in effect_value_0, the level is effect_value_1.
        6 => {
            let name = crate::career::skill_name(v0);
            let skill = if name.is_empty() { format!("skill #{v0}") } else { name };
            plain(format!("{skill} hint lvl +{v1}"), "hint", v1)
        }
        // "Gain" (an item) / "Chance to gain a random skill"
        7 => plain("Gain an item".into(), "other", 0),
        8 => plain("Chance to gain a random skill".into(), "other", 0),
        // "Become {0}" / "Cures {0}"
        9 | 37 => plain(format!("Become {}", condition_label(v0)), "condition", 0),
        40 => plain(
            format!("Become {} (unless prevented)", condition_label(v0)),
            "condition",
            0,
        ),
        10 => plain(format!("Cures {}", condition_label(v0)), "condition", 0),
        // "Unlock recreation with {0}" / "End this Support Card's chain event"
        11 => plain(format!("Unlock recreation with {}", who(v0)), "other", 0),
        12 => plain("Ends this Support Card's chain event".into(), "other", 0),
        // "All attributes +{0}" / "Random {0} attribute(s) ±{1}"
        13 => plain(format!("All attributes +{v0}"), "stat", v0 * 5),
        14 => plain(format!("{v0} random attribute(s) +{v1}"), "stat", v0 * v1),
        34 => plain(format!("{v0} random attribute(s) -{v1}"), "stat", -(v0 * v1)),
        // "Cures all bad conditions" / "Randomly cures {0} bad condition(s)"
        15 => plain("Cures all bad conditions".into(), "condition", 0),
        16 => plain(format!("Randomly cures {v0} bad condition(s)"), "condition", 0),
        // "Restrict {0} training" / "Restrict race entry"
        17 => plain(
            format!(
                "Restricts {} training",
                param_label(v0).map(|(l, _)| l.to_string()).unwrap_or_else(|| format!("#{v0}"))
            ),
            "other",
            0,
        ),
        18 => plain("Restricts race entry".into(), "other", 0),
        // "Previously trained attribute ±{0}"
        19 => plain(format!("Previously trained attribute +{v0}"), "stat", v0),
        20 => plain(format!("Previously trained attribute -{v0}"), "stat", -v0),
        // "Stat gains based on race grade [and result]"
        21 => plain("Stat gains based on race grade".into(), "stat", 0),
        22 => plain("Stat gains based on race grade and result".into(), "stat", 0),
        // Friendship-with-lowest variants (27-30). The slot holding the amount moves between them,
        // so take the last non-zero value rather than hard-coding a position per id.
        27..=30 => {
            let amount = [_v2, v1, v0].into_iter().find(|n| *n != 0).unwrap_or(0);
            plain(
                format!("Bond with lowest-friendship support card(s) +{amount}"),
                "bond",
                amount,
            )
        }
        // "{0} ±{1} (Prevented by {2})"
        38 | 39 => {
            let sign = if display_id == 38 { -1 } else { 1 };
            let label = param_label(v0).map(|(l, _)| l.to_string()).unwrap_or_else(|| format!("param #{v0}"));
            plain(format!("{label} {:+} (unless prevented)", sign * v1), "other", sign * v1)
        }
        n => plain(format!("Effect #{n} ({v0}, {v1})"), "other", 0),
    }
}

/// Buffer the choice event the client is about to play, and retire a reveal whose event is gone.
///
/// Called for every response that carries an `unchecked_event_array`. An array with no choice-
/// bearing entry means the pending event has been answered (or none was ever pending), which is the
/// signal to drop the published reveal — verified safe because no response arrives between the
/// reward table and the player's pick.
pub fn note_events(val: &Value) {
    let mut hits: Vec<&Value> = Vec::new();
    find_key(val, "unchecked_event_array", &mut hits);
    let Some(arr) = hits.into_iter().find_map(as_arr) else { return };

    let found = arr.iter().find_map(|ev| {
        let info = map_get(ev, "event_contents_info")?;
        let choices = map_get(info, "choice_array").and_then(as_arr)?;
        if choices.is_empty() {
            return None;
        }
        Some(Pending {
            event_id: map_get(ev, "event_id").and_then(|v| v.as_i64()).unwrap_or(0),
            story_id: map_get(ev, "story_id").and_then(|v| v.as_i64()).unwrap_or(0),
            support_card_id: map_get(info, "support_card_id").and_then(|v| v.as_i64()).unwrap_or(0),
            answers: choices
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    // gain_select_id_index is the wire answer; select_index is NOT the button
                    // position. Fall back to the 1-based position only when both are absent.
                    map_get(c, "gain_select_id_index")
                        .and_then(|v| v.as_i64())
                        .filter(|n| *n > 0)
                        .unwrap_or(i as i64 + 1)
                })
                .collect(),
            raw: to_json(info),
            before: chara_of(val).unwrap_or_default(),
            table: Vec::new(), // filled by note_choice_rewards
            wire: choices
                .iter()
                .map(|c| {
                    (
                        map_get(c, "select_index").and_then(|v| v.as_i64()).unwrap_or(0),
                        map_get(c, "gain_select_id_index").and_then(|v| v.as_i64()).unwrap_or(0),
                    )
                })
                .collect(),
        })
    });

    let Ok(mut pending) = PENDING.lock() else { return };
    match found {
        Some(mut p) => {
            let same = pending.as_ref().map(|q| q.event_id) == Some(p.event_id);
            if !same {
                // A different event → whatever is published describes the previous one.
                if let Ok(mut l) = LATEST.lock() {
                    *l = None;
                }
            } else if let Some(q) = pending.as_ref() {
                // The SAME event, re-served. The game re-sends a pending event on essentially every
                // response while it is on screen, and blindly replacing the buffer here wiped both
                // the reward table (fetched once, by a response that carries nothing else) and the
                // opening stat snapshot — so by the time the player answered there was nothing left
                // to attribute the outcome to, and no resolution was ever recorded. Carry them over.
                p.table = q.table.clone();
                p.before = q.before.clone();
            }
            *pending = Some(p);
        }
        None => {
            // The event is gone from the queue → it has just been ANSWERED, and this very response
            // carries the resulting chara_info. That is everything needed to identify which row of
            // the table actually fired, with no request hook and no external dumper.
            if let Some(p) = pending.take() {
                record_resolution(&p, val);
            }
            if let Ok(mut l) = LATEST.lock() {
                *l = None;
            }
        }
    }
}

/// Which table row fired, worked out from the trainee's own before/after state, and appended to
/// `overseer-logs/event-outcomes.jsonl`.
///
/// This is the whole point: the server does not label the winning row, but the RESULT is observable,
/// and the wire fields that might select it (`select_index` / `gain_select_id_index` per option) are
/// recorded alongside. A few careers of these rows either reveal the rule — at which point branches
/// become exact predictions — or prove there isn't one. Either way it settles the question with data
/// rather than inference over six historical samples.
fn record_resolution(p: &Pending, val: &Value) {
    let Some(after) = chara_of(val) else {
        crate::tools::log("[event-outcome] skipped: no chara_info in the closing response");
        return;
    };
    if p.table.is_empty() {
        crate::tools::log(&format!(
            "[event-outcome] skipped story {}: no reward table was captured",
            p.story_id
        ));
        return;
    }
    if after == p.before {
        crate::tools::log(&format!(
            "[event-outcome] skipped story {}: nothing observable changed",
            p.story_id
        ));
        return;
    }
    let b = &p.before;
    // Observed deltas, in the same vocabulary the table uses.
    let delta = serde_json::json!({
        "speed": after.speed - b.speed,
        "stamina": after.stamina - b.stamina,
        "power": after.power - b.power,
        "guts": after.guts - b.guts,
        "wiz": after.wiz - b.wiz,
        "vital": after.vital - b.vital,
        "motivation": after.motivation - b.motivation,
        "skill_point": after.skill_point - b.skill_point,
        "tips_gained": after
            .tips
            .iter()
            .filter(|(g, lv)| b.tips.get(g).copied().unwrap_or(0) < **lv)
            .map(|(g, lv)| (g.to_string(), lv - b.tips.get(g).copied().unwrap_or(0)))
            .collect::<std::collections::BTreeMap<_, _>>(),
        "effects_gained": after.effects.iter().filter(|e| !b.effects.contains(e)).collect::<Vec<_>>(),
        "effects_lost": b.effects.iter().filter(|e| !after.effects.contains(e)).collect::<Vec<_>>(),
    });

    let rec = serde_json::json!({
        "event_id": p.event_id,
        "story_id": p.story_id,
        "support_card_id": p.support_card_id,
        // The candidate wire selectors, verbatim — the whole reason to collect this.
        "choice_array": p.wire.iter().map(|(si, g)| serde_json::json!({"select_index": si, "gain_select_id_index": g})).collect::<Vec<_>>(),
        // The WHOLE served event, every field, unfiltered.
        //
        // Recording only the two fields we already believed in is how the branch question stayed
        // open: measured over 12 resolvable multi-branch picks, `select_index` names the right
        // branch 10 times, and the two misses both fired branch 2 while it said 1. Either the
        // selector is a field we never looked at, or there isn't one and those options are a genuine
        // roll — and there is no way to tell those apart from a log that only kept the fields the
        // current theory uses. This is the same mistake as reconstructing `select_index` in the old
        // test fixtures: never let the hypothesis decide what gets written down.
        "raw_event_info": p.raw,
        // Every row of the table, so the winner can be resolved offline against `delta`.
        "table": p.table.iter().map(|(si, row)| serde_json::json!({
            "select_index": si,
            "effects": row.iter().map(|(d, a, bb)| serde_json::json!([d, a, bb])).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "delta": delta,
        "before_effects": b.effects,
        "before_vital": b.vital,
        "before_motivation": b.motivation,
        // Which button the player actually took (0-based), straight from the controller. Without
        // this the log cannot be analysed without assuming the rule being tested.
        "selected_index": SELECTED.swap(-1, std::sync::atomic::Ordering::Relaxed),
    });

    // Off the hot path in spirit: one short append, no read-modify-write, and a failure is silent —
    // a diagnostic log must never disturb the response path.
    let path = crate::paths::log_file("event-outcomes.jsonl");
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            use std::io::Write;
            if let Err(e) = writeln!(f, "{rec}") {
                crate::tools::log(&format!("[event-outcome] write failed: {e}"));
            } else {
                crate::tools::log(&format!(
                    "[event-outcome] recorded story {} ({} rows offered)",
                    p.story_id,
                    p.table.len()
                ));
            }
        }
        Err(e) => crate::tools::log(&format!("[event-outcome] open failed: {e}")),
    }
}

/// Decode a `choice_reward_array` against the buffered event and publish the reveal.
///
/// The table's `select_index` groups rows into on-screen buttons — one group per button, one row per
/// possible result of it. The buffered event's `choice_array` runs in the same button order and
/// names, per button, both which group it is and which row of that group the server has already
/// selected. See the rule and its evidence at the option builder below.
pub fn note_choice_rewards(val: &Value) {
    let mut hits: Vec<&Value> = Vec::new();
    find_key(val, "choice_reward_array", &mut hits);
    let Some(rows) = hits.into_iter().find_map(as_arr) else { return };
    if rows.is_empty() {
        return;
    }
    // Keep the table on the pending event so the resolution recorder can match against it.
    if let Ok(mut g) = PENDING.lock() {
        if let Some(p) = g.as_mut() {
            p.table = rows
                .iter()
                .map(|r| {
                    let si = map_get(r, "select_index").and_then(|v| v.as_i64()).unwrap_or(0);
                    let effects = map_get(r, "gain_param_array")
                        .and_then(as_arr)
                        .map(|gs| {
                            gs.iter()
                                .map(|x| {
                                    let gi = |k: &str| map_get(x, k).and_then(|v| v.as_i64()).unwrap_or(0);
                                    (gi("display_id"), gi("effect_value_0"), gi("effect_value_1"))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    (si, effects)
                })
                .collect();
        }
    }
    let pending = PENDING.lock().ok().and_then(|p| p.clone());

    // Decode every row once, in table order, keeping each row's own `select_index` beside it —
    // the grouping below is built from it.
    let decoded: Vec<(i64, Outcome)> = rows
        .iter()
        .map(|row| {
            let si = map_get(row, "select_index").and_then(|v| v.as_i64()).unwrap_or(0);
            let mut effects: Vec<Effect> = Vec::new();
            let mut stat_total = 0i64;
            if let Some(gains) = map_get(row, "gain_param_array").and_then(as_arr) {
                for g in gains {
                    let gi = |k: &str| map_get(g, k).and_then(|v| v.as_i64()).unwrap_or(0);
                    let e = effect(gi("display_id"), gi("effect_value_0"), gi("effect_value_1"), gi("effect_value_2"));
                    if matches!(e.kind, "stat" | "sp") {
                        stat_total += e.value;
                    }
                    effects.push(e);
                }
            }
            (si, Outcome { effects, stat_total })
        })
        .collect();

    // The table's `select_index` GROUPS rows into on-screen buttons: every row sharing a value is a
    // possible result of the same button, and the distinct values in first-appearance order are the
    // buttons top to bottom.
    let mut groups: Vec<i64> = Vec::new();
    for (si, _) in &decoded {
        if !groups.contains(si) {
            groups.push(*si);
        }
    }

    // One option per `choice_array` entry = one BUTTON, top to bottom. Each entry names its row in
    // two steps:
    //
    //   group = the `gain_select_id_index`-th distinct select_index  → which button
    //   row   = the `select_index`-th row inside that group          → which branch of it
    //
    // Both halves are read from the option's OWN choice_array entry, and both come from the outcome
    // recorder rather than from theory. The previous rule (`gsii + si − 1` over the flat table)
    // ignored the grouping entirely, and a live two-button event falsified it outright: its table
    // was [si 1, si 1, si 2] and the player's second button paid Energy +5, which is the si-2 row —
    // the flat rule pointed at the second si-1 row (Energy −5 plus a cure) instead. Re-scored over
    // every resolvable record with an explicit `selected_index`, grouping is 15/15 against the flat
    // rule's 14/15, and 10/10 versus 9/10 on the subset where exactly one row can explain the
    // observed delta.
    //
    // The two halves rest on different amounts of evidence, and `certain` reflects that. Grouping is
    // settled: that live event turns on it. The within-group step is not isolated yet — no recorded
    // pick has landed on a multi-row group whose outcome distinguishes the candidates — so it is
    // taken from `select_index` because that is the only reading in which the field carries
    // information at all (`gain_select_id_index` is the button ordinal in all 20 grouped records,
    // so it cannot be naming the branch). Every option answered keeps auditing both halves.
    let wire: Vec<(i64, i64)> = match pending.as_ref() {
        Some(p) if !p.wire.is_empty() => p.wire.clone(),
        // No buffered event (table seen before any serving): one option per group, first row each.
        _ => (1..=groups.len() as i64).map(|g| (1, g)).collect(),
    };
    let options: Vec<Opt> = wire
        .iter()
        .enumerate()
        .filter_map(|(i, &(si, gsii))| {
            // A group the table does not have means we cannot say, so the option is dropped rather
            // than shown against someone else's outcome.
            let key = *groups.get(usize::try_from(gsii).ok()?.checked_sub(1)?)?;
            let members: Vec<&Outcome> =
                decoded.iter().filter(|(s, _)| *s == key).map(|(_, o)| o).collect();
            if members.is_empty() {
                return None;
            }
            // Clamp rather than drop: a group is sometimes shorter than its select_index (chain
            // events reuse one table across occurrences), and the first row is a far better answer
            // than no card at all.
            let pick = usize::try_from(si).unwrap_or(1).clamp(1, members.len());
            let outcome = members[pick - 1].clone();
            // What the branches this button did NOT take would have added — the part actually at
            // stake, which is what makes a branching option worth flagging.
            let variable: Vec<Effect> = members
                .iter()
                .enumerate()
                .filter(|(n, _)| *n != pick - 1)
                .flat_map(|(_, o)| o.effects.iter())
                .filter(|e| !outcome.effects.iter().any(|k| k.text == e.text))
                .cloned()
                .collect();
            Some(Opt {
                position: i + 1,
                answer: gsii,
                stat_floor: outcome.stat_total,
                stat_ceiling: outcome.stat_total,
                guaranteed: outcome.effects.clone(),
                variable,
                certain: members.len() == 1,
                variant: pick,
                variant_count: members.len(),
                outcomes: vec![outcome],
            })
        })
        .collect();

    // "Most stats" only when there is something to compare AND one option leads outright — a tie
    // marks nothing, and badging the only button of a single-option event says nothing at all.
    let best = options.iter().max_by_key(|o| o.stat_floor);
    let best_position = match best {
        Some(b)
            if options.len() >= 2
                && options.iter().filter(|o| o.stat_floor == b.stat_floor).count() == 1 =>
        {
            b.position
        }
        _ => 0,
    };

    let reveal = Reveal {
        event_id: pending.as_ref().map(|p| p.event_id).unwrap_or(0),
        story_id: pending.as_ref().map(|p| p.story_id).unwrap_or(0),
        support_card: pending
            .as_ref()
            .map(|p| p.support_card_id)
            .filter(|id| *id > 0)
            .map(crate::names::support_name)
            .unwrap_or_default(),
        all_certain: options.iter().all(|o| o.certain),
        options,
        best_position,
        raw_choice_array: pending
            .as_ref()
            .map(|p| p.raw.clone())
            .unwrap_or(serde_json::Value::Null),
        raw_reward_table: serde_json::Value::Array(rows.iter().map(to_json).collect()),
    };
    if let Ok(mut l) = LATEST.lock() {
        *l = Some(reveal);
    }
}

/// The reveal as the web UI's Predictions page consumes it (`null` when nothing is pending).
pub fn json() -> serde_json::Value {
    match latest() {
        Some(r) => serde_json::to_value(r).unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PENDING/LATEST are process-global (one game, one event on screen), so the tests that drive
    /// them have to run one at a time — cargo runs them on parallel threads by default. Poisoning is
    /// ignored: a panicking test has already failed, and blocking the rest on it helps nobody.
    static TEST_LOCK: Mutex<()> = Mutex::new(());
    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        let g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        super::clear(); // start from a known-empty state, not the previous test's leftovers
        g
    }

    /// Payloads below are pasted VERBATIM out of a Navigator capture (2026-07-22 career), so these
    /// assert against bytes the live server actually sent rather than against our own model of it.
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

    fn texts(o: &Opt, outcome: usize) -> Vec<String> {
        o.outcomes[outcome].effects.iter().map(|e| e.text.clone()).collect()
    }

    /// Event 10002 ("With Passion and Joy!", El Condor Pasa's SR). The player took choice 2 and the
    /// follow-up chara_info moved Speed +15 / Power +10 / Energy -10 — exactly what the table's
    /// second button says, which is the whole claim this module makes.
    ///
    /// The bond row also pins the id spaces apart: the target is a BARE chara id (1014), resolved
    /// through the card table's 6-digit keys, while the event's `support_card_id` (20033) is a
    /// support-card id in a different table entirely.
    #[test]
    fn decodes_a_two_option_event_the_way_the_server_resolved_it() {
        let _g = exclusive();
        note_events(&mp(
            r#"{"unchecked_event_array":[{"event_id":10002,"story_id":820033001,
                "event_contents_info":{"support_card_id":20033,"choice_array":[
                  {"select_index":1,"gain_select_id_index":1},
                  {"select_index":1,"gain_select_id_index":2}]}}]}"#,
        ));
        note_choice_rewards(&mp(
            r#"{"choice_reward_array":[
              {"select_index":1,"gain_param_array":[
                {"display_id":4,"effect_value_0":1014,"effect_value_1":5,"effect_value_2":0},
                {"display_id":1,"effect_value_0":10,"effect_value_1":10,"effect_value_2":0},
                {"display_id":1,"effect_value_0":1,"effect_value_1":5,"effect_value_2":0},
                {"display_id":1,"effect_value_0":3,"effect_value_1":5,"effect_value_2":0}]},
              {"select_index":2,"gain_param_array":[
                {"display_id":4,"effect_value_0":1014,"effect_value_1":5,"effect_value_2":0},
                {"display_id":2,"effect_value_0":10,"effect_value_1":10,"effect_value_2":0},
                {"display_id":1,"effect_value_0":1,"effect_value_1":15,"effect_value_2":0},
                {"display_id":1,"effect_value_0":3,"effect_value_1":10,"effect_value_2":0}]}]}"#,
        ));

        let r = latest().expect("a reveal was published");
        assert_eq!(r.event_id, 10002);
        assert_eq!(r.options.len(), 2);
        assert!(r.all_certain, "both buttons list exactly one outcome");

        // Button 1: Energy +10, Speed +5, Power +5 → stat floor 10 (energy is not a stat).
        let one = &r.options[0];
        assert_eq!((one.position, one.answer, one.certain), (1, 1, true));
        assert_eq!(
            texts(one, 0),
            ["Bond with El Condor Pasa +5", "Energy +10", "Speed +5", "Power +5"]
        );
        assert_eq!(one.stat_floor, 10);

        // Button 2: the one the player took — Speed +15 / Power +10 / Energy -10.
        let two = &r.options[1];
        assert_eq!((two.position, two.answer, two.certain), (2, 2, true));
        assert_eq!(
            texts(two, 0),
            ["Bond with El Condor Pasa +5", "Energy -10", "Speed +15", "Power +10"]
        );
        assert_eq!(two.stat_floor, 25);
        assert_eq!(r.best_position, 2, "button 2 leads on raw stats");
    }

    /// THE FALSIFIER (recorder record 27, story 501068715, verbatim). This event is the reason the
    /// flat `gsii + si − 1` lookup is gone: its table is [si 1, si 1, si 2], the player took the
    /// SECOND button, and the trainee's energy went UP 5 — which is the si-2 row. The flat rule
    /// pointed at the second si-1 row instead (Energy −5 plus a cure), i.e. it named a row of the
    /// wrong button and got the sign of the only visible number backwards.
    ///
    /// Note both buttons carry `select_index: 1` here. That does NOT collapse them into one option:
    /// `select_index` on a choice_array entry is the branch WITHIN a button, and the button itself
    /// comes from `gain_select_id_index`.
    #[test]
    fn the_buttons_group_comes_from_gain_select_id_index() {
        let _g = exclusive();
        note_events(&mp(
            r#"{"chara_info":{"speed":100},"unchecked_event_array":[{"event_id":7017,
                "story_id":501068715,"event_contents_info":{"support_card_id":0,"choice_array":[
                  {"select_index":1,"gain_select_id_index":1},
                  {"select_index":1,"gain_select_id_index":2}]}}]}"#,
        ));
        note_choice_rewards(&mp(
            r#"{"choice_reward_array":[
              {"select_index":1,"gain_param_array":[
                {"display_id":2,"effect_value_0":10,"effect_value_1":5,"effect_value_2":0},
                {"display_id":4,"effect_value_0":9002,"effect_value_1":5,"effect_value_2":0},
                {"display_id":19,"effect_value_0":5,"effect_value_1":0,"effect_value_2":0}]},
              {"select_index":1,"gain_param_array":[
                {"display_id":2,"effect_value_0":10,"effect_value_1":5,"effect_value_2":0},
                {"display_id":4,"effect_value_0":9002,"effect_value_1":5,"effect_value_2":0},
                {"display_id":19,"effect_value_0":5,"effect_value_1":0,"effect_value_2":0},
                {"display_id":16,"effect_value_0":1,"effect_value_1":0,"effect_value_2":0}]},
              {"select_index":2,"gain_param_array":[
                {"display_id":1,"effect_value_0":10,"effect_value_1":5,"effect_value_2":0}]}]}"#,
        ));

        let r = latest().expect("published");
        assert_eq!(r.options.len(), 2, "two choice_array entries -> two buttons");

        // Button 1 is the two-row group: it branches, and what is at stake is the cure.
        let one = &r.options[0];
        assert_eq!((one.variant, one.variant_count, one.certain), (1, 2, false));
        assert!(one.guaranteed.iter().any(|e| e.text == "Energy -5"));
        assert!(one.guaranteed.iter().any(|e| e.text == "Previously trained attribute +5"));
        assert_eq!(
            one.variable.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            ["Randomly cures 1 bad condition(s)"],
            "the sibling row's extra line is what this button was rolling for"
        );

        // Button 2 is the lone si-2 row. This is the assertion the old rule failed.
        let two = &r.options[1];
        assert_eq!((two.variant, two.variant_count, two.certain), (1, 1, true));
        assert_eq!(
            two.guaranteed.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            ["Energy +5"],
            "the player took this button and gained 5 energy"
        );
        assert!(
            !two.guaranteed.iter().any(|e| e.text.contains("cures")),
            "button 1's branch must not leak onto button 2"
        );
    }

    /// Recorder record 13 (story 501068515, verbatim): four rows, TWO to a button. The player took
    /// the second button and gained Guts +20 and nothing else — the si-2 group — while the old flat
    /// rule named row 2 (Stamina +10 / Wit +10), a row belonging to the other button entirely.
    ///
    /// Both buttons branch here, and both branch over the same thing: whether condition 8 is applied
    /// on top. That is the shape `variable` exists to surface.
    #[test]
    fn every_button_gets_its_own_group_when_several_rows_share_one() {
        let _g = exclusive();
        note_events(&mp(
            r#"{"chara_info":{"speed":100},"unchecked_event_array":[{"event_id":3112,
                "story_id":501068515,"event_contents_info":{"support_card_id":0,"choice_array":[
                  {"select_index":1,"gain_select_id_index":1},
                  {"select_index":1,"gain_select_id_index":2}]}}]}"#,
        ));
        note_choice_rewards(&mp(
            r#"{"choice_reward_array":[
              {"select_index":1,"gain_param_array":[
                {"display_id":1,"effect_value_0":2,"effect_value_1":10,"effect_value_2":0},
                {"display_id":1,"effect_value_0":5,"effect_value_1":10,"effect_value_2":0}]},
              {"select_index":1,"gain_param_array":[
                {"display_id":1,"effect_value_0":2,"effect_value_1":10,"effect_value_2":0},
                {"display_id":1,"effect_value_0":5,"effect_value_1":10,"effect_value_2":0},
                {"display_id":9,"effect_value_0":8,"effect_value_1":0,"effect_value_2":0}]},
              {"select_index":2,"gain_param_array":[
                {"display_id":1,"effect_value_0":4,"effect_value_1":20,"effect_value_2":0}]},
              {"select_index":2,"gain_param_array":[
                {"display_id":1,"effect_value_0":4,"effect_value_1":20,"effect_value_2":0},
                {"display_id":9,"effect_value_0":8,"effect_value_1":0,"effect_value_2":0}]}]}"#,
        ));

        let r = latest().expect("published");
        assert_eq!(r.options.len(), 2);
        assert!(!r.all_certain, "both buttons branch");
        assert_eq!(
            r.options[0].guaranteed.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            ["Stamina +10", "Wit +10"]
        );
        // The observed outcome: Guts +20 alone.
        assert_eq!(
            r.options[1].guaranteed.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            ["Guts +20"]
        );
        assert_eq!(r.options[1].stat_floor, 20);
        assert_eq!((r.options[1].variant, r.options[1].variant_count), (1, 2));
        assert_eq!(
            r.options[1].variable.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            ["Become condition #8"],
            "the branch this button dodged"
        );
        assert_eq!(r.best_position, 0, "Stamina+Wit 20 ties Guts 20 -> no badge");
    }

    /// Get Well Soon! (recorder record 23, story 501068713, verbatim): five rows for two buttons —
    /// two under si 1, three under si 2. A group holds every result the button could have had, so
    /// only the branch `select_index` names is live and the siblings must not leak into the display
    /// as extra effects.
    ///
    /// The player took the FIRST button; motivation fell 1 and one attribute fell 5, which is row 1
    /// exactly — and notably NOT row 2, whose Practice Poor would have been invisible here anyway
    /// (they already had conditions 8 and 9, though not 6).
    #[test]
    fn a_groups_sibling_rows_do_not_leak_into_the_button() {
        let _g = exclusive();
        note_events(&mp(
            r#"{"chara_info":{"speed":100},"unchecked_event_array":[{"event_id":7014,
                "story_id":501068713,"event_contents_info":{"support_card_id":0,"choice_array":[
                  {"select_index":1,"gain_select_id_index":1},
                  {"select_index":1,"gain_select_id_index":2}]}}]}"#,
        ));
        note_choice_rewards(&mp(
            r#"{"choice_reward_array":[
              {"select_index":1,"gain_param_array":[
                {"display_id":2,"effect_value_0":20,"effect_value_1":1,"effect_value_2":0},
                {"display_id":20,"effect_value_0":5,"effect_value_1":0,"effect_value_2":0}]},
              {"select_index":1,"gain_param_array":[
                {"display_id":2,"effect_value_0":20,"effect_value_1":1,"effect_value_2":0},
                {"display_id":37,"effect_value_0":6,"effect_value_1":0,"effect_value_2":0},
                {"display_id":20,"effect_value_0":5,"effect_value_1":0,"effect_value_2":0}]},
              {"select_index":2,"gain_param_array":[
                {"display_id":2,"effect_value_0":20,"effect_value_1":1,"effect_value_2":0},
                {"display_id":20,"effect_value_0":10,"effect_value_1":0,"effect_value_2":0}]},
              {"select_index":2,"gain_param_array":[
                {"display_id":20,"effect_value_0":10,"effect_value_1":0,"effect_value_2":0},
                {"display_id":37,"effect_value_0":6,"effect_value_1":0,"effect_value_2":0},
                {"display_id":2,"effect_value_0":20,"effect_value_1":1,"effect_value_2":0}]},
              {"select_index":2,"gain_param_array":[
                {"display_id":9,"effect_value_0":10,"effect_value_1":0,"effect_value_2":0}]}]}"#,
        ));

        let r = latest().expect("published");
        assert_eq!(r.options.len(), 2, "five rows, but the player had two buttons");
        assert!(!r.all_certain, "both buttons branch (2 rows and 3 rows)");

        // Button 1 -> group si 1, branch 1 of 2. This is what the player took and observed.
        let one = &r.options[0];
        assert_eq!((one.variant, one.variant_count), (1, 2));
        assert_eq!(
            one.guaranteed.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            ["Mood -1", "Previously trained attribute -5"]
        );
        // Button 2 -> group si 2, branch 1 of 3: the -10 variant, without the condition rows.
        let two = &r.options[1];
        assert_eq!((two.variant, two.variant_count), (1, 3));
        assert_eq!(
            two.guaranteed.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            ["Mood -1", "Previously trained attribute -10"]
        );
        let live: Vec<String> =
            r.options.iter().flat_map(|o| o.guaranteed.iter().map(|e| e.text.clone())).collect();
        assert!(!live.iter().any(|t| t.contains("Practice Perfect")), "row 5 leaked: {live:?}");
        assert!(!live.iter().any(|t| t.contains("Practice Poor")), "rows 2/4 leaked: {live:?}");
        // ...but they are still reported as what was at stake, which is the point of the panel.
        assert!(two.variable.iter().any(|e| e.text.contains("Practice Perfect")));
    }

    /// Recorder record 12 (story 830078001, verbatim) — the event that shows `select_index` really
    /// is a per-serving roll. Its options carried (si 2, gsii 1) and (si 1, gsii 2): button 1 asks
    /// for the SECOND branch of its group, which is Wit +4 and the chain ending, and that is exactly
    /// what the recorder observed firing.
    ///
    /// The old flat rule mapped BOTH buttons onto that same row — two different buttons, one row,
    /// which cannot be right — and a previous version of this test asserted that as if it were the
    /// intended behaviour. Under grouping the buttons are properly disjoint.
    ///
    /// The same story appears twice in the log served with different `select_index` values over an
    /// identical table (record 6 vs record 8 do this explicitly), which is what a per-occurrence
    /// roll looks like and what `gain_select_id_index` — always the button ordinal — is not.
    #[test]
    fn select_index_is_the_branch_the_server_rolled_for_this_serving() {
        let _g = exclusive();
        note_events(&mp(
            r#"{"chara_info":{"speed":100},"unchecked_event_array":[{"event_id":10002,
                "story_id":830078001,"event_contents_info":{"support_card_id":30078,"choice_array":[
                  {"select_index":2,"gain_select_id_index":1},
                  {"select_index":1,"gain_select_id_index":2}]}}]}"#,
        ));
        note_choice_rewards(&mp(
            r#"{"choice_reward_array":[
              {"select_index":1,"gain_param_array":[
                {"display_id":4,"effect_value_0":1056,"effect_value_1":9,"effect_value_2":0},
                {"display_id":13,"effect_value_0":7,"effect_value_1":0,"effect_value_2":0}]},
              {"select_index":1,"gain_param_array":[
                {"display_id":1,"effect_value_0":5,"effect_value_1":4,"effect_value_2":0},
                {"display_id":12,"effect_value_0":0,"effect_value_1":0,"effect_value_2":0}]},
              {"select_index":2,"gain_param_array":[
                {"display_id":4,"effect_value_0":1056,"effect_value_1":7,"effect_value_2":0},
                {"display_id":1,"effect_value_0":10,"effect_value_1":5,"effect_value_2":0},
                {"display_id":12,"effect_value_0":0,"effect_value_1":0,"effect_value_2":0}]}]}"#,
        ));

        let r = latest().expect("published");
        assert_eq!(r.options.len(), 2);

        // Button 1: group si 1, and its entry asks for branch 2 of that group — the observed row.
        let one = &r.options[0];
        assert_eq!((one.variant, one.variant_count, one.certain), (2, 2, false));
        assert_eq!(
            one.guaranteed.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            ["Wit +4", "Ends this Support Card's chain event"]
        );
        assert!(
            one.variable.iter().any(|e| e.text == "All attributes +7"),
            "branch 1 of the same group is what this serving rolled away from"
        );

        // Button 2: the lone si-2 row. A different button, so necessarily a different row.
        let two = &r.options[1];
        assert_eq!((two.variant, two.variant_count, two.certain), (1, 1, true));
        assert!(two.guaranteed.iter().any(|e| e.text == "Energy +5"));
        assert_ne!(
            one.guaranteed.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            two.guaranteed.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            "two buttons must never resolve to the same row"
        );
        assert_eq!(r.best_position, 1, "Wit +4 is the only stat gain on offer");
    }

    /// "Get Well Soon!" — the shape a player actually sees on a branching event, taken from a live
    /// screenshot: both options always cost Mood and a previously-trained attribute, and what varies
    /// is only whether a condition lands. The game rolls that on commit and nothing on the wire says
    /// which way (verified four ways), so the panel's job is to separate the part that can't change
    /// Skill hints are the reason to take a lot of choices, so the row has to name the skill (the
    /// id is in effect_value_0, the level in effect_value_1 — the reverse of the param rows).
    #[test]
    fn names_the_skill_a_hint_row_points_at() {
        let _g = exclusive();
        note_events(&mp(
            r#"{"unchecked_event_array":[{"event_id":20000,"story_id":801043001,
                "event_contents_info":{"support_card_id":0,"choice_array":[
                  {"select_index":1,"gain_select_id_index":1}]}}]}"#,
        ));
        note_choice_rewards(&mp(
            r#"{"choice_reward_array":[{"select_index":1,"gain_param_array":[
              {"display_id":6,"effect_value_0":200212,"effect_value_1":1,"effect_value_2":0},
              {"display_id":19,"effect_value_0":5,"effect_value_1":0,"effect_value_2":0}]}]}"#,
        ));

        let r = latest().expect("a reveal was published");
        let effects = &r.options[0].outcomes[0].effects;
        assert_eq!(effects[0].text, "Sunny Days ○ hint lvl +1");
        assert_eq!(effects[0].kind, "hint");
        // display_id 19 is "Previously trained attribute +N" — confirmed against an "Extra Training"
        // pick whose observed delta was Power +5 with nothing else in the row.
        assert_eq!(effects[1].text, "Previously trained attribute +5");
    }

    /// The game re-sends a pending event on essentially every response while it is on screen. Each
    /// re-serving used to overwrite the buffer wholesale, discarding the reward table (fetched ONCE,
    /// by a response carrying nothing else) and the opening stat snapshot — so by the time the player
    /// answered there was nothing left to attribute the outcome to and no resolution was recorded.
    /// This is the regression test for that: the table must survive re-servings of the same event.
    #[test]
    fn a_reserved_event_keeps_its_table_and_opening_snapshot() {
        let _g = exclusive();
        let serve = r#"{"chara_info":{"speed":100,"wiz":50,"vital":80},
            "unchecked_event_array":[{"event_id":7015,"story_id":501004714,
              "event_contents_info":{"support_card_id":0,"choice_array":[
                {"select_index":1,"gain_select_id_index":1},
                {"select_index":1,"gain_select_id_index":2}]}}]}"#;
        note_events(&mp(serve));
        note_choice_rewards(&mp(
            r#"{"choice_reward_array":[
              {"select_index":1,"gain_param_array":[{"display_id":1,"effect_value_0":1,"effect_value_1":10,"effect_value_2":0}]},
              {"select_index":1,"gain_param_array":[{"display_id":1,"effect_value_0":1,"effect_value_1":5,"effect_value_2":0}]}]}"#,
        ));
        assert!(latest().is_some(), "reveal published");

        // The same event served again, with the trainee's stats moving on unrelated grounds.
        note_events(&mp(serve));
        note_events(&mp(serve));

        // The buffered table must still be there — that is what the recorder needs on resolution.
        let kept = PENDING.lock().unwrap().as_ref().map(|p| (p.table.len(), p.before.speed));
        assert_eq!(kept, Some((2, 100)), "table and opening snapshot survived re-serving");
    }

    /// The reveal must not outlive its event: once the pending array no longer offers choices, the
    /// player has answered and the panel has to go blank rather than describe the previous screen.
    #[test]
    fn an_answered_event_clears_the_reveal() {
        let _g = exclusive();
        note_events(&mp(
            r#"{"unchecked_event_array":[{"event_id":3001,"story_id":501006101,
                "event_contents_info":{"support_card_id":0,"choice_array":[
                  {"select_index":1,"gain_select_id_index":1}]}}]}"#,
        ));
        note_choice_rewards(&mp(
            r#"{"choice_reward_array":[{"select_index":1,"gain_param_array":[
              {"display_id":1,"effect_value_0":30,"effect_value_1":20,"effect_value_2":0}]}]}"#,
        ));
        assert!(latest().is_some());

        // The response after the pick: the event is gone from the queue.
        note_events(&mp(r#"{"unchecked_event_array":[]}"#));
        assert!(latest().is_none(), "a spent reveal must not linger");

        // A story event with no choices must not resurrect it either.
        note_events(&mp(
            r#"{"unchecked_event_array":[{"event_id":1001,"story_id":400000400,
                "event_contents_info":{"support_card_id":0,"choice_array":[]}}]}"#,
        ));
        assert!(latest().is_none());
    }

    /// An unmodelled display_id must still produce a visible row. Silently dropping it would make
    /// an option look emptier than it is — the one failure mode that turns the panel into a lie.
    #[test]
    fn an_unknown_effect_still_renders() {
        let _g = exclusive();
        note_choice_rewards(&mp(
            r#"{"choice_reward_array":[{"select_index":1,"gain_param_array":[
              {"display_id":999,"effect_value_0":7,"effect_value_1":3,"effect_value_2":0}]}]}"#,
        ));
        let r = latest().expect("a table with no buffered event still publishes");
        assert_eq!(r.options[0].outcomes[0].effects[0].text, "Effect #999 (7, 3)");
        assert_eq!(r.options[0].outcomes[0].stat_total, 0);
    }
}
