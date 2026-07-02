//! The status engine: a [`SessionStore`] reducer that applies parsed events to
//! per-session state and drives the SPEC §2 transition table (working /
//! attention / idle / dead), including `attention → working` recovery (R-2.2),
//! the pending-ask override (R-2.4), timings via an injectable [`Clock`]
//! (R-2.5), and throttled toast-decision output events (R-9.4).
//!
//! The reducer is pure aside from the injected clock and the transcript-stat
//! closures it is handed: no Tauri, no globals. The shell (T3/T7) owns the
//! actual IO (statting transcripts, firing toasts) and feeds this engine.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::events::{HookEvent, SpoolEvent};
use crate::traits::{Clock, ProcessTable, ToastKind};
use crate::{liveness, naming};

/// A transcript must advance to at least this far past the Notification
/// timestamp before an `attention` session recovers to `working` (R-2.2).
pub const RECOVERY_MIN_ADVANCE_MS: u64 = 2_000;

/// How long a `dead` row persists before it is pruned (R-2.5).
pub const DEAD_RETENTION_MS: u64 = 5 * 60 * 1000;

/// Per-session toast throttle window (R-9.4): at most one status toast per
/// session per 10 s; rapid bursts collapse.
pub const TOAST_THROTTLE_MS: u64 = 10_000;

/// Session status (SPEC §2). `dead` sessions linger for [`DEAD_RETENTION_MS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    Working,
    Attention,
    Idle,
    Dead,
}

impl Status {
    /// Lowercase wire name, matching the UI status union.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Working => "working",
            Status::Attention => "attention",
            Status::Idle => "idle",
            Status::Dead => "dead",
        }
    }

    /// Sort/aggregation priority: attention worst, then working, idle, dead
    /// (R-7.3 sort order and R-2.6 worst-of aggregation).
    #[must_use]
    pub fn priority(self) -> u8 {
        match self {
            Status::Attention => 3,
            Status::Working => 2,
            Status::Idle => 1,
            Status::Dead => 0,
        }
    }
}

/// Real wall clock used by the shell. Tests inject a fake instead.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// A toast the engine has decided to emit, already burst-throttled (R-9.4).
///
/// It maps 1:1 onto [`crate::traits::Notifier::notify`]: the shell calls
/// `notify(kind, session_id, project, detail, popup_visible_and_focused)` and
/// owns the copy templates (title/body/sound per `kind`, R-7.6). The engine only
/// decides *whether* and *with what payload* to toast. `detail` is the
/// kind-specific body content: the session task title for
/// [`ToastKind::Idle`], the notification message for [`ToastKind::Attention`].
/// (Ask toasts are fired by the ask subsystem, not this status reducer.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastDecision {
    pub session_id: String,
    pub kind: ToastKind,
    pub project: String,
    pub detail: String,
    /// Event timestamp this decision was throttled against (R-9.4). The shell
    /// passes it back to [`SessionStore::refund_toast`] when it suppresses the
    /// toast for a reason the engine can't see (R-9.5 toggle off), so a
    /// never-shown toast doesn't consume the throttle window.
    pub at_ms: u64,
}

/// Output events the reducer emits for the shell to act on. State itself is read
/// via [`SessionStore::view`] (the UI gets full snapshots, R-3.4), so the only
/// side effect that needs an event channel is notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Toast(ToastDecision),
}

/// A session projected for the UI (engine-side mirror of `ipc::SessionRow`; the
/// shell maps this into the wire type in T7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionView {
    pub id: String,
    pub project: String,
    pub title: String,
    pub branch: Option<String>,
    pub status: Status,
    pub inferred: bool,
    pub since_ms: u64,
    pub cwd: String,
}

/// Per-status counts (engine-side mirror of `ipc::Counts`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatusCounts {
    pub attention: u32,
    pub working: u32,
    pub idle: u32,
    pub dead: u32,
}

