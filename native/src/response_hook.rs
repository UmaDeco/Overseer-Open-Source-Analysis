//! Overseer — the single `Gallop.HttpHelper::DecompressResponse` hook.
//!
//! One detour reads every decrypted + lz4-decompressed msgpack API response and fans it out:
//!   - to the companion-overlay bridge (`uma_bridge`), for ALL responses;
//!   - the player-horse identity (the one with `viewer_id != 0`) → `race::set_net_player`
//!     (+ freecam auto-follow), so the race-result Top-1 skip knows if you WON;
//!   - remaining race retries (`available_continue_num`) → `race::set_continues_available`;
//!   - (full build only) extra career payloads handled by additional consumers.
//!
//! Read-only: it calls the original, reads the decompressed result, and returns it UNCHANGED. If a
//! co-resident mod already detoured DecompressResponse (e.g. a spark collector) we CHAIN on top —
//! both hooks are read-only, so the response passes through both. This replaces the former duplicate
//! response hooks that lived in the full build.rs and the response hook.rs.

#![allow(dead_code)]

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::OnceLock;

use retour::RawDetour;
use rmpv::Value;

use crate::htt_il2cpp as h;
use crate::msgpack::{as_arr, find_key, map_get};

fn log(msg: &str) {
    crate::tools::log(msg);
}

static INSTALLED: AtomicBool = AtomicBool::new(false);
static ORIG: AtomicUsize = AtomicUsize::new(0);
static DETOUR: OnceLock<RawDetour> = OnceLock::new();

/// One capture payload handed off the net thread: the raw decompressed msgpack plus what to do with
/// it (reveal decode and/or which `ipc::set_capture` slot it fills).
struct Job {
    bytes: Vec<u8>,
    has_reveal: bool,
    kind: &'static str,
}

// Bounded hand-off to the "ov-capture" worker. The net thread only builds ONE owned Vec and
// try_sends it; the base64/gzip/binary reveal decode and the capture publish (with its file-free but
// allocation-heavy clone semantics) happen on the worker. A full channel DROPS the job — captures
// are best-effort telemetry — logged at most once ever.
static CAPTURE_TX: OnceLock<SyncSender<Job>> = OnceLock::new();
static CAPTURE_DROPPED: AtomicBool = AtomicBool::new(false);

fn capture_tx() -> &'static SyncSender<Job> {
    CAPTURE_TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Job>(8);
        let spawned = std::thread::Builder::new()
            .name("ov-capture".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    if job.has_reveal {
                        crate::race_reveal::decode_and_publish(&job.bytes);
                    }
                    crate::ipc::set_capture(job.kind, job.bytes);
                }
            });
        if spawned.is_err() {
            // rx is moved into the failed spawn and dropped → try_send returns Disconnected and
            // every job is silently skipped. Captures degrade; the response path never blocks.
            log("[response] ov-capture worker spawn failed — captures disabled");
        }
        tx
    })
}

type DecompStaticFn = unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void;
type DecompInstFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_void) -> *mut c_void;

