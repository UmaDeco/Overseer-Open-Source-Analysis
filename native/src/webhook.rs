//! Career-completion webhooks.
//!
//! When a career finishes, Overseer already knows a great deal about the run: it has watched every
//! turn's `chara_info`, buffered every race reward, tracked the inferred action timeline, and
//! finally sees the server's `single_mode_finish_common` block with the trained chara's full record.
//! This module turns that into ONE structured summary and delivers it to any number of endpoints.
//!
//! Design notes:
//!
//! * **Extensible envelope.** The wire format is `{schema, version, event, sent_at, career:{…}}`.
//!   Consumers should key off `schema` + `version` and ignore unknown fields; new statistics are
//!   added as new keys inside `career`, never by re-shaping or renaming what is already there. Every
//!   section is ALWAYS present (empty/zero rather than absent) so a parser never has to branch on
//!   existence.
//! * **Generic first, Discord as a rendering.** The canonical payload is plain JSON. A target whose
//!   format is `discord` gets that same object rendered into an embed; the raw summary still travels
//!   in the embed's footer-free `overseer` field so a Discord bot can parse it too.
//! * **Off the game thread, always.** `record_finish` hands the summary to a bounded queue; a single
//!   background sender owns all network I/O, with per-attempt timeouts and bounded exponential
//!   backoff. The game thread never blocks, and a dead endpoint can never wedge anything.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

fn log(m: &str) {
    crate::tools::log(m);
}

// ── configuration ───────────────────────────────────────────────────────────────────────────────

/// How a target wants the payload rendered.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum WebhookFormat {
    /// Decide from the URL: discord.com / discordapp.com → Discord embed, anything else → JSON.
    #[default]
    Auto,
    /// Discord webhook (embeds).
    Discord,
    /// The raw Overseer JSON envelope.
    Json,
}

/// One configured endpoint.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct WebhookTarget {
    /// Friendly label shown in the web UI (optional).
    pub name: String,
    /// `https://…` or `http://…` (an explicit port is allowed).
    pub url: String,
    pub format: WebhookFormat,
    pub enabled: bool,
}

impl Default for WebhookTarget {
    fn default() -> Self {
        WebhookTarget {
            name: String::new(),
            url: String::new(),
            format: WebhookFormat::Auto,
            enabled: true,
        }
    }
}

/// Which optional sections to include. Everything defaults ON — the point of the feature is a rich
/// report — but a user pointing this at a chat channel may want it trimmed.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct WebhookSections {
    pub career_info: bool,
    pub final_stats: bool,
    pub sparks: bool,
    pub skills: bool,
    pub racing: bool,
    pub training: bool,
    pub summary: bool,
    /// Per-turn action timeline. Large; off by default so a Discord message stays readable.
    pub timeline: bool,
}

