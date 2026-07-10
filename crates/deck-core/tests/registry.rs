//! SPEC §15 live-registry wiring: title precedence (R-15.2), registry-driven
//! discovery + pid→liveness (R-15.3), and the terminal-pid plumbing for
//! foreground suppression (R-17.2).

mod common;

use common::*;
use deck_core::engine::Status;
use deck_core::registry::{merge_registry_into_store, registry_status_to_engine, RegistryEntry};

fn entry(id: &str) -> RegistryEntry {
    RegistryEntry {
        session_id: id.to_string(),
        ..Default::default()
    }
}

#[test]
fn registry_name_outranks_session_title_and_prompt() {
    // R-15.2: registry `name` is the new head of the precedence chain.
    let (mut store, _c) = store_at(1_000);
    store.on_event(&session_start_full(
        "s1",
        "C:/proj",
        None,
        None,
        Some("SessionStart title"),
        1_000,
    ));
    store.on_event(&prompt("s1", "a later prompt", 1_100));
    // Before any registry entry, session_title heads the R-5.2 chain.
    assert_eq!(store.title_of("s1").as_deref(), Some("SessionStart title"));

    let mut e = entry("s1");
    e.name = Some("Renamed via /rename".to_string());
    assert!(
        store.apply_registry(&[e]),
        "registry name changes the title"
    );
    assert_eq!(store.title_of("s1").as_deref(), Some("Renamed via /rename"));
}

#[test]
fn registry_rename_refreshes_within_a_poll_and_clearing_falls_back() {
    let (mut store, _c) = store_at(1_000);
    store.on_event(&prompt("s1", "prompt title", 1_000));

    let mut e = entry("s1");
    e.name = Some("First name".to_string());
    store.apply_registry(&[e.clone()]);
    assert_eq!(store.title_of("s1").as_deref(), Some("First name"));

    // A rename mid-session updates the row on the next poll.
    e.name = Some("Second name".to_string());
    assert!(store.apply_registry(&[e.clone()]));
    assert_eq!(store.title_of("s1").as_deref(), Some("Second name"));

    // Same name again → no change reported (no churn).
    assert!(!store.apply_registry(&[e.clone()]));

    // Registry dropping the name falls back down the chain to the prompt.
    e.name = None;
    assert!(store.apply_registry(&[e]));
    assert_eq!(store.title_of("s1").as_deref(), Some("prompt title"));
}

#[test]
fn absent_session_clears_its_registry_name() {
    // R-15.2 "Registry names refresh on every poll": a live row whose registry
    // file vanished (the session is dropped from the poll while its process is
    // still alive) must not keep displaying its last registry name — the name
    // falls back down the precedence chain, symmetric with the busy-flag
    // clearing. Even an EMPTY poll (the LAST registry file removed) must clear it.
    let (mut store, _c) = store_at(1_000);
    store.on_event(&prompt("s1", "prompt title", 1_000));
    let mut e = entry("s1");
    e.name = Some("Registry name".to_string());
    store.apply_registry(&[e]);
    assert_eq!(store.title_of("s1").as_deref(), Some("Registry name"));

    // Next poll no longer reports s1 at all (empty registry). The name must clear.
    assert!(
        store.apply_registry(&[]),
        "clearing a vanished session's registry name is a visible change"
    );
    assert_eq!(
        store.title_of("s1").as_deref(),
        Some("prompt title"),
        "name falls back down the chain once the registry drops the session"
    );
    // Idempotent: a second empty poll reports no further change.
    assert!(!store.apply_registry(&[]));
}

