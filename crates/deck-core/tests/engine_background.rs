//! SPEC Â§21 (background work shows as working) + Â§22 (honest time-in-status for
//! pre-existing sessions). Every rule has an injectable-clock test.
//!
//! Â§21: registry busy-override precedence/staleness/reset (R-21.1), the
//! self-correcting subagent badge (R-21.2), the finished-toast-despite-override
//! rule (R-21.3). Â§22: discovery time seeding precedence (R-22.1), the
//! estimateâ†’exact upgrade on the first hook event (R-22.2), the session-age
//! anchor (R-22.3), and the estimated `~` (inferred) marker (R-22.4).

mod common;

use common::*;
use deck_core::engine::{Effect, SessionStore, Status, REGISTRY_BUSY_FRESH_MS};
use deck_core::events::{HookEvent, SpoolEvent};
use deck_core::registry::RegistryEntry;
use deck_core::traits::ToastKind;

const T0: u64 = 1_751_000_000_000;

fn busy_entry(id: &str, updated_at_ms: u64) -> RegistryEntry {
    RegistryEntry {
        session_id: id.to_string(),
        status: Some("busy".to_string()),
        updated_at_ms: Some(updated_at_ms),
        ..Default::default()
    }
}

fn subagent(id: &str, kind: HookEvent, ts: u64) -> SpoolEvent {
    SpoolEvent {
        v: 1,
        session_id: id.to_string(),
        received_at_ms: Some(ts),
        cwd: None,
        transcript_path: None,
        claude_pid: None,
        kind,
    }
}

// --- R-21.1 registry busy-override -----------------------------------------

#[test]
fn busy_override_promotes_hook_idle_to_working() {
    // A session whose Stop hook fired (idle) but whose live registry entry says
    // busy with a fresh updatedAt displays working (R-21.1).
    let (mut store, clock) = store_at(T0);
    store.on_event(&session_start("s", "/p", T0));
    store.on_event(&stop("s", T0 + 10));
    assert_eq!(store.status_of("s"), Some(Status::Idle));

    clock.set(T0 + 1_000);
    store.apply_registry(&[busy_entry("s", T0 + 1_000)]);
    assert_eq!(
        store.status_of("s"),
        Some(Status::Working),
        "fresh busy registry overrides hook-idle"
    );
    // R-21.3: the tray follows the displayed (overridden) status.
    assert_eq!(store.worst_status(), Some(Status::Working));
}

#[test]
fn busy_override_ignored_when_registry_updated_at_is_stale() {
    // The override only bites for a FRESH updatedAt (< 30 s). A stale one is
    // ignored (R-21.1).
    let (mut store, clock) = store_at(T0);
    store.on_event(&session_start("s", "/p", T0));
    store.on_event(&stop("s", T0 + 10));

    clock.set(T0 + 5 * REGISTRY_BUSY_FRESH_MS);
    // updatedAt is far in the past relative to now â†’ stale.
    store.apply_registry(&[busy_entry("s", T0)]);
    assert_eq!(store.status_of("s"), Some(Status::Idle));
}

#[test]
fn busy_override_clears_when_registry_goes_non_busy() {
    let (mut store, clock) = store_at(T0);
    store.on_event(&session_start("s", "/p", T0));
    store.on_event(&stop("s", T0 + 10));
    clock.set(T0 + 1_000);
    store.apply_registry(&[busy_entry("s", T0 + 1_000)]);
    assert_eq!(store.status_of("s"), Some(Status::Working));

    // Registry now reports idle â†’ override clears â†’ back to hook-idle.
    clock.set(T0 + 2_000);
    let mut e = busy_entry("s", T0 + 2_000);
    e.status = Some("idle".to_string());
    store.apply_registry(&[e]);
    assert_eq!(store.status_of("s"), Some(Status::Idle));
}

#[test]
fn busy_override_clears_when_registry_entry_disappears() {
    // The registry file vanished (session ended registry-side) â†’ the override
    // must not wedge the row on `working` (R-21.1 absent-session clearing).
    let (mut store, clock) = store_at(T0);
    store.on_event(&session_start("s", "/p", T0));
    store.on_event(&stop("s", T0 + 10));
    clock.set(T0 + 1_000);
    store.apply_registry(&[busy_entry("s", T0 + 1_000)]);
    assert_eq!(store.status_of("s"), Some(Status::Working));

    clock.set(T0 + 2_000);
    // An empty-but-nonzero poll for OTHER sessions won't reach the running app
    // path (guarded by !entries.is_empty()); model the vanished file with a poll
    // that contains a different session only.
    store.apply_registry(&[busy_entry("other", T0 + 2_000)]);
    assert_eq!(
        store.status_of("s"),
        Some(Status::Idle),
        "override cleared once the session left the registry"
    );
}

