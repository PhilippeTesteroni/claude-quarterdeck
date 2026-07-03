//! Hook event ingestion and the spooled event envelope
//! `{ v: 1, event, receivedAt, payload, extra }` (SPEC §3.1, §3.5, R-4.3, R-4.5).
//!
//! The hook scripts (T2) wrap each Claude Code hook invocation into this
//! envelope and atomically drop it into `<data>/spool/*.json`. This module
//! parses those files back into a strongly typed [`SpoolEvent`] with maximal
//! forward-compatibility (R-4.5: unknown fields ignored, optional fields
//! tolerated) and owns the spool-directory lifecycle: replay, the 24 h freshness
//! cut, the 5000-file cap, and quarantining of malformed files (R-3.5) — all
//! without ever panicking on hostile input.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

/// Envelope schema version we understand (R-4.3 `v:1`).
pub const ENVELOPE_VERSION: u32 = 1;

/// Hard cap on a single spool file. Real envelopes are a few hundred bytes;
/// anything larger is treated as hostile and quarantined instead of read into
/// memory (guards the "huge file" case from the QA matrix).
pub const MAX_SPOOL_FILE_BYTES: u64 = 1_048_576; // 1 MiB

/// Events older than this are discarded (never applied) on replay (R-3.5).
pub const MAX_EVENT_AGE_MS: u64 = 24 * 60 * 60 * 1000;

/// Maximum number of spool files retained; oldest are deleted first (R-3.5).
pub const MAX_SPOOL_FILES: usize = 5000;

/// Maximum number of files retained in `spool-quarantine/`; oldest are deleted
/// first. The spec caps the spool (R-3.5) and the logs (R-10.4); the quarantine
/// dir that receives every malformed/hostile file gets the same disk-growth
/// discipline so a buggy or adversarial hook can't grow it without bound.
pub const MAX_QUARANTINE_FILES: usize = 1000;

/// The five hook events we subscribe to (plus a tolerated catch-all), already
/// projected down to the payload fields we rely on (R-4.5, `docs/hooks-facts.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookEvent {
    /// `SessionStart` — session begins/resumes.
    SessionStart {
        source: Option<String>,
        session_title: Option<String>,
    },
    /// `UserPromptSubmit` — user submitted a prompt.
    UserPromptSubmit { prompt: Option<String> },
    /// `Notification` — Claude Code emitted a notification (matcher-filtered).
    Notification {
        message: Option<String>,
        notification_type: Option<String>,
    },
    /// `Stop` — Claude finished responding.
    Stop,
    /// `SubagentStart` — a Task/subagent child started (SPEC §21, R-21.2). Feeds
    /// the per-session active-subagent counter (the `⛭ N` badge).
    SubagentStart,
    /// `SubagentStop` — a Task/subagent child finished (SPEC §21, R-21.2).
    SubagentStop,
    /// `SessionEnd` — session terminated.
    SessionEnd { reason: Option<String> },
    /// Any other (forward-compatible) event name — tolerated and logged, never a
    /// parse error (R-4.5).
    Unknown { name: String },
}

/// The terminal-window ancestor captured on `SessionStart` (R-15.4a) so a row
/// click can focus the terminal hosting the session. On Windows the hook walks
/// the parent-process chain for the nearest ancestor with a real top-level
/// window (`MainWindowHandle != 0`); on macOS it records `TERM_PROGRAM` + a pid.
/// Every field is optional — the hook writes whatever it could resolve, and a
/// drifting/absent shape must never break ingestion (R-4.5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ancestor {
    /// PID of the ancestor process that owns the terminal window.
    pub pid: Option<u32>,
    /// Native window handle (`HWND`) of that terminal window, if known (Windows).
    /// Stored as `i64` so the full unsigned handle range round-trips through
    /// JSON's number type without loss.
    pub hwnd: Option<i64>,
    /// Executable / terminal-program name (Windows process name, or macOS
    /// `TERM_PROGRAM`), used for the title-substring focus fallback and the
    /// macOS bundle-id mapping (R-15.4b/c).
    pub exe: Option<String>,
}

