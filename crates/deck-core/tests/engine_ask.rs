//! R-2.4 pending-ask override and its resolution.

mod common;

use common::*;
use deck_core::engine::Status;

const T0: u64 = 1_751_000_000_000;

#[test]
fn pending_ask_forces_attention_over_idle() {
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    assert_eq!(s.status_of("a"), Some(Status::Idle));
    s.note_pending_ask("a");
    assert_eq!(s.status_of("a"), Some(Status::Attention));
}

#[test]
fn pending_ask_forces_attention_over_working() {
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&prompt("a", "go", T0 + 1));
    assert_eq!(s.status_of("a"), Some(Status::Working));
    s.note_pending_ask("a");
    assert_eq!(s.status_of("a"), Some(Status::Attention));
}

#[test]
fn answered_ask_transitions_to_working() {
    // SPEC §2: "MCP ask answered → working".
    let (mut s, c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0)); // idle
    s.note_pending_ask("a"); // attention
    c.set(T0 + 5_000);
    s.note_ask_answered("a");
    assert_eq!(s.status_of("a"), Some(Status::Working));
    assert_eq!(s.since_ms_of("a"), Some(0));
}

#[test]
fn cleared_ask_recomputes_from_hook_state() {
    // Timeout/dismiss → drop override; status returns to the last hook state.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0)); // idle
    s.note_pending_ask("a"); // attention
    s.note_ask_cleared("a");
    assert_eq!(s.status_of("a"), Some(Status::Idle));

    // Same, but the underlying hook state was working.
    let (mut s2, _c2) = store_at(T0);
    s2.on_event(&session_start("b", "/p", T0));
    s2.on_event(&prompt("b", "go", T0 + 1)); // working
    s2.note_pending_ask("b"); // attention
    s2.note_ask_cleared("b");
    assert_eq!(s2.status_of("b"), Some(Status::Working));
}

#[test]
fn ask_over_hook_attention_returns_to_attention_when_cleared() {
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&notification("a", "permission_prompt", None, T0 + 1)); // hook attention
    s.note_pending_ask("a"); // still attention (ask override)
    assert_eq!(s.status_of("a"), Some(Status::Attention));
    s.note_ask_cleared("a"); // hook state was attention → stays attention
    assert_eq!(s.status_of("a"), Some(Status::Attention));
}

#[test]
fn hook_events_under_a_pending_ask_do_not_change_shown_status() {
    // A Stop arriving while an ask is pending updates the hidden hook state but
    // the shown status stays attention until the ask resolves.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&prompt("a", "go", T0 + 1));
    s.note_pending_ask("a");
    let fx = s.on_event(&stop("a", T0 + 2)); // hook goes idle underneath
    assert!(fx.is_empty(), "no idle toast while ask forces attention");
    assert_eq!(s.status_of("a"), Some(Status::Attention));
    s.note_ask_cleared("a");
    assert_eq!(s.status_of("a"), Some(Status::Idle));
}

#[test]
fn note_pending_ask_on_unknown_session_is_ignored() {
    let (mut s, _c) = store_at(T0);
    s.note_pending_ask("nope");
    assert_eq!(s.status_of("nope"), None);
}
