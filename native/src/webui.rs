//! In-process web UI server (serves the Overseer SPA on http://127.0.0.1:1620).
//!
//! The unified build keeps Overseer's web control panel. Rather than a separate
//! Python/FastAPI process, the DLL itself serves the single-page app on
//! `http://127.0.0.1:1620` from a background thread. The SPA (index.html +
//! overseer.css) is baked into the DLL via `include_str!`, so there are no loose
//! files to ship and nothing to keep in sync.
//!
//! Threading: this server runs on its OWN std thread and is NOT attached to
//! IL2CPP. It only reads the shared `ipc` state and flips thread-safe atomics
//! (fps/graphics settings). Anything that must call into game/Unity code goes
//! through a store-only request (e.g. `fps::request`) that the game's own
//! main-thread pump applies — the server never touches IL2CPP directly.
//!
//! Port 1620 matches the old Overseer server, so the existing SPA endpoints and
//! any bookmarks keep working. If the port is already taken (e.g. a leftover
//! Python Overseer is still running) we log and give up — run only one.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Duration;

/// The Overseer SPA, baked into the DLL (copied from overseer/web/).
const HTML: &str = include_str!("web/index.html");
const CSS: &str = include_str!("web/overseer.css");

/// Product banners for the Catalogue page. Baked in like the HTML and CSS so the panel has no loose
/// files to install and works before (or without) a full install — the same reason `HTML`/`CSS` are
/// `include_str!`. ~1.2 MB total, static and immutable, so they are served with a long cache.
const PROMO: &[(&str, &[u8])] = &[
    ("overseer.png", include_bytes!("../assets/promo/overseer.png")),
    ("icarus.png", include_bytes!("../assets/promo/icarus.png")),
    ("fortuna.png", include_bytes!("../assets/promo/fortuna.png")),
    ("unfollower.png", include_bytes!("../assets/promo/unfollower.png")),
    ("navigator.png", include_bytes!("../assets/promo/navigator.png")),
];

/// Resolve `/assets/promo/<name>.png` to its embedded bytes.
///
/// The name is matched against the table above by EQUALITY — never joined onto a path — so this
/// cannot be walked out of (`../`, absolute paths and separators simply fail to match an entry).
fn promo_asset(path: &str) -> Option<&'static [u8]> {
    let name = path.strip_prefix("/assets/promo/")?;
    PROMO.iter().find(|(n, _)| *n == name).map(|(_, b)| *b)
}
const ADDR: &str = "127.0.0.1:1620";

/// In-game FPS-counter overlay toggle (SPA `capture.overlay`). Stored so the
/// checkbox reflects the user's choice; wiring it to an on-screen counter is a
/// later nicety.
static OVERLAY_ON: AtomicBool = AtomicBool::new(true);
/// Last non-zero FPS target, so the "Unlock FPS" checkbox can restore it.
static LAST_FPS_TARGET: AtomicI32 = AtomicI32::new(60);

fn log(msg: &str) {
    crate::tools::log(msg);
}

/// Start the web UI server on a background thread. Safe to call before IL2CPP is
/// ready — it serves the SPA immediately and reports "waiting for game" until the
/// engine boots (`ipc::set_core_ready`).
/// How many requests the control panel serves concurrently. The SPA polls several endpoints on
/// timers, so connections arrive steadily — but never in parallel bursts larger than a page load.
const WORKERS: usize = 4;

pub fn spawn() {
    let _ = std::thread::Builder::new().name("overseer-webui".into()).spawn(|| {
        let listener = match TcpListener::bind(ADDR) {
            Ok(l) => l,
            Err(e) => {
                log(&format!("[webui] bind {ADDR} failed ({e}) — is another Overseer still running?"));
                return;
            }
        };
        log(&format!("[webui] serving the control panel on http://{ADDR}"));

        // PERF: the original spawned a FRESH OS THREAD per connection. With the SPA polling roughly
        // five endpoints on 1.5–4 s timers that is thousands of thread creations an hour — each one
        // a kernel object plus a 1 MB stack reservation — purely to read a few atomics and format
        // some JSON. A tiny fixed pool serves the same traffic at a fraction of the cost, and it
        // also bounds what a misbehaving client can make the game process allocate.
        let (tx, rx) = std::sync::mpsc::channel::<TcpStream>();
        let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
        for i in 0..WORKERS {
            let rx = rx.clone();
            let _ = std::thread::Builder::new()
                .name(format!("overseer-web{i}"))
                .spawn(move || loop {
                    let job = { rx.lock().ok().and_then(|r| r.recv().ok()) };
                    match job {
                        Some(stream) => {
                            let _ = handle(stream);
                        }
                        None => return, // channel closed — the listener is gone
                    }
                });
        }
        for stream in listener.incoming().flatten() {
            if tx.send(stream).is_err() {
                return; // every worker died; nothing can serve
            }
        }
    });
}

/// Serve one connection: parse the request line + headers + optional body, route,
/// respond, close. One request per connection (Connection: close) — plenty for a
/// localhost control panel polled by a single browser.
fn handle(stream: TcpStream) -> std::io::Result<()> {
    // Tight read timeout: with a fixed worker pool a slow/abandoned connection occupies a slot, so
    // a client that opens a socket and stops talking must not be able to hold one for long. Local
    // requests complete in microseconds; 5 s is already orders of magnitude of headroom. A write
    // timeout matters for the same reason — a peer that stops reading mid-response.
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut it = request_line.split_whitespace();
    let method = it.next().unwrap_or("").to_string();
    let raw_path = it.next().unwrap_or("/").to_string();

    // Consume headers; capture Content-Length + Origin (CSRF guard) + X-Overseer-Token (sidecar auth).
    let mut content_length = 0usize;
    let mut cross_origin = false;
    let mut token = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let t = line.trim_end();
        if t.is_empty() {
            break;
        }
        let low = t.to_ascii_lowercase();
        if let Some(rest) = low.strip_prefix("content-length:") {
            content_length = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = low.strip_prefix("origin:") {
            // A localhost-only control server: reject any Origin that isn't our own so a random web
            // page you visit can't POST to our endpoints (CSRF). Same-origin fetches send no Origin
            // (or a loopback one); cross-site requests carry the attacker's Origin.
            let o = rest.trim();
            cross_origin = !(o.is_empty()
                || o.contains("127.0.0.1")
                || o.contains("localhost")
                || o.contains("[::1]"));
        } else if let Some(rest) = low.strip_prefix("x-overseer-token:") {
            // NB: value read from the original (case-preserving) line, not the lowercased copy.
            token = t[t.len() - rest.len()..].trim().to_string();
        }
    }

    // Body (JSON for POST). Cap it — control-panel posts are tiny except a bulk glossary import
    // (up to 50k entries), so allow 8 MB; a captured-payload GET has no body.
    let mut body = String::new();
    if content_length > 0 && content_length < 8 << 20 {
        let mut buf = vec![0u8; content_length];
        reader.read_exact(&mut buf)?;
        body = String::from_utf8_lossy(&buf).into_owned();
    }

    let path = raw_path.split('?').next().unwrap_or(&raw_path);
    // Internal sidecar bridge — token-gated, needs the raw path (query) + header. Handled before the
    // Origin guard (the sidecar sends no Origin) and before the SPA router.
    if path.starts_with("/internal/") {
        let (status, ctype, payload) = internal_route(&method, &raw_path, &body, &token);
        return write_response(&mut writer, status, ctype, &payload);
    }
    if cross_origin {
        return write_response(
            &mut writer,
            "403 Forbidden",
            "application/json; charset=utf-8",
            r#"{"error":"cross-origin request refused"}"#,
        );
    }
    // Binary assets bypass the string router (its payload type is String, and a PNG is not UTF-8).
    if method == "GET" {
        if let Some(bytes) = promo_asset(path) {
            return write_bytes(&mut writer, "200 OK", "image/png", "max-age=604800", bytes);
        }
    }
    let (status, ctype, payload) = route(&method, path, &body);
    write_response(&mut writer, status, ctype, &payload)
}