#[derive(Debug)]
struct Session {
    id: String,
    cwd: Option<String>,
    transcript_path: Option<String>,
    claude_pid: Option<u32>,
    /// Status derived from hooks/recovery/liveness, before the ask override.
    hook_status: Status,
    /// Status actually shown (hook status, or `Attention` while an ask is pending).
    effective_status: Status,
    /// Epoch ms the effective status was entered (drives `since_ms`, R-2.5).
    entered_at_ms: u64,
    last_activity_ms: u64,
    session_title: Option<String>,
    latest_prompt: Option<String>,
    /// Cached cold-start transcript title so we read the file at most once.
    transcript_title: Option<String>,
    display_title: String,
    inferred: bool,
    branch: Option<String>,
    /// Number of Quarterdeck asks currently pending for this session → force
    /// `attention` while > 0 (R-2.4). A count, not a bool: two agents can share
    /// one session row (same cwd, or a basename-fallback match, `lib.rs`
    /// `match_session`), so resolving/timing-out ONE ask must not clear the
    /// override while another is still waiting on the human.
    pending_asks: u32,
    /// `attention` originated from a Notification hook (eligible for R-2.2 recovery).
    attention_from_hook: bool,
    /// Receipt time of the Notification that put us in `attention` (R-2.2 anchor).
    last_notification_ms: Option<u64>,
    dead_since_ms: Option<u64>,
    /// Last emission time per toast kind (R-9.4: the throttle is per
    /// `(session, status-change kind)`, so an idle toast never swallows a
    /// genuinely-different attention/reminder alert within the same window).
    last_toast_ms: HashMap<ToastKind, u64>,
}

impl Session {
    fn new(id: String, now_ms: u64) -> Self {
        Session {
            id,
            cwd: None,
            transcript_path: None,
            claude_pid: None,
            hook_status: Status::Idle,
            effective_status: Status::Idle,
            entered_at_ms: now_ms,
            last_activity_ms: now_ms,
            session_title: None,
            latest_prompt: None,
            transcript_title: None,
            display_title: naming::NO_TITLE.to_string(),
            inferred: false,
            branch: None,
            pending_asks: 0,
            attention_from_hook: false,
            last_notification_ms: None,
            dead_since_ms: None,
            last_toast_ms: HashMap::new(),
        }
    }

    fn effective(&self) -> Status {
        if self.pending_asks > 0 {
            Status::Attention
        } else {
            self.hook_status
        }
    }

    /// Set the hook-derived status; returns `Some(new_effective)` iff the
    /// effective (shown) status changed as a result.
    fn set_hook_status(&mut self, new: Status, ts_ms: u64) -> Option<Status> {
        let before = self.effective_status;
        self.hook_status = new;
        if new != Status::Dead {
            self.dead_since_ms = None;
        }
        let after = self.effective();
        if after != before {
            self.effective_status = after;
            self.entered_at_ms = ts_ms;
            Some(after)
        } else {
            None
        }
    }

    /// Re-derive the shown status after `pending_asks` or `hook_status` changed;
    /// returns `Some(new_effective)` iff the shown status changed (so the caller
    /// can re-stamp `entered_at`/`last_activity`, R-2.5).
    fn resettle_effective(&mut self, ts_ms: u64) -> Option<Status> {
        let before = self.effective_status;
        let after = self.effective();
        if after != before {
            self.effective_status = after;
            self.entered_at_ms = ts_ms;
            Some(after)
        } else {
            None
        }
    }

    fn recompute_title(&mut self) {
        let need_fallback = self
            .session_title
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
            && self
                .latest_prompt
                .as_deref()
                .map(str::trim)
                .is_none_or(str::is_empty);
        let fallback = if need_fallback {
            if self.transcript_title.is_none() {
                if let Some(tp) = &self.transcript_path {
                    self.transcript_title = naming::transcript_first_user_text(Path::new(tp));
                }
            }
            self.transcript_title.clone()
        } else {
            None
        };
        self.display_title = naming::title_from_sources(
            self.session_title.as_deref(),
            self.latest_prompt.as_deref(),
            fallback.as_deref(),
        );
    }

    fn throttle_ok(&self, kind: ToastKind, ts_ms: u64) -> bool {
        match self.last_toast_ms.get(&kind) {
            Some(&prev) => ts_ms.saturating_sub(prev) >= TOAST_THROTTLE_MS,
            None => true,
        }
    }

    fn project(&self) -> String {
        naming::project_name(self.cwd.as_deref())
    }
}

