//! Race REVEAL — decode the server's race result the instant the response arrives, before the
//! animation plays (and even when the race is skipped). Pure Rust, no IL2CPP; runs on the
//! response hook's bounded "ov-capture" worker thread (never the game's net thread).
//!
//! The career/daily race response carries the full outcome as `race_scenario`: a base64 string of a
//! gzip blob with a fixed-layout binary result table. This mirrors Icarus's `parse_race_result_array`
//! (career_bot/dailies.py) exactly — validated there against live captures — but in Rust and WITHOUT
//! importing any of Icarus's network layer (which is why the reveal lives here, not in the sidecar:
//! Icarus's decoder is trapped in a module that pulls native wheels). The decoded finish order + times
//! + margins are published to the web UI's Predictions page via `ipc::set_reveal`.

#![allow(dead_code)]

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use base64::Engine;
use once_cell::sync::Lazy;
use rmpv::Value;
use serde::{Deserialize, Serialize};

use crate::msgpack::{as_arr, find_key, map_get};

fn rd_i32(b: &[u8], off: usize) -> Option<i32> {
    b.get(off..off + 4).map(|s| i32::from_le_bytes(s.try_into().unwrap()))
}
fn rd_f32(b: &[u8], off: usize) -> Option<f32> {
    b.get(off..off + 4).map(|s| f32::from_le_bytes(s.try_into().unwrap()))
}

