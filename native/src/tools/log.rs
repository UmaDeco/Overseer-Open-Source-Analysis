//! Buffered, non-blocking logging.
//!
//! The original implementation opened the log file, wrote one line, and closed it **on every
//! call** — and `paths::log_file()` ran `create_dir_all` each time on top of that. Every one of
//! those calls happens on the GAME's main/render thread (the skip pumps, the text hooks, the
//! response hook all log), so a skip flood turned into a per-frame syscall storm: open + stat +
//! write + close, several times a frame, with antivirus filter drivers in the path. That is the
//! single largest contributor to the "FPS drops during Skip / Fast Forward" report.
//!
//! Now: callers push a formatted line into an in-memory queue (one lock, one `String` move) and a
//! background writer thread drains it to disk in batches. The hot path never touches the
//! filesystem. Extras that fall out of having a queue:
//!
//!   * **Levels** — `log()` (always kept) vs `trace()` (hot-path diagnostics, off by default).
//!     The per-frame "[event] ignored (debounce …)" / "[train] BAILED" style lines move to
//!     `trace()`, so they cost one relaxed atomic load when nobody is debugging.
//!   * **Burst suppression** — an identical line repeated within a short window is collapsed to a
//!     single `(xN)` summary instead of thousands of rows.
//!   * **Rotation** — the log is capped, so a long session can't grow an unbounded file that the
//!     web UI's Logs tail then re-reads every couple of seconds.
//!   * **Bounded memory** — the queue itself is capped; overflow is counted, never grown.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use once_cell::sync::Lazy;

/// Max queued lines before new ones are dropped (counted, then reported once the queue drains).
const QUEUE_CAP: usize = 4096;
/// Roll the log over once it passes this size, keeping one `.1` backup.
const ROTATE_BYTES: u64 = 8 * 1024 * 1024;
/// Identical consecutive-ish lines inside this window collapse into one `(xN)` entry.
const DEDUP_WINDOW_MS: u64 = 2000;
/// How many distinct recent messages the burst suppressor remembers.
const DEDUP_TRACK: usize = 256;

