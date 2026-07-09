//! MCP server (SPEC §8): a **streamable-HTTP** Model Context Protocol server bound
//! to `127.0.0.1:<port>` that lets Claude Code agents reach the human through
//! Quarterdeck. It serves two tools:
//!
//! * `ask_user(question, options?, context?, detail?, timeout_seconds?)` —
//!   **blocks** until the user answers, the ask times out, or it is
//!   dismissed/cancelled/orphaned. `timeout_seconds` is optional (≤ 3600);
//!   omitted/0 → **persistent** (no expiry, R-19.2). Returns
//!   `{answer, kind, ask_id}` with `kind ∈ option|text|timeout|dismissed|
//!   cancelled` (R-8.1, R-19.5). While blocked and a `progressToken` was sent,
//!   the server streams `notifications/progress` every 30s to keep the call
//!   alive (R-19.3).
//! * `update_ask(ask_id, question?, options?, detail?)` / `cancel_ask(ask_id)` —
//!   mutate / cancel a pending ask from a parallel call (R-19.5).
//! * `notify_user(message, context?)` — fire-and-forget toast; returns
//!   `{delivered, id}` immediately (R-19.6).
//!
//! ## Auth
//! A random bearer token + a stable port are persisted to `<data>/mcp.json`
//! (R-8.1). Every request must carry `Authorization: Bearer <token>`; without it
//! the server answers `401`.
//!
//! ## The T7 seam
//! The transport is deliberately decoupled from the deck engine by the narrow
//! [`AskGateway`] trait. `mcp_server` never touches the engine, the UI, or the
//! ask queue directly — it only calls [`AskGateway::submit_ask`] /
//! [`AskGateway::notify`] and awaits the returned channel. T7 supplies the real
//! engine-backed implementation when composing the app in `lib.rs`; the tests in
//! this module supply a fake so the whole round-trip is exercised without Tauri.
//!
//! ## Orphaning on restart (R-8.7)
//! A freshly started process cannot answer an ask that a *previous* process left
//! pending — its HTTP connection is gone. [`serve`] therefore calls
//! [`AskGateway::orphan_stale_asks`] once at startup so the engine can mark those
//! asks expired instead of "answering into the void". Within a live process, if
//! the gateway drops the answer channel (teardown), `ask_user` returns a tool
//! error rather than hanging.

use std::convert::Infallible;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_stream::wrappers::ReceiverStream;

use crate::ipc::AskAnswerKind;
use deck_core::ask::AskQuestion;
use deck_core::naming::{strip_bidi_controls, truncate_graphemes};

/// The single MCP endpoint path (streamable HTTP transport).
pub const MCP_PATH: &str = "/mcp";
/// MCP protocol revision we advertise in `initialize`.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Cap for an explicit `timeout_seconds` (R-19.2: raised 600 → 3600). Omitted/0
/// means persistent (no expiry), handled separately.
const MAX_TIMEOUT_SECONDS: u64 = 3600;

// SPEC §29 (R-29.6): central caps for a multi-question form, enforced in
// `parse_ask_request` (the flat single-question path has none). Grapheme-based
// so a multibyte header/question/option is never severed mid-cluster.
/// Max questions in one form.
const MAX_QUESTIONS: usize = 8;
/// Max options offered per question.
const MAX_OPTIONS_PER_QUESTION: usize = 12;
/// Grapheme cap on a per-question header.
const MAX_HEADER_CHARS: usize = 60;
/// Grapheme cap on a question's text.
const MAX_QUESTION_CHARS: usize = 200;
/// Grapheme cap on a single option's text.
const MAX_OPTION_CHARS: usize = 100;
/// Keepalive cadence (R-19.3): while an `ask_user` call is blocked and the
/// client sent a `progressToken`, the server emits `notifications/progress`
/// this often to reset Claude Code's idle abort. Overridable (ms) via
/// `QUARTERDECK_MCP_KEEPALIVE_MS` so tests can time-compress it (SPEC §20).
const DEFAULT_KEEPALIVE: Duration = Duration::from_secs(30);

fn keepalive_interval() -> Duration {
    match std::env::var("QUARTERDECK_MCP_KEEPALIVE_MS") {
        Ok(v) => match v.trim().parse::<u64>() {
            Ok(ms) if ms > 0 => Duration::from_millis(ms),
            _ => DEFAULT_KEEPALIVE,
        },
        Err(_) => DEFAULT_KEEPALIVE,
    }
}

// ---------------------------------------------------------------------------
// Data types crossing the T7 seam
// ---------------------------------------------------------------------------

/// A question routed from an agent to the human (R-8.1, R-19.1/19.2).
#[derive(Debug, Clone)]
pub struct AskRequest {
    pub question: String,
    pub options: Option<Vec<String>>,
    /// Multi-question / multi-select form (SPEC §29, R-29.1): when `Some`, the
    /// agent sent a `questions[]` array (each already bidi-stripped + capped by
    /// [`parse_ask_request`]); `question`/`options` then carry a synthesized
    /// single-question fallback for legacy display paths. `None` → a plain
    /// single-question ask.
    pub questions: Option<Vec<AskQuestion>>,
    /// Long rationale/body (R-19.1), already bidi-stripped by the transport.
    pub detail: Option<String>,
    pub context: Option<String>,
    /// `Some(n)` = expires in `n` seconds (already clamped to `1..=3600`);
    /// `None` = persistent (R-19.2), no expiry.
    pub timeout_seconds: Option<u64>,
}

/// The handle the gateway returns from [`AskGateway::submit_ask`]: the
/// engine-generated `ask_id` (surfaced in the `ask_user` result and used by
/// `update_ask`/`cancel_ask`, R-19.5) plus the channel that resolves when the
/// ask is answered/dismissed/cancelled/timed-out.
pub struct SubmittedAsk {
    pub id: String,
    pub rx: oneshot::Receiver<AskAnswer>,
}

/// A fire-and-forget notification from an agent (`notify_user`).
#[derive(Debug, Clone)]
pub struct NotifyRequest {
    pub message: String,
    pub context: Option<String>,
}

/// The resolved answer to an [`AskRequest`]. Serializes to `{answer, kind}` with
/// `kind` lowercased to match the UI contract (`ipc-contract.ts`).
#[derive(Debug, Clone, Serialize)]
pub struct AskAnswer {
    pub answer: String,
    pub kind: AskAnswerKind,
}

impl AskAnswer {
    /// The user picked one of the offered options.
    pub fn option(answer: impl Into<String>) -> Self {
        Self {
            answer: answer.into(),
            kind: AskAnswerKind::Option,
        }
    }
    /// The user typed a free-text answer.
    pub fn text(answer: impl Into<String>) -> Self {
        Self {
            answer: answer.into(),
            kind: AskAnswerKind::Text,
        }
    }
    /// The ask elapsed without an answer.
    pub fn timeout() -> Self {
        Self {
            answer: String::new(),
            kind: AskAnswerKind::Timeout,
        }
    }
    /// The user dismissed the ask.
    pub fn dismissed() -> Self {
        Self {
            answer: String::new(),
            kind: AskAnswerKind::Dismissed,
        }
    }
    /// The ask was cancelled by a `cancel_ask` tool call (R-19.5).
    pub fn cancelled() -> Self {
        Self {
            answer: String::new(),
            kind: AskAnswerKind::Cancelled,
        }
    }
    /// The user submitted a multi-question / multi-select form (SPEC §29,
    /// R-29.3): `answer` is the `{"answers":[...]}` JSON document.
    #[cfg(test)]
    pub fn form(answer: impl Into<String>) -> Self {
        Self {
            answer: answer.into(),
            kind: AskAnswerKind::Form,
        }
    }
}

/// The narrow seam between the MCP transport and the deck engine (see module
/// docs). T7 wires a real, engine-backed implementation; tests use a fake.
pub trait AskGateway: Send + Sync + 'static {
    /// Register a new ask and return its id + a channel that resolves when the
    /// user answers, the ask times out, or it is dismissed/cancelled. Dropping
    /// the sender (engine teardown) surfaces as an orphaned ask on the caller
    /// side.
    fn submit_ask(&self, req: AskRequest) -> SubmittedAsk;

    /// Mutate a PENDING ask in place (R-19.5 `update_ask`): any of
    /// question/options/detail. Returns `false` for an unknown or already-settled
    /// id (the transport turns that into an error result, not an exception).
    fn update_ask(
        &self,
        _ask_id: &str,
        _question: Option<String>,
        _options: Option<Vec<String>>,
        _detail: Option<String>,
    ) -> bool {
        false
    }

    /// Cancel a PENDING ask (R-19.5 `cancel_ask`): resolve the blocked caller
    /// with `kind:"cancelled"` and remove it from the UI. Returns `false` for an
    /// unknown or already-settled id.
    fn cancel_ask(&self, _ask_id: &str) -> bool {
        false
    }

    /// Deliver a fire-and-forget notification toast (R-19.6). Returns the
    /// notification record id (also logged in `notifier-calls.jsonl`).
    fn notify(&self, req: NotifyRequest) -> String;

    /// Called once at server startup: mark any asks left pending by a previous
    /// process as orphaned/expired so late answers are never delivered into the
    /// void (R-8.7). Default: no-op.
    fn orphan_stale_asks(&self) {}
}