#[test]
fn registry_pid_feeds_liveness() {
    // R-15.3: the registry pid feeds liveness directly. A session with no hook
    // pid gains one from the registry, and a dead process then marks it dead.
    let (mut store, _c) = store_at(1_000);
    store.on_event(&session_start("s1", "C:/proj", 1_000));
    assert_eq!(store.status_of("s1"), Some(Status::Idle));

    let mut e = entry("s1");
    e.pid = Some(4242);
    store.apply_registry(&[e]);

    // The registry pid is now what liveness checks: a process table without it
    // → dead (R-6.1).
    let procs = FakeProcessTable::new();
    store.poll_liveness(&procs, |_| None);
    assert_eq!(store.status_of("s1"), Some(Status::Dead));

    // With the pid alive and named like claude, it stays alive.
    let (mut store2, _c2) = store_at(1_000);
    store2.on_event(&session_start("s2", "C:/proj", 1_000));
    let mut e2 = entry("s2");
    e2.pid = Some(4242);
    store2.apply_registry(&[e2]);
    let procs2 = FakeProcessTable::new().with(4242, "node.exe");
    store2.poll_liveness(&procs2, |_| None);
    assert_ne!(store2.status_of("s2"), Some(Status::Dead));
}

#[test]
fn registry_discovery_creates_rows_for_unknown_sessions() {
    // R-15.3: registry-driven discovery adds inferred rows for registry entries
    // whose transcript is missing/stale (i.e. not already known).
    let (mut store, _c) = store_at(5_000);
    let mut busy = entry("reg-busy");
    busy.name = Some("Background refactor".to_string());
    busy.cwd = Some("C:/Users/phil/proj".to_string());
    busy.status = Some("busy".to_string());
    busy.pid = Some(999);
    let mut idle = entry("reg-idle");
    idle.status = Some("idle".to_string());

    let inserted = merge_registry_into_store(&mut store, &[busy, idle], 5_000);
    assert_eq!(inserted, 2);

    // busy → working, idle → idle; both flagged inferred with the registry name.
    assert_eq!(store.status_of("reg-busy"), Some(Status::Working));
    assert_eq!(store.status_of("reg-idle"), Some(Status::Idle));
    assert_eq!(
        store.title_of("reg-busy").as_deref(),
        Some("Background refactor")
    );
    let view = store.view();
    assert!(view.iter().all(|v| v.inferred));
    assert_eq!(
        view.iter().find(|v| v.id == "reg-busy").unwrap().project,
        "proj"
    );

    // Re-running discovery does not duplicate a now-known session.
    let again = merge_registry_into_store(&mut store, &[entry("reg-busy")], 6_000);
    assert_eq!(again, 0);
}

#[test]
fn registry_discovery_skips_hook_known_sessions() {
    let (mut store, _c) = store_at(1_000);
    store.on_event(&session_start("s1", "C:/proj", 1_000));
    let inserted = merge_registry_into_store(&mut store, &[entry("s1")], 1_000);
    assert_eq!(inserted, 0, "a hook-known session is not re-created");
    assert_eq!(store.len(), 1);
}

#[test]
fn status_mapping_busy_is_working_else_idle() {
    assert_eq!(registry_status_to_engine(Some("busy")), Status::Working);
    assert_eq!(registry_status_to_engine(Some("BUSY")), Status::Working);
    assert_eq!(registry_status_to_engine(Some("idle")), Status::Idle);
    assert_eq!(registry_status_to_engine(Some("anything")), Status::Idle);
    assert_eq!(registry_status_to_engine(None), Status::Idle);
}

#[test]
fn terminal_pids_carry_the_registry_claude_pid() {
    // R-17.2 matching set: a session's terminal pid is the registry/claude pid
    // that hosts it.
    let (mut store, _c) = store_at(1_000);
    store.on_event(&session_start("s1", "C:/proj", 1_000));
    let mut e = entry("s1");
    e.pid = Some(2222);
    store.apply_registry(&[e]);

    let pids = store.terminal_pids();
    let s1 = pids.iter().find(|(id, _)| id == "s1").unwrap();
    assert!(s1.1.contains(&2222));

    // A session with no known pid is omitted (nothing to match).
    store.on_event(&session_start("s2", "C:/proj2", 1_000));
    let pids = store.terminal_pids();
    assert!(pids.iter().all(|(id, _)| id != "s2"));
}
