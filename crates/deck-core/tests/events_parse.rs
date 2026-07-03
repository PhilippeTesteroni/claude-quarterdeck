//! Spool envelope parsing: R-4.3 wrapper shape and R-4.5 tolerance.

use deck_core::events::{parse_envelope, HookEvent, ParseError, MAX_SPOOL_FILE_BYTES};

fn env(json: &str) -> Result<deck_core::events::SpoolEvent, ParseError> {
    parse_envelope(json.as_bytes())
}

#[test]
fn parses_session_start_ancestor_r15_4a() {
    // R-15.4a: `extra.ancestor = {pid, hwnd, exe}` captured on SessionStart, the
    // exact shape the real Windows hook writes (verified live).
    let ev = env(r#"{
        "event": "SessionStart",
        "payload": { "session_id": "s1", "hook_event_name": "SessionStart" },
        "extra": { "claudePid": 14216,
                   "ancestor": { "pid": 12960, "hwnd": 197486, "exe": "WindowsTerminal.exe" } }
    }"#)
    .unwrap();
    let anc = ev.ancestor.expect("ancestor parsed");
    assert_eq!(anc.pid, Some(12960));
    assert_eq!(anc.hwnd, Some(197486));
    assert_eq!(anc.exe.as_deref(), Some("WindowsTerminal.exe"));
}

#[test]
fn tolerates_partial_and_zeroed_ancestor() {
    // R-15.4a defensiveness: a `0` handle/pid means "unresolved" and is dropped;
    // an all-empty ancestor collapses to None; a missing ancestor is fine.
    let ev = env(r#"{
        "event": "SessionStart",
        "payload": { "session_id": "s1" },
        "extra": { "ancestor": { "pid": 0, "hwnd": 0 } }
    }"#)
    .unwrap();
    assert!(
        ev.ancestor.is_none(),
        "an all-zero ancestor collapses to None"
    );

    let ev2 = env(r#"{
        "event": "SessionStart",
        "payload": { "session_id": "s1" },
        "extra": { "ancestor": { "exe": "iTerm.app" } }
    }"#)
    .unwrap();
    let anc = ev2.ancestor.expect("exe-only ancestor kept");
    assert_eq!(anc.pid, None);
    assert_eq!(anc.exe.as_deref(), Some("iTerm.app"));
}

#[test]
fn parses_session_start_with_extra_pid() {
    let ev = env(r#"{
        "v": 1,
        "event": "SessionStart",
        "receivedAt": 1751000000000,
        "payload": {
            "session_id": "abc-123",
            "cwd": "/home/user/proj",
            "transcript_path": "/t/abc.jsonl",
            "hook_event_name": "SessionStart",
            "source": "startup",
            "session_title": "Fix the bug"
        },
        "extra": { "claudePid": 4242 }
    }"#)
    .unwrap();

    assert_eq!(ev.v, 1);
    assert_eq!(ev.session_id, "abc-123");
    assert_eq!(ev.received_at_ms, Some(1_751_000_000_000));
    assert_eq!(ev.cwd.as_deref(), Some("/home/user/proj"));
    assert_eq!(ev.transcript_path.as_deref(), Some("/t/abc.jsonl"));
    assert_eq!(ev.claude_pid, Some(4242));
    match ev.kind {
        HookEvent::SessionStart {
            source,
            session_title,
        } => {
            assert_eq!(source.as_deref(), Some("startup"));
            assert_eq!(session_title.as_deref(), Some("Fix the bug"));
        }
        other => panic!("wrong kind: {other:?}"),
    }
}