// ---------------------------------------------------------------------------
// Persisted config: <data>/mcp.json
// ---------------------------------------------------------------------------

/// The `<data>/mcp.json` payload: the stable port and bearer token (R-8.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    pub port: u16,
    pub token: String,
}

/// Reads the persisted `<data>/mcp.json`, if present and valid. Useful for the
/// settings pane (R-8.6) to build the `claude mcp add ...` command.
pub fn load_config() -> Option<McpConfig> {
    read_config(&data_dir())
}

fn read_config(dir: &Path) -> Option<McpConfig> {
    let raw = std::fs::read_to_string(dir.join("mcp.json")).ok()?;
    let cfg: McpConfig = serde_json::from_str(&raw).ok()?;
    if cfg.token.is_empty() || cfg.port == 0 {
        return None;
    }
    Some(cfg)
}

fn write_config(dir: &Path, cfg: &McpConfig) -> std::io::Result<()> {
    let final_path = dir.join("mcp.json");
    let tmp_path = dir.join("mcp.json.tmp");
    let data = serde_json::to_vec_pretty(cfg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp_path, data)?;
    // Atomic publish (tmp + rename); rename replaces on Windows and Unix.
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

/// The Quarterdeck data root: `QUARTERDECK_DATA_DIR` override, else the OS
/// app-data dir (R-3.3). Kept private and self-contained so the MCP module has
/// no cross-module coupling; T7 may unify this with `settings.rs` later.
fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("QUARTERDECK_DATA_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            if !appdata.is_empty() {
                return PathBuf::from(appdata).join("quarterdeck");
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            if !home.is_empty() {
                return PathBuf::from(home)
                    .join("Library")
                    .join("Application Support")
                    .join("quarterdeck");
            }
        }
    }
    std::env::temp_dir().join("quarterdeck")
}

/// Generates a 48-hex-char (192-bit) bearer token. `RandomState` is seeded from
/// OS entropy per allocation, which is ample for a loopback-only credential.
fn generate_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();

    let mut token = String::with_capacity(64);
    for i in 0..4u64 {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(i);
        h.write_u128(nanos);
        h.write_u32(pid);
        let a = h.finish();

        let mut h2 = RandomState::new().build_hasher();
        h2.write_u64(a);
        h2.write_u64(i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let b = h2.finish();

        token.push_str(&format!("{a:016x}{b:016x}"));
    }
    token.truncate(48);
    token
}

// ---------------------------------------------------------------------------
// Server lifecycle
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AppState {
    token: Arc<String>,
    gateway: Arc<dyn AskGateway>,
    /// R-19.3 keepalive cadence for the SSE `ask_user` path. Resolved once at
    /// bind time (`keepalive_interval`), carried here so the request path never
    /// re-reads the env (and tests can inject a short interval without a
    /// process-global env var).
    keepalive: Duration,
}

/// A running MCP server. Query [`port`](Self::port)/[`token`](Self::token) to
/// build the `claude mcp add` command; call [`shutdown`](Self::shutdown) to stop
/// it gracefully (used in tests and on app exit).
pub struct ServerHandle {
    port: u16,
    token: String,
    shutdown: Option<oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<()>,
}

impl ServerHandle {
    /// The bound loopback port.
    pub fn port(&self) -> u16 {
        self.port
    }
    /// The bearer token clients must present.
    pub fn token(&self) -> &str {
        &self.token
    }
    /// The full endpoint URL agents connect to.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}{}", self.port, MCP_PATH)
    }
    /// Signals graceful shutdown and awaits the server task.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = self.join.await;
    }
}

/// Binds the MCP server, persists `<data>/mcp.json`, orphans stale asks (R-8.7),
/// and spawns the accept loop. Returns immediately with a [`ServerHandle`].
///
/// Port policy (R-8.1, "random-stable"): reuse the persisted port when free,
/// otherwise fall back to an OS-assigned port and re-persist. The bearer token
/// is generated once and then reused across restarts.
pub async fn serve(gateway: Arc<dyn AskGateway>) -> std::io::Result<ServerHandle> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;

    let existing = read_config(&dir);
    let token = existing
        .as_ref()
        .map(|c| c.token.clone())
        .unwrap_or_else(generate_token);

    let listener = match existing.as_ref() {
        Some(cfg) => match bind_local(cfg.port).await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(port = cfg.port, error = %e, "persisted MCP port unavailable; choosing a new one");
                bind_local(0).await?
            }
        },
        None => bind_local(0).await?,
    };
    let port = listener.local_addr()?.port();
    write_config(
        &dir,
        &McpConfig {
            port,
            token: token.clone(),
        },
    )?;

    // R-8.7: a fresh process cannot answer asks the previous run left pending.
    gateway.orphan_stale_asks();

    tracing::info!(port, "MCP server listening on 127.0.0.1");
    Ok(spawn_server(listener, port, token, gateway))
}

async fn bind_local(port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await
}

fn spawn_server(
    listener: TcpListener,
    port: u16,
    token: String,
    gateway: Arc<dyn AskGateway>,
) -> ServerHandle {
    let state = AppState {
        token: Arc::new(token.clone()),
        gateway,
        keepalive: keepalive_interval(),
    };
    let router = build_router(state);
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let join = tokio::spawn(async move {
        let served =
            axum::serve(listener, router.into_make_service()).with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            });
        if let Err(e) = served.await {
            tracing::error!(error = %e, "MCP server exited with error");
        }
    });
    ServerHandle {
        port,
        token,
        shutdown: Some(shutdown_tx),
        join,
    }
}

fn build_router(state: AppState) -> Router {
    Router::new()
        .route(MCP_PATH, post(handle_post).get(handle_get))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------

/// GET on the MCP endpoint would open a server→client SSE stream. Quarterdeck
/// never initiates messages, so we decline it. Per R-8.1 ("requests without the
/// token → 401") the bearer check runs first, so an unauthenticated GET gets a
/// 401 before the 405; an authenticated GET gets the spec-permitted 405.
async fn handle_get(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&headers, state.token.as_str()) {
        return unauthorized();
    }
    (StatusCode::METHOD_NOT_ALLOWED, "SSE stream not supported").into_response()
}

/// The streamable-HTTP POST entry point. A JSON-RPC *request* (has `id`) gets a
/// JSON response; a *notification* (no `id`) is acknowledged with `202`.
async fn handle_post(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if !authorized(&headers, state.token.as_str()) {
        return unauthorized();
    }

    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &rpc_error(Value::Null, -32700, "Parse error"),
            )
        }
    };
    if !msg.is_object() {
        return json_response(
            StatusCode::BAD_REQUEST,
            &rpc_error(
                Value::Null,
                -32600,
                "Invalid Request (batching is not supported)",
            ),
        );
    }

    let method = msg
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);

    match msg.get("id").cloned() {
        // Notification: no response body, just acknowledge.
        None => {
            tracing::debug!(method = %method, "mcp notification");
            StatusCode::ACCEPTED.into_response()
        }
        Some(id) => {
            // R-19.3 keepalive: a blocking `ask_user` for which the client sent a
            // `progressToken` is answered over an SSE stream so the server can
            // interleave `notifications/progress` while it waits. Every other
            // request (and an `ask_user` with no progressToken) gets a single
            // JSON response, unchanged.
            if method == "tools/call" && tool_name(&params) == "ask_user" {
                if let Some(token) = progress_token(&params) {
                    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
                    return ask_user_sse(&state, id, &args, token);
                }
            }
            let response = dispatch(&state, &method, id, params).await;
            json_response(StatusCode::OK, &response)
        }
    }
}

/// The tool name of a `tools/call` request, or `""`.
fn tool_name(params: &Value) -> &str {
    params.get("name").and_then(Value::as_str).unwrap_or("")
}

/// The request's `params._meta.progressToken` (string or number), if the client
/// opted into progress notifications (R-19.3). Kept as a raw JSON value so it is
/// echoed back verbatim in every `notifications/progress`.
fn progress_token(params: &Value) -> Option<Value> {
    params
        .get("_meta")
        .and_then(|m| m.get("progressToken"))
        .filter(|t| !t.is_null())
        .cloned()
}