/// Classification of a `Notification.notification_type` (R-2.1).
enum NotifClass {
    /// Blocks on a human → `attention`.
    Attention,
    /// `idle_prompt` → no status change, optional reminder (R-2.3).
    IdlePrompt,
    /// Known-but-inert or unknown → ignored for status, logged.
    Ignored,
}

fn classify_notification(notification_type: Option<&str>) -> NotifClass {
    match notification_type {
        Some("permission_prompt") | Some("elicitation_dialog") => NotifClass::Attention,
        Some("idle_prompt") => NotifClass::IdlePrompt,
        _ => NotifClass::Ignored,
    }
}

/// The reducer: owns all live session state and the injected clock.
///
/// The clock is `Send + Sync` so the whole store can live behind a `Mutex` in
/// the Tauri shell and be driven from the watcher and timer tasks (T7).
pub struct SessionStore {
    sessions: HashMap<String, Session>,
    /// Tombstones for sessions that received `SessionEnd`, mapping id → the end
    /// timestamp. Guards R-2.5 ("SessionEnd always wins immediately"): a
    /// debounce-reordered or otherwise trailing event (Stop / Notification /
    /// prompt) for an already-ended session must NOT resurrect the removed row
    /// (which would revive a phantom red tray + a false "needs you" alert). A
    /// genuinely-later `SessionStart` (a resume, or a reused id) — one whose
    /// timestamp is strictly after the recorded end — clears the tombstone and
    /// re-creates the row. Pruned to the spool freshness window so it stays
    /// bounded (a stale event beyond it is discarded on replay anyway, R-3.5).
    ended: HashMap<String, u64>,
    clock: Box<dyn Clock + Send + Sync>,
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStore")
            .field("sessions", &self.sessions.len())
            .finish()
    }
}

impl SessionStore {
    /// Construct with an injected clock (fake in tests, [`SystemClock`] in prod).
    #[must_use]
    pub fn new(clock: Box<dyn Clock + Send + Sync>) -> Self {
        SessionStore {
            sessions: HashMap::new(),
            ended: HashMap::new(),
            clock,
        }
    }

    /// Convenience constructor using the real system clock.
    #[must_use]
    pub fn with_system_clock() -> Self {
        Self::new(Box::new(SystemClock))
    }

    /// Current time from the injected clock.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    #[must_use]
    pub fn contains(&self, session_id: &str) -> bool {
        self.sessions.contains_key(session_id)
    }

    /// Set of currently-tracked session ids (feeds cold-start discovery so it
    /// only infers *unknown* sessions, R-5.4).
    #[must_use]
    pub fn known_ids(&self) -> HashSet<String> {
        self.sessions.keys().cloned().collect()
    }

    /// Effective (shown) status of a session, if tracked.
    #[must_use]
    pub fn status_of(&self, session_id: &str) -> Option<Status> {
        self.sessions.get(session_id).map(Session::effective)
    }

    /// Milliseconds the session has spent in its current status (R-2.5).
    #[must_use]
    pub fn since_ms_of(&self, session_id: &str) -> Option<u64> {
        let now = self.clock.now_ms();
        self.sessions
            .get(session_id)
            .map(|s| now.saturating_sub(s.entered_at_ms))
    }

    /// Rendered title of a session (for tests/tools).
    #[must_use]
    pub fn title_of(&self, session_id: &str) -> Option<String> {
        self.sessions
            .get(session_id)
            .map(|s| s.display_title.clone())
    }

    // --- Core reducer ------------------------------------------------------