unsafe fn on_response(ret: *mut c_void) {
    if ret.is_null() {
        return;
    }
    // Suspended? Then do NOTHING with the response — not the byte scan, not the msgpack decode, not
    // the companion fan-out. This runs on the game's network thread for every single API call, over
    // a payload that can be megabytes. "Overseer disabled" has to mean this path is free, not merely
    // that its consumers ignore the result.
    let analysis = crate::runtime::active(crate::runtime::Subsystem::Analysis);
    let export = crate::runtime::active(crate::runtime::Subsystem::Export);
    if !analysis && !export {
        return;
    }
    let len = h::array_len(ret as *mut h::RawObject);
    if len == 0 || len > 50 * 1024 * 1024 {
        return;
    }
    let data = (ret as *mut u8).add(0x20);
    let slice = std::slice::from_raw_parts(data, len);
    // Feed the plain response to the companion-overlay bridge (all responses, before our filter).
    if export {
        crate::uma_bridge::send_response(slice);
    }
    if !analysis {
        return;
    }

    // ONE pass over the payload for every marker we care about. This used to be eleven separate
    // `contains()` calls, i.e. eleven full scans of a buffer that can be several megabytes — on the
    // game's network thread, for every API response. See `msgpack::contains_any`.
    const N_HORSE: usize = 0;
    const N_CONT: usize = 1;
    const N_CHOICE: usize = 2;
    const N_CHOICE_REWARD: usize = 3;
    const N_CHARA: usize = 4;
    const N_COIN: usize = 5;
    const N_TP: usize = 6;
    const N_RP: usize = 7;
    const N_REWARD: usize = 8;
    const N_HOME: usize = 9;
    const N_SCENARIO: usize = 10;
    const N_FINISH: usize = 11;
    const N_FACTOR: usize = 12;
    const N_UNCHECKED: usize = 13;
    const N_FACTOR_SELECT: usize = 14;
    const N_START: usize = 15;
    const N_NOTUP: usize = 16;
    let hits = crate::msgpack::contains_any(
        slice,
        &[
            b"race_horse_data",
            b"available_continue_num",
            b"choice_array",
            b"choice_reward_array",
            b"chara_info",
            b"coin_info",
            b"tp_info",
            b"rp_info",
            b"race_reward_info",
            b"home_info",
            b"race_scenario",
            b"single_mode_finish_common",
            b"factor_id_array",
            b"unchecked_event_array",
            b"factor_select_info_array",
            b"single_mode_start_common",
            b"not_up_parameter_info",
        ],
    );
    let has_race = hits[N_HORSE];
    let has_cont = hits[N_CONT] && (hits[N_CHOICE] || hits[N_CHOICE_REWARD]);
    // Account/career capture (Phase 6a): any response carrying resources or a career snapshot.
    // `race_reward_info` included so a race-flow response that lacks chara_info still delivers the
    // race result (place + fans) to the career tracker.
    let has_account =
        hits[N_CHARA] || hits[N_COIN] || hits[N_TP] || hits[N_RP] || hits[N_REWARD];
    // Advisor sidecar capture: a full career screen (chara_info + home_info → training tiles + events)
    // or a race result (race_scenario blob). We forward the WHOLE decrypted msgpack to the sidecar.
    let has_career = hits[N_CHARA] && hits[N_HOME];
    let has_reveal = hits[N_SCENARIO];
    // Career-run tracking (Dashboard + Player Actions): any chara_info gives the live turn state; the
    // finish payload gives the completed-run record.
    let has_finish = hits[N_FINISH];
    // Any response carrying inheritance factors — the Legacy Select candidate list, the veteran
    // roster, a finish block — feeds the legacy analyser's spark inventory.
    let has_factors = hits[N_FACTOR] && crate::settings::legacy_capture();
    // Event REVEAL: the pending choice event, and the reward table that describes its options. The
    // table (`get_choice_reward`) arrives as a response with NOTHING else in it — no chara_info, no
    // home_info — so it needs its own gate; every other branch above would drop it on the floor.
    let has_events = hits[N_UNCHECKED];
    let has_choice_reward = hits[N_CHOICE_REWARD];
    // Pre-race FIELD: `race_start_info.race_horse_data` rides on `race_entry` (the decision point)
    // as well as `race_start`. Gated on the horse array rather than on the container, because an
    // empty `race_start_info` also rides along on every ordinary turn response.
    let has_field = hits[N_HORSE];
    // End-of-career spark offer, the career-start plan, and the "did not go up" notices.
    let has_offer = hits[N_FACTOR_SELECT];
    let has_start = hits[N_START];
    let has_notup = hits[N_NOTUP];

    if !has_race
        && !has_cont
        && !has_account
        && !has_career
        && !has_reveal
        && !has_finish
        && !has_factors
        && !has_events
        && !has_choice_reward
        && !has_field
        && !has_offer
        && !has_start
        && !has_notup
    {
        return;
    }
    // Decode the msgpack tree ONCE (only when an in-thread parser matched) and fan it out by
    // reference — the four parse_* consumers used to each decode the same payload themselves.
    if has_race
        || has_cont
        || has_account
        || has_finish
        || has_factors
        || has_events
        || has_choice_reward
        || has_field
        || has_offer
        || has_start
        || has_notup
    {
        if let Some(val) = crate::msgpack::decode(slice) {
            if has_race {
                parse_race(&val);
            }
            if has_cont {
                parse_continues(&val);
            }
            if has_account {
                parse_account(&val);
            }
            if has_finish {
                parse_finish(&val);
            }
            if has_factors {
                parse_factors(&val);
            }
            // Order matters: the pending event must be buffered before the table that describes it,
            // and in the one response that carries both it is the SAME event.
            if has_events {
                crate::event_reveal::note_events(&val);
            }
            if has_choice_reward {
                crate::event_reveal::note_choice_rewards(&val);
            }
            if has_field {
                crate::race_field::note(&val);
            }
            if has_offer {
                parse_spark_offer(&val);
            }
            if has_start {
                parse_career_plan(&val);
            }
            if has_notup {
                parse_blocked(&val);
            }
        }
    }
    // Capture cases (race takes priority over the career snapshot): the ONLY owned copy the net
    // thread makes. The reveal decode (base64 + gzip + binary walk) and the capture publish run on
    // the bounded "ov-capture" worker; a full queue drops the job (best-effort telemetry).
    if has_reveal || has_career {
        let job = Job {
            bytes: slice.to_vec(),
            has_reveal,
            kind: if has_reveal { "race" } else { "career" },
        };
        if capture_tx().try_send(job).is_err() && !CAPTURE_DROPPED.swap(true, Ordering::Relaxed) {
            log("[response] capture queue full — job dropped (further drops are silent)");
        }
    }
}

