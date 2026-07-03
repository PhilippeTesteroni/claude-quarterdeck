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

pub mod focus;
pub mod foreground;
pub mod ipc;
pub mod mcp_server;
pub mod notify;
pub mod settings;
pub mod tray;
pub mod watcher;
pub mod windows;

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
use deck_core::traits::{Notifier, ProcessTable, ToastKind};
use deck_core::{discovery, events, hooks_config, registry};

use crate::ipc::{
    AppState, AskAnswerKind, AskRow, Counts, SessionRow, SessionStatus, SettingsState,
    StateSnapshot,
};
use crate::mcp_server::{AskAnswer, AskGateway, AskRequest, NotifyRequest};
use crate::notify::DesktopNotifier;
use crate::settings::Settings;

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

static ASK_SEQ: AtomicU64 = AtomicU64::new(0);

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
        timeout_at: Some(ask.timeout_at_ms),
        // R-8.2: only unmatched asks surface the raw context ("Unknown agent (<context>)").
        context: if ask.session_id.is_none() {
            ask.context.clone()
        } else {
            None
        },
        // R-8.7: an ask recovered from disk at startup is shown as expired.
        orphaned: ask.orphaned,
    }
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
    notifier: DesktopNotifier<Wry>,
    tray: tauri::tray::TrayIcon<Wry>,
    data_dir: PathBuf,
    version: String,
}

impl Shell {
    // --- state projection --------------------------------------------------