async fn dispatch(state: &AppState, method: &str, id: Value, params: Value) -> Value {
    match method {
        "initialize" => rpc_result(id, initialize_result()),
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, tools_list_result()),
        "tools/call" => call_tool(state, id, params).await,
        "" => rpc_error(id, -32600, "Invalid Request"),
        other => rpc_error(id, -32601, &format!("Method not found: {other}")),
    }
}

async fn call_tool(state: &AppState, id: Value, params: Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    match name {
        "ask_user" => ask_user(state, id, &args).await,
        "update_ask" => update_ask(state, id, &args),
        "cancel_ask" => cancel_ask(state, id, &args),
        "notify_user" => notify_user(state, id, &args),
        other => rpc_error(id, -32602, &format!("Unknown tool: {other}")),
    }
}

/// Parse + sanitize `ask_user` arguments into an [`AskRequest`], or return the
/// `rpc_error` [`Value`] to send back. Strips Unicode bidi override controls
/// from every agent-supplied string the human reads in the ask window / popup
/// row / "Unknown agent (<context>)" label, so a compromised or prompt-injected
/// agent can't visually spoof the text into reading the opposite of its real
/// code points (Trojan-Source / RLO; see `deck_core::naming::strip_bidi_controls`,
/// R-5.3 / R-8).
fn parse_ask_request(id: &Value, args: &Value) -> Result<AskRequest, Value> {
    // R-29.1: a valid non-empty `questions[]` array switches to the multi-question
    // form path; `question`/`options` are then ignored for input (but a fallback
    // is synthesized from the first item for legacy display / matching). Otherwise
    // the legacy single-question path runs. Reject only when NEITHER a valid
    // `question` nor any valid `questions[]` survives sanitization.
    let questions = parse_questions(args);

    let raw_question = strip_bidi_controls(
        args.get("question")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim(),
    );

    // The single-question `question`/`options` fields: in the form path they are
    // synthesized from the first form item so downstream (toast, popup mirror,
    // session matching) still has a headline; in the legacy path they are the
    // agent's own values.
    let (question, options) = match &questions {
        Some(qs) => {
            let first = &qs[0];
            let synthesized = truncate_graphemes(&first.question, MAX_QUESTION_CHARS);
            let opts = (!first.options.is_empty()).then(|| first.options.clone());
            (synthesized, opts)
        }
        None => {
            if raw_question.is_empty() {
                return Err(rpc_error(
                    id.clone(),
                    -32602,
                    "ask_user requires a non-empty `question` (or a non-empty `questions` array)",
                ));
            }
            let opts = args
                .get("options")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(strip_bidi_controls))
                        .collect::<Vec<_>>()
                })
                .filter(|v| !v.is_empty());
            (raw_question, opts)
        }
    };

    let detail = args
        .get("detail")
        .and_then(Value::as_str)
        .map(|s| strip_bidi_controls(s.trim()))
        .filter(|s| !s.is_empty());
    let context = args
        .get("context")
        .and_then(Value::as_str)
        .map(strip_bidi_controls);

    // R-19.2: `timeout_seconds` is optional; omitted / 0 / negative → persistent
    // (None). An explicit positive value is clamped to `1..=3600`.
    let requested = args
        .get("timeout_seconds")
        .and_then(|t| t.as_u64().or_else(|| t.as_f64().map(|f| f as u64)));
    let timeout_seconds = requested
        .filter(|&s| s > 0)
        .map(|s| s.min(MAX_TIMEOUT_SECONDS));

    Ok(AskRequest {
        question,
        options,
        questions,
        detail,
        context,
        timeout_seconds,
    })
}

/// Parse + sanitize the optional `questions[]` array (SPEC §29, R-29.1/R-29.6).
/// Each item is `{header?, question, multiSelect?, options[]}`: every string is
/// bidi-stripped and grapheme-capped, empty options are dropped, and the whole
/// form is bounded (≤[`MAX_QUESTIONS`] questions, ≤[`MAX_OPTIONS_PER_QUESTION`]
/// options each). A question item whose `question` text is empty after
/// sanitization is dropped. Returns `None` when `questions` is absent, not an
/// array, or nothing valid survives — the caller then falls back to the legacy
/// single-question path.
fn parse_questions(args: &Value) -> Option<Vec<AskQuestion>> {
    let arr = args.get("questions").and_then(Value::as_array)?;
    let mut out = Vec::new();
    for item in arr {
        if out.len() >= MAX_QUESTIONS {
            break;
        }
        let question = strip_bidi_controls(
            item.get("question")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim(),
        );
        if question.is_empty() {
            continue;
        }
        let question = truncate_graphemes(&question, MAX_QUESTION_CHARS);
        let header = item
            .get("header")
            .and_then(Value::as_str)
            .map(|s| strip_bidi_controls(s.trim()))
            .filter(|s| !s.is_empty())
            .map(|s| truncate_graphemes(&s, MAX_HEADER_CHARS));
        let multi_select = item
            .get("multiSelect")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let options = item
            .get("options")
            .and_then(Value::as_array)
            .map(|opts| {
                opts.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| truncate_graphemes(&strip_bidi_controls(s.trim()), MAX_OPTION_CHARS))
                    .filter(|s| !s.is_empty())
                    .take(MAX_OPTIONS_PER_QUESTION)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        out.push(AskQuestion {
            header,
            question,
            multi_select,
            options,
        });
    }
    (!out.is_empty()).then_some(out)
}

/// Non-streaming `ask_user`: blocks on the ask and returns a single JSON result.
/// Used when the client did NOT send a `progressToken` (no keepalive needed).
async fn ask_user(state: &AppState, id: Value, args: &Value) -> Value {
    let req = match parse_ask_request(&id, args) {
        Ok(req) => req,
        Err(err) => return err,
    };
    let timeout = req.timeout_seconds;
    let SubmittedAsk { id: ask_id, rx } = state.gateway.submit_ask(req);

    let result = match timeout {
        // Persistent (R-19.2): await indefinitely — no MCP-side timer.
        None => match rx.await {
            Ok(answer) => ask_answer_content(&ask_id, &answer),
            Err(_) => orphaned_content(),
        },
        Some(secs) => match tokio::time::timeout(Duration::from_secs(secs), rx).await {
            Ok(Ok(answer)) => ask_answer_content(&ask_id, &answer),
            Ok(Err(_)) => orphaned_content(),
            Err(_) => ask_answer_content(&ask_id, &AskAnswer::timeout()),
        },
    };
    rpc_result(id, result)
}

/// Streaming `ask_user` (R-19.3): answers over an SSE stream, emitting
/// `notifications/progress` every [`keepalive_interval`] while blocked, then the
/// final JSON-RPC response, then closing the stream. Used only when the client
/// sent a `progressToken`.
fn ask_user_sse(state: &AppState, id: Value, args: &Value, token: Value) -> Response {
    let req = match parse_ask_request(&id, args) {
        Ok(req) => req,
        // A parse error still goes back as a single SSE message so the client's
        // stream reader sees one JSON-RPC error and the connection closes.
        Err(err) => return single_event_sse(err),
    };
    let timeout = req.timeout_seconds;
    let SubmittedAsk { id: ask_id, rx } = state.gateway.submit_ask(req);
    let interval = state.keepalive;

    let (tx_ev, rx_ev) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(8);
    // R-32.4: `drive_ask` dismisses the ask if the client disconnects mid-wait,
    // so it needs the gateway handle.
    let gateway = state.gateway.clone();
    tokio::spawn(async move {
        let final_msg = drive_ask(rx, timeout, interval, &id, &ask_id, &token, &tx_ev, &gateway).await;
        let _ = tx_ev
            .send(Ok(Event::default().data(final_msg.to_string())))
            .await;
        // Dropping `tx_ev` here ends the stream → the HTTP body closes, so the
        // client's `fetch`/reader completes right after the final result.
    });

    Sse::new(ReceiverStream::new(rx_ev)).into_response()
}

/// The select loop shared by the SSE path: pump `notifications/progress` on the
/// keepalive tick until the ask resolves (or its explicit timeout elapses), then
/// return the final JSON-RPC response value.
#[allow(clippy::too_many_arguments)]
async fn drive_ask(
    rx: oneshot::Receiver<AskAnswer>,
    timeout: Option<u64>,
    interval: Duration,
    id: &Value,
    ask_id: &str,
    token: &Value,
    tx_ev: &tokio::sync::mpsc::Sender<Result<Event, Infallible>>,
    gateway: &Arc<dyn AskGateway>,
) -> Value {
    tokio::pin!(rx);
    let deadline = timeout.map(|s| tokio::time::Instant::now() + Duration::from_secs(s));
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await; // consume the immediate first tick
    let mut progress: u64 = 0;

    loop {
        let sleep = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            biased;
            res = &mut rx => {
                return match res {
                    Ok(answer) => rpc_result(id.clone(), ask_answer_content(ask_id, &answer)),
                    Err(_) => rpc_result(id.clone(), orphaned_content()),
                };
            }
            _ = sleep => {
                return rpc_result(id.clone(), ask_answer_content(ask_id, &AskAnswer::timeout()));
            }
            _ = ticker.tick() => {
                progress += 1;
                let note = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {
                        "progressToken": token,
                        "progress": progress,
                        "message": "waiting for the human to answer",
                    }
                });
                // If the receiver is gone (client disconnected) stop pumping. The
                // agent's SSE connection died mid-wait, so it can never receive an
                // answer — R-32.4: dismiss the ask now (removes the pending row +
                // re-renders the FIFO) instead of leaving it lingering until its
                // timeout. The returned value is moot: the closed stream drops it.
                if tx_ev.send(Ok(Event::default().data(note.to_string()))).await.is_err() {
                    gateway.cancel_ask(ask_id);
                    return rpc_result(id.clone(), ask_answer_content(ask_id, &AskAnswer::timeout()));
                }
            }
        }
    }
}