impl Ancestor {
    /// Whether this ancestor carries anything actionable for focus (R-15.4).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pid.is_none() && self.hwnd.is_none() && self.exe.is_none()
    }
}

impl HookEvent {
    /// The canonical hook event name for logging/attribution.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            HookEvent::SessionStart { .. } => "SessionStart",
            HookEvent::UserPromptSubmit { .. } => "UserPromptSubmit",
            HookEvent::Notification { .. } => "Notification",
            HookEvent::Stop => "Stop",
            HookEvent::SubagentStart => "SubagentStart",
            HookEvent::SubagentStop => "SubagentStop",
            HookEvent::SessionEnd { .. } => "SessionEnd",
            HookEvent::Unknown { name } => name,
        }
    }
}

/// A parsed spool envelope. `received_at_ms` is `None` when the hook omitted the
/// timestamp; the engine then substitutes its own clock (R-4.5 tolerance).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpoolEvent {
    pub v: u32,
    pub session_id: String,
    pub received_at_ms: Option<u64>,
    pub cwd: Option<String>,
    pub transcript_path: Option<String>,
    /// `extra.claudePid` — present only on `SessionStart` (R-4.3).
    pub claude_pid: Option<u32>,
    /// `extra.ancestor` — the terminal-window ancestor, present only on
    /// `SessionStart` (R-15.4a). `None` when the hook couldn't resolve one.
    pub ancestor: Option<Ancestor>,
    pub kind: HookEvent,
}

/// Errors from parsing a single spool file. Any of these routes the file to
/// `spool-quarantine/` (R-3.5) — parsing NEVER panics.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("empty or whitespace-only spool file")]
    Empty,
    #[error("spool file exceeds {0} bytes")]
    TooLarge(u64),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("payload missing session_id")]
    MissingSessionId,
    #[error("envelope missing event name")]
    MissingEvent,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// --- Raw serde mirror of the on-disk envelope ------------------------------
//
// Everything is optional and unknown fields are ignored, so a future hook
// version that adds keys still round-trips (R-4.5).

#[derive(Debug, Deserialize)]
struct RawEnvelope {
    #[serde(default)]
    v: Option<u32>,
    #[serde(default)]
    event: Option<String>,
    #[serde(default, rename = "receivedAt")]
    received_at: Option<TimeValue>,
    #[serde(default)]
    payload: RawPayload,
    #[serde(default)]
    extra: RawExtra,
}

#[derive(Debug, Default, Deserialize)]
struct RawPayload {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    session_title: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    notification_type: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawExtra {
    #[serde(default, rename = "claudePid")]
    claude_pid: Option<u32>,
    #[serde(default)]
    ancestor: Option<RawAncestor>,
}

/// Raw serde mirror of `extra.ancestor` (R-15.4a). Every field is optional so a
/// partial or drifting shape parses without failing the whole envelope (R-4.5).
#[derive(Debug, Default, Deserialize)]
struct RawAncestor {
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    hwnd: Option<i64>,
    #[serde(default)]
    exe: Option<String>,
}

/// A JSON value that could carry a timestamp in several shapes (R-4.5 tolerance):
/// epoch millis or seconds as an int/float, or an ISO-8601 / numeric string.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TimeValue {
    Int(i64),
    Float(f64),
    Str(String),
}

/// Strip a leading UTF-8 BOM if present (a naive PowerShell writer can emit one).
fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

