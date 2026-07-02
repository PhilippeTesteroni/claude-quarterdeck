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
use tauri::{AppHandle, Emitter, Manager, Wry};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tokio::sync::oneshot;

use deck_core::engine::{Effect, SessionStore, SessionView, Status as EngineStatus};
use deck_core::traits::{Notifier, ProcessTable, ToastKind};
use deck_core::{discovery, events, hooks_config};

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
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
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
        na.eq_ignore_ascii_case(&nb)
    } else {
        na == nb
    }
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

struct PendingAsk {
    id: String,
    session_id: Option<String>,
    project: Option<String>,
    question: String,
    options: Option<Vec<String>>,
    context: Option<String>,
    timeout_at_ms: u64,
    responder: Option<oneshot::Sender<AskAnswer>>,
}

impl PendingAsk {
    fn to_row(&self) -> AskRow {
        AskRow {
            id: self.id.clone(),
            session_id: self.session_id.clone(),
            project: self.project.clone(),
            question: self.question.clone(),
            options: self.options.clone(),
            timeout_at: Some(self.timeout_at_ms),
            // R-8.2: only unmatched asks surface the raw context ("Unknown agent (<context>)").
            context: if self.session_id.is_none() {
                self.context.clone()
            } else {
                None
            },
        }
    }
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
    asks: Mutex<Vec<PendingAsk>>,
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
            .map(PendingAsk::to_row)
            .collect();

        let settings = settings::load(&self.data_dir);
        let mcp_enabled = settings
            .extra
            .get("mcpEnabled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let settings_state = SettingsState {
            notify_idle: settings.notify_idle,
            notify_attention: settings.notify_attention,
            notify_reminder: settings.notify_reminder,
            launch_at_login: settings.launch_at_login,
            onboarding_done: settings.onboarding_done,
            mcp_enabled,
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
        for effect in effects {
            let Effect::Toast(decision) = effect;
            let enabled = match decision.kind {
                ToastKind::Idle => settings.notify_idle,
                ToastKind::Attention => settings.notify_attention,
                ToastKind::Reminder => settings.notify_reminder,
                ToastKind::Ask => true,
            };
            if !enabled {
                continue;
            }
            self.notifier.notify(
                decision.kind,
                &decision.session_id,
                &decision.project,
                &decision.detail,
                popup,
            );
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
            if !v.cwd.is_empty() && basename(&v.cwd) == base {
                return (Some(v.id.clone()), Some(v.project.clone()));
            }
        }
        (None, None)
    }

    fn ask_file_path(&self, id: &str) -> PathBuf {
        settings::asks_dir().join(format!("{id}.json"))
    }

    fn write_ask_file(&self, ask: &PendingAsk, timeout_seconds: u64) {
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

        let ask = PendingAsk {
            id: ask_id.clone(),
            session_id: session_id.clone(),
            project: project.clone(),
            question: req.question.clone(),
            options: req.options.clone(),
            context: req.context.clone(),
            timeout_at_ms,
            responder: Some(tx),
        };
        self.write_ask_file(&ask, req.timeout_seconds);
        self.asks.lock().expect("asks poisoned").push(ask);

        self.push_state();

        // Always-on-top ask window (never steals focus; R-8.3).
        run_on_main(&self.app, |app| {
            if let Err(err) = windows::show_ask_window(app) {
                tracing::warn!(error = %err, "failed to show ask window");
            }
        });

        // R-8.4: alert toast, exempt from throttle/suppression/toggles.
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

        tracing::info!(ask_id = %ask_id, matched = session_id.is_some(), "ask submitted");
        rx
    }

    fn notify_user(&self, req: NotifyRequest) {
        let (session_id, project) = self.match_session(req.context.as_deref());
        let project = project
            .or_else(|| req.context.as_deref().map(basename))
            .unwrap_or_else(|| "Agent".to_string());
        let key = session_id.unwrap_or_else(|| format!("notify-{}", now_ms()));
        self.notifier.notify(
            ToastKind::Attention,
            &key,
            &project,
            &req.message,
            self.popup_focused(),
        );
    }

