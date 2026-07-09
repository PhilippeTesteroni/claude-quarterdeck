//! R-6 liveness (via a fake ProcessTable) and R-2.5 dead retention/pruning.

mod common;

use common::*;
use deck_core::engine::Status;

const T0: u64 = 1_751_000_000_000;
const SIX_H: u64 = 6 * 60 * 60 * 1000;

#[test]
fn pid_backed_session_with_matching_name_stays_alive() {
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start_full(
        "a",
        "/p",
        Some("/t/a.jsonl"),
        Some(1234),
        None,
        T0,
    ));
    let procs = FakeProcessTable::new().with(1234, "node.exe");
    s.poll_liveness(&procs, |_p| Some(T0));
    assert_eq!(s.status_of("a"), Some(Status::Idle));
}

#[test]
fn pid_gone_marks_dead() {
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start_full(
        "a",
        "/p",
        Some("/t/a.jsonl"),
        Some(1234),
        None,
        T0,
    ));
    let procs = FakeProcessTable::new(); // pid 1234 absent
    s.poll_liveness(&procs, |_p| Some(T0));
    assert_eq!(s.status_of("a"), Some(Status::Dead));
}

#[test]
fn pid_reused_by_foreign_process_marks_dead() {
    // R-6.1: name must still match claude|node|bun.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start_full(
        "a",
        "/p",
        Some("/t/a.jsonl"),
        Some(1234),
        None,
        T0,
    ));
    let procs = FakeProcessTable::new().with(1234, "notepad.exe");
    s.poll_liveness(&procs, |_p| Some(T0));
    assert_eq!(s.status_of("a"), Some(Status::Dead));
}

#[test]
fn accepts_claude_and_bun_process_names() {
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start_full("c", "/p", None, Some(1), None, T0));
    s.on_event(&session_start_full("b", "/p", None, Some(2), None, T0));
    let procs = FakeProcessTable::new().with(1, "claude").with(2, "bun");
    s.poll_liveness(&procs, |_p| None);
    assert_eq!(s.status_of("c"), Some(Status::Idle));
    assert_eq!(s.status_of("b"), Some(Status::Idle));
}

#[test]
fn inferred_no_pid_dead_when_transcript_stale_over_6h() {
    // R-6.2
    let (mut s, _c) = store_at(T0);
    s.add_inferred(
        "inf".into(),
        Some("/p".into()),
        Some("/t/inf.jsonl".into()),
        Status::Idle,
        "title".into(),
        T0 - SIX_H - 1,
    );
    let procs = FakeProcessTable::new();
    s.poll_liveness(&procs, |_p| Some(T0 - SIX_H - 1));
    assert_eq!(s.status_of("inf"), Some(Status::Dead));
}

#[test]
fn inferred_no_pid_alive_when_transcript_fresh() {
    let (mut s, _c) = store_at(T0);
    s.add_inferred(
        "inf".into(),
        Some("/p".into()),
        Some("/t/inf.jsonl".into()),
        Status::Working,
        "title".into(),
        T0 - 1000,
    );
    let procs = FakeProcessTable::new();
    s.poll_liveness(&procs, |_p| Some(T0 - 1000));
    assert_eq!(s.status_of("inf"), Some(Status::Working));
}

#[test]
fn no_pid_no_transcript_is_dead() {
    let (mut s, _c) = store_at(T0);
    // A session created by a Notification carries no PID and no transcript.
    s.on_event(&notification("ghost", "permission_prompt", None, T0));
    let procs = FakeProcessTable::new();
    s.poll_liveness(&procs, |_p| None);
    assert_eq!(s.status_of("ghost"), Some(Status::Dead));
}

#[test]
fn registry_discovered_pidless_no_transcript_survives_on_fresh_registry() {
    // R-15.3 vs R-6: a registry entry that omits its pid discovers a row with
    // NO pid and NO transcript. Its fresh registry `updatedAt` is the only
    // activity signal — it must NOT be declared dead on the very next liveness
    // tick; it persists (registry-driven discovery), honoring R-6.2's grace.
    use deck_core::registry::{merge_registry_into_store, RegistryEntry};
    let (mut s, _c) = store_at(T0);
    let entry = RegistryEntry {
        session_id: "reg".into(),
        cwd: Some("/p".into()),
        name: Some("Background job".into()),
        status: Some("busy".into()),
        updated_at_ms: Some(T0 - 5_000), // fresh, no pid, no transcript
        ..Default::default()
    };
    merge_registry_into_store(&mut s, std::slice::from_ref(&entry), T0);
    assert!(s.contains("reg"));
    let procs = FakeProcessTable::new();
    // No pid, no transcript, but a fresh registry updatedAt → still alive.
    s.poll_liveness(&procs, |_p| None);
    assert_ne!(s.status_of("reg"), Some(Status::Dead));

    // Once the registry entry vanishes, its updatedAt is cleared → the pid-less,
    // transcript-less row correctly falls through to dead on the next tick.
    s.apply_registry(&[]);
    s.poll_liveness(&procs, |_p| None);
    assert_eq!(s.status_of("reg"), Some(Status::Dead));
}