/// Scrape account resources (TP/RP/carats/gold/SP) + career presence from a response, publishing to
/// `ipc` for the web UI top-bar. Ported from Overseer's AccountState. Read-only; unknown fields stay
/// as their previous value (so a partial response never blanks the strip).
///
/// Also feeds the dashboard-v2 career panels from the SAME walk: MANT inventory + shop catalog
/// (`free_data_set`), training tiles (`home_info.command_info_array`), and the buffered race result
/// (`race_reward_info`) — see the career module for the carried-forward/one-shot semantics.
fn parse_account(val: &Value) {
    let first = |key: &str| -> Option<Value> {
        let mut hits: Vec<&Value> = Vec::new();
        find_key(val, key, &mut hits);
        hits.first().map(|v| (*v).clone())
    };
    let mut acc = crate::ipc::account(); // keep prior values for fields this response lacks

    if let Some(tp) = first("tp_info") {
        if let Some(v) = map_get(&tp, "current_tp").and_then(|x| x.as_i64()) {
            acc.tp_cur = Some(v);
        }
        if let Some(v) = map_get(&tp, "max_tp").and_then(|x| x.as_i64()) {
            acc.tp_max = Some(v);
        }
    }
    if let Some(rp) = first("rp_info") {
        if let Some(v) = map_get(&rp, "current_rp").and_then(|x| x.as_i64()) {
            acc.rp_cur = Some(v);
        }
        if let Some(v) = map_get(&rp, "max_rp").and_then(|x| x.as_i64()) {
            acc.rp_max = Some(v);
        }
    }
    if let Some(coin) = first("coin_info") {
        let coin_n = map_get(&coin, "coin").and_then(|x| x.as_i64());
        let fcoin = map_get(&coin, "fcoin").and_then(|x| x.as_i64());
        let carats = match (coin_n, fcoin) {
            (Some(c), f) => Some(c + f.unwrap_or(0)),
            (None, _) => map_get(&coin, "coin_num").and_then(|x| x.as_i64()),
        };
        if carats.is_some() {
            acc.carats = carats;
        }
    }
    let mut cur_turn = 0i64; // this response's career turn — the shop-catalog expiry check needs it
    if let Some(ci) = first("chara_info") {
        crate::ipc::set_career_present(true);
        if let Some(v) = map_get(&ci, "skill_point").and_then(|x| x.as_i64()) {
            acc.skill_point = Some(v);
        }
        for k in ["money", "current_money", "gold"] {
            if let Some(v) = map_get(&ci, k).and_then(|x| x.as_i64()) {
                acc.gold = Some(v);
                break;
            }
        }
        // Career-run tracking (Dashboard + Player Actions): full trainee state each turn.
        let gi = |k: &str| map_get(&ci, k).and_then(|x| x.as_i64()).unwrap_or(0);
        let st = crate::career::CareerState {
            turn: gi("turn"),
            speed: gi("speed"),
            stamina: gi("stamina"),
            power: gi("power"),
            guts: gi("guts"),
            wit: gi("wiz"), // server key `wiz` == Wit
            skill_point: gi("skill_point"),
            fans: gi("fans"),
            mood: gi("motivation"),
            energy: gi("vital"),
            max_energy: gi("max_vital"),
            card_id: gi("card_id"),
            active: true,
        };
        cur_turn = st.turn;
        if st.turn > 0 {
            crate::career::update(st);
        }
    }
    // Dashboard v2 extras (MANT inventory / shop / training tiles). Each setter fires ONLY when its
    // source array is present in this response — absent arrays keep the carried-forward panel value,
    // matching the server's own partial-update semantics (a partial response never blanks a panel).
    if let Some(free) = first("free_data_set") {
        // Held items → "Name xN" (count key varies by endpoint: num | current_num | item_num,
        // same fallback chain as Icarus items.py _owned_map).
        if let Some(rows) = map_get(&free, "user_item_info_array").and_then(as_arr) {
            let mut items: Vec<String> = Vec::new();
            for row in rows {
                let id = map_get(row, "item_id").and_then(|x| x.as_i64()).unwrap_or(0);
                let num = ["num", "current_num", "item_num"]
                    .iter()
                    .find_map(|k| map_get(row, k).and_then(|x| x.as_i64()))
                    .unwrap_or(0);
                if id > 0 && num > 0 {
                    items.push(format!("{} x{num}", crate::career::item_name(id)));
                }
            }
            crate::career::set_items(items);
        }
        // Shop catalog → "Name (cost)". Sold-out (item_buy_num >= limit_buy_count) and expired
        // (limit_turn < current turn) offers are dropped, mirroring Icarus buy_shop_items.
        if let Some(rows) = map_get(&free, "pick_up_item_info_array").and_then(as_arr) {
            let mut catalog: Vec<String> = Vec::new();
            for row in rows {
                let ri = |k: &str| map_get(row, k).and_then(|x| x.as_i64()).unwrap_or(0);
                let limit = ri("limit_buy_count");
                if limit > 0 && ri("item_buy_num") >= limit {
                    continue; // sold out
                }
                let limit_turn = ri("limit_turn");
                if limit_turn > 0 && limit_turn < cur_turn {
                    continue; // offer expired
                }
                catalog.push(format!("{} ({})", crate::career::item_name(ri("item_id")), ri("coin_num")));
            }
            crate::career::set_catalog(catalog);
        }
    }
    // Support-card bonds, read LIVE each turn rather than only at finish. `friendships_completed`
    // in the career report needs them, and the finish block does not reliably carry the roster — so
    // taking the last observed turn's values is both more available and more accurate.
    if let Some(ci) = first("chara_info") {
        for key in ["support_card_array", "support_card_list", "training_partner_array"] {
            let Some(rows) = map_get(&ci, key).and_then(as_arr) else { continue };
            let mut bonds: Vec<crate::career::BondRow> = Vec::new();
            for row in rows {
                let id = ["support_card_id", "card_id", "id"]
                    .iter()
                    .find_map(|k| map_get(row, k).and_then(|x| x.as_i64()))
                    .unwrap_or(0);
                let bond = ["evaluation", "friendship", "bond", "evaluation_point"]
                    .iter()
                    .find_map(|k| map_get(row, k).and_then(|x| x.as_i64()))
                    .unwrap_or(0);
                if id > 0 {
                    bonds.push(crate::career::BondRow { id, name: String::new(), bond, maxed: bond >= 100 });
                }
            }
            if !bonds.is_empty() {
                crate::career::set_bonds(bonds);
                break;
            }
        }
    }
    // Training tiles (command_type == 1 only): the game's OWN preview numbers, no advice on top.
    // command_id → label (101/601 Speed, 105/602 Stamina, 102/603 Power, 103/604 Guts, 106/605 Wit —
    // the 6xx ids are the summer-camp variants); params target_type → gain label, summed per label.
    if let Some(home) = first("home_info") {
        if let Some(rows) = map_get(&home, "command_info_array").and_then(as_arr) {
            // Deck position (1-6) → (support-card name, card id), so a tile can say WHO is standing
            // on it rather than printing raw slot numbers, and can look up that card's hint pool.
            // Built HERE rather than beside the other chara_info reads because only the tiles need
            // it, and this runs on the game's network thread for every response with a chara_info.
            let deck: std::collections::HashMap<i64, (String, i64)> = first("chara_info")
                .and_then(|ci| map_get(&ci, "support_card_array").and_then(as_arr).cloned())
                .map(|cards| {
                    cards
                        .iter()
                        .filter_map(|row| {
                            let pos = map_get(row, "position").and_then(|x| x.as_i64())?;
                            let id = map_get(row, "support_card_id").and_then(|x| x.as_i64())?;
                            let name = crate::names::support_name(id);
                            let label = if name.is_empty() { format!("Card #{id}") } else { name };
                            Some((pos, (label, id)))
                        })
                        .collect()
                })
                .unwrap_or_default();
            // Hint GROUPS the trainee already holds, for dimming pool entries she's seen before.
            // `skill_tips_array` is keyed by group, which is exactly what the pool names resolve to.
            let held: std::collections::HashSet<i64> = first("chara_info")
                .and_then(|ci| map_get(&ci, "skill_tips_array").and_then(as_arr).cloned())
                .map(|rows| {
                    rows.iter()
                        .filter_map(|r| map_get(r, "group_id").and_then(|x| x.as_i64()))
                        .collect()
                })
                .unwrap_or_default();
            let mut opts: Vec<crate::career::TrainingOpt> = Vec::new();
            for cmd in rows {
                let ci = |k: &str| map_get(cmd, k).and_then(|x| x.as_i64()).unwrap_or(0);
                if ci("command_type") != 1 {
                    continue;
                }
                let name = match ci("command_id") {
                    101 | 601 => "Speed",
                    105 | 602 => "Stamina",
                    102 | 603 => "Power",
                    103 | 604 => "Guts",
                    106 | 605 => "Wit",
                    _ => continue, // not a stat-training command
                };
                let mut gains: Vec<(String, i64)> = Vec::new();
                if let Some(params) = map_get(cmd, "params_inc_dec_info_array").and_then(as_arr) {
                    for p in params {
                        let tt = map_get(p, "target_type").and_then(|x| x.as_i64()).unwrap_or(0);
                        let v = map_get(p, "value").and_then(|x| x.as_i64()).unwrap_or(0);
                        let label = match tt {
                            1 => "SPD",
                            2 => "STA",
                            3 => "PWR",
                            4 => "GUT",
                            5 => "WIT",
                            30 => "Energy",
                            _ => continue, // other target types (bonds etc.) aren't shown
                        };
                        match gains.iter_mut().find(|(l, _)| l == label) {
                            Some(g) => g.1 += v,
                            None => gains.push((label.to_string(), v)),
                        }
                    }
                }
                // Who is on this tile, and which of them the server has ALREADY rolled a skill hint
                // for (`tips_event_partner_array` is decided before the click, not on resolution).
                //
                // These arrays mix three id spaces: 1-6 are DECK POSITIONS, four-digit values are
                // bare chara ids (the scenario umas that stand on training), and the three-digit
                // ones are scenario NPCs we have no name table for. Only the first two can be named;
                // the rest print their id rather than borrowing a wrong name from another space.
                // (`tips_event_partner_array` has only ever held deck positions.)
                let name_partner = |id: i64| -> String {
                    if (1..=6).contains(&id) {
                        if let Some((n, _)) = deck.get(&id) {
                            return n.clone();
                        }
                    } else if id >= 1000 {
                        let n = crate::names::chara_name_by_chara_id(id);
                        if !n.is_empty() {
                            return n;
                        }
                    }
                    format!("Partner #{id}")
                };
                let slots = |key: &str| -> Vec<i64> {
                    map_get(cmd, key)
                        .and_then(as_arr)
                        .map(|rows| rows.iter().filter_map(|v| v.as_i64()).collect())
                        .unwrap_or_default()
                };
                let tips = slots("tips_event_partner_array");
                // What the [!] could give: the offering cards' pools, pooled and de-duplicated,
                // with the ones she already has a hint for flagged. The tile never names the skill,
                // so this is a candidate set — the UI labels it as one.
                let mut hints: Vec<crate::career::HintOption> = Vec::new();
                for pos in &tips {
                    let Some((_, card_id)) = deck.get(pos) else { continue };
                    for skill in crate::names::support_hint_pool(*card_id) {
                        if hints.iter().any(|h| h.name == skill) {
                            continue;
                        }
                        let known = crate::names::skill_group_of_name(&skill)
                            .map(|g| held.contains(&g))
                            .unwrap_or(false);
                        hints.push(crate::career::HintOption { name: skill, known });
                    }
                }
                // New skills first — a repeat hint is worth less than one you don't have.
                hints.sort_by_key(|h| h.known);
                opts.push(crate::career::TrainingOpt {
                    name: name.to_string(),
                    gains,
                    fail: ci("failure_rate"),
                    partners: slots("training_partner_array").into_iter().map(name_partner).collect(),
                    hint_partners: tips.into_iter().map(name_partner).collect(),
                    hints,
                });
            }
            crate::career::set_training(opts);
        }
    }
    // Race result (place + exact fan payout). Buffered one-shot in career — the turn only advances
    // on the NEXT chara_info, where update() attaches this to the inferred RACE action.
    if let Some(rr) = first("race_reward_info") {
        let rank = map_get(&rr, "result_rank").and_then(|x| x.as_i64()).unwrap_or(0);
        let fans = map_get(&rr, "gained_fans").and_then(|x| x.as_i64()).unwrap_or(0);
        if rank > 0 || fans > 0 {
            crate::career::note_race_result(rank, fans);
        }
        // `race_reward_info` only rides on `race_end` — the race is over, so the field that
        // described it is history and must stop being shown as upcoming.
        crate::race_field::clear();
        // G1 victories for the completion report. The grade code travels with the race entry
        // (1 = G1 on this client); a missing grade simply doesn't count, never mis-counts.
        if rank == 1 {
            let grade = ["grade", "race_grade", "grade_id"]
                .iter()
                .find_map(|k| map_get(&rr, k).and_then(|x| x.as_i64()))
                .unwrap_or(0);
            if grade == 1 {
                crate::career::note_g1_win();
            }
        }
    }
    // Skill purchases: the learn response carries the skills that were just bought with their SP
    // cost. This is the only place the COST is visible (the finish block reports the end state, not
    // what was spent), so the report's "skill point usage" comes from here.
    for key in ["skill_learning_info_array", "learning_skill_array", "skill_tips_array"] {
        let mut hits: Vec<&Value> = Vec::new();
        find_key(val, key, &mut hits);
        for h in hits {
            let Some(rows) = as_arr(h) else { continue };
            for row in rows {
                let id = ["skill_id", "id"]
                    .iter()
                    .find_map(|k| map_get(row, k).and_then(|x| x.as_i64()))
                    .unwrap_or(0);
                let cost = ["need_skill_point", "skill_point", "cost", "consume_skill_point"]
                    .iter()
                    .find_map(|k| map_get(row, k).and_then(|x| x.as_i64()))
                    .unwrap_or(0);
                if id > 0 && cost > 0 {
                    crate::career::note_skill_purchase(id, &crate::career::skill_name(id), cost);
                }
            }
        }
    }
    crate::ipc::set_account(acc);
}

