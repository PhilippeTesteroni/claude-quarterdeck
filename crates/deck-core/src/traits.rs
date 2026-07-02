//! Public traits implemented by the Tauri shell (`src-tauri`) and by test
//! fakes. Keeping them here lets the engine depend only on these abstractions
//! while `deck-core` stays GUI-free (SPEC R-3.2).

/// Toast copy classes fired by Quarterdeck (SPEC §9, §8, R-2.3, R-9.5).
/// Shared vocabulary between the engine's `Effect::Toast` decisions (T1) and
/// the [`Notifier`] trait (T5) so both sides agree on which copy template
/// and settings toggle (`notifyIdle`/`notifyAttention`/`notifyReminder`,
/// R-9.5/R-10.1) applies. Pure data — no OS dependency, so it lives here
/// alongside the trait rather than in `src-tauri`. `Serialize` is derived so
/// `src-tauri/src/notify.rs` can record it verbatim in the
/// `QUARTERDECK_FAKE_NOTIFIER=1` jsonl trail (R-3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToastKind {
    /// R-9.1 — `Stop`: title `"<project> finished"`, system default sound.
    /// Gated by the `notifyIdle` toggle (default on).
    Idle,
    /// R-9.2 — `attention` from `permission_prompt`/`elicitation_dialog`:
    /// title `"<project> needs you"`, distinct alert sound. Gated by the
    /// `notifyAttention` toggle (default on).
    Attention,
    /// R-8.4 — a pending agent question: title `"<project> asks:
    /// <question…>"`. Reuses the `Attention` alert sound ("same channel as
    /// R-9.2") but per R-9.4 ("Ask toasts never suppressed") is exempt from
    /// both throttling and popup-visible suppression, and from the
    /// `notifyAttention` toggle (asks are the core interactive channel, not
    /// an informational ping).
    Ask,
    /// R-2.3 — an `idle_prompt` notification while already `idle`: the
    /// optional "still waiting" nudge, title `"<project> still waiting"`,
    /// same (non-alert) sound as `Idle`. Gated by the `notifyReminder`
    /// toggle (default **off**, R-9.5).
    Reminder,
}

/// Fires native notifications. The real implementation
/// (`src-tauri/src/notify.rs::DesktopNotifier`) lives in `src-tauri`; tests
/// provide a fake that records calls.
///
/// The engine (T1) never calls a `Notifier` directly — it emits
/// `crate::engine::Effect::Toast` decisions that the shell turns into calls
/// on this trait, translating `detail` per `kind` (session task title for
/// `Idle`, notification message for `Attention`, question text for `Ask`).
pub trait Notifier {
    /// Fires (or throttles/suppresses per R-9.4) a toast for `session_id`.
    /// `popup_visible_and_focused` lets the implementation apply the R-9.4
    /// suppression rule. Returns whether a toast was actually sent.
    fn notify(
        &self,
        kind: ToastKind,
        session_id: &str,
        project: &str,
        detail: &str,
        popup_visible_and_focused: bool,
    ) -> bool;
}

/// Injectable wall-clock time source so status transitions and timers are
/// deterministically testable (SPEC §2, R-2.5 reference an "injectable clock").
///
/// The engine reads "now" exclusively through this trait; tests inject a fake
/// that advances on demand, the shell injects [`crate::engine::SystemClock`].
pub trait Clock {
    /// Current wall-clock time in milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;
}

/// Abstraction over the OS process table used for liveness checks (SPEC §6,
/// R-6.1). The real implementation is backed by `sysinfo` in the shell; tests
/// provide a fake table.
pub trait ProcessTable {
    /// Executable/process name for `pid` (e.g. `"node.exe"`), or `None` when no
    /// live process with that PID exists. Liveness treats `None` as "gone".
    fn process_name(&self, pid: u32) -> Option<String>;
}
