//! Typed IPC contract between the Rust shell and the frontend.
//!
//! These serde types are the Rust mirror of `ui/src/ipc-contract.ts` and MUST
//! stay in lockstep with it (SPEC R-3.4). The shell pushes a full
//! [`StateSnapshot`] over the `deck://state` event; the frontend sends intent
//! back through the commands documented in the TypeScript contract
//! (`answer_ask`, `remove_row`, `set_setting`, `install_hooks`,
//! `uninstall_hooks`, `get_state`). The command handlers themselves are
//! implemented by T3.

use serde::{Deserialize, Serialize};

/// Tauri event name carrying full state snapshots to the frontend.
pub const STATE_EVENT: &str = "deck://state";

/// A single monitored Claude Code session, as rendered in the popup.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRow {
    pub id: String,
    pub project: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub branch: Option<String>,
    pub status: SessionStatus,
    /// True when the row was inferred at cold start (UI shows a `~` marker).
    pub inferred: bool,
    /// Milliseconds spent in the current status.
    pub since_ms: u64,
    pub cwd: String,
    /// Active background subagents (SPEC R-21.2): the row shows a `⛭ N` badge
    /// while > 0. Defaulted so an older snapshot without the field deserializes.
    #[serde(default)]
    pub subagents: u32,
    /// Total session age in ms when an anchor is known (SPEC R-22.3): shown in
    /// the row tooltip as "session 2h 14m". Omitted when unknown.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub age_ms: Option<u64>,
    /// Context fill percent (SPEC R-23.2a/R-23.4): the row's second line shows
    /// `ctx {ctxPercent}% · …`, amber ≥75, red ≥90. Omitted until a usage
    /// record has been read (or when `showTokenStats` is off).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ctx_percent: Option<u32>,
    /// Session spend, compact (SPEC R-23.2b/R-23.4): the `· {spend}` half of the
    /// row's second line (e.g. `1.4M`). Omitted when zero/unavailable.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub spend: Option<String>,
    /// True when `spend` is a lower bound after a truncation/overflow rescan
    /// (SPEC R-23.1) — the UI renders it as "≥".
    #[serde(default)]
    pub spend_approx: bool,
    /// Combined subagent/sidechain spend, compact (SPEC R-23.3): the `· {spend}`
    /// suffix on the `⛭ N` badge (`⛭ 3 · 2.1M`). Omitted when zero.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub subagent_spend: Option<String>,
}

/// Session status (SPEC §2). Serializes lowercase to match the TS union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Working,
    Attention,
    Idle,
    Dead,
}

/// A pending agent question mirrored into the UI (SPEC §8).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskRow {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub project: Option<String>,
    pub question: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub options: Option<Vec<String>>,
    /// Multi-question / multi-select form (SPEC §29, R-29.5): when present and
    /// non-empty, the ask window renders a form of these blocks (radio/checkbox
    /// per `multiSelect`) instead of the single-question options, and the popup
    /// mirror shows "N questions — Answer in window". Defaulted/optional so an
    /// older snapshot without the field still deserializes (R-29.6).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub questions: Option<Vec<deck_core::ask::AskQuestion>>,
    /// Long rationale/body (R-19.1), rendered muted under the question. Absent
    /// for asks that carried no `detail`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub detail: Option<String>,
    /// Epoch milliseconds when the ask times out, if a timeout was set. Absent
    /// for persistent asks (R-19.2) — the UI then shows no countdown.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub timeout_at: Option<u64>,
    /// The raw `context` (agent cwd) the MCP call carried, needed verbatim for
    /// the R-8.2 unmatched-ask display "Unknown agent (<context>)". Present only
    /// when the ask could not be matched to a known session (mirrors
    /// `AskRow.context` in `ui/src/ipc-contract.ts`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub context: Option<String>,
    /// R-8.7: true when this ask was recovered from disk after a restart — it can
    /// never be answered (its MCP connection is gone), so the UI shows it as
    /// expired with only a Dismiss action.
    #[serde(default)]
    pub orphaned: bool,
    /// Epoch ms the ask was enqueued (arrival time). The shared ask/perm FIFO
    /// (R-16.2): the ask window's primary slot goes to whichever of the front
    /// ask / front perm has the smaller `queued_at`. Defaulted so an older
    /// snapshot without the field still deserializes.
    #[serde(default)]
    pub queued_at: u64,
}