#[test]
fn parses_each_event_type() {
    let up = env(
        r#"{"event":"UserPromptSubmit","receivedAt":1,"payload":{"session_id":"s","prompt":"hi"}}"#,
    )
    .unwrap();
    assert!(matches!(up.kind, HookEvent::UserPromptSubmit { .. }));

    let n = env(r#"{"event":"Notification","receivedAt":1,"payload":{"session_id":"s","message":"m","notification_type":"permission_prompt"}}"#).unwrap();
    assert!(matches!(n.kind, HookEvent::Notification { .. }));

    let stop = env(r#"{"event":"Stop","receivedAt":1,"payload":{"session_id":"s"}}"#).unwrap();
    assert!(matches!(stop.kind, HookEvent::Stop));

    let end = env(
        r#"{"event":"SessionEnd","receivedAt":1,"payload":{"session_id":"s","reason":"clear"}}"#,
    )
    .unwrap();
    match end.kind {
        HookEvent::SessionEnd { reason } => assert_eq!(reason.as_deref(), Some("clear")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn unknown_event_name_is_tolerated_not_error() {
    let ev =
        env(r#"{"event":"SubagentStop","receivedAt":1,"payload":{"session_id":"s"}}"#).unwrap();
    match ev.kind {
        HookEvent::Unknown { name } => assert_eq!(name, "SubagentStop"),
        other => panic!("expected Unknown, got {other:?}"),
    }
}

#[test]
fn unknown_payload_fields_are_ignored() {
    // R-4.5: forward-compatible — unknown keys everywhere must not break parsing.
    let ev = env(r#"{
        "v": 2,
        "event": "Stop",
        "receivedAt": 5,
        "brandNew": true,
        "payload": { "session_id": "s", "prompt_id": "p", "permission_mode": "x", "effort": "high", "future": [1,2,3] },
        "extra": { "claudePid": 1, "somethingElse": "ok" }
    }"#)
    .unwrap();
    assert_eq!(ev.session_id, "s");
    assert_eq!(ev.v, 2);
    assert!(matches!(ev.kind, HookEvent::Stop));
}

#[test]
fn falls_back_to_hook_event_name_when_wrapper_event_missing() {
    let ev =
        env(r#"{"receivedAt":1,"payload":{"session_id":"s","hook_event_name":"Stop"}}"#).unwrap();
    assert!(matches!(ev.kind, HookEvent::Stop));
}

#[test]
fn missing_session_id_is_error() {
    assert!(matches!(
        env(r#"{"event":"Stop","payload":{}}"#),
        Err(ParseError::MissingSessionId)
    ));
    // Blank session_id also rejected.
    assert!(matches!(
        env(r#"{"event":"Stop","payload":{"session_id":"   "}}"#),
        Err(ParseError::MissingSessionId)
    ));
}

#[test]
fn missing_event_name_is_error() {
    assert!(matches!(
        env(r#"{"payload":{"session_id":"s"}}"#),
        Err(ParseError::MissingEvent)
    ));
}

#[test]
fn empty_and_whitespace_are_error() {
    assert!(matches!(env(""), Err(ParseError::Empty)));
    assert!(matches!(env("   \n\t "), Err(ParseError::Empty)));
}

#[test]
fn garbage_and_truncated_json_are_json_error() {
    assert!(matches!(env("not json at all"), Err(ParseError::Json(_))));
    // Truncated mid-object.
    assert!(matches!(
        env(r#"{"event":"Stop","payload":{"session_id":"s""#),
        Err(ParseError::Json(_))
    ));
}

#[test]
fn oversized_input_is_too_large() {
    let big = vec![b'x'; (MAX_SPOOL_FILE_BYTES + 1) as usize];
    assert!(matches!(parse_envelope(&big), Err(ParseError::TooLarge(_))));
}

#[test]
fn strips_utf8_bom() {
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(br#"{"event":"Stop","receivedAt":1,"payload":{"session_id":"s"}}"#);
    let ev = parse_envelope(&bytes).unwrap();
    assert_eq!(ev.session_id, "s");
}

// --- receivedAt tolerance (R-4.5) -----------------------------------------

#[test]
fn received_at_accepts_epoch_millis_and_seconds() {
    let ms =
        env(r#"{"event":"Stop","receivedAt":1751000000000,"payload":{"session_id":"s"}}"#).unwrap();
    assert_eq!(ms.received_at_ms, Some(1_751_000_000_000));

    // Epoch seconds get scaled up to millis.
    let secs =
        env(r#"{"event":"Stop","receivedAt":1751000000,"payload":{"session_id":"s"}}"#).unwrap();
    assert_eq!(secs.received_at_ms, Some(1_751_000_000_000));
}

#[test]
fn received_at_accepts_float_and_numeric_string() {
    let f =
        env(r#"{"event":"Stop","receivedAt":1751000000.5,"payload":{"session_id":"s"}}"#).unwrap();
    assert_eq!(f.received_at_ms, Some(1_751_000_000_500));

    let s = env(r#"{"event":"Stop","receivedAt":"1751000000000","payload":{"session_id":"s"}}"#)
        .unwrap();
    assert_eq!(s.received_at_ms, Some(1_751_000_000_000));
}

#[test]
fn received_at_accepts_iso8601() {
    // 2026-07-02T00:00:00Z == 1782950400 s == 1782950400000 ms.
    let z =
        env(r#"{"event":"Stop","receivedAt":"2026-07-02T00:00:00Z","payload":{"session_id":"s"}}"#)
            .unwrap();
    assert_eq!(z.received_at_ms, Some(1_782_950_400_000));

    // With milliseconds and an explicit UTC offset of zero.
    let frac = env(r#"{"event":"Stop","receivedAt":"2026-07-02T00:00:00.250+00:00","payload":{"session_id":"s"}}"#).unwrap();
    assert_eq!(frac.received_at_ms, Some(1_782_950_400_250));

    // A positive offset shifts back to UTC (03:00+03:00 == 00:00Z).
    let off = env(
        r#"{"event":"Stop","receivedAt":"2026-07-02T03:00:00+03:00","payload":{"session_id":"s"}}"#,
    )
    .unwrap();
    assert_eq!(off.received_at_ms, Some(1_782_950_400_000));
}

#[test]
fn missing_received_at_is_none_not_error() {
    let ev = env(r#"{"event":"Stop","payload":{"session_id":"s"}}"#).unwrap();
    assert_eq!(ev.received_at_ms, None);
}

#[test]
fn unparseable_received_at_degrades_to_none() {
    let ev =
        env(r#"{"event":"Stop","receivedAt":"tuesday","payload":{"session_id":"s"}}"#).unwrap();
    assert_eq!(ev.received_at_ms, None);
    assert_eq!(ev.session_id, "s"); // rest still parses (R-4.5 tolerance)
}

#[test]
fn cwd_and_transcript_are_trimmed_and_blanked() {
    let ev = env(r#"{"event":"Stop","receivedAt":1,"payload":{"session_id":"s","cwd":"  ","transcript_path":"  /t.jsonl  "}}"#).unwrap();
    assert_eq!(ev.cwd, None);
    assert_eq!(ev.transcript_path.as_deref(), Some("/t.jsonl"));
}

#[test]
fn cyrillic_cwd_round_trips() {
    let ev = env(r#"{"event":"SessionStart","receivedAt":1,"payload":{"session_id":"с-1","cwd":"C:/Проекты/мой-агент","source":"startup"}}"#).unwrap();
    assert_eq!(ev.session_id, "с-1");
    assert_eq!(ev.cwd.as_deref(), Some("C:/Проекты/мой-агент"));
}