/// Record a completed career run from `single_mode_finish_common` (Icarus's finish shape, see
/// runner.py `_capture_career_summary`): the block carries the account's FULL trained-chara ROSTER
/// plus the finished chara's id as the sibling key `trained_chara_id` — the run's final stats /
/// rank_score / fans / wins live on the roster entry whose id matches.
fn parse_finish(val: &Value) {
    let mut hits: Vec<&Value> = Vec::new();
    find_key(val, "single_mode_finish_common", &mut hits);
    let Some(&fin) = hits.first() else { return };
    // The finished chara's id. ALSO the gate: login/start payloads can embed the same block with the
    // roster but no finished id — "first roster entry with a rank_score" used to record some old
    // G+ veteran's base stats at every login (the "default values" Run History bug).
    let my_id = match map_get(fin, "trained_chara_id").and_then(|v| v.as_i64()) {
        Some(id) if id > 0 => id,
        _ => return,
    };
    let tc = match map_get(fin, "trained_chara") {
        Some(v) => v,
        None => return,
    };
    let is_mine = |c: &Value| map_get(c, "trained_chara_id").and_then(|v| v.as_i64()) == Some(my_id);
    let chara: &Value = match as_arr(tc) {
        Some(list) => match list.iter().find(|c| is_mine(c)) {
            Some(c) => c,
            None => return, // finished chara not in the roster → nothing trustworthy to record
        },
        None if is_mine(tc) => tc,
        None => return,
    };
    let gi = |k: &str| map_get(chara, k).and_then(|x| x.as_i64()).unwrap_or(0);
    /// First present key out of several candidates, as an i64. Game updates rename these fields
    /// fairly often, so every optional lookup tries the spellings we've seen rather than hard-coding
    /// one and silently reporting zero when it changes.
    fn first_i64(v: &Value, keys: &[&str]) -> Option<i64> {
        keys.iter().find_map(|k| map_get(v, k).and_then(|x| x.as_i64()))
    }
    fn first_arr<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Vec<Value>> {
        keys.iter().find_map(|k| map_get(v, k).and_then(as_arr))
    }
    let race_count = map_get(chara, "race_result_list")
        .and_then(|v| as_arr(v))
        .map(|a| a.len() as i64)
        .unwrap_or(0);

    // ── the rich completion report ─────────────────────────────────────────────────────────────
    // Everything below is OPTIONAL by construction: a missing key degrades exactly one field (and
    // is reported through `resolved`), never the record. The historical RunRecord is unchanged.
    let mut p = crate::career::FinishPayload {
        card_id: gi("card_id"),
        rank_score: gi("rank_score"),
        fans: gi("fans"),
        speed: gi("speed"),
        stamina: gi("stamina"),
        power: gi("power"),
        guts: gi("guts"),
        wit: gi("wiz"),
        skill_point: gi("skill_point"),
        wins: gi("wins"),
        race_count,
        scenario_id: gi("scenario_id"),
        ..Default::default()
    };

    // Sparks (inheritance "factors"). The server exposes them as a flat id array on the trained
    // chara; the parent-supplied ones, when separated, live under their own key.
    if let Some(arr) = first_arr(chara, &["factor_id_array", "factor_array", "succession_factor_array"]) {
        p.factor_ids = arr
            .iter()
            .filter_map(|v| v.as_i64().or_else(|| first_i64(v, &["factor_id", "id"])))
            .filter(|i| *i > 0)
            .collect();
        if !p.factor_ids.is_empty() {
            p.resolved.push("sparks".into());
        }
    }
    if let Some(arr) = first_arr(chara, &["succession_factor_id_array", "inherit_factor_id_array"]) {
        p.inherited_factor_ids = arr
            .iter()
            .filter_map(|v| v.as_i64().or_else(|| first_i64(v, &["factor_id", "id"])))
            .filter(|i| *i > 0)
            .collect();
    }

    // Skills the trainee finished with. This is the END STATE (it includes innate/scenario skills,
    // not only purchases), so it is reported as the skill list; the PURCHASE list with SP costs is
    // accumulated live during the run by `career::note_skill_purchase`.
    if let Some(arr) = first_arr(chara, &["skill_array", "skill_list", "chara_skill_array"]) {
        p.skill_ids = arr
            .iter()
            .filter_map(|v| first_i64(v, &["skill_id", "id"]).or_else(|| v.as_i64()))
            .filter(|i| *i > 0)
            .collect();
        if !p.skill_ids.is_empty() {
            p.resolved.push("skills".into());
        }
    }

    // Support-card bonds. `maxed` uses the game's own 100 cap; anything at/over it is a completed
    // friendship, which is what "friendships completed" means in the report.
    if let Some(arr) = first_arr(chara, &["support_card_array", "support_card_list", "training_partner_array"]) {
        let mut rows: Vec<crate::career::BondRow> = Vec::new();
        for row in arr {
            let id = first_i64(row, &["support_card_id", "card_id", "id"]).unwrap_or(0);
            let bond = first_i64(row, &["evaluation", "friendship", "bond", "evaluation_point"]).unwrap_or(0);
            if id > 0 {
                rows.push(crate::career::BondRow { id, name: String::new(), bond, maxed: bond >= 100 });
            }
        }
        if !rows.is_empty() {
            p.resolved.push("bonds".into());
            p.bonds = rows;
        }
    }

    // Career descriptors (running style / distance aptitude / difficulty), each best-effort.
    p.running_style = first_i64(chara, &["running_style", "runningStyle", "last_running_style"]).unwrap_or(0);
    p.proper_distance = first_i64(chara, &["proper_distance", "proper_distance_middle", "distance_type"]).unwrap_or(0);
    p.difficulty = first_i64(fin, &["difficulty", "difficulty_id"])
        .or_else(|| first_i64(chara, &["difficulty", "difficulty_id"]))
        .unwrap_or(0);

    // Scenario-specific end-of-run numbers: anything on the finish block whose key mentions the
    // scenario. Verbatim + bounded, so a new scenario's results appear in the report the day it
    // ships without a code change.
    if let Value::Map(m) = fin {
        for (k, v) in m {
            let (Some(key), Some(n)) = (k.as_str(), v.as_i64()) else { continue };
            if key.contains("scenario") || key.contains("score") || key.contains("rank") {
                if p.scenario_results.len() < 32 {
                    p.scenario_results.insert(key.to_string(), n);
                }
            }
        }
    }
    // Every other integer on the trained-chara block, bounded — future-proofing so a statistic we
    // haven't modelled yet still reaches the webhook consumer instead of being dropped.
    if let Value::Map(m) = chara {
        for (k, v) in m {
            let (Some(key), Some(n)) = (k.as_str(), v.as_i64()) else { continue };
            if p.raw.len() >= 96 {
                break;
            }
            p.raw.insert(key.to_string(), n);
        }
    }
    p.resolved.push("core".into());
    crate::career::record_finish_full(p);
    // The run is over — retire everything that only describes a live turn, or the prediction HUD
    // keeps drawing the finished career's last event and training tiles over the lobby. `present`
    // was previously set true and never cleared (nothing read it until the HUD did).
    crate::event_reveal::clear();
    crate::career::set_training(Vec::new());
    crate::race_field::clear();
    crate::legacy::clear_spark_offer();
    crate::career::set_blocked(Vec::new());
    crate::ipc::set_career_present(false);
}

