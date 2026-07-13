//! Quarterdeck Tauri shell — composition root (T7).
//!
//! T0 scaffolds the module tree; T1–T6 fill the module bodies; **this file wires
//! them into a running application**:
//!
//! * logging (R-10.4): a size-rotating file appender at `<data>/logs/quarterdeck.log`
//!   (1 MB × 3), `QUARTERDECK_DEBUG=1` → debug level;
//! * the tray icon ([`tray`]) and the popup / ask windows ([`windows`]);
//! * the spool pipeline: [`watcher`] → [`deck_core::events`] → [`deck_core::engine`]
//!   → tray / UI snapshots / [`notify`] toasts, with startup replay (R-3.5) and
//!   cold-start discovery (R-5.4);
//! * the MCP server ([`mcp_server`]) bridged to the engine + UI via an
//!   [`EngineGateway`] `AskGateway` (the ask channel, §8);
//! * the settings-driven side effects: autostart (R-10.3) and the hook installer
//!   (§4) behind the onboarding consent gate (R-10.2).
//!
//! The frontend stays dumb (R-3.4): every change rebuilds a full
//! [`ipc::StateSnapshot`] pushed over `deck://state`; the UI sends intent back
//! through the commands registered below.

pub mod foreground;
pub mod ipc;
pub mod mcp_server;
pub mod notify;
pub mod session_names;
pub mod settings;
pub mod tray;
pub mod util;
pub mod watcher;
pub mod windows;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tauri::{AppHandle, Emitter, Listener, Manager, Wry};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tokio::sync::oneshot;

use deck_core::ask::{AskStore, PendingAsk};
use deck_core::engine::{Effect, SessionStore, SessionView, Status as EngineStatus};
use deck_core::naming::{extract_ai_title, strip_bidi_controls, truncate_graphemes};
use deck_core::traits::{Notifier, ProcessTable, ToastKind};
use deck_core::usage::{self, SessionUsageGroup};
use deck_core::{discovery, events, hooks_config, registry};

use crate::ipc::{
    AppState, AskAnswerKind, AskRow, Counts, PermDecision, PermRow, SessionRow, SessionStatus,
    SettingsState, StateSnapshot,
};
use crate::mcp_server::{AskAnswer, AskGateway, AskRequest, NotifyRequest, SubmittedAsk};
use crate::notify::DesktopNotifier;
use crate::settings::{PopupMode, Settings};
use crate::util::CommandNoWindow;

/// Rust-side liveness / recovery / prune cadence (R-3.6, R-6.1).
const ENGINE_TICK: Duration = Duration::from_secs(10);
/// How long the engine loop blocks waiting for a spool path before it re-checks
/// whether a tick is due.
const LOOP_SLICE: Duration = Duration::from_millis(400);
/// Safety-net rescan cadence for the spool directory (R-2.5, R-3.5). The
/// `notify`-rs watcher is the primary, immediate ingest path, but OS file-watch
/// delivery is not guaranteed: under a burst of files each written by a
/// freshly-spawned hook process — exactly how real Claude Code hooks fire, one
/// interpreter per event (R-4.4) — Windows' `ReadDirectoryChangesW` can drop a
/// whole burst, stranding spool files unseen until the next restart (which
/// recovers them via the startup replay path). This periodic directory scan
/// ingests anything the watcher missed, bounding worst-case latency to one
/// interval instead of "until the user restarts Quarterdeck". Kept short so a
/// missed `SessionEnd` still clears its row promptly (R-2.5): a scan of a
/// near-always-empty directory is a single cheap `read_dir`.
const SPOOL_SWEEP: Duration = Duration::from_secs(1);
/// Foreground-window sampling cadence for focus-aware suppression (R-17.1).
/// Only actually samples while a suppressed ask is pending (see the engine
/// loop), so it never spawns a sampler when there is nothing to un-suppress.
const FOREGROUND_POLL: Duration = Duration::from_secs(2);

/// Character budget for the R-24.1 finished-toast body (the model's last
/// words). Matches "the SAME character budget as today's body" — the R-9.1
/// fallback body is at most a 60-grapheme title (R-5.2) plus the fixed
/// " Waiting for new instructions." suffix (30 chars).
const IDLE_BODY_MAX_CHARS: usize = 90;

static ASK_SEQ: AtomicU64 = AtomicU64::new(0);
/// Monotonic sequence for `notify_user` record ids (R-19.6).
static NOTIFY_SEQ: AtomicU64 = AtomicU64::new(0);

/// Serializes the handful of tests across modules that mutate the process-global
/// `QUARTERDECK_DATA_DIR` env var, so they don't race under the parallel test
/// harness (integration fix requested in T3's report). Test-only.
#[cfg(test)]
pub static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Logging (R-10.4): a self-contained size-rotating file writer. `tracing-appender`
// only rotates by time, and the spec wants 1 MB × 3, so we roll our own tiny
// writer and feed it through `non_blocking` so logging never blocks the app.
// ---------------------------------------------------------------------------

mod logging {
    use std::fs::{self, File, OpenOptions};
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};

    use tracing_appender::non_blocking::WorkerGuard;
    use tracing_subscriber::EnvFilter;

    /// Max bytes per log file before rotation (R-10.4 "1 MB").
    const MAX_BYTES: u64 = 1_048_576;
    /// Total number of log files kept: `quarterdeck.log`, `.1`, `.2` (R-10.4 "× 3").
    const KEEP: usize = 3;

    struct RotatingWriter {
        dir: PathBuf,
        base: String,
        file: Option<File>,
        written: u64,
    }

    impl RotatingWriter {
        fn new(dir: PathBuf) -> Self {
            Self {
                dir,
                base: "quarterdeck.log".to_string(),
                file: None,
                written: 0,
            }
        }

        fn path(&self, n: usize) -> PathBuf {
            if n == 0 {
                self.dir.join(&self.base)
            } else {
                self.dir.join(format!("{}.{n}", self.base))
            }
        }

        fn open(&mut self) -> io::Result<()> {
            fs::create_dir_all(&self.dir)?;
            let p = self.path(0);
            self.written = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            self.file = Some(OpenOptions::new().create(true).append(true).open(&p)?);
            Ok(())
        }

        fn rotate(&mut self) -> io::Result<()> {
            self.file = None;
            let last = KEEP - 1;
            let _ = fs::remove_file(self.path(last));
            for n in (1..last).rev() {
                let from = self.path(n);
                if from.exists() {
                    let _ = fs::rename(&from, self.path(n + 1));
                }
            }
            if self.path(0).exists() {
                let _ = fs::rename(self.path(0), self.path(1));
            }
            self.written = 0;
            self.open()
        }
    }

    impl Write for RotatingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.file.is_none() {
                self.open()?;
            }
            if self.written > 0 && self.written + buf.len() as u64 > MAX_BYTES {
                self.rotate()?;
            }
            let file = self
                .file
                .as_mut()
                .ok_or_else(|| io::Error::other("log file not open"))?;
            let n = file.write(buf)?;
            self.written += n as u64;
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            match self.file.as_mut() {
                Some(f) => f.flush(),
                None => Ok(()),
            }
        }
    }

    /// Initialise the global tracing subscriber writing to
    /// `<data>/logs/quarterdeck.log` with 1 MB × 3 rotation. Returns the
    /// [`WorkerGuard`] that must be kept alive for the process lifetime (dropping
    /// it flushes the async writer). Returns `None` if a subscriber was already
    /// installed (e.g. in a test process).
    pub fn init(data_dir: &Path) -> Option<WorkerGuard> {
        let writer = RotatingWriter::new(data_dir.join("logs"));
        let (non_blocking, guard) = tracing_appender::non_blocking(writer);
        let default_level = if std::env::var("QUARTERDECK_DEBUG").as_deref() == Ok("1") {
            "debug"
        } else {
            "info"
        };
        let filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
        let installed = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_target(false)
            .try_init()
            .is_ok();
        installed.then_some(guard)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rotates_at_one_megabyte_and_keeps_three_files() {
            let dir = std::env::temp_dir().join(format!(
                "quarterdeck-log-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let mut w = RotatingWriter::new(dir.clone());
            // ~2.5 MB in 64 KiB chunks forces two rotations.
            let chunk = vec![b'x'; 64 * 1024];
            for _ in 0..40 {
                w.write_all(&chunk).unwrap();
            }
            w.flush().unwrap();
            drop(w);

            assert!(dir.join("quarterdeck.log").exists(), "active log exists");
            assert!(
                dir.join("quarterdeck.log.1").exists(),
                "first archive exists"
            );
            assert!(
                dir.join("quarterdeck.log.2").exists(),
                "second archive exists"
            );
            // KEEP = 3: never a fourth file.
            assert!(!dir.join("quarterdeck.log.3").exists(), "no fourth file");
            // Each retained file stays within (roughly) the 1 MB budget.
            for n in 0..3 {
                let p = if n == 0 {
                    dir.join("quarterdeck.log")
                } else {
                    dir.join(format!("quarterdeck.log.{n}"))
                };
                let len = std::fs::metadata(&p).unwrap().len();
                assert!(
                    len <= MAX_BYTES + 64 * 1024,
                    "file {p:?} within budget: {len}"
                );
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

// ---------------------------------------------------------------------------
// OS process table (R-6.1) backed by sysinfo.
// ---------------------------------------------------------------------------

struct SysProcs {
    sys: sysinfo::System,
}

impl SysProcs {
    fn refreshed() -> Self {
        let mut sys = sysinfo::System::new();
        // Refresh names/existence ONLY (`ProcessRefreshKind::nothing()`), not the
        // full `everything()` default `refresh_processes` uses. Liveness (R-6.1)
        // only needs `process_name(pid)`, which comes from the base process-list
        // enumeration. The optional details `everything()` fetches (cmdline,
        // user, exe, cwd, …) each `OpenProcess` + query per process, which HANGS
        // on Windows while the machine is churning through a burst of
        // freshly-spawned-and-exiting hook interpreters (`powershell.exe`/
        // `node.exe`, R-4.4) — the app's core parallel-sessions case. That hang
        // is on this same thread as the spool-ingest loop (R-3.6), so it would
        // freeze all spool ingestion until restart, silently stranding a whole
        // burst of Stop/SessionEnd events (R-2.5). Names-only never opens those
        // handles.
        sys.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::All,
            true,
            sysinfo::ProcessRefreshKind::nothing(),
        );
        Self { sys }
    }
}

impl ProcessTable for SysProcs {
    fn process_name(&self, pid: u32) -> Option<String> {
        self.sys
            .process(sysinfo::Pid::from_u32(pid))
            .map(|p| p.name().to_string_lossy().into_owned())
    }
}

fn transcript_mtime_ms(path: &str) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// §34 `aiTitle` tail-read budget: the last 128 KB of a transcript is plenty —
/// `aiTitle` is (re)written on recent lines — so a multi-MB transcript is never
/// read whole on the tick (R-34).
const AI_TITLE_TAIL_BYTES: u64 = 128 * 1024;

/// Read up to the last `max_bytes` of a file into memory (the transcript TAIL,
/// §34). Seeks to `len - max_bytes` so a large transcript is never read whole.
/// Best-effort: `None` on any IO error. The returned slice may begin mid-line
/// (and mid-UTF-8-char), which is fine — [`extract_ai_title`] scans for the
/// ASCII `"aiTitle"` key and decodes only its complete value.
fn read_transcript_tail(path: &str, max_bytes: u64) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    if start > 0 {
        file.seek(SeekFrom::Start(start)).ok()?;
    }
    let mut buf = Vec::with_capacity(len.saturating_sub(start).min(max_bytes) as usize);
    file.take(max_bytes).read_to_end(&mut buf).ok()?;
    Some(buf)
}

// ---------------------------------------------------------------------------
// Path helpers.
// ---------------------------------------------------------------------------

fn norm_path(p: &str) -> String {
    p.trim()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string()
}

fn basename(p: &str) -> String {
    let trimmed = p.trim().trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed)
        .to_string()
}

fn paths_eq(a: &str, b: &str) -> bool {
    let (na, nb) = (norm_path(a), norm_path(b));
    if cfg!(windows) {
        // NTFS is case-insensitive for the whole Unicode range, not just ASCII:
        // `eq_ignore_ascii_case` would miss a Cyrillic case-only difference
        // (e.g. `Мой-Проект` vs `МОЙ-ПРОЕКТ`) even though Windows treats them as
        // the same directory. `to_lowercase` folds case Unicode-wide (R-5.3/R-8.2).
        na.to_lowercase() == nb.to_lowercase()
    } else {
        na == nb
    }
}

/// Whether a `claude` executable is resolvable on `PATH` (R-8.6): drives whether
/// the settings pane shows the manual `claude mcp add …` command to copy. A cheap
/// PATH scan (no subprocess) so it's fine to compute on every snapshot.
fn claude_cli_on_path() -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    let candidates: &[&str] = if cfg!(windows) {
        &["claude.exe", "claude.cmd", "claude.bat", "claude"]
    } else {
        &["claude"]
    };
    std::env::split_paths(&paths).any(|dir| candidates.iter().any(|name| dir.join(name).is_file()))
}

/// The user-level Claude settings file we install hooks into (`~/.claude/settings.json`),
/// with the `QUARTERDECK_CLAUDE_DIR` override so tests target an isolated copy (R-4.1).
fn claude_settings_path() -> PathBuf {
    discovery::claude_dir_from_env()
        .unwrap_or_else(|| PathBuf::from(".claude"))
        .join("settings.json")
}

fn map_status(status: EngineStatus) -> SessionStatus {
    match status {
        EngineStatus::Working => SessionStatus::Working,
        EngineStatus::WaitingWorkflow => SessionStatus::WaitingWorkflow,
        EngineStatus::Attention => SessionStatus::Attention,
        EngineStatus::Idle => SessionStatus::Idle,
        EngineStatus::Dead => SessionStatus::Dead,
    }
}

fn map_row(view: &SessionView) -> SessionRow {
    SessionRow {
        id: view.id.clone(),
        project: view.project.clone(),
        title: view.title.clone(),
        branch: view.branch.clone(),
        status: map_status(view.status),
        inferred: view.inferred,
        since_ms: view.since_ms,
        cwd: view.cwd.clone(),
        subagents: view.subagents,
        age_ms: view.age_ms,
        work_started_ms: view.work_started_ms,
        last_work_ms: view.last_work_ms,
        // §23 token telemetry is projected in `snapshot` from the shell's usage
        // map (the engine view carries none); default to absent here.
        ctx_percent: None,
        spend: None,
        spend_approx: false,
        subagent_spend: None,
        // §38: the Claude host pid drives the "Kill process" context item.
        pid: view.pid,
    }
}

// ---------------------------------------------------------------------------
// Pending asks (§8) tracked shell-side.
// ---------------------------------------------------------------------------

/// Shell-side pending ask: the portable [`deck_core::ask::PendingAsk`] carrying
/// the shell's `oneshot` responder that unblocks the blocked MCP `ask_user` call.
/// The queue/timeout/orphan logic lives in `deck-core` ([`AskStore`]); the shell
/// only owns the responder and the file I/O.
type ShellAsk = PendingAsk<oneshot::Sender<AskAnswer>>;

fn ask_to_row(ask: &ShellAsk) -> AskRow {
    AskRow {
        id: ask.id.clone(),
        session_id: ask.session_id.clone(),
        project: ask.project.clone(),
        question: ask.question.clone(),
        options: ask.options.clone(),
        // R-29.5: carry the multi-question form through to the UI so the ask
        // window renders it and the popup mirror shows "N questions".
        questions: ask.questions.clone(),
        detail: ask.detail.clone(),
        // R-19.2: a persistent ask has no expiry, so it carries no countdown.
        timeout_at: if ask.persistent {
            None
        } else {
            Some(ask.timeout_at_ms)
        },
        // R-8.2: only unmatched asks surface the raw context ("Unknown agent (<context>)").
        context: if ask.session_id.is_none() {
            ask.context.clone()
        } else {
            None
        },
        // R-8.7: an ask recovered from disk at startup is shown as expired.
        orphaned: ask.orphaned,
        // R-16.2: arrival time for the shared ask/perm FIFO ordering.
        queued_at: ask.enqueued_ms,
    }
}

/// Display cap for the perm modal's `tool_input` (SPEC R-16.1 / R-16.5 length
/// caps). Applied in grapheme units after bidi-stripping. Raised from 2KB to
/// 16KB in §49 (in tandem with the hook's byte cap) so a multi-question
/// `AskUserQuestion` stays valid JSON and renders structured (R-35.1) instead
/// of overflowing into the raw-blob fallback.
const PERM_INPUT_CAP: usize = 16384;
/// Display cap for a tool name (defence in depth; real names are short).
const PERM_TOOL_NAME_CAP: usize = 200;
/// SPEC R-32.1: a pending perm's lifetime. The `PermissionRequest` hook that
/// raised it polls `<data>/perm-answers/` with a 90 s timeout (R-16.1), so a
/// deck decision made past this point can never reach the hook — it has already
/// exited and Claude Code has fallen back to the terminal dialog. Past this
/// deadline (`received_ms + PERM_DEADLINE_MS`) the perm is swept off the tick
/// (mirror of `AskStore::sweep_expired`) and, until swept, the UI disables its
/// Allow/Deny buttons so a stale answer is never routed into the void.
const PERM_DEADLINE_MS: u64 = 90_000;

/// SPEC R-16.2: the perm modal body is "tool name + compact pretty-printed input
/// (truncated)". The hook serialises `tool_input` to a JSON string and caps it at
/// 2KB (R-16.1). Here we normalise that string into indented (pretty-printed) JSON
/// for the modal, tool-agnostically — the same output whether the ps1, python, or
/// jq hook wrote the file. When the input parses (the common case) we re-emit it
/// via `serde_json` so the indentation is consistent regardless of how the hook
/// formatted it. When it does NOT parse — a `tool_input` that overflowed the hook's
/// 2KB cap and was truncated mid-JSON — we keep the hook's text verbatim rather
/// than invent structure from a fragment (the truncation-safety note from QA round
/// 6). On that fallback branch ONLY we still un-escape the printable-ASCII `\uXXXX`
/// escapes PowerShell's `ConvertTo-Json` over-produces (`'`, `<`, `>`, `&`, …) so a
/// truncated blob reads as text instead of `'`/`<` noise (§28); the parse
/// path is untouched because serde already decodes those escapes natively. Bidi-
/// stripping and the display cap are applied by the caller AFTER this, per R-16.5.
fn pretty_tool_input(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|_| raw.to_string()),
        Err(_) => decode_printable_ascii_escapes(raw),
    }
}

