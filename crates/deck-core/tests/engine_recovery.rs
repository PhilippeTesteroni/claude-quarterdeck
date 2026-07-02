//! R-2.2 `attention → working` recovery via transcript stat.

mod common;

use common::*;
use deck_core::engine::Status;

const T0: u64 = 1_751_000_000_000;

fn attention_session(
    transcript: &str,
    notif_ts: u64,
) -> (deck_core::engine::SessionStore, std::sync::Arc<FakeClock>) {
    let (mut s, c) = store_at(notif_ts);
    s.on_event(&session_start_full(
        "a",
        "/p",
        Some(transcript),
        Some(10),
        None,
        notif_ts - 1000,
    ));
    s.on_event(&notification(
        "a",
        "permission_prompt",
        Some("Allow?"),
        notif_ts,
    ));
    assert_eq!(s.status_of("a"), Some(Status::Attention));
    (s, c)
}

#[test]
fn recovers_to_working_when_transcript_advances_2s_after_notification() {
    let notif = T0;
    let (mut s, c) = attention_session("/t/a.jsonl", notif);
    c.set(notif + 10_000);
    // Transcript mtime is 2s past the notification → recover.
    s.poll_recovery(|_path| Some(notif + 2_000));
    assert_eq!(s.status_of("a"), Some(Status::Working));
}

#[test]
fn stays_attention_when_transcript_not_advanced_enough() {
    let notif = T0;
    let (mut s, c) = attention_session("/t/a.jsonl", notif);
    c.set(notif + 10_000);
    // Only 1.5s past the notification → still blocked.
    s.poll_recovery(|_path| Some(notif + 1_500));
    assert_eq!(s.status_of("a"), Some(Status::Attention));
}

#[test]
fn stays_attention_when_transcript_missing() {
    let notif = T0;
    let (mut s, c) = attention_session("/t/a.jsonl", notif);
    c.set(notif + 10_000);
    s.poll_recovery(|_path| None);
    assert_eq!(s.status_of("a"), Some(Status::Attention));
}

#[test]
fn recovery_only_applies_to_hook_driven_attention_not_ask() {
    // A pending ask forces attention (R-2.4); transcript activity must NOT clear it.
    let (mut s, c) = store_at(T0);
    s.on_event(&session_start_full(
        "a",
        "/p",
        Some("/t/a.jsonl"),
        Some(10),
        None,
        T0,
    ));
    s.on_event(&prompt("a", "go", T0 + 1)); // working
    s.note_pending_ask("a");
    assert_eq!(s.status_of("a"), Some(Status::Attention));
    c.set(T0 + 100_000);
    s.poll_recovery(|_p| Some(T0 + 100_000));
    assert_eq!(s.status_of("a"), Some(Status::Attention));
}

#[test]
fn stop_clears_recovery_bookkeeping() {
    // After a Stop the session is idle; a stale transcript advance must not
    // resurrect a recovery.
    let notif = T0;
    let (mut s, c) = attention_session("/t/a.jsonl", notif);
    s.on_event(&stop("a", notif + 500)); // back to idle, clears attention_from_hook
    assert_eq!(s.status_of("a"), Some(Status::Idle));
    c.set(notif + 10_000);
    s.poll_recovery(|_p| Some(notif + 9_000));
    assert_eq!(s.status_of("a"), Some(Status::Idle));
}

#[test]
fn recovery_is_noop_for_working_and_idle_sessions() {
    let (mut s, c) = store_at(T0);
    s.on_event(&session_start_full(
        "a",
        "/p",
        Some("/t/a.jsonl"),
        Some(10),
        None,
        T0,
    ));
    s.on_event(&prompt("a", "go", T0 + 1)); // working
    c.set(T0 + 10_000);
    s.poll_recovery(|_p| Some(T0 + 9_000));
    assert_eq!(s.status_of("a"), Some(Status::Working));
}