    /// Apply one parsed spool event, returning any toast decisions (R-9.1,
    /// R-9.2, R-2.3), each already burst-throttled (R-9.4).
    pub fn on_event(&mut self, ev: &SpoolEvent) -> Vec<Effect> {
        let ts = ev.received_at_ms.unwrap_or_else(|| self.clock.now_ms());

        // SessionEnd removes the row immediately, any reason (R-2.5, R-5.1), and
        // tombstones the id so a reordered trailing event can't resurrect it.
        if let HookEvent::SessionEnd { .. } = ev.kind {
            self.sessions.remove(&ev.session_id);
            self.record_ended(ev.session_id.clone(), ts);
            return Vec::new();
        }

        // R-2.5 tombstone guard: ignore trailing/stale events for an ended
        // session. A genuinely-later SessionStart (resume / reused id, strictly
        // after the end) clears the tombstone and is allowed to re-create the row.
        if let Some(&end_ts) = self.ended.get(&ev.session_id) {
            let is_restart = matches!(ev.kind, HookEvent::SessionStart { .. }) && ts > end_ts;
            if is_restart {
                self.ended.remove(&ev.session_id);
            } else {
                tracing::debug!(
                    session_id = %ev.session_id,
                    event = ev.kind.name(),
                    "ignoring event for ended session (R-2.5 tombstone)"
                );
                return Vec::new();
            }
        }

        let session = self
            .sessions
            .entry(ev.session_id.clone())
            .or_insert_with(|| Session::new(ev.session_id.clone(), ts));

        // Common payload fields update whenever present (never blanked by absence).
        if let Some(cwd) = &ev.cwd {
            session.cwd = Some(cwd.clone());
        }
        if let Some(tp) = &ev.transcript_path {
            session.transcript_path = Some(tp.clone());
        }
        session.last_activity_ms = ts;

        let mut effect: Option<(ToastDecision, u64)> = None;

        match &ev.kind {
            HookEvent::SessionStart { session_title, .. } => {
                if let Some(pid) = ev.claude_pid {
                    session.claude_pid = Some(pid);
                }
                if let Some(t) = session_title {
                    session.session_title = Some(t.clone());
                }
                session.recompute_title();
                // SessionStart → idle. No toast: a fresh/resumed session hasn't
                // "finished" anything (R-9.1 is Stop-only).
                session.set_hook_status(Status::Idle, ts);
                session.attention_from_hook = false;
                session.last_notification_ms = None;
            }
            HookEvent::UserPromptSubmit { prompt } => {
                if let Some(p) = prompt {
                    session.latest_prompt = Some(p.clone());
                }
                session.recompute_title();
                session.set_hook_status(Status::Working, ts);
                session.attention_from_hook = false;
                session.last_notification_ms = None;
            }
            HookEvent::Notification {
                message,
                notification_type,
            } => {
                session.recompute_title();
                match classify_notification(notification_type.as_deref()) {
                    NotifClass::Attention => {
                        session.attention_from_hook = true;
                        session.last_notification_ms = Some(ts);
                        if session.set_hook_status(Status::Attention, ts) == Some(Status::Attention)
                        {
                            let detail = message
                                .clone()
                                .filter(|m| !m.is_empty())
                                .unwrap_or_else(|| "Needs your attention.".to_string());
                            effect = Some((
                                ToastDecision {
                                    session_id: session.id.clone(),
                                    kind: ToastKind::Attention,
                                    project: session.project(),
                                    detail,
                                    at_ms: ts,
                                },
                                ts,
                            ));
                        }
                    }
                    NotifClass::IdlePrompt => {
                        // R-2.3: `idle_prompt` does NOT change status (the session
                        // is already `idle` via Stop). It does, however, drive the
                        // optional "still waiting" reminder toast (R-9.5, default
                        // OFF): the engine emits the decision, the shell gates it
                        // on `notifyReminder`. Only meaningful while actually idle.
                        tracing::debug!(session_id = %session.id, "idle_prompt: no status change (R-2.3)");
                        if session.effective() == Status::Idle {
                            effect = Some((
                                ToastDecision {
                                    session_id: session.id.clone(),
                                    kind: ToastKind::Reminder,
                                    project: session.project(),
                                    // Reminder copy is fixed (R-2.3); detail unused.
                                    detail: String::new(),
                                    at_ms: ts,
                                },
                                ts,
                            ));
                        }
                    }
                    NotifClass::Ignored => {
                        tracing::debug!(
                            session_id = %session.id,
                            notification_type = ?notification_type,
                            "notification ignored for status (R-2.1)"
                        );
                    }
                }
            }
            HookEvent::Stop => {
                session.recompute_title();
                session.attention_from_hook = false;
                session.last_notification_ms = None;
                if session.set_hook_status(Status::Idle, ts) == Some(Status::Idle) {
                    // `detail` = the session task title; empty when unknown so the
                    // shell renders just "Waiting for new instructions." (R-9.1).
                    let detail = if session.display_title == naming::NO_TITLE {
                        String::new()
                    } else {
                        session.display_title.clone()
                    };
                    effect = Some((
                        ToastDecision {
                            session_id: session.id.clone(),
                            kind: ToastKind::Idle,
                            project: session.project(),
                            detail,
                            at_ms: ts,
                        },
                        ts,
                    ));
                }
            }
            HookEvent::SessionEnd { .. } => unreachable!("handled above"),
            HookEvent::Unknown { name } => {
                tracing::debug!(session_id = %session.id, event = %name, "unknown hook event ignored (R-4.5)");
                session.recompute_title();
            }
        }

        // Apply the R-9.4 burst throttle at the point of emission, keyed per
        // toast kind so different status changes don't collapse into each other.
        // The stamp records an *emitted* decision; if the shell then suppresses
        // it for a reason only the shell can see (R-9.5 toggle off), it calls
        // `refund_toast` to release the slot so the throttle keeps bounding
        // *actually-shown* toasts (R-9.4).
        if let Some((toast, at)) = effect {
            if session.throttle_ok(toast.kind, at) {
                session.last_toast_ms.insert(toast.kind, at);
                return vec![Effect::Toast(toast)];
            }
        }
        Vec::new()
    }