/// PowerShell's `ConvertTo-Json` escapes the HTML-sensitive ASCII characters
/// `'`, `<`, `>`, `&` (its default "escape-handling" set) as `\uXXXX` even though
/// they are printable, which makes a raw perm blob read as noise (`It's`
/// for `It's`). Used only on [`pretty_tool_input`]'s verbatim fallback — where the
/// blob failed to parse and so can't be round-tripped through serde — to decode
/// just those over-produced printable-ASCII escapes back to their literal char.
/// Restricted to the printable ASCII range (`0x20..=0x7E`) so control characters
/// and a fragment cut mid-escape (`…\u00`) are left untouched: we only un-escape
/// what PowerShell needlessly escaped, never invent structure.
fn decode_printable_ascii_escapes(raw: &str) -> String {
    // Fast path: no escape marker at all (the common truncated-fragment case).
    if !raw.contains("\\u") {
        return raw.to_string();
    }
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < bytes.len() {
        // A full `\uXXXX` escape is 6 bytes; decode only when all four hex digits
        // are present (a truncated tail like `\u00` falls through verbatim). The
        // hex digits are ASCII, so slicing `raw` at those offsets is char-safe.
        if bytes[i] == b'\\'
            && i + 6 <= bytes.len()
            && bytes[i + 1] == b'u'
            && bytes[i + 2..i + 6].iter().all(u8::is_ascii_hexdigit)
        {
            let code = u32::from_str_radix(&raw[i + 2..i + 6], 16).unwrap();
            if (0x20..=0x7E).contains(&code) {
                out.push(code as u8 as char);
                i += 6;
                continue;
            }
        }
        // Not a decodable escape: copy this char through intact (raw is UTF-8, so
        // step by the scalar's byte length to keep multi-byte codepoints whole).
        let ch = raw[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Shell-side pending permission request (SPEC §16). Unlike an ask there is no
/// blocked in-process caller: the `PermissionRequest` hook polls
/// `<data>/perm-answers/<id>.json` directly, so the deck only owns the display
/// state + the decision-file write.
struct ShellPerm {
    id: String,
    session_id: Option<String>,
    project: Option<String>,
    tool_name: String,
    tool_input: String,
    context: Option<String>,
    /// Epoch ms the perm arrived — its position in the shared ask/perm FIFO
    /// (R-16.2). Compared against a front ask's `enqueued_ms` so the ask window's
    /// primary slot follows arrival order, not a blanket perm-over-ask priority.
    /// Also the anchor for the R-32.1 deadline (`received_ms + PERM_DEADLINE_MS`).
    received_ms: u64,
}

impl ShellPerm {
    /// SPEC R-32.1: epoch ms at which this perm expires (its hook has given up).
    fn deadline_ms(&self) -> u64 {
        self.received_ms.saturating_add(PERM_DEADLINE_MS)
    }
}

fn perm_to_row(perm: &ShellPerm) -> PermRow {
    PermRow {
        id: perm.id.clone(),
        session_id: perm.session_id.clone(),
        project: perm.project.clone(),
        tool_name: perm.tool_name.clone(),
        tool_input: perm.tool_input.clone(),
        context: perm.context.clone(),
        queued_at: perm.received_ms,
        // R-32.1: the UI disables Allow/Deny once `now >= expires_at` (until the
        // tick sweep removes the row entirely).
        expires_at: Some(perm.deadline_ms()),
    }
}

/// SPEC R-32.1: remove and return every perm whose deadline has elapsed — the
/// mirror of [`AskStore::sweep_expired`] for the shell-owned perm queue. Pure
/// over `(perms, now)` so it is unit-tested without a live `Shell`.
fn drain_expired_perms(perms: &mut Vec<ShellPerm>, now: u64) -> Vec<ShellPerm> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < perms.len() {
        if now >= perms[i].deadline_ms() {
            out.push(perms.remove(i));
        } else {
            i += 1;
        }
    }
    out
}

/// On-disk shape of a perm request file (`<data>/perms/<id>.json`), written by
/// the `PermissionRequest` hook (SPEC R-16.1). Parsed defensively — every field
/// is optional so a format drift never crashes ingestion.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
struct PermFileRecord {
    #[serde(default)]
    tool_name: String,
    /// Compact JSON of the tool input, already truncated by the hook. A string
    /// (not a nested object) so the 2KB cap is meaningful and the display is a
    /// single deterministic blob.
    #[serde(default)]
    tool_input: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

/// On-disk shape of a persisted ask file (`<data>/asks/<id>.json`, written by
/// [`Shell::write_ask_file`]) — read back only when recovering orphaned asks
/// after a restart (R-8.7).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AskFileRecord {
    #[serde(default)]
    id: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    question: String,
    #[serde(default)]
    options: Option<Vec<String>>,
    /// R-29.2: a recovered multi-question form still renders (as expired). Old
    /// ask files predate this field, so it defaults to `None`.
    #[serde(default)]
    questions: Option<Vec<deck_core::ask::AskQuestion>>,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    context: Option<String>,
}

/// The on-disk answer written by the `answer_ask` command (mirror of
/// `ipc::AnswerRecord`), consumed by the answers watcher to unblock the MCP call.
#[derive(Deserialize)]
struct AnswerFile {
    #[serde(default)]
    answer: String,
    kind: AskAnswerKind,
}

// ---------------------------------------------------------------------------
// The Shell: the single owner of the engine + asks + notifier, shared (behind an
// Arc) by the engine threads, the answers watcher, and the MCP gateway.
// ---------------------------------------------------------------------------

struct Shell {
    app: AppHandle<Wry>,
    store: Mutex<SessionStore>,
    asks: Mutex<AskStore<oneshot::Sender<AskAnswer>>>,
    /// Pending permission requests (§16), owned entirely shell-side.
    perms: Mutex<Vec<ShellPerm>>,
    /// Per-session incremental token telemetry (§23), keyed by session id. Read
    /// (ingested) on the engine tick and just before a finished-toast fires;
    /// projected into `SessionRow` in `snapshot` and mined for the R-24.1 idle
    /// toast body. Pruned to live sessions each update.
    usage: Mutex<HashMap<String, SessionUsageGroup>>,
    /// §34: per-session transcript mtime (epoch ms) at the last `aiTitle` tail
    /// read, keyed by session id. Gates [`Shell::refresh_ai_titles`] so a
    /// transcript is re-read only when its file mtime advanced (R-34), and pruned
    /// to live sessions each pass.
    ai_title_mtime: Mutex<HashMap<String, u64>>,
    notifier: DesktopNotifier<Wry>,
    tray: tauri::tray::TrayIcon<Wry>,
    data_dir: PathBuf,
    version: String,
    /// R-17.1: most recent foreground-pid sample (instant taken + pids). Every
    /// sample site writes through here so latency-critical hot paths — the
    /// R-16.3 perm auto-defer — can reuse a recent sample instead of paying the
    /// ~250ms synchronous foreground-sampler spawn (a `powershell` + CIM query
    /// on Windows) on the ingest hot path. Bounded by [`FOREGROUND_POLL`] so a
    /// reused sample is never older than the R-17.1 poll resolution.
    foreground_cache: Mutex<Option<(Instant, Vec<u32>)>>,
}

impl Shell {
    // --- state projection --------------------------------------------------

    fn snapshot(&self) -> StateSnapshot {
        let (mut sessions, counts) = {
            let store = self.store.lock().expect("store poisoned");
            let sessions: Vec<SessionRow> = store.view().iter().map(map_row).collect();
            let c = store.counts();
            (
                sessions,
                Counts {
                    attention: c.attention,
                    working: c.working,
                    waiting: c.waiting,
                    idle: c.idle,
                    dead: c.dead,
                },
            )
        };
        let asks: Vec<AskRow> = self
            .asks
            .lock()
            .expect("asks poisoned")
            .iter()
            .map(ask_to_row)
            .collect();
        let perms: Vec<PermRow> = self
            .perms
            .lock()
            .expect("perms poisoned")
            .iter()
            .map(perm_to_row)
            .collect();

        let settings = settings::load(&self.data_dir);

        // §23: project per-session token telemetry onto the rows (R-23.4). Only
        // when `showTokenStats` is on (R-23.5/R-23.6 "toggle off hides all of
        // it"); the heavy transcript IO happened on the tick, this is a cheap
        // map lookup per row.
        if settings.show_token_stats {
            let usage = self.usage.lock().expect("usage poisoned");
            for row in &mut sessions {
                let Some(group) = usage.get(&row.id) else {
                    continue;
                };
                row.ctx_percent = group.context_percent();
                let spend = group.spend();
                if spend > 0 {
                    row.spend = Some(usage::format_compact(spend));
                    row.spend_approx = group.spend_approx();
                }
                let group_spend = group.group_spend();
                if group_spend > 0 {
                    row.subagent_spend = Some(usage::format_compact(group_spend));
                }
            }
        }

        let mcp_enabled = settings
            .extra
            .get("mcpEnabled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        // R-8.6: the exact `claude mcp add …` command with the real port + token,
        // surfaced so the user can run it when the `claude` CLI isn't on PATH.
        let mcp_command = mcp_server::load_config().map(|cfg| {
            format!(
                "claude mcp add --transport http --scope user quarterdeck \
                 http://127.0.0.1:{}{} --header \"Authorization: Bearer {}\"",
                cfg.port,
                mcp_server::MCP_PATH,
                cfg.token
            )
        });
        let settings_state = SettingsState {
            notify_idle: settings.notify_idle,
            notify_attention: settings.notify_attention,
            notify_reminder: settings.notify_reminder,
            launch_at_login: settings.launch_at_login,
            onboarding_done: settings.onboarding_done,
            popup_pinned: settings.popup_pinned,
            takeover_permissions: settings.takeover_permissions,
            show_token_stats: settings.show_token_stats,
            popup_mode: settings.popup_mode,
            mcp_enabled,
            mcp_cli_available: claude_cli_on_path(),
            mcp_command,
            data_dir: self.data_dir.display().to_string(),
            version: self.version.clone(),
        };

        StateSnapshot {
            sessions,
            asks,
            perms,
            hooks_installed: self.hooks_installed(),
            counts,
            settings: Some(settings_state),
        }
    }

    /// Rebuild the full snapshot, publish it to the shared [`AppState`], emit
    /// `deck://state`, and refresh the tray (R-3.4, R-2.6). Never call while
    /// holding `store`/`asks` locks.
    fn push_state(&self) {
        let snapshot = self.snapshot();
        if let Some(state) = self.app.try_state::<AppState>() {
            *state.0.lock().expect("state poisoned") = snapshot.clone();
        }
        let _ = self.app.emit(ipc::STATE_EVENT, &snapshot);
        if let Err(err) = tray::update(&self.tray, &snapshot.counts) {
            tracing::warn!(error = %err, "failed to update tray");
        }
    }

    /// Set (or clear) a session's user title override (§27 R-27.4), persist the
    /// map, and push a fresh snapshot so the renamed row surfaces at once. An
    /// empty/whitespace `name` clears the override, restoring the normal title
    /// chain. Sanitization (bidi-strip + 60-grapheme cap, R-27.7) happens inside
    /// [`deck_core::engine::SessionStore::set_override_name`].
    fn rename_session(&self, session_id: &str, name: &str) {
        let trimmed = name.trim();
        let value = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        {
            let mut store = self.store.lock().expect("store poisoned");
            store.set_override_name(session_id, value);
        }
        self.persist_session_names();
        self.push_state();
    }

    /// Re-persist `<data>/session-names.json` when the engine's overrides map has
    /// changed since the last call (R-27.3/R-27.6: rename + end-of-session prune).
    /// A no-op when nothing changed. Serialized so the tick thread and a command
    /// thread can't reorder two writes. Never call while holding `store`.
    fn persist_session_names(&self) {
        static PERSIST_LOCK: Mutex<()> = Mutex::new(());
        let _guard = PERSIST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot = {
            let mut store = self.store.lock().expect("store poisoned");
            if !store.take_overrides_dirty() {
                return;
            }
            store.overrides_snapshot()
        };
        if let Err(err) = session_names::save(&self.data_dir, &snapshot) {
            tracing::warn!(error = %err, "failed to persist session-names.json");
        }
    }

    fn hooks_installed(&self) -> bool {
        match std::fs::read_to_string(claude_settings_path()) {
            Ok(text) => text.contains(hooks_config::MARKER),
            Err(_) => false,
        }
    }

    fn popup_focused(&self) -> bool {
        self.app
            .get_webview_window(windows::POPUP_LABEL)
            .map(|w| w.is_visible().unwrap_or(false) && w.is_focused().unwrap_or(false))
            .unwrap_or(false)
    }

    // --- focus-aware suppression (§17) ------------------------------------

    /// The terminal PIDs for one session (the registry/claude pid, R-17.2).
    /// Empty when the session is unknown or has no known pid.
    fn session_terminal_pids(&self, session_id: &str) -> Vec<u32> {
        self.store
            .lock()
            .expect("store poisoned")
            .terminal_pids()
            .into_iter()
            .find(|(id, _)| id == session_id)
            .map(|(_, pids)| pids)
            .unwrap_or_default()
    }

    /// Take a fresh foreground-pid sample (R-17.1) and store it in the shared
    /// cache so latency-critical hot paths (R-16.3 perm auto-defer) can reuse a
    /// recent sample instead of re-spawning the sampler. Every fresh sample in
    /// the shell goes through here to keep the cache warm.
    fn sample_foreground_and_cache(&self) -> Vec<u32> {
        let fg = foreground::sample_foreground_pids();
        *self
            .foreground_cache
            .lock()
            .expect("foreground cache poisoned") = Some((Instant::now(), fg.clone()));
        fg
    }

    /// The cached foreground sample if it is no older than `max_age`, else
    /// `None` (cold/stale → caller must take a fresh sample).
    fn cached_foreground(&self, max_age: Duration) -> Option<Vec<u32>> {
        self.foreground_cache
            .lock()
            .expect("foreground cache poisoned")
            .as_ref()
            .filter(|(at, _)| at.elapsed() <= max_age)
            .map(|(_, pids)| pids.clone())
    }

    /// R-17.2: is the given session's terminal window the foreground window?
    /// Samples the foreground chain (R-17.1) and intersects with the session's
    /// terminal pids. Unmatched asks (`None`) are never terminal-foreground —
    /// there's no terminal to defer to. A fresh sample is taken here (and cached)
    /// so callers get the "immediately before showing / firing" check R-17.1
    /// requires.
    fn session_foreground(&self, session_id: Option<&str>) -> bool {
        let Some(sid) = session_id else {
            return false;
        };
        let terminal = self.session_terminal_pids(sid);
        if terminal.is_empty() {
            return false;
        }
        let fg = self.sample_foreground_and_cache();
        foreground::session_is_foreground(&terminal, &fg)
    }

    /// R-16.3: like [`Self::session_foreground`] but reuses the cached R-17.1
    /// foreground sample when it is fresher than `max_age`, so the perm
    /// auto-defer decision does not pay the ~250ms synchronous foreground-sampler
    /// spawn on the ingest hot path (which, added to the watcher debounce, blew
    /// the 300ms auto-defer budget). Degrades gracefully: a cold/stale cache
    /// falls back to a fresh sample (still correct, just the old latency).
    fn session_foreground_cached(&self, session_id: Option<&str>, max_age: Duration) -> bool {
        let Some(sid) = session_id else {
            return false;
        };
        let terminal = self.session_terminal_pids(sid);
        if terminal.is_empty() {
            return false;
        }
        let fg = match self.cached_foreground(max_age) {
            Some(cached) => cached,
            None => self.sample_foreground_and_cache(),
        };
        foreground::session_is_foreground(&terminal, &fg)
    }

    /// R-17.2/R-17.1: surface the ask window for any pending ask whose session's
    /// terminal is NOT the foreground window (or is unmatched), but only when the
    /// window is currently hidden. Called after enqueue and on the 2s poll so a
    /// suppressed (queued-but-hidden) ask appears the moment focus leaves its
    /// terminal. Never yanks the window when every pending ask's terminal is
    /// still foreground (R-17.2: the ask stays queued + mirrored in the popup).
    fn maybe_surface_asks(&self) {
        if self.asks_empty() {
            return;
        }
        let ask_visible = self
            .app
            .get_webview_window(windows::ASK_LABEL)
            .and_then(|w| w.is_visible().ok())
            .unwrap_or(false);
        if ask_visible {
            return;
        }
        // Sample the foreground once (caching it for the R-16.3 perm hot path),
        // then check every pending ask against it.
        let fg = self.sample_foreground_and_cache();
        let terminal_pids = self.store.lock().expect("store poisoned").terminal_pids();
        let sessions: Vec<Option<String>> = self
            .asks
            .lock()
            .expect("asks poisoned")
            .iter()
            .map(|a| a.session_id.clone())
            .collect();
        let ready = sessions.iter().any(|sid| match sid {
            None => true, // unmatched ask: nothing to defer to.
            Some(id) => {
                let pids = terminal_pids
                    .iter()
                    .find(|(t, _)| t == id)
                    .map(|(_, p)| p.as_slice())
                    .unwrap_or(&[]);
                !foreground::session_is_foreground(pids, &fg)
            }
        });
        if ready {
            run_on_main(&self.app, |app| {
                if let Err(err) = windows::show_ask_window(app) {
                    tracing::warn!(error = %err, "failed to surface ask window (R-17.2)");
                }
            });
        }
    }

    // --- toast firing ------------------------------------------------------

    /// Fire engine-emitted toast decisions, honoring the R-9.5 per-type toggles
    /// (the engine cannot see user settings; the shell gates here) and the R-9.4
    /// popup-focus suppression (handled inside the notifier).
    fn fire_effects(&self, effects: Vec<Effect>) {
        if effects.is_empty() {
            return;
        }
        let settings = settings::load(&self.data_dir);
        let popup = self.popup_focused();
        // R-17.1: sample the foreground chain once for this batch (caching it for
        // the R-16.3 perm hot path); a toast whose session terminal is the
        // foreground window is suppressed (R-17.2).
        let foreground = self.sample_foreground_and_cache();
        let terminal_pids = self.store.lock().expect("store poisoned").terminal_pids();
        for effect in effects {
            let Effect::Toast(decision) = effect;
            let enabled = match decision.kind {
                ToastKind::Idle => settings.notify_idle,
                ToastKind::Attention => settings.notify_attention,
                // §47: the reminder is retired — the engine no longer emits
                // `ToastKind::Reminder`, so this arm is unreachable at runtime.
                // Kept (still reading the backward-compat `notify_reminder`
                // toggle) only for match exhaustiveness over `ToastKind`.
                ToastKind::Reminder => settings.notify_reminder,
                ToastKind::Ask => true,
            };
            // R-17.2: suppress when the session's terminal is the foreground
            // window (the user is already looking at it).
            let terminal_foreground = terminal_pids
                .iter()
                .find(|(id, _)| *id == decision.session_id)
                .map(|(_, pids)| foreground::session_is_foreground(pids, &foreground))
                .unwrap_or(false);
            // A toast actually shows only if its R-9.5 toggle is on, its session's
            // terminal isn't foreground (R-17.2), AND the notifier didn't suppress
            // it (R-9.4 popup-visible-and-focused suppression, applied inside
            // `notify`, which returns whether a toast fired). Short-circuit so
            // `notify` is not called when the toggle is off / terminal foreground.
            // R-24.1: a finished (idle) toast carries the model's last words as
            // its body when the §23 reader has them; every other kind carries
            // its own message/question (assistant_body ignored). Only read the
            // transcript when the toast will actually be attempted (R-23.5 perf).
            let will_attempt = enabled && !terminal_foreground;
            let assistant_body = if will_attempt && decision.kind == ToastKind::Idle {
                self.idle_assistant_body(&decision.session_id)
            } else {
                None
            };
            let shown = will_attempt
                && self.notifier.send(
                    crate::notify::ToastRequest {
                        kind: decision.kind,
                        session_id: decision.session_id.clone(),
                        project: decision.project.clone(),
                        detail: decision.detail.clone(),
                        assistant_body,
                    },
                    popup,
                );
            if !shown {
                // R-9.4/R-9.5: the engine already stamped this (session, kind)
                // throttle slot when it emitted the decision, but no toast
                // actually showed — the R-9.5 per-type toggle is off, the
                // session's terminal is the foreground window (R-17.2), or the
                // notifier suppressed it because the popup is visible AND focused
                // (R-9.4). Release the slot so a later same-kind toast isn't
                // dropped for up to 10 s (R-17.2 "suppressed toasts refund the
                // throttle slot"), once the toggle is re-enabled or focus leaves.
                self.store.lock().expect("store poisoned").refund_toast(
                    &decision.session_id,
                    decision.kind,
                    decision.at_ms,
                );
            }
        }
    }

    // --- token telemetry (§23) --------------------------------------------

    /// Whether the §23 token-stats feature is enabled (`showTokenStats`,
    /// default ON, R-23.5). Read fresh so a toggle takes effect at once.
    fn token_stats_enabled(&self) -> bool {
        settings::load(&self.data_dir).show_token_stats
    }

    /// Drive the incremental usage reader for every live session (R-23.1),
    /// pruning state for gone sessions. Called on the engine tick (≤1 read per
    /// session per tick, R-23.5). A no-op — clearing any accumulated state — when
    /// the feature toggle is off (R-23.6 "toggle off hides all of it").
    fn update_usage(&self) {
        if !self.token_stats_enabled() {
            let mut usage = self.usage.lock().expect("usage poisoned");
            if !usage.is_empty() {
                usage.clear();
            }
            return;
        }
        let transcripts = self
            .store
            .lock()
            .expect("store poisoned")
            .session_transcripts();
        let present: std::collections::HashSet<&str> =
            transcripts.iter().map(|(id, _)| id.as_str()).collect();
        let mut usage = self.usage.lock().expect("usage poisoned");
        usage.retain(|id, _| present.contains(id.as_str()));
        for (id, path) in &transcripts {
            let Some(path) = path.as_deref().filter(|p| !p.is_empty()) else {
                continue;
            };
            usage.entry(id.clone()).or_default().update(Path::new(path));
        }
    }

    /// §34: refresh each session's transcript `aiTitle` — the terminal-tab chat
    /// name that is the default row title (R-34). mtime-gated (a transcript is
    /// re-read only when its file mtime advanced since the last read) and reads
    /// only the TAIL (last [`AI_TITLE_TAIL_BYTES`]), since `aiTitle` is rewritten
    /// on recent lines — a multi-MB transcript is never read whole. Failure-
    /// tolerant: a missing/unreadable transcript, or one with no `aiTitle` yet,
    /// leaves the cached title untouched (never clobbers a known name with a
    /// blank). Runs on the shell tick before `push_state`.
    fn refresh_ai_titles(&self) {
        let transcripts = self
            .store
            .lock()
            .expect("store poisoned")
            .session_transcripts();
        let present: std::collections::HashSet<&str> =
            transcripts.iter().map(|(id, _)| id.as_str()).collect();
        let mut seen = self.ai_title_mtime.lock().expect("ai_title_mtime poisoned");
        seen.retain(|id, _| present.contains(id.as_str()));
        for (id, path) in &transcripts {
            let Some(path) = path.as_deref().filter(|p| !p.is_empty()) else {
                continue;
            };
            let Some(mtime) = transcript_mtime_ms(path) else {
                continue; // missing/unreadable — leave ai_title as-is (R-34).
            };
            if seen.get(id) == Some(&mtime) {
                continue; // unchanged since the last read — skip the tail read.
            }
            seen.insert(id.clone(), mtime);
            let Some(bytes) = read_transcript_tail(path, AI_TITLE_TAIL_BYTES) else {
                continue;
            };
            // Only push through a found title; an absent `aiTitle` (early in a
            // session) must not clear a previously-read one.
            if let Some(title) = extract_ai_title(&bytes) {
                self.store
                    .lock()
                    .expect("store poisoned")
                    .set_ai_title(id, Some(title));
            }
        }
    }

    /// R-24.1: the model's last words for a session's finished toast, refreshed
    /// on demand (the Stop event arrives on the spool path, not the tick, so the
    /// tail may be newer than the last tick's read), then sanitized (bidi strip,
    /// R-16.5), whitespace-collapsed, and truncated to the idle-body budget.
    /// `None` when the feature is off, no assistant text is known yet, or the
    /// transcript is unavailable — the toast then falls back to the R-9.1 copy.
    fn idle_assistant_body(&self, session_id: &str) -> Option<String> {
        if !self.token_stats_enabled() {
            return None;
        }
        let path = self
            .store
            .lock()
            .expect("store poisoned")
            .transcript_path_of(session_id)
            .filter(|p| !p.is_empty())?;
        let text = {
            let mut usage = self.usage.lock().expect("usage poisoned");
            let group = usage.entry(session_id.to_string()).or_default();
            group.update(Path::new(&path));
            group.last_assistant_text().map(str::to_string)
        }?;
        let cleaned = strip_bidi_controls(&text);
        let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
        let out = truncate_graphemes(&collapsed, IDLE_BODY_MAX_CHARS);
        (!out.trim().is_empty()).then_some(out)
    }

    // --- ask channel (§8) --------------------------------------------------

    fn match_session(&self, context: Option<&str>) -> (Option<String>, Option<String>) {
        let Some(ctx) = context.map(str::trim).filter(|s| !s.is_empty()) else {
            return (None, None);
        };
        let views = self.store.lock().expect("store poisoned").view();
        for v in &views {
            if !v.cwd.is_empty() && paths_eq(&v.cwd, ctx) {
                return (Some(v.id.clone()), Some(v.project.clone()));
            }
        }
        let base = basename(ctx);
        for v in &views {
            // Reuse `paths_eq` so the basename fallback folds case the same
            // (Unicode-wide) way the full-path match does (R-8.2, R-5.3) — a
            // plain `==` here would miss a Cyrillic case-only difference.
            if !v.cwd.is_empty() && paths_eq(&basename(&v.cwd), &base) {
                return (Some(v.id.clone()), Some(v.project.clone()));
            }
        }
        (None, None)
    }

    fn ask_file_path(&self, id: &str) -> PathBuf {
        settings::asks_dir().join(format!("{id}.json"))
    }

    fn write_ask_file(&self, ask: &ShellAsk) {
        let record = serde_json::json!({
            "id": ask.id,
            "sessionId": ask.session_id,
            "project": ask.project,
            "question": ask.question,
            "options": ask.options,
            "questions": ask.questions,
            "detail": ask.detail,
            "context": ask.context,
            "persistent": ask.persistent,
            "timeoutAtMs": ask.timeout_at_ms,
            "createdAtMs": now_ms(),
        });
        if let Ok(bytes) = serde_json::to_vec_pretty(&record) {
            if let Err(err) = settings::atomic_write(&self.ask_file_path(&ask.id), &bytes) {
                tracing::warn!(error = %err, ask_id = %ask.id, "failed to persist ask file");
            }
        }
    }

    fn submit_ask(&self, req: AskRequest) -> SubmittedAsk {
        let (tx, rx) = oneshot::channel();
        let ask_id = format!(
            "ask-{}-{}",
            now_ms(),
            ASK_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let (session_id, project) = self.match_session(req.context.as_deref());
        // R-19.2: `timeout_seconds` is `None` for a persistent ask (no expiry).
        let persistent = req.timeout_seconds.is_none();
        let timeout_at_ms = match req.timeout_seconds {
            Some(secs) => now_ms() + secs.saturating_mul(1000),
            None => 0,
        };

        if let Some(sid) = &session_id {
            self.store
                .lock()
                .expect("store poisoned")
                .note_pending_ask(sid);
        }

        let ask = ShellAsk {
            id: ask_id.clone(),
            session_id: session_id.clone(),
            project: project.clone(),
            question: req.question.clone(),
            options: req.options.clone(),
            questions: req.questions.clone(),
            detail: req.detail.clone(),
            context: req.context.clone(),
            enqueued_ms: now_ms(),
            timeout_at_ms,
            persistent,
            orphaned: false,
            responder: Some(tx),
        };
        self.write_ask_file(&ask);
        self.asks.lock().expect("asks poisoned").push(ask);

        self.push_state();

        // R-17.2: if the matched session's terminal is the foreground window,
        // the ask window does NOT auto-appear (the ask stays queued + mirrored in
        // the popup, surfacing as soon as focus leaves — see `maybe_surface_asks`
        // + the 2s poll) and the alert toast is suppressed. Otherwise the
        // always-on-top ask window comes up (never steals focus; R-8.3).
        let terminal_foreground = self.session_foreground(session_id.as_deref());
        if terminal_foreground {
            tracing::debug!(ask_id = %ask_id, "ask suppressed: session terminal is foreground (R-17.2)");
        } else {
            run_on_main(&self.app, |app| {
                if let Err(err) = windows::show_ask_window(app) {
                    tracing::warn!(error = %err, "failed to show ask window");
                }
            });
            // R-8.4: alert toast, exempt from throttle/toggles (but R-17.2 applies).
            let toast_project = project
                .or_else(|| req.context.as_deref().map(basename))
                .unwrap_or_else(|| "Agent".to_string());
            self.notifier.notify(
                ToastKind::Ask,
                &ask_id,
                &toast_project,
                &req.question,
                self.popup_focused(),
            );
        }

        tracing::info!(ask_id = %ask_id, matched = session_id.is_some(), "ask submitted");
        SubmittedAsk { id: ask_id, rx }
    }

    /// R-19.5 `update_ask`: mutate a pending ask in place. Keeps its queue
    /// position; re-persists the ask file; re-renders the UI. Returns `false`
    /// for an unknown/settled/orphaned id (the MCP layer maps that to an error
    /// result).
    fn update_ask(
        &self,
        ask_id: &str,
        question: Option<String>,
        options: Option<Vec<String>>,
        detail: Option<String>,
    ) -> bool {
        {
            let mut asks = self.asks.lock().expect("asks poisoned");
            let Some(ask) = asks.get_mut(ask_id) else {
                return false;
            };
            // An orphaned (expired) ask can no longer be answered, so it can't be
            // meaningfully revised either.
            if ask.orphaned {
                return false;
            }
            if let Some(q) = question {
                if !q.is_empty() {
                    ask.question = q;
                }
            }
            if let Some(o) = options {
                ask.options = if o.is_empty() { None } else { Some(o) };
            }
            if let Some(d) = detail {
                ask.detail = if d.is_empty() { None } else { Some(d) };
            }
            // Re-persist the revised ask (write_ask_file touches disk, not the
            // asks lock, so this shared reborrow is safe under the guard).
            self.write_ask_file(ask);
        }
        self.push_state();
        tracing::info!(ask_id = %ask_id, "ask updated (R-19.5)");
        true
    }

    /// R-19.5 `cancel_ask`: resolve a pending ask toward the blocked caller with
    /// `kind:"cancelled"` and remove it from the UI. Returns `false` for an
    /// unknown/settled id.
    fn cancel_ask(&self, ask_id: &str) -> bool {
        let Some(mut ask) = self.take_ask(ask_id) else {
            return false;
        };
        if let Some(tx) = ask.responder.take() {
            let _ = tx.send(AskAnswer::cancelled());
        }
        if let Some(sid) = &ask.session_id {
            self.store
                .lock()
                .expect("store poisoned")
                .note_ask_cleared(sid);
        }
        let _ = std::fs::remove_file(self.ask_file_path(&ask.id));
        self.push_state();
        if self.ask_window_idle() {
            run_on_main(&self.app, |app| {
                let _ = windows::hide_ask_window(app);
            });
        }
        tracing::info!(ask_id = %ask_id, "ask cancelled (R-19.5)");
        true
    }

    fn notify_user(&self, req: NotifyRequest) -> String {
        let record_id = format!(
            "ntf-{}-{}",
            now_ms(),
            NOTIFY_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        // R-9.5: the MCP `notify_user` toast uses the alert (Attention) channel,
        // so it honors the `notifyAttention` toggle just like hook-driven
        // attention toasts (`fire_effects`). Unlike `ask_user` (R-8.4 "Ask toasts
        // never suppressed"), no spec text exempts `notify_user` from the toggle.
        if !settings::load(&self.data_dir).notify_attention {
            tracing::debug!("notify_user suppressed: notifyAttention is off (R-9.5)");
            // R-19.6: still return a record id — the notification was accepted,
            // just not shown (the toggle is off), same as a throttled toast.
            return record_id;
        }
        let (session_id, project) = self.match_session(req.context.as_deref());
        let project = project
            .or_else(|| req.context.as_deref().map(basename))
            .unwrap_or_else(|| "Agent".to_string());
        // R-9.4: throttle notifications per calling context (cwd), not per call.
        // A per-call `now_ms()` key was unique every time, so the 10 s (key, kind)
        // collapse never applied and a looping agent could flood the desktop.
        //
        // The key is ALWAYS namespaced under `notify-…`, even for a matched
        // session: a `notify_user` FYI must not share the throttle bucket with
        // that session's genuine `(session_id, Attention)` status-change alert.
        // If it did, a chatty agent calling `notify_user` on its cwd every <10 s
        // would keep the bucket hot and throttle away the session's real
        // permission-prompt "needs you" toast (R-9.2) — the app's core purpose.
        // Matched calls still throttle per session (`notify-<sid>`), unmatched
        // per context, so `notify_user`'s own R-9.4 flood guard is unaffected.
        let key = match session_id {
            Some(sid) => format!("notify-{sid}"),
            None => match req
                .context
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(ctx) => format!("notify-{ctx}"),
                None => "notify-agent".to_string(),
            },
        };
        // R-19.6: fire with the record id so the fake-notifier jsonl carries it.
        self.notifier.send_with_id(
            crate::notify::ToastRequest {
                kind: ToastKind::Attention,
                session_id: key,
                project,
                detail: req.message.clone(),
                assistant_body: None,
            },
            self.popup_focused(),
            &record_id,
        );
        record_id
    }

    /// R-8.7: at startup, recover any ask files left by a previous process. Their
    /// MCP connections died with that process, so they can never be answered — but
    /// rather than vanishing silently, they are loaded back as `orphaned` rows
    /// (no responder, exempt from the timeout sweep) so the UI shows them as
    /// **expired** until the user dismisses them, "never answered into the void".
    fn orphan_stale_asks(&self) {
        let records = recover_ask_files(&settings::asks_dir(), &settings::spool_quarantine_dir());
        let recovered = records.len();
        for rec in records {
            let ask = ShellAsk {
                id: rec.id,
                session_id: rec.session_id,
                project: rec.project,
                question: rec.question,
                options: rec.options,
                questions: rec.questions,
                detail: rec.detail,
                context: rec.context,
                // Recovered from a previous run → older than anything enqueued
                // this session; 0 sorts it to the front of the FIFO (R-16.2).
                enqueued_ms: 0,
                timeout_at_ms: 0,
                persistent: false,
                orphaned: true,
                responder: None,
            };
            self.asks.lock().expect("asks poisoned").push(ask);
        }
        if recovered > 0 {
            tracing::warn!(
                recovered,
                "recovered orphaned asks from a previous run, shown as expired (R-8.7)"
            );
            self.push_state();
            run_on_main(&self.app, |app| {
                if let Err(err) = windows::show_ask_window(app) {
                    tracing::warn!(error = %err, "failed to show ask window for orphaned asks");
                }
            });
        }
    }

    fn take_ask(&self, id: &str) -> Option<ShellAsk> {
        self.asks.lock().expect("asks poisoned").take(id)
    }

    fn asks_empty(&self) -> bool {
        self.asks.lock().expect("asks poisoned").is_empty()
    }

    /// Resolve an answer written to `<data>/answers/<askId>.json` (by the
    /// `answer_ask` command) into the blocked MCP `ask_user` call.
    fn resolve_answer(&self, path: &Path) {
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            return;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            // R-3.5 "(also applies to asks/answers)": quarantine + log, never crash.
            tracing::warn!(?path, "quarantining unreadable answer file (R-3.5)");
            quarantine_bad_file(path);
            return;
        };
        let Ok(parsed) = serde_json::from_str::<AnswerFile>(&text) else {
            tracing::warn!(?path, "quarantining malformed answer file (R-3.5)");
            quarantine_bad_file(path);
            return;
        };

        let Some(mut ask) = self.take_ask(&id) else {
            // Already resolved / not ours — consume the file so it isn't reprocessed.
            let _ = std::fs::remove_file(path);
            return;
        };

        let answer = AskAnswer {
            answer: parsed.answer,
            kind: parsed.kind,
        };
        // Whether the answer actually reached the blocked `ask_user` call. The
        // send fails when the MCP side already gave up (its `timeout_seconds`
        // fired and it returned `timeout`, dropping the receiver) or the ask was
        // orphaned (no responder). In that race — up to one engine tick wide,
        // before `sweep_expired_asks` removes the row — the ask is still visible
        // and answerable; we must NOT then flip the session to Working as if the
        // agent received the answer (it didn't).
        let delivered = match ask.responder.take() {
            Some(tx) => tx.send(answer).is_ok(),
            None => false,
        };
        if let Some(sid) = &ask.session_id {
            let mut store = self.store.lock().expect("store poisoned");
            if delivered {
                match parsed.kind {
                    // R-29.2: a submitted form counts as answered, same as an
                    // option/text answer (the agent received the user's decision).
                    AskAnswerKind::Option | AskAnswerKind::Text | AskAnswerKind::Form => {
                        store.note_ask_answered(sid)
                    }
                    // §46: "In terminal" is not an answer here — the user will
                    // answer via the native picker, so clear the pending-ask
                    // override like a dismiss (the status recomputes from the
                    // last hook state).
                    AskAnswerKind::Timeout
                    | AskAnswerKind::Dismissed
                    | AskAnswerKind::Cancelled
                    | AskAnswerKind::Terminal => store.note_ask_cleared(sid),
                }
            } else {
                // Agent already stopped waiting: just drop the pending-ask
                // override so the status recomputes from the last hook state
                // (R-2.4), rather than mis-reporting the session as Working.
                store.note_ask_cleared(sid);
            }
        }
        if !delivered {
            tracing::warn!(
                ask_id = %id,
                "answer arrived after the ask was no longer awaiting (timed out/orphaned); not delivered to the agent"
            );
        }
        let _ = std::fs::remove_file(self.ask_file_path(&ask.id));
        let _ = std::fs::remove_file(path);

        self.push_state();
        if self.ask_window_idle() {
            run_on_main(&self.app, |app| {
                let _ = windows::hide_ask_window(app);
            });
        }
        tracing::info!(ask_id = %id, "ask answered");
    }

    /// Drop asks whose timeout has elapsed (the MCP call already returned
    /// `timeout` via its own timer); recompute the session status (R-2.4).
    /// Orphaned (shown-as-expired) asks are exempt (handled in [`AskStore`]).
    fn sweep_expired_asks(&self) {
        let expired = self.asks.lock().expect("asks poisoned").sweep_expired();
        if expired.is_empty() {
            return;
        }
        for ask in &expired {
            if let Some(sid) = &ask.session_id {
                self.store
                    .lock()
                    .expect("store poisoned")
                    .note_ask_cleared(sid);
            }
            let _ = std::fs::remove_file(self.ask_file_path(&ask.id));
            tracing::info!(ask_id = %ask.id, "ask timed out");
        }
        if self.ask_window_idle() {
            run_on_main(&self.app, |app| {
                let _ = windows::hide_ask_window(app);
            });
        }
    }

    /// SPEC R-32.1: drop perms whose deadline (`received_ms + PERM_DEADLINE_MS`)
    /// has elapsed — their `PermissionRequest` hook has already timed out, so no
    /// deck decision could reach it. The mirror of [`Shell::sweep_expired_asks`]
    /// for the shell-owned perm queue; runs on the same 10 s tick. Recomputes the
    /// session status (R-2.4, via `note_ask_cleared`) and re-renders the shared
    /// ask/perm window so the next queued item surfaces.
    fn sweep_expired_perms(&self) {
        let expired = {
            let mut perms = self.perms.lock().expect("perms poisoned");
            drain_expired_perms(&mut perms, now_ms())
        };
        if expired.is_empty() {
            return;
        }
        for perm in &expired {
            if let Some(sid) = &perm.session_id {
                self.store
                    .lock()
                    .expect("store poisoned")
                    .note_ask_cleared(sid);
            }
            tracing::info!(perm_id = %perm.id, "perm expired past its deadline (R-32.1)");
        }
        self.push_state();
        if self.ask_window_idle() {
            run_on_main(&self.app, |app| {
                let _ = windows::hide_ask_window(app);
            });
        }
    }

    /// SPEC R-32.2: dismiss every pending ask + perm belonging to a session that
    /// just ended (`SessionEnd`) or died (liveness) — the agent that raised them
    /// is gone, so no answer could ever reach it. Cancels each ask via the reused
    /// [`Shell::cancel_ask`] path (the blocked MCP caller unblocks with
    /// `kind:"cancelled"`, and the ask window re-renders to the next queued item)
    /// and drops the session's perms, re-rendering the shared window.
    fn dismiss_gone_sessions(&self, session_ids: &[String]) {
        let mut perms_dropped = false;
        for sid in session_ids {
            // Cancel each pending ask attributed to this session. `cancel_ask`
            // itself re-renders (push_state) + hides the window when idle, so a
            // freshly-surfaced next item is handled per-ask.
            let ask_ids: Vec<String> = {
                let asks = self.asks.lock().expect("asks poisoned");
                asks.iter()
                    .filter(|a| a.session_id.as_deref() == Some(sid.as_str()))
                    .map(|a| a.id.clone())
                    .collect()
            };
            for id in &ask_ids {
                self.cancel_ask(id);
            }
            // Drop the session's pending perms (no blocked in-process caller to
            // notify — the hook died with the agent; the perm-answer file, if any
            // late one lands, is reaped by `sweep_stale_perm_answers`).
            let dropped: Vec<ShellPerm> = {
                let mut perms = self.perms.lock().expect("perms poisoned");
                let mut out = Vec::new();
                let mut i = 0;
                while i < perms.len() {
                    if perms[i].session_id.as_deref() == Some(sid.as_str()) {
                        out.push(perms.remove(i));
                    } else {
                        i += 1;
                    }
                }
                out
            };
            for perm in &dropped {
                if let Some(psid) = &perm.session_id {
                    self.store
                        .lock()
                        .expect("store poisoned")
                        .note_ask_cleared(psid);
                }
                perms_dropped = true;
                tracing::info!(perm_id = %perm.id, session_id = %sid, "perm dropped: session ended/died (R-32.2)");
            }
        }
        // `cancel_ask` already refreshed the UI per ask; a perms-only drop still
        // needs one push + a window-hide when nothing remains pending.
        if perms_dropped {
            self.push_state();
            if self.ask_window_idle() {
                run_on_main(&self.app, |app| {
                    let _ = windows::hide_ask_window(app);
                });
            }
        }
    }

    /// SPEC R-32.2: drain the engine's just-ended / just-dead session ids and
    /// dismiss their pending asks + perms. Called on the tick and after each live
    /// event burst so an externally-resolved ask/perm clears promptly.
    fn reap_gone_sessions(&self) {
        let gone = {
            let mut store = self.store.lock().expect("store poisoned");
            store.take_gone_sessions()
        };
        if !gone.is_empty() {
            self.dismiss_gone_sessions(&gone);
        }
    }

    // --- permission requests (§16) ----------------------------------------

    fn perms_empty(&self) -> bool {
        self.perms.lock().expect("perms poisoned").is_empty()
    }

    /// Should the ask window be hidden now? Only when NEITHER an ask nor a perm
    /// is pending — the two share the one always-on-top window (R-16.2).
    fn ask_window_idle(&self) -> bool {
        self.asks_empty() && self.perms_empty()
    }

    /// Match a perm to a known session: by `session_id` first (perms carry the
    /// real Claude session id, R-16.1), else by cwd (R-8.2-style fallback).
    /// Returns `(session_id, project, unmatched_context)`.
    fn match_perm_session(
        &self,
        rec: &PermFileRecord,
    ) -> (Option<String>, Option<String>, Option<String>) {
        let views = self.store.lock().expect("store poisoned").view();
        if let Some(sid) = rec
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if let Some(v) = views.iter().find(|v| v.id == sid) {
                return (Some(v.id.clone()), Some(v.project.clone()), None);
            }
        }
        if let Some(cwd) = rec.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            for v in &views {
                if !v.cwd.is_empty() && paths_eq(&v.cwd, cwd) {
                    return (Some(v.id.clone()), Some(v.project.clone()), None);
                }
            }
            return (None, None, Some(cwd.to_string()));
        }
        (None, None, None)
    }

    /// Ingest one `<data>/perms/*.json` file (R-16.1): parse defensively, match a
    /// session, and either surface a deck modal (attention + alert toast + ask
    /// window) or, if the session's terminal is already the foreground window,
    /// auto-defer to the terminal dialog (R-16.3). The perm file is consumed
    /// (the deck now owns the state); a malformed one is quarantined (R-3.5).
    fn ingest_perm_path(&self, quarantine_dir: &Path, path: &Path) {
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            return;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            return;
        };
        // Already tracked (the safety-net drain re-reads the dir): consume + skip.
        if self
            .perms
            .lock()
            .expect("perms poisoned")
            .iter()
            .any(|p| p.id == id)
        {
            let _ = std::fs::remove_file(path);
            return;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            tracing::warn!(?path, "quarantining unreadable perm file (R-3.5)");
            quarantine_bad_file_to(path, quarantine_dir);
            return;
        };
        let Ok(rec) = serde_json::from_str::<PermFileRecord>(&text) else {
            tracing::warn!(?path, "quarantining malformed perm file (R-3.5)");
            quarantine_bad_file_to(path, quarantine_dir);
            return;
        };
        // We own the state now — consume the source file.
        let _ = std::fs::remove_file(path);

        let (session_id, project, context) = self.match_perm_session(&rec);

        // R-16.5: display tool_name + input VERBATIM but sanitized (bidi strip)
        // and length-capped.
        let tool_name = truncate_graphemes(
            strip_bidi_controls(rec.tool_name.trim()).trim(),
            PERM_TOOL_NAME_CAP,
        );
        let tool_input = truncate_graphemes(
            &strip_bidi_controls(&pretty_tool_input(&rec.tool_input)),
            PERM_INPUT_CAP,
        );
        let tool_name = if tool_name.is_empty() {
            "(unknown tool)".to_string()
        } else {
            tool_name
        };

        // R-16.3 / R-17.2: if the session's terminal window is the foreground,
        // the dialog is already right in front of the user — auto-defer to the
        // terminal (write a "no decision" answer so the hook exits silently) and
        // show nothing. Reuse a recent (≤FOREGROUND_POLL) foreground sample so
        // this stays within the 300ms auto-defer budget instead of spawning a
        // fresh ~250ms foreground sampler on the ingest hot path.
        if self.session_foreground_cached(session_id.as_deref(), FOREGROUND_POLL) {
            if let Err(err) = ipc::write_perm_answer_file(
                &settings::perm_answers_dir(),
                &id,
                PermDecision::Defer,
                None,
            ) {
                tracing::warn!(perm_id = %id, error = %err, "failed to write auto-defer perm answer");
            }
            tracing::debug!(perm_id = %id, "perm auto-deferred: session terminal is foreground (R-16.3)");
            return;
        }

        // Pending perm forces the session to attention (R-16.2), reusing the
        // ask override counter (any pending ask/perm ⇒ attention, R-2.4).
        if let Some(sid) = &session_id {
            self.store
                .lock()
                .expect("store poisoned")
                .note_pending_ask(sid);
        }

        let perm = ShellPerm {
            id: id.clone(),
            session_id: session_id.clone(),
            project: project.clone(),
            tool_name: tool_name.clone(),
            tool_input,
            context,
            received_ms: now_ms(),
        };
        self.perms.lock().expect("perms poisoned").push(perm);
        self.push_state();

        // R-16.2: bring up the always-on-top ask window (shared with asks).
        run_on_main(&self.app, |app| {
            if let Err(err) = windows::show_ask_window(app) {
                tracing::warn!(error = %err, "failed to show ask window for perm");
            }
        });

        // R-16.2: alert toast, same class as R-9.2 (attention), honoring the
        // per-type toggle (R-9.5). Suppressed when the terminal is foreground is
        // moot here — that path auto-deferred above.
        if settings::load(&self.data_dir).notify_attention {
            let toast_project = project
                .or_else(|| rec.cwd.as_deref().map(basename))
                .unwrap_or_else(|| "Agent".to_string());
            self.notifier.notify(
                ToastKind::Attention,
                &format!("perm-{id}"),
                &toast_project,
                &format!("requests permission to run {tool_name}"),
                self.popup_focused(),
            );
        }

        tracing::info!(perm_id = %id, matched = session_id.is_some(), "perm ingested");
    }

    /// Answer a pending perm (from the deck UI): persist the decision for the
    /// blocked hook to poll, drop the perm + its attention override, and hide the
    /// ask window if nothing else is pending (SPEC §16, R-16.2).
    fn answer_perm(
        &self,
        perm_id: &str,
        decision: PermDecision,
        reason: Option<&str>,
    ) -> Result<(), String> {
        ipc::write_perm_answer_file(&settings::perm_answers_dir(), perm_id, decision, reason)?;
        let removed = {
            let mut perms = self.perms.lock().expect("perms poisoned");
            perms
                .iter()
                .position(|p| p.id == perm_id)
                .map(|i| perms.remove(i))
        };
        if let Some(perm) = removed {
            if let Some(sid) = &perm.session_id {
                self.store
                    .lock()
                    .expect("store poisoned")
                    .note_ask_cleared(sid);
            }
        }
        self.push_state();
        if self.ask_window_idle() {
            run_on_main(&self.app, |app| {
                let _ = windows::hide_ask_window(app);
            });
        }
        tracing::info!(perm_id = %perm_id, ?decision, "perm answered");
        Ok(())
    }
}

