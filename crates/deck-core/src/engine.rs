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
use crate::registry::RegistryEntry;
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

/// Registry busy-override freshness window (R-21.1): a hook-idle session is
/// displayed as `working` only while the live registry reports it `busy` with an
/// `updatedAt` no older than this. Beyond it the registry signal is stale and
/// the override clears.
pub const REGISTRY_BUSY_FRESH_MS: u64 = 30_000;

/// Session status (SPEC §2). `dead` sessions linger for [`DEAD_RETENTION_MS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    Working,
    /// §43 "waiting for workflow" (blue, `#58a6ff`): the parent turn's `Stop`
    /// fired (hook `idle`) but background subagents/workflows are still running
    /// (`active_subagents > 0`). Distinct from green `idle` (nothing running) and
    /// yellow `working` (the parent turn itself is executing). A pending
    /// ask/perm (attention) and the §21 registry busy-override both outrank it.
    WaitingWorkflow,
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
            Status::WaitingWorkflow => "waiting",
            Status::Attention => "attention",
            Status::Idle => "idle",
            Status::Dead => "dead",
        }
    }

    /// Sort/aggregation priority: attention worst, then working,
    /// waiting-workflow (§43, between working and idle), idle, dead (R-7.3 sort
    /// order and R-2.6 worst-of aggregation).
    #[must_use]
    pub fn priority(self) -> u8 {
        match self {
            Status::Attention => 4,
            Status::Working => 3,
            Status::WaitingWorkflow => 2,
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
    /// Active background subagents for the `⛭ N` badge (R-21.2); 0 hides it.
    pub subagents: u32,
    /// Total session age in ms when an anchor is known (R-22.3): registry
    /// `startedAt` → `SessionStart` receivedAt → first-seen. Drives the tooltip
    /// "session 2h 14m" line alongside the cwd.
    pub age_ms: Option<u64>,
    /// §36 working-time timer anchor: epoch ms the current turn's real work
    /// started (a `UserPromptSubmit`), or `None`. While the row is `working` the
    /// UI shows a live counter of `now − work_started_ms` ("Xm Ys") instead of
    /// the raw time-in-status — anchored at the prompt, so §30 reverse-gear /
    /// §21 busy-override flips don't reset it.
    pub work_started_ms: Option<u64>,
    /// §36: total working time of the just-finished turn, frozen at its `Stop`.
    /// While the row is `idle` the UI shows "took <this>" instead of a running
    /// idle timer; `None` until a started turn has stopped, cleared on the next
    /// prompt.
    pub last_work_ms: Option<u64>,
    /// §38 kill-agent-process: the session's nearest-ancestor Claude PID (R-4.3 /
    /// R-15.3), when known. Exposed so the shell's `kill_session` command can
    /// resolve and force-terminate it; the UI shows the "Kill process" context
    /// item only for a row that carries one.
    pub pid: Option<u32>,
}

/// Per-status counts (engine-side mirror of `ipc::Counts`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatusCounts {
    pub attention: u32,
    pub working: u32,
    /// §43 blue "waiting for workflow": hook-idle parent with open background
    /// subagents/workflows (`WaitingWorkflow`).
    pub waiting: u32,
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
    /// Timestamp of the most recent `SessionStart` for THIS row incarnation, or
    /// 0 for a row first materialized by some other event (Stop/Notification/
    /// prompt/cold-start). Anchors the reordered-`SessionEnd` guard (R-2.5): an
    /// End older than this row's start belongs to a *previous* incarnation of a
    /// reused id and must be ignored, but a normal Stop-then-End reorder (whose
    /// End post-dates the SessionStart) must still remove the row.
    started_at_ms: u64,
    session_title: Option<String>,
    latest_prompt: Option<String>,
    /// The live registry `name` matched by sessionId (R-15.2), refreshed on
    /// every registry poll so a mid-session `/rename` shows up within ≤10 s. Its
    /// §34 precedence rung depends on [`Session::registry_name_is_user`].
    registry_name: Option<String>,
    /// Whether [`Session::registry_name`] was set by an explicit Claude-side
    /// `/rename` (registry `nameSource == "user"`, R-34). A user-set name
    /// outranks the transcript `aiTitle`; a derived `phily-XX` handle loses to
    /// it. Set by [`SessionStore::apply_registry_entry`].
    registry_name_is_user: bool,
    /// The transcript `aiTitle` (§34, R-34): the terminal-tab chat name Claude
    /// derives for the conversation. The DEFAULT row title — it wins over the
    /// derived `phily-XX` registry handle so a Quarterdeck row matches its
    /// terminal tab. Refreshed by the shell's mtime-gated tail read via
    /// [`SessionStore::set_ai_title`]. Below an override / user `/rename`.
    ai_title: Option<String>,
    /// Cached cold-start transcript title so we read the file at most once.
    transcript_title: Option<String>,
    /// Highest-precedence title: a user-set override (R-27.1), typed by
    /// double-clicking the row name. Seeded on session create from
    /// [`SessionStore::overrides`] and wins over the registry `name` in
    /// [`Session::recompute_title`]. `None`/blank falls back to the normal chain.
    override_name: Option<String>,
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
    /// Raw registry busy flag from the last poll that mentioned this session
    /// (R-21.1): `true` iff the registry `status` mapped to `working`/busy.
    /// Combined with [`Session::registry_updated_at_ms`] freshness to derive
    /// [`Session::busy_override`], recomputed every tick so the override ages out
    /// even on a tick where the registry poll was empty/skipped.
    registry_busy: bool,
    /// The registry `updatedAt` (epoch ms) from the last poll that mentioned this
    /// session — the freshness clock for the busy-override (R-21.1) AND the §44
    /// registry-driven demote (R-44).
    registry_updated_at_ms: Option<u64>,
    /// §44 (R-44): the last registry poll that mentioned this session reported it
    /// with an EXPLICIT quiescent status (`idle`/`waiting`) — an authoritative
    /// "the turn ended / was interrupted" signal from Claude Code, distinct from a
    /// missing/unknown status. Combined with the `registry_updated_at_ms`
    /// freshness and a stuck `hook_status == Working` (an ESC-interrupt fires no
    /// Stop hook, so the hook status wedges `working`) to drive the registry-driven
    /// demote. Cleared when the registry reports the session busy or drops it from
    /// the poll.
    registry_quiescent: bool,
    /// Registry busy-override state (R-21.1), derived from `registry_busy` +
    /// `registry_updated_at_ms` freshness. While set AND the hook-derived status
    /// is `idle`, the row displays `working` (background subagents/workflows are
    /// running though the turn's `Stop` fired). Attention always outranks it; it
    /// clears when the registry reports non-busy or the `updatedAt` goes stale.
    busy_override: bool,
    /// Active background subagents (R-21.2): incremented on `SubagentStart`,
    /// decremented on `SubagentStop`, and SELF-CORRECTING — reset to 0 whenever
    /// the session settles to a non-`working` state (fresh `Stop` with a stale
    /// registry, attention, dead) or the registry reports it non-busy, so a lost
    /// `SubagentStop` can never wedge the `⛭ N` badge on.
    active_subagents: u32,
    /// Registry `startedAt` (epoch ms), highest-precedence session-age anchor
    /// (R-22.3), refreshed from the registry poll.
    registry_started_ms: Option<u64>,
    /// When this row was first materialized (clock now at [`Session::new`]) —
    /// the "first-seen" fallback anchor for session age (R-22.3) when neither a
    /// registry `startedAt` nor a `SessionStart` receivedAt is known.
    created_ms: u64,
    /// `working` was reached via a transcript-recovery promote in `poll_recovery`
    /// (R-30.1 reverse gear), not a real hook. While set, `poll_recovery` keeps
    /// the row `working` only as long as the transcript mtime keeps advancing,
    /// and DEMOTES it back to `idle` on the first tick with no advance. Cleared by
    /// [`Session::set_hook_status`] so any real hook event (SessionStart / prompt /
    /// Stop / ask / dead) drops the reverse gear.
    recovery_promoted: bool,
    /// Transcript mtime (epoch ms) observed at the last recovery promote/hold —
    /// the reference the next `poll_recovery` tick compares against to decide
    /// whether the transcript advanced (R-30.1). Cleared with `recovery_promoted`.
    last_transcript_mtime_ms: Option<u64>,
    /// §45 (R-45): the most recent transcript mtime the shell has reported for
    /// this row, refreshed every poll by [`SessionStore::refresh_transcript_activity`]
    /// (never cleared by a hook event — it tracks the FILE, not the status). This
    /// is the transcript-quiescence reference the §44 registry-demote consults: a
    /// genuinely-working agent writes its transcript continuously, so its mtime
    /// sits within `RECOVERY_MIN_ADVANCE_MS` of `now` and must never be demoted by
    /// a transient mid-turn registry `waiting`; an ESC-interrupt stops the writes,
    /// so the mtime goes stale and the demote fires. `None` until the first poll
    /// reports one (a fresh read failure preserves the last-seen value rather than
    /// clobbering it, so a transient miss can't spoof quiescence).
    seen_transcript_mtime_ms: Option<u64>,
    /// §36 working-time timer: epoch ms of the real work start for the current
    /// turn — set on a `UserPromptSubmit` (the genuine "user hit enter" moment)
    /// and NOT restamped by the §30 reverse-gear promote/demote or a §21
    /// busy-override toggle, so the live "working" counter measures time on THIS
    /// task, not time-in-status. Reset (re-anchored) by the next prompt. `None`
    /// for a row that never saw a real prompt (cold-start/discovered), which then
    /// falls back to the plain time-in-status display.
    work_started_ms: Option<u64>,
    /// §36: total working time of the just-finished turn, frozen at the `Stop`
    /// that ended it (`stop_ts − work_started_ms`). While the row is idle the UI
    /// shows "took <this>" instead of a running idle timer; cleared on the next
    /// `UserPromptSubmit`. `None` until a turn with a known start has stopped.
    last_work_ms: Option<u64>,
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
            started_at_ms: 0,
            session_title: None,
            latest_prompt: None,
            registry_name: None,
            registry_name_is_user: false,
            ai_title: None,
            transcript_title: None,
            override_name: None,
            display_title: naming::NO_TITLE.to_string(),
            inferred: false,
            branch: None,
            pending_asks: 0,
            attention_from_hook: false,
            last_notification_ms: None,
            dead_since_ms: None,
            last_toast_ms: HashMap::new(),
            registry_busy: false,
            registry_updated_at_ms: None,
            registry_quiescent: false,
            busy_override: false,
            active_subagents: 0,
            registry_started_ms: None,
            created_ms: now_ms,
            recovery_promoted: false,
            last_transcript_mtime_ms: None,
            seen_transcript_mtime_ms: None,
            work_started_ms: None,
            last_work_ms: None,
        }
    }

    fn effective(&self) -> Status {
        if self.pending_asks > 0 {
            // Attention (pending ask/perm) always outranks the busy-override
            // (R-21.1) and the §43 waiting-workflow blue.
            return Status::Attention;
        }
        // R-21.1: a hook-idle session the registry says is busy (fresh) displays
        // `working`. Only `idle` is overridden — a hook `attention`/`dead` never is.
        if self.busy_override && self.hook_status == Status::Idle {
            return Status::Working;
        }
        // §43: the parent turn's `Stop` fired (hook `idle`) but background
        // subagents/workflows are still open → blue `WaitingWorkflow` instead of
        // green `idle`. Below the busy-override (a fresh-busy registry means the
        // parent itself is active → yellow), above plain idle.
        if self.active_subagents > 0 && self.hook_status == Status::Idle {
            return Status::WaitingWorkflow;
        }
        self.hook_status
    }

    /// Recompute [`Session::busy_override`] from the raw registry flag + the
    /// `updatedAt` freshness at `now` (R-21.1), re-settling the shown status and
    /// the subagent badge when it flips. Returns whether the displayed status
    /// changed. Runs both on a registry poll and on every tick, so the override
    /// ages out to stale even when a tick's registry read was empty/skipped.
    fn recompute_busy_override(&mut self, now: u64) -> bool {
        let fresh = self
            .registry_updated_at_ms
            .is_some_and(|u| now.saturating_sub(u) < REGISTRY_BUSY_FRESH_MS);
        // §43 / R-21.2: the subagent counter (and the blue `WaitingWorkflow` it
        // drives) is owned by the `SubagentStart`/`SubagentStop` balance and MUST
        // survive a clean `Stop → idle` while children are still open — that was
        // the §43 bug (the old blanket "zero on any non-working settle" made the
        // multi-agent indicator vanish the instant the parent hit `Stop`). Only
        // an AUTHORITATIVE "no background work" signal reaps a leaked count here:
        // a FRESH registry poll that explicitly reports the session non-busy. An
        // absent/stale registry, or a mere hook-idle, leaves the balance intact.
        // A genuinely lost `SubagentStop` is instead reaped by liveness `dead`
        // (which zeroes directly). Cleared BEFORE re-deriving the shown status so
        // `resettle_effective` below sees the settled counter.
        if fresh && !self.registry_busy {
            self.active_subagents = 0;
        }
        self.busy_override = self.registry_busy && fresh;
        // R-22.1: a discovered (inferred) row that the override holds INTO
        // `working` at cold start did NOT enter `working` at app-launch `now`.
        // Its status-entry timestamp must stay seeded from the activity estimate
        // (`entered_at_ms`: transcript mtime, later walked to the registry
        // `updatedAt` by `seed_inferred_entered_at`), never restamped to `now` —
        // that is the exact §22 dishonesty (a `~0s` "just now" on background work
        // whose registry `updatedAt` is seconds old). Clamp defends against a
        // future-dated estimate. For a hook-tracked row, or when the override is
        // not what puts us into `working`, the transition genuinely happens now.
        let settle_ts = if self.inferred && self.busy_override {
            self.entered_at_ms.min(now)
        } else {
            now
        };
        // `resettle_effective` only restamps when the shown status actually
        // changed, so calling it unconditionally is safe (and now necessary: the
        // subagent clear above can flip a row out of blue `WaitingWorkflow`
        // without any busy-override edge).
        if self.resettle_effective(settle_ts).is_some() {
            self.last_activity_ms = now;
            true
        } else {
            false
        }
    }

    /// §44 (R-44) registry-driven demote. Interrupting Claude with ESC fires no
    /// Stop hook, so `hook_status` wedges on `working` and the deck stays stuck
    /// 🟡 — but Claude Code writes the session's registry status to `idle`/
    /// `waiting`. When the registry AUTHORITATIVELY reports the session quiescent
    /// on a FRESH poll while our hook status is still `working` and no real hook
    /// event has landed since that registry write, drop the hook status to `idle`.
    /// This is the demote-side complement to §30's transcript-quiescence demote
    /// (which owns recovery-promoted rows) — Quarterdeck otherwise only ever uses
    /// the registry to OVERRIDE idle→working (R-21.1), never to demote. Guards:
    /// - only a genuine hook `working` is demoted — never a §30 recovery-promoted
    ///   row (that one is the transcript-quiescence demote's job) nor an
    ///   override/blue row (whose hook status is already `idle`);
    /// - a busy registry never reaches here (`registry_quiescent` is false), so
    ///   this never fights a genuinely-busy registry;
    /// - the registry `updatedAt` must be FRESH and strictly AFTER the last hook
    ///   activity, so a new `UserPromptSubmit` that re-armed `working` after the
    ///   registry went idle is never clobbered ("no fresher hook activity");
    /// - §45 (R-45): the transcript must be QUIESCENT — its last-seen mtime (kept
    ///   fresh by `refresh_transcript_activity`) must NOT have advanced within
    ///   `RECOVERY_MIN_ADVANCE_MS` of `now`. Claude Code writes the registry status
    ///   to `waiting` MID-TURN (waiting on a tool/permission), not only when the
    ///   turn ends, so a transient `waiting` would otherwise false-demote a busy
    ///   agent to idle ("finished") and §30 would immediately re-promote it — a
    ///   🟡→🟢→🟡 flap plus a spurious "finished" toast. A genuinely-working agent
    ///   writes its transcript continuously (mtime ≈ `now`), so it is never
    ///   demoted; an ESC-interrupt stops the writes, so the mtime goes stale and
    ///   the demote still fires. A never-seen (`None`) or vanished mtime reads as
    ///   quiescent, matching the §30 reverse-gear demote on a gone transcript.
    ///
    /// Returns whether the shown status changed. Not a real `Stop`, so — like the
    /// §30 reverse-gear demote — it does NOT freeze the §36 working-time timer.
    fn maybe_registry_demote(&mut self, now: u64) -> bool {
        if self.hook_status != Status::Working || self.recovery_promoted || !self.registry_quiescent
        {
            return false;
        }
        // §45/R-45: never demote while the transcript is still advancing — that is
        // an agent actively working through a mid-turn `waiting`, not a finished
        // (or ESC-interrupted) one.
        let transcript_quiescent = self
            .seen_transcript_mtime_ms
            .is_none_or(|mtime| now.saturating_sub(mtime) >= RECOVERY_MIN_ADVANCE_MS);
        if !transcript_quiescent {
            return false;
        }
        let Some(updated) = self.registry_updated_at_ms else {
            return false;
        };
        let fresh = now.saturating_sub(updated) < REGISTRY_BUSY_FRESH_MS;
        if fresh && updated > self.last_activity_ms {
            self.set_hook_status(Status::Idle, now).is_some()
        } else {
            false
        }
    }

    /// Best-known session start anchor for the age tooltip (R-22.3): registry
    /// `startedAt` → `SessionStart` receivedAt → first-seen (row creation).
    ///
    /// Whatever the source, the anchor is clamped to be no more recent than
    /// `entered_at_ms`: a session cannot have entered its current status BEFORE
    /// it was born, so age (`now - anchor`) must always be ≥ time-in-status
    /// (`now - entered_at`). This matters for a discovered/inferred row seeded
    /// (R-22.1) from a past transcript mtime whose only age anchor is the
    /// app-launch `created_ms` (first-seen): without the clamp the tooltip could
    /// read "session just now" beside a "~12m" time-in-status — an age younger
    /// than the current status, which is logically impossible. The clamp is a
    /// no-op for the normal case (session birth precedes the current status).
    fn age_anchor_ms(&self) -> u64 {
        let anchor = if let Some(started) = self.registry_started_ms {
            started
        } else if self.started_at_ms > 0 {
            self.started_at_ms
        } else {
            self.created_ms
        };
        anchor.min(self.entered_at_ms)
    }

    /// Set the hook-derived status; returns `Some(new_effective)` iff the
    /// effective (shown) status changed as a result.
    fn set_hook_status(&mut self, new: Status, ts_ms: u64) -> Option<Status> {
        // R-30.1: any real hook event (or a recovery demote) drops the reverse
        // gear. `poll_recovery` re-sets these AFTER its own `set_hook_status` call.
        self.recovery_promoted = false;
        self.last_transcript_mtime_ms = None;
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
        // §34: the single registry `name` occupies exactly one precedence rung —
        // the user-set slot (above `aiTitle`) when it came from a Claude `/rename`,
        // else the derived slot (below `aiTitle`) for the `phily-XX` handle (R-34).
        let (user_registry, derived_registry) = if self.registry_name_is_user {
            (self.registry_name.as_deref(), None)
        } else {
            (None, self.registry_name.as_deref())
        };
        self.display_title = naming::title_full(
            self.override_name.as_deref(),
            user_registry,
            self.ai_title.as_deref(),
            derived_registry,
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
    /// User title overrides (R-27.1), keyed by session id — the persisted layer
    /// behind `<data>/session-names.json`. Seeded at startup (`set_overrides`),
    /// updated by `set_override_name`, and pruned when a session ends so a reused
    /// id never inherits a stale name (R-27.6). A live [`Session`] mirrors its own
    /// entry in `override_name`; this map is what a freshly (re)materialized row
    /// reads to seed itself.
    overrides: HashMap<String, String>,
    /// Set whenever `overrides` is mutated by a rename or an end-of-session prune,
    /// so the shell knows to re-persist `<data>/session-names.json` on its next
    /// tick (R-27.3/R-27.6). Cleared by [`SessionStore::take_overrides_dirty`].
    /// Not set by [`SessionStore::set_overrides`] (that IS the on-disk state).
    overrides_dirty: bool,
    /// Session ids that just ended (`SessionEnd`, R-2.5) or died (liveness turned
    /// them `dead`, R-6) and whose pending asks/perms the shell must now dismiss
    /// (R-32.2): the agent that raised them is gone and can never receive an
    /// answer. Accumulated here and drained by [`SessionStore::take_gone_sessions`]
    /// on the shell's tick, so `deck-core` stays free of the ask/perm channels.
    gone_sessions: Vec<String>,
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
            overrides: HashMap::new(),
            overrides_dirty: false,
            gone_sessions: Vec::new(),
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

    /// Active background-subagent count for a session (R-21.2 `⛭ N` badge).
    #[must_use]
    pub fn subagents_of(&self, session_id: &str) -> Option<u32> {
        self.sessions.get(session_id).map(|s| s.active_subagents)
    }

    /// Milliseconds the session has spent in its current status (R-2.5).
    #[must_use]
    pub fn since_ms_of(&self, session_id: &str) -> Option<u64> {
        let now = self.clock.now_ms();
        self.sessions
            .get(session_id)
            .map(|s| now.saturating_sub(s.entered_at_ms))
    }

    /// §36: epoch ms the session's current turn started working (the last real
    /// `UserPromptSubmit`), or `None`. For tests/tools.
    #[must_use]
    pub fn work_started_ms_of(&self, session_id: &str) -> Option<u64> {
        self.sessions
            .get(session_id)
            .and_then(|s| s.work_started_ms)
    }

    /// §36: frozen total working time of the just-finished turn, or `None`. For
    /// tests/tools.
    #[must_use]
    pub fn last_work_ms_of(&self, session_id: &str) -> Option<u64> {
        self.sessions.get(session_id).and_then(|s| s.last_work_ms)
    }

    /// Rendered title of a session (for tests/tools).
    #[must_use]
    pub fn title_of(&self, session_id: &str) -> Option<String> {
        self.sessions
            .get(session_id)
            .map(|s| s.display_title.clone())
    }

    // --- User title overrides (§27) ---------------------------------------

    /// Seed the persisted user overrides at startup (R-27.3). Replaces the whole
    /// map; does NOT mark it dirty (this IS the on-disk state). Any override for
    /// an already-tracked session is applied to its live row too, so seeding after
    /// a replay still wins.
    pub fn set_overrides(&mut self, overrides: HashMap<String, String>) {
        self.overrides = overrides;
        for (id, name) in &self.overrides {
            if let Some(s) = self.sessions.get_mut(id) {
                s.override_name = Some(name.clone());
                s.recompute_title();
            }
        }
    }

    /// Set (or clear, with a `None`/blank name) the user title override for a
    /// session (R-27.2). Updates the persisted-overrides map AND the live row,
    /// re-derives the title (so the finished-toast body also reflects a rename),
    /// and marks the map dirty for re-persistence. The name rides
    /// [`naming::normalize_title`] (bidi-strip + 60-grapheme cap, R-27.7); an
    /// empty/whitespace name clears the override (R-27.4). Returns whether the
    /// row's `display_title` changed.
    pub fn set_override_name(&mut self, session_id: &str, name: Option<String>) -> bool {
        let normalized = name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(naming::normalize_title);
        let map_changed = match &normalized {
            Some(n) => {
                self.overrides
                    .insert(session_id.to_string(), n.clone())
                    .as_deref()
                    != Some(n.as_str())
            }
            None => self.overrides.remove(session_id).is_some(),
        };
        if map_changed {
            self.overrides_dirty = true;
        }
        if let Some(s) = self.sessions.get_mut(session_id) {
            s.override_name = normalized;
            let before = s.display_title.clone();
            s.recompute_title();
            s.display_title != before
        } else {
            false
        }
    }

    /// The current user override for a session, if any (tests/tools).
    #[must_use]
    pub fn override_name_of(&self, session_id: &str) -> Option<String> {
        self.overrides.get(session_id).cloned()
    }

    /// A clone of the whole overrides map for persistence (R-27.3).
    #[must_use]
    pub fn overrides_snapshot(&self) -> HashMap<String, String> {
        self.overrides.clone()
    }

    /// Whether the overrides map changed since the last call, clearing the flag
    /// (R-27.3/R-27.6). The shell checks this each tick and re-persists when set.
    pub fn take_overrides_dirty(&mut self) -> bool {
        std::mem::take(&mut self.overrides_dirty)
    }

    /// Drain the ids of sessions that ended (`SessionEnd`) or died (liveness)
    /// since the last call (R-32.2). The shell dispatches each to cancel that
    /// session's pending asks (`kind:"cancelled"`) and drop its pending perms —
    /// the agent that raised them is gone, so no answer could ever reach it.
    #[must_use]
    pub fn take_gone_sessions(&mut self) -> Vec<String> {
        std::mem::take(&mut self.gone_sessions)
    }

    // --- Core reducer ------------------------------------------------------

    /// Apply one parsed spool event, returning any toast decisions (R-9.1,
    /// R-9.2, R-2.3), each already burst-throttled (R-9.4).
    pub fn on_event(&mut self, ev: &SpoolEvent) -> Vec<Effect> {
        let ts = ev.received_at_ms.unwrap_or_else(|| self.clock.now_ms());

        // SessionEnd removes the row immediately, any reason (R-2.5, R-5.1), and
        // tombstones the id so a reordered trailing event can't resurrect it.
        if let HookEvent::SessionEnd { .. } = ev.kind {
            // Guard the mirror image of the tombstone check below: an *older*
            // SessionEnd applied AFTER a genuinely-newer same-id SessionStart must
            // not wipe the freshly (re)created row. The live ingest burst
            // (watcher.rs flush drains a HashSet in nondeterministic order) can
            // deliver a reused-id SessionStart(ts=tr+n) before a SessionEnd(ts=tr)
            // that coalesced into the same debounce window; applying the stale End
            // unconditionally would delete the just-recreated row AND tombstone the
            // id at the older ts, dropping every subsequent event for the live
            // session. Ignore only an End strictly older than THIS row's start
            // (`started_at_ms`) — i.e. one that belongs to a previous incarnation of
            // a reused id. Keying on the start rather than the last-applied event is
            // what keeps a normal Stop-then-End reorder (a later-stamped Stop landing
            // before the genuine End, both post-dating the SessionStart) from being
            // mistaken for a restart and dropping the real End (R-2.5 "SessionEnd
            // always wins immediately").
            if let Some(session) = self.sessions.get(&ev.session_id) {
                if session.started_at_ms > ts {
                    tracing::debug!(
                        session_id = %ev.session_id,
                        end_ts = ts,
                        started_at_ms = session.started_at_ms,
                        "ignoring stale reordered SessionEnd (older than this row's SessionStart)"
                    );
                    return Vec::new();
                }
            }
            self.sessions.remove(&ev.session_id);
            // R-27.6: drop any user override for an ended session so a reused id
            // can't inherit a stale name and the persisted file stays bounded.
            // Flag it so the shell re-persists `<data>/session-names.json`.
            if self.overrides.remove(&ev.session_id).is_some() {
                self.overrides_dirty = true;
            }
            // R-32.2: the agent is gone — the shell must cancel its pending asks
            // and drop its pending perms (nothing can answer them now).
            self.gone_sessions.push(ev.session_id.clone());
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

        // R-27.1: a freshly (re)materialized row inherits any persisted user
        // override for its id (computed before the mutable `entry` borrow; only
        // consumed when the entry is actually vacant).
        let seed_override = self.overrides.get(&ev.session_id).cloned();
        let session = self
            .sessions
            .entry(ev.session_id.clone())
            .or_insert_with(|| {
                let mut s = Session::new(ev.session_id.clone(), ts);
                s.override_name = seed_override;
                s
            });

        // Common payload fields update whenever present (never blanked by absence).
        if let Some(cwd) = &ev.cwd {
            session.cwd = Some(cwd.clone());
        }
        if let Some(tp) = &ev.transcript_path {
            session.transcript_path = Some(tp.clone());
        }
        session.last_activity_ms = ts;

        // R-22.2: whether this row is still a cold-start estimate. The first
        // genuine status-marking hook event upgrades it to exact tracking below.
        let was_inferred = session.inferred;

        let mut effect: Option<(ToastDecision, u64)> = None;

        match &ev.kind {
            HookEvent::SessionStart { session_title, .. } => {
                // Anchor the reordered-SessionEnd guard (R-2.5) on this incarnation's
                // start, so a later End for an earlier same-id session can't wipe it.
                session.started_at_ms = ts;
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
                // §36: a real prompt is the genuine work-start of a new turn —
                // (re-)anchor the working-time timer here and drop the frozen
                // "took" from the previous turn.
                session.work_started_ms = Some(ts);
                session.last_work_ms = None;
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
                        // R-21.2: the session is now blocked on a human (attention),
                        // not working — no subagents should still be counted.
                        session.active_subagents = 0;
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
                        // is already `idle` via Stop) and fires no toast. §47: the
                        // "still waiting" reminder is retired — it landed right after
                        // the Stop "finished" toast and only ever duplicated it, so
                        // `idle_prompt` is now inert (may return later as a delayed
                        // nudge, R-9.5).
                        tracing::debug!(session_id = %session.id, "idle_prompt: no status change, no toast (R-2.3, §47)");
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
                // Capture whether the hook status genuinely transitioned INTO
                // idle (a real turn finished), independent of what the row will
                // *display*: with a live busy-override the effective status can
                // stay `working` across this Stop (R-21.3).
                let finished_a_turn = session.hook_status != Status::Idle;
                session.set_hook_status(Status::Idle, ts);
                // §36: freeze the working-time timer of the turn that just ended —
                // the row now shows "took <this>" instead of a running idle timer,
                // until the next prompt re-anchors. Only when a real work-start is
                // known (a discovered/never-prompted row keeps `None` and falls
                // back to the plain time-in-status display). Not restamped by the
                // §30 reverse-gear demote, which is not a real Stop.
                if let Some(started) = session.work_started_ms {
                    session.last_work_ms = Some(ts.saturating_sub(started));
                }
                // §43: a fresh Stop no longer zeroes the subagent counter — if
                // background subagents/workflows are still open the row settles to
                // blue `WaitingWorkflow` (via `effective`), not green idle, and the
                // multi-agent indicator stays. The count is reaped by a genuine
                // `SubagentStop` balance, a fresh registry non-busy poll
                // (`recompute_busy_override`), or liveness `dead`.
                // R-9.1 / R-21.3: fire the "finished" toast whenever the turn
                // finished and the session isn't still blocked on a pending ask —
                // EVEN if the busy-override immediately keeps the row displayed as
                // working (the turn DID finish; the user may still want to know).
                if finished_a_turn && session.pending_asks == 0 {
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
            HookEvent::SubagentStart => {
                // R-21.2: a background subagent/workflow child started. Bump the
                // per-session counter for the `⛭ N` badge. Does NOT change status
                // (the registry busy-override drives `working`, R-21.1).
                session.active_subagents = session.active_subagents.saturating_add(1);
            }
            HookEvent::SubagentStop => {
                // R-21.2: a subagent child finished. Saturating so a stray/extra
                // stop can't underflow; a LOST stop is reaped by a fresh registry
                // non-busy poll (`recompute_busy_override`) or liveness `dead`.
                session.active_subagents = session.active_subagents.saturating_sub(1);
            }
            HookEvent::SessionEnd { .. } => unreachable!("handled above"),
            HookEvent::Unknown { name } => {
                tracing::debug!(session_id = %session.id, event = %name, "unknown hook event ignored (R-4.5)");
                session.recompute_title();
            }
        }

        // R-22.2: the first genuine status-marking hook event upgrades a
        // discovered (estimated) row to exact tracking — drop the inferred `~`
        // and re-stamp the status-entry from the event's real receivedAt from
        // here on. Non-status events (idle_prompt, subagent, unknown) leave the
        // estimate in place until a real transition lands.
        if was_inferred {
            let status_marker = matches!(
                ev.kind,
                HookEvent::SessionStart { .. }
                    | HookEvent::UserPromptSubmit { .. }
                    | HookEvent::Stop
            ) || (matches!(ev.kind, HookEvent::Notification { .. })
                && session.attention_from_hook);
            if status_marker {
                session.inferred = false;
                session.entered_at_ms = ts;
                session.last_activity_ms = ts;
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
            // A hook-derived permission/elicitation attention can be live at the
            // same time as the ask we just answered — two agents sharing this cwd
            // (R-8.2 `match_session` attributes an ask by cwd), where the OTHER
            // agent independently hit a `permission_prompt`. That prompt is a
            // separate block the human still owes a decision, so answering the ask
            // must NOT flip the row to Working. Re-derive from the last hook state
            // instead (symmetric with `note_ask_cleared`): the row stays Attention
            // and R-2.2 transcript recovery + R-2.2 self-heal remain armed
            // (`attention_from_hook` / `last_notification_ms` preserved), rather
            // than being wiped so the row is stuck yellow until the next hook event.
            if s.attention_from_hook {
                if s.resettle_effective(now).is_some() {
                    s.last_activity_ms = now;
                }
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
                            // R-30.1 reverse gear: arm the transcript-mtime watch
                            // so a stalled recovered turn demotes back to idle.
                            s.recovery_promoted = true;
                            s.last_transcript_mtime_ms = Some(mtime);
                            tracing::debug!(
                                session_id = %s.id,
                                entered_at = s.entered_at_ms,
                                mtime,
                                now,
                                "transcript recovery promote"
                            );
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
                            // R-30.1 reverse gear: arm the transcript-mtime watch
                            // so a stalled recovered turn demotes back to idle.
                            s.recovery_promoted = true;
                            s.last_transcript_mtime_ms = Some(mtime);
                            tracing::debug!(
                                session_id = %s.id,
                                entered_at = s.entered_at_ms,
                                mtime,
                                now,
                                "transcript recovery promote"
                            );
                        }
                    }
                }
                // R-30.1 reverse gear: a row promoted to `working` by recovery
                // stays working only while the transcript mtime keeps advancing;
                // the first tick with no advance (or a vanished transcript)
                // demotes it back to `idle`.
                Status::Working if s.recovery_promoted => {
                    let mtime = s.transcript_path.as_deref().and_then(&mut mtime_of);
                    match mtime {
                        Some(m) if Some(m) != s.last_transcript_mtime_ms => {
                            s.last_transcript_mtime_ms = Some(m);
                        }
                        _ => {
                            s.set_hook_status(Status::Idle, now);
                            tracing::debug!(
                                session_id = %s.id,
                                entered_at = s.entered_at_ms,
                                mtime,
                                now,
                                "transcript recovery demote"
                            );
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
        // R-32.2: ids that transition to `dead` on this poll, reported to the
        // shell (via `gone_sessions`) so it dismisses their pending asks/perms.
        // Collected in the loop and appended after it — the loop holds a mutable
        // borrow of `self.sessions`, so `self.gone_sessions` can't be touched
        // inside it.
        let mut newly_dead = Vec::new();
        for s in self.sessions.values_mut() {
            if s.effective() == Status::Dead {
                continue;
            }
            let mtime = s.transcript_path.as_deref().and_then(&mut mtime_of);
            let input = liveness::LivenessInput {
                claude_pid: s.claude_pid,
                transcript_mtime_ms: mtime,
                // R-15.3: a registry-discovered PID-less row with no transcript
                // yet stays alive on the registry file's freshness, not dead on
                // the next tick. Cleared to `None` by `apply_registry` once the
                // registry entry vanishes.
                registry_updated_at_ms: s.registry_updated_at_ms,
            };
            if liveness::is_dead(&input, procs, now) {
                s.attention_from_hook = false;
                s.last_notification_ms = None;
                s.hook_status = Status::Dead;
                // Dead overrides even a pending ask: the process is gone.
                s.pending_asks = 0;
                // A dead process runs nothing — clear the busy-override and its
                // subagent badge (R-21.1/R-21.2) so a stale registry file can't
                // keep a gone session showing `working`.
                s.busy_override = false;
                s.active_subagents = 0;
                s.effective_status = Status::Dead;
                s.entered_at_ms = now;
                s.dead_since_ms = Some(now);
                newly_dead.push(s.id.clone());
            }
        }
        self.gone_sessions.extend(newly_dead);
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

    /// §45 (R-45): refresh each row's last-seen transcript mtime — the
    /// transcript-quiescence reference the §44 registry-demote consults. The
    /// engine never touches transcripts itself; `mtime_of` returns the file's
    /// current mtime (epoch ms). A read miss (`None`) PRESERVES the last-seen
    /// value rather than clobbering it, so a transient failure can't spoof
    /// quiescence and false-demote a busy row. The shell calls this on every
    /// poll BEFORE `apply_registry` (so the registry-poll demote sees fresh
    /// activity), and [`SessionStore::tick`] calls it before `poll_busy_override`.
    pub fn refresh_transcript_activity(&mut self, mut mtime_of: impl FnMut(&str) -> Option<u64>) {
        for s in self.sessions.values_mut() {
            if let Some(mtime) = s.transcript_path.as_deref().and_then(&mut mtime_of) {
                s.seen_transcript_mtime_ms = Some(mtime);
            }
        }
    }

    /// Age out stale busy-overrides (R-21.1) from each session's stored raw
    /// registry state, independent of whether this tick's registry poll ran or
    /// was empty. Without this, a registry that goes entirely empty (so the
    /// shell skips `apply_registry`) would leave a session wedged displaying
    /// `working` past the freshness window.
    pub fn poll_busy_override(&mut self) {
        let now = self.clock.now_ms();
        for s in self.sessions.values_mut() {
            s.recompute_busy_override(now);
            // §44/R-44: age the registry-driven demote on the plain tick too, so a
            // registry idle/waiting seen on the last poll still unsticks an
            // ESC-wedged `working` even on a tick where no registry poll ran.
            s.maybe_registry_demote(now);
        }
    }

    /// Convenience: age busy-overrides, then run recovery, liveness, and prune —
    /// the shell's 10 s tick (R-3.6). `mtime_of` is shared by recovery and
    /// liveness.
    pub fn tick(
        &mut self,
        procs: &impl ProcessTable,
        mut mtime_of: impl FnMut(&str) -> Option<u64>,
    ) -> Vec<Effect> {
        // §45/R-45: refresh transcript activity first so `poll_busy_override`'s
        // §44 registry-demote gates on this tick's transcript quiescence.
        self.refresh_transcript_activity(&mut mtime_of);
        self.poll_busy_override();
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
        // R-22.1: a discovery-created row seeds its status-entry timestamp from
        // the caller's best activity estimate (transcript mtime, or registry
        // `updatedAt`), NOT app-launch "now" — so its time-in-status reflects
        // when the agent actually entered that status. The registry-vs-transcript
        // precedence is applied by `seed_inferred_entered_at` at cold start.
        s.entered_at_ms = activity_ms;
        s.last_activity_ms = activity_ms;
        let base = if title.trim().is_empty() {
            naming::NO_TITLE.to_string()
        } else {
            title
        };
        // R-27.1: a discovered row also inherits a persisted user override, which
        // wins over the transcript-derived title.
        s.override_name = self.overrides.get(&id).cloned();
        s.display_title =
            naming::title_with_override(s.override_name.as_deref(), None, None, None, Some(&base));
        // Cache the transcript-derived title (NOT the override) so we never re-read
        // the file, and clearing the override falls back to it.
        s.transcript_title = Some(base);
        self.sessions.insert(id, s);
    }

    /// Re-seed a discovered (inferred) row's status-entry timestamp (R-22.1
    /// precedence: registry `updatedAt`, matched by sessionId, from a **fresh
    /// file**, outranks the transcript mtime it was first seeded with). No-op for
    /// a hook-tracked row (whose times are exact) or an unknown session. Called
    /// once at cold start, never on the periodic poll (which would keep resetting
    /// a live row's timer).
    ///
    /// R-22.1's parenthetical "fresh file" qualifier is enforced here: the
    /// registry `updatedAt` only wins when it is at least as fresh as the
    /// transcript-mtime estimate the row already carries (`seed_ms >=
    /// entered_at_ms`). A STALE registry `updatedAt` (older than the transcript's
    /// last activity) must NOT drag the row backwards — that would inflate
    /// time-in-status past reality, exactly the dishonest time §22 exists to
    /// remove. In that case the fresher transcript mtime stands.
    pub fn seed_inferred_entered_at(&mut self, session_id: &str, seed_ms: u64) {
        if let Some(s) = self.sessions.get_mut(session_id) {
            if s.inferred && seed_ms >= s.entered_at_ms {
                s.entered_at_ms = seed_ms;
                if seed_ms > s.last_activity_ms {
                    s.last_activity_ms = seed_ms;
                }
            }
        }
    }

    // --- Live registry poll (§15) -----------------------------------------

    /// Set (or clear) the transcript `aiTitle` for a session (§34, R-34): the
    /// terminal-tab chat name and the DEFAULT row title (it wins over the derived
    /// `phily-XX` registry handle so a row matches its terminal tab). The shell
    /// feeds this from its mtime-gated transcript tail read. Mirrors the
    /// registry-name update path: stores the trimmed value (normalization runs in
    /// [`Session::recompute_title`] via `pick_title`), re-derives the title, and
    /// returns whether the row's `display_title` changed. No-op (returns `false`)
    /// for an unknown session or an unchanged value.
    pub fn set_ai_title(&mut self, session_id: &str, ai_title: Option<String>) -> bool {
        let Some(s) = self.sessions.get_mut(session_id) else {
            return false;
        };
        let normalized = ai_title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        if s.ai_title == normalized {
            return false;
        }
        s.ai_title = normalized;
        let before = s.display_title.clone();
        s.recompute_title();
        s.display_title != before
    }

    /// Apply one live-registry entry (R-15.2/R-15.3) to its matching row (by
    /// session id): refresh the registry `name` (highest-precedence title) and
    /// feed the registry pid into liveness. No-op for an unknown session.
    /// Returns whether anything the UI would show changed.
    pub fn apply_registry_entry(&mut self, entry: &RegistryEntry) -> bool {
        let now = self.clock.now_ms();
        let Some(s) = self.sessions.get_mut(&entry.session_id) else {
            return false;
        };
        let mut changed = false;
        // Registry pid feeds liveness directly (R-15.3) — no ancestor walk
        // needed for a registry-known session. `claude_pid` is what liveness
        // reads; keep the newest non-zero pid the registry reports.
        if let Some(pid) = entry.pid {
            if s.claude_pid != Some(pid) {
                s.claude_pid = Some(pid);
            }
        }
        // R-22.3: remember the registry `startedAt` as the top-precedence
        // session-age anchor.
        if let Some(started) = entry.started_at_ms {
            s.registry_started_ms = Some(started);
        }
        // R-21.1 busy-override: store the raw registry busy flag + `updatedAt`,
        // then derive the override (busy AND fresh) via the shared helper — which
        // also re-settles the shown status (R-21.3 tray follows the displayed
        // status, no toast) and the subagent badge (R-21.2). The display effect
        // only bites while the hook status is `idle` (handled in `effective`);
        // attention always outranks.
        s.registry_busy = matches!(
            crate::registry::registry_status_to_engine(entry.status.as_deref()),
            Status::Working
        );
        // §44/R-44: remember whether this poll EXPLICITLY reported idle/waiting
        // (an authoritative quiescent signal), so `maybe_registry_demote` below can
        // unstick an ESC-interrupted `working` hook status. Absent/unknown status
        // is not quiescent (it only reads as non-busy for the override).
        s.registry_quiescent =
            crate::registry::registry_status_is_quiescent(entry.status.as_deref());
        s.registry_updated_at_ms = entry.updated_at_ms;
        changed |= s.recompute_busy_override(now);
        // §44/R-44: a fresh registry idle/waiting demotes a hook status wedged on
        // `working` by an ESC-interrupt (no Stop hook fired). Runs after the
        // busy-override recompute so it sees the settled state; a busy registry
        // never demotes (guarded by `registry_quiescent`).
        changed |= s.maybe_registry_demote(now);
        // R-15.2: the registry `name` refreshes on every poll. Only re-derive
        // the title (which may touch the transcript for a fallback) when the
        // name actually changed, so a /rename lands within one poll but a stable
        // name doesn't churn.
        let new_name = entry
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        // R-34: classify the registry name's origin. `nameSource == "user"` (an
        // explicit Claude `/rename`) routes it above the `aiTitle`; anything else
        // (derived/absent = `phily-XX`) below it. A flip in origin alone — same
        // name string, `derived → user` — changes the title, so it must also
        // trigger a recompute.
        let new_is_user = entry
            .name_source
            .as_deref()
            .map(str::trim)
            .is_some_and(|src| src.eq_ignore_ascii_case("user"));
        if s.registry_name.as_deref() != new_name || s.registry_name_is_user != new_is_user {
            s.registry_name = new_name.map(str::to_string);
            s.registry_name_is_user = new_is_user;
            let before = s.display_title.clone();
            s.recompute_title();
            changed |= s.display_title != before;
        }
        changed
    }

    /// Apply a whole registry poll's worth of entries (R-15.2/R-15.3/R-21.1).
    /// Sessions absent from this poll have their busy-override cleared (their
    /// registry file vanished → no longer busy). Returns whether any row's
    /// displayed state changed.
    pub fn apply_registry(&mut self, entries: &[RegistryEntry]) -> bool {
        let now = self.clock.now_ms();
        let present: HashSet<&str> = entries.iter().map(|e| e.session_id.as_str()).collect();
        let mut changed = false;
        for entry in entries {
            changed |= self.apply_registry_entry(entry);
        }
        // R-21.1: a session no longer reported by the registry can't be "busy" —
        // clear the raw busy flag and re-derive, so a removed registry file
        // doesn't wedge a row displaying `working` forever. Clearing
        // `registry_updated_at_ms` also makes the poll non-authoritative for the
        // §43 subagent clear (`recompute_busy_override` only reaps the counter on
        // a FRESH non-busy poll), so an absent registry leaves a genuinely
        // multi-agent row's counter intact — it is owned by the
        // SubagentStart/Stop balance and reaped only by liveness `dead`.
        for s in self.sessions.values_mut() {
            if present.contains(s.id.as_str()) {
                continue;
            }
            s.registry_busy = false;
            s.registry_quiescent = false;
            s.registry_updated_at_ms = None;
            changed |= s.recompute_busy_override(now);
            // R-15.2: "Registry names refresh on every poll." A session the
            // registry no longer reports is no longer claimed by it, so its
            // registry `name` must not linger — clear it symmetrically with the
            // busy flag above and let the title fall back down the precedence
            // chain (session_title → prompt → transcript). Without this, a live
            // row whose `~/.claude/sessions/<id>.json` vanished while the process
            // is still alive would keep displaying its last registry name forever.
            if s.registry_name.is_some() || s.registry_name_is_user {
                s.registry_name = None;
                s.registry_name_is_user = false;
                let before = s.display_title.clone();
                s.recompute_title();
                changed |= s.display_title != before;
            }
        }
        changed
    }

    /// Per-session terminal PIDs used by foreground-suppression matching
    /// (R-17.2): a session's terminal is whichever process hosts it (the
    /// registry/`claude` pid). Sessions with no known pid are omitted (nothing to
    /// match against the foreground).
    #[must_use]
    pub fn terminal_pids(&self) -> Vec<(String, Vec<u32>)> {
        self.sessions
            .values()
            .filter_map(|s| s.claude_pid.map(|pid| (s.id.clone(), vec![pid])))
            .collect()
    }

    /// Per-session transcript paths (SPEC §23 token reader): `(session_id,
    /// transcript_path)` for every tracked session. The shell's incremental
    /// usage reader iterates these on its tick; `None` transcript paths (a row
    /// that never carried one) are still returned so the caller can prune stale
    /// usage state for gone sessions by intersecting ids.
    #[must_use]
    pub fn session_transcripts(&self) -> Vec<(String, Option<String>)> {
        self.sessions
            .values()
            .map(|s| (s.id.clone(), s.transcript_path.clone()))
            .collect()
    }

    /// The transcript path for one session (SPEC §23 / R-24.1): used to refresh
    /// a session's usage on demand (e.g. right before its finished-toast fires,
    /// so the toast body carries the model's actual last words).
    #[must_use]
    pub fn transcript_path_of(&self, session_id: &str) -> Option<String> {
        self.sessions
            .get(session_id)
            .and_then(|s| s.transcript_path.clone())
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
                subagents: s.active_subagents,
                age_ms: Some(now.saturating_sub(s.age_anchor_ms())),
                work_started_ms: s.work_started_ms,
                last_work_ms: s.last_work_ms,
                pid: s.claude_pid,
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
                Status::WaitingWorkflow => c.waiting += 1,
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
