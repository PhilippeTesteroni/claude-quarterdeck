//! Engine-side ask lifecycle (SPEC §8): the pending-ask queue plus its timeout
//! / dismissal / orphaning bookkeeping (R-8.3 FIFO queue, R-8.7 orphaning),
//! driven by the injectable [`Clock`] so it is part of the portable,
//! heavy-tested core (R-3.1/R-3.2) rather than the Tauri shell. The MCP
//! transport (`src-tauri/src/mcp_server.rs`) and the OS I/O (writing ask files,
//! reading answers, showing the window) stay in the shell; this module owns the
//! pure queue state.
//!
//! [`PendingAsk`] is generic over the responder handle `R` so the shell can
//! store its `tokio::sync::oneshot::Sender<AskAnswer>` while `deck-core` stays
//! GUI/async-runtime free; tests use `()` or a small fake.

use crate::engine::SystemClock;
use crate::traits::Clock;
use serde::{Deserialize, Serialize};

/// A single question inside a multi-question / multi-select `ask_user` form
/// (SPEC §29, R-29.1/R-29.2) — the shape of Claude Code's native
/// `AskUserQuestion`. Portable (no GUI/transport deps) so it lives on the
/// engine-side [`PendingAsk`], the transport [`AskRequest`], and the persisted
/// ask file alike, and every field is defaulted/optional so an older ask
/// snapshot with no `questions` still deserializes (R-29.6). The transport
/// (`mcp_server::parse_ask_request`) is responsible for bidi-stripping and
/// grapheme-capping every string before one reaches here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AskQuestion {
    /// Optional short header/label rendered above the question (e.g. a category).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// The question text (required, non-empty by construction).
    pub question: String,
    /// True → the user may pick several options (checkboxes); false → exactly
    /// one (radio buttons). Defaulted so it may be omitted by the agent / an old
    /// file.
    #[serde(default)]
    pub multi_select: bool,
    /// The offered choices. May be empty (a free-text-only sub-question).
    #[serde(default)]
    pub options: Vec<String>,
}

/// A pending agent question (SPEC §8). `R` is the shell's responder handle used
/// to unblock the MCP `ask_user` call (e.g. a `oneshot::Sender`); `deck-core`
/// never touches it beyond moving it out on resolution.
#[derive(Debug)]
pub struct PendingAsk<R> {
    pub id: String,
    /// Matched session (R-8.2), or `None` for an unmatched / unknown agent.
    pub session_id: Option<String>,
    pub project: Option<String>,
    pub question: String,
    pub options: Option<Vec<String>>,
    /// Multi-question / multi-select form (SPEC §29, R-29.2): when `Some` and
    /// non-empty, the ask window renders a form of [`AskQuestion`] blocks instead
    /// of the single-question options/free-text, and the answer comes back as a
    /// `form`-kind JSON document. `None` → the legacy single-question ask. Carried
    /// through `submit_ask` / the persisted ask file so a recovered form still
    /// renders (expired).
    pub questions: Option<Vec<AskQuestion>>,
    /// Long rationale/body rendered under the question (R-19.1), sanitized by
    /// the transport before it reaches here.
    pub detail: Option<String>,
    /// Raw `context` (agent cwd) the MCP call carried (R-8.2).
    pub context: Option<String>,
    /// Epoch ms the ask was enqueued (arrival time). Drives the shared
    /// ask/perm FIFO ordering (R-16.2: a pending perm "FIFO-queues with asks",
    /// one queue by arrival — the ask window's primary slot goes to whichever of
    /// the front ask / front perm arrived first, not to perms unconditionally).
    pub enqueued_ms: u64,
    /// Epoch ms at which the ask times out. Meaningless when `persistent`.
    pub timeout_at_ms: u64,
    /// True when the ask has no expiry (R-19.2: `timeout_seconds` omitted/0). It
    /// lives until answered/dismissed/cancelled and is exempt from the timeout
    /// sweep; the UI shows no countdown.
    pub persistent: bool,
    /// True when this ask was recovered from disk at startup and can never be
    /// answered (its MCP connection died with the previous process, R-8.7). It
    /// is shown as expired and is exempt from the timeout sweep.
    pub orphaned: bool,
    /// Shell-side handle to deliver the answer back to the blocked call.
    pub responder: Option<R>,
}