/// The end-of-career spark OFFER: every candidate the run is putting in front of you, before you
/// pick one. `factor_select` sends the initial pool, `factor_lottery` the re-rolled one; both use the
/// same container, so this keys on shape rather than on which endpoint answered.
fn parse_spark_offer(val: &Value) {
    let mut hits: Vec<&Value> = Vec::new();
    find_key(val, "factor_select_info_array", &mut hits);
    let Some(groups) = hits.into_iter().find_map(as_arr) else { return };

    // The pool can arrive split across lottery groups (a re-roll adds one); flatten them in order.
    let mut ids: Vec<i64> = Vec::new();
    for g in groups {
        let Some(rows) = map_get(g, "factor_info_array").and_then(as_arr) else { continue };
        for row in rows {
            if let Some(id) = map_get(row, "factor_id").and_then(|v| v.as_i64()).filter(|i| *i > 0) {
                ids.push(id);
            }
        }
    }
    if ids.is_empty() {
        return;
    }
    // The counters live on the enclosing `single_mode_factor_*_common`, whichever answered.
    let first_i64 = |key: &str| -> i64 {
        let mut h: Vec<&Value> = Vec::new();
        find_key(val, key, &mut h);
        h.into_iter().find_map(|v| v.as_i64()).unwrap_or(0)
    };
    crate::legacy::note_spark_offer(&ids, first_i64("lottery_remain_num"), first_i64("lottery_count"));
}