/// A one-shot SSE response carrying a single JSON-RPC message then closing (used
/// for a parse error on the streaming path).
fn single_event_sse(msg: Value) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(1);
    tokio::spawn(async move {
        let _ = tx.send(Ok(Event::default().data(msg.to_string()))).await;
    });
    Sse::new(ReceiverStream::new(rx)).into_response()
}

/// R-19.5 `update_ask`: mutate a pending ask in place. Unknown/settled id → an
/// error *result* (not a JSON-RPC exception).
fn update_ask(state: &AppState, id: Value, args: &Value) -> Value {
    let Some(ask_id) = args
        .get("ask_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return rpc_error(id, -32602, "update_ask requires a non-empty `ask_id`");
    };
    let question = args
        .get("question")
        .and_then(Value::as_str)
        .map(|s| strip_bidi_controls(s.trim()))
        .filter(|s| !s.is_empty());
    let options = args.get("options").and_then(Value::as_array).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(strip_bidi_controls))
            .collect::<Vec<_>>()
    });
    let detail = args
        .get("detail")
        .and_then(Value::as_str)
        .map(|s| strip_bidi_controls(s.trim()));

    if state.gateway.update_ask(ask_id, question, options, detail) {
        rpc_result(
            id,
            json!({
                "content": [{ "type": "text", "text": "Ask updated." }],
                "structuredContent": { "ask_id": ask_id, "updated": true },
                "isError": false,
            }),
        )
    } else {
        rpc_result(
            id,
            tool_error_content(&format!(
                "No pending ask with id {ask_id:?} — it may have been answered, dismissed, cancelled, or timed out."
            )),
        )
    }
}

/// R-19.5 `cancel_ask`: cancel a pending ask (resolves the blocked caller with
/// `kind:"cancelled"`). Unknown/settled id → an error result.
fn cancel_ask(state: &AppState, id: Value, args: &Value) -> Value {
    let Some(ask_id) = args
        .get("ask_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return rpc_error(id, -32602, "cancel_ask requires a non-empty `ask_id`");
    };
    if state.gateway.cancel_ask(ask_id) {
        rpc_result(
            id,
            json!({
                "content": [{ "type": "text", "text": "Ask cancelled." }],
                "structuredContent": { "ask_id": ask_id, "cancelled": true },
                "isError": false,
            }),
        )
    } else {
        rpc_result(
            id,
            tool_error_content(&format!(
                "No pending ask with id {ask_id:?} — it may have already been answered, dismissed, cancelled, or timed out."
            )),
        )
    }
}

fn notify_user(state: &AppState, id: Value, args: &Value) -> Value {
    let message = strip_bidi_controls(
        args.get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim(),
    );
    if message.is_empty() {
        return rpc_error(id, -32602, "notify_user requires a non-empty `message`");
    }
    let context = args
        .get("context")
        .and_then(Value::as_str)
        .map(strip_bidi_controls);
    // R-19.6: return the notification record id (also logged in the fake-notifier
    // jsonl).
    let record_id = state.gateway.notify(NotifyRequest { message, context });
    rpc_result(
        id,
        json!({
            "content": [{ "type": "text", "text": "Notification delivered." }],
            "structuredContent": { "delivered": true, "id": record_id },
            "isError": false,
        }),
    )
}

// ---------------------------------------------------------------------------
// JSON-RPC / MCP payload helpers
// ---------------------------------------------------------------------------

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// A successful `CallToolResult` carrying `{answer, kind, ask_id}` (R-19.5) both
/// as text and as `structuredContent` so clients can parse either shape.
fn ask_answer_content(ask_id: &str, answer: &AskAnswer) -> Value {
    let mut structured = serde_json::to_value(answer).unwrap_or_else(|_| json!({}));
    if let Some(obj) = structured.as_object_mut() {
        obj.insert("ask_id".to_string(), Value::String(ask_id.to_string()));
    }
    let text = serde_json::to_string(&structured).unwrap_or_default();
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
        "isError": false,
    })
}

/// The `CallToolResult` for an ask whose answer channel was dropped — the ask
/// was orphaned (R-8.7): Quarterdeck was closed/restarted while it was pending.
fn orphaned_content() -> Value {
    tool_error_content(
        "The ask was orphaned: Quarterdeck was closed or restarted while the question was pending.",
    )
}

fn tool_error_content(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true,
    })
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "quarterdeck", "version": env!("CARGO_PKG_VERSION") },
    })
}

fn tools_list_result() -> Value {
    json!({
        "tools": [
            {
                "name": "ask_user",
                "description": "Ask the human operating this machine a question and BLOCK until they answer. Use during long autonomous runs when you hit a decision only a human can make. Pass your current working directory as `context` so Quarterdeck attributes the question to your session. Provide EITHER a single `question` (+ optional `options`) OR a `questions` array for a multi-question / multi-select form; when `questions` is present, `question`/`options` are ignored. Keep `question` short; put the reasoning/body in `detail`. Prefer `options` for multiple-choice decisions. Omit `timeout_seconds` (or pass 0) to wait indefinitely (persistent). Returns {answer, kind, ask_id} where kind is option|text|timeout|dismissed|cancelled|form; a form answer is a JSON string {\"answers\":[{header,question,selected:[...],text?}, ...]}. On timeout/dismissal, proceed on your best judgment. Keep the returned `ask_id` if a parallel task may need to update_ask/cancel_ask it.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "question": { "type": "string", "description": "The question to put to the user. One short, specific decision. Use this (with `options`) for a single question; omit it when sending `questions`." },
                        "options": { "type": "array", "items": { "type": "string" }, "description": "Optional multiple-choice answers for the single-question form; the user may still type a free-text reply." },
                        "questions": {
                            "type": "array",
                            "description": "Multi-question / multi-select form (max 8 questions). When present, `question`/`options` are ignored. Each item is one question block.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "header": { "type": "string", "description": "Optional short label/category shown above the question." },
                                    "question": { "type": "string", "description": "The question text for this block (required)." },
                                    "multiSelect": { "type": "boolean", "description": "true → the user may pick several options (checkboxes); false/omitted → exactly one (radio)." },
                                    "options": { "type": "array", "items": { "type": "string" }, "description": "Choices for this question (max 12); the user may still add free text." }
                                },
                                "required": ["question"]
                            }
                        },
                        "detail": { "type": "string", "description": "Optional long-form rationale/body shown under the question in muted, smaller type. Put the context the user needs to decide here, not in `question`." },
                        "context": { "type": "string", "description": "Your current working directory (cwd), used to attribute the question to the right session." },
                        "timeout_seconds": { "type": "number", "maximum": 3600, "description": "Seconds to wait before giving up (max 3600). Omit or pass 0 to wait indefinitely (persistent)." }
                    }
                }
            },
            {
                "name": "update_ask",
                "description": "Revise a still-pending ask_user question in place (its situation changed). Pass the `ask_id` returned by ask_user and any of question/options/detail to replace. Typically called from a PARALLEL tool call or a different session — the blocked ask_user call cannot update itself. Errors if the ask is no longer pending (already answered/dismissed/cancelled/timed out).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "ask_id": { "type": "string", "description": "The id returned by the ask_user call to revise." },
                        "question": { "type": "string", "description": "New question text (optional)." },
                        "options": { "type": "array", "items": { "type": "string" }, "description": "New option list (optional; pass [] to clear)." },
                        "detail": { "type": "string", "description": "New detail/body (optional; pass \"\" to clear)." }
                    },
                    "required": ["ask_id"]
                }
            },
            {
                "name": "cancel_ask",
                "description": "Cancel a still-pending ask_user question (the decision is no longer needed). Pass the `ask_id` returned by ask_user; the blocked ask_user call returns with kind:\"cancelled\". Call from a PARALLEL tool call or a different session — the blocked call cannot cancel itself. Errors if the ask is no longer pending.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "ask_id": { "type": "string", "description": "The id returned by the ask_user call to cancel." }
                    },
                    "required": ["ask_id"]
                }
            },
            {
                "name": "notify_user",
                "description": "Send the human a fire-and-forget notification (toast) and return immediately. Use for progress FYIs that need no answer. Returns {delivered, id}.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "message": { "type": "string", "description": "The message to show." },
                        "context": { "type": "string", "description": "Your current working directory (cwd), used to attribute the notification." }
                    },
                    "required": ["message"]
                }
            }
        ]
    })
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|presented| constant_time_eq(presented.trim(), token))
        .unwrap_or(false)
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [
            (header::WWW_AUTHENTICATE, "Bearer"),
            (header::CONTENT_TYPE, "application/json"),
        ],
        r#"{"error":"unauthorized","message":"Missing or invalid bearer token"}"#,
    )
        .into_response()
}