/// A pending permission request mirrored into the UI (SPEC §16, R-16.2). Shares
/// the always-on-top ask window with [`AskRow`] but renders distinctly (amber)
/// with Allow / Deny / In terminal actions. Mirror of `PermRow` in
/// `ui/src/ipc-contract.ts`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermRow {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub project: Option<String>,
    /// The tool Claude Code is asking permission to run (e.g. `Bash`), sanitized.
    pub tool_name: String,
    /// Compact pretty-printed tool input, sanitized (bidi-stripped) and capped
    /// (R-16.1 2KB / R-16.5), rendered verbatim under the tool name.
    pub tool_input: String,
    /// Raw calling context (agent cwd) for an unmatched perm, shown verbatim
    /// like the unmatched-ask label. Present only when unmatched.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub context: Option<String>,
    /// Epoch ms the perm arrived — its position in the shared ask/perm FIFO
    /// (R-16.2). Compared against a front ask's `queued_at`. Defaulted so an
    /// older snapshot without the field still deserializes.
    #[serde(default)]
    pub queued_at: u64,
    /// SPEC R-32.1: epoch ms at which this perm expires — its `PermissionRequest`
    /// hook (90 s timeout, R-16.1) has by then exited, so no deck decision could
    /// reach it. The shell sweeps the perm off the tick past this instant; until
    /// then the UI disables its Allow/Deny buttons. Optional so an older snapshot
    /// without the field still deserializes.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub expires_at: Option<u64>,
}

/// Per-status session counts shown in the footer.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub attention: u32,
    pub working: u32,
    pub idle: u32,
    pub dead: u32,
}

/// Persisted user settings plus the read-only facts the settings pane and
/// onboarding card need (SPEC R-10.1, R-7.4, R-8.6). Mirror of `SettingsState`
/// in `ui/src/ipc-contract.ts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsState {
    pub notify_idle: bool,
    pub notify_attention: bool,
    pub notify_reminder: bool,
    pub launch_at_login: bool,
    pub onboarding_done: bool,
    /// Popup pin-on-top state (SPEC R-14.2), mirrored so the header pin
    /// toggle renders its persisted state on load.
    pub popup_pinned: bool,
    /// Take over permission prompts (SPEC R-16.4), mirrored so the settings
    /// toggle + onboarding consent line render their persisted state.
    pub takeover_permissions: bool,
    /// Show per-session token usage on rows (SPEC R-23.5), mirrored so the
    /// settings toggle renders its persisted state and the UI can hide the row
    /// usage line when off.
    pub show_token_stats: bool,
    /// Popup display mode (SPEC §25, R-25.2): `list` or `lamp`. Drives whether
    /// the popup renders the full list or the compact traffic-light square, and
    /// whether the header's collapse button / pin icon reflect lamp state.
    pub popup_mode: crate::settings::PopupMode,
    /// Agent-questions (MCP) enabled, R-8.6.
    pub mcp_enabled: bool,
    /// R-8.6: whether the `claude` CLI is on PATH. When false, the settings pane
    /// shows `mcp_command` for the user to run manually ("else shows the exact
    /// command to copy").
    pub mcp_cli_available: bool,
    /// R-8.6: the exact `claude mcp add …` command (with the real port + token)
    /// to register the MCP server by hand. `None` until the server is up.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mcp_command: Option<String>,
    pub data_dir: String,
    pub version: String,
}

/// The full application state pushed to the frontend on every change.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateSnapshot {
    pub sessions: Vec<SessionRow>,
    pub asks: Vec<AskRow>,
    /// Pending permission requests (SPEC §16), rendered in the same window as
    /// asks. Defaulted so an older backend/test snapshot without the field still
    /// deserializes.
    #[serde(default)]
    pub perms: Vec<PermRow>,
    pub hooks_installed: bool,
    pub counts: Counts,
    /// Populated by T7 composition; omitted only by an unwired backend, in which
    /// case the UI falls back to safe defaults (onboarding/settings inert).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub settings: Option<SettingsState>,
}

/// The kind of answer produced by an MCP `ask_user` call (mirrored to the UI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AskAnswerKind {
    Option,
    Text,
    Timeout,
    Dismissed,
    /// R-19.5: the ask was cancelled by a `cancel_ask` tool call (from a
    /// parallel tool call / another session) before the user answered.
    Cancelled,
    /// SPEC §29 (R-29.2/R-29.3): the answer to a multi-question / multi-select
    /// form. The `answer` string is a JSON document
    /// `{"answers":[{header,question,selected:[...],text?}, ...]}` carried on the
    /// existing answer channel — the delivery pipe (`AnswerFile`, the oneshot,
    /// `resolve_answer`) is unchanged.
    Form,
}