#[test]
fn attention_outranks_busy_override() {
    // A pending ask (attention) always outranks the busy-override (R-21.1).
    let (mut store, clock) = store_at(T0);
    store.on_event(&session_start("s", "/p", T0));
    store.on_event(&stop("s", T0 + 10));
    clock.set(T0 + 1_000);
    store.apply_registry(&[busy_entry("s", T0 + 1_000)]);
    assert_eq!(store.status_of("s"), Some(Status::Working));

    store.note_pending_ask("s");
    assert_eq!(
        store.status_of("s"),
        Some(Status::Attention),
        "pending ask outranks the override"
    );

    // Answering it drops back to the override-driven working.
    store.note_ask_answered("s");
    // note_ask_answered on a non-attention-from-hook ask resumes to working
    // directly; either way the row is working, not idle.
    assert_eq!(store.status_of("s"), Some(Status::Working));
}

#[test]
fn hook_attention_is_not_overridden_by_busy() {
    // A hook-derived attention (permission prompt) is not `idle`, so the
    // override never applies (R-21.1 "only idle is overridden").
    let (mut store, clock) = store_at(T0);
    store.on_event(&session_start("s", "/p", T0));
    store.on_event(&notification(
        "s",
        "permission_prompt",
        Some("Allow?"),
        T0 + 10,
    ));
    clock.set(T0 + 1_000);
    store.apply_registry(&[busy_entry("s", T0 + 1_000)]);
    assert_eq!(store.status_of("s"), Some(Status::Attention));
}

#[test]
fn busy_override_ages_out_on_tick_without_a_fresh_registry_poll() {
    // R-21.1: even if the shell stops polling the registry (e.g. the sessions
    // dir went empty, so `apply_registry` is skipped), the override must age out
    // to stale on the plain tick from the last-seen `updatedAt`.
    let (mut store, clock) = store_at(T0);
    // Keep the process alive so liveness doesn't mark it dead â€” we're isolating
    // the override-aging behavior.
    let procs = FakeProcessTable::new().with(1, "node");
    store.on_event(&session_start_full(
        "s",
        "/p",
        Some("/p/s.jsonl"),
        Some(1),
        None,
        T0,
    ));
    store.on_event(&stop("s", T0 + 10));
    clock.set(T0 + 1_000);
    store.apply_registry(&[busy_entry("s", T0 + 1_000)]);
    assert_eq!(store.status_of("s"), Some(Status::Working));

    // Time marches well past the freshness window with no further registry poll.
    clock.set(T0 + 1_000 + 2 * REGISTRY_BUSY_FRESH_MS);
    store.tick(&procs, |_| None);
    assert_eq!(
        store.status_of("s"),
        Some(Status::Idle),
        "override aged out to stale on the tick alone"
    );
}

// --- §44 / R-44 registry-driven demote (ESC-interrupt) ---------------------

fn idle_entry(id: &str, updated_at_ms: u64) -> RegistryEntry {
    RegistryEntry {
        session_id: id.to_string(),
        status: Some("idle".to_string()),
        updated_at_ms: Some(updated_at_ms),
        ..Default::default()
    }
}

#[test]
fn registry_idle_demotes_esc_wedged_working() {
    // R-44: an ESC-interrupt fires no Stop hook, so the hook status wedges on
    // `working` and the deck stays stuck 🟡. Claude writes the registry status to
    // idle; a fresh idle poll (updatedAt after the last hook event) demotes the
    // row to idle.
    let (mut store, clock) = store_at(T0);
    store.on_event(&session_start("s", "/p", T0));
    store.on_event(&prompt("s", "go", T0 + 10)); // hook working, last activity T0+10
    // Registry was busy while the turn genuinely ran.
    clock.set(T0 + 1_000);
    store.apply_registry(&[busy_entry("s", T0 + 1_000)]);
    assert_eq!(store.status_of("s"), Some(Status::Working));

    // ESC: no Stop hook fires; Claude writes the registry idle (fresh).
    clock.set(T0 + 2_000);
    store.apply_registry(&[idle_entry("s", T0 + 2_000)]);
    assert_eq!(
        store.status_of("s"),
        Some(Status::Idle),
        "fresh registry idle demotes the ESC-stuck working row (R-44)"
    );
    // Tray follows the demoted status.
    assert_eq!(store.worst_status(), Some(Status::Idle));
}