/// Scan `asks_dir` for pending ask files left behind by a previous process
/// (R-8.7). Valid files are consumed (removed) and returned as records so the
/// caller can surface them as orphaned/expired asks; malformed or unreadable
/// files are quarantined into `quarantine_dir` + logged, never silently deleted
/// (R-3.5 "also applies to asks/answers"). Pure disk I/O with explicit
/// directories, so it is unit-testable without a `Shell`/`AppHandle`.
fn recover_ask_files(asks_dir: &Path, quarantine_dir: &Path) -> Vec<AskFileRecord> {
    let Ok(entries) = std::fs::read_dir(asks_dir) else {
        return Vec::new();
    };
    let mut recovered = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            tracing::warn!(?path, "quarantining unreadable ask file (R-3.5)");
            quarantine_bad_file_to(&path, quarantine_dir);
            continue;
        };
        let parsed = serde_json::from_str::<AskFileRecord>(&text)
            .ok()
            .filter(|rec| !rec.id.is_empty());
        let Some(rec) = parsed else {
            tracing::warn!(?path, "quarantining malformed ask file (R-3.5)");
            quarantine_bad_file_to(&path, quarantine_dir);
            continue;
        };
        // Valid file: the pending call died with the previous process and can
        // never be fulfilled again — consume it and hand the record back.
        let _ = std::fs::remove_file(&path);
        recovered.push(rec);
    }
    recovered
}

