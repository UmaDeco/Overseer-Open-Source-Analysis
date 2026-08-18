//! Shared in-process state store (Plan B).
//!
//! In Plan A this was a loopback TCP server receiving GameState JSON from the
//! Python host. In Plan B everything is in-process: the native readers
//! (state.rs, race.rs) write directly here and the overlay reads via `latest()`.
//! No sockets, no JSON, no Python. The module name is kept as `ipc` only to
//! avoid churn in the `mod` graph — it is now a plain RwLock-backed store.

use crate::data::{Account, CareerState, GameState, RaceState};
use once_cell::sync::Lazy;
use std::sync::{Mutex, RwLock};

/// The live game state. The overlay clones it cheaply per frame via `latest()`.
static STATE: Lazy<RwLock<GameState>> = Lazy::new(|| RwLock::new(GameState::default()));
/// Account-level resources (web UI top-bar), updated from response_hook's capture consumer.
static ACCOUNT: Lazy<RwLock<Account>> = Lazy::new(|| RwLock::new(Account::default()));
/// One-line engine status shown in the overlay footer.
static STATUS: Lazy<Mutex<String>> = Lazy::new(|| Mutex::new("starting…".into()));

pub fn latest() -> GameState {
    STATE.read().map(|g| g.clone()).unwrap_or_default()
}

/// Native career reader → publishes the latest CareerState.
pub fn set_career(c: CareerState) {
    if let Ok(mut g) = STATE.write() {
        g.career = c;
    }
}

/// Mark the native engine as booted (all IL2CPP hooks installed). Read by the
/// web UI server (webui.rs) to report "attached" vs "waiting for game".
pub fn set_core_ready(ready: bool) {
    if let Ok(mut g) = STATE.write() {
        g.core_ready = ready;
    }
}

/// Record which native modules installed successfully (for the web UI status).
pub fn set_core_modules(mods: Vec<String>) {
    if let Ok(mut g) = STATE.write() {
        g.core_modules = mods;
    }
}

/// Latest account resources (web UI top-bar strip).
pub fn account() -> Account {
    ACCOUNT.read().map(|a| a.clone()).unwrap_or_default()
}

/// Capture consumer (response_hook) → publishes the account resources.
pub fn set_account(a: Account) {
    if let Ok(mut g) = ACCOUNT.write() {
        *g = a;
    }
}

/// Mark whether a career (single-mode run) is currently in progress.
pub fn set_career_present(present: bool) {
    if let Ok(mut g) = STATE.write() {
        g.career.present = present;
    }
}

/// Mutate the race state in place (for incremental frame/event updates).
pub fn with_race<F: FnOnce(&mut RaceState)>(f: F) {
    if let Ok(mut g) = STATE.write() {
        f(&mut g.race);
    }
}

pub fn status() -> String {
    STATUS.lock().map(|s| s.clone()).unwrap_or_default()
}

pub fn set_status(s: impl Into<String>) {
    if let Ok(mut g) = STATUS.lock() {
        *g = s.into();
    }
}

// ─── advisor sidecar bridge ─────────────────────────────────────────────────────────────────────
//
// The DLL captures the game's decrypted career/race network payload and publishes the RAW msgpack
// bytes here (CAPTURE). The Python advisor sidecar long-polls `/internal/capture`, computes advice
// with Icarus's career_bot, and POSTs it back to `/internal/advice`, which lands in ADVICE. The SPA
// reads ADVICE via the existing `/api/advisor/state`. The DLL is a pure hub — it never parses the
// advice JSON (so the career_bot contract can evolve without Rust changes).

/// Latest captured career/race payload awaiting the sidecar.
#[derive(Clone, Default)]
pub struct Capture {
    pub seq: u64,
    pub kind: &'static str, // "career" | "race" | ""
    pub bytes: Vec<u8>,
}
static CAPTURE: Lazy<RwLock<Capture>> = Lazy::new(|| RwLock::new(Capture::default()));

/// Advice/reveal JSON the sidecar produced (stored verbatim as strings; the DLL never parses them).
#[derive(Clone, Default)]
pub struct AdviceCache {
    pub seq_seen: u64,
    pub advice_json: Option<String>,
    pub reveal_json: Option<String>,
    pub heartbeat_ms: u64,
    pub ok: bool,
    pub err: Option<String>,
}
static ADVICE: Lazy<RwLock<AdviceCache>> = Lazy::new(|| RwLock::new(AdviceCache::default()));

/// Per-boot random token; the sidecar must present it on `/internal/*` (defeats a stale process /
/// cross-tool access on the shared port). Set once at boot.
static TOKEN: Lazy<RwLock<String>> = Lazy::new(|| RwLock::new(String::new()));

/// Publish a freshly-captured payload (response_hook, on the game net thread — cheap, non-blocking).
pub fn set_capture(kind: &'static str, bytes: Vec<u8>) {
    if let Ok(mut c) = CAPTURE.write() {
        c.seq = c.seq.wrapping_add(1);
        c.kind = kind;
        c.bytes = bytes;
    }
}
pub fn capture() -> Capture {
    CAPTURE.read().map(|c| c.clone()).unwrap_or_default()
}
pub fn capture_seq() -> u64 {
    CAPTURE.read().map(|c| c.seq).unwrap_or(0)
}

/// Store advice/reveal the sidecar POSTed (`/internal/advice`).
pub fn set_advice(seq: u64, advice: Option<String>, reveal: Option<String>, ok: bool, err: Option<String>, now_ms: u64) {
    if let Ok(mut a) = ADVICE.write() {
        a.seq_seen = seq;
        // Keep the last non-null advice/reveal so a heartbeat post doesn't blank the screen.
        if advice.is_some() {
            a.advice_json = advice;
        }
        if reveal.is_some() {
            a.reveal_json = reveal;
        }
        a.ok = ok;
        a.err = err;
        a.heartbeat_ms = now_ms;
    }
}
pub fn advice_cache() -> AdviceCache {
    ADVICE.read().map(|a| a.clone()).unwrap_or_default()
}

/// Publish a race REVEAL decoded in-process (pure-Rust `race_reveal`, on the ov-capture worker). Kept
/// separate from `set_advice` so it doesn't touch the sidecar's heartbeat/seq/ok fields — the Rust
/// reveal and the (optional) Python advisor coexist: the sidecar posts reveal=None, so it never
/// clobbers this, and this never blanks the advice.
pub fn set_reveal(json: String) {
    if let Ok(mut a) = ADVICE.write() {
        a.reveal_json = Some(json);
    }
}

/// Drop the decoded race result.
///
/// MUST be called when a new race is entered. The result only exists from `race_start` onward, but
/// the panel kept showing the PREVIOUS race's finish until then — so at the entry screen it
/// confidently displayed a stale "1st" for a race that hadn't been run. The decode was right; the
/// staleness made it a lie at exactly the moment someone would act on it.
pub fn clear_reveal() {
    if let Ok(mut a) = ADVICE.write() {
        a.reveal_json = None;
    }
}

pub fn set_token(t: String) {
    if let Ok(mut g) = TOKEN.write() {
        *g = t;
    }
}
pub fn token() -> String {
    TOKEN.read().map(|t| t.clone()).unwrap_or_default()
}