    fn snapshot(&self) -> StateSnapshot {
        let (sessions, counts) = {
            let store = self.store.lock().expect("store poisoned");
            let sessions: Vec<SessionRow> = store.view().iter().map(map_row).collect();
            let c = store.counts();
            (
                sessions,
                Counts {
                    attention: c.attention,
                    working: c.working,
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

        let settings = settings::load(&self.data_dir);
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
            mcp_enabled,
            mcp_cli_available: claude_cli_on_path(),
            mcp_command,
            data_dir: self.data_dir.display().to_string(),
            version: self.version.clone(),
        };

        StateSnapshot {
            sessions,
            asks,
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

    /// The terminal PIDs for one session (ancestor pid ∪ registry/claude pid,
    /// R-17.2). Empty when the session is unknown or has no known pid.
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

    /// R-17.2: is the given session's terminal window the foreground window?
    /// Samples the foreground chain (R-17.1) and intersects with the session's
    /// terminal pids. Unmatched asks (`None`) are never terminal-foreground —
    /// there's no terminal to defer to. A fresh sample is taken here so callers
    /// get the "immediately before showing / firing" check R-17.1 requires.
    fn session_foreground(&self, session_id: Option<&str>) -> bool {
        let Some(sid) = session_id else {
            return false;
        };
        let terminal = self.session_terminal_pids(sid);
        if terminal.is_empty() {
            return false;
        }
        let fg = foreground::sample_foreground_pids();
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
        // Sample the foreground once, then check every pending ask against it.
        let fg = foreground::sample_foreground_pids();
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

    /// Focus the terminal window hosting `session_id` (R-15.4). Looks up the
    /// captured ancestor + project from the store, then best-effort focuses.
    fn focus_terminal(&self, session_id: &str) -> Result<(), String> {
        let (ancestor, project) = {
            let store = self.store.lock().expect("store poisoned");
            (store.ancestor_of(session_id), store.project_of(session_id))
        };
        focus::focus_terminal(ancestor, &project.unwrap_or_default())
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
        // R-17.1: sample the foreground chain once for this batch; a toast whose
        // session terminal is the foreground window is suppressed (R-17.2).
        let foreground = foreground::sample_foreground_pids();
        let terminal_pids = self.store.lock().expect("store poisoned").terminal_pids();
        for effect in effects {
            let Effect::Toast(decision) = effect;
            let enabled = match decision.kind {
                ToastKind::Idle => settings.notify_idle,
                ToastKind::Attention => settings.notify_attention,
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
            let shown = enabled
                && !terminal_foreground
                && self.notifier.notify(
                    decision.kind,
                    &decision.session_id,
                    &decision.project,
                    &decision.detail,
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

    fn write_ask_file(&self, ask: &ShellAsk, timeout_seconds: u64) {
        let record = serde_json::json!({
            "id": ask.id,
            "sessionId": ask.session_id,
            "project": ask.project,
            "question": ask.question,
            "options": ask.options,
            "context": ask.context,
            "timeoutSeconds": timeout_seconds,
            "timeoutAtMs": ask.timeout_at_ms,
            "createdAtMs": now_ms(),
        });
        if let Ok(bytes) = serde_json::to_vec_pretty(&record) {
            if let Err(err) = settings::atomic_write(&self.ask_file_path(&ask.id), &bytes) {
                tracing::warn!(error = %err, ask_id = %ask.id, "failed to persist ask file");
            }
        }
    }

    fn submit_ask(&self, req: AskRequest) -> oneshot::Receiver<AskAnswer> {
        let (tx, rx) = oneshot::channel();
        let ask_id = format!(
            "ask-{}-{}",
            now_ms(),
            ASK_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let (session_id, project) = self.match_session(req.context.as_deref());
        let timeout_at_ms = now_ms() + req.timeout_seconds.saturating_mul(1000);

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
            context: req.context.clone(),
            timeout_at_ms,
            orphaned: false,
            responder: Some(tx),
        };
        self.write_ask_file(&ask, req.timeout_seconds);
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
        rx
    }

    fn notify_user(&self, req: NotifyRequest) {
        // R-9.5: the MCP `notify_user` toast uses the alert (Attention) channel,
        // so it honors the `notifyAttention` toggle just like hook-driven
        // attention toasts (`fire_effects`). Unlike `ask_user` (R-8.4 "Ask toasts
        // never suppressed"), no spec text exempts `notify_user` from the toggle.
        if !settings::load(&self.data_dir).notify_attention {
            tracing::debug!("notify_user suppressed: notifyAttention is off (R-9.5)");
            return;
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
        self.notifier.notify(
            ToastKind::Attention,
            &key,
            &project,
            &req.message,
            self.popup_focused(),
        );
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
                context: rec.context,
                timeout_at_ms: 0,
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
                    AskAnswerKind::Option | AskAnswerKind::Text => store.note_ask_answered(sid),
                    AskAnswerKind::Timeout | AskAnswerKind::Dismissed => {
                        store.note_ask_cleared(sid)
                    }
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
        if self.asks_empty() {
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
        if self.asks_empty() {
            run_on_main(&self.app, |app| {
                let _ = windows::hide_ask_window(app);
            });
        }
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
    fn submit_ask(&self, req: AskRequest) -> oneshot::Receiver<AskAnswer> {
        self.shell.submit_ask(req)
    }

    fn notify(&self, req: NotifyRequest) {
        self.shell.notify_user(req);
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
        if !entries.is_empty() {
            shell
                .store
                .lock()
                .expect("store poisoned")
                .apply_registry(&entries);
        }
    }
    let procs = SysProcs::refreshed();
    let effects = {
        let mut store = shell.store.lock().expect("store poisoned");
        store.tick(&procs, transcript_mtime_ms)
    };
    shell.fire_effects(effects);
    shell.sweep_expired_asks();
    enforce_disk_caps();
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
/// entries into `settings_path`. Pure (no `AppHandle`) so it is unit-testable.
fn install_hooks_to(
    settings_path: &Path,
    hooks_src: &Path,
    hooks_dst: &Path,
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
    hooks_config::install_hooks(settings_path, &command).map_err(|e| e.to_string())
}

fn perform_install_hooks(app: &AppHandle<Wry>) -> Result<(), String> {
    let hooks_src = resolve_hooks_src(app)
        .ok_or_else(|| "could not locate bundled hook scripts".to_string())?;
    let change = install_hooks_to(&claude_settings_path(), &hooks_src, &settings::hooks_dir())?;
    tracing::info!(
        changed = change.changed,
        events = ?change.events_added,
        backup = ?change.backup,
        "hook install complete"
    );
    Ok(())
}

fn perform_uninstall_hooks() -> Result<(), String> {
    let change = hooks_config::uninstall_hooks(&claude_settings_path(), hooks_config::MARKER)
        .map_err(|e| e.to_string())?;
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
        }
        "mcpEnabled" => {
            let enabled = settings
                .extra
                .get("mcpEnabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            setup_agent_questions(app, enabled);
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

/// Focus the terminal window hosting `session_id` (R-15.4), invoked by the
/// `focus_terminal` command (row click / context menu). Returns
/// `focus::NOT_FOUND_MSG` when no window could be focused, which the UI shows as
/// an inline notice (R-15.4b).
pub fn focus_terminal_command(app: &AppHandle<Wry>, session_id: &str) -> Result<(), String> {
    let shell = app
        .try_state::<Arc<Shell>>()
        .ok_or_else(|| "Quarterdeck is still starting up".to_string())?;
    shell.focus_terminal(session_id)
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
            ipc::answer_ask,
            ipc::set_setting,
            ipc::install_hooks,
            ipc::uninstall_hooks,
            ipc::resize_popup,
            ipc::show_ask_window,
            ipc::focus_terminal,
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
                notifier: DesktopNotifier::new(handle.clone()),
                tray,
                data_dir: data_dir.clone(),
                version: env!("CARGO_PKG_VERSION").to_string(),
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
            sync_autostart(&handle, settings::load(&data_dir).launch_at_login);

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
            map_status(EngineStatus::Attention),
            SessionStatus::Attention
        );
        assert_eq!(map_status(EngineStatus::Idle), SessionStatus::Idle);
        assert_eq!(map_status(EngineStatus::Dead), SessionStatus::Dead);
    }

    #[test]
    fn install_hooks_to_writes_isolated_settings_json() {
        let tmp = unique_tmp("install");
        let settings_path = tmp.join("claude").join("settings.json");
        let hooks_dst = tmp.join("data").join("hooks");
        let hooks_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../hooks");

        let change = install_hooks_to(&settings_path, &hooks_src, &hooks_dst).unwrap();
        assert!(change.changed, "first install writes");
        assert_eq!(change.events_added.len(), 5, "all five hook events added");

        let text = std::fs::read_to_string(&settings_path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let hooks = &value["hooks"];
        for event in [
            "SessionStart",
            "UserPromptSubmit",
            "Notification",
            "Stop",
            "SessionEnd",
        ] {
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

        // Scripts copied to the stable path (R-4.4).
        assert!(
            hooks_dst.join("quarterdeck-hook.ps1").exists()
                || hooks_dst.join("quarterdeck-hook.sh").exists()
        );

        // Idempotent re-install (R-4.1).
        let again = install_hooks_to(&settings_path, &hooks_src, &hooks_dst).unwrap();
        assert!(!again.changed, "re-install is a no-op");

        // Uninstall restores a hook-free config (R-4.2).
        let removed = hooks_config::uninstall_hooks(&settings_path, hooks_config::MARKER).unwrap();
        assert!(removed.changed);
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(after.get("hooks").is_none(), "hooks pruned after uninstall");

        let _ = std::fs::remove_dir_all(&tmp);
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
    fn recover_ask_files_on_missing_dir_is_empty() {
        let tmp = unique_tmp("recover-missing");
        let recovered = recover_ask_files(&tmp.join("asks"), &tmp.join("spool-quarantine"));
        assert!(recovered.is_empty());
    }
}