/// This run's race plan, from the career-start payload. `race_random_program_array` is the schedule
/// THIS career rolled (it varies run to run, so it is only knowable here) and `reserved_race_array`
/// is what the player has already entered on a deck.
fn parse_career_plan(val: &Value) {
    let mut plan: Vec<crate::career::PlannedRace> = Vec::new();
    let mut push = |year: i64, program_id: i64, reserved: bool| {
        if program_id <= 0 || plan.len() >= 64 {
            return;
        }
        if plan.iter().any(|p: &crate::career::PlannedRace| p.program_id == program_id) {
            return; // a reserved race is usually also in the rolled set; list it once, as reserved
        }
        let (name, ground, distance) = crate::names::race_program(program_id);
        plan.push(crate::career::PlannedRace {
            year,
            program_id,
            name,
            distance,
            ground: if ground == 2 { "Dirt" } else { "Turf" },
            reserved,
        });
    };

    // Reserved first, so the dedupe above keeps the player's own entries over the rolled duplicates.
    let mut hits: Vec<&Value> = Vec::new();
    find_key(val, "reserved_race_array", &mut hits);
    for deck in hits.into_iter().filter_map(as_arr).flatten() {
        let Some(races) = map_get(deck, "race_array").and_then(as_arr) else { continue };
        for r in races {
            let gi = |k: &str| map_get(r, k).and_then(|x| x.as_i64()).unwrap_or(0);
            push(gi("year"), gi("program_id"), true);
        }
    }
    let mut hits: Vec<&Value> = Vec::new();
    find_key(val, "race_random_program_array", &mut hits);
    for r in hits.into_iter().filter_map(as_arr).flatten() {
        let gi = |k: &str| map_get(r, k).and_then(|x| x.as_i64()).unwrap_or(0);
        // Rows are sometimes a bare id rather than a record, depending on the scenario.
        let program = if gi("program_id") > 0 { gi("program_id") } else { r.as_i64().unwrap_or(0) };
        push(gi("year"), program, false);
    }
    if !plan.is_empty() {
        plan.sort_by_key(|p| (p.year, p.program_id));
        crate::career::set_plan(plan);
    }
}

/// `not_up_parameter_info` — what the server says did NOT increase.
///
/// Every array in this block was empty for the entire career this was written against, so there is
/// no sample of a populated one to model. It is reported literally rather than interpreted: stats
/// get named (`status_type_array` shares the 1-5 target space used everywhere else in this API), and
/// anything else is reported as "<array>: N" so a real occurrence is visible and diagnosable instead
/// of silently dropped. Empty — the normal case — publishes nothing.
fn parse_blocked(val: &Value) {
    let mut hits: Vec<&Value> = Vec::new();
    find_key(val, "not_up_parameter_info", &mut hits);
    let Some(info) = hits.into_iter().next() else { return };
    let Value::Map(entries) = info else { return };

    let mut out: Vec<String> = Vec::new();
    for (k, v) in entries {
        let (Some(key), Some(rows)) = (k.as_str(), as_arr(v)) else { continue };
        if rows.is_empty() {
            continue;
        }
        if key == "status_type_array" {
            for r in rows {
                let label = match r.as_i64().unwrap_or(0) {
                    1 => "Speed".to_string(),
                    2 => "Stamina".to_string(),
                    3 => "Power".to_string(),
                    4 => "Guts".to_string(),
                    5 => "Wit".to_string(),
                    n => format!("stat #{n}"),
                };
                out.push(format!("{label} did not increase"));
            }
        } else {
            out.push(format!("{key}: {}", rows.len()));
        }
        if out.len() >= 16 {
            break;
        }
    }
    crate::career::set_blocked(out);
}