impl Default for WebhookSections {
    fn default() -> Self {
        WebhookSections {
            career_info: true,
            final_stats: true,
            sparks: true,
            skills: true,
            racing: true,
            training: true,
            summary: true,
            timeline: false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct WebhookConfig {
    pub enabled: bool,
    pub targets: Vec<WebhookTarget>,
    pub sections: WebhookSections,
    /// Extra fields merged into every payload's `meta` (e.g. a trainer nickname or an environment
    /// tag). Free-form so a user can route/filter downstream without us adding knobs.
    pub tags: std::collections::BTreeMap<String, String>,
    /// Delivery attempts per target (1 = no retry).
    pub retries: u32,
    /// Per-attempt timeout, milliseconds.
    pub timeout_ms: u32,
    /// Also fire when a career ENDS EARLY (retired / abandoned) rather than only on a full finish.
    pub on_incomplete: bool,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        WebhookConfig {
            enabled: false,
            targets: Vec::new(),
            sections: WebhookSections::default(),
            tags: std::collections::BTreeMap::new(),
            retries: 3,
            timeout_ms: 8000,
            on_incomplete: false,
        }
    }
}

static CONFIG: Lazy<Mutex<WebhookConfig>> = Lazy::new(|| Mutex::new(WebhookConfig::default()));

/// Push a new configuration live (called from settings on boot and on every edit).
pub fn apply(s: &crate::settings::Settings) {
    if let Ok(mut c) = CONFIG.lock() {
        *c = s.webhook.clone();
    }
    if config().enabled && !config().targets.is_empty() {
        start_sender();
    }
}

pub fn config() -> WebhookConfig {
    CONFIG.lock().map(|c| c.clone()).unwrap_or_default()
}

// ── delivery status (surfaced in the web UI) ────────────────────────────────────────────────────

static SENT_OK: AtomicU64 = AtomicU64::new(0);
static SENT_FAIL: AtomicU64 = AtomicU64::new(0);
static LAST_STATUS: Mutex<String> = Mutex::new(String::new());
static LAST_MS: AtomicU64 = AtomicU64::new(0);

fn note_status(ok: bool, msg: String) {
    if ok {
        SENT_OK.fetch_add(1, Ordering::Relaxed);
    } else {
        SENT_FAIL.fetch_add(1, Ordering::Relaxed);
    }
    LAST_MS.store(crate::tools::now_ms(), Ordering::Relaxed);
    if let Ok(mut g) = LAST_STATUS.lock() {
        *g = msg;
    }
}

/// `{delivered, failed, last, queued}` for the web UI.
pub fn status_json() -> serde_json::Value {
    let queued = QUEUE.lock().map(|q| q.items.len()).unwrap_or(0);
    let last_ms = LAST_MS.load(Ordering::Relaxed);
    let last_age = if last_ms == 0 {
        serde_json::Value::Null
    } else {
        serde_json::json!(crate::tools::now_ms().saturating_sub(last_ms))
    };
    serde_json::json!({
        "delivered": SENT_OK.load(Ordering::Relaxed),
        "failed": SENT_FAIL.load(Ordering::Relaxed),
        "queued": queued,
        "last": LAST_STATUS.lock().map(|g| g.clone()).unwrap_or_default(),
        "last_age_ms": last_age,
    })
}

// ── send queue + worker ─────────────────────────────────────────────────────────────────────────

struct Item {
    /// The canonical envelope.
    payload: serde_json::Value,
    /// A short human line (used for the Discord `content` and for logs).
    headline: String,
    /// Attempt counter, incremented per failed pass.
    attempts: u32,
    /// Process-clock ms before which this item must not be retried (backoff).
    not_before: u64,
}

struct Queue {
    items: Vec<Item>,
    stop: bool,
}

const QUEUE_CAP: usize = 32;
static QUEUE: Lazy<Mutex<Queue>> = Lazy::new(|| Mutex::new(Queue { items: Vec::new(), stop: false }));
static CV: Condvar = Condvar::new();
static SENDER_STARTED: AtomicBool = AtomicBool::new(false);

fn start_sender() {
    if SENDER_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    if std::thread::Builder::new()
        .name("overseer-webhook".into())
        .spawn(sender_main)
        .is_err()
    {
        SENDER_STARTED.store(false, Ordering::Release);
        log("[webhook] sender thread spawn failed — webhooks disabled this session");
    }
}

fn enqueue(payload: serde_json::Value, headline: String) {
    start_sender();
    if let Ok(mut q) = QUEUE.lock() {
        if q.items.len() >= QUEUE_CAP {
            q.items.remove(0); // drop the OLDEST — the newest career summary matters most
            log("[webhook] queue full — dropped the oldest pending delivery");
        }
        q.items.push(Item { payload, headline, attempts: 0, not_before: 0 });
    }
    CV.notify_one();
}

fn sender_main() {
    loop {
        let mut item = {
            let mut q = match QUEUE.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            loop {
                if q.stop {
                    return;
                }
                let now = crate::tools::now_ms();
                if let Some(i) = q.items.iter().position(|it| it.not_before <= now) {
                    break q.items.remove(i);
                }
                // Either nothing queued, or everything is in backoff — park until the soonest
                // deadline (bounded, so a spurious wake can't spin).
                let wait_ms = q
                    .items
                    .iter()
                    .map(|it| it.not_before.saturating_sub(now))
                    .min()
                    .unwrap_or(5_000)
                    .clamp(50, 5_000);
                q = match CV.wait_timeout(q, std::time::Duration::from_millis(wait_ms)) {
                    Ok((g, _)) => g,
                    Err(_) => return,
                };
            }
        };

        let cfg = config();
        if !cfg.enabled {
            continue; // disabled between enqueue and send — drop silently
        }
        let mut all_ok = true;
        for target in cfg.targets.iter().filter(|t| t.enabled && !t.url.trim().is_empty()) {
            let discord = matches!(target.format, WebhookFormat::Discord)
                || (matches!(target.format, WebhookFormat::Auto) && is_discord_url(&target.url));
            let body = if discord {
                discord_body(&item.payload, &item.headline)
            } else {
                item.payload.clone()
            };
            let bytes = serde_json::to_vec(&body).unwrap_or_default();
            match crate::http::post_any(
                &target.url,
                &bytes,
                "application/json; charset=utf-8",
                cfg.timeout_ms,
            ) {
                Ok((status, resp)) if (200..300).contains(&status) => {
                    let label = target_label(target);
                    note_status(true, format!("{label}: HTTP {status}"));
                    log(&format!("[webhook] delivered to {label} (HTTP {status})"));
                    let _ = resp;
                }
                Ok((status, resp)) => {
                    all_ok = false;
                    let label = target_label(target);
                    let snip: String =
                        String::from_utf8_lossy(&resp).chars().take(200).collect();
                    note_status(false, format!("{label}: HTTP {status} {snip}"));
                    log(&format!("[webhook] {label} rejected the delivery: HTTP {status} {snip}"));
                }
                Err(e) => {
                    all_ok = false;
                    let label = target_label(target);
                    note_status(false, format!("{label}: {e}"));
                    log(&format!("[webhook] {label} failed: {e}"));
                }
            }
        }

        if !all_ok {
            item.attempts += 1;
            if item.attempts < cfg.retries.max(1) {
                // Exponential backoff, capped: 2s, 8s, 32s…
                let delay = 2000u64.saturating_mul(4u64.saturating_pow(item.attempts - 1)).min(120_000);
                item.not_before = crate::tools::now_ms() + delay;
                log(&format!(
                    "[webhook] retry {}/{} in {}s",
                    item.attempts + 1,
                    cfg.retries.max(1),
                    delay / 1000
                ));
                if let Ok(mut q) = QUEUE.lock() {
                    q.items.push(item);
                }
                CV.notify_one();
            } else {
                log("[webhook] giving up on this delivery after the configured retries");
            }
        }
    }
}

fn target_label(t: &WebhookTarget) -> String {
    if !t.name.trim().is_empty() {
        return t.name.clone();
    }
    // Never log the full URL — a webhook URL is a bearer credential.
    match t.url.split("://").nth(1).and_then(|r| r.split('/').next()) {
        Some(host) => format!("{host}/…"),
        None => "webhook".into(),
    }
}

fn is_discord_url(url: &str) -> bool {
    let u = url.to_ascii_lowercase();
    u.contains("discord.com/api/webhooks") || u.contains("discordapp.com/api/webhooks")
}

// ── public entry points ─────────────────────────────────────────────────────────────────────────

/// Fire the career-completed webhook. Called from `career::record_finish` on the response-parse
/// thread; returns immediately (all I/O is on the sender thread).
pub fn career_completed(summary: &crate::career::CareerSummary) {
    if !crate::runtime::active(crate::runtime::Subsystem::Webhook) {
        return;
    }
    let cfg = config();
    if !cfg.enabled || cfg.targets.iter().all(|t| !t.enabled || t.url.trim().is_empty()) {
        return;
    }
    if !summary.completed && !cfg.on_incomplete {
        return;
    }
    let payload = envelope("career.completed", summary.to_json(&cfg.sections), &cfg);
    enqueue(payload, headline(summary));
}

/// Send a synthetic sample delivery so the user can verify their endpoint from the web UI.
/// Returns immediately; the result shows up in `status_json()`.
pub fn send_test() {
    let cfg = config();
    if cfg.targets.is_empty() {
        note_status(false, "no targets configured".into());
        return;
    }
    let summary = crate::career::CareerSummary::sample();
    let payload = envelope("career.test", summary.to_json(&cfg.sections), &cfg);
    enqueue(payload, format!("{} (test delivery)", headline(&summary)));
}

/// Wrap a career body in the versioned envelope every consumer keys off.
fn envelope(event: &str, career: serde_json::Value, cfg: &WebhookConfig) -> serde_json::Value {
    serde_json::json!({
        "schema": "overseer.career",
        // Bump ONLY for a breaking change. Additive fields keep version 1.
        "version": 1,
        "event": event,
        "sent_at": iso_now(),
        "meta": {
            "source": "Overseer",
            "overseer_version": crate::selfupdate::current_version(),
            "tags": cfg.tags,
        },
        "career": career,
    })
}

fn headline(s: &crate::career::CareerSummary) -> String {
    let name = if s.trainee.is_empty() { "Career".to_string() } else { s.trainee.clone() };
    format!(
        "{name} finished — {} ({} pts) · {} fans · {}W/{}R",
        s.grade, s.rank_score, s.fans, s.wins, s.races_entered
    )
}

// ── Discord rendering ───────────────────────────────────────────────────────────────────────────

/// Render the canonical payload as a Discord webhook body. The full envelope is NOT embedded (it
/// would blow the 6000-character embed budget); a Discord-side consumer that wants structured data
/// should point a second, `json`-format target at itself.
fn discord_body(payload: &serde_json::Value, headline: &str) -> serde_json::Value {
    let c = payload.get("career").cloned().unwrap_or(serde_json::Value::Null);
    let g = |path: &[&str]| -> serde_json::Value {
        let mut cur = &c;
        for p in path {
            match cur.get(*p) {
                Some(v) => cur = v,
                None => return serde_json::Value::Null,
            }
        }
        cur.clone()
    };
    let num = |path: &[&str]| -> i64 { g(path).as_i64().unwrap_or(0) };
    let txt = |path: &[&str]| -> String {
        g(path).as_str().unwrap_or("").to_string()
    };

    let mut fields: Vec<serde_json::Value> = Vec::new();
    let push = |fields: &mut Vec<serde_json::Value>, name: &str, value: String, inline: bool| {
        if value.trim().is_empty() {
            return;
        }
        // Discord caps a field value at 1024 chars.
        let v: String = value.chars().take(1000).collect();
        fields.push(serde_json::json!({ "name": name, "value": v, "inline": inline }));
    };

    // Career
    let scenario = txt(&["career_info", "scenario"]);
    let difficulty = txt(&["career_info", "difficulty"]);
    let style = txt(&["career_info", "running_style"]);
    let distance = txt(&["career_info", "distance"]);
    let dur = num(&["career_info", "duration_seconds"]);
    push(
        &mut fields,
        "Career",
        format!(
            "**Scenario** {}\n**Difficulty** {}\n**Style** {} · **Distance** {}\n**Duration** {}",
            dash(&scenario),
            dash(&difficulty),
            dash(&style),
            dash(&distance),
            hms(dur as u64)
        ),
        true,
    );

    // Final stats
    push(
        &mut fields,
        "Final Stats",
        format!(
            "SPD **{}** · STA **{}** · PWR **{}**\nGUT **{}** · WIS **{}**\nSkill Points left **{}**",
            num(&["final_stats", "speed"]),
            num(&["final_stats", "stamina"]),
            num(&["final_stats", "power"]),
            num(&["final_stats", "guts"]),
            num(&["final_stats", "wisdom"]),
            num(&["final_stats", "skill_points_remaining"]),
        ),
        true,
    );

    // Racing
    push(
        &mut fields,
        "Racing",
        format!(
            "Entered **{}** · Wins **{}** · Losses **{}**\nG1 wins **{}**\nFans gained **{}** (total **{}**)",
            num(&["racing", "races_entered"]),
            num(&["racing", "wins"]),
            num(&["racing", "losses"]),
            num(&["racing", "g1_wins"]),
            num(&["racing", "fans_gained"]),
            num(&["racing", "final_fan_count"]),
        ),
        true,
    );

    // Skills
    let skills = g(&["skills", "purchased"]);
    let skill_names: Vec<String> = skills
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let shown: Vec<String> = skill_names.iter().take(12).cloned().collect();
    let more = skill_names.len().saturating_sub(shown.len());
    push(
        &mut fields,
        "Skills",
        format!(
            "**{}** learned · **{}** SP spent{}{}",
            num(&["skills", "total_purchased"]),
            num(&["skills", "skill_points_spent"]),
            if shown.is_empty() { String::new() } else { format!("\n{}", shown.join(", ")) },
            if more > 0 { format!(" (+{more} more)") } else { String::new() },
        ),
        false,
    );

    // Training
    let dist = g(&["training", "distribution"]);
    let train_line = dist
        .as_object()
        .map(|o| {
            let mut v: Vec<String> = o
                .iter()
                .map(|(k, val)| format!("{k} {}", val.as_i64().unwrap_or(0)))
                .collect();
            v.sort();
            v.join(" · ")
        })
        .unwrap_or_default();
    push(
        &mut fields,
        "Training",
        format!(
            "{}{}{}",
            if train_line.is_empty() { String::new() } else { format!("{train_line}\n") },
            format!("Friendships completed **{}**", num(&["training", "friendships_completed"])),
            {
                let f = num(&["training", "failures"]);
                if f > 0 { format!(" · Failures **{f}**") } else { String::new() }
            }
        ),
        false,
    );

    // Sparks
    let sp = g(&["sparks"]);
    if !sp.is_null() {
        let fmt_list = |key: &str| -> String {
            sp.get(key)
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|e| {
                            let n = e.get("name").and_then(|x| x.as_str())?;
                            let stars = e.get("stars").and_then(|x| x.as_i64()).unwrap_or(0);
                            Some(format!("{n} {}", "★".repeat(stars.clamp(0, 3) as usize)))
                        })
                        .take(8)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default()
        };
        let chara = sp
            .get("character")
            .and_then(|c| c.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("");
        let chara_stars = sp
            .get("character")
            .and_then(|c| c.get("stars"))
            .and_then(|n| n.as_i64())
            .unwrap_or(0);
        let mut body = String::new();
        if !chara.is_empty() {
            body.push_str(&format!(
                "**Character** {chara} {}\n",
                "★".repeat(chara_stars.clamp(0, 3) as usize)
            ));
        }
        for (label, key) in [("Stat", "stats"), ("Skill", "skills"), ("Race", "races")] {
            let l = fmt_list(key);
            if !l.is_empty() {
                body.push_str(&format!("**{label}** {l}\n"));
            }
        }
        push(&mut fields, "Sparks", body, false);
    }

    // Summary
    push(
        &mut fields,
        "Result",
        format!(
            "**Rank** {} · **Score** {}{}",
            dash(&txt(&["summary", "final_rank"])),
            num(&["summary", "evaluation_score"]),
            {
                let a = g(&["summary", "achievements"]);
                let list: Vec<String> = a
                    .as_array()
                    .map(|x| x.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                if list.is_empty() { String::new() } else { format!("\n{}", list.join("\n")) }
            }
        ),
        false,
    );

    // Colour by grade band so a channel scans at a glance.
    let colour = match txt(&["summary", "final_rank"]).chars().next() {
        Some('S') => 0xF2C94C, // gold
        Some('A') => 0xEB5757, // red
        Some('B') => 0xBB6BD9, // purple
        Some('C') => 0x2D9CDB, // blue
        _ => 0x6FCF97,         // green
    };

    serde_json::json!({
        "username": "Overseer",
        "content": headline,
        "embeds": [{
            "title": txt(&["career_info", "character"]),
            "description": format!(
                "{} · {}",
                dash(&txt(&["career_info", "scenario"])),
                dash(&txt(&["summary", "final_rank"]))
            ),
            "color": colour,
            "fields": fields,
            "timestamp": payload.get("sent_at").and_then(|v| v.as_str()).unwrap_or_default(),
            "footer": { "text": format!("Overseer {}", crate::selfupdate::current_version()) },
        }],
        // Structured mirror for bots that read the raw webhook body rather than the embed.
        "overseer": { "schema": payload.get("schema"), "version": payload.get("version"), "event": payload.get("event") },
    })
}

fn dash(s: &str) -> String {
    if s.trim().is_empty() { "—".to_string() } else { s.to_string() }
}

/// `H:MM:SS` (or `M:SS` under an hour) for a duration in seconds.
pub fn hms(total: u64) -> String {
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Local time as an ISO-8601 string with offset — Discord parses it and so does everything else.
pub fn iso_now() -> String {
    use windows_sys::Win32::System::SystemInformation::{GetLocalTime, GetSystemTime};
    unsafe {
        let mut lt: windows_sys::Win32::Foundation::SYSTEMTIME = std::mem::zeroed();
        let mut ut: windows_sys::Win32::Foundation::SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut lt);
        GetSystemTime(&mut ut);
        // Offset in whole minutes, derived from the two clocks (avoids a timezone-API dependency).
        let lmin = day_minutes(&lt);
        let umin = day_minutes(&ut);
        let mut diff = lmin as i64 - umin as i64;
        if diff > 12 * 60 {
            diff -= 24 * 60;
        } else if diff < -12 * 60 {
            diff += 24 * 60;
        }
        let sign = if diff < 0 { '-' } else { '+' };
        let ad = diff.abs();
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}{:02}:{:02}",
            lt.wYear, lt.wMonth, lt.wDay, lt.wHour, lt.wMinute, lt.wSecond, sign, ad / 60, ad % 60
        )
    }
}

fn day_minutes(st: &windows_sys::Win32::Foundation::SYSTEMTIME) -> u32 {
    st.wHour as u32 * 60 + st.wMinute as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discord_urls_are_auto_detected() {
        assert!(is_discord_url("https://discord.com/api/webhooks/123/abc"));
        assert!(is_discord_url("https://discordapp.com/api/webhooks/123/abc"));
        assert!(is_discord_url("HTTPS://DISCORD.COM/API/WEBHOOKS/1/x"));
        // A generic endpoint must NOT be rendered as an embed — it would receive a Discord-shaped
        // body instead of the documented envelope.
        assert!(!is_discord_url("https://example.com/hooks/uma"));
        assert!(!is_discord_url("http://127.0.0.1:8080/overseer"));
    }

    #[test]
    fn target_labels_never_leak_the_url() {
        // A webhook URL is a bearer credential: the token portion must never reach the log or the UI.
        let t = WebhookTarget {
            name: String::new(),
            url: "https://discord.com/api/webhooks/12345/SUPERSECRETTOKEN".into(),
            format: WebhookFormat::Auto,
            enabled: true,
        };
        let label = target_label(&t);
        assert!(!label.contains("SUPERSECRETTOKEN"), "label was: {label}");
        assert!(label.contains("discord.com"));
        // An explicit name wins and is used verbatim.
        let named = WebhookTarget { name: "My server".into(), ..t };
        assert_eq!(target_label(&named), "My server");
    }

    #[test]
    fn durations_read_as_clock_time() {
        assert_eq!(hms(0), "0:00");
        assert_eq!(hms(59), "0:59");
        assert_eq!(hms(61), "1:01");
        assert_eq!(hms(3600), "1:00:00");
        assert_eq!(hms(3 * 3600 + 12 * 60 + 5), "3:12:05");
    }

    #[test]
    fn the_envelope_is_stable_and_self_describing() {
        // Consumers key off schema+version; those two must not drift without a deliberate change.
        let cfg = WebhookConfig::default();
        let summary = crate::career::CareerSummary::sample();
        let env = envelope("career.completed", summary.to_json(&cfg.sections), &cfg);
        assert_eq!(env["schema"], "overseer.career");
        assert_eq!(env["version"], 1);
        assert_eq!(env["event"], "career.completed");
        assert!(env["sent_at"].as_str().is_some_and(|s| s.contains('T')));
        let c = &env["career"];
        // Every section the default config enables must be present, not merely optional.
        for k in ["career_info", "final_stats", "sparks", "skills", "racing", "training", "summary"] {
            assert!(c.get(k).is_some(), "missing section: {k}");
        }
        // …and the ones it disables must be absent rather than empty (payload size matters).
        assert!(c.get("timeline").is_none());
    }

    #[test]
    fn sections_are_actually_honoured() {
        let mut sec = WebhookSections::default();
        sec.sparks = false;
        sec.training = false;
        let c = crate::career::CareerSummary::sample().to_json(&sec);
        assert!(c.get("sparks").is_none());
        assert!(c.get("training").is_none());
        assert!(c.get("racing").is_some());
    }

    #[test]
    fn the_discord_body_fits_discords_limits() {
        let cfg = WebhookConfig::default();
        let summary = crate::career::CareerSummary::sample();
        let env = envelope("career.completed", summary.to_json(&cfg.sections), &cfg);
        let body = discord_body(&env, &headline(&summary));
        let embed = &body["embeds"][0];
        let fields = embed["fields"].as_array().expect("fields");
        assert!(!fields.is_empty());
        assert!(fields.len() <= 25, "Discord allows at most 25 embed fields");
        for f in fields {
            let v = f["value"].as_str().unwrap_or("");
            assert!(v.chars().count() <= 1024, "field value over Discord's 1024-char cap");
        }
        assert!(body["content"].as_str().is_some_and(|s| !s.is_empty()));
    }
}