#[test]
fn registry_waiting_status_also_demotes() {
    // R-44: the `waiting` registry status string is handled the same as `idle`.
    let (mut store, clock) = store_at(T0);
    store.on_event(&session_start("s", "/p", T0));
    store.on_event(&prompt("s", "go", T0 + 10));
    assert_eq!(store.status_of("s"), Some(Status::Working));

    clock.set(T0 + 2_000);
    let mut e = idle_entry("s", T0 + 2_000);
    e.status = Some("waiting".to_string());
    store.apply_registry(&[e]);
    assert_eq!(
        store.status_of("s"),
        Some(Status::Idle),
        "registry `waiting` demotes just like `idle` (R-44)"
    );
}

#[test]
fn busy_registry_does_not_demote_genuine_working() {
    // R-44: a genuinely-busy registry must NEVER demote — the guard is the
    // explicit idle/waiting signal, and busy never sets it. Repeated busy polls
    // and a plain tick all leave the row working.
    let procs = FakeProcessTable::new().with(1, "node");
    let (mut store, clock) = store_at(T0);
    store.on_event(&session_start_full(
        "s",
        "/p",
        Some("/p/s.jsonl"),
        Some(1),
        None,
        T0,
    ));
    store.on_event(&prompt("s", "go", T0 + 10));
    clock.set(T0 + 1_000);
    store.apply_registry(&[busy_entry("s", T0 + 1_000)]);
    clock.set(T0 + 2_000);
    store.apply_registry(&[busy_entry("s", T0 + 2_000)]);
    clock.set(T0 + 3_000);
    store.tick(&procs, |_| Some(T0 + 3_000));
    assert_eq!(
        store.status_of("s"),
        Some(Status::Working),
        "a busy registry never demotes"
    );
}

#[test]
fn fresher_hook_prompt_is_not_clobbered_by_a_stale_idle_registry() {
    // R-44 "no fresher hook activity": a registry idle whose updatedAt PREDATES
    // the last hook event must not demote — a new UserPromptSubmit that re-armed
    // `working` after the registry went idle wins.
    let (mut store, clock) = store_at(T0);
    store.on_event(&session_start("s", "/p", T0)); // hook idle
    // Registry reports idle at T0+100 (harmless: hook is already idle).
    clock.set(T0 + 100);
    store.apply_registry(&[idle_entry("s", T0 + 100)]);
    // A genuine new turn starts AFTER that registry write.
    store.on_event(&prompt("s", "go", T0 + 200)); // hook working, last activity T0+200
    assert_eq!(store.status_of("s"), Some(Status::Working));
    // The registry file hasn't been rewritten (same stale updatedAt) — a re-poll
    // must NOT demote the fresher working turn.
    clock.set(T0 + 300);
    store.apply_registry(&[idle_entry("s", T0 + 100)]);
    assert_eq!(
        store.status_of("s"),
        Some(Status::Working),
        "a registry idle older than the last hook event does not demote"
    );
}

#[test]
fn stale_registry_idle_does_not_demote() {
    // R-44: the demote only fires for a FRESH updatedAt (< 30 s), symmetric with
    // the busy-override freshness. A stale idle poll is ignored.
    let (mut store, clock) = store_at(T0);
    store.on_event(&session_start("s", "/p", T0));
    store.on_event(&prompt("s", "go", T0 + 10));
    // updatedAt is far in the past relative to now → stale.
    clock.set(T0 + 5 * REGISTRY_BUSY_FRESH_MS);
    store.apply_registry(&[idle_entry("s", T0)]);
    assert_eq!(
        store.status_of("s"),
        Some(Status::Working),
        "a stale registry idle is not authoritative enough to demote"
    );
}