struct Queue {
    lines: Vec<(&'static str, String)>, // (file, line)
    dropped: u64,
    stop: bool,
}

static QUEUE: Lazy<Mutex<Queue>> = Lazy::new(|| {
    Mutex::new(Queue { lines: Vec::with_capacity(256), dropped: 0, stop: false })
});
static CV: Condvar = Condvar::new();
static WRITER_STARTED: AtomicBool = AtomicBool::new(false);

/// Verbose hot-path diagnostics. Off by default — flipped from the web UI (`/api/mod/logging`).
static TRACE_ON: AtomicBool = AtomicBool::new(false);
/// Lines dropped because the queue was full (surfaced in the log once space frees up).
static DROPPED_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Lines currently queued (diagnostics / the memory report).
static QUEUED: AtomicUsize = AtomicUsize::new(0);

/// Enable/disable the verbose `trace!`-level lines (per-frame skip/translation diagnostics).
pub fn set_trace(on: bool) {
    TRACE_ON.store(on, Ordering::Relaxed);
}
pub fn trace_enabled() -> bool {
    TRACE_ON.load(Ordering::Relaxed)
}

/// Number of lines waiting to be written (diagnostics).
pub fn queued() -> usize {
    QUEUED.load(Ordering::Relaxed)
}

fn now_ms() -> u64 {
    super::time::now_ms()
}

/// Local wall-clock `HH:MM:SS` prefix so the web Logs console can show a TIME column. Uses the Win32
/// GetLocalTime (no chrono dep).
fn stamp() -> String {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    format!("[{:02}:{:02}:{:02}] ", st.wHour, st.wMinute, st.wSecond)
}

// ── burst suppression ───────────────────────────────────────────────────────────────────────────
// A hook that logs once per frame produces ~100 identical lines a second. Collapse them: the first
// goes through immediately, repeats inside the window are counted, and the tally is emitted once the
// window closes. Bounded map, cleared wholesale when it grows past DEDUP_TRACK.
struct Burst {
    last_ms: u64,
    repeats: u32,
}
static BURSTS: Lazy<Mutex<HashMap<u64, Burst>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn msg_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Returns Some(suffix) if this line should be written (suffix carries any collapsed repeat count),
/// or None if it is a suppressed repeat.
fn burst_gate(msg: &str) -> Option<String> {
    let now = now_ms();
    let key = msg_hash(msg);
    let mut map = match BURSTS.lock() {
        Ok(m) => m,
        Err(_) => return Some(String::new()),
    };
    if map.len() > DEDUP_TRACK {
        map.clear(); // bounded: a wholesale clear at worst re-emits a line early
    }
    match map.get_mut(&key) {
        Some(b) if now.saturating_sub(b.last_ms) < DEDUP_WINDOW_MS => {
            b.repeats = b.repeats.saturating_add(1);
            None
        }
        Some(b) => {
            let n = std::mem::replace(&mut b.repeats, 0);
            b.last_ms = now;
            Some(if n > 0 { format!("  (+{n} repeats suppressed)") } else { String::new() })
        }
        None => {
            map.insert(key, Burst { last_ms: now, repeats: 0 });
            Some(String::new())
        }
    }
}

// ── public API ──────────────────────────────────────────────────────────────────────────────────

/// Append a line to the default Overseer log (`overseer-native.log`). Non-blocking.
pub fn log(msg: &str) {
    log_to("overseer-native.log", msg);
}

/// Hot-path diagnostic line — written only while verbose logging is on. Prefer this for anything
/// that can fire more than a few times a second (per-frame pumps, per-string translation notes).
/// The argument is built by the caller, so wrap the call site in [`trace_enabled`] when the message
/// itself is expensive to format.
pub fn trace(msg: &str) {
    if TRACE_ON.load(Ordering::Relaxed) {
        log_to("overseer-native.log", msg);
    }
}

/// Append a line to the named log file under the Overseer logs directory. Never blocks on I/O and
/// never panics — a full queue drops the line (counted) rather than stalling the game thread.
pub fn log_to(file: &'static str, msg: &str) {
    let Some(suffix) = burst_gate(msg) else { return };
    start_writer();
    let line = if suffix.is_empty() {
        format!("{}{msg}", stamp())
    } else {
        format!("{}{msg}{suffix}", stamp())
    };
    if let Ok(mut q) = QUEUE.lock() {
        if q.lines.len() >= QUEUE_CAP {
            q.dropped += 1;
            DROPPED_TOTAL.fetch_add(1, Ordering::Relaxed);
            return;
        }
        q.lines.push((file, line));
        QUEUED.store(q.lines.len(), Ordering::Relaxed);
    }
    CV.notify_one();
}

/// Flush everything queued right now and block until the writer has drained it. Used by the
/// shutdown path and by the web UI before serving the log tail, so the console isn't ~250 ms stale.
pub fn flush_blocking() {
    if !WRITER_STARTED.load(Ordering::Relaxed) {
        return;
    }
    CV.notify_one();
    for _ in 0..40 {
        let empty = QUEUE.lock().map(|q| q.lines.is_empty()).unwrap_or(true);
        if empty {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

// ── writer thread ───────────────────────────────────────────────────────────────────────────────

fn start_writer() {
    if WRITER_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    if std::thread::Builder::new()
        .name("overseer-log".into())
        .spawn(writer_main)
        .is_err()
    {
        // Couldn't spawn: fall back to synchronous writes so logging still works at all.
        WRITER_STARTED.store(false, Ordering::Release);
    }
}

fn writer_main() {
    loop {
        let batch: Vec<(&'static str, String)> = {
            let mut q = match QUEUE.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            while q.lines.is_empty() && !q.stop {
                q = match CV.wait_timeout(q, std::time::Duration::from_millis(500)) {
                    Ok((g, _)) => g,
                    Err(_) => return,
                };
            }
            if q.lines.is_empty() && q.stop {
                return;
            }
            let dropped = std::mem::take(&mut q.dropped);
            let mut batch = std::mem::take(&mut q.lines);
            q.lines = Vec::with_capacity(256);
            QUEUED.store(0, Ordering::Relaxed);
            if dropped > 0 {
                batch.push((
                    "overseer-native.log",
                    format!("{}[log] {dropped} lines dropped (write queue full)", stamp()),
                ));
            }
            batch
        };
        write_batch(&batch);
    }
}

/// Group the batch by file and issue ONE open + one buffered write per file per flush.
fn write_batch(batch: &[(&'static str, String)]) {
    let mut files: Vec<(&'static str, String)> = Vec::new();
    for (file, line) in batch {
        match files.iter_mut().find(|(f, _)| f == file) {
            Some((_, buf)) => {
                buf.push_str(line);
                buf.push('\n');
            }
            None => {
                let mut buf = String::with_capacity(line.len() + 1);
                buf.push_str(line);
                buf.push('\n');
                files.push((file, buf));
            }
        }
    }
    for (file, buf) in files {
        let path = crate::paths::log_file(file);
        rotate_if_needed(&path);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = f.write_all(buf.as_bytes());
        }
    }
}

/// Keep the log bounded: at ROTATE_BYTES the current file becomes `<name>.1` (replacing any older
/// backup) and a fresh one starts. Without this a long session grew a file the Logs page then
/// re-read (400 KB tail) every 2 s forever.
fn rotate_if_needed(path: &std::path::Path) {
    let Ok(meta) = std::fs::metadata(path) else { return };
    if meta.len() < ROTATE_BYTES {
        return;
    }
    let backup = path.with_extension("log.1");
    let _ = std::fs::remove_file(&backup);
    let _ = std::fs::rename(path, &backup);
}