#[test]
fn registry_pidless_dead_when_registry_updatedat_stale_over_6h() {
    // R-6.2 grace still bounds it: a registry-discovered pid-less row whose only
    // signal (registry updatedAt) is stale > 6h is dead.
    use deck_core::registry::{merge_registry_into_store, RegistryEntry};
    let (mut s, _c) = store_at(T0);
    let entry = RegistryEntry {
        session_id: "old".into(),
        cwd: Some("/p".into()),
        updated_at_ms: Some(T0 - SIX_H - 1),
        ..Default::default()
    };
    merge_registry_into_store(&mut s, std::slice::from_ref(&entry), T0);
    let procs = FakeProcessTable::new();
    s.poll_liveness(&procs, |_p| None);
    assert_eq!(s.status_of("old"), Some(Status::Dead));
}

#[test]
fn dead_row_persists_5min_then_is_pruned() {
    // R-2.5
    let (mut s, c) = store_at(T0);
    s.on_event(&session_start_full("a", "/p", None, Some(1234), None, T0));
    let procs = FakeProcessTable::new(); // gone
    s.poll_liveness(&procs, |_p| None);
    assert_eq!(s.status_of("a"), Some(Status::Dead));

    // Before 5 minutes: still present.
    c.set(T0 + 4 * 60 * 1000);
    assert!(s.prune_dead().is_empty());
    assert!(s.contains("a"));

    // At/after 5 minutes: pruned.
    c.set(T0 + 5 * 60 * 1000);
    let removed = s.prune_dead();
    assert_eq!(removed, vec!["a".to_string()]);
    assert!(!s.contains("a"));
}

#[test]
fn session_end_removes_immediately_even_when_not_dead() {
    // R-2.5: SessionEnd always wins immediately.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&session_end("a", "clear", T0 + 1));
    assert!(!s.contains("a"));
}

#[test]
fn tick_runs_recovery_liveness_and_prune_together() {
    let (mut s, c) = store_at(T0);
    // One session that will die (pid gone), one that recovers.
    s.on_event(&session_start_full(
        "dead",
        "/p",
        Some("/t/d.jsonl"),
        Some(1),
        None,
        T0,
    ));
    s.on_event(&session_start_full(
        "rec",
        "/p",
        Some("/t/r.jsonl"),
        Some(2),
        None,
        T0,
    ));
    s.on_event(&notification("rec", "permission_prompt", None, T0 + 10));
    assert_eq!(s.status_of("rec"), Some(Status::Attention));

    let procs = FakeProcessTable::new().with(2, "node.exe"); // pid 1 gone
    c.set(T0 + 20_000);
    s.tick(&procs, |path| {
        if path == "/t/r.jsonl" {
            Some(T0 + 15_000) // advanced well past the notification
        } else {
            None
        }
    });
    assert_eq!(s.status_of("dead"), Some(Status::Dead));
    assert_eq!(s.status_of("rec"), Some(Status::Working));
}

#[test]
fn dead_overrides_pending_ask() {
    // A process that vanished while an ask was pending is still dead.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start_full("a", "/p", None, Some(1), None, T0));
    s.note_pending_ask("a");
    assert_eq!(s.status_of("a"), Some(Status::Attention));
    let procs = FakeProcessTable::new(); // pid gone
    s.poll_liveness(&procs, |_p| None);
    assert_eq!(s.status_of("a"), Some(Status::Dead));
}

#[test]
fn liveness_dead_reports_the_session_as_gone_once() {
    // R-32.2: a session the liveness poll turns `dead` is reported via
    // `take_gone_sessions` so the shell can cancel its pending asks + drop its
    // perms. It is reported exactly once — a still-dead row on later polls is
    // skipped (its status is already `dead`), so the queue does not re-report it.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start_full("a", "/p", None, Some(1), None, T0));
    // A live session has nothing to report.
    assert!(s.take_gone_sessions().is_empty());

    let procs = FakeProcessTable::new(); // pid 1 gone
    s.poll_liveness(&procs, |_p| None);
    assert_eq!(s.status_of("a"), Some(Status::Dead));
    assert_eq!(s.take_gone_sessions(), vec!["a".to_string()]);

    // Already dead → not re-reported on the next poll.
    s.poll_liveness(&procs, |_p| None);
    assert!(s.take_gone_sessions().is_empty());
}

#[test]
fn session_end_reports_the_session_as_gone() {
    // R-32.2: a genuine `SessionEnd` reports the id (so the shell cancels the
    // agent's pending asks with `kind:"cancelled"` and drops its perms), even
    // though the row itself is removed immediately (R-2.5).
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start_full("a", "/p", None, Some(1), None, T0));
    assert!(s.take_gone_sessions().is_empty());

    s.on_event(&session_end("a", "clear", T0 + 5));
    assert!(!s.contains("a"), "SessionEnd removes the row (R-2.5)");
    assert_eq!(s.take_gone_sessions(), vec!["a".to_string()]);
}

#[test]
fn stale_reordered_session_end_does_not_report_gone() {
    // A `SessionEnd` older than the row's own `SessionStart` belongs to a
    // previous incarnation of a reused id and is ignored (R-2.5) — it must NOT
    // report the live session as gone (that would wrongly cancel its asks).
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start_full("a", "/p", None, Some(1), None, T0 + 100));
    let _ = s.take_gone_sessions();
    // End stamped BEFORE this incarnation's start → ignored.
    s.on_event(&session_end("a", "clear", T0 + 50));
    assert!(s.contains("a"), "stale reordered End is ignored");
    assert!(s.take_gone_sessions().is_empty());
}
