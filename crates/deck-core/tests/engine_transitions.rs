//! SPEC §2 transition table + toast decisions + throttle (R-2.1, R-2.3, R-2.6,
//! R-7.3, R-9.1/2/4). Every rule has an injectable-clock test.

mod common;

use common::*;
use deck_core::engine::{Effect, Status};
use deck_core::traits::ToastKind;

const T0: u64 = 1_751_000_000_000;

fn toast_kinds(effects: &[Effect]) -> Vec<ToastKind> {
    effects.iter().map(|Effect::Toast(t)| t.kind).collect()
}

#[test]
fn session_start_enters_idle() {
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    assert_eq!(s.status_of("a"), Some(Status::Idle));
}

#[test]
fn user_prompt_enters_working() {
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&prompt("a", "do a thing", T0 + 10));
    assert_eq!(s.status_of("a"), Some(Status::Working));
}

#[test]
fn permission_prompt_enters_attention_with_alert_toast() {
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/home/дом/проект", T0));
    let fx = s.on_event(&notification(
        "a",
        "permission_prompt",
        Some("Allow Bash?"),
        T0 + 10,
    ));
    assert_eq!(s.status_of("a"), Some(Status::Attention));
    assert_eq!(toast_kinds(&fx), [ToastKind::Attention]);
    let Effect::Toast(t) = &fx[0];
    assert_eq!(t.kind, ToastKind::Attention);
    assert_eq!(t.project, "проект");
    assert_eq!(t.detail, "Allow Bash?");
}

#[test]
fn elicitation_dialog_enters_attention() {
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&notification("a", "elicitation_dialog", None, T0 + 10));
    assert_eq!(s.status_of("a"), Some(Status::Attention));
}

#[test]
fn idle_prompt_does_not_change_status_and_emits_no_toast() {
    // R-2.3
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&stop("a", T0 + 10)); // now idle
    let fx = s.on_event(&notification("a", "idle_prompt", None, T0 + 20));
    assert_eq!(s.status_of("a"), Some(Status::Idle));
    assert!(fx.is_empty());
}

#[test]
fn unknown_notification_type_is_ignored_for_status() {
    // R-2.1
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&prompt("a", "x", T0 + 10)); // working
    let fx = s.on_event(&notification("a", "auth_success", Some("ok"), T0 + 20));
    assert_eq!(s.status_of("a"), Some(Status::Working));
    assert!(fx.is_empty());
    let fx2 = s.on_event(&notification("a", "totally_new_type", None, T0 + 30));
    assert_eq!(s.status_of("a"), Some(Status::Working));
    assert!(fx2.is_empty());
}

#[test]
fn stop_enters_idle_with_finished_toast() {
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start_full(
        "a",
        "/p/myproj",
        None,
        None,
        Some("Refactor auth"),
        T0,
    ));
    s.on_event(&prompt("a", "go", T0 + 10));
    let fx = s.on_event(&stop("a", T0 + 20));
    assert_eq!(s.status_of("a"), Some(Status::Idle));
    let Effect::Toast(t) = &fx[0];
    assert_eq!(t.kind, ToastKind::Idle);
    assert_eq!(t.project, "myproj");
    assert_eq!(t.detail, "Refactor auth");
}

#[test]
fn session_end_removes_row_any_reason() {
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    assert!(s.contains("a"));
    s.on_event(&session_end("a", "logout", T0 + 10));
    assert!(!s.contains("a"));
    assert_eq!(s.status_of("a"), None);
}

#[test]
fn event_for_unknown_session_creates_the_row() {
    // Robustness: a Notification arriving before we saw SessionStart still tracks.
    let (mut s, _c) = store_at(T0);
    s.on_event(&notification("ghost", "permission_prompt", Some("?"), T0));
    assert_eq!(s.status_of("ghost"), Some(Status::Attention));
}

#[test]
fn since_ms_uses_injected_clock() {
    // R-2.5
    let (mut s, c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&prompt("a", "go", T0)); // enters working at T0
    c.advance(4_070);
    assert_eq!(s.since_ms_of("a"), Some(4_070));
    // A later status change resets the timer.
    c.set(T0 + 10_000);
    s.on_event(&stop("a", T0 + 10_000));
    c.advance(1_500);
    assert_eq!(s.since_ms_of("a"), Some(1_500));
}

#[test]
fn worst_status_aggregation_and_empty_is_none() {
    // R-2.6
    let (mut s, _c) = store_at(T0);
    assert_eq!(s.worst_status(), None);

    s.on_event(&session_start("idle1", "/p", T0));
    assert_eq!(s.worst_status(), Some(Status::Idle));

    s.on_event(&session_start("work1", "/p", T0));
    s.on_event(&prompt("work1", "x", T0));
    assert_eq!(s.worst_status(), Some(Status::Working));

    s.on_event(&notification("att1", "permission_prompt", None, T0));
    assert_eq!(s.worst_status(), Some(Status::Attention));
}

#[test]
fn counts_reflect_effective_status() {
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("i", "/p", T0));
    s.on_event(&session_start("w", "/p", T0));
    s.on_event(&prompt("w", "x", T0));
    s.on_event(&notification("a", "permission_prompt", None, T0));
    let c = s.counts();
    assert_eq!(c.idle, 1);
    assert_eq!(c.working, 1);
    assert_eq!(c.attention, 1);
    assert_eq!(c.dead, 0);
}

#[test]
fn view_sorts_attention_working_idle_dead() {
    // R-7.3
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("idle", "/p", T0));
    s.on_event(&session_start("work", "/p", T0 + 1));
    s.on_event(&prompt("work", "x", T0 + 1));
    s.on_event(&notification("att", "permission_prompt", None, T0 + 2));

    let view = s.view();
    let order: Vec<&str> = view.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(order, ["att", "work", "idle"]);
}

#[test]
fn throttle_collapses_bursts_then_allows_after_window() {
    // R-9.4: at most one toast per session per 10 s.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&prompt("a", "x", T0));

    let first = s.on_event(&stop("a", T0 + 100)); // idle toast
    assert_eq!(first.len(), 1);

    // Bounce back to working and stop again within 10 s → suppressed.
    s.on_event(&prompt("a", "y", T0 + 200));
    let second = s.on_event(&stop("a", T0 + 5_000));
    assert!(second.is_empty(), "burst within 10s must collapse");

    // After the window, a new status toast is allowed again.
    s.on_event(&prompt("a", "z", T0 + 11_000));
    let third = s.on_event(&stop("a", T0 + 12_000));
    assert_eq!(third.len(), 1);
}

#[test]
fn throttle_is_per_session() {
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&prompt("a", "x", T0));
    s.on_event(&session_start("b", "/p", T0));
    s.on_event(&prompt("b", "x", T0));

    let a = s.on_event(&stop("a", T0 + 100));
    let b = s.on_event(&stop("b", T0 + 100));
    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 1);
}

#[test]
fn repeated_permission_prompts_do_not_retoast_within_window() {
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    let first = s.on_event(&notification("a", "permission_prompt", Some("q1"), T0 + 10));
    assert_eq!(first.len(), 1);
    // Still attention; a second prompt is not a status change → no toast.
    let second = s.on_event(&notification("a", "permission_prompt", Some("q2"), T0 + 20));
    assert!(second.is_empty());
}
