//! MCP server (SPEC §8): a **streamable-HTTP** Model Context Protocol server bound
//! to `127.0.0.1:<port>` that lets Claude Code agents reach the human through
//! Quarterdeck. It serves two tools:
//!
//! * `ask_user(question, options?, context?, timeout_seconds?)` — **blocks** until
//!   the user answers, the ask times out (`timeout_seconds` ≤ 600), or it is
//!   dismissed/orphaned. Returns `{answer, kind}` with `kind ∈ option|text|
//!   timeout|dismissed` (R-8.1).
//! * `notify_user(message, context?)` — fire-and-forget toast, returns immediately.
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

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::ipc::AskAnswerKind;

/// The single MCP endpoint path (streamable HTTP transport).
pub const MCP_PATH: &str = "/mcp";
/// MCP protocol revision we advertise in `initialize`.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

const MAX_TIMEOUT_SECONDS: u64 = 600;
const DEFAULT_TIMEOUT_SECONDS: u64 = 600;

// ---------------------------------------------------------------------------
// Data types crossing the T7 seam
// ---------------------------------------------------------------------------

/// A question routed from an agent to the human. `timeout_seconds` is already
/// clamped to `1..=600` by the transport before the gateway sees it.
#[derive(Debug, Clone)]
pub struct AskRequest {
    pub question: String,
    pub options: Option<Vec<String>>,
    pub context: Option<String>,
    pub timeout_seconds: u64,
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
}

/// The narrow seam between the MCP transport and the deck engine (see module
/// docs). T7 wires a real, engine-backed implementation; tests use a fake.
pub trait AskGateway: Send + Sync + 'static {
    /// Register a new ask and return a channel that resolves when the user
    /// answers, the ask times out, or it is dismissed. Dropping the sender
    /// (engine teardown) surfaces as an orphaned ask on the caller side.
    fn submit_ask(&self, req: AskRequest) -> oneshot::Receiver<AskAnswer>;

    /// Deliver a fire-and-forget notification toast.
    fn notify(&self, req: NotifyRequest);

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
/// never initiates messages, so we decline it (spec-permitted 405).
async fn handle_get() -> Response {
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
            let response = dispatch(&state, &method, id, params).await;
            json_response(StatusCode::OK, &response)
        }
    }
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
        "notify_user" => notify_user(state, id, &args),
        other => rpc_error(id, -32602, &format!("Unknown tool: {other}")),
    }
}

async fn ask_user(state: &AppState, id: Value, args: &Value) -> Value {
    let question = args
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if question.is_empty() {
        return rpc_error(id, -32602, "ask_user requires a non-empty `question`");
    }

    let options = args
        .get("options")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty());
    let context = args
        .get("context")
        .and_then(Value::as_str)
        .map(str::to_string);

    let requested = args
        .get("timeout_seconds")
        .and_then(|t| t.as_u64().or_else(|| t.as_f64().map(|f| f as u64)));
    let timeout_seconds = requested
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS)
        .min(MAX_TIMEOUT_SECONDS);

    let req = AskRequest {
        question,
        options,
        context,
        timeout_seconds,
    };
    let rx = state.gateway.submit_ask(req);

    let answer = match tokio::time::timeout(Duration::from_secs(timeout_seconds), rx).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(_)) => {
            // Answer channel dropped: the ask was orphaned (R-8.7). Never a
            // silent success — surface a tool error.
            return rpc_result(
                id,
                tool_error_content(
                    "The ask was orphaned: Quarterdeck was closed or restarted while the question was pending.",
                ),
            );
        }
        Err(_) => AskAnswer::timeout(),
    };
    rpc_result(id, tool_answer_content(&answer))
}