#[test]
fn reverse_gear_recovery_promoted_row_is_owned_by_transcript_not_registry() {
    // R-44 interplay with §30: a row promoted to `working` by transcript recovery
    // (reverse gear) is owned by the transcript-quiescence demote, NOT §44 — a
    // registry idle poll must not fight it while the transcript is still
    // advancing. §30 demotes it on the first tick with no advance.
    let procs = FakeProcessTable::new().with(1, "node");
    let (mut store, clock) = store_at(T0);
    store.on_event(&session_start_full(
        "s",
        "/p",
        Some("/p/s.jsonl"),
        Some(1),
        None,
        T0,
    ));
    store.on_event(&stop("s", T0 + 10)); // hook idle at T0+10
    // Transcript advances ≥2 s past the idle anchor → §30 promotes to working.
    clock.set(T0 + 20_000);
    store.poll_recovery(|_| Some(T0 + 13_000));
    assert_eq!(store.status_of("s"), Some(Status::Working));

    // A fresh registry idle poll must NOT demote the recovery-promoted row.
    store.apply_registry(&[idle_entry("s", T0 + 20_000)]);
    assert_eq!(
        store.status_of("s"),
        Some(Status::Working),
        "§44 does not demote a §30 recovery-promoted row (transcript owns it)"
    );

    // §30 still owns the demote: a tick with no transcript advance drops it.
    clock.set(T0 + 30_000);
    store.tick(&procs, |_| Some(T0 + 13_000));
    assert_eq!(
        store.status_of("s"),
        Some(Status::Idle),
        "§30 transcript-quiescence demote still fires"
    );
}

#[test]
fn registry_demote_is_re_evaluated_on_the_tick() {
    // R-44: the demote is checked on the plain tick too (poll_busy_override), not
    // only on the registry poll — so it stays consistent as time advances and the
    // §30 recovery path (which shares the tick) does not re-promote a legitimately
    // demoted, quiescent-transcript row.
    let procs = FakeProcessTable::new().with(1, "node");
    let (mut store, clock) = store_at(T0);
    store.on_event(&session_start_full(
        "s",
        "/p",
        Some("/p/s.jsonl"),
        Some(1),
        None,
        T0,
    ));
    store.on_event(&prompt("s", "go", T0 + 10)); // working, last activity T0+10
    clock.set(T0 + 1_000);
    store.apply_registry(&[idle_entry("s", T0 + 1_000)]); // demotes to idle
    assert_eq!(store.status_of("s"), Some(Status::Idle));
    // A subsequent tick keeps it idle (transcript held quiescent so §30 doesn't
    // re-promote, and the registry-demote re-check is a no-op on an idle hook).
    clock.set(T0 + 2_000);
    store.tick(&procs, |_| Some(T0 + 10));
    assert_eq!(store.status_of("s"), Some(Status::Idle));
}

// --- R-21.3 finished toast despite override --------------------------------

#[test]
fn stop_still_fires_finished_toast_even_when_override_keeps_it_working() {
    // R-21.3: the Stop's "finished" toast fires (the turn DID finish) even
    // though the live busy-override immediately flips the row back to working.
    let (mut store, clock) = store_at(T0);
    store.on_event(&session_start_full(
        "s",
        "/p/proj",
        None,
        None,
        Some("A task"),
        T0,
    ));
    store.on_event(&prompt("s", "go", T0 + 10)); // working
    clock.set(T0 + 20);
    // Registry already says busy (fresh) before the Stop lands.
    store.apply_registry(&[busy_entry("s", T0 + 20)]);
    assert_eq!(store.status_of("s"), Some(Status::Working));

    let fx = store.on_event(&stop("s", T0 + 30));
    // The row still displays working (override), but a finished toast fired.
    assert_eq!(store.status_of("s"), Some(Status::Working));
    let kinds: Vec<ToastKind> = fx.iter().map(|Effect::Toast(t)| t.kind).collect();
    assert_eq!(kinds, [ToastKind::Idle], "finished toast fires (R-21.3)");
}

#[test]
fn override_flip_emits_no_toast() {
    // R-21.3: no toast on the idleâ†’working override flip itself (not a
    // user-actionable event). apply_registry returns a changed flag, no Effects.
    let (mut store, clock) = store_at(T0);
    store.on_event(&session_start("s", "/p", T0));
    store.on_event(&prompt("s", "go", T0 + 5)); // working, so the Stop transitions
    let fx = store.on_event(&stop("s", T0 + 10));
    assert_eq!(
        fx.iter().map(|Effect::Toast(t)| t.kind).collect::<Vec<_>>(),
        [ToastKind::Idle]
    );
    clock.set(T0 + 1_000);
    // apply_registry has no toast channel at all â€” the flip is silent.
    let changed = store.apply_registry(&[busy_entry("s", T0 + 1_000)]);
    assert!(changed, "the displayed status changed");
    assert_eq!(store.status_of("s"), Some(Status::Working));
}

// --- R-21.2 subagent badge -------------------------------------------------

