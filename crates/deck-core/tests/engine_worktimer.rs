//! §36 working-time timer: a working row's timer is anchored at the real work
//! start (`UserPromptSubmit`), survives §30 reverse-gear flips without
//! resetting, freezes as "took <dur>" on `Stop`, and re-anchors on the next
//! prompt.

mod common;

use common::*;
use deck_core::engine::Status;

const T0: u64 = 1_751_000_000_000;

#[test]
fn work_timer_anchored_at_prompt_survives_reverse_gear_flip() {
    // The anchor is the prompt (T0+1000), NOT the effective-status entry — so a
    // §30 reverse-gear idle→working→idle flip (a resumed turn writing to the
    // transcript with no new hook) never resets it.
    let (mut s, c) = store_at(T0);
    s.on_event(&session_start_full(
        "a",
        "/p",
        Some("/t/a.jsonl"),
        Some(10),
        None,
        T0,
    ));
    s.on_event(&prompt("a", "go", T0 + 1_000)); // working, work-start = T0+1000
    assert_eq!(s.work_started_ms_of("a"), Some(T0 + 1_000));

    // Turn ends, then the reverse gear promotes it back on transcript advance.
    s.on_event(&stop("a", T0 + 5_000)); // idle at T0+5000
    assert_eq!(s.status_of("a"), Some(Status::Idle));
    c.set(T0 + 20_000);
    s.poll_recovery(|_p| Some(T0 + 18_000)); // > idle-entry+2s → working
    assert_eq!(s.status_of("a"), Some(Status::Working));
    // The reverse-gear promote restamps time-in-status, but NOT the work anchor.
    assert_eq!(s.work_started_ms_of("a"), Some(T0 + 1_000));
    let row = s.view().into_iter().find(|r| r.id == "a").unwrap();
    assert_eq!(row.work_started_ms, Some(T0 + 1_000));

    // Reverse gear demotes on a stall — anchor still untouched.
    c.set(T0 + 30_000);
    s.poll_recovery(|_p| Some(T0 + 18_000)); // unchanged mtime → idle
    assert_eq!(s.status_of("a"), Some(Status::Idle));
    assert_eq!(s.work_started_ms_of("a"), Some(T0 + 1_000));
}

#[test]
fn work_timer_freezes_on_stop() {
    let (mut s, c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&prompt("a", "go", T0 + 1_000));
    assert_eq!(s.last_work_ms_of("a"), None); // nothing frozen mid-turn

    s.on_event(&stop("a", T0 + 8_500)); // took 7.5s
    assert_eq!(s.status_of("a"), Some(Status::Idle));
    assert_eq!(s.last_work_ms_of("a"), Some(7_500));

    // Frozen: idle time passing does not grow it.
    c.set(T0 + 60_000);
    let row = s.view().into_iter().find(|r| r.id == "a").unwrap();
    assert_eq!(row.last_work_ms, Some(7_500));
    assert_eq!(row.work_started_ms, Some(T0 + 1_000));
}

#[test]
fn work_timer_resets_on_new_prompt() {
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&prompt("a", "first", T0 + 1_000));
    s.on_event(&stop("a", T0 + 5_000)); // last_work = 4000
    assert_eq!(s.last_work_ms_of("a"), Some(4_000));

    // A fresh prompt re-anchors the work-start and drops the previous "took".
    s.on_event(&prompt("a", "second", T0 + 9_000));
    assert_eq!(s.work_started_ms_of("a"), Some(T0 + 9_000));
    assert_eq!(s.last_work_ms_of("a"), None);
}

#[test]
fn stop_without_prior_prompt_leaves_no_took() {
    // A discovered/never-prompted row has no real work-start, so a Stop cannot
    // manufacture a bogus "took" — the row falls back to time-in-status.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0)); // idle, no prompt
    s.on_event(&stop("a", T0 + 3_000));
    assert_eq!(s.work_started_ms_of("a"), None);
    assert_eq!(s.last_work_ms_of("a"), None);
}

#[test]
fn busy_override_flip_does_not_reset_the_work_anchor() {
    // §21 busy-override can keep a hook-idle row displayed as working across a
    // Stop; the §36 anchor stays at the prompt so the live counter measures the
    // whole turn, not just since the override kicked in.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&prompt("a", "go", T0 + 1_000));
    assert_eq!(s.work_started_ms_of("a"), Some(T0 + 1_000));
    s.on_event(&stop("a", T0 + 4_000)); // hook idle; work anchor preserved
    assert_eq!(s.work_started_ms_of("a"), Some(T0 + 1_000));
    assert_eq!(s.last_work_ms_of("a"), Some(3_000));
}