/// Move a malformed/unreadable file into `<data>/spool-quarantine/` (R-3.5 "also
/// applies to asks/answers"), disambiguating name collisions, and never leaving
/// it in place to be reprocessed forever. Best-effort: if the move fails the file
/// is at least removed so it can't loop.
fn quarantine_bad_file(path: &Path) {
    quarantine_bad_file_to(path, &settings::spool_quarantine_dir());
}

/// [`quarantine_bad_file`] with an explicit destination dir, so the recovery
/// scanners and tests don't have to route through the env-derived data root.
fn quarantine_bad_file_to(path: &Path, dir: &Path) {
    if std::fs::create_dir_all(dir).is_err() {
        let _ = std::fs::remove_file(path);
        return;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed.json".to_string());
    let mut dest = dir.join(&name);
    let mut n = 1u32;
    while dest.exists() {
        dest = dir.join(format!("{name}.{n}"));
        n += 1;
    }
    if std::fs::rename(path, &dest).is_err() {
        // Cross-device or racing rename: best-effort copy, then remove either way
        // so the bad file can never be reprocessed in a loop.
        let _ = std::fs::copy(path, &dest);
        let _ = std::fs::remove_file(path);
    }
}

/// Runs `f` on the Tauri main thread — the safe way to touch window geometry
/// from the engine / gateway threads.
fn run_on_main<F: FnOnce(&AppHandle<Wry>) + Send + 'static>(app: &AppHandle<Wry>, f: F) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || f(&handle));
}

