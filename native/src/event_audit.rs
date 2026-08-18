//! Does the event decoder's rule actually hold? Scored against the recorder's own log.
//!
//! [`crate::event_reveal`] appends one line per answered event to
//! `overseer-logs/event-outcomes.jsonl`: the whole served event, every reward row with its
//! `select_index`, which button the player pressed, and the trainee's observed before→after delta.
//! That is enough to replay the decode offline and check it against what the game actually did —
//! which is the only reason the log exists. This module is that replay, in-process, so the UI can
//! show the audit instead of asking the user to trust a claim.
//!
//! Two numbers come out, kept deliberately apart because they rest on different evidence (see the
//! rule's own comment in `event_reveal::note_choice_rewards`):
//!
//!   * **events** — did the shipped rule name a row that could have produced the observed delta?
//!     This is dominated by the grouping half, which is settled.
//!   * **branches** — restricted to picks where the button really branched *and* the outcome
//!     identifies exactly one of its rows. That puts the within-group half (`select_index` names
//!     the branch) on trial by itself, which is the half still under suspicion.
//!
//! Records the observation cannot resolve — no row explains the delta, no `selected_index`, or an
//! old pre-grouping line — are counted as unresolved and score nothing. A ratio that quietly
//! counted the unresolvable as passes would be worse than no ratio at all.

use serde_json::Value;

/// GET /api/predict/accuracy
pub fn accuracy_json() -> String {
    let a = audit();
    serde_json::json!({
        "events_ok": a.events_ok,
        "events_total": a.events_total,
        "branch_ok": a.branch_ok,
        "branch_total": a.branch_total,
        "records": a.records,
        "unresolved": a.unresolved,
    })
    .to_string()
}

#[derive(Default, Debug, PartialEq)]
struct Audit {
    /// Lines in the log, including ones nothing can be concluded from.
    records: usize,
    unresolved: usize,
    events_ok: usize,
    events_total: usize,
    branch_ok: usize,
    branch_total: usize,
}

fn audit() -> Audit {
    let mut a = Audit::default();
    let path = crate::paths::log_file("event-outcomes.jsonl");
    let Ok(text) = std::fs::read_to_string(path) else {
        return a; // no careers answered yet — zeros, and the UI hides the chip
    };
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        a.records += 1;
        match serde_json::from_str::<Value>(line) {
            Ok(rec) => score(&rec, &mut a),
            Err(_) => a.unresolved += 1,
        }
    }
    a
}

/// Replay one recorded event through the shipped rule and mark it.
fn score(rec: &Value, a: &mut Audit) {
    let (Some(table), Some(choices)) = (
        rec.get("table").and_then(|v| v.as_array()),
        rec.get("choice_array").and_then(|v| v.as_array()),
    ) else {
        a.unresolved += 1;
        return;
    };
    // Early records stored rows bare, before per-row select_index was kept. They cannot test a rule
    // about select_index, and inventing one for them is exactly the circularity this log exists to
    // avoid.
    if table.is_empty() || table.iter().any(|r| r.get("select_index").is_none()) {
        a.unresolved += 1;
        return;
    }
    let Some(entry) = usize::try_from(rec.get("selected_index").and_then(Value::as_i64).unwrap_or(-1))
        .ok()
        .and_then(|i| choices.get(i))
    else {
        a.unresolved += 1;
        return;
    };

    let keys: Vec<i64> =
        table.iter().map(|r| r.get("select_index").and_then(Value::as_i64).unwrap_or(0)).collect();
    let rows: Vec<Vec<(i64, i64, i64)>> = table.iter().map(effects_of).collect();

    // Which rows could have produced what was actually observed.
    let fits: Vec<usize> = (0..rows.len()).filter(|&i| consistent(&rows[i], rec)).collect();
    if fits.is_empty() {
        a.unresolved += 1;
        return;
    }

    // The shipped rule, verbatim: `gain_select_id_index` names the button's group, the entry's own
    // `select_index` names the row inside it (clamped, as the decoder clamps).
    let mut groups: Vec<i64> = Vec::new();
    for k in &keys {
        if !groups.contains(k) {
            groups.push(*k);
        }
    }
    let gsii = entry.get("gain_select_id_index").and_then(Value::as_i64).unwrap_or(0);
    let si = entry.get("select_index").and_then(Value::as_i64).unwrap_or(0);
    let Some(&key) = usize::try_from(gsii)
        .ok()
        .and_then(|g| g.checked_sub(1))
        .and_then(|g| groups.get(g))
    else {
        a.unresolved += 1;
        return;
    };
    let members: Vec<usize> = (0..rows.len()).filter(|&i| keys[i] == key).collect();
    if members.is_empty() {
        a.unresolved += 1;
        return;
    }
    let pick = members[usize::try_from(si).unwrap_or(1).clamp(1, members.len()) - 1];

    a.events_total += 1;
    a.events_ok += usize::from(fits.contains(&pick));

    // The branch half on its own. A single-row group tests nothing here, and neither does a group
    // whose rows the outcome cannot tell apart.
    let fitting: Vec<usize> = members.iter().copied().filter(|i| fits.contains(i)).collect();
    if members.len() > 1 && fitting.len() == 1 {
        a.branch_total += 1;
        a.branch_ok += usize::from(fitting[0] == pick);
    }
}