#[test]
fn subagent_counter_increments_and_decrements() {
    let (mut store, _c) = store_at(T0);
    store.on_event(&session_start("s", "/p", T0));
    store.on_event(&prompt("s", "go", T0 + 10)); // working
    store.on_event(&subagent("s", HookEvent::SubagentStart, T0 + 20));
    store.on_event(&subagent("s", HookEvent::SubagentStart, T0 + 21));
    assert_eq!(store.subagents_of("s"), Some(2));
    store.on_event(&subagent("s", HookEvent::SubagentStop, T0 + 30));
    assert_eq!(store.subagents_of("s"), Some(1));
    // Surfaced on the view for the badge.
    let v = store.view();
    let row = v.iter().find(|r| r.id == "s").unwrap();
    assert_eq!(row.subagents, 1);
}

#[test]
fn subagent_counter_saturates_on_extra_stop() {
    let (mut store, _c) = store_at(T0);
    store.on_event(&session_start("s", "/p", T0));
    store.on_event(&prompt("s", "go", T0 + 10));
    store.on_event(&subagent("s", HookEvent::SubagentStop, T0 + 20));
    assert_eq!(
        store.subagents_of("s"),
        Some(0),
        "saturating, never underflows"
    );
}

#[test]
fn lost_subagent_stop_is_self_corrected_by_registry_non_busy() {
    // R-21.2: a lost SubagentStop must never wedge the badge. The registry
    // reporting the session non-busy zeroes the counter.
    let (mut store, clock) = store_at(T0);
    store.on_event(&session_start("s", "/p", T0));
    clock.set(T0 + 1_000);
    // Registry busy keeps the row working; two subagents counted.
    store.apply_registry(&[busy_entry("s", T0 + 1_000)]);
    store.on_event(&subagent("s", HookEvent::SubagentStart, T0 + 1_010));
    store.on_event(&subagent("s", HookEvent::SubagentStart, T0 + 1_020));
    assert_eq!(store.subagents_of("s"), Some(2));

    // Registry flips to idle (the background work finished; one Stop was lost).
    clock.set(T0 + 2_000);
    let mut e = busy_entry("s", T0 + 2_000);
    e.status = Some("idle".to_string());
    store.apply_registry(&[e]);
    assert_eq!(
        store.subagents_of("s"),
        Some(0),
        "badge self-corrected to 0"
    );
}

#[test]
fn stop_with_open_subagent_shows_waiting_workflow_not_zeroed() {
    // §43 (the bug fix): a fresh Stop (parent turn ended → hook idle) while a
    // background subagent is still open must NOT zero the counter — the row shows
    // blue `WaitingWorkflow`, not green idle (the §21 regression where the
    // multi-agent indicator vanished the instant the parent hit Stop). A real
    // `SubagentStop` balance back to 0 then clears it and drops to idle.
    let (mut store, _c) = store_at(T0);
    store.on_event(&session_start("s", "/p", T0));
    store.on_event(&prompt("s", "go", T0 + 10));
    store.on_event(&subagent("s", HookEvent::SubagentStart, T0 + 20));
    assert_eq!(store.subagents_of("s"), Some(1));

    store.on_event(&stop("s", T0 + 30));
    assert_eq!(
        store.status_of("s"),
        Some(Status::WaitingWorkflow),
        "hook idle + open subagent => blue waiting-workflow, not idle"
    );
    assert_eq!(
        store.subagents_of("s"),
        Some(1),
        "counter is NOT zeroed by a clean Stop while a subagent is still open"
    );
    // The blue row aggregates as WaitingWorkflow on the tray worst-of too.
    assert_eq!(store.worst_status(), Some(Status::WaitingWorkflow));

    // A genuine SubagentStop balance returning to 0 clears it → back to idle.
    store.on_event(&subagent("s", HookEvent::SubagentStop, T0 + 40));
    assert_eq!(store.subagents_of("s"), Some(0));
    assert_eq!(store.status_of("s"), Some(Status::Idle));
}