    /// R-8.7: at startup, clear any ask files left by a previous process — their
    /// MCP connections are gone, so they can never be answered.
    fn orphan_stale_asks(&self) {
        let dir = settings::asks_dir();
        let mut cleared = 0usize;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("json")
                    && std::fs::remove_file(&path).is_ok()
                {
                    cleared += 1;
                }
            }
        }
        if cleared > 0 {
            tracing::warn!(cleared, "orphaned stale asks from a previous run (R-8.7)");
        }
    }

    fn take_ask(&self, id: &str) -> Option<PendingAsk> {
        let mut asks = self.asks.lock().expect("asks poisoned");
        asks.iter().position(|a| a.id == id).map(|i| asks.remove(i))
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
            return;
        };
        let Ok(parsed) = serde_json::from_str::<AnswerFile>(&text) else {
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
        if let Some(tx) = ask.responder.take() {
            let _ = tx.send(answer);
        }
        if let Some(sid) = &ask.session_id {
            let mut store = self.store.lock().expect("store poisoned");
            match parsed.kind {
                AskAnswerKind::Option | AskAnswerKind::Text => store.note_ask_answered(sid),
                AskAnswerKind::Timeout | AskAnswerKind::Dismissed => store.note_ask_cleared(sid),
            }
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
    fn sweep_expired_asks(&self) {
        let now = now_ms();
        let expired: Vec<PendingAsk> = {
            let mut asks = self.asks.lock().expect("asks poisoned");
            let mut out = Vec::new();
            let mut i = 0;
            while i < asks.len() {
                if now >= asks[i].timeout_at_ms {
                    out.push(asks.remove(i));
                } else {
                    i += 1;
                }
            }
            out
        };
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

    shell.push_state();

    let watcher = match watcher::SpoolWatcher::spawn(&spool_dir, watcher::DEFAULT_DEBOUNCE) {
        Ok(w) => w,
        Err(err) => {
            tracing::error!(error = %err, "spool watcher failed to start");
            return;
        }
    };

    let mut last_tick = Instant::now();
    loop {
        match watcher.paths.recv_timeout(LOOP_SLICE) {
            Ok(path) => {
                ingest_path(&shell, &quarantine_dir, &path);
                while let Ok(path) = watcher.paths.try_recv() {
                    ingest_path(&shell, &quarantine_dir, &path);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                tracing::warn!("spool watcher channel closed; engine loop exiting");
                break;
            }
        }

        if last_tick.elapsed() >= ENGINE_TICK {
            last_tick = Instant::now();
            run_tick(&shell);
        }
    }
}

fn ingest_path(shell: &Arc<Shell>, quarantine_dir: &Path, path: &Path) {
    if path.extension().and_then(|e| e.to_str()) != Some("json") {
        return;
    }
    match events::ingest_file(path, quarantine_dir, now_ms()) {
        Ok(Some(event)) => {
            let effects = {
                let mut store = shell.store.lock().expect("store poisoned");
                store.on_event(&event)
            };
            let _ = std::fs::remove_file(path);
            shell.fire_effects(effects);
            shell.push_state();
        }
        Ok(None) => {}
        Err(err) => tracing::warn!(error = %err, ?path, "failed to ingest spool file"),
    }
}

fn run_tick(shell: &Arc<Shell>) {
    let procs = SysProcs::refreshed();
    let effects = {
        let mut store = shell.store.lock().expect("store poisoned");
        store.tick(&procs, transcript_mtime_ms)
    };
    shell.fire_effects(effects);
    shell.sweep_expired_asks();
    shell.push_state();
}

// ---------------------------------------------------------------------------
// Answers watcher: turns `<data>/answers/*.json` (written by `answer_ask`) into
// resolved MCP `ask_user` results (R-8.7).
// ---------------------------------------------------------------------------

fn run_answers(shell: Arc<Shell>) {
    let answers_dir = settings::answers_dir();
    let watcher = match watcher::SpoolWatcher::spawn(&answers_dir, watcher::DEFAULT_DEBOUNCE) {
        Ok(w) => w,
        Err(err) => {
            tracing::error!(error = %err, "answers watcher failed to start");
            return;
        }
    };
    loop {
        match watcher.paths.recv_timeout(LOOP_SLICE) {
            Ok(path) => shell.resolve_answer(&path),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
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
/// bundled skill. Best-effort — the `claude` CLI may not be on PATH; if not, the
/// UI still shows the copy-paste command elsewhere.
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
                        "`claude mcp add` unavailable; user must run it manually (R-8.6)"
                    )
                }
            }
        }
        copy_skill(app);
    } else {
        let _ = std::process::Command::new("claude")
            .args(["mcp", "remove", "--scope", "user", "quarterdeck"])
            .output();
    }
}

/// Apply the side effect for a persisted setting change (called by `set_setting`).
pub fn apply_setting_side_effect(app: &AppHandle<Wry>, key: &str, settings: &Settings) {
    match key {
        "launchAtLogin" => sync_autostart(app, settings.launch_at_login),
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

    tauri::Builder::default()
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
        ])
        .setup(move |app| {
            let handle = app.handle().clone();

            // Tray + windows (R-2.6, R-7.1, R-8.3).
            let tray = tray::build(&handle)?;
            if let Err(err) = windows::setup_popup_behavior(&handle) {
                tracing::warn!(error = %err, "failed to set up popup behavior");
            }

            // The shared shell.
            let shell = Arc::new(Shell {
                app: handle.clone(),
                store: Mutex::new(SessionStore::with_system_clock()),
                asks: Mutex::new(Vec::new()),
                notifier: DesktopNotifier::new(handle.clone()),
                tray,
                data_dir: data_dir.clone(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            });
            app.manage(shell.clone());

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
        .run(tauri::generate_context!())
        .expect("error while running Quarterdeck");
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
}