/// `[[display_type, value_a, value_b], …]` for one recorded row.
fn effects_of(row: &Value) -> Vec<(i64, i64, i64)> {
    row.get("effects")
        .and_then(Value::as_array)
        .map(|es| {
            es.iter()
                .map(|e| {
                    let g = |i: usize| e.get(i).and_then(Value::as_i64).unwrap_or(0);
                    (g(0), g(1), g(2))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What one reward row would leave behind, in the same vocabulary as the recorded delta.
///
/// `flex` is stat gain the row does not pin to a named stat ("+N to a random stat"), so it is
/// checked against the stat *total* rather than any single one. `fuzzy` marks rows whose size the
/// wire does not state at all (race-grade scaling) — those can never be ruled out, so they are
/// treated as consistent rather than silently failing every comparison.
#[derive(Default)]
struct Footprint {
    stats: i64,
    vital: i64,
    motivation: i64,
    skill_point: i64,
    flex: i64,
    /// Conditions removed; `-1` means "all of them", i.e. an unknown count.
    lost: i64,
    gained: Vec<i64>,
    tips: usize,
    fuzzy: bool,
}

impl Footprint {
    fn add(&mut self, param: i64, amount: i64) {
        match param {
            1..=5 => self.stats += amount,
            10 => self.vital += amount,
            20 => self.motivation += amount,
            30 => self.skill_point += amount,
            _ => self.fuzzy = true,
        }
    }
}

fn footprint(row: &[(i64, i64, i64)]) -> Footprint {
    let mut f = Footprint::default();
    for &(disp, a, b) in row {
        match disp {
            1 | 23 => f.add(a, b),
            2 => f.add(a, -b),
            6 => f.tips += 1,
            9 | 37 | 40 => f.gained.push(a),
            10 => f.lost += 1,
            13 => f.stats += a * 5, // "+a to every stat"
            14 => f.flex += a * b,
            34 => f.flex -= a * b,
            19 => f.flex += a,
            20 => f.flex -= a,
            15 => f.lost = -1, // "cures all bad conditions"
            16 => f.lost += a, // "cures up to a", capped by what was held
            21 | 22 => f.fuzzy = true,
            _ => {}
        }
    }
    f
}

/// Could this row have produced the delta the recorder observed?
fn consistent(row: &[(i64, i64, i64)], rec: &Value) -> bool {
    let f = footprint(row);
    if f.fuzzy {
        return true;
    }
    let d = rec.get("delta").unwrap_or(&Value::Null);
    let g = |k: &str| d.get(k).and_then(Value::as_i64).unwrap_or(0);
    if f.vital != g("vital") || f.motivation != g("motivation") || f.skill_point != g("skill_point")
    {
        return false;
    }
    let observed_stats: i64 = ["speed", "stamina", "power", "guts", "wiz"].iter().map(|k| g(k)).sum();
    if observed_stats != f.stats + f.flex {
        return false;
    }
    let tips_seen = d.get("tips_gained").and_then(Value::as_object).map_or(0, serde_json::Map::len);
    if f.tips != tips_seen {
        return false;
    }
    let before = ids(rec.get("before_effects"));
    let lost_seen = ids(d.get("effects_lost")).len() as i64;
    if f.lost >= 0 {
        // A cure can fire with nothing to cure, but it can never remove more than it promises.
        if lost_seen > f.lost {
            return false;
        }
        if f.lost > 0 && lost_seen == 0 && !before.is_empty() {
            return false;
        }
    }
    // "Become <condition>" on a trainee who already has it changes nothing observable, so only
    // conditions they did not already hold are required to show up as gained.
    let mut want: Vec<i64> = f.gained.iter().copied().filter(|c| !before.contains(c)).collect();
    let mut seen = ids(d.get("effects_gained"));
    want.sort_unstable();
    seen.sort_unstable();
    seen == want
}

fn ids(v: Option<&Value>) -> Vec<i64> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// One button, two branches, and an outcome that names which one fired: +5 energy (disp 1,
    /// param 10) versus −5. The recorded `select_index` is 2, so the rule picks the second row.
    fn two_branch_record(select_index: i64) -> Value {
        json!({
            "choice_array": [{"select_index": select_index, "gain_select_id_index": 1}],
            "selected_index": 0,
            "table": [
                {"select_index": 1, "effects": [[1, 10, 5]]},
                {"select_index": 1, "effects": [[2, 10, 5]]},
            ],
            "delta": {"speed":0,"stamina":0,"power":0,"guts":0,"wiz":0,
                      "vital": -5, "motivation": 0, "skill_point": 0,
                      "tips_gained": {}, "effects_gained": [], "effects_lost": []},
            "before_effects": [],
        })
    }

    #[test]
    fn a_branch_the_rule_gets_right_scores_on_both_counts() {
        let mut a = Audit::default();
        score(&two_branch_record(2), &mut a); // row 2 is the −5, which is what happened
        assert_eq!((a.events_ok, a.events_total), (1, 1));
        assert_eq!((a.branch_ok, a.branch_total), (1, 1));
    }

    #[test]
    fn a_branch_the_rule_gets_wrong_is_counted_as_a_miss_not_dropped() {
        let mut a = Audit::default();
        score(&two_branch_record(1), &mut a); // rule says +5; the trainee lost 5
        assert_eq!((a.events_ok, a.events_total), (0, 1));
        assert_eq!((a.branch_ok, a.branch_total), (0, 1));
        assert_eq!(a.unresolved, 0);
    }

    /// The grouping half: `gain_select_id_index` is the button ordinal, so button 2 must read the
    /// second distinct select_index — not the second row of the flat table.
    #[test]
    fn the_second_button_reads_the_second_group() {
        let rec = json!({
            "choice_array": [{"select_index": 1, "gain_select_id_index": 1},
                             {"select_index": 1, "gain_select_id_index": 2}],
            "selected_index": 1,
            "table": [
                {"select_index": 1, "effects": [[2, 10, 5]]},   // button 1, branch a
                {"select_index": 1, "effects": [[2, 10, 9]]},   // button 1, branch b
                {"select_index": 2, "effects": [[1, 10, 5]]},   // button 2
            ],
            "delta": {"speed":0,"stamina":0,"power":0,"guts":0,"wiz":0,
                      "vital": 5, "motivation": 0, "skill_point": 0,
                      "tips_gained": {}, "effects_gained": [], "effects_lost": []},
            "before_effects": [],
        });
        let mut a = Audit::default();
        score(&rec, &mut a);
        assert_eq!((a.events_ok, a.events_total), (1, 1));
        // Button 2's group holds a single row, so it says nothing about branch selection.
        assert_eq!((a.branch_ok, a.branch_total), (0, 0));
    }

    /// A delta no row explains means the log is wrong, the decoder is wrong, or something else
    /// moved the trainee. Whichever it is, it is not evidence for the rule.
    #[test]
    fn a_record_no_row_explains_scores_nothing() {
        let rec = json!({
            "choice_array": [{"select_index": 1, "gain_select_id_index": 1}],
            "selected_index": 0,
            "table": [{"select_index": 1, "effects": [[1, 10, 5]]}],
            "delta": {"speed":40,"stamina":0,"power":0,"guts":0,"wiz":0,
                      "vital": 0, "motivation": 0, "skill_point": 0,
                      "tips_gained": {}, "effects_gained": [], "effects_lost": []},
            "before_effects": [],
        });
        let mut a = Audit::default();
        score(&rec, &mut a);
        assert_eq!(a, Audit { unresolved: 1, ..Audit::default() });
    }

    /// Rows whose amount the wire never states (race-grade scaling) can't be ruled out, so they
    /// must not be scored as failures.
    #[test]
    fn unstated_amounts_are_treated_as_possible() {
        assert!(consistent(&[(21, 0, 0)], &json!({"delta": {"vital": -99}})));
    }
}
