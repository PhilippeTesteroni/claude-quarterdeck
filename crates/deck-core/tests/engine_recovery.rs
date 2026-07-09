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
fn stop_clears_attention_bookkeeping_then_idle_recovery_takes_over() {
    // After a Stop the session is idle (attention_from_hook cleared). A later
    // transcript advance ≥2 s past the idle-entry promotes it to working via the
    // §2 "transcript activity while idle → working" rule — NOT a resurrected
    // attention recovery (the notification anchor is gone).
    let notif = T0;
    let (mut s, c) = attention_session("/t/a.jsonl", notif);
    s.on_event(&stop("a", notif + 500)); // back to idle, clears attention_from_hook
    assert_eq!(s.status_of("a"), Some(Status::Idle));
    c.set(notif + 10_000);
    s.poll_recovery(|_p| Some(notif + 9_000)); // > idle-entry(notif+500)+2s
    assert_eq!(s.status_of("a"), Some(Status::Working));
}

#[test]
fn idle_recovers_to_working_when_transcript_advances_2s_after_going_idle() {
    // §2 status table: "transcript activity while idle → working" (a resumed
    // turn writing to the transcript without a UserPromptSubmit hook).
    let notif = T0;
    let (mut s, c) = attention_session("/t/a.jsonl", notif);
    s.on_event(&stop("a", notif + 500)); // idle at notif+500
    c.set(notif + 10_000);
    s.poll_recovery(|_p| Some(notif + 3_000)); // 2.5s past idle-entry → promote
    assert_eq!(s.status_of("a"), Some(Status::Working));
}

#[test]
fn idle_stays_idle_when_transcript_not_advanced_past_entry() {
    let notif = T0;
    let (mut s, c) = attention_session("/t/a.jsonl", notif);
    s.on_event(&stop("a", notif + 5_000)); // idle at notif+5000
    c.set(notif + 10_000);
    // Only 1s past idle-entry → still idle.
    s.poll_recovery(|_p| Some(notif + 6_000));
    assert_eq!(s.status_of("a"), Some(Status::Idle));
}

#[test]
fn idle_without_transcript_never_promotes() {
    let (mut s, c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0)); // idle, no transcript path
    s.on_event(&prompt("a", "go", T0 + 1));
    s.on_event(&stop("a", T0 + 2)); // idle again
    c.set(T0 + 100_000);
    s.poll_recovery(|_p| Some(T0 + 100_000));
    assert_eq!(s.status_of("a"), Some(Status::Idle));
}

#[test]
fn recovery_promoted_working_demotes_to_idle_when_transcript_goes_quiescent() {
    // R-30.3 reverse gear: a row promoted idle→working by a transcript advance
    // must fall back to idle on the first later tick where the transcript mtime
    // does NOT advance — the recovered turn finished without a Stop hook, so the
    // reverse gear is the only thing that unsticks it.
    let notif = T0;
    let (mut s, c) = attention_session("/t/a.jsonl", notif);
    s.on_event(&stop("a", notif + 500)); // idle at notif+500
    assert_eq!(s.status_of("a"), Some(Status::Idle));
    // Promote: transcript 3s past idle-entry → working.
    c.set(notif + 10_000);
    s.poll_recovery(|_p| Some(notif + 3_000));
    assert_eq!(s.status_of("a"), Some(Status::Working));
    // Quiescent tick: transcript mtime unchanged → demote back to idle.
    c.set(notif + 20_000);
    s.poll_recovery(|_p| Some(notif + 3_000));
    assert_eq!(s.status_of("a"), Some(Status::Idle));
}

#[test]
fn recovery_promoted_working_stays_working_while_transcript_advances() {
    // Reverse gear only demotes on a stall: as long as the transcript mtime keeps
    // advancing the recovered turn is genuinely alive and stays `working`.
    let notif = T0;
    let (mut s, c) = attention_session("/t/a.jsonl", notif);
    s.on_event(&stop("a", notif + 500)); // idle at notif+500
    c.set(notif + 10_000);
    s.poll_recovery(|_p| Some(notif + 3_000)); // promote
    assert_eq!(s.status_of("a"), Some(Status::Working));
    c.set(notif + 20_000);
    s.poll_recovery(|_p| Some(notif + 6_000)); // advanced → hold working
    assert_eq!(s.status_of("a"), Some(Status::Working));
    // Now it stalls at the new mtime → demote.
    c.set(notif + 30_000);
    s.poll_recovery(|_p| Some(notif + 6_000));
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
