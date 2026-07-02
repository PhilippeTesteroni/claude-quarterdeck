//! Shared test fakes and builders for the deck-core integration tests.
//!
//! (Owned by T1. `hooks_config` tests are owned by T2 and live elsewhere.)

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use deck_core::engine::SessionStore;
use deck_core::events::{HookEvent, SpoolEvent};
use deck_core::traits::{Clock, ProcessTable};

/// A controllable clock. `Send + Sync` (via atomics) so it satisfies the
/// `SessionStore` clock bound.
#[derive(Debug)]
pub struct FakeClock {
    now: AtomicU64,
}

impl FakeClock {
    pub fn new(now_ms: u64) -> Self {
        FakeClock {
            now: AtomicU64::new(now_ms),
        }
    }

    /// Move the clock forward by `delta_ms`.
    pub fn advance(&self, delta_ms: u64) {
        self.now.fetch_add(delta_ms, Ordering::SeqCst);
    }

    /// Jump to an absolute time.
    pub fn set(&self, now_ms: u64) {
        self.now.store(now_ms, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }
}

/// A fake process table: `pid -> process name`. Absent PIDs read as "gone".
#[derive(Debug, Default, Clone)]
pub struct FakeProcessTable {
    names: HashMap<u32, String>,
}

impl FakeProcessTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, pid: u32, name: &str) -> Self {
        self.names.insert(pid, name.to_string());
        self
    }

    pub fn insert(&mut self, pid: u32, name: &str) {
        self.names.insert(pid, name.to_string());
    }

    pub fn kill(&mut self, pid: u32) {
        self.names.remove(&pid);
    }
}

impl ProcessTable for FakeProcessTable {
    fn process_name(&self, pid: u32) -> Option<String> {
        self.names.get(&pid).cloned()
    }
}

/// A store wired to a shared, controllable clock. The returned handle lets the
/// test advance time after construction.
pub fn store_at(now_ms: u64) -> (SessionStore, std::sync::Arc<FakeClock>) {
    // The store owns a boxed clock; we keep an `Arc` alias so the test can drive
    // it. Both point at the same atomic via a thin forwarding wrapper.
    let clock = std::sync::Arc::new(FakeClock::new(now_ms));
    let store = SessionStore::new(Box::new(ArcClock(clock.clone())));
    (store, clock)
}

/// Forwards `Clock` to a shared `Arc<FakeClock>` so tests and the store observe
/// the same time source.
#[derive(Debug)]
pub struct ArcClock(pub std::sync::Arc<FakeClock>);

impl Clock for ArcClock {
    fn now_ms(&self) -> u64 {
        self.0.now_ms()
    }
}

// --- Event builders --------------------------------------------------------

pub fn session_start(id: &str, cwd: &str, ts: u64) -> SpoolEvent {
    SpoolEvent {
        v: 1,
        session_id: id.to_string(),
        received_at_ms: Some(ts),
        cwd: Some(cwd.to_string()),
        transcript_path: None,
        claude_pid: None,
        kind: HookEvent::SessionStart {
            source: Some("startup".to_string()),
            session_title: None,
        },
    }
}

pub fn session_start_full(
    id: &str,
    cwd: &str,
    transcript: Option<&str>,
    pid: Option<u32>,
    title: Option<&str>,
    ts: u64,
) -> SpoolEvent {
    SpoolEvent {
        v: 1,
        session_id: id.to_string(),
        received_at_ms: Some(ts),
        cwd: Some(cwd.to_string()),
        transcript_path: transcript.map(ToString::to_string),
        claude_pid: pid,
        kind: HookEvent::SessionStart {
            source: Some("startup".to_string()),
            session_title: title.map(ToString::to_string),
        },
    }
}

pub fn prompt(id: &str, text: &str, ts: u64) -> SpoolEvent {
    SpoolEvent {
        v: 1,
        session_id: id.to_string(),
        received_at_ms: Some(ts),
        cwd: None,
        transcript_path: None,
        claude_pid: None,
        kind: HookEvent::UserPromptSubmit {
            prompt: Some(text.to_string()),
        },
    }
}

pub fn notification(id: &str, ntype: &str, message: Option<&str>, ts: u64) -> SpoolEvent {
    SpoolEvent {
        v: 1,
        session_id: id.to_string(),
        received_at_ms: Some(ts),
        cwd: None,
        transcript_path: None,
        claude_pid: None,
        kind: HookEvent::Notification {
            message: message.map(ToString::to_string),
            notification_type: Some(ntype.to_string()),
        },
    }
}

pub fn stop(id: &str, ts: u64) -> SpoolEvent {
    SpoolEvent {
        v: 1,
        session_id: id.to_string(),
        received_at_ms: Some(ts),
        cwd: None,
        transcript_path: None,
        claude_pid: None,
        kind: HookEvent::Stop,
    }
}

pub fn session_end(id: &str, reason: &str, ts: u64) -> SpoolEvent {
    SpoolEvent {
        v: 1,
        session_id: id.to_string(),
        received_at_ms: Some(ts),
        cwd: None,
        transcript_path: None,
        claude_pid: None,
        kind: HookEvent::SessionEnd {
            reason: Some(reason.to_string()),
        },
    }
}