/// Parse a spool envelope from raw bytes. Pure (no IO); the primary unit under
/// test for the R-4.3/R-4.5 contract.
///
/// # Errors
/// Returns [`ParseError`] for empty, oversized, non-JSON, or semantically
/// unusable (no `session_id` / no event name) input — each of which the caller
/// quarantines.
pub fn parse_envelope(bytes: &[u8]) -> Result<SpoolEvent, ParseError> {
    if bytes.len() as u64 > MAX_SPOOL_FILE_BYTES {
        return Err(ParseError::TooLarge(bytes.len() as u64));
    }
    let bytes = strip_bom(bytes);
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Err(ParseError::Empty);
    }

    let raw: RawEnvelope = serde_json::from_slice(bytes)?;

    let session_id = raw
        .payload
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .ok_or(ParseError::MissingSessionId)?;

    // Prefer the wrapper's `event`, fall back to the payload's `hook_event_name`.
    let event_name = raw
        .event
        .filter(|s| !s.trim().is_empty())
        .or_else(|| raw.payload.hook_event_name.clone())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .ok_or(ParseError::MissingEvent)?;

    let kind = classify_event(&event_name, &raw.payload);

    Ok(SpoolEvent {
        v: raw.v.unwrap_or(ENVELOPE_VERSION),
        session_id,
        received_at_ms: raw.received_at.and_then(time_value_to_ms),
        cwd: clean(raw.payload.cwd),
        transcript_path: clean(raw.payload.transcript_path),
        claude_pid: raw.extra.claude_pid,
        ancestor: raw
            .extra
            .ancestor
            .map(ancestor_from_raw)
            .filter(|a| !a.is_empty()),
        kind,
    })
}

fn clean(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

fn ancestor_from_raw(raw: RawAncestor) -> Ancestor {
    Ancestor {
        // A `0` hwnd/pid means "no window / unresolved" from the hook's walk —
        // drop it so focus code never tries to act on a null handle (R-15.4b).
        pid: raw.pid.filter(|&p| p != 0),
        hwnd: raw.hwnd.filter(|&h| h != 0),
        exe: clean(raw.exe),
    }
}

fn classify_event(name: &str, payload: &RawPayload) -> HookEvent {
    match name {
        "SessionStart" => HookEvent::SessionStart {
            source: clean(payload.source.clone()),
            session_title: clean(payload.session_title.clone()),
        },
        "UserPromptSubmit" => HookEvent::UserPromptSubmit {
            prompt: payload.prompt.clone(),
        },
        "Notification" => HookEvent::Notification {
            message: clean(payload.message.clone()),
            notification_type: clean(payload.notification_type.clone()),
        },
        "Stop" => HookEvent::Stop,
        "SubagentStart" => HookEvent::SubagentStart,
        "SubagentStop" => HookEvent::SubagentStop,
        "SessionEnd" => HookEvent::SessionEnd {
            reason: clean(payload.reason.clone()),
        },
        other => HookEvent::Unknown {
            name: other.to_owned(),
        },
    }
}

// --- Timestamp coercion ----------------------------------------------------

fn time_value_to_ms(tv: TimeValue) -> Option<u64> {
    match tv {
        TimeValue::Int(i) => normalize_epoch(i as f64),
        TimeValue::Float(f) => normalize_epoch(f),
        TimeValue::Str(s) => {
            let t = s.trim();
            if let Ok(i) = t.parse::<i64>() {
                normalize_epoch(i as f64)
            } else if let Ok(f) = t.parse::<f64>() {
                normalize_epoch(f)
            } else {
                parse_iso8601_ms(t)
            }
        }
    }
}

/// Disambiguate epoch seconds from epoch milliseconds. "Now" is ~1.7e9 s and
/// ~1.7e12 ms, so 1e11 cleanly separates the two for any plausible timestamp.
fn normalize_epoch(v: f64) -> Option<u64> {
    if !v.is_finite() || v <= 0.0 {
        return None;
    }
    let ms = if v < 1e11 { v * 1000.0 } else { v };
    if ms >= u64::MAX as f64 {
        None
    } else {
        Some(ms as u64)
    }
}

/// Best-effort ISO-8601 / RFC-3339 parse to epoch millis, no external crates.
/// Returns `None` on anything it does not fully understand (R-4.5: tolerate,
/// don't fail the whole envelope).
fn parse_iso8601_ms(s: &str) -> Option<u64> {
    // date <sep> time, sep in {T, t, space}
    let sep = s.find(['T', 't', ' '])?;
    let (date, rest) = s.split_at(sep);
    let rest = &rest[1..];

    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    if d.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // Split off the timezone designator, if any.
    let (time_part, tz_offset_min) = split_timezone(rest)?;

    let mut t = time_part.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next().unwrap_or("0").parse().ok()?;
    let sec_field = t.next().unwrap_or("0");
    if t.next().is_some() {
        return None;
    }
    let (sec_str, frac_ms) = match sec_field.split_once(['.', ',']) {
        Some((whole, frac)) => {
            let frac: String = frac.chars().take(3).collect();
            let padded = format!("{frac:0<3}");
            (whole, padded.parse::<i64>().ok()?)
        }
        None => (sec_field, 0),
    };
    let second: i64 = sec_str.parse().ok()?;
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - tz_offset_min * 60;
    let ms = secs.checked_mul(1000)?.checked_add(frac_ms)?;
    u64::try_from(ms).ok()
}

/// Returns `(time_without_tz, offset_in_minutes)`. `Z`/absent → UTC (0).
fn split_timezone(rest: &str) -> Option<(&str, i64)> {
    if let Some(stripped) = rest.strip_suffix(['Z', 'z']) {
        return Some((stripped, 0));
    }
    // Look for a +HH:MM / -HH:MM suffix (but not the '-' inside the time).
    for (i, c) in rest.char_indices() {
        if (c == '+' || c == '-') && i > 0 {
            let sign = if c == '+' { 1 } else { -1 };
            let off = &rest[i + 1..];
            let mut parts = off.split(':');
            let oh: i64 = parts.next()?.parse().ok()?;
            let om: i64 = parts.next().unwrap_or("0").parse().ok()?;
            return Some((&rest[..i], sign * (oh * 60 + om)));
        }
    }
    Some((rest, 0))
}

/// Days since 1970-01-01 for a proleptic-Gregorian date (Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// --- Spool directory lifecycle ---------------------------------------------

/// A parsed, still-on-disk spool file. The caller applies `event` to the engine
/// and only THEN deletes `path` (parse → apply → delete, R-3.5).
#[derive(Debug, PartialEq, Eq)]
pub struct SpoolItem {
    pub path: PathBuf,
    pub event: SpoolEvent,
}

/// Bookkeeping returned by [`drain_spool`] for logging/metrics.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DrainOutcome {
    pub events: Vec<SpoolItem>,
    pub quarantined: usize,
    pub discarded_old: usize,
    pub capped: usize,
}