/// Feed the legacy analyser's spark inventory from ANY response that carries inheritance factors —
/// the Legacy Select candidate list, the veteran roster, a career finish block.
///
/// Deliberately structural rather than endpoint-specific: it walks for every `factor_id_array` in
/// the tree and attributes it to the nearest owning record that also carries a character/card id.
/// The game moves these arrays between endpoints and renames their containers across updates, so
/// keying on shape instead of on a container name is what keeps this working after a game patch.
fn parse_factors(val: &Value) {
    /// Depth-first: whenever a map has BOTH an id and a factor array, record the pair.
    fn walk(v: &Value, out: &mut Vec<(i64, Vec<i64>)>, depth: u32) {
        if depth > 12 || out.len() >= 64 {
            return; // bounded: this runs on the network thread
        }
        match v {
            Value::Map(m) => {
                let get = |k: &str| m.iter().find(|(mk, _)| mk.as_str() == Some(k)).map(|(_, mv)| mv);
                let id = ["chara_id", "card_id"]
                    .iter()
                    .find_map(|k| get(k).and_then(|x| x.as_i64()))
                    .unwrap_or(0);
                let factors = ["factor_id_array", "factor_array", "succession_factor_array"]
                    .iter()
                    .find_map(|k| get(k).and_then(as_arr));
                if id > 0 {
                    if let Some(arr) = factors {
                        let ids: Vec<i64> = arr
                            .iter()
                            .filter_map(|e| {
                                e.as_i64().or_else(|| {
                                    ["factor_id", "id"]
                                        .iter()
                                        .find_map(|k| map_get(e, k).and_then(|x| x.as_i64()))
                                })
                            })
                            .filter(|i| *i > 0)
                            .collect();
                        if !ids.is_empty() {
                            out.push((id, ids));
                        }
                    }
                }
                for (_, mv) in m {
                    walk(mv, out, depth + 1);
                }
            }
            Value::Array(a) => {
                for e in a {
                    walk(e, out, depth + 1);
                }
            }
            _ => {}
        }
    }
    let mut found: Vec<(i64, Vec<i64>)> = Vec::new();
    walk(val, &mut found, 0);
    for (id, ids) in found {
        crate::legacy::note_sparks(crate::legacy::chara_of_card(id), &ids);
    }
}

/// Find the player's horse in `race_horse_data` (the one with `viewer_id != 0`; NPCs are all 0)
/// and publish its array index + `frame_order` for the race module.
fn parse_race(val: &Value) {
    let mut arrs: Vec<&Value> = Vec::new();
    find_key(val, "race_horse_data", &mut arrs);
    for a in arrs {
        if let Some(list) = as_arr(a) {
            for (i, hh) in list.iter().enumerate() {
                let vid = map_get(hh, "viewer_id").and_then(|x| x.as_i64()).unwrap_or(0);
                if vid != 0 {
                    let fo = map_get(hh, "frame_order").and_then(|x| x.as_i64()).unwrap_or(0) as i32;
                    log(&format!(
                        "[response] race player: arrIdx={i} frame_order={fo} viewer={vid} horses={}",
                        list.len()
                    ));
                    crate::race::set_net_player(i as i32, fo, list.len() as i32);
                    // Auto-frame the player's Uma at race start (freecam build only).
                    #[cfg(feature = "freecam")]
                    crate::freecam::auto_follow_player(fo);
                    return;
                }
            }
        }
    }
}

/// Read `available_continue_num` (remaining race retries) and publish it so the race-result skip
/// can auto-advance once no retries remain.
fn parse_continues(val: &Value) {
    let mut hits: Vec<&Value> = Vec::new();
    find_key(val, "available_continue_num", &mut hits);
    if let Some(n) = hits.first().and_then(|v| v.as_i64()) {
        crate::race::set_continues_available(n as i32);
    }
}

unsafe extern "C" fn hook_static(arg0: *mut c_void, m: *const c_void) -> *mut c_void {
    let t0 = std::time::Instant::now();
    let ret = {
        let t = ORIG.load(Ordering::Relaxed);
        if t != 0 {
            let f: DecompStaticFn = std::mem::transmute(t);
            f(arg0, m)
        } else {
            std::ptr::null_mut()
        }
    };
    profile(ret, t0);
    ret
}

unsafe extern "C" fn hook_inst(this: *mut c_void, arg0: *mut c_void, m: *const c_void) -> *mut c_void {
    let t0 = std::time::Instant::now();
    let ret = {
        let t = ORIG.load(Ordering::Relaxed);
        if t != 0 {
            let f: DecompInstFn = std::mem::transmute(t);
            f(this, arg0, m)
        } else {
            std::ptr::null_mut()
        }
    };
    profile(ret, t0);
    ret
}