// ---------------------------------------------------------------------------
// MCP gateway (T7 seam): binds the transport to the engine + UI.
// ---------------------------------------------------------------------------

struct EngineGateway {
    shell: Arc<Shell>,
}

impl AskGateway for EngineGateway {
    fn submit_ask(&self, req: AskRequest) -> SubmittedAsk {
        self.shell.submit_ask(req)
    }

    fn update_ask(
        &self,
        ask_id: &str,
        question: Option<String>,
        options: Option<Vec<String>>,
        detail: Option<String>,
    ) -> bool {
        self.shell.update_ask(ask_id, question, options, detail)
    }

    fn cancel_ask(&self, ask_id: &str) -> bool {
        self.shell.cancel_ask(ask_id)
    }

    fn notify(&self, req: NotifyRequest) -> String {
        self.shell.notify_user(req)
    }

    fn orphan_stale_asks(&self) {
        self.shell.orphan_stale_asks();
    }
}

// ---------------------------------------------------------------------------
// Engine loop: startup replay (R-3.5) + cold-start discovery (R-5.4), then live
// spool consumption + the 10 s liveness/recovery/prune tick.
// ---------------------------------------------------------------------------

fn run_engine(shell: Arc<Shell>) {
    let spool_dir = settings::spool_dir();
    let quarantine_dir = settings::spool_quarantine_dir();

    // R-27.3: load persisted user title overrides into the engine BEFORE any row
    // is materialized (by the replay/discovery below), so replayed and discovered
    // sessions inherit their renamed name from the start.
    {
        let overrides = session_names::load(&shell.data_dir);
        if !overrides.is_empty() {
            let mut store = shell.store.lock().expect("store poisoned");
            store.set_overrides(overrides);
        }
    }

    // Startup replay: apply spooled events WITHOUT firing toasts (avoids a
    // first-launch toast storm for events that occurred while we were down; the
    // rows/tray still reflect them). Live events below do fire toasts.
    match events::drain_spool(&spool_dir, &quarantine_dir, now_ms()) {
        Ok(outcome) => {
            let replayed = outcome.events.len();
            for item in outcome.events {
                {
                    let mut store = shell.store.lock().expect("store poisoned");
                    let _ = store.on_event(&item.event);
                }
                let _ = std::fs::remove_file(&item.path);
            }
            tracing::info!(
                replayed,
                quarantined = outcome.quarantined,
                discarded_old = outcome.discarded_old,
                "spool replay complete"
            );
        }
        Err(err) => tracing::warn!(error = %err, "spool replay failed"),
    }

    // Cold-start discovery of already-running sessions (R-5.4), after replay.
    if let Some(claude_dir) = discovery::claude_dir_from_env() {
        let inserted = {
            let mut store = shell.store.lock().expect("store poisoned");
            discovery::merge_into_store(&mut store, &claude_dir, now_ms())
        };
        tracing::info!(inserted, "cold-start discovery complete");
    }

    // Live-registry cold start (R-15.3): read `~/.claude/sessions/*.json`, add
    // inferred rows for registry sessions whose transcript was missing/stale (so
    // transcript discovery above didn't create them), and refresh names/pids on
    // every already-known row (R-15.2). Registry discovery runs AFTER transcript
    // discovery so it only fills the gaps.
    if let Some(sessions_dir) = registry::sessions_dir_from_env() {
        let entries = registry::read_registry(&sessions_dir);
        let inserted = {
            let mut store = shell.store.lock().expect("store poisoned");
            let n = registry::merge_registry_into_store(&mut store, &entries, now_ms());
            store.apply_registry(&entries);
            // R-22.1 seeding precedence: for a discovered (inferred) row that also
            // has a live registry entry, the registry `updatedAt` outranks the
            // transcript mtime it was first seeded with. Apply it once, here at
            // cold start only (the periodic poll must not keep resetting timers).
            for e in &entries {
                if let Some(updated) = e.updated_at_ms {
                    store.seed_inferred_entered_at(&e.session_id, updated);
                }
            }
            n
        };
        tracing::info!(
            inserted,
            registry = entries.len(),
            "registry cold start complete"
        );
    }

    shell.push_state();

    let watcher = match watcher::SpoolWatcher::spawn(&spool_dir, watcher::DEFAULT_DEBOUNCE) {
        Ok(w) => w,
        Err(err) => {
            tracing::error!(error = %err, "spool watcher failed to start");
            return;
        }
    };

    // Close the replay→watch gap (R-3.5): a hook could drop a spool file in the
    // window between the initial `drain_spool` directory read and the watcher
    // becoming active — `notify` only reports events after `watch()`, and there
    // is no periodic rescan, so such a file would sit unseen until the next
    // launch. Re-drain once now that the watch is live to sweep up anything
    // stranded. Toasts stay suppressed (still "while we were coming up"); the
    // watcher may also report these paths, but `ingest_file` no-ops a file a
    // drain already consumed.
    match events::drain_spool(&spool_dir, &quarantine_dir, now_ms()) {
        Ok(outcome) => {
            let swept = outcome.events.len();
            for item in outcome.events {
                {
                    let mut store = shell.store.lock().expect("store poisoned");
                    let _ = store.on_event(&item.event);
                }
                let _ = std::fs::remove_file(&item.path);
            }
            if swept > 0 {
                tracing::info!(swept, "startup replay→watch gap swept (R-3.5)");
                shell.push_state();
            }
        }
        Err(err) => tracing::warn!(error = %err, "startup gap re-drain failed"),
    }

    let mut last_tick = Instant::now();
    let mut last_sweep = Instant::now();
    let mut last_foreground = Instant::now();
    loop {
        match watcher.paths.recv_timeout(LOOP_SLICE) {
            Ok(path) => {
                // Drain the whole debounce-window burst, then push ONE state
                // snapshot. `push_state()` clones + serializes a full snapshot,
                // emits `deck://state`, and repaints the tray — O(session-count)
                // work that must not run per ingested file, or a replay/flood of
                // N events would fire N full broadcasts in a tight loop (chaos
                // ingestion outpacing the UI). Toasts still fire per event inside
                // `ingest_path` (each status change matters and is throttled);
                // only the UI/tray push is coalesced.
                let mut changed = ingest_path(&shell, &quarantine_dir, &path);
                while let Ok(path) = watcher.paths.try_recv() {
                    changed |= ingest_path(&shell, &quarantine_dir, &path);
                }
                if changed {
                    shell.push_state();
                    // R-32.2: a `SessionEnd` in this burst leaves the agent's asks
                    // /perms dangling — dismiss them now (the raising agent is gone)
                    // rather than waiting for the next 10 s tick.
                    shell.reap_gone_sessions();
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                tracing::warn!("spool watcher channel closed; engine loop exiting");
                break;
            }
        }

        // Safety-net rescan (R-2.5, R-3.5): catch any spool files the OS
        // file-watch dropped (see `SPOOL_SWEEP`). Runs on this same loop thread
        // as the watcher-driven ingest above, so it never races it — whichever
        // reaches a file first ingests and removes it; the other finds it gone
        // (`ingest_file` no-ops a vanished path).
        if last_sweep.elapsed() >= SPOOL_SWEEP {
            last_sweep = Instant::now();
            if sweep_spool(&shell, &spool_dir, &quarantine_dir) {
                shell.push_state();
                // R-32.2: a `SessionEnd` recovered by the safety-net sweep must
                // also dismiss the gone agent's asks/perms.
                shell.reap_gone_sessions();
            }
        }

        if last_tick.elapsed() >= ENGINE_TICK {
            last_tick = Instant::now();
            run_tick(&shell);
        }

        // R-17.1/R-17.2: every 2s, re-surface any queued ask whose session's
        // terminal is no longer the foreground window. Gated on there being a
        // pending ask so the foreground sampler (a powershell spawn on Windows)
        // never runs when there is nothing to un-suppress.
        if last_foreground.elapsed() >= FOREGROUND_POLL {
            last_foreground = Instant::now();
            if !shell.asks_empty() {
                shell.maybe_surface_asks();
            }
        }
    }
}

/// Safety-net rescan of the spool directory: ingest (in receipt-time order,
/// firing toasts) any files the live `notify` watcher missed. Reuses
/// [`events::drain_spool`], so it also enforces the R-3.5 spool cap, discards
/// events older than 24 h, and quarantines malformed files. Normally a no-op
/// (the watcher already consumed and deleted everything, so the directory is
/// empty and this is a single cheap `read_dir`); does real work only when the
/// OS dropped a file-watch event. Returns whether any event was applied (so the
/// caller coalesces one `push_state`). See [`SPOOL_SWEEP`].
fn sweep_spool(shell: &Arc<Shell>, spool_dir: &Path, quarantine_dir: &Path) -> bool {
    let outcome = match events::drain_spool(spool_dir, quarantine_dir, now_ms()) {
        Ok(o) => o,
        Err(err) => {
            tracing::warn!(error = %err, "spool safety-net sweep failed");
            return false;
        }
    };
    if outcome.events.is_empty() {
        return false;
    }
    tracing::warn!(
        recovered = outcome.events.len(),
        "spool safety-net sweep ingested files the live watcher missed (R-2.5/R-3.5)"
    );
    for item in outcome.events {
        let effects = {
            let mut store = shell.store.lock().expect("store poisoned");
            store.on_event(&item.event)
        };
        let _ = std::fs::remove_file(&item.path);
        shell.fire_effects(effects);
    }
    true
}

/// Ingest one spool file: parse, apply to the store, fire its toasts. Returns
/// `true` iff an event was applied (so the caller knows the UI state changed and
/// can coalesce a single `push_state()` after draining a whole burst — see the
/// engine loop). Deliberately does NOT push state itself.
fn ingest_path(shell: &Arc<Shell>, quarantine_dir: &Path, path: &Path) -> bool {
    if path.extension().and_then(|e| e.to_str()) != Some("json") {
        return false;
    }
    match events::ingest_file(path, quarantine_dir, now_ms()) {
        Ok(Some(event)) => {
            let effects = {
                let mut store = shell.store.lock().expect("store poisoned");
                store.on_event(&event)
            };
            let _ = std::fs::remove_file(path);
            shell.fire_effects(effects);
            true
        }
        Ok(None) => false,
        Err(err) => {
            tracing::warn!(error = %err, ?path, "failed to ingest spool file");
            false
        }
    }
}

fn run_tick(shell: &Arc<Shell>) {
    // R-15.2/R-15.3: refresh names + pids from the live registry BEFORE liveness,
    // so a registry-supplied pid feeds the liveness check on the same tick and a
    // /rename shows up within ≤10 s.
    if let Some(sessions_dir) = registry::sessions_dir_from_env() {
        let entries = registry::read_registry(&sessions_dir);
        // Apply UNCONDITIONALLY, even for an empty poll: `apply_registry`'s
        // absent-session branch is what clears a vanished session's stale
        // registry `name` (R-15.2) and busy flag (R-21.1). Guarding it behind
        // `!entries.is_empty()` used to leave the LAST removed registry file's
        // name/busy state wedged on its row forever (nothing else refreshes the
        // name). An empty read is cheap — it just walks the rows and clears.
        let mut store = shell.store.lock().expect("store poisoned");
        // §45/R-45: refresh each row's last-seen transcript mtime BEFORE
        // `apply_registry`, so the §44 registry-demote gates on the CURRENT
        // transcript quiescence — a mid-turn registry `waiting` on an agent whose
        // transcript is still advancing must not flap the row idle→working.
        store.refresh_transcript_activity(transcript_mtime_ms);
        store.apply_registry(&entries);
    }
    let procs = SysProcs::refreshed();
    let effects = {
        let mut store = shell.store.lock().expect("store poisoned");
        store.tick(&procs, transcript_mtime_ms)
    };
    shell.fire_effects(effects);
    shell.sweep_expired_asks();
    // R-32.1: expire perms past their ~90 s deadline (their hook has given up).
    shell.sweep_expired_perms();
    // R-32.2: cancel asks + drop perms for sessions that ended or died this tick
    // (SessionEnd / liveness `dead`) — the raising agent is gone.
    shell.reap_gone_sessions();
    // §23: fold newly-appended transcript bytes into per-session token telemetry
    // (R-23.1), off the UI thread, before the snapshot is pushed below.
    shell.update_usage();
    // §34: refresh the default row title from each transcript's `aiTitle` (the
    // terminal-tab chat name), mtime-gated + tail-read (R-34), before push_state.
    shell.refresh_ai_titles();
    enforce_disk_caps();
    // R-27.6: re-persist session-names.json when an end-of-session prune (or a
    // rename) dirtied the overrides map. No-op when nothing changed.
    shell.persist_session_names();
    shell.push_state();
}

/// Enforce the R-3.5 spool cap and the quarantine cap on the live path (the
/// running app), oldest-first. `drain_spool` only enforces the spool cap once at
/// startup; this keeps both directories bounded for a long-running instance.
fn enforce_disk_caps() {
    match events::enforce_spool_cap(&settings::spool_dir()) {
        Ok(n) if n > 0 => tracing::warn!(removed = n, "spool cap enforced on live path (R-3.5)"),
        Ok(_) => {}
        Err(err) => tracing::warn!(error = %err, "failed to enforce spool cap"),
    }
    match events::enforce_quarantine_cap(&settings::spool_quarantine_dir()) {
        Ok(n) if n > 0 => tracing::warn!(removed = n, "quarantine cap enforced"),
        Ok(_) => {}
        Err(err) => tracing::warn!(error = %err, "failed to enforce quarantine cap"),
    }
    // R-3.5 hygiene: sweep stray non-`.json` leftovers (e.g. a `<id>.json.tmp`
    // from a hook killed mid atomic-write) that no ingest path consumes and the
    // spool cap never counts, so they can't accumulate on disk unbounded.
    match events::sweep_stray_spool_files(
        &settings::spool_dir(),
        &settings::spool_quarantine_dir(),
        now_ms(),
        events::STRAY_FILE_MIN_AGE_MS,
    ) {
        Ok(n) if n > 0 => {
            tracing::warn!(swept = n, "stray non-json spool files quarantined (R-3.5)")
        }
        Ok(_) => {}
        Err(err) => tracing::warn!(error = %err, "failed to sweep stray spool files"),
    }
}

// ---------------------------------------------------------------------------
// Answers watcher: turns `<data>/answers/*.json` (written by `answer_ask`) into
// resolved MCP `ask_user` results (R-8.7).
// ---------------------------------------------------------------------------

fn run_answers(shell: Arc<Shell>) {
    let answers_dir = settings::answers_dir();
    // Startup drain: resolve any answer files already on disk before the watcher
    // is live (written while we were down, or whose create event a prior run's
    // watcher never delivered). Mirrors `run_engine`'s startup `drain_spool`.
    drain_answers(&shell, &answers_dir);
    let watcher = match watcher::SpoolWatcher::spawn(&answers_dir, watcher::DEFAULT_DEBOUNCE) {
        Ok(w) => w,
        Err(err) => {
            tracing::error!(error = %err, "answers watcher failed to start");
            return;
        }
    };
    let mut last_sweep = Instant::now();
    loop {
        match watcher.paths.recv_timeout(LOOP_SLICE) {
            Ok(path) => shell.resolve_answer(&path),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // Safety-net rescan (mirrors the spool `SPOOL_SWEEP` in `run_engine`).
        // `resolve_answer` fires ONLY on a watcher-delivered path, but OS
        // file-watch delivery is not guaranteed — Windows `ReadDirectoryChangesW`
        // can drop the create/modify event for `<data>/answers/<askId>.json`. On
        // this path a dropped event is unrecoverable: the answer file sits on disk
        // forever, the oneshot responder never fires, and the blocked `ask_user`
        // returns `timeout` despite the human having answered (the row already
        // vanished from the popup, so they can't re-answer). A periodic directory
        // scan resolves anything the watcher missed, bounding worst-case delivery
        // latency to one interval instead of "never".
        if last_sweep.elapsed() >= SPOOL_SWEEP {
            last_sweep = Instant::now();
            drain_answers(&shell, &answers_dir);
        }
    }
}

/// Resolve every `*.json` answer file currently in `answers_dir` — a safety net
/// for watch events the OS dropped (see the sweep in [`run_answers`]).
/// [`Shell::resolve_answer`] removes each file it handles (delivering, or
/// quarantining a malformed one, or consuming an already-resolved one), so this
/// is normally a no-op single `read_dir` and races the watcher harmlessly:
/// whichever reaches a file first handles it; the other finds it gone.
fn drain_answers(shell: &Arc<Shell>, answers_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(answers_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            shell.resolve_answer(&path);
        }
    }
}

// ---------------------------------------------------------------------------
// Perms watcher (§16): turns `<data>/perms/*.json` (written by the
// `PermissionRequest` hook) into deck-side perm modals, and reaps stale
// perm-answer files whose hook already gave up.
// ---------------------------------------------------------------------------

/// A perm-answer file older than this is stale: the hook that would poll it
/// (timeout 90 s, R-16.1) has long exited, so nothing will consume it. Reaped
/// on the perms-watcher sweep so `<data>/perm-answers/` can't grow unbounded.
const PERM_ANSWER_STALE_MS: u64 = 180_000;

fn run_perms(shell: Arc<Shell>) {
    let perms_dir = settings::perms_dir();
    let quarantine_dir = settings::spool_quarantine_dir();
    // Startup drain: ingest any perm files written while we were down (the hook
    // is likely still polling; a fresh decision will unblock it).
    drain_perms(&shell, &perms_dir, &quarantine_dir);
    let watcher = match watcher::SpoolWatcher::spawn(&perms_dir, watcher::DEFAULT_DEBOUNCE) {
        Ok(w) => w,
        Err(err) => {
            tracing::error!(error = %err, "perms watcher failed to start");
            return;
        }
    };
    let mut last_sweep = Instant::now();
    loop {
        match watcher.paths.recv_timeout(LOOP_SLICE) {
            Ok(path) => shell.ingest_perm_path(&quarantine_dir, &path),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        // Safety-net rescan for watch events the OS dropped (mirrors the spool /
        // answers sweeps), plus stale perm-answer reaping.
        if last_sweep.elapsed() >= SPOOL_SWEEP {
            last_sweep = Instant::now();
            drain_perms(&shell, &perms_dir, &quarantine_dir);
            sweep_stale_perm_answers(&settings::perm_answers_dir());
        }
    }
}

/// Ingest every `*.json` perm file currently in `perms_dir` — a safety net for
/// watch events the OS dropped. [`Shell::ingest_perm_path`] consumes each file
/// it handles, so this is normally a no-op single `read_dir`.
fn drain_perms(shell: &Arc<Shell>, perms_dir: &Path, quarantine_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(perms_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            shell.ingest_perm_path(quarantine_dir, &path);
        }
    }
}

/// Remove perm-answer files older than [`PERM_ANSWER_STALE_MS`]: the hook that
/// would poll them has exited, so they are dead weight. Best-effort.
fn sweep_stale_perm_answers(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let stale = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|mtime| now.duration_since(mtime).ok())
            .is_some_and(|age| age.as_millis() as u64 >= PERM_ANSWER_STALE_MS);
        if stale {
            let _ = std::fs::remove_file(&path);
        }
    }
}