/// The deck-side decision for a pending permission request (SPEC §16, R-16.2).
/// Serializes lowercase to match the TS union and the perm-answer file the hook
/// polls. `Defer` = "In terminal" / R-16.3 auto-defer: no decision, the hook
/// exits silently so the normal terminal dialog appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermDecision {
    Allow,
    Deny,
    Defer,
}

/// The value carried by the `set_setting` command (TS union `boolean |
/// string`). Untagged so it serializes as a bare JSON bool/string, matching
/// the contract exactly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SettingValue {
    Bool(bool),
    Text(String),
}

impl SettingValue {
    /// Best-effort coercion to `bool` for the known boolean settings
    /// (`notifyIdle`, `launchAtLogin`, ...); a stray string value from a
    /// misbehaving caller degrades to `false` unless it's an obvious truthy
    /// token, rather than panicking or silently dropping the update.
    pub fn as_bool_lossy(&self) -> bool {
        match self {
            SettingValue::Bool(b) => *b,
            SettingValue::Text(s) => matches!(s.as_str(), "true" | "1"),
        }
    }
}

impl From<SettingValue> for serde_json::Value {
    fn from(value: SettingValue) -> Self {
        match value {
            SettingValue::Bool(b) => serde_json::Value::Bool(b),
            SettingValue::Text(s) => serde_json::Value::String(s),
        }
    }
}

/// Shared application state managed by Tauri (`app.manage(AppState::default())`,
/// wired up by T7). Holds the latest [`StateSnapshot`] behind a mutex so
/// command handlers — and, once composed in, the engine — can both read and
/// push updates (SPEC R-3.4: the frontend is dumb, all logic lives in Rust).
pub struct AppState(pub std::sync::Mutex<StateSnapshot>);

impl Default for AppState {
    fn default() -> Self {
        Self(std::sync::Mutex::new(StateSnapshot::default()))
    }
}

/// Recomputes footer counts from the current session rows (SPEC R-7.3 footer,
/// R-2.6 tray worst-of input).
pub fn recompute_counts(sessions: &[SessionRow]) -> Counts {
    let mut counts = Counts::default();
    for session in sessions {
        match session.status {
            SessionStatus::Attention => counts.attention += 1,
            SessionStatus::Working => counts.working += 1,
            SessionStatus::Idle => counts.idle += 1,
            SessionStatus::Dead => counts.dead += 1,
        }
    }
    counts
}

/// Removes a session row by id and recomputes counts (`remove_row` command,
/// right-click "Remove row" per SPEC R-7.2).
pub fn apply_remove_row(state: &mut StateSnapshot, session_id: &str) {
    state.sessions.retain(|session| session.id != session_id);
    state.counts = recompute_counts(&state.sessions);
}

/// Removes a pending ask by id once its answer has been persisted to disk.
pub fn apply_remove_ask(state: &mut StateSnapshot, ask_id: &str) {
    state.asks.retain(|ask| ask.id != ask_id);
}

/// The on-disk shape of a persisted ask answer (SPEC §8, R-8.7): the blocked
/// `ask_user` MCP call (T6) polls `<data>/answers/` for this file.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AnswerRecord<'a> {
    id: &'a str,
    answer: &'a str,
    kind: AskAnswerKind,
    answered_at_ms: u64,
}