/// The pending-ask queue (SPEC §8), FIFO-ordered (R-8.3). Owns the injected
/// [`Clock`] so timeout sweeps are deterministic under test.
pub struct AskStore<R> {
    asks: Vec<PendingAsk<R>>,
    clock: Box<dyn Clock + Send + Sync>,
}

impl<R> std::fmt::Debug for AskStore<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AskStore")
            .field("asks", &self.asks.len())
            .finish()
    }
}

impl<R> AskStore<R> {
    /// Construct with an injected clock (fake in tests, [`SystemClock`] in prod).
    #[must_use]
    pub fn new(clock: Box<dyn Clock + Send + Sync>) -> Self {
        Self {
            asks: Vec::new(),
            clock,
        }
    }

    /// Convenience constructor using the real system clock.
    #[must_use]
    pub fn with_system_clock() -> Self {
        Self::new(Box::new(SystemClock))
    }

    /// Current time from the injected clock (so callers compute `timeout_at_ms`
    /// against the same source the sweep uses).
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.asks.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.asks.len()
    }

    /// FIFO iteration for the UI projection (R-8.3 queue order).
    pub fn iter(&self) -> std::slice::Iter<'_, PendingAsk<R>> {
        self.asks.iter()
    }

    /// Enqueue a new pending ask at the back of the FIFO queue.
    pub fn push(&mut self, ask: PendingAsk<R>) {
        self.asks.push(ask);
    }

    /// Remove and return the ask with `id`, if present (answer / dismissal /
    /// cancellation).
    pub fn take(&mut self, id: &str) -> Option<PendingAsk<R>> {
        self.asks
            .iter()
            .position(|a| a.id == id)
            .map(|i| self.asks.remove(i))
    }

    /// Mutable access to a pending ask by id, for in-place mutation
    /// (`update_ask`, R-19.5). Returns `None` for an unknown id; the queue
    /// position is preserved (this never removes/reorders).
    pub fn get_mut(&mut self, id: &str) -> Option<&mut PendingAsk<R>> {
        self.asks.iter_mut().find(|a| a.id == id)
    }

    /// Remove and return every ask whose timeout has elapsed (R-8.3). Orphaned
    /// (R-8.7) and persistent (R-19.2) asks have no expiry and are never swept.
    pub fn sweep_expired(&mut self) -> Vec<PendingAsk<R>> {
        let now = self.clock.now_ms();
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.asks.len() {
            let a = &self.asks[i];
            if !a.orphaned && !a.persistent && now >= a.timeout_at_ms {
                out.push(self.asks.remove(i));
            } else {
                i += 1;
            }
        }
        out
    }

    /// Remove and return every ask (used when tearing the process down).
    pub fn drain_all(&mut self) -> Vec<PendingAsk<R>> {
        std::mem::take(&mut self.asks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::Clock;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    #[derive(Debug)]
    struct FakeClock(AtomicU64);
    impl Clock for Arc<FakeClock> {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn store_at(now: u64) -> (AskStore<u32>, Arc<FakeClock>) {
        let clock = Arc::new(FakeClock(AtomicU64::new(now)));
        (AskStore::new(Box::new(clock.clone())), clock)
    }

    fn ask(id: &str, session: Option<&str>, timeout_at: u64, responder: u32) -> PendingAsk<u32> {
        PendingAsk {
            id: id.to_string(),
            session_id: session.map(ToString::to_string),
            project: None,
            question: "q?".to_string(),
            options: None,
            questions: None,
            detail: None,
            context: None,
            enqueued_ms: 0,
            timeout_at_ms: timeout_at,
            persistent: false,
            orphaned: false,
            responder: Some(responder),
        }
    }

    #[test]
    fn queue_is_fifo() {
        let (mut store, _c) = store_at(0);
        store.push(ask("a", None, 100, 1));
        store.push(ask("b", None, 100, 2));
        store.push(ask("c", None, 100, 3));
        let ids: Vec<&str> = store.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(store.len(), 3);
        assert!(!store.is_empty());
    }

    #[test]
    fn take_removes_the_matching_ask() {
        let (mut store, _c) = store_at(0);
        store.push(ask("a", Some("s1"), 100, 1));
        store.push(ask("b", None, 100, 2));
        let taken = store.take("a").expect("ask a present");
        assert_eq!(taken.session_id.as_deref(), Some("s1"));
        assert_eq!(taken.responder, Some(1));
        assert!(store.take("a").is_none(), "already removed");
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn sweep_expired_uses_the_injected_clock() {
        let (mut store, clock) = store_at(1_000);
        store.push(ask("soon", None, 2_000, 1));
        store.push(ask("later", None, 5_000, 2));

        // Nothing expired yet.
        assert!(store.sweep_expired().is_empty());

        clock.0.store(2_000, Ordering::SeqCst);
        let expired = store.sweep_expired();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, "soon");
        assert_eq!(store.len(), 1, "the not-yet-expired ask remains");

        clock.0.store(9_999, Ordering::SeqCst);
        let expired = store.sweep_expired();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, "later");
        assert!(store.is_empty());
    }

    #[test]
    fn orphaned_asks_are_never_swept_by_timeout() {
        let (mut store, clock) = store_at(0);
        let mut orphan = ask("orphan", None, 0, 1);
        orphan.orphaned = true;
        orphan.responder = None;
        store.push(orphan);
        clock.0.store(1_000_000, Ordering::SeqCst);
        // Past its (zeroed) timeout, but orphaned → shown-as-expired, not swept.
        assert!(store.sweep_expired().is_empty());
        assert_eq!(store.len(), 1);
        // It can still be dismissed explicitly.
        assert!(store.take("orphan").is_some());
    }

    #[test]
    fn persistent_asks_are_never_swept_by_timeout() {
        // R-19.2: a persistent ask (timeout_seconds omitted/0) has no expiry and
        // must survive the timeout sweep indefinitely, until explicitly taken
        // (answer / dismiss / cancel).
        let (mut store, clock) = store_at(0);
        let mut persistent = ask("persist", Some("s1"), 0, 7);
        persistent.persistent = true;
        store.push(persistent);
        clock.0.store(u64::MAX, Ordering::SeqCst);
        assert!(
            store.sweep_expired().is_empty(),
            "persistent ask must never time out (R-19.2)"
        );
        assert_eq!(store.len(), 1);
        // It can still be taken (cancel / dismiss / answer).
        assert!(store.take("persist").is_some());
    }

    #[test]
    fn get_mut_updates_in_place_and_keeps_queue_position() {
        // R-19.5 update_ask: mutating a pending ask keeps its FIFO position.
        let (mut store, _c) = store_at(0);
        store.push(ask("a", None, 100, 1));
        store.push(ask("b", None, 100, 2));
        store.push(ask("c", None, 100, 3));

        let b = store.get_mut("b").expect("b present");
        b.question = "updated?".to_string();
        b.options = Some(vec!["x".to_string(), "y".to_string()]);
        b.detail = Some("the reasoning".to_string());

        let ids: Vec<&str> = store.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"], "position preserved after update");
        let updated = store.get_mut("b").unwrap();
        assert_eq!(updated.question, "updated?");
        assert_eq!(updated.options.as_deref().unwrap().len(), 2);
        assert_eq!(updated.detail.as_deref(), Some("the reasoning"));

        assert!(store.get_mut("missing").is_none());
    }

    #[test]
    fn ask_question_defaults_when_fields_omitted() {
        // R-29.6: an agent may send only `question`; header/multiSelect/options
        // default so the item still deserializes (and an old ask file with no
        // `questions` field leaves `PendingAsk.questions` at None).
        let q: AskQuestion = serde_json::from_str(r#"{"question":"Pick a lane?"}"#).unwrap();
        assert_eq!(q.question, "Pick a lane?");
        assert_eq!(q.header, None);
        assert!(!q.multi_select);
        assert!(q.options.is_empty());

        // Round-trips as camelCase `multiSelect` for the UI contract.
        let full = AskQuestion {
            header: Some("Env".to_string()),
            question: "Which?".to_string(),
            multi_select: true,
            options: vec!["prod".to_string(), "staging".to_string()],
        };
        let v = serde_json::to_value(&full).unwrap();
        assert_eq!(v["multiSelect"], true);
        assert_eq!(v["header"], "Env");
        let back: AskQuestion = serde_json::from_value(v).unwrap();
        assert_eq!(back, full);
    }

    #[test]
    fn drain_all_empties_the_queue() {
        let (mut store, _c) = store_at(0);
        store.push(ask("a", None, 100, 1));
        store.push(ask("b", None, 100, 2));
        let drained = store.drain_all();
        assert_eq!(drained.len(), 2);
        assert!(store.is_empty());
    }
}