/// Time the game's decompress (`t0`→now) and Overseer's own scan of the result, then fan out. The
/// diagnostic split lets us tell whether a slow response is the game's decrypt/lz4 or our parsing.
unsafe fn profile(ret: *mut c_void, t0: std::time::Instant) {
    let decomp_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let len = if ret.is_null() { 0 } else { h::array_len(ret as *mut h::RawObject) };
    crate::loadprof::decompress(len, decomp_ms);
    let p0 = std::time::Instant::now();
    on_response(ret);
    crate::loadprof::parse(p0.elapsed().as_secs_f64() * 1000.0, &format!("{}KB", len / 1024));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These parsers publish into process-global stores; cargo runs tests on parallel threads.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
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

    /// The nine sparks a real `factor_select` offered at the end of a career, verbatim. This is the
    /// whole point of the bundled factor table: every one of these ids is named, where the id
    /// heuristic alone could only have guessed at the stat spark.
    #[test]
    fn the_spark_offer_is_decoded_and_named() {
        let _g = exclusive();
        crate::legacy::clear_spark_offer();
        parse_spark_offer(&mp(
            r#"{"single_mode_factor_select_common":{"lottery_remain_num":1,"lottery_count":1,
              "factor_select_info_array":[{"lottery_id":1,"factor_info_array":[
                {"factor_id":403,"level":0},{"factor_id":2302,"level":0},
                {"factor_id":10060101,"level":0},{"factor_id":2010601,"level":0},
                {"factor_id":2011801,"level":0},{"factor_id":2014401,"level":0},
                {"factor_id":2016601,"level":0},{"factor_id":1003401,"level":0},
                {"factor_id":3000402,"level":0}]}]}}"#,
        ));

        let offer = crate::legacy::spark_offer().expect("an offer was published");
        assert_eq!(offer.sparks.len(), 9);
        assert_eq!((offer.rerolls_left, offer.rerolls_used), (1, 1));
        assert!(
            offer.sparks.iter().all(|s| !s.name.is_empty()),
            "every offered spark resolved to a name"
        );
        // Sorted best-stars-first, so the 3★ stat spark leads.
        assert_eq!((offer.sparks[0].name.as_str(), offer.sparks[0].stars), ("Guts", 3));
        let by_id = |id: i64| offer.sparks.iter().find(|s| s.id == id).unwrap();
        assert_eq!((by_id(10060101).kind.as_str(), by_id(10060101).name.as_str()), ("unique", "Triumphant Pulse"));
        assert_eq!(by_id(1003401).kind, "race");
        assert_eq!(by_id(2302).name, "Late Surger");
    }

    /// A re-roll adds a second lottery group to the SAME container; both must be collected.
    #[test]
    fn a_rerolled_pool_collects_every_group() {
        let _g = exclusive();
        crate::legacy::clear_spark_offer();
        parse_spark_offer(&mp(
            r#"{"single_mode_factor_lottery_common":{"lottery_remain_num":0,"lottery_count":2,
              "factor_select_info_array":[
                {"lottery_id":1,"factor_info_array":[{"factor_id":403}]},
                {"lottery_id":2,"factor_info_array":[{"factor_id":2302},{"factor_id":0}]}]}}"#,
        ));
        let offer = crate::legacy::spark_offer().unwrap();
        assert_eq!(offer.sparks.len(), 2, "both groups, and the zero id dropped");
        assert_eq!(offer.rerolls_left, 0);
    }

    /// The career-start plan: the player's entered races win over the rolled set on a duplicate, and
    /// unnamed programs still appear with their id intact.
    #[test]
    fn the_career_plan_merges_entered_and_rolled_races() {
        let _g = exclusive();
        crate::career::set_plan(Vec::new());
        parse_career_plan(&mp(
            r#"{"single_mode_start_common":{
              "reserved_race_array":[{"deck_num":0,"race_array":[{"year":1,"program_id":623}]},
                                     {"deck_num":1,"race_array":[]}],
              "race_random_program_array":[{"year":1,"program_id":623},{"year":2,"program_id":81},
                                           {"year":3,"program_id":1106}]}}"#,
        ));
        let plan = crate::career::plan_snapshot();
        assert_eq!(plan.len(), 3, "623 listed once, not twice");

        let entered: Vec<_> = plan.iter().filter(|p| p.reserved).collect();
        assert_eq!(entered.len(), 1);
        assert_eq!(entered[0].program_id, 623);
        assert_eq!(entered[0].name, "Hanshin Juvenile Fillies");

        let arima = plan.iter().find(|p| p.program_id == 81).unwrap();
        assert_eq!((arima.name.as_str(), arima.distance, arima.ground), ("Arima Kinen", 2500, "Turf"));
        // 1106 is one of the scenario-only programs the bundled table doesn't carry.
        let unlisted = plan.iter().find(|p| p.program_id == 1106).unwrap();
        assert_eq!(unlisted.name, "");
    }

    /// `not_up_parameter_info` is empty on every turn of the capture this was built from, and an
    /// all-empty block must publish nothing rather than a row of zeroes.
    #[test]
    fn the_blocked_notice_is_silent_when_nothing_is_blocked() {
        let _g = exclusive();
        crate::career::set_blocked(vec!["stale".into()]);
        parse_blocked(&mp(
            r#"{"not_up_parameter_info":{"status_type_array":[],"chara_effect_id_array":[],
              "skill_id_array":[],"skill_tips_array":[],"evaluation_chara_id_array":[]}}"#,
        ));
        assert!(crate::career::blocked_snapshot().is_empty());

        // A populated one is reported literally: stats by name, other arrays by count.
        parse_blocked(&mp(
            r#"{"not_up_parameter_info":{"status_type_array":[1,5],"skill_id_array":[200212,200362]}}"#,
        ));
        let b = crate::career::blocked_snapshot();
        assert!(b.contains(&"Speed did not increase".to_string()), "{b:?}");
        assert!(b.contains(&"Wit did not increase".to_string()), "{b:?}");
        assert!(b.contains(&"skill_id_array: 2".to_string()), "{b:?}");
        crate::career::set_blocked(Vec::new());
    }
}

/// Install the DecompressResponse hook. Idempotent. Called once at boot.
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        if !h::init() {
            log("[response] il2cpp init failed");
            return;
        }
        let image = h::find_game_image();
        if image.is_null() {
            log("[response] game image not found");
            return;
        }
        let ns = std::ffi::CString::new("Gallop").unwrap();
        let cn = std::ffi::CString::new("HttpHelper").unwrap();
        let klass = match h::CLASS_FROM_NAME {
            Some(f) => f(image, ns.as_ptr(), cn.as_ptr()),
            None => std::ptr::null_mut(),
        };
        if klass.is_null() {
            log("[response] Gallop.HttpHelper not found");
            return;
        }
        let mname = std::ffi::CString::new("DecompressResponse").unwrap();
        let method = match h::CLASS_GET_METHOD_FROM_NAME {
            Some(f) => f(klass, mname.as_ptr(), 1),
            None => std::ptr::null_mut(),
        };
        if method.is_null() {
            log("[response] DecompressResponse(1) not found");
            return;
        }
        let is_static = match h::METHOD_GET_FLAGS {
            Some(f) => (f(method, std::ptr::null_mut()) & h::METHOD_ATTRIBUTE_STATIC) != 0,
            None => true,
        };
        let fnptr = h::method_addr(method);
        if fnptr == 0 {
            log("[response] method pointer null");
            return;
        }
        // If another mod (e.g. a spark collector) detoured DecompressResponse first, CHAIN on top
        // instead of yielding. Both hooks are read-only — each calls the original, reads the
        // decompressed result, and returns it UNCHANGED — so they coexist: the response passes
        // through both. retour relocates the existing jmp prologue into our trampoline.
        let chained = crate::il2cpp::is_detoured(fnptr as *const c_void);
        let det = if is_static { hook_static as *const () } else { hook_inst as *const () };
        match RawDetour::new(fnptr as *const (), det) {
            Ok(d) => {
                if d.enable().is_err() {
                    log("[response] detour enable failed");
                    return;
                }
                ORIG.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                let _ = DETOUR.set(d);
                if chained {
                    log("[response] already detoured (another mod) — chaining on top");
                }
                log(&format!("[response] hooked Gallop.HttpHelper::DecompressResponse (static={is_static})"));
            }
            Err(e) => log(&format!("[response] detour failed: {e}")),
        }
    }
}