/// Rejects ids that would escape `dir`. Ask ids are engine-generated, but the
/// file name is still derived from an IPC argument, so this is defence in
/// depth rather than trust.
fn safe_file_stem(id: &str) -> Result<&str, String> {
    if id.is_empty() || id.contains(['/', '\\']) || id == "." || id == ".." {
        return Err(format!("invalid id: {id:?}"));
    }
    Ok(id)
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// SPEC §29 (R-29.6): backstop cap on the assembled answer string before it is
/// persisted, so a pathologically large form answer (many questions × long
/// free-text) can never write an unbounded file / deliver an unbounded blob to
/// the agent. Well above any real answer (the request-side caps already bound a
/// form to ≤8 questions × ≤12 options); grapheme-based so a multibyte answer is
/// never cut mid-cluster. Applies to every answer kind uniformly.
const ANSWER_MAX_CHARS: usize = 8192;

/// Persists an ask answer to `<dir>/<askId>.json` atomically (SPEC R-8.7).
pub fn write_answer_file(
    dir: &std::path::Path,
    ask_id: &str,
    answer: &str,
    kind: AskAnswerKind,
) -> Result<(), String> {
    let stem = safe_file_stem(ask_id)?;
    // R-29.6: cap the assembled answer string before writing (a no-op for the
    // short single-question answers that never approach the cap).
    let capped = deck_core::naming::truncate_graphemes(answer, ANSWER_MAX_CHARS);
    let record = AnswerRecord {
        id: stem,
        answer: &capped,
        kind,
        answered_at_ms: now_ms(),
    };
    let json = serde_json::to_vec_pretty(&record).map_err(|err| err.to_string())?;
    crate::settings::atomic_write(&dir.join(format!("{stem}.json")), &json)
        .map_err(|err| err.to_string())
}

/// Removes a pending perm by id once its decision has been persisted to disk.
pub fn apply_remove_perm(state: &mut StateSnapshot, perm_id: &str) {
    state.perms.retain(|perm| perm.id != perm_id);
}

/// The on-disk shape of a perm decision (SPEC R-16.1): the blocked
/// `PermissionRequest` hook polls `<data>/perm-answers/<id>.json` for this.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PermAnswerRecord<'a> {
    decision: PermDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
    answered_at_ms: u64,
}

/// Persists a perm decision to `<dir>/<permId>.json` atomically (SPEC R-16.1)
/// for the blocked `PermissionRequest` hook to poll.
pub fn write_perm_answer_file(
    dir: &std::path::Path,
    perm_id: &str,
    decision: PermDecision,
    reason: Option<&str>,
) -> Result<(), String> {
    let stem = safe_file_stem(perm_id)?;
    let record = PermAnswerRecord {
        decision,
        reason: reason.map(str::trim).filter(|r| !r.is_empty()),
        answered_at_ms: now_ms(),
    };
    let json = serde_json::to_vec_pretty(&record).map_err(|err| err.to_string())?;
    crate::settings::atomic_write(&dir.join(format!("{stem}.json")), &json)
        .map_err(|err| err.to_string())
}

/// Answers a pending permission request (`answer_perm` command, SPEC §16): the
/// decision is persisted for the blocked hook to poll, and the deck-side state
/// (pending-perm attention override, mirrored rows, ask window) is updated.
#[tauri::command]
pub fn answer_perm(
    app: tauri::AppHandle,
    perm_id: String,
    decision: PermDecision,
    reason: Option<String>,
) -> Result<(), String> {
    crate::answer_perm_command(&app, &perm_id, decision, reason.as_deref())
}

/// Returns the current snapshot (`get_state` command — also used by the
/// frontend on load, before the first `deck://state` event arrives).
#[tauri::command]
pub fn get_state(state: tauri::State<AppState>) -> StateSnapshot {
    state.0.lock().expect("state mutex poisoned").clone()
}

/// Removes a session row from the popup (`remove_row` command, SPEC R-7.2).
#[tauri::command]
pub fn remove_row(
    state: tauri::State<AppState>,
    app: tauri::AppHandle,
    session_id: String,
) -> Result<(), String> {
    // SPEC R-27.6: drop any user title override for the removed row (engine map +
    // `<data>/session-names.json`) so a reused id never inherits a stale name.
    crate::prune_session_override(&app, &session_id);
    let snapshot = {
        let mut guard = state.0.lock().map_err(|err| err.to_string())?;
        apply_remove_row(&mut guard, &session_id);
        guard.clone()
    };
    emit_state(&app, &snapshot)
}

/// Renames a session by setting a user title override (`rename_session` command,
/// SPEC §27 R-27.4): the new name wins over every other title source (registry
/// name, session title, prompt). An empty/whitespace name clears the override,
/// restoring the normal title chain. Sanitized + capped shell-side (R-27.7).
#[tauri::command]
pub fn rename_session(
    app: tauri::AppHandle,
    session_id: String,
    name: String,
) -> Result<(), String> {
    crate::rename_session_command(&app, &session_id, &name)
}

/// Submits an answer for a pending ask (`answer_ask` command, SPEC §8): the
/// answer is persisted for the blocked MCP call and the row disappears from
/// the popup/ask window.
#[tauri::command]
pub fn answer_ask(
    state: tauri::State<AppState>,
    app: tauri::AppHandle,
    ask_id: String,
    answer: String,
    kind: AskAnswerKind,
) -> Result<(), String> {
    write_answer_file(&crate::settings::answers_dir(), &ask_id, &answer, kind)?;
    let snapshot = {
        let mut guard = state.0.lock().map_err(|err| err.to_string())?;
        apply_remove_ask(&mut guard, &ask_id);
        guard.clone()
    };
    emit_state(&app, &snapshot)
}