fn route(method: &str, path: &str, body: &str) -> (&'static str, &'static str, String) {
    const JSON: &str = "application/json; charset=utf-8";
    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => {
            ("200 OK", "text/html; charset=utf-8", HTML.to_string())
        }
        ("GET", "/overseer.css") => ("200 OK", "text/css; charset=utf-8", CSS.to_string()),

        // ── live state (polled) ──
        ("GET", "/api/advisor/state") => ("200 OK", JSON, advisor_state()),
        ("GET", "/api/translation/languages") => ("200 OK", JSON, translation_languages()),

        // ── mod controls (real Overseer features) ──
        ("POST", "/api/mod/fps") => ("200 OK", JSON, handle_fps(body)),
        ("POST", "/api/mod/graphics") => ("200 OK", JSON, handle_graphics(body)),
        ("POST", "/api/mod/overlay") => ("200 OK", JSON, handle_overlay(body)),
        ("GET", "/api/mod/gameplay") => ("200 OK", JSON, gameplay_state()),
        ("POST", "/api/mod/gameplay") => ("200 OK", JSON, handle_gameplay(body)),
        // Performance page — FPS / graphics / cloth / display-window, all in one state+apply pair.
        ("GET", "/api/mod/performance") => ("200 OK", JSON, performance_state()),
        ("POST", "/api/mod/performance") => ("200 OK", JSON, handle_performance(body)),
        // Accessibility: colour-vision (daltonization) filter mode. Current mode is also surfaced
        // in the GET /api/mod/performance state ("cvd").
        ("POST", "/api/mod/cvd") => ("200 OK", JSON, handle_cvd(body)),
        // Logs page — live tail (viewer) + full log (export). Plain text.
        ("GET", "/api/logs") => ("200 OK", "text/plain; charset=utf-8", logs_tail()),
        ("GET", "/api/logs/full") => ("200 OK", "text/plain; charset=utf-8", logs_full()),
        // Dashboard + Player Actions: live career state, this run's action timeline, completed runs.
        ("GET", "/api/career") => ("200 OK", JSON, crate::career::snapshot_json()),
        // Veterans page: the trained-uma roster (overseer_umas/veterans.json), returned verbatim.
        ("GET", "/api/veterans") => ("200 OK", JSON, veterans()),
        // AI Brain page: learned.json override when present, else COMPUTED from runs + race history.
        ("GET", "/api/ai/learned") => ("200 OK", JSON, ai_learned()),
        // Race telemetry: every decoded race (newest first) + derived aggregates.
        ("GET", "/api/races/history") => ("200 OK", JSON, races_history()),
        // Gamemaster's "verified" chip: the event decoder replayed against its own recorded
        // outcomes. Reads the log on demand — it is a few hundred short lines and is only fetched
        // once per page load.
        ("GET", "/api/predict/accuracy") => {
            ("200 OK", JSON, crate::event_audit::accuracy_json())
        }

        // ── translation controls ──
        ("POST", "/api/translation/language") => ("200 OK", JSON, handle_language(body)),
        // Toggle the in-process NLLB machine-translation fallback (for lines the glossary can't cover).
        ("POST", "/api/translation/mtl") => ("200 OK", JSON, handle_mtl(body)),
        // DIAGNOSTIC: log every string the set_text hooks translate (see loc_settext::set_trace).
        ("POST", "/api/mod/tracetext") => {
            let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
            let on = v.get("on").and_then(|x| x.as_bool()).unwrap_or(false);
            crate::loc_settext::set_trace(on);
            ("200 OK", JSON, r#"{"ok":true}"#.into())
        }
        // Toggle the higher-quality 1.3B model (vs. the 600M default); hot-reloads the engine.
        ("POST", "/api/translation/hq") => ("200 OK", JSON, handle_hq(body)),
        // Manual glossary overrides (per-language manual.json): read + upsert a source→target pair.
        ("GET", "/api/translation/manual") => ("200 OK", JSON, get_manual()),
        ("POST", "/api/translation/manual") => ("200 OK", JSON, handle_manual(body)),
        // Protected names — the player's own trainer name etc., kept in the original language.
        ("GET", "/api/translation/names") => ("200 OK", JSON, get_protected_names()),
        ("POST", "/api/translation/names") => ("200 OK", JSON, handle_protected_names(body)),
        // Recent translations feed (source → shown output, newest first, cap 50) for click-to-fix.
        ("GET", "/api/translation/recent") => ("200 OK", JSON, translation_recent()),
        // Glossary bulk export/import (whole manual.json / mtl.json for the active language).
        // NB: this router strips query strings, so the export selector travels in a POST body
        // ({which:"manual"|"mtl"}); a plain GET exports the manual overrides (the default).
        ("GET", "/api/translation/glossary/export") | ("POST", "/api/translation/glossary/export") => {
            ("200 OK", JSON, glossary_export(body))
        }
        ("POST", "/api/translation/glossary/import") => ("200 OK", JSON, glossary_import(body)),
        // The model ships with the installer (not downloaded); install is a no-op.
        ("POST", "/api/translation/install") => ("200 OK", JSON, r#"{"ok":true}"#.into()),
        // Force-persist the learned-translation cache to disk right now.
        ("POST", "/api/translation/save") => {
            crate::mtl::save_now();
            ("200 OK", JSON, r#"{"ok":true}"#.into())
        }
        // Translation PACK (localized_data/ community format): status + hot-reload of dict content.
        // Note: dict content hot-swaps; the db/texture/font HOOKS arm at boot, so a pack added while
        // the game runs needs a restart to fully take (the status carries that hint).
        ("GET", "/api/translation/pack") => ("200 OK", JSON, pack_state()),
        ("POST", "/api/translation/pack") => {
            crate::localize::reload();
            ("200 OK", JSON, pack_state())
        }

        // ── Power state: the Overseer master switch and the INDEPENDENT translation switch ──────
        ("GET", "/api/mod/runtime") => ("200 OK", JSON, crate::runtime::status_json().to_string()),
        ("POST", "/api/mod/runtime") => ("200 OK", JSON, handle_runtime(body)),

        // ── Health: soft-lock recovery counters, skip progression, watchdog heartbeat ───────────
        ("GET", "/api/mod/health") => ("200 OK", JSON, health_state()),

        // ── Memory report + on-demand trim ──────────────────────────────────────────────────────
        ("GET", "/api/mod/memory") => ("200 OK", JSON, memory_state()),
        ("POST", "/api/mod/memory/trim") => ("200 OK", JSON, handle_memory_trim()),
        ("POST", "/api/mod/memory/idle") => {
            let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
            if let Some(sec) = v.get("seconds").and_then(|x| x.as_u64()) {
                crate::settings::set_mtl_idle_unload_s(sec);
            }
            (
                "200 OK",
                JSON,
                serde_json::json!({ "ok": true, "seconds": crate::settings::mtl_idle_unload_s() })
                    .to_string(),
            )
        }

        // ── Diagnostics toggles (verbose logging / profiler) ────────────────────────────────────
        ("POST", "/api/mod/logging") => ("200 OK", JSON, handle_logging(body)),

        // ── Legacy / inheritance analysis ───────────────────────────────────────────────────────
        ("GET", "/api/legacy") => ("200 OK", JSON, crate::legacy::state_json().to_string()),
        ("POST", "/api/legacy/recommend") => ("200 OK", JSON, handle_legacy_recommend(body)),
        ("POST", "/api/legacy/plan") => ("200 OK", JSON, handle_legacy_plan(body)),
        ("POST", "/api/legacy/suggest") => ("200 OK", JSON, handle_legacy_suggest(body)),
        ("POST", "/api/legacy/capture") => ("200 OK", JSON, handle_legacy_capture(body)),

        // ── Career-completion webhooks ──────────────────────────────────────────────────────────
        ("GET", "/api/webhook") => ("200 OK", JSON, webhook_state()),
        ("POST", "/api/webhook") => ("200 OK", JSON, handle_webhook(body)),
        ("POST", "/api/webhook/test") => {
            crate::webhook::send_test();
            ("200 OK", JSON, r#"{"ok":true}"#.into())
        }
        ("POST", "/api/webhook/resend") => {
            let ok = crate::career::resend_last();
            ("200 OK", JSON, serde_json::json!({ "ok": ok }).to_string())
        }
        // The most recent completed-career summary, exactly as the webhook would send it.
        ("GET", "/api/career/last") => ("200 OK", JSON, last_career_json()),

        // ── Self-updater ────────────────────────────────────────────────────────────────────────
        // Current version + whether a newer release is pending (drives the header banner + footer).
        ("GET", "/api/update/status") => ("200 OK", JSON, update_status()),
        // Kick a manual check (force = ignore the per-version "don't ask again" skip). Background.
        ("POST", "/api/update/check") => {
            crate::selfupdate::check(true);
            ("200 OK", JSON, r#"{"ok":true}"#.into())
        }
        // Download the pending update + auto-restart the game to apply it. Background.
        ("POST", "/api/update/apply") => {
            crate::selfupdate::download();
            ("200 OK", JSON, r#"{"ok":true}"#.into())
        }
        // "Not now": dismiss (returns next launch) or, with skip=true, silence this exact version.
        ("POST", "/api/update/dismiss") => {
            let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
            if v.get("skip").and_then(|x| x.as_bool()).unwrap_or(false) {
                crate::selfupdate::skip();
            } else {
                crate::selfupdate::dismiss();
            }
            ("200 OK", JSON, r#"{"ok":true}"#.into())
        }

        _ => ("404 Not Found", JSON, r#"{"error":"not found"}"#.into()),
    }
}

/// Token-gated bridge to the advisor sidecar. `/internal/capture?since=<seq>` long/short-polls the
/// captured game payload; `/internal/advice` receives the computed advice/reveal JSON. A missing or
/// wrong token → 404 (indistinguishable from "no such route", and it locks out a stale process).
fn internal_route(method: &str, raw_path: &str, body: &str, token: &str) -> (&'static str, &'static str, String) {
    const JSON: &str = "application/json; charset=utf-8";
    let expected = crate::ipc::token();
    if expected.is_empty() || token != expected {
        return ("404 Not Found", JSON, r#"{"error":"not found"}"#.into());
    }
    // Suspended advisor (Overseer disabled) → serve 204 rather than the payload. The sidecar treats
    // that as "nothing new", backs off to its idle poll and stops computing advice, which is what
    // "Overseer disabled" has to mean for the Live Advisor. It is NOT killed, so re-enabling is
    // instant and we never race its own shutdown path.
    if !crate::sidecar::active() {
        return ("204 No Content", JSON, String::new());
    }
    let path = raw_path.split('?').next().unwrap_or(raw_path);
    match (method, path) {
        ("GET", "/internal/capture") => {
            let since = raw_path
                .split('?')
                .nth(1)
                .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("since=")))
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            let cap = crate::ipc::capture();
            if cap.seq == 0 || cap.seq == since {
                return ("204 No Content", JSON, String::new());
            }
            (
                "200 OK",
                JSON,
                serde_json::json!({
                    "seq": cap.seq, "kind": cap.kind, "payload_b64": base64_encode(&cap.bytes)
                })
                .to_string(),
            )
        }
        ("POST", "/internal/advice") => {
            let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
            let seq = v.get("seq").and_then(|x| x.as_u64()).unwrap_or(0);
            // Store advice/reveal verbatim (the DLL never parses them → the contract can evolve).
            let advice = v.get("advice").filter(|x| !x.is_null()).map(|x| x.to_string());
            let reveal = v.get("reveal").filter(|x| !x.is_null()).map(|x| x.to_string());
            let ok = v.get("sidecar").and_then(|s| s.get("ok")).and_then(|x| x.as_bool()).unwrap_or(true);
            let err = v.get("sidecar").and_then(|s| s.get("error")).and_then(|x| x.as_str()).map(|s| s.to_string());
            crate::ipc::set_advice(seq, advice, reveal, ok, err, now_ms());
            ("200 OK", JSON, r#"{"ok":true}"#.into())
        }
        _ => ("404 Not Found", JSON, r#"{"error":"not found"}"#.into()),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── power state ─────────────────────────────────────────────────────────────────────────────────

/// POST /api/mod/runtime — flip the master switch, the translation switch, or any of the
/// "keep this running while Overseer is disabled" overrides. Every field is optional so the SPA
/// can send just the one that changed.
fn handle_runtime(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let b = |k: &str| v.get(k).and_then(|x| x.as_bool());
    if let Some(x) = b("overseer_enabled") {
        crate::settings::set_bot_enabled(x);
    }
    if let Some(x) = b("translation_enabled") {
        crate::settings::set_translation_enabled(x);
    }
    if let Some(keep) = v.get("keep_when_disabled").and_then(|x| x.as_object()) {
        let kb = |k: &str| keep.get(k).and_then(|x| x.as_bool());
        if let Some(x) = kb("translation") {
            crate::settings::set_keep_translation(x);
        }
        if let Some(x) = kb("analysis") {
            crate::settings::set_keep_analysis(x);
        }
        if let Some(x) = kb("monitoring") {
            crate::settings::set_keep_monitoring(x);
        }
        if let Some(x) = kb("export") {
            crate::settings::set_keep_export(x);
        }
        if let Some(x) = kb("webhook") {
            crate::settings::set_keep_webhook(x);
        }
        if let Some(x) = kb("overlay") {
            crate::settings::set_keep_overlay(x);
        }
    }
    crate::runtime::status_json().to_string()
}

/// POST /api/mod/logging {verbose, profiler} — diagnostics toggles.
fn handle_logging(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    if let Some(x) = v.get("verbose").and_then(|x| x.as_bool()) {
        crate::settings::set_verbose_log(x);
    }
    if let Some(x) = v.get("profiler").and_then(|x| x.as_bool()) {
        crate::settings::set_profiler(x);
    }
    serde_json::json!({
        "ok": true,
        "verbose": crate::settings::verbose_log(),
        "profiler": crate::settings::profiler(),
    })
    .to_string()
}

// ── health / soft-lock recovery ─────────────────────────────────────────────────────────────────

/// GET /api/mod/health — everything a user needs to answer "is Overseer stuck?" without reading a
/// log: the watchdog heartbeat, how many times it has rescued a stuck flag, and whether any skip
/// leg is currently waiting on the game (or has stood itself down).
fn health_state() -> String {
    let (recoveries, since_ms, last) = crate::guard::recovery_stats();
    let (ticks, last_tick) = crate::guard::heartbeat();
    let (main_ticks, main_last) = crate::ui_tempo::heartbeat();
    let now = crate::tools::now_ms();
    serde_json::json!({
        "watchdog": {
            "ticks": ticks,
            "last_tick_age_ms": if last_tick == 0 { serde_json::Value::Null } else { serde_json::json!(now.saturating_sub(last_tick)) },
        },
        "main_thread": {
            "frames": main_ticks,
            // A large value here means the GAME's main thread is blocked — a hang, not a stall.
            "last_frame_age_ms": if main_last == 0 { serde_json::Value::Null } else { serde_json::json!(now.saturating_sub(main_last)) },
        },
        "recoveries": { "count": recoveries, "age_ms": since_ms, "last": last },
        // Re-entry guards leaked by a managed exception unwinding past our frame. Non-zero is
        // normal (the guard self-expires); a fast-growing number means the game is throwing a lot.
        "guard_leaks": crate::hooks::leak_count(),
        "skip_progress": crate::skip::progress_status(),
        "click_engine_busy": crate::ui_input::busy(),
        "log_queue": crate::tools::log::queued(),
    })
    .to_string()
}

// ── memory report ───────────────────────────────────────────────────────────────────────────────

/// Process working set + private bytes, in MB. Win32 only; zeros if the query fails.
fn process_memory_mb() -> (f64, f64) {
    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
        private_usage: usize,
    }
    #[link(name = "psapi")]
    extern "system" {
        fn GetProcessMemoryInfo(process: isize, counters: *mut Counters, cb: u32) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> isize;
    }
    unsafe {
        let mut c = Counters { cb: std::mem::size_of::<Counters>() as u32, ..Default::default() };
        if GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) == 0 {
            return (0.0, 0.0);
        }
        let mb = |b: usize| (b as f64 / (1024.0 * 1024.0) * 10.0).round() / 10.0;
        (mb(c.working_set_size), mb(c.private_usage))
    }
}

/// GET /api/mod/memory — the process footprint plus every Overseer cache that can grow, so a user
/// reporting "Overseer uses N GB" can say WHICH part, and so a regression is visible immediately.
fn memory_state() -> String {
    let (ws, private) = process_memory_mb();
    let (names, press, dialogs, logged) = crate::ui_input::cache_sizes();
    let idle = crate::mtl::idle_ms();
    let idle_json = if idle == u64::MAX { serde_json::Value::Null } else { serde_json::json!(idle) };
    serde_json::json!({
        "process": { "working_set_mb": ws, "private_mb": private },
        "translation": {
            // The single largest allocation Overseer can hold: ~0.7 GB for NLLB-600M, ~2 GB for 1.3B.
            "model_resident": crate::nllb::resident(),
            "model_present": crate::nllb::model_present(),
            "hq": crate::settings::mtl_hq(),
            "idle_unload_s": crate::settings::mtl_idle_unload_s(),
            "idle_ms": idle_json,
            "cache_entries": crate::mtl::cache_count(),
            "queue": crate::mtl::queue_depth(),
            "tracked_components": crate::mtl::tracked_len(),
            "user_field_cache": crate::loc_settext::uf_cache_len(),
        },
        "click_engine": { "names": names, "press_state": press, "dialogs": dialogs, "logged_names": logged },
        "logging": { "queued_lines": crate::tools::log::queued() },
        "legacy": { "observations": crate::legacy::observation_count() },
    })
    .to_string()
}

/// POST /api/mod/memory/trim — drop every reclaimable cache and ask Windows to return the freed
/// pages. Non-destructive: caches rebuild on demand and nothing persisted is touched.
fn handle_memory_trim() -> String {
    let (before, _) = process_memory_mb();
    crate::mtl::trim();
    crate::loc_settext::clear_uf_cache();
    crate::loc_settext::reset_emitted();
    crate::ui_input::clear_caches();
    // Hand the freed pages back to the OS. `SetProcessWorkingSetSize(-1, -1)` is the documented
    // "trim now" idiom; the pages come back on demand, so the only cost is some soft faults.
    #[link(name = "kernel32")]
    extern "system" {
        fn SetProcessWorkingSetSize(process: isize, min: usize, max: usize) -> i32;
        fn GetCurrentProcess() -> isize;
    }
    unsafe {
        SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
    let (after, private) = process_memory_mb();
    crate::tools::log(&format!("[memory] trim: working set {before:.1} MB -> {after:.1} MB"));
    serde_json::json!({ "ok": true, "before_mb": before, "after_mb": after, "private_mb": private })
        .to_string()
}

// ── legacy / inheritance ────────────────────────────────────────────────────────────────────────

fn json_i64_list(v: &serde_json::Value, key: &str) -> Vec<i64> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
        .unwrap_or_default()
}

/// POST /api/legacy/recommend {trainee, limit} — ranked legacy-parent suggestions.
fn handle_legacy_recommend(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let trainee = v.get("trainee").and_then(|x| x.as_i64()).unwrap_or(0);
    if trainee <= 0 {
        return r#"{"ok":false,"error":"trainee required"}"#.into();
    }
    let limit = v.get("limit").and_then(|x| x.as_u64()).unwrap_or(20) as usize;
    let list = crate::legacy::recommend_parents(crate::legacy::chara_of_card(trainee), limit);
    serde_json::json!({ "ok": true, "trainee": trainee, "suggestions": list }).to_string()
}

/// POST /api/legacy/plan {characters:[…]} — evaluate ONE specific rotation.
fn handle_legacy_plan(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let chars: Vec<i64> = json_i64_list(&v, "characters")
        .into_iter()
        .map(crate::legacy::chara_of_card)
        .collect();
    match crate::legacy::plan_loop(&chars) {
        Some(p) => serde_json::json!({ "ok": true, "plan": p }).to_string(),
        None => r#"{"ok":false,"error":"pick between 3 and 6 distinct characters"}"#.into(),
    }
}

/// POST /api/legacy/suggest {pool:[…], size, limit} — search for the best rotations in a pool.
fn handle_legacy_suggest(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let pool: Vec<i64> = json_i64_list(&v, "pool")
        .into_iter()
        .map(crate::legacy::chara_of_card)
        .collect();
    let size = v.get("size").and_then(|x| x.as_u64()).unwrap_or(4) as usize;
    let limit = v.get("limit").and_then(|x| x.as_u64()).unwrap_or(5) as usize;
    let plans = crate::legacy::suggest_loops(&pool, size, limit);
    serde_json::json!({ "ok": true, "plans": plans }).to_string()
}

/// POST /api/legacy/capture {on} — toggle affinity capture (and force a save).
fn handle_legacy_capture(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    if let Some(on) = v.get("on").and_then(|x| x.as_bool()) {
        crate::settings::set_legacy_capture(on);
    }
    crate::legacy::save_now();
    serde_json::json!({ "ok": true, "capture_enabled": crate::settings::legacy_capture() }).to_string()
}

// ── webhooks ────────────────────────────────────────────────────────────────────────────────────

/// GET /api/webhook — the configuration plus delivery status.
fn webhook_state() -> String {
    serde_json::json!({
        "config": crate::settings::webhook_config(),
        "status": crate::webhook::status_json(),
        "has_last_career": crate::career::last_summary().is_some(),
    })
    .to_string()
}

/// POST /api/webhook — replace the configuration (whole object; the SPA always sends it entire).
fn handle_webhook(body: &str) -> String {
    match serde_json::from_str::<crate::webhook::WebhookConfig>(body) {
        Ok(mut cfg) => {
            // Reject anything that isn't an http(s) URL rather than storing a value the sender can
            // only ever fail on (and which would look like a delivery bug to the user).
            cfg.targets.retain(|t| {
                let u = t.url.trim();
                u.is_empty() || u.starts_with("http://") || u.starts_with("https://")
            });
            cfg.retries = cfg.retries.clamp(1, 10);
            cfg.timeout_ms = cfg.timeout_ms.clamp(1000, 60_000);
            cfg.targets.truncate(8);
            crate::settings::set_webhook_config(cfg);
            webhook_state()
        }
        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }).to_string(),
    }
}

/// GET /api/career/last — the most recent completed-career summary (the webhook payload's body).
fn last_career_json() -> String {
    match crate::career::last_summary() {
        Some(s) => serde_json::json!({
            "ok": true,
            "career": s.to_json(&crate::webhook::WebhookSections::default()),
        })
        .to_string(),
        None => r#"{"ok":false,"career":null}"#.into(),
    }
}

/// Standard base64 (for carrying the raw msgpack capture in a JSON field to the sidecar).
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// The advisor/state payload the SPA polls every 1.5s. Phase 1 fills the parts
/// Overseer already knows (engine-attached status, FPS/graphics mod state, and the
/// career skill-point if a career is open); `advice`/`reveal` arrive in Phase 4.
fn advisor_state() -> String {
    let g = crate::ipc::latest();
    let ready = g.core_ready;
    let fps = crate::performance::fps::current();
    let render_scale = crate::settings::render_scale();
    let gfx_quality = crate::settings::gfx_quality();
    let overlay = OVERLAY_ON.load(Ordering::Relaxed);

    // Advisor: advice/reveal JSON the sidecar posted (parsed back to values for embedding), plus a
    // liveness state derived from the last heartbeat.
    let ac = crate::ipc::advice_cache();
    let advice: serde_json::Value = ac
        .advice_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(serde_json::Value::Null);
    let reveal: serde_json::Value = ac
        .reveal_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(serde_json::Value::Null);
    let sidecar_state = if ac.heartbeat_ms == 0 {
        "offline"
    } else if now_ms().saturating_sub(ac.heartbeat_ms) < 15_000 {
        "online"
    } else {
        "reconnecting"
    };

    // Account resources captured from the network responses (Phase 6a).
    let acc = crate::ipc::account();
    let account = serde_json::json!({
        "skill_point": acc.skill_point,
        "tp": { "cur": acc.tp_cur, "max": acc.tp_max },
        "rp": { "cur": acc.rp_cur, "max": acc.rp_max },
        "carats": acc.carats,
        "gold": acc.gold,
    });

    // Succession-affinity (Legacy Select badges): the exact in-game CalcRelationPoint value + the
    // screen gate. values() is None until a pairing is evaluated on that screen; a parent branch is
    // -1 when that parent is unset → emitted as null.
    let (aff_total, aff_p1, aff_p2) = match crate::affinity::values() {
        Some((t, a, b)) => (
            serde_json::json!(t),
            if a < 0 { serde_json::Value::Null } else { serde_json::json!(a) },
            if b < 0 { serde_json::Value::Null } else { serde_json::json!(b) },
        ),
        None => (serde_json::Value::Null, serde_json::Value::Null, serde_json::Value::Null),
    };
    let affinity = serde_json::json!({
        "active": crate::affinity::active(),
        "step": crate::affinity::step_active(),
        "detail": crate::affinity::show_detail(),
        "total": aff_total,
        "p1": aff_p1,
        "p2": aff_p2,
    });

    serde_json::json!({
        "capture": {
            "hooked": ready,
            "attached": ready,
            "udid": if ready { serde_json::json!("native") } else { serde_json::Value::Null },
            "stats": { "responses": 0 },
            "overlay": overlay,
            "mod": {
                "installed": ready,
                "enabled": fps != 0,
                "targetFps": fps,
                "measuredFps": serde_json::Value::Null,
                "readbackFps": fps,
                "readbackVsync": if fps != 0 { 0 } else { 1 },
                "resMult": render_scale,
                "graphicsQuality": if gfx_quality { 3 } else { -1 },
            }
        },
        "account": account,
        "advice": advice,
        "reveal": reveal,
        // Pre-click outcomes decoded in-process: every option of the event on screen, and this
        // turn's training tiles. Both are null/empty outside a career — the page shows its own
        // placeholder rather than stale numbers from the last run.
        "event_reveal": crate::event_reveal::json(),
        "training": crate::career::training_snapshot(),
        // The rest of what the server volunteers before you act: who you're racing (at the entry
        // screen, before the outcome exists), the end-of-career spark pool, this run's rolled race
        // schedule, and anything the server flagged as not having gone up.
        "field": crate::race_field::json(),
        "spark_offer": crate::legacy::spark_offer(),
        "plan": crate::career::plan_snapshot(),
        "blocked": crate::career::blocked_snapshot(),
        "sidecar": { "state": sidecar_state, "reason": ac.err },
        "affinity": affinity,
        // The two independent switches + the derived per-subsystem verdicts, so the header pill and
        // the OFF banner always show what is REALLY running (rather than assuming that "Overseer
        // off" just means translation off — the bug this replaces).
        "runtime": crate::runtime::status_json(),
        // Which UI this build serves. The SPA compares it across polls and reloads itself when it
        // changes, so a tab left open across a deploy stops silently running the previous build.
        "ui_build": ui_build(),
    })
    .to_string()
}

/// Stable per-process id for the served UI assets: FNV-1a over the HTML and CSS that are baked in.
///
/// Computed once and cached — it must not vary between polls, or the page would reload forever.
/// It changes only when index.html or overseer.css change, so restarting the game does NOT trigger
/// a reload; deploying a new UI does.
fn ui_build() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in HTML.as_bytes().iter().chain(CSS.as_bytes()) {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{h:016x}")
    })
}

/// Trained-uma roster the Veterans page reads (overseer_umas/veterans.json, the umas export's output).
/// Returned verbatim — the SPA parses the TrainedChara objects. Never panics: a missing / unreadable /
/// non-array file yields an empty roster.
fn veterans() -> String {
    let path = crate::paths::dll_dir().join("overseer_umas").join("veterans.json");
    let arr = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let count = arr.len();
    serde_json::json!({ "success": true, "veterans": arr, "count": count }).to_string()
}

/// The AI Brain page's learned-model snapshot. An advisor-written learned.json
/// (advisor/uma_runtime/ai/learned.json) is an OVERRIDE hook: returned verbatim when present +
/// valid JSON. Otherwise the payload is COMPUTED from what the passive observer actually recorded —
/// completed career runs (career.rs) + decoded race history (race_reveal.rs). Never panics.
fn ai_learned() -> String {
    let path = crate::paths::dll_dir()
        .join("advisor")
        .join("uma_runtime")
        .join("ai")
        .join("learned.json");
    if let Ok(s) = std::fs::read_to_string(&path) {
        if serde_json::from_str::<serde_json::Value>(&s).is_ok() {
            return s;
        }
    }
    ai_learned_computed()
}

/// The computed AI Brain payload: run counts + per-run action tallies (RunRecord.act_*) and
/// per-race-program stats from the race history. All zeros/empties when nothing is recorded yet.
fn ai_learned_computed() -> String {
    let runs = crate::career::runs_snapshot();
    let hist = crate::race_reveal::history_snapshot();

    // Action tallies summed across every recorded run (legacy runs default to 0 → drop out).
    let (mut train, mut race, mut rest, mut skill, mut other) = (0u64, 0u64, 0u64, 0u64, 0u64);
    for r in &runs {
        train += r.act_train as u64;
        race += r.act_race as u64;
        rest += r.act_rest as u64;
        skill += r.act_skill as u64;
        other += r.act_other as u64;
    }
    let turn_decisions = train + race + rest + skill + other;

    // Per-race "programs": one row per distinct non-empty race name. distance/grade come from any
    // record of that race (first non-zero seen; 0 is fine when the header capture missed them).
    struct Prog {
        distance: i64,
        grade: i64,
        starts: u64,
        wins: u64,
    }
    let mut by_name: std::collections::HashMap<String, Prog> = std::collections::HashMap::new();
    for r in hist.iter().filter(|r| !r.name.is_empty()) {
        let e = by_name.entry(r.name.clone()).or_insert(Prog { distance: 0, grade: 0, starts: 0, wins: 0 });
        if e.distance == 0 {
            e.distance = r.distance;
        }
        if e.grade == 0 {
            e.grade = r.grade;
        }
        e.starts += 1;
        if r.place == 1 {
            e.wins += 1;
        }
    }
    let races_learned = by_name.len();
    let mut progs: Vec<(String, Prog)> = by_name.into_iter().collect();
    progs.sort_by(|a, b| b.1.starts.cmp(&a.1.starts).then_with(|| a.0.cmp(&b.0)));
    let programs: Vec<serde_json::Value> = progs
        .iter()
        .take(30)
        .map(|(name, p)| {
            let win_rate = if p.starts > 0 {
                ((p.wins as f64 / p.starts as f64) * 10000.0).round() / 10000.0
            } else {
                0.0
            };
            serde_json::json!({
                "name": name,
                "distance": p.distance,
                "grade": p.grade,
                "starts": p.starts,
                "wins": p.wins,
                "win_rate": win_rate,
            })
        })
        .collect();

    // Observed action mix — zero-count keys omitted (the page hides empty bars).
    let mut actions = serde_json::Map::new();
    for (key, n) in [("train", train), ("race", race), ("rest", rest), ("skills", skill)] {
        if n > 0 {
            actions.insert(key.into(), serde_json::json!({ "count": n }));
        }
    }

    serde_json::json!({
        "data": {
            "careers": runs.len(),
            "turn_decisions": turn_decisions,
            "races_learned": races_learned,
            "total_races": hist.len(),
            "event_choices": 0,
            "completion_rate": serde_json::Value::Null,
        },
        "programs": programs,
        "actions": actions,
        "events": [],
        "tuning": [],
    })
    .to_string()
}

/// GET /api/races/history — the persisted per-race telemetry (race_reveal.rs), newest first (capped
/// at 200 rows in the response; the summary covers ALL records). Zeros/empty on no history.
fn races_history() -> String {
    let hist = crate::race_reveal::history_snapshot();
    let total = hist.len();
    let wins = hist.iter().filter(|r| r.place == 1).count();
    let podium = hist.iter().filter(|r| (1..=3).contains(&r.place)).count();
    let rate = |n: usize| {
        if total > 0 {
            ((n as f64 / total as f64) * 10000.0).round() / 10000.0
        } else {
            0.0
        }
    };
    let avg_field = if total > 0 {
        let s: i64 = hist.iter().map(|r| r.field).sum();
        ((s as f64 / total as f64) * 100.0).round() / 100.0
    } else {
        0.0
    };
    let races: Vec<&crate::race_reveal::RaceRecord> = hist.iter().rev().take(200).collect();
    serde_json::json!({
        "races": races,
        "summary": {
            "total": total,
            "wins": wins,
            "podium": podium,
            "win_rate": rate(wins),      // fraction 0.0–1.0, 4 decimals
            "podium_rate": rate(podium), // fraction 0.0–1.0, 4 decimals
            "avg_field": avg_field,      // 2 decimals; 0 when no history
        },
    })
    .to_string()
}

/// Translation status. The glossary/MTL engine is ported in Phase 2; until then
/// this reports "off / not attached" honestly so the SPA renders cleanly.
/// Translation status for the web UI: the 26 glossary languages (flagging which have data installed),
/// the active language + its loaded counts, and whether a translation pack is also loaded.
fn translation_languages() -> String {
    let active = crate::settings::tl_lang().unwrap_or_default();
    let (ui_labels, terms) = crate::glossary::counts();
    let ready = crate::ipc::latest().core_ready;
    let pack_loaded = crate::localize::is_loaded();

    let mut languages: Vec<serde_json::Value> = crate::glossary::LANGUAGES
        .iter()
        .map(|(code, name)| {
            serde_json::json!({ "code": code, "name": name, "mtl_model": crate::glossary::has_data(code) })
        })
        .collect();
    // English is the SOURCE on Global (hence absent from glossary::LANGUAGES) but a VALID target on the
    // JP client (JP→English). Offer it as a selectable target ONLY there — never on Global, where it
    // would mean English→English. mtl::target_flores("en") already maps to "eng_Latn".
    if crate::loc_ui::is_jp_client() {
        languages.push(serde_json::json!({ "code": "en", "name": "English", "mtl_model": crate::glossary::has_data("en") }));
    }

    serde_json::json!({
        "languages": languages,
        "manager": {
            "active": if active.is_empty() { serde_json::Value::Null } else { serde_json::json!(active) },
            "active_name": crate::glossary::active_name(),
            "mtl_enabled": crate::settings::mtl_enabled(),
            "mtl_model_ready": crate::nllb::ready(),
            "mtl_model_present": crate::nllb::model_present(),
            "mtl_hq": crate::settings::mtl_hq(),
            "hq_present": crate::nllb::hq_model_present(),
            "learned": crate::mtl::cache_count(),
            "dict_size": ui_labels + terms,
            "argos": crate::nllb::model_present(),
            "pack_loaded": pack_loaded,
        },
        "agent": {
            "installed": ready,
            "hookNames": if ready {
                serde_json::json!(["Localize.Get", "Sqlite3", "StoryTimeline", "AssetBundle", "Font"])
            } else {
                serde_json::json!([])
            },
            "seen": 0,
            "replaced": 0,
        },
    })
    .to_string()
}

/// Set the active glossary language (POST /api/translation/language {code}). Empty/null → off.
fn handle_language(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let raw = v.get("code").and_then(|x| x.as_str()).unwrap_or("");
    // Accept only a known language code (or empty = off). The code is joined into filesystem paths
    // (glossary/<code>/…) and persisted, so an arbitrary value would be a path-traversal vector.
    // "en" is not in glossary::LANGUAGES (English is the source on Global) but is a valid JP→English
    // target on the JP client; allow that one fixed literal through there (no traversal risk).
    let code = if raw.is_empty() {
        None
    } else if crate::glossary::LANGUAGES.iter().any(|(c, _)| *c == raw)
        || (raw == "en" && crate::loc_ui::is_jp_client())
    {
        Some(raw.to_string())
    } else {
        return r#"{"ok":false,"error":"unknown language code"}"#.into();
    };
    crate::settings::set_tl_lang(code);
    crate::glossary::reload();
    crate::mtl::flush(); // persist the OLD language's pending MTL cache before switching
    crate::mtl::reload(); // load the new language's mtl.json + clear transient state (bumps epoch)
    crate::loc_settext::reset_emitted(); // new language → drop the old-language emitted-output guard
    r#"{"ok":true}"#.into()
}

/// Toggle the NLLB machine-translation fallback (POST /api/translation/mtl {enabled|on}).
fn handle_mtl(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let on = v
        .get("enabled")
        .or_else(|| v.get("on"))
        .and_then(|x| x.as_bool());
    if let Some(on) = on {
        crate::settings::set_mtl_enabled(on);
    }
    serde_json::json!({
        "ok": true,
        "mtl_enabled": crate::settings::mtl_enabled(),
        "mtl_model_ready": crate::nllb::ready(),
        "mtl_model_present": crate::nllb::model_present(),
    })
    .to_string()
}

/// Toggle the higher-quality 1.3B model (POST /api/translation/hq {on}); hot-reloads the engine.
fn handle_hq(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    if let Some(on) = v.get("on").or_else(|| v.get("enabled")).and_then(|x| x.as_bool()) {
        // Refuse to switch to HQ if that model isn't installed (would just go "not ready").
        if on && !crate::nllb::hq_model_present() {
            return r#"{"ok":false,"error":"1.3B model not installed"}"#.into();
        }
        crate::settings::set_mtl_hq(on);
        crate::nllb::reload_model();
    }
    serde_json::json!({
        "ok": true,
        "mtl_hq": crate::settings::mtl_hq(),
        "hq_present": crate::nllb::hq_model_present(),
    })
    .to_string()
}

/// Read the active language's manual overrides (GET /api/translation/manual).
fn get_manual() -> String {
    let lang = crate::settings::tl_lang().unwrap_or_default();
    if lang.is_empty() {
        return r#"{"lang":null,"entries":{}}"#.into();
    }
    let path = crate::paths::dll_dir().join("glossary").join(&lang).join("manual.json");
    let entries: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    serde_json::json!({ "lang": lang, "entries": entries }).to_string()
}

/// Upsert (or delete, if target empty) a manual override (POST /api/translation/manual
/// {source, target}). Persists to `<lang>/manual.json` and hot-reloads so it applies immediately.
fn handle_manual(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let source = v.get("source").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let target = v.get("target").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let lang = crate::settings::tl_lang().unwrap_or_default();
    if lang.is_empty() || source.is_empty() {
        return r#"{"ok":false,"error":"no language / empty source"}"#.into();
    }
    let dir = crate::paths::dll_dir().join("glossary").join(&lang);
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("manual.json");
    let mut map: std::collections::HashMap<String, String> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if target.is_empty() {
        map.remove(&source);
    } else {
        map.insert(source, target);
    }
    if let Ok(json) = serde_json::to_string_pretty(&map) {
        let tmp = dir.join("manual.json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
    crate::mtl::reload(); // manual overrides load into CACHE (manual over mtl) → applies immediately
    serde_json::json!({ "ok": true, "count": map.len() }).to_string()
}

/// Export a whole glossary file for the active language (feature 6). Selector: POST body
/// {which:"manual"|"mtl"} — the router strips query strings, so ?which= can't travel on a GET; a
/// plain GET (empty body) exports the manual overrides. No active language / missing file →
/// ok:true with empty entries (nothing to export is not an error).
fn glossary_export(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let which = match v.get("which").and_then(|x| x.as_str()) {
        Some("mtl") => "mtl",
        _ => "manual",
    };
    let lang = crate::settings::tl_lang().unwrap_or_default();
    if lang.is_empty() {
        return serde_json::json!({
            "ok": true, "lang": serde_json::Value::Null, "which": which, "count": 0, "entries": {},
        })
        .to_string();
    }
    let path = crate::paths::dll_dir().join("glossary").join(&lang).join(format!("{which}.json"));
    let entries: std::collections::HashMap<String, String> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    serde_json::json!({
        "ok": true, "lang": lang, "which": which, "count": entries.len(), "entries": entries,
    })
    .to_string()
}

/// Import entries into the active language's manual.json (feature 6). POST body
/// {entries:{src:dst}, mode:"merge"|"replace" (default merge)}. Validates a string→string map
/// (≤ 50000 entries), writes atomically (tmp + rename — same pattern as handle_manual), then
/// hot-reloads the MTL cache so the imported overrides apply immediately.
fn glossary_import(body: &str) -> String {
    let lang = crate::settings::tl_lang().unwrap_or_default();
    if lang.is_empty() {
        return r#"{"ok":false,"err":"no active language"}"#.into();
    }
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let entries_obj = match v.get("entries").and_then(|x| x.as_object()) {
        Some(o) => o,
        None => return r#"{"ok":false,"err":"entries must be a string->string map"}"#.into(),
    };
    if entries_obj.len() > 50_000 {
        return r#"{"ok":false,"err":"too many entries (max 50000)"}"#.into();
    }
    // Every value must be a string (a wrong-shaped file should fail loudly, not half-import);
    // empty keys/values are silently dropped — mtl::reload discards them anyway.
    let mut incoming: std::collections::HashMap<String, String> =
        std::collections::HashMap::with_capacity(entries_obj.len());
    for (k, val) in entries_obj {
        match val.as_str() {
            Some(s) => {
                if !k.is_empty() && !s.is_empty() {
                    incoming.insert(k.clone(), s.to_string());
                }
            }
            None => return r#"{"ok":false,"err":"entries must be a string->string map"}"#.into(),
        }
    }
    let replace = v.get("mode").and_then(|x| x.as_str()) == Some("replace");
    let dir = crate::paths::dll_dir().join("glossary").join(&lang);
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("manual.json");
    let prev: std::collections::HashMap<String, String> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    // "added" = imported keys the file didn't have before (in replace mode too, so the number
    // answers "how many NEW translations did this import bring").
    let added = incoming.keys().filter(|k| !prev.contains_key(*k)).count();
    let mut merged = if replace { std::collections::HashMap::new() } else { prev };
    merged.extend(incoming);
    match serde_json::to_string_pretty(&merged) {
        Ok(json) => {
            let tmp = dir.join("manual.json.tmp");
            if std::fs::write(&tmp, json).is_err() || std::fs::rename(&tmp, &path).is_err() {
                return r#"{"ok":false,"err":"write failed"}"#.into();
            }
        }
        Err(_) => return r#"{"ok":false,"err":"write failed"}"#.into(),
    }
    crate::mtl::reload(); // manual overrides load into the MTL cache → apply immediately
    serde_json::json!({ "ok": true, "lang": lang, "added": added, "total": merged.len() }).to_string()
}

/// Read the user's protected-name list (GET /api/translation/names).
fn get_protected_names() -> String {
    serde_json::json!({ "names": crate::settings::protected_names() }).to_string()
}

/// Recent translations feed (GET /api/translation/recent): the last on-screen text swaps as
/// {source, output} pairs, NEWEST first, capped at 50. Drives the web UI's click-to-fix flow.
/// serde_json handles escaping; never panics.
fn translation_recent() -> String {
    let recent: Vec<serde_json::Value> = crate::mtl::recent(50)
        .into_iter()
        .map(|(source, output)| serde_json::json!({ "source": source, "output": output }))
        .collect();
    serde_json::json!({ "recent": recent }).to_string()
}

/// Replace the user's protected-name list (POST /api/translation/names {names:[...]}). These names are
/// kept in the original language (never translated) — the player's own trainer name, friends, etc.
fn handle_protected_names(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let list: Vec<String> = v
        .get("names")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    crate::settings::set_protected_names(list);
    get_protected_names()
}

fn handle_fps(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    if let Some(fps) = v.get("fps").and_then(|x| x.as_i64()) {
        let fps = fps as i32;
        if fps != 0 {
            LAST_FPS_TARGET.store(fps, Ordering::Relaxed);
        }
        crate::performance::fps::request(fps);
    } else if let Some(enabled) = v.get("enabled").and_then(|x| x.as_bool()) {
        if enabled {
            let t = LAST_FPS_TARGET.load(Ordering::Relaxed);
            crate::performance::fps::request(if t == 0 { 60 } else { t });
        } else {
            crate::performance::fps::request(0);
        }
    }
    crate::settings::save_current();
    r#"{"ok":true}"#.into()
}

fn handle_graphics(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    if let Some(x) = v.get("res_mult").and_then(|x| x.as_f64()) {
        crate::settings::set_render_scale(x as f32);
    }
    if let Some(x) = v.get("quality").and_then(|x| x.as_i64()) {
        // Overseer forces the full toon-quality tier (a single unlock), not a numeric tier.
        crate::settings::set_gfx_quality(x >= 3);
    }
    // aniso / texture_limit / lod_bias / shadow_res are accepted (so an old client still applies)
    // but ignored: Unity stripped those QualitySettings setters out of this build entirely. The real
    // knobs live on the Performance page — see performance::graphics.
    r#"{"ok":true}"#.into()
}

fn handle_overlay(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    if let Some(show) = v.get("show").and_then(|x| x.as_bool()) {
        OVERLAY_ON.store(show, Ordering::Relaxed);
    }
    r#"{"ok":true}"#.into()
}

/// True when the in-game FPS counter should draw (the overlay reads this). Set by the web toggle.
pub fn fps_counter_on() -> bool {
    OVERLAY_ON.load(Ordering::Relaxed)
}

/// Translation-pack state: whether a localized_data/ translation pack is loaded, its dict sizes,
/// and whether its hooks are armed (they install at boot, so a pack dropped in mid-session shows
/// loaded=true / hooks_armed=false until the next launch).
fn pack_state() -> String {
    let ld = crate::localize::data();
    let loaded = crate::localize::is_loaded();
    let (ui, hashed, db) = ld.stats();
    serde_json::json!({
        "loaded": loaded,
        "ui_strings": ui,
        "hashed": hashed,
        "db_text": db,
        "has_font": ld.config.replacement_font_name.is_some(),
        "hooks_armed": crate::diag::install_state("loc_db").starts_with("armed"),
    })
    .to_string()
}

/// Self-updater state for the SPA: the running version, whether the feature is configured at all,
/// and any pending update (target tag, how many versions ahead, changelog, hotfix flag). The banner
/// and the sidebar footer both read this.
fn update_status() -> String {
    let pending = crate::selfupdate::pending();
    let (has, target, count, changelog, same, staged) = match &pending {
        Some(p) => (
            true,
            p.target.clone(),
            p.count,
            p.changelog.clone(),
            p.same_version,
            crate::selfupdate::staged(),
        ),
        None => (false, String::new(), 0, String::new(), false, false),
    };
    serde_json::json!({
        "version": crate::selfupdate::current_version(),
        "enabled": crate::selfupdate::enabled(), // false = no repo configured → updater dormant
        "busy": crate::selfupdate::is_busy(),
        "status": crate::selfupdate::status(),
        "update": {
            "available": has,
            "target": target,       // e.g. "v3.6.0"
            "count": count,         // versions ahead (0 for a same-tag hotfix)
            "hotfix": same,         // true = fixed DLL re-published under the same version
            "staged": staged,       // downloaded, waiting to apply on restart
            "changelog": changelog, // combined, newest-first
        },
    })
    .to_string()
}

/// Gameplay controls state for the web sidebar — all of Overseer's toggles that used to live in the
/// in-game menu (which is now FPS-only).
fn gameplay_state() -> String {
    serde_json::json!({
        "bot_enabled": crate::settings::bot_enabled(),
        // RAW toggle values, on purpose: a checkbox shows what the user CHOSE, and the master pill +
        // OFF banner show whether it's currently allowed to act. Rendering the gated is_* values here
        // made every checkbox appear unchecked while the bot was off — inviting the user to "re-check"
        // them (churning their saved prefs) and feeding the gated read-back that save_current used to
        // persist. (skip_scene was removed with its dead checkbox: no implementation behind it.)
        "skip_training": crate::skip::raw_train_enabled(),
        "skip_events": crate::skip::raw_event_enabled(),
        "skip_shop": crate::skip::raw_shop_enabled(),
        "skip_rival": crate::skip::raw_rival_enabled(),
        "skip_race_result": crate::skip::raw_race_result_enabled(),
        "hyper_skip": crate::skip::raw_hyper_enabled(),
        "skip_warnings": crate::skip::raw_warnings_enabled(),
        "skip_inspiration": crate::skip::raw_inspiration_enabled(),
        "skip_skill_learn": crate::skip::raw_skill_learn_enabled(),
        "event_auto_choice": crate::skip::raw_event_choice_enabled(),
        "race_ff": crate::skip::raw_race_ff_enabled(),
        "tempo": crate::ui_tempo::stored_tempo(),
        "fps_counter": OVERLAY_ON.load(Ordering::Relaxed),
    })
    .to_string()
}

/// Apply gameplay control changes from the web sidebar (POST /api/mod/gameplay {field: value}).
fn handle_gameplay(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let b = |k: &str| v.get(k).and_then(|x| x.as_bool());
    if let Some(x) = b("bot_enabled") {
        crate::settings::set_bot_enabled(x); // master power switch
    }
    if let Some(x) = b("skip_training") {
        crate::skip::set_train_enabled(x);
    }
    if let Some(x) = b("skip_events") {
        crate::skip::set_event_enabled(x);
    }
    if let Some(x) = b("skip_shop") {
        crate::skip::set_shop_enabled(x);
    }
    if let Some(x) = b("skip_rival") {
        crate::skip::set_rival_enabled(x);
    }
    if let Some(x) = b("skip_race_result") {
        crate::skip::set_race_result_enabled(x);
    }
    if let Some(x) = b("hyper_skip") {
        crate::skip::set_hyper_enabled(x);
    }
    if let Some(x) = b("skip_warnings") {
        crate::skip::set_warnings_enabled(x);
    }
    if let Some(x) = b("skip_inspiration") {
        crate::skip::set_inspiration_enabled(x);
    }
    if let Some(x) = b("skip_skill_learn") {
        crate::skip::set_skill_learn_enabled(x);
    }
    if let Some(x) = b("event_auto_choice") {
        crate::skip::set_event_choice_enabled(x);
    }
    if let Some(x) = b("race_ff") {
        crate::skip::set_race_ff_enabled(x);
    }
    if let Some(x) = v.get("tempo").and_then(|x| x.as_f64()) {
        crate::ui_tempo::set_tempo(x as f32);
    }
    if let Some(x) = b("fps_counter") {
        OVERLAY_ON.store(x, Ordering::Relaxed);
    }
    crate::settings::save_current(); // persist skip/tempo live state
    r#"{"ok":true}"#.into()
}

/// Performance-page state (GET /api/mod/performance): every FPS / graphics / display-window
/// control + the true measured FPS. `fps` is the cap request (0 = uncapped-off, -1 = unlimited, N =
/// cap at N).
fn performance_state() -> String {
    let fps = crate::performance::fps::current();
    serde_json::json!({
        "low_spec": crate::settings::low_spec(),
        "low_level": crate::settings::low_level(),
        // Measured frame interval + the tween pump's share of it (see ui_tempo::frame_stats).
        "frame_ms": crate::ui_tempo::frame_stats().0,
        "tempo_budget_ms": crate::ui_tempo::frame_stats().1,
        "fps": fps,
        "fps_cap_on": fps > 0, // a finite cap (Unlimited is -1, off is 0)
        "fps_unlimited": fps == -1,
        "measured_fps": crate::overlay::measured_fps(),
        "fps_counter": OVERLAY_ON.load(Ordering::Relaxed),
        "gfx_quality": crate::settings::gfx_quality(),
        "gfx_aa": crate::settings::gfx_aa(),
        "gfx_shadows": crate::settings::gfx_shadows(),
        "always_on_top": crate::settings::always_on_top(),
        "block_minimize": crate::settings::block_minimize(),
        "display_mode": crate::settings::display_mode(),
        // How many times the game has re-applied graphics quality since launch. The graphics knobs
        // only take effect inside that pass, so 0 = nothing has reloaded yet and no knob can show.
        "gfx_applies": crate::performance::graphics::apply_count(),
        // What the ENGINE reports back — proof the writes landed (not just stored).
        "actual_aa": crate::performance::graphics::readback().0,
        "actual_shadow_dist": crate::performance::graphics::readback().1,
        "gfx_shadow_dist": crate::settings::gfx_shadow_dist(),
        // Accessibility: colour-vision (daltonization) filter mode (0 off, 1 deuter, 2 protan, 3 tritan).
        "cvd": crate::settings::cvd_mode(),
        "cvd_strength": crate::settings::cvd_strength(),
    })
    .to_string()
}

/// Apply Performance-page changes (POST /api/mod/performance {field: value}). Every field is optional,
/// so the SPA can send just the one that changed. All setters persist + push to the live module.
fn handle_performance(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let b = |k: &str| v.get(k).and_then(|x| x.as_bool());

    // Low-resources master first — it fans out to graphics/cloth/display, and an explicit per-knob
    // change in the same request should win over it (so we apply it before the individual knobs).
    if let Some(x) = v.get("low_level").and_then(|x| x.as_i64()) {
        crate::settings::set_low_level(x as i32);
    }
    if let Some(x) = b("low_spec") {
        crate::settings::set_low_spec(x);
    }
    // FPS cap: numeric target (0 off, -1 unlimited, N cap) or the on/off + unlimited convenience bools.
    if let Some(fps) = v.get("fps").and_then(|x| x.as_i64()) {
        let fps = fps as i32;
        if fps > 0 {
            LAST_FPS_TARGET.store(fps, Ordering::Relaxed);
        }
        crate::performance::fps::request(fps);
    } else if let Some(unlimited) = b("fps_unlimited") {
        crate::performance::fps::request(if unlimited { -1 } else { 0 });
    } else if let Some(on) = b("fps_cap_on") {
        if on {
            let t = LAST_FPS_TARGET.load(Ordering::Relaxed);
            crate::performance::fps::request(if t == 0 { 60 } else { t });
        } else {
            crate::performance::fps::request(0);
        }
    }
    if let Some(x) = b("gfx_quality") {
        crate::settings::set_gfx_quality(x);
    }
    // Granular graphics knobs (integers; -1 = leave the game's value). Shadow distance is a float.
    let i = |k: &str| v.get(k).and_then(|x| x.as_i64()).map(|n| n as i32);
    if let Some(x) = i("gfx_aa") {
        crate::settings::set_gfx_aa(x);
    }
    if let Some(x) = i("gfx_shadows") {
        crate::settings::set_gfx_shadows(x);
    }
    if let Some(x) = v.get("gfx_shadow_dist").and_then(|x| x.as_f64()) {
        crate::settings::set_gfx_shadow_dist(x as f32);
    }
    if let Some(x) = b("always_on_top") {
        crate::settings::set_always_on_top(x);
    }
    if let Some(x) = b("block_minimize") {
        crate::settings::set_block_minimize(x);
    }
    if let Some(x) = v.get("display_mode").and_then(|x| x.as_i64()) {
        crate::settings::set_display_mode(x.clamp(0, 3) as i32);
    }
    if let Some(x) = b("fps_counter") {
        OVERLAY_ON.store(x, Ordering::Relaxed);
    }
    crate::settings::save_current(); // persist fps/overlay live state (the perf setters persist their own)
    performance_state()
}

/// Set the colour-vision (daltonization) accessibility filter (POST /api/mod/cvd {"mode": 0..3}).
/// 0 off, 1 deuteranopia, 2 protanopia, 3 tritanopia. Out-of-range is rejected; the setter persists
/// and pushes live to the render-thread filter. Responds {"ok":bool,"mode":i32}.
fn handle_cvd(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    // Strength-only updates carry no mode; keep the current one.
    let mode = v
        .get("mode")
        .and_then(|x| x.as_i64())
        .unwrap_or(crate::settings::cvd_mode() as i64);
    if !(0..=3).contains(&mode) {
        return r#"{"ok":false,"error":"mode must be 0..3"}"#.into();
    }
    let mode = mode as i32;
    crate::settings::set_cvd_mode(mode);
    if let Some(st) = v.get("strength").and_then(|x| x.as_f64()) {
        crate::settings::set_cvd_strength(st as f32);
    }
    serde_json::json!({
        "ok": true,
        "mode": mode,
        "strength": crate::settings::cvd_strength(),
    })
    .to_string()
}

/// Read the tail (last `max_bytes`) of a log file as UTF-8 (lossy — the log holds em-dashes etc.).
/// Reading only the tail keeps the polled Logs viewer cheap even when the log grows large.
fn read_tail(name: &str, max_bytes: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let path = crate::paths::log_file(name);
    let mut f = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max_bytes);
    let _ = f.seek(SeekFrom::Start(start));
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        // We may have started mid-line — drop the partial first line.
        if let Some(i) = s.find('\n') {
            s = s[i + 1..].to_string();
        }
    }
    s
}

/// Live tail for the Logs console.
///
/// PERF: this used to read and return the last 400 KB on EVERY poll, and the SPA polls it every two
/// seconds for as long as the page is open — a steady ~200 KB/s of disk reads plus the same amount
/// of HTTP traffic and JS string work, forever, whether or not anything was logged. The tail is now
/// 128 KB (still far more than the console displays) and is served from a short-lived cache keyed on
/// the file's size, so repeat polls with no new lines cost a `metadata()` call. The buffered writer
/// is flushed first so the console is never stale.
fn logs_tail() -> String {
    use std::sync::Mutex;
    static CACHE: Mutex<(u64, String)> = Mutex::new((0, String::new()));
    crate::tools::log::flush_blocking();
    let path = crate::paths::log_file("overseer-native.log");
    let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if let Ok(c) = CACHE.lock() {
        if c.0 == len && len != 0 {
            return c.1.clone();
        }
    }
    let text = read_tail("overseer-native.log", 128 * 1024);
    if let Ok(mut c) = CACHE.lock() {
        *c = (len, text.clone());
    }
    text
}

/// The whole log, for the Export button.
fn logs_full() -> String {
    let path = crate::paths::log_file("overseer-native.log");
    std::fs::read(&path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

fn write_response(stream: &mut TcpStream, status: &str, ctype: &str, body: &str) -> std::io::Result<()> {
    write_bytes(stream, status, ctype, "no-store", body.as_bytes())
}

/// The one response writer. `cache` is the Cache-Control value: every dynamic route sends
/// `no-store` (state must never be served stale), while the immutable baked-in assets send a long
/// max-age so the banners aren't re-sent on every visit to the Catalogue page.
fn write_bytes(
    stream: &mut TcpStream,
    status: &str,
    ctype: &str,
    cache: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: {cache}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::{promo_asset, ui_build, HTML, PROMO};

    /// The SPA reloads itself when this value changes, so it must be CONSTANT within a process.
    /// A stamp that varied between polls would put every open tab into a reload loop.
    #[test]
    fn the_ui_stamp_never_changes_under_a_running_server() {
        let a = ui_build();
        let b = ui_build();
        assert_eq!(a, b);
        assert_eq!(a.len(), 16, "16 hex chars: {a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Every banner the Catalogue page asks for must actually resolve, and be a real PNG. A typo in
    /// either the markup or the PROMO table is otherwise a broken image nobody notices until a user
    /// opens the page.
    #[test]
    fn every_catalogue_banner_resolves_to_a_png() {
        assert!(!PROMO.is_empty());
        for (name, bytes) in PROMO {
            let path = format!("/assets/promo/{name}");
            let got = promo_asset(&path).unwrap_or_else(|| panic!("{path} did not resolve"));
            assert_eq!(got.len(), bytes.len());
            assert_eq!(&got[..8], b"\x89PNG\r\n\x1a\n", "{name} is not a PNG");
            assert!(
                HTML.contains(&path),
                "{path} is embedded but no card references it"
            );
        }
    }

    /// The name is matched by EQUALITY against a fixed table, never joined onto a filesystem path,
    /// so nothing outside that table can be reached however the request is spelled.
    #[test]
    fn the_asset_route_serves_nothing_but_the_table() {
        for path in [
            "/assets/promo/../../../../Windows/win.ini",
            "/assets/promo/..%2fsecret",
            "/assets/promo/",
            "/assets/promo/overseer.png/../icarus.png",
            "/assets/promo/OVERSEER.PNG", // matching is exact, not case-folded
            "/assets/other/overseer.png",
            "/overseer.png",
        ] {
            assert!(promo_asset(path).is_none(), "{path} must not resolve");
        }
    }
}