/// How a single file classified before any side effects.
enum FileClass {
    Good(SpoolEvent),
    Malformed(ParseError),
    TooOld,
}

fn classify_file(path: &Path, now_ms: u64) -> FileClass {
    match read_and_parse(path) {
        Ok(event) => {
            if let Some(ts) = event.received_at_ms {
                if now_ms.saturating_sub(ts) > MAX_EVENT_AGE_MS {
                    return FileClass::TooOld;
                }
            }
            FileClass::Good(event)
        }
        Err(err) => FileClass::Malformed(err),
    }
}

/// Read a spool file and parse it, guarding file size before reading it all.
///
/// # Errors
/// See [`ParseError`]; oversized files short-circuit as [`ParseError::TooLarge`].
pub fn read_and_parse(path: &Path) -> Result<SpoolEvent, ParseError> {
    let meta = fs::metadata(path)?;
    if meta.len() > MAX_SPOOL_FILE_BYTES {
        return Err(ParseError::TooLarge(meta.len()));
    }
    let bytes = fs::read(path)?;
    parse_envelope(&bytes)
}

/// Drain a spool directory: enforce the 5000-file cap, parse every file, discard
/// events older than 24 h, quarantine malformed ones, and return the fresh
/// events sorted by receipt time (ascending, so the reducer sees them in order).
///
/// Good files are left on disk for the caller to delete after applying them.
///
/// # Errors
/// Propagates directory-read IO failures. Per-file IO failures are treated as
/// "malformed" and quarantined, never fatal.
pub fn drain_spool(
    spool_dir: &Path,
    quarantine_dir: &Path,
    now_ms: u64,
) -> std::io::Result<DrainOutcome> {
    let mut outcome = DrainOutcome::default();

    let mut files: Vec<(PathBuf, SystemTime)> = Vec::new();
    let entries = match fs::read_dir(spool_dir) {
        Ok(e) => e,
        // A missing spool dir just means "nothing to replay".
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(outcome),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        files.push((path, mtime));
    }

    // R-3.5 cap: delete oldest beyond MAX_SPOOL_FILES.
    if files.len() > MAX_SPOOL_FILES {
        files.sort_by_key(|(_, mtime)| *mtime);
        let overflow = files.len() - MAX_SPOOL_FILES;
        for (path, _) in files.drain(0..overflow) {
            if fs::remove_file(&path).is_ok() {
                outcome.capped += 1;
            }
        }
    }

    for (path, _) in &files {
        match classify_file(path, now_ms) {
            FileClass::Good(event) => outcome.events.push(SpoolItem {
                path: path.clone(),
                event,
            }),
            FileClass::TooOld => {
                if fs::remove_file(path).is_ok() {
                    outcome.discarded_old += 1;
                }
            }
            FileClass::Malformed(err) => {
                tracing::warn!(?path, %err, "quarantining malformed spool file");
                if quarantine(path, quarantine_dir).is_ok() {
                    outcome.quarantined += 1;
                }
            }
        }
    }

    outcome.events.sort_by_key(|item| item.event.received_at_ms);
    Ok(outcome)
}