fn ordinal(n: i64) -> String {
    let suffix = match (n % 100, n % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// One decoded finisher (frame-order/post-position record).
struct Row {
    viewer_id: i64,
    place: i64,   // 1-based finish order
    time_s: f32,  // display finish time (seconds)
    raw_s: f32,   // precise finish time (seconds) — margins/ordering
}

/// Everything a successful decode yields: the reveal JSON for the web UI plus the player's own
/// result (when the player row was identified) for the persistent race history.
struct Decoded {
    json: String,
    field_size: i64,
    player_place: Option<i64>,
    player_time: Option<f64>,
}

/// Decode the race payload, publish the reveal JSON, and append the player's result to the
/// persistent race history. Called from the "ov-capture" worker thread when the decompressed
/// msgpack contains `race_scenario` — the file I/O below is off the net thread by construction.
/// Silently no-ops on any malformed/edge input — a reveal is best-effort telemetry and must never
/// disturb the response path.
pub fn decode_and_publish(bytes: &[u8]) {
    if let Some(d) = decode(bytes) {
        if let (Some(place), Some(time_s)) = (d.player_place, d.player_time) {
            record_race(place, d.field_size, time_s);
        }
        crate::ipc::set_reveal(d.json);
    }
}

fn decode(bytes: &[u8]) -> Option<Decoded> {
    let mut cur = std::io::Cursor::new(bytes);
    let val = rmpv::decode::read_value(&mut cur).ok()?;

    // race_scenario: base64 string of a gzip blob.
    let scenario_b64 = {
        let mut hits: Vec<&Value> = Vec::new();
        find_key(&val, "race_scenario", &mut hits);
        hits.into_iter().find_map(|v| v.as_str()).map(|s| s.to_string())?
    };
    // race_horse_data: array of {frame_order, viewer_id} — maps a result index → the horse's viewer_id.
    let horses = {
        let mut hits: Vec<&Value> = Vec::new();
        find_key(&val, "race_horse_data", &mut hits);
        hits.into_iter().find_map(|v| as_arr(v).cloned())?
    };
    // The account's own viewer_id (data_headers.viewer_id) identifies the player row; NPCs are 0.
    let mut player_vid: i64 = {
        let mut hits: Vec<&Value> = Vec::new();
        find_key(&val, "viewer_id", &mut hits);
        // The first non-zero viewer_id at the top of the tree is the account header's.
        hits.into_iter().find_map(|v| v.as_i64()).filter(|&x| x != 0).unwrap_or(0)
    };

    // frame_order (1-based) → viewer_id, so result index i (== frame_order-1) resolves to a horse.
    let mut vid_by_index: std::collections::HashMap<usize, i64> = std::collections::HashMap::new();
    for h in &horses {
        let fo = map_get(h, "frame_order").and_then(|v| v.as_i64()).unwrap_or(0);
        let vid = map_get(h, "viewer_id").and_then(|v| v.as_i64()).unwrap_or(0);
        if fo >= 1 {
            vid_by_index.insert((fo - 1) as usize, vid);
            if player_vid == 0 && vid != 0 {
                player_vid = vid; // fallback: the lone non-zero horse is the player (single-mode)
            }
        }
    }

    // base64 → gzip.
    let gz = base64::engine::general_purpose::STANDARD.decode(scenario_b64.as_bytes()).ok()?;
    if gz.len() < 2 || gz[0] != 0x1f || gz[1] != 0x8b {
        return None; // not a gzip stream
    }
    let mut blob = Vec::new();
    flate2::read::GzDecoder::new(&gz[..]).read_to_end(&mut blob).ok()?;

    // Walk the header exactly as Icarus's parse_race_result_array (dailies.py).
    let mut off = 0usize;
    let header_len = rd_i32(&blob, off)? as usize;
    off = off.checked_add(4)?.checked_add(header_len)?;
    let horse_num = rd_i32(&blob, off + 4)?;
    let horse_result_size = rd_i32(&blob, off + 12)? as usize;
    off = off.checked_add(16)?;
    let pad = rd_i32(&blob, off)? as usize;
    off = off.checked_add(4)?.checked_add(pad)?;
    let frame_count = rd_i32(&blob, off)? as usize;
    let frame_size = rd_i32(&blob, off + 4)? as usize;
    off = off.checked_add(8)?.checked_add(frame_count.checked_mul(frame_size)?)?;
    let pad = rd_i32(&blob, off)? as usize;
    off = off.checked_add(4)?.checked_add(pad)?;

    if !(1..=30).contains(&horse_num) || horse_result_size == 0 {
        return None; // implausible field → bail rather than read garbage
    }
    let field_size = horse_num as i64;

    let mut rows: Vec<Row> = Vec::with_capacity(horse_num as usize);
    for i in 0..horse_num as usize {
        let base = off.checked_add(i.checked_mul(horse_result_size)?)?;
        let place = rd_i32(&blob, base)? as i64 + 1;
        let time_s = rd_f32(&blob, base + 4)?;
        let raw_s = rd_f32(&blob, base + 27)?;
        rows.push(Row {
            viewer_id: vid_by_index.get(&i).copied().unwrap_or(0),
            place,
            time_s,
            raw_s,
        });
    }
    rows.sort_by_key(|r| r.place);

    // margin = lengths behind the horse one place AHEAD (0 for the winner). 1 bashin ≈ 7.08 × Δraw-seconds.
    let mut order = Vec::with_capacity(rows.len());
    let mut player_place: Option<i64> = None;
    let mut player_time: Option<f64> = None;
    for (idx, r) in rows.iter().enumerate() {
        let is_player = r.viewer_id != 0 && r.viewer_id == player_vid;
        let margin = if idx == 0 {
            0.0
        } else {
            ((r.raw_s - rows[idx - 1].raw_s) as f64 * 7.08).max(0.0)
        };
        if is_player {
            player_place = Some(r.place);
            player_time = Some(r.time_s as f64);
        }
        order.push(serde_json::json!({
            "place": r.place,
            "time_s": (r.time_s as f64 * 100.0).round() / 100.0,
            "margin": (margin * 10.0).round() / 10.0,
            "is_player": is_player,
        }));
    }

    let label = player_place
        .map(|p| format!("{} of {}", ordinal(p), field_size))
        .unwrap_or_else(|| "—".to_string());

    let reveal = serde_json::json!({
        "player_finish_label": label,
        "field_size": field_size,
        "player_time_s": player_time,
        "order": order,
    });
    crate::tools::log(&format!(
        "[reveal] decoded race: {} of {} finishers",
        player_place.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
        field_size
    ));
    Some(Decoded {
        json: reveal.to_string(),
        field_size,
        player_place,
        player_time,
    })
}

// ---------------------------------------------------------------------------------------------
// Race telemetry history — one compact record per decoded race, persisted next to the DLL.
// Same ensure_loaded / append-cap / tmp+rename write pattern as career.rs's run history.
// ---------------------------------------------------------------------------------------------

/// One finished race from the player's perspective (persisted to `overseer-races-history.json`,
/// newest last on disk).
#[derive(Clone, Serialize, Deserialize)]
pub struct RaceRecord {
    pub epoch_ms: u64, // unix ms at decode time
    pub name: String,  // race::race_header() at the time — "" if the header wasn't captured
    pub grade: i64,    // race::race_grade(): 100=G1, 200=G2, 300=G3, 400=OP, 0/other=unknown
    pub distance: i64, // course metres; 0 unknown
    pub place: i64,    // 1-based finish position
    pub field: i64,    // field size
    pub time_s: f64,   // player's display finish time (seconds)
}

static HISTORY: Lazy<Mutex<Vec<RaceRecord>>> = Lazy::new(|| Mutex::new(Vec::new()));
static HISTORY_LOADED: AtomicBool = AtomicBool::new(false);

const MAX_HISTORY: usize = 2000;
/// Dedup window: the same `race_scenario` can arrive more than once around a race (retry screens,
/// result revisits) — an identical (place, field, ~time) within this window is a resend, not a race.
const DEDUP_WINDOW_MS: u64 = 120_000;
const DEDUP_TIME_EPS: f64 = 0.05;

fn history_path() -> std::path::PathBuf {
    crate::paths::local_file("overseer-races-history.json")
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn ensure_loaded() {
    if HISTORY_LOADED.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Ok(s) = std::fs::read_to_string(history_path()) {
        if let Ok(v) = serde_json::from_str::<Vec<RaceRecord>>(&s) {
            if let Ok(mut h) = HISTORY.lock() {
                *h = v;
            }
        }
    }
    // Missing/corrupt file → empty history (the next append recreates it).
}

/// Append one decoded race to the persistent history (dedup-guarded, capped, atomic write).
/// Runs on the "ov-capture" worker thread — never on the game's net thread.
fn record_race(place: i64, field: i64, time_s: f64) {
    ensure_loaded();
    let now_ms = now_unix_ms();
    let rec = RaceRecord {
        epoch_ms: now_ms,
        name: crate::race::race_header(),
        grade: crate::race::race_grade() as i64,
        distance: crate::race::course_distance() as i64,
        place,
        field,
        time_s,
    };
    if let Ok(mut hist) = HISTORY.lock() {
        // Dedup: the same race resent within the window (same place + field, ~same time) is skipped.
        if let Some(last) = hist.last() {
            if last.place == rec.place
                && last.field == rec.field
                && (last.time_s - rec.time_s).abs() < DEDUP_TIME_EPS
                && now_ms.saturating_sub(last.epoch_ms) < DEDUP_WINDOW_MS
            {
                crate::tools::log("[reveal] duplicate race_scenario ignored (history dedup)");
                return;
            }
        }
        hist.push(rec);
        let over = hist.len().saturating_sub(MAX_HISTORY);
        if over > 0 {
            hist.drain(0..over);
        }
        // Persist atomically (tmp + rename) — same pattern as career.rs.
        if let Ok(json) = serde_json::to_string_pretty(&*hist) {
            let path = history_path();
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
}

/// Snapshot of the persisted race history (oldest first, as stored on disk). For the endpoint layer
/// (webui) to shape its own payloads (/api/races/history, AI Brain) — same clone history_json takes.
pub fn history_snapshot() -> Vec<RaceRecord> {
    ensure_loaded();
    HISTORY.lock().map(|h| h.clone()).unwrap_or_default()
}

/// The full race-history endpoint payload for the web UI: every record (newest FIRST) plus the
/// derived aggregates. Every field is always present — zeros/empties when there is no history.
pub fn history_json() -> String {
    ensure_loaded();
    let hist = HISTORY.lock().map(|h| h.clone()).unwrap_or_default();
    let total = hist.len() as i64;
    let wins = hist.iter().filter(|r| r.place == 1).count() as i64;
    let top3 = hist.iter().filter(|r| (1..=3).contains(&r.place)).count() as i64;
    let g1_wins = hist.iter().filter(|r| r.place == 1 && r.grade == 100).count() as i64;
    let pct = |n: i64| if total > 0 { ((n as f64 / total as f64) * 1000.0).round() / 10.0 } else { 0.0 };
    let avg_place = if total > 0 {
        let s: i64 = hist.iter().map(|r| r.place).sum();
        ((s as f64 / total as f64) * 100.0).round() / 100.0
    } else {
        0.0
    };
    let records: Vec<&RaceRecord> = hist.iter().rev().collect(); // newest first for the UI
    serde_json::json!({
        "records": records,
        "summary": {
            "total": total,
            "wins": wins,
            "top3": top3,
            "g1_wins": g1_wins,
            "win_rate": pct(wins),   // percent, 1 decimal
            "top3_rate": pct(top3),  // percent, 1 decimal
            "avg_place": avg_place,  // 2 decimals
        },
    })
    .to_string()
}
