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
    /// Epoch milliseconds when the ask times out, if a timeout was set.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub timeout_at: Option<u64>,
    /// The raw `context` (agent cwd) the MCP call carried, needed verbatim for
    /// the R-8.2 unmatched-ask display "Unknown agent (<context>)". Present only
    /// when the ask could not be matched to a known session (mirrors
    /// `AskRow.context` in `ui/src/ipc-contract.ts`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub context: Option<String>,
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
    /// Agent-questions (MCP) enabled, R-8.6.
    pub mcp_enabled: bool,
    pub data_dir: String,
    pub version: String,
}

/// The full application state pushed to the frontend on every change.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateSnapshot {
    pub sessions: Vec<SessionRow>,
    pub asks: Vec<AskRow>,
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

/// Persists an ask answer to `<dir>/<askId>.json` atomically (SPEC R-8.7).
pub fn write_answer_file(
    dir: &std::path::Path,
    ask_id: &str,
    answer: &str,
    kind: AskAnswerKind,
) -> Result<(), String> {
    let stem = safe_file_stem(ask_id)?;
    let record = AnswerRecord {
        id: stem,
        answer,
        kind,
        answered_at_ms: now_ms(),
    };
    let json = serde_json::to_vec_pretty(&record).map_err(|err| err.to_string())?;
    crate::settings::atomic_write(&dir.join(format!("{stem}.json")), &json)
        .map_err(|err| err.to_string())
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
    let snapshot = {
        let mut guard = state.0.lock().map_err(|err| err.to_string())?;
        apply_remove_row(&mut guard, &session_id);
        guard.clone()
    };
    emit_state(&app, &snapshot)
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
                    timeout_at: None,
                    context: None,
                },
                AskRow {
                    id: "ask-2".to_string(),
                    session_id: None,
                    project: None,
                    question: "Also proceed?".to_string(),
                    options: None,
                    timeout_at: None,
                    context: None,
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
    fn write_answer_file_rejects_unsafe_ids() {
        let dir = std::env::temp_dir().join("quarterdeck-ipc-test-answers-unsafe");
        assert!(write_answer_file(&dir, "../escape", "x", AskAnswerKind::Text).is_err());
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