fn notify_user(state: &AppState, id: Value, args: &Value) -> Value {
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if message.is_empty() {
        return rpc_error(id, -32602, "notify_user requires a non-empty `message`");
    }
    let context = args
        .get("context")
        .and_then(Value::as_str)
        .map(str::to_string);
    state.gateway.notify(NotifyRequest { message, context });
    rpc_result(
        id,
        json!({
            "content": [{ "type": "text", "text": "Notification delivered." }],
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

/// A successful `CallToolResult` carrying the answer both as text and as
/// `structuredContent` so clients can parse either shape.
fn tool_answer_content(answer: &AskAnswer) -> Value {
    let structured = serde_json::to_value(answer).unwrap_or_else(|_| json!({}));
    let text = serde_json::to_string(&structured).unwrap_or_default();
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
        "isError": false,
    })
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
                "description": "Ask the human operating this machine a question and BLOCK until they answer. Use during long autonomous runs when you hit a decision only a human can make. Pass your current working directory as `context` so Quarterdeck attributes the question to your session. Prefer `options` for multiple-choice decisions. Returns {answer, kind} where kind is option|text|timeout|dismissed; on timeout or dismissal, proceed on your best judgment.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "question": { "type": "string", "description": "The question to put to the user. Be concise and specific." },
                        "options": { "type": "array", "items": { "type": "string" }, "description": "Optional multiple-choice answers; the user may still type a free-text reply." },
                        "context": { "type": "string", "description": "Your current working directory (cwd), used to attribute the question to the right session." },
                        "timeout_seconds": { "type": "number", "maximum": 600, "description": "Seconds to wait before giving up (max 600). Defaults to 600." }
                    },
                    "required": ["question"]
                }
            },
            {
                "name": "notify_user",
                "description": "Send the human a fire-and-forget notification (toast) and return immediately. Use for progress FYIs that need no answer.",
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
        pending: VecDeque<(AskRequest, oneshot::Sender<AskAnswer>)>,
        asks: Vec<AskRequest>,
        notifies: Vec<NotifyRequest>,
        orphaned: bool,
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
                notifies: Vec::new(),
                orphaned: false,
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
                Some((_, tx)) => tx.send(answer).is_ok(),
                None => false,
            }
        }

        fn answer_next_with<F: FnOnce(&AskRequest) -> AskAnswer>(&self, f: F) -> bool {
            let popped = self.inner.lock().unwrap().pending.pop_front();
            match popped {
                Some((req, tx)) => tx.send(f(&req)).is_ok(),
                None => false,
            }
        }

        fn drop_next(&self) -> bool {
            self.inner.lock().unwrap().pending.pop_front().is_some()
        }

        fn asks(&self) -> Vec<AskRequest> {
            self.inner.lock().unwrap().asks.clone()
        }
        fn notifies(&self) -> Vec<NotifyRequest> {
            self.inner.lock().unwrap().notifies.clone()
        }
        fn orphaned(&self) -> bool {
            self.inner.lock().unwrap().orphaned
        }
    }

    impl AskGateway for TestGateway {
        fn submit_ask(&self, req: AskRequest) -> oneshot::Receiver<AskAnswer> {
            let (tx, rx) = oneshot::channel();
            {
                let mut g = self.inner.lock().unwrap();
                g.asks.push(req.clone());
                g.pending.push_back((req, tx));
            }
            let _ = self.signal.send(());
            rx
        }
        fn notify(&self, req: NotifyRequest) {
            self.inner.lock().unwrap().notifies.push(req);
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
        // The content text mirrors the structured answer.
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["kind"], "option");

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
    async fn ask_user_clamps_timeout_to_600() {
        let (gw, mut signal) = TestGateway::new();
        let gw = Arc::new(gw);
        let (port, shutdown) = spawn_test_server("t", gw.clone()).await;
        let body = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"ask_user","arguments":{"question":"q","timeout_seconds":99999}}}"#;
        let call = tokio::spawn(http_post(port, Some("t".into()), body.into()));
        signal.recv().await.unwrap();
        assert_eq!(gw.asks()[0].timeout_seconds, MAX_TIMEOUT_SECONDS);
        assert!(gw.answer_next(AskAnswer::text("done")));
        let (status, _resp) = call.await.unwrap();
        assert_eq!(status, 200);
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

        // Auto-answer each incoming ask via the exposed test channel: pick the
        // first offered option, else echo a text answer.
        let gw_ans = gw.clone();
        let answerer = tokio::spawn(async move {
            while signal.recv().await.is_some() {
                gw_ans.answer_next_with(|req| match req.options.as_ref().and_then(|o| o.first()) {
                    Some(opt) => AskAnswer::option(opt.clone()),
                    None => AskAnswer::text("ok from test channel"),
                });
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