#[test]
fn dead_clears_the_subagent_badge_even_with_open_subagents() {
    // §43: a genuine leak still clears — liveness `dead` (the process is gone)
    // zeroes the counter and the blue WaitingWorkflow with it, unlike a clean Stop.
    let (mut store, clock) = store_at(T0);
    // PID-backed row so liveness can find the process gone.
    let procs = FakeProcessTable::new(); // pid 1 absent => dead
    store.on_event(&session_start_full(
        "s",
        "/p",
        Some("/p/s.jsonl"),
        Some(1),
        None,
        T0,
    ));
    store.on_event(&prompt("s", "go", T0 + 10));
    store.on_event(&subagent("s", HookEvent::SubagentStart, T0 + 20));
    store.on_event(&stop("s", T0 + 30));
    assert_eq!(store.status_of("s"), Some(Status::WaitingWorkflow));
    assert_eq!(store.subagents_of("s"), Some(1));

    clock.set(T0 + 40);
    store.poll_liveness(&procs, |_| None);
    assert_eq!(store.status_of("s"), Some(Status::Dead));
    assert_eq!(
        store.subagents_of("s"),
        Some(0),
        "dead reaps the badge (genuine leak), unlike a clean Stop"
    );
}

#[test]
fn attention_resets_the_badge() {
    // R-21.2: going to attention (permission prompt) zeroes the counter.
    let (mut store, _c) = store_at(T0);
    store.on_event(&session_start("s", "/p", T0));
    store.on_event(&prompt("s", "go", T0 + 10));
    store.on_event(&subagent("s", HookEvent::SubagentStart, T0 + 20));
    assert_eq!(store.subagents_of("s"), Some(1));
    store.on_event(&notification(
        "s",
        "permission_prompt",
        Some("Allow?"),
        T0 + 30,
    ));
    assert_eq!(store.subagents_of("s"), Some(0));
}

#[test]
fn subagent_badge_survives_across_a_busy_override_poll() {
    // While the row displays working via the override, the badge is kept across
    // registry polls (the background subagents are genuinely running).
    let (mut store, clock) = store_at(T0);
    store.on_event(&session_start("s", "/p", T0));
    store.on_event(&stop("s", T0 + 10)); // hook-idle
    clock.set(T0 + 1_000);
    store.apply_registry(&[busy_entry("s", T0 + 1_000)]); // working via override
    store.on_event(&subagent("s", HookEvent::SubagentStart, T0 + 1_010));
    assert_eq!(store.subagents_of("s"), Some(1));

    clock.set(T0 + 5_000);
    store.apply_registry(&[busy_entry("s", T0 + 5_000)]); // still busy
    assert_eq!(store.status_of("s"), Some(Status::Working));
    assert_eq!(
        store.subagents_of("s"),
        Some(1),
        "kept while working via override"
    );
}

// --- R-22.1 discovery time seeding -----------------------------------------

#[test]
fn discovery_seeds_since_from_activity_not_now() {
    // R-22.1: a discovery-created row's time-in-status seeds from its activity
    // estimate (transcript mtime), NOT app-launch "now".
    let (mut store, _c) = store_at(T0);
    let ten_min_ago = T0 - 600_000;
    store.add_inferred(
        "s".to_string(),
        Some("/p".to_string()),
        Some("/p/s.jsonl".to_string()),
        Status::Idle,
        "Discovered".to_string(),
        ten_min_ago,
    );
    // since_ms is measured from the seeded mtime, ~10 min, not ~0.
    assert_eq!(store.since_ms_of("s"), Some(600_000));
}

#[test]
fn registry_updated_at_outranks_transcript_mtime_for_seeding() {
    // R-22.1 precedence: registry updatedAt outranks the transcript mtime the
    // row was first seeded with.
    let (mut store, _c) = store_at(T0);
    let mtime = T0 - 600_000; // 10 min ago
    let updated = T0 - 120_000; // 2 min ago
    store.add_inferred(
        "s".to_string(),
        Some("/p".to_string()),
        Some("/p/s.jsonl".to_string()),
        Status::Idle,
        "Discovered".to_string(),
        mtime,
    );
    assert_eq!(store.since_ms_of("s"), Some(600_000));
    // The cold-start re-seed applies the registry updatedAt (shell does this).
    store.seed_inferred_entered_at("s", updated);
    assert_eq!(
        store.since_ms_of("s"),
        Some(120_000),
        "registry updatedAt wins"
    );
}