/// Persists a settings toggle (`set_setting` command, SPEC R-10.1) and applies
/// its side effect: autostart (R-10.3) for `launchAtLogin`, MCP registration +
/// skill copy (R-8.6) for `mcpEnabled`. Pushes a fresh snapshot so onboarding /
/// settings state propagates to the UI immediately.
#[tauri::command]
pub fn set_setting(app: tauri::AppHandle, key: String, value: SettingValue) -> Result<(), String> {
    let settings = crate::settings::set_setting(&crate::settings::data_dir(), &key, value)
        .map_err(|err| err.to_string())?;
    crate::apply_setting_side_effect(&app, &key, &settings);
    crate::push_state(&app);
    Ok(())
}

/// Installs the Quarterdeck hooks (`install_hooks` command, SPEC R-4.1): copies
/// the scripts to a stable path and merges our entries into the user-level
/// `~/.claude/settings.json` (overridable via `QUARTERDECK_CLAUDE_DIR` for
/// tests). Surfaces any merge/IO error to the UI banner (R-7.6).
#[tauri::command]
pub fn install_hooks(state: tauri::State<AppState>, app: tauri::AppHandle) -> Result<(), String> {
    crate::install_hooks_command(&app)?;
    set_hooks_installed(&state, &app, true)
}

/// Uninstalls the Quarterdeck hooks (`uninstall_hooks` command, SPEC R-4.2):
/// removes exactly the entries whose command carries the `quarterdeck` marker.
#[tauri::command]
pub fn uninstall_hooks(state: tauri::State<AppState>, app: tauri::AppHandle) -> Result<(), String> {
    crate::uninstall_hooks_command()?;
    set_hooks_installed(&state, &app, false)
}

/// Resizes the popup to fit its content within the 460..=560 band (SPEC R-7.1
/// grow-then-scroll). The frontend measures its own content height and calls
/// this; all clamping/anchoring lives in Rust (R-3.4).
#[tauri::command]
pub fn resize_popup(app: tauri::AppHandle, content_height: f64) -> Result<(), String> {
    crate::windows::resize_popup_to_content(&app, content_height)
}

/// Brings the ask window forward without stealing focus (`show_ask_window`
/// command, SPEC R-18.1 "(or via popup mirror click)"): a mirrored ask row in
/// the popup can be clicked to re-surface the ask window after it was closed
/// via its X button while asks are still pending. A no-op if already visible.
#[tauri::command]
pub fn show_ask_window(app: tauri::AppHandle) -> Result<(), String> {
    crate::windows::show_ask_window(&app)
}

/// Focuses the terminal window hosting a session (`focus_terminal` command, SPEC
/// R-15.4): a row click (or the "Focus terminal" context-menu item). Best-effort
/// — returns an error string the UI shows as an inline notice ("Couldn't find
/// the terminal window") when no window could be focused (R-15.4b).
#[tauri::command]
pub fn focus_terminal(app: tauri::AppHandle, session_id: String) -> Result<(), String> {
    crate::focus_terminal_command(&app, &session_id)
}

fn set_hooks_installed(
    state: &tauri::State<AppState>,
    app: &tauri::AppHandle,
    installed: bool,
) -> Result<(), String> {
    let snapshot = {
        let mut guard = state.0.lock().map_err(|err| err.to_string())?;
        guard.hooks_installed = installed;
        guard.clone()
    };
    emit_state(app, &snapshot)
}