// ---------------------------------------------------------------------------
// Hook installer wiring (§4, R-4.4), pointed at an isolated settings.json via
// QUARTERDECK_CLAUDE_DIR for testing.
// ---------------------------------------------------------------------------

fn has_hook_scripts(dir: &Path) -> bool {
    dir.join("quarterdeck-hook.ps1").exists() || dir.join("quarterdeck-hook.sh").exists()
}

/// Locate the bundled hook scripts: `QUARTERDECK_HOOKS_SRC` override, then the
/// packaged resource dir, then (dev) an ancestor of the executable.
fn resolve_hooks_src(app: &AppHandle<Wry>) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("QUARTERDECK_HOOKS_SRC") {
        let p = PathBuf::from(&dir);
        if !dir.is_empty() && has_hook_scripts(&p) {
            return Some(p);
        }
    }
    if let Ok(res) = app.path().resource_dir() {
        let p = res.join("hooks");
        if has_hook_scripts(&p) {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors() {
            let p = ancestor.join("hooks");
            if has_hook_scripts(&p) {
                return Some(p);
            }
        }
    }
    None
}

/// Copy the hook scripts to `hooks_dst` (a stable path, R-4.4) and merge our
/// entries into `settings_path`. When `install_perm` is set (the
/// `takeoverPermissions` setting is on, R-16.4) the opt-in `PermissionRequest`
/// entry is added too. Pure (no `AppHandle`) so it is unit-testable.
fn install_hooks_to(
    settings_path: &Path,
    hooks_src: &Path,
    hooks_dst: &Path,
    install_perm: bool,
) -> Result<hooks_config::HooksChange, String> {
    std::fs::create_dir_all(hooks_dst).map_err(|e| e.to_string())?;
    for name in ["quarterdeck-hook.ps1", "quarterdeck-hook.sh"] {
        let src = hooks_src.join(name);
        if !src.exists() {
            continue;
        }
        let dst = hooks_dst.join(name);
        std::fs::copy(&src, &dst).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        if name.ends_with(".sh") {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&dst) {
                let mut perms = meta.permissions();
                perms.set_mode(0o755);
                let _ = std::fs::set_permissions(&dst, perms);
            }
        }
    }
    let script = hooks_dst.join(if cfg!(windows) {
        "quarterdeck-hook.ps1"
    } else {
        "quarterdeck-hook.sh"
    });
    let command = hooks_config::command_line(&script);
    let change = hooks_config::install_hooks(settings_path, &command).map_err(|e| e.to_string())?;
    // R-16.4: the opt-in PermissionRequest entry, only when the setting is on.
    if install_perm {
        let perm_change =
            hooks_config::install_perm_hook(settings_path, &command).map_err(|e| e.to_string())?;
        if perm_change.changed {
            return Ok(hooks_config::HooksChange {
                changed: true,
                backup: change.backup.or(perm_change.backup),
                events_added: change
                    .events_added
                    .into_iter()
                    .chain(perm_change.events_added)
                    .collect(),
                entries_removed: 0,
            });
        }
    }
    Ok(change)
}

fn perform_install_hooks(app: &AppHandle<Wry>) -> Result<(), String> {
    let hooks_src = resolve_hooks_src(app)
        .ok_or_else(|| "could not locate bundled hook scripts".to_string())?;
    let takeover = settings::load(&settings::data_dir()).takeover_permissions;
    let change = install_hooks_to(
        &claude_settings_path(),
        &hooks_src,
        &settings::hooks_dir(),
        takeover,
    )?;
    tracing::info!(
        changed = change.changed,
        events = ?change.events_added,
        backup = ?change.backup,
        takeover,
        "hook install complete"
    );
    Ok(())
}

/// R-16.4: toggle just the `PermissionRequest` hook entry when the
/// `takeoverPermissions` setting changes. Enabling (re)installs it (copying the
/// scripts first, so a perm hook can be turned on without a full re-install);
/// disabling removes only that entry, leaving the five lifecycle hooks intact.
/// A no-op unless the lifecycle hooks are already installed (nothing to gate).
fn sync_takeover_permissions(app: &AppHandle<Wry>, enable: bool) {
    let settings_path = claude_settings_path();
    let installed = std::fs::read_to_string(&settings_path)
        .map(|t| t.contains(hooks_config::MARKER))
        .unwrap_or(false);
    if !installed {
        tracing::debug!("takeoverPermissions toggled but hooks not installed; nothing to sync");
        return;
    }
    if enable {
        let Some(hooks_src) = resolve_hooks_src(app) else {
            tracing::warn!("could not locate hook scripts to enable permission takeover");
            return;
        };
        match install_hooks_to(&settings_path, &hooks_src, &settings::hooks_dir(), true) {
            Ok(change) => tracing::info!(changed = change.changed, "permission takeover enabled"),
            Err(err) => tracing::warn!(error = %err, "failed to enable permission takeover"),
        }
    } else {
        match hooks_config::uninstall_perm_hook(&settings_path, hooks_config::MARKER) {
            Ok(change) => tracing::info!(
                removed = change.entries_removed,
                "permission takeover disabled"
            ),
            Err(err) => tracing::warn!(error = %err, "failed to disable permission takeover"),
        }
    }
}