#[test]
fn stale_registry_updated_at_does_not_drag_seeding_backwards() {
    // R-22.1 "(fresh file)": a STALE registry updatedAt (older than the
    // transcript mtime the row was seeded with) must NOT outrank the fresher
    // transcript activity â€” dragging entered_at backwards would inflate
    // time-in-status past reality, exactly the dishonest time Â§22 removes.
    let (mut store, _c) = store_at(T0);
    let mtime = T0 - 60_000; // transcript active 1 min ago (fresh)
    let stale_updated = T0 - 3_600_000; // registry file 1 h ago (stale)
    store.add_inferred(
        "s".to_string(),
        Some("/p".to_string()),
        Some("/p/s.jsonl".to_string()),
        Status::Idle,
        "Discovered".to_string(),
        mtime,
    );
    assert_eq!(store.since_ms_of("s"), Some(60_000));
    // The stale registry updatedAt is ignored (does not move entered_at back);
    // the fresher transcript mtime stands.
    store.seed_inferred_entered_at("s", stale_updated);
    assert_eq!(
        store.since_ms_of("s"),
        Some(60_000),
        "stale registry updatedAt must not inflate time-in-status"
    );
}

#[test]
fn coldstart_order_background_busy_row_seeds_from_registry_not_app_launch() {
    // R-22.1 in its exact §21+§22 motivating case, through the REAL cold-start
    // order the shell runs (src-tauri/src/lib.rs): add_inferred (transcript
    // discovery) -> apply_registry (busy-override flip) -> seed_inferred_entered_at
    // (registry updatedAt precedence). Earlier this regressed: apply_registry's
    // override flip restamped entered_at = app-launch `now`, and the later
    // seed_inferred_entered_at no-oped (its forward-only guard rejected the older
    // registry updatedAt), so a background-busy row rendered `~0s` / "just now".
    // The honest reading is ~10s (the registry updatedAt).
    let (mut store, _c) = store_at(T0);
    let mtime = T0 - 600_000; // transcript last touched 10 min ago
    let updated = T0 - 10_000; // registry says busy, updatedAt 10s ago (fresh)

    // 1) transcript discovery: inferred, hook-idle, seeded from transcript mtime.
    store.add_inferred(
        "s".to_string(),
        Some("/p".to_string()),
        Some("/p/s.jsonl".to_string()),
        Status::Idle,
        "Discovered".to_string(),
        mtime,
    );
    // 2) registry cold start: busy + fresh -> override flips the row to working.
    store.apply_registry(&[busy_entry("s", updated)]);
    // 3) R-22.1 re-seed: registry updatedAt outranks the transcript mtime.
    store.seed_inferred_entered_at("s", updated);

    assert_eq!(
        store.status_of("s"),
        Some(Status::Working),
        "background-busy row displays working via the override"
    );
    assert_eq!(
        store.since_ms_of("s"),
        Some(10_000),
        "time-in-status seeds from registry updatedAt (~10s), not app-launch now (~0s)"
    );
    // R-22.3 secondary symptom: age must not collapse to 0 either. With no
    // registry startedAt the anchor clamps to entered_at, so age >= time-in-status.
    let row = store
        .view()
        .into_iter()
        .find(|r| r.id == "s")
        .expect("row present");
    assert_eq!(
        row.age_ms,
        Some(10_000),
        "session age must not read ~0 for a pre-existing background-busy row"
    );
    assert!(
        row.inferred,
        "still an estimate (~) until a hook event arrives"
    );
}

#[test]
fn seed_inferred_entered_at_is_a_noop_for_hook_tracked_rows() {
    // A hook-tracked (exact) row must not be re-seeded (R-22.1 targets estimates).
    let (mut store, clock) = store_at(T0);
    store.on_event(&session_start("s", "/p", T0));
    store.on_event(&prompt("s", "go", T0 + 5)); // working
    store.on_event(&stop("s", T0 + 10)); // exact idle transition at T0+10
    clock.set(T0 + 20);
    store.seed_inferred_entered_at("s", T0 - 1_000_000);
    // Unchanged: still measured from the exact Stop at T0+10 (the seed is a no-op
    // on a hook-tracked, non-inferred row).
    assert_eq!(store.since_ms_of("s"), Some(10));
}

// --- R-22.2 estimate -> exact upgrade --------------------------------------

#[test]
fn first_hook_event_upgrades_discovered_row_to_exact() {
    // R-22.2 / R-22.4: a discovered (inferred, `~`) row becomes exact on the
    // first status-marking hook event; the `~` marker (inferred flag) drops and
    // times re-stamp from the event.
    let (mut store, clock) = store_at(T0);
    let mtime = T0 - 600_000;
    store.add_inferred(
        "s".to_string(),
        Some("/p".to_string()),
        Some("/p/s.jsonl".to_string()),
        Status::Idle,
        "Discovered".to_string(),
        mtime,
    );
    assert!(store.view().iter().find(|r| r.id == "s").unwrap().inferred);

    clock.set(T0 + 5);
    store.on_event(&prompt("s", "a real prompt", T0 + 5));
    let row = store.view().into_iter().find(|r| r.id == "s").unwrap();
    assert!(!row.inferred, "no longer an estimate after a hook event");
    assert_eq!(row.status, Status::Working);
    assert_eq!(
        store.since_ms_of("s"),
        Some(0),
        "time re-stamped from the event"
    );
}