fn emit_state(app: &tauri::AppHandle, snapshot: &StateSnapshot) -> Result<(), String> {
    use tauri::Emitter;
    app.emit(STATE_EVENT, snapshot)
        .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_session(id: &str, status: SessionStatus) -> SessionRow {
        SessionRow {
            id: id.to_string(),
            project: "quarterdeck".to_string(),
            title: "test session".to_string(),
            branch: None,
            status,
            inferred: false,
            since_ms: 0,
            cwd: "C:/repo".to_string(),
            subagents: 0,
            age_ms: None,
            ctx_percent: None,
            spend: None,
            spend_approx: false,
            subagent_spend: None,
        }
    }

    #[test]
    fn recompute_counts_tallies_each_status() {
        let sessions = vec![
            sample_session("a", SessionStatus::Attention),
            sample_session("b", SessionStatus::Working),
            sample_session("c", SessionStatus::Working),
            sample_session("d", SessionStatus::Idle),
            sample_session("e", SessionStatus::Dead),
        ];
        let counts = recompute_counts(&sessions);
        assert_eq!(counts.attention, 1);
        assert_eq!(counts.working, 2);
        assert_eq!(counts.idle, 1);
        assert_eq!(counts.dead, 1);
    }

    #[test]
    fn apply_remove_row_drops_the_matching_row_and_recomputes_counts() {
        let sessions = vec![
            sample_session("a", SessionStatus::Attention),
            sample_session("b", SessionStatus::Idle),
        ];
        let counts = recompute_counts(&sessions);
        let mut state = StateSnapshot {
            sessions,
            counts,
            ..Default::default()
        };

        apply_remove_row(&mut state, "a");

        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].id, "b");
        assert_eq!(state.counts.attention, 0);
        assert_eq!(state.counts.idle, 1);
    }

    #[test]
    fn apply_remove_row_is_a_no_op_for_unknown_ids() {
        let mut state = StateSnapshot {
            sessions: vec![sample_session("a", SessionStatus::Idle)],
            ..Default::default()
        };
        apply_remove_row(&mut state, "does-not-exist");
        assert_eq!(state.sessions.len(), 1);
    }

    #[test]
    fn apply_remove_ask_drops_the_matching_ask() {
        let mut state = StateSnapshot {
            asks: vec![
                AskRow {
                    id: "ask-1".to_string(),
                    session_id: None,
                    project: None,
                    question: "Proceed?".to_string(),
                    options: None,
                    questions: None,
                    detail: None,
                    timeout_at: None,
                    context: None,
                    orphaned: false,
                    queued_at: 0,
                },
                AskRow {
                    id: "ask-2".to_string(),
                    session_id: None,
                    project: None,
                    question: "Also proceed?".to_string(),
                    options: None,
                    questions: None,
                    detail: None,
                    timeout_at: None,
                    context: None,
                    orphaned: false,
                    queued_at: 0,
                },
            ],
            ..Default::default()
        };
        apply_remove_ask(&mut state, "ask-1");
        assert_eq!(state.asks.len(), 1);
        assert_eq!(state.asks[0].id, "ask-2");
    }

    #[test]
    fn setting_value_round_trips_as_a_bare_json_scalar() {
        let bool_json = serde_json::to_string(&SettingValue::Bool(true)).unwrap();
        assert_eq!(bool_json, "true");
        let text_json = serde_json::to_string(&SettingValue::Text("x".to_string())).unwrap();
        assert_eq!(text_json, "\"x\"");

        let parsed: SettingValue = serde_json::from_str("false").unwrap();
        assert_eq!(parsed, SettingValue::Bool(false));
        let parsed: SettingValue = serde_json::from_str("\"hello\"").unwrap();
        assert_eq!(parsed, SettingValue::Text("hello".to_string()));
    }

    #[test]
    fn setting_value_as_bool_lossy() {
        assert!(SettingValue::Bool(true).as_bool_lossy());
        assert!(SettingValue::Text("true".to_string()).as_bool_lossy());
        assert!(!SettingValue::Text("nope".to_string()).as_bool_lossy());
    }

    #[test]
    fn safe_file_stem_rejects_path_traversal() {
        assert!(safe_file_stem("ask-123").is_ok());
        assert!(safe_file_stem("../escape").is_err());
        assert!(safe_file_stem("a/b").is_err());
        assert!(safe_file_stem("a\\b").is_err());
        assert!(safe_file_stem("").is_err());
        assert!(safe_file_stem(".").is_err());
        assert!(safe_file_stem("..").is_err());
    }

    #[test]
    fn write_answer_file_persists_valid_json() {
        let dir = std::env::temp_dir().join(format!(
            "quarterdeck-ipc-test-answers-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        write_answer_file(&dir, "ask-42", "yes please", AskAnswerKind::Option).unwrap();

        let contents = std::fs::read_to_string(dir.join("ask-42.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(value["id"], "ask-42");
        assert_eq!(value["answer"], "yes please");
        assert_eq!(value["kind"], "option");
        assert!(value["answeredAtMs"].as_u64().unwrap() > 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn perm_decision_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&PermDecision::Allow).unwrap(),
            "\"allow\""
        );
        assert_eq!(
            serde_json::to_string(&PermDecision::Deny).unwrap(),
            "\"deny\""
        );
        assert_eq!(
            serde_json::to_string(&PermDecision::Defer).unwrap(),
            "\"defer\""
        );
        // The command arg is deserialized from the TS union.
        let d: PermDecision = serde_json::from_str("\"deny\"").unwrap();
        assert_eq!(d, PermDecision::Deny);
    }

    #[test]
    fn write_perm_answer_file_persists_decision_and_reason() {
        // SPEC R-16.1: the hook polls this file. Deny carries the optional reason;
        // an empty/whitespace reason is dropped so the hook emits a bare deny.
        let dir = std::env::temp_dir().join(format!(
            "quarterdeck-perm-answer-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        write_perm_answer_file(&dir, "perm-1", PermDecision::Deny, Some("too risky")).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("perm-1.json")).unwrap())
                .unwrap();
        assert_eq!(v["decision"], "deny");
        assert_eq!(v["reason"], "too risky");
        assert!(v["answeredAtMs"].as_u64().unwrap() > 0);

        write_perm_answer_file(&dir, "perm-2", PermDecision::Allow, Some("   ")).unwrap();
        let v2: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("perm-2.json")).unwrap())
                .unwrap();
        assert_eq!(v2["decision"], "allow");
        assert!(v2.get("reason").is_none(), "blank reason omitted");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_perm_answer_file_rejects_unsafe_ids() {
        let dir = std::env::temp_dir().join("quarterdeck-perm-answer-unsafe");
        assert!(write_perm_answer_file(&dir, "../escape", PermDecision::Defer, None).is_err());
    }

    #[test]
    fn apply_remove_perm_drops_the_matching_perm() {
        let mut state = StateSnapshot {
            perms: vec![
                PermRow {
                    id: "p1".to_string(),
                    session_id: Some("s1".to_string()),
                    project: Some("quarterdeck".to_string()),
                    tool_name: "Bash".to_string(),
                    tool_input: "{}".to_string(),
                    context: None,
                    queued_at: 0,
                    expires_at: None,
                },
                PermRow {
                    id: "p2".to_string(),
                    session_id: None,
                    project: None,
                    tool_name: "Write".to_string(),
                    tool_input: "{}".to_string(),
                    context: Some("C:/x".to_string()),
                    queued_at: 0,
                    expires_at: None,
                },
            ],
            ..Default::default()
        };
        apply_remove_perm(&mut state, "p1");
        assert_eq!(state.perms.len(), 1);
        assert_eq!(state.perms[0].id, "p2");
    }

    #[test]
    fn perm_row_serializes_with_contract_field_names() {
        // Guards the wire shape against `ui/src/ipc-contract.ts` PermRow drift.
        let perm = PermRow {
            id: "p1".to_string(),
            session_id: Some("s1".to_string()),
            project: Some("quarterdeck".to_string()),
            tool_name: "Bash".to_string(),
            tool_input: "{\"command\":\"ls\"}".to_string(),
            context: None,
            queued_at: 0,
            expires_at: Some(90_000),
        };
        let v = serde_json::to_value(&perm).unwrap();
        assert_eq!(v["toolName"], "Bash");
        assert_eq!(v["toolInput"], "{\"command\":\"ls\"}");
        assert_eq!(v["sessionId"], "s1");
        assert_eq!(v["expiresAt"], 90_000);
        assert!(v.get("context").is_none(), "None context omitted");
    }

    #[test]
    fn ask_answer_kind_form_serializes_lowercase() {
        // SPEC §29 (R-29.2): the new `Form` kind is the lowercase `"form"` token
        // on the wire, matching the TS `AskAnswerKind` union.
        assert_eq!(
            serde_json::to_string(&AskAnswerKind::Form).unwrap(),
            "\"form\""
        );
        let k: AskAnswerKind = serde_json::from_str("\"form\"").unwrap();
        assert_eq!(k, AskAnswerKind::Form);
    }

    #[test]
    fn ask_row_serializes_questions_with_camel_case_multi_select() {
        // SPEC §29 (R-29.5): `questions` mirrors to the UI with `multiSelect`
        // camelCase, matching `AskQuestion` in `ui/src/ipc-contract.ts`.
        let ask = AskRow {
            id: "a1".to_string(),
            session_id: None,
            project: None,
            question: "Which environment?".to_string(),
            options: Some(vec!["prod".to_string()]),
            questions: Some(vec![deck_core::ask::AskQuestion {
                header: Some("Env".to_string()),
                question: "Which environment?".to_string(),
                multi_select: true,
                options: vec!["prod".to_string(), "staging".to_string()],
            }]),
            detail: None,
            timeout_at: None,
            context: None,
            orphaned: false,
            queued_at: 0,
        };
        let v = serde_json::to_value(&ask).unwrap();
        assert_eq!(v["questions"][0]["header"], "Env");
        assert_eq!(v["questions"][0]["multiSelect"], true);
        assert_eq!(v["questions"][0]["options"][1], "staging");

        // R-29.6: an old AskRow JSON with no `questions` still deserializes.
        let legacy = serde_json::json!({
            "id": "a2", "question": "q?", "orphaned": false, "queuedAt": 0
        });
        let back: AskRow = serde_json::from_value(legacy).unwrap();
        assert!(back.questions.is_none());
    }

    #[test]
    fn write_answer_file_caps_a_pathologically_large_answer() {
        // SPEC §29 (R-29.6): a huge assembled answer is grapheme-capped before it
        // is written, so it can never persist / deliver an unbounded blob.
        let dir = std::env::temp_dir().join(format!(
            "quarterdeck-answer-cap-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let huge = "z".repeat(50_000);
        write_answer_file(&dir, "ask-big", &huge, AskAnswerKind::Form).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("ask-big.json")).unwrap())
                .unwrap();
        let written = v["answer"].as_str().unwrap();
        assert!(written.chars().count() <= ANSWER_MAX_CHARS, "answer capped");
        assert!(written.ends_with('…'), "cap appends an ellipsis");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_answer_file_rejects_unsafe_ids() {
        let dir = std::env::temp_dir().join("quarterdeck-ipc-test-answers-unsafe");
        assert!(write_answer_file(&dir, "../escape", "x", AskAnswerKind::Text).is_err());
    }

    /// SPEC R-14.2: the pin state mirrors as `popupPinned` (camelCase),
    /// matching `SettingsState` in `ui/src/ipc-contract.ts`.
    #[test]
    fn settings_state_serializes_popup_pinned_camel_case() {
        let settings = SettingsState {
            notify_idle: true,
            notify_attention: true,
            notify_reminder: false,
            launch_at_login: false,
            onboarding_done: true,
            popup_pinned: true,
            takeover_permissions: true,
            show_token_stats: true,
            popup_mode: crate::settings::PopupMode::List,
            mcp_enabled: false,
            mcp_cli_available: true,
            mcp_command: None,
            data_dir: "C:/data".to_string(),
            version: "0.1.0".to_string(),
        };
        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(value["popupPinned"], true);
    }

    /// SPEC R-25.2: `popupMode` mirrors as a lowercase string matching the TS
    /// union (`'list' | 'lamp'`) in `ui/src/ipc-contract.ts`.
    #[test]
    fn settings_state_serializes_popup_mode_as_lowercase_string() {
        let settings = SettingsState {
            notify_idle: true,
            notify_attention: true,
            notify_reminder: false,
            launch_at_login: false,
            onboarding_done: true,
            popup_pinned: true,
            takeover_permissions: true,
            show_token_stats: true,
            popup_mode: crate::settings::PopupMode::Lamp,
            mcp_enabled: false,
            mcp_cli_available: true,
            mcp_command: None,
            data_dir: "C:/data".to_string(),
            version: "0.1.0".to_string(),
        };
        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(value["popupMode"], "lamp");
    }

    /// Guards the wire shape against accidental drift from
    /// `ui/src/ipc-contract.ts` (SPEC R-3.4): every field name below is
    /// exactly what the TypeScript side expects.
    #[test]
    fn state_snapshot_serializes_with_contract_field_names() {
        let mut snapshot = StateSnapshot::default();
        snapshot
            .sessions
            .push(sample_session("s1", SessionStatus::Working));
        snapshot.hooks_installed = true;
        snapshot.counts = recompute_counts(&snapshot.sessions);

        let value = serde_json::to_value(&snapshot).unwrap();
        assert!(value.get("hooksInstalled").is_some());
        assert!(value.get("sessions").is_some());
        assert!(value.get("asks").is_some());
        assert!(value.get("counts").is_some());

        let session = &value["sessions"][0];
        assert_eq!(session["id"], "s1");
        assert_eq!(session["sinceMs"], 0);
        assert_eq!(session["inferred"], false);
        assert_eq!(session["status"], "working");
        // `branch` is omitted entirely when `None` (SPEC: optional row field).
        assert!(session.get("branch").is_none());
    }
}
