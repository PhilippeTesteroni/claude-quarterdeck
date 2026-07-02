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

/// The full application state pushed to the frontend on every change.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateSnapshot {
    pub sessions: Vec<SessionRow>,
    pub asks: Vec<AskRow>,
    pub hooks_installed: bool,
    pub counts: Counts,
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