fn json_response(status: StatusCode, value: &Value) -> Response {
    let body = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

// ---------------------------------------------------------------------------
// Tests: a fake gateway with an exposed answer channel + a raw HTTP client, plus
// an end-to-end run of scripts/mcp-client-test.mjs against this server.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

    struct TestInner {
        pending: VecDeque<(String, AskRequest, oneshot::Sender<AskAnswer>)>,
        asks: Vec<AskRequest>,
        ids: Vec<String>,
        notifies: Vec<NotifyRequest>,
        orphaned: bool,
        seq: u64,
    }

    /// Fake [`AskGateway`] whose pending asks can be answered programmatically —
    /// the "exposed test channel" the AC requires. `signal` fires whenever a new
    /// ask arrives, so a test can await it and then call [`answer_next`].
    #[derive(Clone)]
    struct TestGateway {
        inner: Arc<Mutex<TestInner>>,
        signal: mpsc::UnboundedSender<()>,
    }

    impl TestGateway {
        fn new() -> (Self, mpsc::UnboundedReceiver<()>) {
            let (tx, rx) = mpsc::unbounded_channel();
            let inner = TestInner {
                pending: VecDeque::new(),
                asks: Vec::new(),
                ids: Vec::new(),
                notifies: Vec::new(),
                orphaned: false,
                seq: 0,
            };
            (
                Self {
                    inner: Arc::new(Mutex::new(inner)),
                    signal: tx,
                },
                rx,
            )
        }

        fn answer_next(&self, answer: AskAnswer) -> bool {
            let popped = self.inner.lock().unwrap().pending.pop_front();
            match popped {
                Some((_, _, tx)) => tx.send(answer).is_ok(),
                None => false,
            }
        }

        fn drop_next(&self) -> bool {
            self.inner.lock().unwrap().pending.pop_front().is_some()
        }

        /// Pop the front pending ask (id, request, responder) so a test can
        /// inspect it and answer on its own schedule (e.g. delay so a keepalive
        /// progress fires first).
        #[allow(clippy::type_complexity)]
        fn pop_next(&self) -> Option<(String, AskRequest, oneshot::Sender<AskAnswer>)> {
            self.inner.lock().unwrap().pending.pop_front()
        }

        fn asks(&self) -> Vec<AskRequest> {
            self.inner.lock().unwrap().asks.clone()
        }
        fn last_id(&self) -> Option<String> {
            self.inner.lock().unwrap().ids.last().cloned()
        }
        fn notifies(&self) -> Vec<NotifyRequest> {
            self.inner.lock().unwrap().notifies.clone()
        }
        fn orphaned(&self) -> bool {
            self.inner.lock().unwrap().orphaned
        }
    }

    impl AskGateway for TestGateway {
        fn submit_ask(&self, req: AskRequest) -> SubmittedAsk {
            let (tx, rx) = oneshot::channel();
            let id = {
                let mut g = self.inner.lock().unwrap();
                let id = format!("test-ask-{}", g.seq);
                g.seq += 1;
                g.asks.push(req.clone());
                g.ids.push(id.clone());
                g.pending.push_back((id.clone(), req, tx));
                id
            };
            let _ = self.signal.send(());
            SubmittedAsk { id, rx }
        }
        fn update_ask(
            &self,
            ask_id: &str,
            question: Option<String>,
            options: Option<Vec<String>>,
            detail: Option<String>,
        ) -> bool {
            let mut g = self.inner.lock().unwrap();
            let Some((_, req, _)) = g.pending.iter_mut().find(|(id, _, _)| id == ask_id) else {
                return false;
            };
            if let Some(q) = question {
                req.question = q;
            }
            if let Some(o) = options {
                req.options = if o.is_empty() { None } else { Some(o) };
            }
            if let Some(d) = detail {
                req.detail = if d.is_empty() { None } else { Some(d) };
            }
            true
        }
        fn cancel_ask(&self, ask_id: &str) -> bool {
            let popped = {
                let mut g = self.inner.lock().unwrap();
                g.pending
                    .iter()
                    .position(|(id, _, _)| id == ask_id)
                    .and_then(|i| g.pending.remove(i))
            };
            match popped {
                Some((_, _, tx)) => {
                    let _ = tx.send(AskAnswer::cancelled());
                    true
                }
                None => false,
            }
        }
        fn notify(&self, req: NotifyRequest) -> String {
            let mut g = self.inner.lock().unwrap();
            let id = format!("test-notify-{}", g.notifies.len());
            g.notifies.push(req);
            id
        }
        fn orphan_stale_asks(&self) {
            self.inner.lock().unwrap().orphaned = true;
        }
    }

    async fn spawn_test_server(
        token: &str,
        gateway: Arc<dyn AskGateway>,
    ) -> (u16, oneshot::Sender<()>) {
        let listener = bind_local(0).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = AppState {
            token: Arc::new(token.to_string()),
            gateway,
            // Short keepalive so the SSE/progress tests emit a few ticks quickly
            // without a process-global env var (no cross-test race).
            keepalive: Duration::from_millis(80),
        };
        let router = build_router(state);
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router.into_make_service())
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
        });
        (port, tx)
    }

    /// Minimal HTTP/1.1 client: sends one request with `Connection: close` and
    /// reads the whole response, returning `(status, body)`. Avoids pulling in a
    /// heavier HTTP client crate the manifest doesn't declare.
    async fn http_post(port: u16, token: Option<String>, body: String) -> (u16, String) {
        let mut stream = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let mut req = String::new();
        req.push_str("POST /mcp HTTP/1.1\r\n");
        req.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
        req.push_str("Content-Type: application/json\r\n");
        req.push_str("Accept: application/json, text/event-stream\r\n");
        if let Some(t) = &token {
            req.push_str(&format!("Authorization: Bearer {t}\r\n"));
        }
        req.push_str("Connection: close\r\n");
        req.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        req.push_str(&body);

        stream.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        let body = text
            .split_once("\r\n\r\n")
            .map(|x| x.1)
            .unwrap_or("")
            .to_string();
        (status, body)
    }

    /// Parse the `data: {json}` lines out of a raw SSE HTTP body. `http_post`
    /// does not decode chunked transfer-encoding, so chunk-size lines are
    /// interleaved — they never start with `data:`, so scanning by line skips
    /// them.
    fn sse_frames(raw: &str) -> Vec<Value> {
        raw.lines()
            .filter_map(|l| l.trim_start().strip_prefix("data:"))
            .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
            .collect()
    }

    fn unique_temp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CNT: AtomicU64 = AtomicU64::new(0);
        let n = CNT.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!(
            "quarterdeck-mcp-test-{}-{nanos}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn rejects_requests_without_a_valid_bearer_token() {
        let (gw, _sig) = TestGateway::new();
        let (port, shutdown) = spawn_test_server("sekret", Arc::new(gw)).await;
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;

        let (no_token, _) = http_post(port, None, body.into()).await;
        assert_eq!(no_token, 401, "missing token must be 401");

        let (wrong, _) = http_post(port, Some("wrong".into()), body.into()).await;
        assert_eq!(wrong, 401, "wrong token must be 401");

        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn initialize_and_list_tools() {
        let (gw, _sig) = TestGateway::new();
        let (port, shutdown) = spawn_test_server("k", Arc::new(gw)).await;

        let (s1, b1) = http_post(
            port,
            Some("k".into()),
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}}"#.into(),
        )
        .await;
        assert_eq!(s1, 200);
        let v1: Value = serde_json::from_str(&b1).unwrap();
        assert_eq!(v1["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(v1["result"]["serverInfo"]["name"], "quarterdeck");

        let (s2, b2) = http_post(
            port,
            Some("k".into()),
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#.into(),
        )
        .await;
        assert_eq!(s2, 200);
        let v2: Value = serde_json::from_str(&b2).unwrap();
        let names: Vec<&str> = v2["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"ask_user"));
        assert!(names.contains(&"notify_user"));

        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn notification_is_accepted_with_202() {
        let (gw, _sig) = TestGateway::new();
        let (port, shutdown) = spawn_test_server("k", Arc::new(gw)).await;
        let (status, _body) = http_post(
            port,
            Some("k".into()),
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.into(),
        )
        .await;
        assert_eq!(status, 202);
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn ask_user_round_trip_via_test_channel() {
        let (gw, mut signal) = TestGateway::new();
        let gw = Arc::new(gw);
        let (port, shutdown) = spawn_test_server("secret-token", gw.clone()).await;

        let body = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ask_user","arguments":{"question":"Deploy to prod?","options":["yes","no"],"context":"C:/proj","timeout_seconds":30}}}"#;
        let call = tokio::spawn(http_post(port, Some("secret-token".into()), body.into()));

        // Block arrives on the exposed test channel, then we answer it.
        signal.recv().await.unwrap();
        assert_eq!(gw.asks()[0].question, "Deploy to prod?");
        assert_eq!(gw.asks()[0].options.as_ref().unwrap(), &["yes", "no"]);
        assert_eq!(gw.asks()[0].context.as_deref(), Some("C:/proj"));
        assert!(gw.answer_next(AskAnswer::option("yes")));

        let (status, resp) = call.await.unwrap();
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["id"], 3);
        assert_eq!(v["result"]["isError"], false);
        assert_eq!(v["result"]["structuredContent"]["answer"], "yes");
        assert_eq!(v["result"]["structuredContent"]["kind"], "option");
        // R-19.5: the result carries the ask_id for update_ask/cancel_ask.
        assert_eq!(
            v["result"]["structuredContent"]["ask_id"],
            gw.last_id().unwrap()
        );
        // The content text mirrors the structured answer.
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["kind"], "option");
        assert!(parsed["ask_id"].is_string());

        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn ask_user_strips_bidi_override_controls_from_agent_text() {
        // A compromised/prompt-injected agent embeds RLO (U+202E) + PDF (U+202C)
        // so the browser would render "cod.exe" reversed as "exe.doc" in the
        // always-on-top ask window. The transport must strip these before the
        // string reaches the DOM (R-5.3 / R-8) — assert on what the gateway sees.
        let (gw, mut signal) = TestGateway::new();
        let gw = Arc::new(gw);
        let (port, shutdown) = spawn_test_server("t", gw.clone()).await;
        let body = "{\"jsonrpc\":\"2.0\",\"id\":13,\"method\":\"tools/call\",\"params\":{\"name\":\"ask_user\",\"arguments\":{\"question\":\"OK to run \u{202E}cod.exe\u{202C} now\",\"options\":[\"\u{202E}yes\u{202C}\"],\"context\":\"C:/\u{202E}jorp\u{202C}\"}}}";
        let call = tokio::spawn(http_post(port, Some("t".into()), body.to_string()));
        signal.recv().await.unwrap();
        let ask = &gw.asks()[0];
        assert_eq!(ask.question, "OK to run cod.exe now");
        assert_eq!(ask.options.as_ref().unwrap(), &["yes"]);
        assert_eq!(ask.context.as_deref(), Some("C:/jorp"));
        assert!(gw.answer_next(AskAnswer::option("yes")));
        let _ = call.await.unwrap();
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn ask_user_times_out_when_unanswered() {
        let (gw, _sig) = TestGateway::new();
        let (port, shutdown) = spawn_test_server("t", Arc::new(gw)).await;
        let body = r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"ask_user","arguments":{"question":"q","timeout_seconds":1}}}"#;
        let (status, resp) = http_post(port, Some("t".into()), body.into()).await;
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["isError"], false);
        assert_eq!(v["result"]["structuredContent"]["kind"], "timeout");
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn ask_user_clamps_timeout_to_3600() {
        // R-19.2: the explicit-timeout cap was raised 600 → 3600.
        let (gw, mut signal) = TestGateway::new();
        let gw = Arc::new(gw);
        let (port, shutdown) = spawn_test_server("t", gw.clone()).await;
        let body = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"ask_user","arguments":{"question":"q","timeout_seconds":99999}}}"#;
        let call = tokio::spawn(http_post(port, Some("t".into()), body.into()));
        signal.recv().await.unwrap();
        assert_eq!(gw.asks()[0].timeout_seconds, Some(MAX_TIMEOUT_SECONDS));
        assert!(gw.answer_next(AskAnswer::text("done")));
        let (status, _resp) = call.await.unwrap();
        assert_eq!(status, 200);
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn ask_user_omitted_timeout_is_persistent() {
        // R-19.2: omitting timeout_seconds → persistent (None), no MCP-side timer.
        let (gw, mut signal) = TestGateway::new();
        let gw = Arc::new(gw);
        let (port, shutdown) = spawn_test_server("t", gw.clone()).await;
        let body = r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"ask_user","arguments":{"question":"forever?","detail":"the long rationale"}}}"#;
        let call = tokio::spawn(http_post(port, Some("t".into()), body.into()));
        signal.recv().await.unwrap();
        assert_eq!(gw.asks()[0].timeout_seconds, None, "persistent");
        assert_eq!(gw.asks()[0].detail.as_deref(), Some("the long rationale"));
        // Only an explicit answer resolves it.
        assert!(gw.answer_next(AskAnswer::text("done")));
        let (status, resp) = call.await.unwrap();
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["structuredContent"]["kind"], "text");
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn cancel_ask_resolves_blocked_call_as_cancelled() {
        // R-19.5: cancel_ask from a parallel call resolves the blocked ask_user
        // with kind:"cancelled".
        let (gw, mut signal) = TestGateway::new();
        let gw = Arc::new(gw);
        let (port, shutdown) = spawn_test_server("t", gw.clone()).await;
        // Persistent ask so it stays blocked until we cancel it.
        let body = r#"{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"ask_user","arguments":{"question":"still needed?"}}}"#;
        let call = tokio::spawn(http_post(port, Some("t".into()), body.into()));
        signal.recv().await.unwrap();
        let ask_id = gw.last_id().unwrap();

        // A parallel cancel_ask call.
        let cancel_body = format!(
            r#"{{"jsonrpc":"2.0","id":21,"method":"tools/call","params":{{"name":"cancel_ask","arguments":{{"ask_id":"{ask_id}"}}}}}}"#
        );
        let (cs, cresp) = http_post(port, Some("t".into()), cancel_body).await;
        assert_eq!(cs, 200);
        let cv: Value = serde_json::from_str(&cresp).unwrap();
        assert_eq!(cv["result"]["isError"], false);
        assert_eq!(cv["result"]["structuredContent"]["cancelled"], true);

        // The blocked ask_user returns cancelled.
        let (status, resp) = call.await.unwrap();
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["structuredContent"]["kind"], "cancelled");
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn update_ask_mutates_pending_and_errors_on_unknown_id() {
        // R-19.5: update_ask mutates a pending ask; an unknown/settled id yields
        // an error RESULT (isError:true), never a JSON-RPC exception.
        let (gw, mut signal) = TestGateway::new();
        let gw = Arc::new(gw);
        let (port, shutdown) = spawn_test_server("t", gw.clone()).await;
        let body = r#"{"jsonrpc":"2.0","id":30,"method":"tools/call","params":{"name":"ask_user","arguments":{"question":"old?"}}}"#;
        let call = tokio::spawn(http_post(port, Some("t".into()), body.into()));
        signal.recv().await.unwrap();
        let ask_id = gw.last_id().unwrap();

        let upd = format!(
            r#"{{"jsonrpc":"2.0","id":31,"method":"tools/call","params":{{"name":"update_ask","arguments":{{"ask_id":"{ask_id}","question":"new?","detail":"why"}}}}}}"#
        );
        let (us, uresp) = http_post(port, Some("t".into()), upd).await;
        assert_eq!(us, 200);
        let uv: Value = serde_json::from_str(&uresp).unwrap();
        assert_eq!(uv["result"]["isError"], false);
        assert_eq!(uv["result"]["structuredContent"]["updated"], true);
        assert_eq!(gw.asks()[0].question, "old?", "snapshot is submit-time");

        // Unknown id → error result.
        let bad = r#"{"jsonrpc":"2.0","id":32,"method":"tools/call","params":{"name":"update_ask","arguments":{"ask_id":"nope"}}}"#;
        let (bs, bresp) = http_post(port, Some("t".into()), bad.into()).await;
        assert_eq!(bs, 200);
        let bv: Value = serde_json::from_str(&bresp).unwrap();
        assert_eq!(bv["result"]["isError"], true);

        assert!(gw.answer_next(AskAnswer::text("done")));
        let _ = call.await.unwrap();
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn cancel_ask_unknown_id_is_error_result() {
        let (gw, _sig) = TestGateway::new();
        let (port, shutdown) = spawn_test_server("t", Arc::new(gw)).await;
        let body = r#"{"jsonrpc":"2.0","id":40,"method":"tools/call","params":{"name":"cancel_ask","arguments":{"ask_id":"missing"}}}"#;
        let (status, resp) = http_post(port, Some("t".into()), body.into()).await;
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["isError"], true);
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn ask_user_progress_notifications_stream_over_sse() {
        // R-19.3: with a progressToken, the blocked ask_user answers over SSE and
        // interleaves at least one notifications/progress before the final result.
        // The test server's keepalive is 80ms (see `spawn_test_server`).
        let (gw, mut signal) = TestGateway::new();
        let gw = Arc::new(gw);
        let (port, shutdown) = spawn_test_server("t", gw.clone()).await;
        let body = r#"{"jsonrpc":"2.0","id":50,"method":"tools/call","params":{"name":"ask_user","arguments":{"question":"slow?"},"_meta":{"progressToken":"tok-1"}}}"#;
        let call = tokio::spawn(http_post(port, Some("t".into()), body.into()));
        signal.recv().await.unwrap();
        // Let a couple of keepalive ticks fire, then answer.
        tokio::time::sleep(Duration::from_millis(220)).await;
        assert!(gw.answer_next(AskAnswer::text("ok")));

        let (status, resp) = call.await.unwrap();
        assert_eq!(status, 200);
        // The SSE body is a sequence of `data: {json}` lines. `http_post` returns
        // the raw HTTP body without decoding chunked transfer-encoding, so scan
        // line-by-line (chunk-size lines never start with `data:`).
        let frames: Vec<Value> = sse_frames(&resp);
        let progress: Vec<&Value> = frames
            .iter()
            .filter(|f| f["method"] == "notifications/progress")
            .collect();
        assert!(
            !progress.is_empty(),
            "expected >=1 progress notification, got frames: {resp}"
        );
        assert_eq!(progress[0]["params"]["progressToken"], "tok-1");
        // The last frame is the final result with the answer.
        let last = frames.last().unwrap();
        assert_eq!(last["result"]["structuredContent"]["kind"], "text");
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn dismiss_resolves_blocked_call_over_sse_not_a_timeout() {
        // R-19.4 regression: a dismiss must resolve the blocked call with
        // kind:"dismissed" — even on the SSE (progressToken) path — rather than
        // hanging until a transport timeout.
        let (gw, mut signal) = TestGateway::new();
        let gw = Arc::new(gw);
        let (port, shutdown) = spawn_test_server("t", gw.clone()).await;
        let body = r#"{"jsonrpc":"2.0","id":60,"method":"tools/call","params":{"name":"ask_user","arguments":{"question":"dismiss me"},"_meta":{"progressToken":9}}}"#;
        let call = tokio::spawn(http_post(port, Some("t".into()), body.into()));
        signal.recv().await.unwrap();
        assert!(gw.answer_next(AskAnswer::dismissed()));
        let (status, resp) = call.await.unwrap();
        assert_eq!(status, 200);
        let last = sse_frames(&resp).pop().expect("a final SSE frame");
        assert_eq!(last["result"]["structuredContent"]["kind"], "dismissed");
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn ask_user_orphaned_when_answer_channel_dropped() {
        let (gw, mut signal) = TestGateway::new();
        let gw = Arc::new(gw);
        let (port, shutdown) = spawn_test_server("t", gw.clone()).await;
        let body = r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"ask_user","arguments":{"question":"q","timeout_seconds":30}}}"#;
        let call = tokio::spawn(http_post(port, Some("t".into()), body.into()));
        signal.recv().await.unwrap();
        assert!(
            gw.drop_next(),
            "dropping the sender simulates orphaning (R-8.7)"
        );
        let (status, resp) = call.await.unwrap();
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["isError"], true);
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn notify_user_records_and_returns_immediately() {
        let (gw, _sig) = TestGateway::new();
        let gw = Arc::new(gw);
        let (port, shutdown) = spawn_test_server("k", gw.clone()).await;
        let body = r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"notify_user","arguments":{"message":"build done","context":"C:/x"}}}"#;
        let (status, resp) = http_post(port, Some("k".into()), body.into()).await;
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["isError"], false);
        // R-19.6: returns {delivered, id}.
        assert_eq!(v["result"]["structuredContent"]["delivered"], true);
        assert!(
            v["result"]["structuredContent"]["id"].is_string(),
            "notify_user returns a record id"
        );
        let notifies = gw.notifies();
        assert_eq!(notifies.len(), 1);
        assert_eq!(notifies[0].message, "build done");
        assert_eq!(notifies[0].context.as_deref(), Some("C:/x"));
        let _ = shutdown.send(());
    }

    #[tokio::test]
    // The env lock is intentionally held across awaits to serialize the whole
    // env-var-sensitive section; the tokio test runtime is single-threaded, so
    // there is no risk of a cross-task deadlock.
    #[allow(clippy::await_holding_lock)]
    async fn serve_persists_and_reuses_port_and_token() {
        // Serialize with the other cross-module `QUARTERDECK_DATA_DIR` mutator
        // (notify's data_dir test) so the parallel harness can't race us.
        let _env = crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = unique_temp_dir();
        std::env::set_var("QUARTERDECK_DATA_DIR", &dir);

        let (gw, _sig) = TestGateway::new();
        let gw = Arc::new(gw);
        let handle = serve(gw.clone()).await.unwrap();
        let port = handle.port();
        let token = handle.token().to_string();
        assert!(port > 0);
        assert_eq!(token.len(), 48);
        assert!(
            gw.orphaned(),
            "orphan_stale_asks must run at startup (R-8.7)"
        );

        let cfg = load_config().unwrap();
        assert_eq!(cfg.port, port);
        assert_eq!(cfg.token, token);
        handle.shutdown().await;

        // Restart: the persisted port + token are reused (random-stable).
        let (gw2, _sig2) = TestGateway::new();
        let handle2 = serve(Arc::new(gw2)).await.unwrap();
        assert_eq!(
            handle2.port(),
            port,
            "port should be stable across restarts"
        );
        assert_eq!(
            handle2.token(),
            token,
            "token should be stable across restarts"
        );
        handle2.shutdown().await;

        std::env::remove_var("QUARTERDECK_DATA_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- §29 multi-question / multi-select form ----------------------------

    #[test]
    fn parse_ask_request_legacy_single_question_is_unchanged() {
        // R-29.1 back-compat: a single-question caller with no `questions` gets
        // exactly the old AskRequest (questions == None), options preserved.
        let args = json!({
            "question": "Deploy now?",
            "options": ["yes", "no"],
            "context": "C:/proj",
        });
        let req = parse_ask_request(&json!(1), &args).expect("valid legacy ask");
        assert_eq!(req.question, "Deploy now?");
        assert_eq!(req.options.as_deref().unwrap(), &["yes", "no"]);
        assert!(req.questions.is_none(), "no form for a legacy ask");
    }

    #[test]
    fn parse_ask_request_parses_questions_and_synthesizes_fallback() {
        // R-29.1: a valid `questions[]` switches to the form path; `question`
        // is synthesized from the first item (its options become the fallback
        // `options`), and `multiSelect` defaults to false when omitted.
        let args = json!({
            "question": "ignored when questions present",
            "options": ["ignored"],
            "questions": [
                { "header": "Env", "question": "Which environment?", "options": ["prod", "staging"] },
                { "question": "Extra flags?", "multiSelect": true, "options": ["--fast", "--safe", "--verbose"] },
            ],
        });
        let req = parse_ask_request(&json!(1), &args).expect("valid form");
        let qs = req.questions.as_ref().expect("questions parsed");
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].header.as_deref(), Some("Env"));
        assert_eq!(qs[0].question, "Which environment?");
        assert!(!qs[0].multi_select, "multiSelect defaults false");
        assert_eq!(qs[0].options, ["prod", "staging"]);
        assert!(qs[1].multi_select);
        assert_eq!(qs[1].options.len(), 3);
        // Fallback headline synthesized from the first question (not the ignored
        // top-level `question`).
        assert_eq!(req.question, "Which environment?");
        assert_eq!(req.options.as_deref().unwrap(), &["prod", "staging"]);
    }

    #[test]
    fn parse_ask_request_enforces_form_caps_and_sanitizes() {
        // R-29.6: ≤8 questions, ≤12 options/question, grapheme caps, empty
        // questions dropped, bidi controls stripped.
        let mut questions: Vec<Value> = Vec::new();
        // 10 questions → clamp to 8.
        for i in 0..10 {
            questions.push(json!({ "question": format!("Q{i}?") }));
        }
        // A question with 20 options (clamp to 12) + an empty option (dropped).
        let many_opts: Vec<String> = (0..20).map(|i| format!("opt{i}")).collect();
        let mut opts_with_blank = many_opts.clone();
        opts_with_blank.push(String::new());
        questions.push(json!({ "question": "Too many opts?", "options": opts_with_blank }));
        // An empty-question item is dropped entirely.
        questions.push(json!({ "question": "   " }));
        // A bidi-spoofed header/question/option is stripped.
        questions.push(json!({
            "header": "H\u{202E}dr",
            "question": "Run \u{202E}cod.exe\u{202C}?",
            "options": ["\u{202E}yes\u{202C}"],
        }));
        // An over-long question is grapheme-capped to 200 (+ ellipsis).
        let long_q = "x".repeat(400);
        questions.push(json!({ "question": long_q }));

        let args = json!({ "questions": questions });
        let req = parse_ask_request(&json!(1), &args).expect("valid form");
        let qs = req.questions.as_ref().unwrap();
        assert_eq!(qs.len(), MAX_QUESTIONS, "questions clamped to 8");
        // All 8 survivors are the first 8 non-empty "Q{i}?" items.
        assert_eq!(qs[0].question, "Q0?");
        assert_eq!(qs[7].question, "Q7?");
        // (The over-cap options / bidi / long items came after the first 8 and
        // were cut by the question cap — assert the caps directly on a focused
        // form below.)

        // Focused: options cap + blank drop + bidi strip + question cap.
        let args2 = json!({ "questions": [
            { "question": "opts?", "options": (0..20).map(|i| format!("o{i}")).collect::<Vec<_>>() },
            { "header": "H\u{202E}", "question": "\u{202E}spoof\u{202C}?", "options": ["\u{202E}a\u{202C}", ""] },
            { "question": "y".repeat(400) },
        ]});
        let req2 = parse_ask_request(&json!(2), &args2).unwrap();
        let qs2 = req2.questions.as_ref().unwrap();
        assert_eq!(qs2[0].options.len(), MAX_OPTIONS_PER_QUESTION, "options clamped to 12");
        assert_eq!(qs2[1].header.as_deref(), Some("H"), "bidi stripped from header");
        assert_eq!(qs2[1].question, "spoof?", "bidi stripped from question");
        assert_eq!(qs2[1].options, ["a"], "bidi stripped + blank option dropped");
        // 200-cap keeps 199 chars + the ellipsis.
        assert_eq!(qs2[2].question.chars().count(), MAX_QUESTION_CHARS);
        assert!(qs2[2].question.ends_with('…'));
    }

    #[test]
    fn parse_ask_request_rejects_when_neither_question_nor_valid_questions() {
        // R-29.1: reject only when NEITHER a valid `question` nor any valid
        // `questions[]` survives. An empty questions array / all-empty items are
        // not enough.
        assert!(parse_ask_request(&json!(1), &json!({})).is_err());
        assert!(parse_ask_request(&json!(1), &json!({ "questions": [] })).is_err());
        assert!(parse_ask_request(&json!(1), &json!({ "questions": [{ "question": "  " }] })).is_err());
        // But an empty top-level `question` WITH a valid form is fine.
        assert!(parse_ask_request(
            &json!(1),
            &json!({ "question": "", "questions": [{ "question": "ok?" }] })
        )
        .is_ok());
    }

    #[test]
    fn ask_answer_form_serializes_kind_form() {
        // R-29.3: a form answer serializes with kind "form" and the answer JSON
        // on the existing channel.
        let doc = r#"{"answers":[{"question":"Which?","selected":["prod"]}]}"#;
        let content = ask_answer_content("ask-1", &AskAnswer::form(doc));
        assert_eq!(content["structuredContent"]["kind"], "form");
        assert_eq!(content["structuredContent"]["answer"], doc);
        assert_eq!(content["structuredContent"]["ask_id"], "ask-1");
        assert_eq!(content["isError"], false);
    }

    #[tokio::test]
    async fn ask_user_form_round_trip_returns_kind_form() {
        // R-29.1/R-29.3 end-to-end: an ask with `questions[]` blocks, the form
        // answer resolves it, and the MCP result carries kind:"form" + the doc.
        let (gw, mut signal) = TestGateway::new();
        let gw = Arc::new(gw);
        let (port, shutdown) = spawn_test_server("t", gw.clone()).await;
        let body = r#"{"jsonrpc":"2.0","id":70,"method":"tools/call","params":{"name":"ask_user","arguments":{"questions":[{"header":"Env","question":"Which environment?","options":["prod","staging"]},{"question":"Flags?","multiSelect":true,"options":["--fast","--safe"]}]}}}"#;
        let call = tokio::spawn(http_post(port, Some("t".into()), body.into()));
        signal.recv().await.unwrap();
        let ask = &gw.asks()[0];
        let qs = ask.questions.as_ref().expect("form carried to the gateway");
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].question, "Which environment?");
        // The synthesized headline is the first question.
        assert_eq!(ask.question, "Which environment?");

        let doc = r#"{"answers":[{"header":"Env","question":"Which environment?","selected":["prod"]},{"question":"Flags?","selected":["--fast","--safe"],"text":"go"}]}"#;
        assert!(gw.answer_next(AskAnswer::form(doc)));

        let (status, resp) = call.await.unwrap();
        assert_eq!(status, 200);
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["structuredContent"]["kind"], "form");
        assert_eq!(v["result"]["structuredContent"]["answer"], doc);
        let _ = shutdown.send(());
    }

    fn node_available() -> bool {
        std::process::Command::new("node")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Drives the real Node client (`scripts/mcp-client-test.mjs`) against this
    /// server: initialize → tools/list → blocking ask_user round-trip →
    /// notify_user, with answers injected via the exposed test channel.
    #[tokio::test]
    async fn node_client_end_to_end() {
        if !node_available() {
            eprintln!("skipping node_client_end_to_end: `node` not found on PATH");
            return;
        }

        let (gw, mut signal) = TestGateway::new();
        let gw = Arc::new(gw);
        let (port, shutdown) = spawn_test_server("node-token", gw.clone()).await;

        // Auto-answer each incoming ask via the exposed test channel. The node
        // script tags questions so this harness can drive the R-19 flows:
        //   * "DISMISS" → resolve as dismissed (R-19.4 regression)
        //   * "PROGRESS" → delay past a keepalive tick, then answer (R-19.3)
        //   * otherwise → first option, else a text answer.
        let gw_ans = gw.clone();
        let answerer = tokio::spawn(async move {
            while signal.recv().await.is_some() {
                let Some((_, req, tx)) = gw_ans.pop_next() else {
                    continue;
                };
                let q = req.question.clone();
                if q.contains("DISMISS") {
                    let _ = tx.send(AskAnswer::dismissed());
                } else if q.contains("PROGRESS") {
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        let _ = tx.send(AskAnswer::text("ok after progress"));
                    });
                } else {
                    let ans = match req.options.as_ref().and_then(|o| o.first()) {
                        Some(opt) => AskAnswer::option(opt.clone()),
                        None => AskAnswer::text("ok from test channel"),
                    };
                    let _ = tx.send(ans);
                }
            }
        });

        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/mcp-client-test.mjs");
        let output = tokio::process::Command::new("node")
            .arg(script)
            .env("QUARTERDECK_MCP_PORT", port.to_string())
            .env("QUARTERDECK_MCP_TOKEN", "node-token")
            .output()
            .await
            .expect("failed to spawn node");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("--- node stdout ---\n{stdout}\n--- node stderr ---\n{stderr}");
        assert!(
            output.status.success(),
            "node client exited non-zero: {stderr}"
        );
        assert!(
            stdout.contains("ALL CHECKS PASSED"),
            "node client did not report success"
        );

        let _ = shutdown.send(());
        answerer.abort();
    }
}