fn perform_uninstall_hooks() -> Result<(), String> {
    let change = hooks_config::uninstall_hooks(&claude_settings_path(), hooks_config::MARKER)
        .map_err(|e| e.to_string())?;
    // R-24.2: "also remove toast registration" — the AUMID identity key is
    // reversed alongside the hooks.
    notify::unregister_toast_identity();
    tracing::info!(
        changed = change.changed,
        removed = change.entries_removed,
        "hook uninstall complete"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Setting side effects: autostart (R-10.3) + agent-questions MCP setup (R-8.6).
// ---------------------------------------------------------------------------

/// R-24.2: register the Windows toast identity (AUMID DisplayName + icon) in
/// HKCU at startup, idempotently. Skipped under `QUARTERDECK_DATA_DIR` test
/// isolation (like autostart) unless an explicit AUMID base override is set, so
/// the e2e/QA harness never mutates the real HKCU toast-identity key.
fn register_toast_identity_at_startup() {
    let isolated = std::env::var("QUARTERDECK_DATA_DIR")
        .map(|d| !d.is_empty())
        .unwrap_or(false);
    let base_override = std::env::var(notify::AUMID_BASE_ENV)
        .map(|d| !d.is_empty())
        .unwrap_or(false);
    if isolated && !base_override {
        tracing::debug!("skipping toast identity registration under data-dir isolation (R-24.2)");
        return;
    }
    notify::register_toast_identity();
}

fn sync_autostart(app: &AppHandle<Wry>, enable: bool) {
    // Test-isolation guard (R-3.3). `autolaunch().enable()/.disable()` registers
    // or unregisters the app in the REAL machine's login items (Windows registry
    // Run key / macOS LaunchAgent) using the current exe path — a machine-wide
    // side effect that no data-dir override contains. `QUARTERDECK_DATA_DIR` is
    // the documented test-isolation surface (all QA/E2E launches set it); when it
    // is present we must not mutate the real login-item config. The
    // `launchAtLogin` setting is still persisted and surfaced to the UI — only the
    // real-OS registration is skipped, so a toggle stays observable in the
    // isolated settings.json without touching the host machine.
    let isolated = std::env::var("QUARTERDECK_DATA_DIR")
        .map(|dir| !dir.is_empty())
        .unwrap_or(false);
    if isolated {
        tracing::debug!(
            enable,
            "skipping real autostart sync under QUARTERDECK_DATA_DIR isolation (R-3.3)"
        );
        return;
    }
    let manager = app.autolaunch();
    let current = manager.is_enabled().unwrap_or(false);
    let result = if enable && !current {
        manager.enable()
    } else if !enable && current {
        manager.disable()
    } else {
        Ok(())
    };
    if let Err(err) = result {
        tracing::warn!(error = %err, enable, "failed to sync autostart");
    }
}

fn resolve_skill_src(app: &AppHandle<Wry>) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("QUARTERDECK_SKILL_SRC") {
        let p = PathBuf::from(&dir);
        if !dir.is_empty() && p.join("SKILL.md").exists() {
            return Some(p);
        }
    }
    if let Ok(res) = app.path().resource_dir() {
        let p = res.join("skills").join("quarterdeck");
        if p.join("SKILL.md").exists() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors() {
            let p = ancestor.join("skills").join("quarterdeck");
            if p.join("SKILL.md").exists() {
                return Some(p);
            }
        }
    }
    None
}

fn copy_skill(app: &AppHandle<Wry>) {
    let (Some(src), Some(claude)) = (resolve_skill_src(app), discovery::claude_dir_from_env())
    else {
        return;
    };
    let dst = claude.join("skills").join("quarterdeck");
    if std::fs::create_dir_all(&dst)
        .and_then(|()| std::fs::copy(src.join("SKILL.md"), dst.join("SKILL.md")).map(|_| ()))
        .is_err()
    {
        tracing::warn!("failed to copy bundled skill");
    }
}

/// R-8.6: register/unregister the MCP server with the Claude CLI and copy the
/// bundled skill. When the `claude` CLI isn't on PATH (or the add fails), the
/// settings pane surfaces the exact `claude mcp add …` command to run manually
/// (the snapshot's `mcpCommand` + `mcpCliAvailable`, rendered by the UI).
/// Idempotent; "Disable" reverses BOTH the registration and the skill copy.
fn setup_agent_questions(app: &AppHandle<Wry>, enable: bool) {
    if enable {
        if let Some(cfg) = mcp_server::load_config() {
            let url = format!("http://127.0.0.1:{}{}", cfg.port, mcp_server::MCP_PATH);
            let header = format!("Authorization: Bearer {}", cfg.token);
            let result = std::process::Command::new("claude")
                .args([
                    "mcp",
                    "add",
                    "--transport",
                    "http",
                    "--scope",
                    "user",
                    "quarterdeck",
                    &url,
                    "--header",
                    &header,
                ])
                .no_console_window()
                .output();
            match result {
                Ok(out) if out.status.success() => tracing::info!("registered MCP with claude CLI"),
                Ok(_) | Err(_) => {
                    tracing::warn!(
                        "`claude mcp add` unavailable; the settings pane shows the command to run manually (R-8.6)"
                    )
                }
            }
        }
        copy_skill(app);
    } else {
        let _ = std::process::Command::new("claude")
            .args(["mcp", "remove", "--scope", "user", "quarterdeck"])
            .no_console_window()
            .output();
        // R-8.6 "Disable reverses both": also remove the copied skill.
        remove_skill();
    }
}

/// Delete the bundled skill copied by [`copy_skill`] (R-8.6 "Disable reverses
/// both"). No-op when it was never installed.
fn remove_skill() {
    let Some(claude) = discovery::claude_dir_from_env() else {
        return;
    };
    let dst = claude.join("skills").join("quarterdeck");
    if dst.exists() {
        if let Err(err) = std::fs::remove_dir_all(&dst) {
            tracing::warn!(error = %err, "failed to remove bundled skill on disable");
        }
    }
}

/// Apply the side effect for a persisted setting change (called by `set_setting`).
pub fn apply_setting_side_effect(app: &AppHandle<Wry>, key: &str, settings: &Settings) {
    match key {
        "launchAtLogin" => sync_autostart(app, settings.launch_at_login),
        "popupPinned" => {
            if let Err(err) = windows::set_popup_pinned(app, settings.popup_pinned) {
                tracing::warn!(error = %err, "failed to sync popup pin state (R-14.2)");
            }
            // R-25.2 "Unpin while in lamp mode → expand to list + revert to
            // v1.0 tray-anchored behavior": the collapse button that reaches
            // lamp mode only shows while pinned, so this is the path back out
            // for a user who unpins (e.g. via the lamp's right-click menu)
            // without expanding first.
            if windows::should_force_list_on_unpin(settings.popup_pinned, settings.popup_mode) {
                match crate::settings::set_setting(
                    &crate::settings::data_dir(),
                    "popupMode",
                    crate::ipc::SettingValue::Text(PopupMode::List.as_str().to_string()),
                ) {
                    Ok(updated) => {
                        if let Err(err) = windows::set_popup_mode(app, updated.popup_mode) {
                            tracing::warn!(error = %err, "failed to expand popup on unpin (R-25.2)");
                        }
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "failed to force list mode on unpin (R-25.2)")
                    }
                }
            }
        }
        "popupMode" => {
            if let Err(err) = windows::set_popup_mode(app, settings.popup_mode) {
                tracing::warn!(error = %err, "failed to sync popup mode (R-25.2)");
            }
        }
        "mcpEnabled" => {
            let enabled = settings
                .extra
                .get("mcpEnabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            setup_agent_questions(app, enabled);
        }
        "takeoverPermissions" => sync_takeover_permissions(app, settings.takeover_permissions),
        "showTokenStats" => {
            // R-23.5: reflect the toggle at once — turning it ON populates the
            // usage map before the next tick; turning it OFF clears it so the
            // row usage lines disappear immediately (R-23.6).
            if let Some(shell) = app.try_state::<Arc<Shell>>() {
                shell.update_usage();
            }
        }
        _ => {}
    }
}

/// Install the hooks (copy scripts + merge settings.json), invoked by the
/// `install_hooks` command (T7 seam).
pub fn install_hooks_command(app: &AppHandle<Wry>) -> Result<(), String> {
    perform_install_hooks(app)
}

/// Uninstall the hooks, invoked by the `uninstall_hooks` command (T7 seam).
pub fn uninstall_hooks_command() -> Result<(), String> {
    perform_uninstall_hooks()
}

/// Rename a session (§27 R-27.4), invoked by the `rename_session` command (the
/// popup's double-click inline editor). An empty/whitespace name clears the
/// override.
pub fn rename_session_command(
    app: &AppHandle<Wry>,
    session_id: &str,
    name: &str,
) -> Result<(), String> {
    let shell = app
        .try_state::<Arc<Shell>>()
        .ok_or_else(|| "Quarterdeck is still starting up".to_string())?;
    shell.rename_session(session_id, name);
    Ok(())
}

/// Drop a session's user title override (§27 R-27.6), invoked when its row is
/// removed (`remove_row`): a reused id must never inherit a stale name. A no-op
/// (best-effort) before the shell is fully up.
pub fn prune_session_override(app: &AppHandle<Wry>, session_id: &str) {
    if let Some(shell) = app.try_state::<Arc<Shell>>() {
        {
            let mut store = shell.store.lock().expect("store poisoned");
            store.set_override_name(session_id, None);
        }
        shell.persist_session_names();
    }
}

/// Answer a pending permission request (`answer_perm` command, SPEC §16, T7
/// seam): persists the decision for the blocked hook and updates deck state.
pub fn answer_perm_command(
    app: &AppHandle<Wry>,
    perm_id: &str,
    decision: PermDecision,
    reason: Option<&str>,
) -> Result<(), String> {
    let shell = app
        .try_state::<Arc<Shell>>()
        .ok_or_else(|| "Quarterdeck is still starting up".to_string())?;
    shell.answer_perm(perm_id, decision, reason)
}

/// Rebuild + push a state snapshot from the managed [`Shell`] (used by commands
/// after they mutate persisted state, so settings/onboarding propagate at once).
pub fn push_state(app: &AppHandle<Wry>) {
    if let Some(shell) = app.try_state::<Arc<Shell>>() {
        shell.push_state();
    }
}

// ---------------------------------------------------------------------------
// Managed handle keeping the MCP server + its runtime alive for the app lifetime.
// ---------------------------------------------------------------------------

/// Kept in managed state purely so its `Drop` (and thus the MCP server + its
/// tokio runtime) lives for the whole app lifetime.
#[allow(dead_code)]
struct McpRuntime {
    runtime: tokio::runtime::Runtime,
    server: mcp_server::ServerHandle,
}

// ---------------------------------------------------------------------------
// Composition root.
// ---------------------------------------------------------------------------

/// Builds and runs the Quarterdeck application.
pub fn run() {
    let data_dir = settings::data_dir();
    // Keep the logging guard alive for the whole `run()` (i.e. the app lifetime).
    let _log_guard = logging::init(&data_dir);
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        ?data_dir,
        "Quarterdeck starting"
    );

    // R-10.4 / live-smoke step 5: the log is THE diagnostic surface. A panic
    // during startup — a plugin/`setup` `expect`, `generate_context!`, a
    // background worker thread — would otherwise leave only "Quarterdeck
    // starting" in the log and vanish with just a stderr backtrace the user (and
    // the smoke reader) never see. Route panics through the tracing subscriber so
    // the failure is always recorded; chain the default hook so stderr still gets
    // the backtrace. On the (default) unwind strategy the async log writer is
    // flushed when `_log_guard` drops as the panic unwinds out of `run()`.
    {
        let default_panic_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            tracing::error!(panic = %info, "Quarterdeck panicked");
            default_panic_hook(info);
        }));
    }

    let result = tauri::Builder::default()
        // R-3.3 / critical: a single-instance guard. A second launch would
        // otherwise collide on the shared per-identifier WebView2 profile
        // (`%LOCALAPPDATA%\pro.philippgross.quarterdeck\EBWebView`, one browser
        // process per profile, unaffected by QUARTERDECK_DATA_DIR): both windows
        // fail to create and the app runs on as a UI-less zombie with no visible
        // error. This plugin makes the second instance hand off to the first
        // (surfacing the popup) and exit, so the collision can't happen. It MUST
        // be registered first.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            tracing::info!("second instance launched; surfacing the existing window");
            run_on_main(app, |app| {
                if let Err(err) = windows::open_popup(app) {
                    tracing::warn!(error = %err, "failed to surface popup for second instance");
                }
            });
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            ipc::get_state,
            ipc::remove_row,
            ipc::kill_session,
            ipc::rename_session,
            ipc::answer_ask,
            ipc::answer_perm,
            ipc::set_setting,
            ipc::install_hooks,
            ipc::uninstall_hooks,
            ipc::resize_popup,
            ipc::resize_ask,
            ipc::show_ask_window,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();

            // macOS: keep Quarterdeck out of the Dock and the Cmd+Tab app
            // switcher (R-7.1 "absent from taskbar/Dock/alt-tab"). `skipTaskbar`
            // alone doesn't remove the Dock icon — the activation policy does.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            // Tray + windows (R-2.6, R-7.1, R-8.3).
            let tray = tray::build(&handle)?;
            if let Err(err) = windows::setup_popup_behavior(&handle) {
                tracing::warn!(error = %err, "failed to set up popup behavior");
            }

            // The shared shell.
            let shell = Arc::new(Shell {
                app: handle.clone(),
                store: Mutex::new(SessionStore::with_system_clock()),
                asks: Mutex::new(AskStore::with_system_clock()),
                perms: Mutex::new(Vec::new()),
                usage: Mutex::new(HashMap::new()),
                ai_title_mtime: Mutex::new(HashMap::new()),
                notifier: DesktopNotifier::new(handle.clone()),
                tray,
                data_dir: data_dir.clone(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                foreground_cache: Mutex::new(None),
            });
            app.manage(shell.clone());

            // R-9.6: a clicked toast opens the popup (or the ask window for ask
            // toasts). The click source is wired in `notify.rs` (Windows WinRT
            // `on_activated`); this handler does the routing for every platform.
            {
                let click_handle = handle.clone();
                handle.listen(notify::TOAST_CLICKED_EVENT, move |event| {
                    let is_ask = serde_json::from_str::<notify::ToastClickPayload>(event.payload())
                        .map(|p| {
                            tracing::debug!(kind = ?p.kind, session_id = %p.session_id, "toast clicked (R-9.6)");
                            p.kind == ToastKind::Ask
                        })
                        .unwrap_or(false);
                    run_on_main(&click_handle, move |app| {
                        let result = if is_ask {
                            windows::show_ask_window(app)
                        } else {
                            windows::open_popup(app)
                        };
                        if let Err(err) = result {
                            tracing::warn!(error = %err, "failed to open window from toast click");
                        }
                    });
                });
            }

            // Reflect the persisted autostart preference (no change without prior
            // consent: the default is off, so nothing is enabled here). R-10.2/10.3.
            let startup_settings = settings::load(&data_dir);
            sync_autostart(&handle, startup_settings.launch_at_login);

            // R-14.2/R-25.2: the native pin/mode state (`windows.rs`'s own
            // mirrored statics, defaulted at process start) must be brought in
            // line with what was persisted last run — otherwise a relaunch would
            // show the header's pin icon as pinned (the UI reads `settings.json`
            // fresh on every snapshot) while the ACTUAL window still hides on
            // blur, a real functional mismatch, not just cosmetic.
            if let Err(err) = windows::set_popup_pinned(&handle, startup_settings.popup_pinned) {
                tracing::warn!(error = %err, "failed to apply persisted pin state at startup (R-14.2)");
            }
            if let Err(err) = windows::set_popup_mode(&handle, startup_settings.popup_mode) {
                tracing::warn!(error = %err, "failed to apply persisted popup mode at startup (R-25.2)");
            }
            // §48: restore the position for the PERSISTED mode — a pinned app
            // that last quit collapsed reopens the lamp where it was, one that
            // quit expanded reopens the list where it was. The two positions are
            // independent (`lampPos`/`popupPos`) so neither clobbers the other.
            if startup_settings.popup_pinned {
                if let Some(pos) = windows::saved_pos_for_mode(
                    startup_settings.popup_mode,
                    startup_settings.popup_pos,
                    startup_settings.lamp_pos,
                ) {
                    windows::restore_popup_position(&handle, pos);
                }
            }

            // R-24.2: register the toast identity so toasts read "Quarterdeck"
            // (+ icon), not "Windows PowerShell", in dev AND packaged runs.
            register_toast_identity_at_startup();

            // MCP server on its own tokio runtime (kept alive via managed state).
            match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => {
                    let gateway: Arc<dyn AskGateway> = Arc::new(EngineGateway {
                        shell: shell.clone(),
                    });
                    match runtime.block_on(mcp_server::serve(gateway)) {
                        Ok(server) => {
                            tracing::info!(port = server.port(), "MCP server ready");
                            app.manage(McpRuntime { runtime, server });
                        }
                        Err(err) => tracing::error!(error = %err, "MCP server failed to start"),
                    }
                }
                Err(err) => tracing::error!(error = %err, "failed to build MCP runtime"),
            }

            // Background workers.
            let engine_shell = shell.clone();
            thread::Builder::new()
                .name("quarterdeck-engine".to_string())
                .spawn(move || run_engine(engine_shell))
                .expect("failed to spawn engine thread");
            let answers_shell = shell.clone();
            thread::Builder::new()
                .name("quarterdeck-answers".to_string())
                .spawn(move || run_answers(answers_shell))
                .expect("failed to spawn answers thread");
            let perms_shell = shell.clone();
            thread::Builder::new()
                .name("quarterdeck-perms".to_string())
                .spawn(move || run_perms(perms_shell))
                .expect("failed to spawn perms thread");

            Ok(())
        })
        .run(tauri::generate_context!());

    // R-10.4 / live-smoke step 5: the log is the diagnostic surface. A failed
    // Tauri/WebView2 startup (e.g. a locked/corrupt shared WebView2 profile)
    // must not exit with only "Quarterdeck starting" in the log — route the
    // error through the subscriber, flush it, then exit non-zero.
    if let Err(err) = result {
        tracing::error!(error = %err, "Quarterdeck exited with a fatal error");
        drop(_log_guard); // flush the async log writer before we exit
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "quarterdeck-t7-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn sysprocs_names_only_refresh_still_resolves_the_current_process_name() {
        // Regression guard for the R-6.1 liveness hang fix: `SysProcs::refreshed`
        // now refreshes with `ProcessRefreshKind::nothing()` (names/existence
        // only) to avoid the per-process handle queries that hang under a burst
        // of transient hook interpreters. Liveness still needs `process_name`, so
        // assert the name is populated for our own live PID.
        let procs = SysProcs::refreshed();
        let name = procs.process_name(std::process::id());
        assert!(
            name.as_deref().is_some_and(|n| !n.is_empty()),
            "names-only refresh must still resolve a live process's name, got {name:?}"
        );
    }

    #[test]
    fn read_transcript_tail_reads_only_the_last_bytes_and_feeds_ai_title() {
        // §34: a large transcript is tail-read (last N bytes), and the LAST
        // aiTitle in that tail is what `extract_ai_title` resolves. Also proves
        // the tail can begin mid-line without corrupting the extracted value.
        let dir = unique_tmp("aititle-tail");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        // A big head padding (so a small tail budget skips it), an EARLY aiTitle
        // that must be excluded by the tail window, then a recent one to keep.
        let mut body = String::new();
        body.push_str("{\"aiTitle\":\"early - should be skipped by the tail\"}\n");
        body.push_str(&"{\"pad\":\"x\"}\n".repeat(4000)); // ~48 KB of padding
        body.push_str("{\"type\":\"summary\",\"aiTitle\":\"Тестирование Dreambook\"}\n");
        std::fs::write(&path, &body).unwrap();

        let path_str = path.to_str().unwrap();
        // Tail budget smaller than the file → the early aiTitle is out of window.
        let tail = read_transcript_tail(path_str, 8 * 1024).unwrap();
        assert!((tail.len() as u64) <= 8 * 1024);
        assert_eq!(
            extract_ai_title(&tail).as_deref(),
            Some("Тестирование Dreambook")
        );
        assert!(
            !tail.windows(5).any(|w| w == b"early"),
            "the early aiTitle must fall outside the tail window"
        );

        // A tail budget larger than the file reads the whole thing; the LAST
        // aiTitle still wins over the earlier one.
        let whole = read_transcript_tail(path_str, 1024 * 1024).unwrap();
        assert_eq!(whole.len(), body.len());
        assert_eq!(
            extract_ai_title(&whole).as_deref(),
            Some("Тестирование Dreambook")
        );

        // Missing file → None, never panics (failure-tolerant, R-34).
        assert!(read_transcript_tail("/no/such/transcript.jsonl", 4096).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn basename_and_paths_eq_handle_mixed_separators() {
        assert_eq!(basename("C:/Users/phil/repo"), "repo");
        assert_eq!(basename("C:\\Users\\phil\\repo\\"), "repo");
        assert_eq!(basename("/home/phil/proj/"), "proj");
        assert!(paths_eq("C:/Users/phil/repo", "C:\\Users\\phil\\repo\\"));
        assert!(!paths_eq("C:/a/b", "C:/a/c"));
    }

    #[test]
    fn map_status_covers_every_variant() {
        assert_eq!(map_status(EngineStatus::Working), SessionStatus::Working);
        assert_eq!(
            map_status(EngineStatus::WaitingWorkflow),
            SessionStatus::WaitingWorkflow
        );
        assert_eq!(
            map_status(EngineStatus::Attention),
            SessionStatus::Attention
        );
        assert_eq!(map_status(EngineStatus::Idle), SessionStatus::Idle);
        assert_eq!(map_status(EngineStatus::Dead), SessionStatus::Dead);
    }

    #[test]
    fn foreground_suppression_composition_matches_a_session_terminal_pid() {
        // R-17.2 composition: the shell decides "is this session's terminal the
        // foreground window?" by intersecting the engine's per-session terminal
        // pids (the registry/claude pid, R-17.2) with the sampled
        // foreground chain — the exact matching `session_foreground` /
        // `fire_effects` / `maybe_surface_asks` gate on. The lower-level pieces
        // (the `QUARTERDECK_FAKE_FOREGROUND` parse, the pid intersection) are
        // unit-tested in `foreground.rs`; this asserts they compose correctly on
        // top of a REAL engine terminal-pid set, driving both outcomes:
        //   (a) terminal IS foreground → suppress (toast withheld / ask stays
        //       queued), (b) terminal is NOT foreground → surface.
        let mut store = SessionStore::with_system_clock();
        // A registry-discovered session gets its host pid from the registry —
        // this is what liveness + R-17.2 matching read (R-15.3).
        let entry = registry::RegistryEntry {
            session_id: "s-fg".to_string(),
            pid: Some(4242),
            status: Some("busy".to_string()),
            ..Default::default()
        };
        registry::merge_registry_into_store(&mut store, std::slice::from_ref(&entry), now_ms());

        let terminal: Vec<u32> = store
            .terminal_pids()
            .into_iter()
            .find(|(id, _)| id == "s-fg")
            .map(|(_, pids)| pids)
            .expect("the discovered session exposes its registry pid as a terminal pid");
        assert!(terminal.contains(&4242));

        // (a) The session's terminal pid is in the foreground chain → suppress.
        assert!(
            crate::foreground::session_is_foreground(&terminal, &[999, 4242, 7]),
            "a session whose terminal pid is foreground must match (suppress toast, keep ask queued)"
        );
        // (b) An unrelated foreground window → do not suppress (surface).
        assert!(
            !crate::foreground::session_is_foreground(&terminal, &[999, 7]),
            "a foreground window that isn't the session's terminal must not suppress"
        );
        // A session with no known pid has no terminal to defer to → never matches.
        assert!(!crate::foreground::session_is_foreground(&[], &[4242]));
    }

    #[test]
    fn install_hooks_to_writes_isolated_settings_json() {
        let tmp = unique_tmp("install");
        let settings_path = tmp.join("claude").join("settings.json");
        let hooks_dst = tmp.join("data").join("hooks");
        let hooks_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../hooks");

        let change = install_hooks_to(&settings_path, &hooks_src, &hooks_dst, true).unwrap();
        assert!(change.changed, "first install writes");
        assert_eq!(
            change.events_added.len(),
            hooks_config::HOOK_EVENTS.len() + 1,
            "every lifecycle hook event + the opt-in PermissionRequest entry added"
        );
        assert!(
            change
                .events_added
                .contains(&"PermissionRequest".to_string()),
            "PermissionRequest installed when takeover is on"
        );

        let text = std::fs::read_to_string(&settings_path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let hooks = &value["hooks"];
        for event in hooks_config::HOOK_EVENTS {
            let arr = hooks[event].as_array().unwrap();
            let entry = &arr[0]["hooks"][0];
            assert!(
                entry["command"].as_str().unwrap().contains("quarterdeck"),
                "{event} command carries the marker"
            );
            assert_eq!(entry["timeout"], 10, "{event} timeout is 10s");
        }
        assert_eq!(
            hooks["Notification"][0]["matcher"],
            "permission_prompt|idle_prompt|elicitation_dialog"
        );

        // R-16.1: the PermissionRequest entry carries the marker + a 90s timeout.
        let perm = &hooks["PermissionRequest"].as_array().unwrap()[0]["hooks"][0];
        assert!(perm["command"].as_str().unwrap().contains("quarterdeck"));
        assert_eq!(perm["timeout"], 90, "PermissionRequest timeout is 90s");

        // Scripts copied to the stable path (R-4.4).
        assert!(
            hooks_dst.join("quarterdeck-hook.ps1").exists()
                || hooks_dst.join("quarterdeck-hook.sh").exists()
        );

        // Idempotent re-install (R-4.1).
        let again = install_hooks_to(&settings_path, &hooks_src, &hooks_dst, true).unwrap();
        assert!(!again.changed, "re-install is a no-op");

        // R-16.4: toggling takeover off removes ONLY the PermissionRequest entry.
        let perm_off =
            hooks_config::uninstall_perm_hook(&settings_path, hooks_config::MARKER).unwrap();
        assert!(perm_off.changed);
        assert_eq!(perm_off.entries_removed, 1);
        let mid: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(
            mid["hooks"].get("PermissionRequest").is_none(),
            "perm entry gone after takeover-off"
        );
        assert!(
            mid["hooks"].get("Stop").is_some(),
            "lifecycle hooks survive a takeover-off toggle"
        );

        // Uninstall restores a hook-free config (R-4.2).
        let removed = hooks_config::uninstall_hooks(&settings_path, hooks_config::MARKER).unwrap();
        assert!(removed.changed);
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(after.get("hooks").is_none(), "hooks pruned after uninstall");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn install_hooks_to_omits_perm_entry_when_takeover_off() {
        // R-16.4: with takeover off, the installer adds the lifecycle hooks
        // only — no PermissionRequest entry.
        let tmp = unique_tmp("install-noperm");
        let settings_path = tmp.join("claude").join("settings.json");
        let hooks_dst = tmp.join("data").join("hooks");
        let hooks_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../hooks");

        let change = install_hooks_to(&settings_path, &hooks_src, &hooks_dst, false).unwrap();
        assert_eq!(
            change.events_added.len(),
            hooks_config::HOOK_EVENTS.len(),
            "no perm entry when takeover off"
        );
        assert!(!change
            .events_added
            .contains(&"PermissionRequest".to_string()));
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(value["hooks"].get("PermissionRequest").is_none());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn perm_file_parses_hook_written_shape() {
        // Matches the perm envelope the hook writes (R-16.1): snake_case fields,
        // tool_input as a compact JSON string, unknown fields (v/kind/receivedAt)
        // ignored.
        let json = r#"{"v":1,"kind":"perm","tool_name":"Bash","tool_input":"{\"command\":\"rm -rf x\"}","session_id":"sess-1","cwd":"C:/proj","receivedAt":"2026-07-03T00:00:00.000Z"}"#;
        let rec: PermFileRecord = serde_json::from_str(json).unwrap();
        assert_eq!(rec.tool_name, "Bash");
        assert_eq!(rec.tool_input, "{\"command\":\"rm -rf x\"}");
        assert_eq!(rec.session_id.as_deref(), Some("sess-1"));
        assert_eq!(rec.cwd.as_deref(), Some("C:/proj"));

        // Missing optional fields tolerated (defensive parse, R-16.1).
        let sparse: PermFileRecord = serde_json::from_str(r#"{"tool_name":"Read"}"#).unwrap();
        assert_eq!(sparse.tool_name, "Read");
        assert!(sparse.tool_input.is_empty());
        assert!(sparse.session_id.is_none());
    }

    #[test]
    fn pretty_tool_input_indents_parseable_json() {
        // R-16.2: a compact tool_input (as the jq fallback or a legacy hook writes
        // it) is re-emitted as indented JSON for the modal body.
        let out = pretty_tool_input(r#"{"command":"rm -rf x","timeout":120000}"#);
        assert!(out.contains('\n'), "pretty output is multi-line: {out:?}");
        assert!(
            out.contains("  \"command\": \"rm -rf x\""),
            "indented keys: {out:?}"
        );
        // Round-trips to the same value (formatting-only change).
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["command"], "rm -rf x");
        assert_eq!(v["timeout"], 120000);
    }

    #[test]
    fn pretty_tool_input_keeps_truncated_fragment_verbatim() {
        // A tool_input truncated past the hook's 2KB cap no longer parses; we must
        // NOT invent structure from the fragment — keep it verbatim (already
        // indented at the source by the ps1/python hooks).
        let fragment = "{\n  \"content\": \"aaaaaaaaaa"; // unterminated
        assert_eq!(pretty_tool_input(fragment), fragment);
        // Empty / whitespace-only input collapses to empty.
        assert_eq!(pretty_tool_input("   "), "");
    }

    #[test]
    fn pretty_tool_input_decodes_powershell_escapes_on_truncated_fallback() {
        // §28: PowerShell's ConvertTo-Json over-escapes the HTML-sensitive ASCII
        // chars ' < > & as \uXXXX even though they are printable. When such a blob
        // overflows the 2KB cap and is cut mid-JSON (so serde can't parse it), the
        // verbatim fallback must still un-escape those printable chars so the modal
        // reads as text, not `&`/`<` noise.
        let truncated =
            "{\"question\":\"Ship it \\u0026 tag \\u003crelease\\u003e? It\\u0027s ready";
        let out = pretty_tool_input(truncated);
        assert!(
            out.contains("Ship it & tag <release>? It's ready"),
            "printable escapes decoded: {out:?}"
        );
        assert!(!out.contains("\\u"), "no raw escapes remain: {out:?}");

        // A fragment cut mid-escape leaves the partial `\\u00` verbatim (only two
        // hex digits present, so it is never decoded).
        let partial = "{\"a\":\"x\\u00";
        assert_eq!(pretty_tool_input(partial), partial);

        // A non-printable escape (BEL, U+0007) is outside 0x20..=0x7E and is left
        // verbatim: we only touch what PowerShell needlessly escaped.
        let control = "{\"a\":\"x\\u0007";
        assert_eq!(pretty_tool_input(control), control);

        // The valid-input path is unaffected: serde decodes the escape to ' natively
        // and re-emits indented JSON, so the decoder never runs on parseable input.
        let valid = pretty_tool_input("{\"question\":\"It\\u0027s fine\"}");
        let v: serde_json::Value = serde_json::from_str(&valid).unwrap();
        assert_eq!(v["question"], "It's fine");
        assert!(valid.contains('\n'), "valid input pretty-printed: {valid:?}");
    }

    #[test]
    fn answer_file_parses_command_written_shape() {
        // Matches ipc::AnswerRecord (camelCase, extra fields ignored).
        let json = r#"{"id":"ask-1","answer":"Yes","kind":"option","answeredAtMs":1720000000000}"#;
        let parsed: AnswerFile = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.answer, "Yes");
        assert_eq!(parsed.kind, AskAnswerKind::Option);
    }

    #[test]
    fn recover_ask_files_recovers_valid_and_quarantines_malformed() {
        // R-8.7 + R-3.5 "(also applies to asks/answers)": at startup, a valid
        // ask file is consumed and returned (to be shown as orphaned/expired),
        // while a malformed/unreadable/empty-id one is moved to spool-quarantine
        // and logged — never silently deleted.
        let tmp = unique_tmp("recover-asks");
        let asks_dir = tmp.join("asks");
        let quarantine_dir = tmp.join("spool-quarantine");
        std::fs::create_dir_all(&asks_dir).unwrap();

        let good = asks_dir.join("ask-good.json");
        std::fs::write(
            &good,
            br#"{"id":"ask-good","question":"Tabs or spaces?","project":"quarterdeck"}"#,
        )
        .unwrap();
        let malformed = asks_dir.join("ask-bad.json");
        std::fs::write(&malformed, b"{ this is not json").unwrap();
        let empty_id = asks_dir.join("ask-empty.json");
        std::fs::write(&empty_id, br#"{"id":"","question":"x"}"#).unwrap();
        // A non-json sibling must be ignored entirely (neither recovered nor
        // quarantined).
        std::fs::write(asks_dir.join("notes.txt"), b"ignore me").unwrap();

        let recovered = recover_ask_files(&asks_dir, &quarantine_dir);

        // Exactly the one valid file comes back, with its fields intact.
        assert_eq!(recovered.len(), 1, "only the valid ask file is recovered");
        assert_eq!(recovered[0].id, "ask-good");
        assert_eq!(recovered[0].question, "Tabs or spaces?");

        // Valid file consumed; both bad files moved out of asks/ (not deleted).
        assert!(!good.exists(), "recovered file is consumed");
        assert!(!malformed.exists(), "malformed file left asks/");
        assert!(!empty_id.exists(), "empty-id file left asks/");

        // ...and landed in spool-quarantine (R-3.5), not the void.
        let quarantined: Vec<_> = std::fs::read_dir(&quarantine_dir)
            .expect("quarantine dir created")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            quarantined.iter().any(|n| n.starts_with("ask-bad.json")),
            "malformed file quarantined, got {quarantined:?}"
        );
        assert!(
            quarantined.iter().any(|n| n.starts_with("ask-empty.json")),
            "empty-id file quarantined, got {quarantined:?}"
        );

        // The non-json file was ignored: still in asks/, never quarantined.
        assert!(asks_dir.join("notes.txt").exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn ask_file_record_deserializes_form_and_legacy() {
        // SPEC §29 (R-29.2/R-29.6): a persisted form ask round-trips its
        // `questions`, and an OLD ask file with no `questions` field still
        // deserializes (field defaults to None) so a recovered legacy ask renders.
        let form: AskFileRecord = serde_json::from_str(
            r#"{"id":"ask-form","question":"Which environment?","questions":[
                {"header":"Env","question":"Which environment?","multiSelect":false,"options":["prod","staging"]},
                {"question":"Flags?","multiSelect":true,"options":["--fast"]}
            ]}"#,
        )
        .unwrap();
        let qs = form.questions.as_ref().expect("questions present");
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].header.as_deref(), Some("Env"));
        assert!(!qs[0].multi_select);
        assert!(qs[1].multi_select);
        assert_eq!(qs[1].options, ["--fast"]);

        let legacy: AskFileRecord =
            serde_json::from_str(r#"{"id":"ask-old","question":"Tabs or spaces?"}"#).unwrap();
        assert!(legacy.questions.is_none(), "old ask file → no form");
    }

    #[test]
    fn recover_ask_files_on_missing_dir_is_empty() {
        let tmp = unique_tmp("recover-missing");
        let recovered = recover_ask_files(&tmp.join("asks"), &tmp.join("spool-quarantine"));
        assert!(recovered.is_empty());
    }

    fn shell_perm(id: &str, session: Option<&str>, received_ms: u64) -> ShellPerm {
        ShellPerm {
            id: id.to_string(),
            session_id: session.map(ToString::to_string),
            project: None,
            tool_name: "Bash".to_string(),
            tool_input: "{}".to_string(),
            context: None,
            received_ms,
        }
    }

    #[test]
    fn perm_deadline_is_received_plus_ninety_seconds() {
        // R-32.1: a perm's deadline anchors on its arrival time + the ~90 s hook
        // timeout, and the projected PermRow carries it as `expires_at`.
        let perm = shell_perm("p1", Some("s1"), 1_000_000);
        assert_eq!(perm.deadline_ms(), 1_000_000 + PERM_DEADLINE_MS);
        let row = perm_to_row(&perm);
        assert_eq!(row.expires_at, Some(1_000_000 + PERM_DEADLINE_MS));
        assert_eq!(row.queued_at, 1_000_000);
    }

    #[test]
    fn drain_expired_perms_sweeps_only_past_deadline() {
        // R-32.1: the perm-queue mirror of `AskStore::sweep_expired` — a perm past
        // `received_ms + PERM_DEADLINE_MS` is removed; a fresher one stays. The
        // deadline is exclusive of "just arrived" and inclusive at the boundary.
        let mut perms = vec![
            shell_perm("stale", Some("s1"), 1_000),
            shell_perm("fresh", Some("s2"), 50_000),
        ];

        // Before either deadline: nothing swept.
        assert!(drain_expired_perms(&mut perms, 1_000).is_empty());
        assert_eq!(perms.len(), 2);

        // Exactly at the stale perm's deadline (1_000 + 90_000): it is swept, the
        // fresh one (deadline 140_000) survives.
        let expired = drain_expired_perms(&mut perms, 1_000 + PERM_DEADLINE_MS);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, "stale");
        assert_eq!(perms.len(), 1);
        assert_eq!(perms[0].id, "fresh");

        // Long past everything: the last one goes too.
        let expired = drain_expired_perms(&mut perms, 1_000_000);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, "fresh");
        assert!(perms.is_empty());
    }
}