    /// Release the R-9.4 throttle slot for a toast the shell decided NOT to show
    /// (e.g. the R-9.5 per-type toggle for its kind is off). `on_event` stamps
    /// the throttle when it *emits* a decision, but the throttle bounds
    /// *actually-shown* toasts — a suppressed toast must not consume the window,
    /// else the next same-kind toast is wrongly dropped for up to 10 s after the
    /// toggle is re-enabled. Only clears the stamp when it still matches `at_ms`
    /// (no newer toast replaced it); removal is equivalent to restoring the
    /// previous stamp, because a stamp is only ever created ≥10 s after the one
    /// before it (`throttle_ok`), so the next toast would pass either way.
    /// Record a `SessionEnd` tombstone, pruning entries older than the spool
    /// freshness window so the map stays bounded (R-3.5: a stale event beyond
    /// 24 h is discarded on replay anyway, so its tombstone is moot).
    fn record_ended(&mut self, id: String, end_ts: u64) {
        let now = self.clock.now_ms();
        self.ended
            .retain(|_, &mut end| now.saturating_sub(end) <= crate::events::MAX_EVENT_AGE_MS);
        self.ended.insert(id, end_ts);
    }

    pub fn refund_toast(&mut self, session_id: &str, kind: ToastKind, at_ms: u64) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            if session.last_toast_ms.get(&kind) == Some(&at_ms) {
                session.last_toast_ms.remove(&kind);
            }
        }
    }

    // --- Ask override (R-2.4) ---------------------------------------------

    /// A Quarterdeck ask became pending for this session → force `attention`
    /// (R-2.4). No toast here: the ask subsystem fires its own alert (R-8.4).
    pub fn note_pending_ask(&mut self, session_id: &str) {
        let now = self.clock.now_ms();
        if let Some(s) = self.sessions.get_mut(session_id) {
            s.pending_asks = s.pending_asks.saturating_add(1);
            if s.resettle_effective(now).is_some() {
                s.last_activity_ms = now;
            }
        }
    }

    /// The ask was answered → session resumes working (SPEC §2 "MCP ask answered
    /// → working").
    pub fn note_ask_answered(&mut self, session_id: &str) {
        let now = self.clock.now_ms();
        if let Some(s) = self.sessions.get_mut(session_id) {
            s.pending_asks = s.pending_asks.saturating_sub(1);
            // Two asks can be attributed to one session row (same cwd / basename
            // fallback, `lib.rs` `match_session`). If another ask is still
            // pending, the session remains blocked on a human (R-2.4) — do NOT
            // resume it to working; only the LAST answer does (§2 "MCP ask
            // answered → working"). The still-shown `attention` is left intact.
            if s.pending_asks > 0 {
                return;
            }
            // A liveness poll may have marked this session `dead` (its `claude`
            // process is gone) while the ask was still answerable shell-side.
            // Answering must NOT resurrect a dead process to `working` (a phantom
            // green/yellow row + a wrong worst-of tray change, self-corrected only
            // on the next tick). Leave it dead (the override is already cleared);
            // a genuine new turn will re-create the row via SessionStart.
            if s.hook_status == Status::Dead {
                return;
            }
            s.attention_from_hook = false;
            s.last_notification_ms = None;
            s.hook_status = Status::Working;
            s.dead_since_ms = None;
            s.effective_status = Status::Working;
            s.entered_at_ms = now;
            s.last_activity_ms = now;
        }
    }

    /// The ask timed out or was dismissed → drop one pending ask; status
    /// recomputes from the last hook state only once the LAST pending ask for the
    /// session resolves (R-2.4).
    pub fn note_ask_cleared(&mut self, session_id: &str) {
        let now = self.clock.now_ms();
        if let Some(s) = self.sessions.get_mut(session_id) {
            s.pending_asks = s.pending_asks.saturating_sub(1);
            if s.resettle_effective(now).is_some() {
                s.last_activity_ms = now;
            }
        }
    }

    // --- Timers (Rust-side, R-3.6) ----------------------------------------

    /// Transcript-driven `→ working` recovery (§2 status table, R-2.2): for a
    /// hook-driven `attention` session, recover to `working` once the transcript
    /// advances ≥2 s past the Notification timestamp; likewise an `idle` session
    /// whose transcript advances ≥2 s past the moment it went idle (a turn
    /// resumed without a `UserPromptSubmit` hook, e.g. resume/compact) is
    /// promoted to `working`. `mtime_of` returns the transcript's current mtime
    /// (epoch ms) — the engine never reads transcripts itself.
    pub fn poll_recovery(&mut self, mut mtime_of: impl FnMut(&str) -> Option<u64>) -> Vec<Effect> {
        let now = self.clock.now_ms();
        for s in self.sessions.values_mut() {
            // A pending ask forces attention (R-2.4); transcript activity must
            // never clear it — only an explicit answer does.
            if s.pending_asks > 0 {
                continue;
            }
            match s.effective() {
                // R-2.2: hook-driven attention → working on transcript advance.
                Status::Attention if s.attention_from_hook => {
                    let (Some(tp), Some(notif)) =
                        (s.transcript_path.as_deref(), s.last_notification_ms)
                    else {
                        continue;
                    };
                    if let Some(mtime) = mtime_of(tp) {
                        if mtime >= notif + RECOVERY_MIN_ADVANCE_MS {
                            s.attention_from_hook = false;
                            s.last_notification_ms = None;
                            s.set_hook_status(Status::Working, now);
                        }
                    }
                }
                // §2 table "transcript activity while idle → working": anchored on
                // the instant the session entered `idle` so the write that
                // produced the preceding `Stop` cannot immediately re-promote it.
                Status::Idle => {
                    let Some(tp) = s.transcript_path.as_deref() else {
                        continue;
                    };
                    let anchor = s.entered_at_ms;
                    if let Some(mtime) = mtime_of(tp) {
                        if mtime >= anchor + RECOVERY_MIN_ADVANCE_MS {
                            s.set_hook_status(Status::Working, now);
                        }
                    }
                }
                _ => {}
            }
        }
        Vec::new()
    }

    /// Liveness poll (R-6): mark PID-backed sessions whose process is gone/renamed
    /// and PID-less sessions whose transcript is stale as `dead`.
    pub fn poll_liveness(
        &mut self,
        procs: &impl ProcessTable,
        mut mtime_of: impl FnMut(&str) -> Option<u64>,
    ) -> Vec<Effect> {
        let now = self.clock.now_ms();
        for s in self.sessions.values_mut() {
            if s.effective() == Status::Dead {
                continue;
            }
            let mtime = s.transcript_path.as_deref().and_then(&mut mtime_of);
            let input = liveness::LivenessInput {
                claude_pid: s.claude_pid,
                transcript_mtime_ms: mtime,
            };
            if liveness::is_dead(&input, procs, now) {
                s.attention_from_hook = false;
                s.last_notification_ms = None;
                s.hook_status = Status::Dead;
                // Dead overrides even a pending ask: the process is gone.
                s.pending_asks = 0;
                s.effective_status = Status::Dead;
                s.entered_at_ms = now;
                s.dead_since_ms = Some(now);
            }
        }
        Vec::new()
    }

    /// Remove `dead` rows that have lingered past [`DEAD_RETENTION_MS`] (R-2.5).
    /// Returns the removed session ids.
    pub fn prune_dead(&mut self) -> Vec<String> {
        let now = self.clock.now_ms();
        let expired: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| {
                s.effective() == Status::Dead
                    && s.dead_since_ms
                        .is_some_and(|d| now.saturating_sub(d) >= DEAD_RETENTION_MS)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired {
            self.sessions.remove(id);
        }
        expired
    }

    /// Convenience: run recovery, then liveness, then prune — the shell's 10 s
    /// tick (R-3.6). `mtime_of` is shared by recovery and liveness.
    pub fn tick(
        &mut self,
        procs: &impl ProcessTable,
        mut mtime_of: impl FnMut(&str) -> Option<u64>,
    ) -> Vec<Effect> {
        let mut effects = self.poll_recovery(&mut mtime_of);
        effects.extend(self.poll_liveness(procs, &mut mtime_of));
        self.prune_dead();
        effects
    }

    // --- Cold-start discovery merge (R-5.4) -------------------------------

    /// Insert an inferred session discovered at cold start (R-5.4). No-op if the
    /// session is already tracked.
    #[allow(clippy::too_many_arguments)]
    pub fn add_inferred(
        &mut self,
        id: String,
        cwd: Option<String>,
        transcript_path: Option<String>,
        status: Status,
        title: String,
        activity_ms: u64,
    ) {
        // Never resurrect a cleanly-ended session (R-2.5): if its SessionEnd was
        // replayed this session, cold-start discovery must not re-infer a row
        // from the still-present transcript file.
        if self.sessions.contains_key(&id) || self.ended.contains_key(&id) {
            return;
        }
        let now = self.clock.now_ms();
        let mut s = Session::new(id.clone(), now);
        s.cwd = cwd;
        s.transcript_path = transcript_path;
        s.inferred = true;
        s.hook_status = status;
        s.effective_status = status;
        s.entered_at_ms = now;
        s.last_activity_ms = activity_ms;
        s.display_title = if title.trim().is_empty() {
            naming::NO_TITLE.to_string()
        } else {
            title
        };
        // Cache so we never re-read the transcript just for the title.
        s.transcript_title = Some(s.display_title.clone());
        self.sessions.insert(id, s);
    }

    // --- Projection for the UI / tray -------------------------------------

    /// Full snapshot for the UI, sorted per R-7.3 (attention → working → idle →
    /// dead; within a group, most-recently-active first).
    #[must_use]
    pub fn view(&self) -> Vec<SessionView> {
        let now = self.clock.now_ms();
        let mut rows: Vec<SessionView> = self
            .sessions
            .values()
            .map(|s| SessionView {
                id: s.id.clone(),
                project: s.project(),
                title: s.display_title.clone(),
                branch: s.branch.clone(),
                status: s.effective(),
                inferred: s.inferred,
                since_ms: now.saturating_sub(s.entered_at_ms),
                cwd: s.cwd.clone().unwrap_or_default(),
            })
            .collect();
        rows.sort_by(|a, b| {
            b.status
                .priority()
                .cmp(&a.status.priority())
                .then_with(|| {
                    let ba = self.sessions.get(&b.id).map_or(0, |s| s.last_activity_ms);
                    let aa = self.sessions.get(&a.id).map_or(0, |s| s.last_activity_ms);
                    ba.cmp(&aa)
                })
                .then_with(|| a.id.cmp(&b.id))
        });
        rows
    }

    /// Per-status counts for the footer (R-7.3).
    #[must_use]
    pub fn counts(&self) -> StatusCounts {
        let mut c = StatusCounts::default();
        for s in self.sessions.values() {
            match s.effective() {
                Status::Attention => c.attention += 1,
                Status::Working => c.working += 1,
                Status::Idle => c.idle += 1,
                Status::Dead => c.dead += 1,
            }
        }
        c
    }

    /// Worst status across all sessions for the tray icon (R-2.6); `None` when
    /// there are no sessions (→ neutral/gray icon).
    #[must_use]
    pub fn worst_status(&self) -> Option<Status> {
        self.sessions
            .values()
            .map(Session::effective)
            .max_by_key(|s| s.priority())
    }
}