#[test]
fn non_status_hook_event_leaves_estimate_in_place() {
    // A subagent event is not a status marker â†’ the row stays an estimate (`~`)
    // until a real transition lands (R-22.2).
    let (mut store, _c) = store_at(T0);
    store.add_inferred(
        "s".to_string(),
        Some("/p".to_string()),
        None,
        Status::Idle,
        "Discovered".to_string(),
        T0 - 600_000,
    );
    store.on_event(&subagent("s", HookEvent::SubagentStart, T0 + 5));
    assert!(
        store.view().iter().find(|r| r.id == "s").unwrap().inferred,
        "still an estimate after a non-status event"
    );
}

// --- R-22.3 session age -----------------------------------------------------

#[test]
fn session_age_prefers_registry_started_at() {
    // R-22.3 precedence: registry startedAt is the top age anchor.
    let (mut store, clock) = store_at(T0);
    store.on_event(&session_start("s", "/p", T0)); // SessionStart receivedAt = T0
    let mut e = RegistryEntry {
        session_id: "s".to_string(),
        started_at_ms: Some(T0 - 3_600_000), // 1 h ago
        ..Default::default()
    };
    e.updated_at_ms = Some(T0);
    store.apply_registry(std::slice::from_ref(&e));

    clock.set(T0 + 60_000); // 1 min later
    let age = store
        .view()
        .into_iter()
        .find(|r| r.id == "s")
        .unwrap()
        .age_ms
        .unwrap();
    // 1 h (registry startedAt) + 1 min = 3_660_000 ms, NOT from SessionStart.
    assert_eq!(age, 3_660_000);
}

#[test]
fn session_age_falls_back_to_session_start_receipt() {
    // R-22.3: without a registry startedAt, the SessionStart receivedAt anchors.
    let (mut store, clock) = store_at(T0);
    store.on_event(&session_start("s", "/p", T0));
    clock.set(T0 + 120_000);
    let age = store
        .view()
        .into_iter()
        .find(|r| r.id == "s")
        .unwrap()
        .age_ms
        .unwrap();
    assert_eq!(age, 120_000);
}

#[test]
fn discovered_row_age_is_never_smaller_than_time_in_status() {
    // R-22.3: an inferred row seeded (R-22.1) from a past transcript mtime, with
    // no registry startedAt and no SessionStart receivedAt, anchors its age on
    // first-seen (app-launch created_ms). The anchor is clamped so age is never
    // MORE RECENT than entered_at â€” an age younger than the current status
    // ("session just now" beside "~12m") is logically impossible.
    let (mut store, clock) = store_at(T0);
    let mtime = T0 - 720_000; // transcript last active 12 min ago
    store.add_inferred(
        "s".to_string(),
        Some("/p".to_string()),
        Some("/p/s.jsonl".to_string()),
        Status::Idle,
        "Discovered".to_string(),
        mtime,
    );
    clock.set(T0 + 1_000); // 1 s after app launch (created_ms == T0)
    let row = store.view().into_iter().find(|r| r.id == "s").unwrap();
    let since = store.since_ms_of("s").unwrap();
    let age = row.age_ms.unwrap();
    assert!(
        age >= since,
        "age ({age}) must be >= time-in-status ({since})"
    );
    // Concretely: entered_at seeded 12 min ago â†’ both read ~12 min + 1 s, NOT a
    // ~1 s age beside a ~12 min time-in-status.
    assert_eq!(since, 721_000);
    assert_eq!(age, 721_000);
}

// --- R-22 does not disturb hook-born rows ----------------------------------

#[test]
fn hook_born_row_is_never_inferred() {
    // R-22.2: hook-born rows are exact from the start (never `~`).
    let (mut store, _c) = store_at(T0);
    store.on_event(&session_start("s", "/p", T0));
    assert!(!store.view().iter().find(|r| r.id == "s").unwrap().inferred);
}

// Compile-time guard that `SessionStore` is the type under test (keeps the
// import used even if a test above is removed during maintenance).
#[allow(dead_code)]
fn _type_guard(_: &SessionStore) {}