/// Enforce a "keep at most `max` files, oldest deleted first" cap on `dir`.
/// Returns how many files were removed. A missing directory is a no-op.
///
/// When `only_json` is set, only `*.json` files count toward (and are removed
/// by) the cap — used for the spool, whose live files are always `*.json`. The
/// quarantine dir passes `false` because collision-renamed files there end in
/// `.json.1`, `.json.2`, … and must still be counted/trimmed.
///
/// # Errors
/// Propagates directory-read IO failures (other than "not found").
fn cap_dir(dir: &Path, max: usize, only_json: bool) -> std::io::Result<usize> {
    let mut files: Vec<(PathBuf, SystemTime)> = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if only_json && path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        files.push((path, mtime));
    }
    if files.len() <= max {
        return Ok(0);
    }
    files.sort_by_key(|(_, mtime)| *mtime);
    let overflow = files.len() - max;
    let mut removed = 0;
    for (path, _) in files.drain(0..overflow) {
        if fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Enforce the R-3.5 5000-file spool cap on the *live* path (oldest deleted
/// first). The startup replay path enforces the same cap inside [`drain_spool`];
/// this is the running-app counterpart the engine calls on its periodic tick so
/// the directory can't grow unbounded between restarts if the engine is ever
/// outpaced by a flood of writes.
///
/// # Errors
/// Propagates directory-read IO failures (other than "not found").
pub fn enforce_spool_cap(spool_dir: &Path) -> std::io::Result<usize> {
    cap_dir(spool_dir, MAX_SPOOL_FILES, true)
}

/// Enforce the [`MAX_QUARANTINE_FILES`] cap on `spool-quarantine/` (oldest
/// deleted first), so a sustained flood of malformed/hostile spool files can't
/// grow the quarantine dir without bound.
///
/// # Errors
/// Propagates directory-read IO failures (other than "not found").
pub fn enforce_quarantine_cap(quarantine_dir: &Path) -> std::io::Result<usize> {
    cap_dir(quarantine_dir, MAX_QUARANTINE_FILES, false)
}

/// Grace period before a stray non-`.json` file in `spool/` is swept
/// ([`sweep_stray_spool_files`]). Comfortably longer than an atomic tmp-write +
/// rename (sub-millisecond in practice), so a file this old is definitely an
/// abandoned leftover, never an in-flight write.
pub const STRAY_FILE_MIN_AGE_MS: u64 = 60_000;

/// Quarantine stray non-`.json` files left in `spool/` (R-3.5 disk hygiene).
///
/// The hook contract (R-4.3) writes each envelope atomically: `<id>.json.tmp`
/// then rename to `<id>.json`. A hook process killed *between* those two steps
/// leaves a `<id>.json.tmp` (or, defensively, any non-`.json` leftover) behind.
/// Neither [`drain_spool`] nor [`ingest_file`] ever consumes such a file (both
/// skip anything without a `.json` extension), and [`enforce_spool_cap`] counts
/// only `.json` files — so without this it would accumulate on disk forever.
///
/// Only files older than `min_age_ms` are swept, so an in-flight atomic write
/// (its `.tmp` exists for well under a millisecond) is never disturbed. Swept
/// files go to `quarantine_dir`, which has its own [`MAX_QUARANTINE_FILES`] cap,
/// keeping total growth bounded. Returns how many files were swept.
///
/// # Errors
/// Propagates the directory-read IO failure (other than "not found").
pub fn sweep_stray_spool_files(
    spool_dir: &Path,
    quarantine_dir: &Path,
    now_ms: u64,
    min_age_ms: u64,
) -> std::io::Result<usize> {
    let entries = match fs::read_dir(spool_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut swept = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            continue;
        }
        let mtime_ms = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        // Too young to be sure it isn't an in-flight tmp-then-rename write.
        if now_ms.saturating_sub(mtime_ms) < min_age_ms {
            continue;
        }
        tracing::warn!(?path, "quarantining stray non-json spool file (R-3.5)");
        if quarantine(&path, quarantine_dir).is_ok() {
            swept += 1;
        }
    }
    Ok(swept)
}

/// Live-path ingest of a single spool file discovered by the watcher.
///
/// Returns `Ok(Some(event))` when the file parsed and is fresh (caller applies,
/// then deletes `path`); `Ok(None)` when the file was quarantined or discarded
/// (already removed/moved). Never returns a parse error to the caller.
///
/// # Errors
/// Only surfaces the directory-side IO failure of moving a file to quarantine.
pub fn ingest_file(
    path: &Path,
    quarantine_dir: &Path,
    now_ms: u64,
) -> std::io::Result<Option<SpoolEvent>> {
    // The watcher can report a path that a concurrent drain (e.g. the startup
    // re-drain that closes the replay→watch gap) already consumed. A vanished
    // file is a no-op, not a parse error to quarantine.
    if !path.exists() {
        return Ok(None);
    }
    match classify_file(path, now_ms) {
        FileClass::Good(event) => Ok(Some(event)),
        FileClass::TooOld => {
            let _ = fs::remove_file(path);
            Ok(None)
        }
        FileClass::Malformed(err) => {
            tracing::warn!(?path, %err, "quarantining malformed spool file");
            quarantine(path, quarantine_dir)?;
            Ok(None)
        }
    }
}

/// Move a bad file into `quarantine_dir`, disambiguating name collisions, and
/// never leaving the file in place to be reprocessed forever.
fn quarantine(path: &Path, quarantine_dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(quarantine_dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed.json".to_string());
    let mut dest = quarantine_dir.join(&name);
    let mut n = 1u32;
    while dest.exists() {
        dest = quarantine_dir.join(format!("{name}.{n}"));
        n += 1;
    }

    if fs::rename(path, &dest).is_ok() {
        return Ok(());
    }
    // Cross-device rename can fail: fall back to copy+remove.
    match fs::copy(path, &dest).and_then(|_| fs::remove_file(path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Last resort: remove so we don't loop on it. Report the failure.
            let _ = fs::remove_file(path);
            Err(e)
        }
    }
}

/// Convenience map used by tests/tools to summarise a batch of parsed events by
/// event name.
#[must_use]
pub fn count_by_event(events: &[SpoolEvent]) -> HashMap<String, usize> {
    let mut m = HashMap::new();
    for e in events {
        *m.entry(e.kind.name().to_owned()).or_insert(0) += 1;
    }
    m
}
