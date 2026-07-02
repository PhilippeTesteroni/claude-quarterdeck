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
fn resolving_one_of_two_asks_keeps_attention_until_the_last_resolves() {
    // R-2.4: two agents can map onto one session row (same cwd / basename
    // fallback, shell `match_session`), so two asks can be attributed to one
    // session. Resolving/timing-out the FIRST must NOT clear the attention
    // override while the second is still pending.
    let (mut s, c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0)); // idle
    s.note_pending_ask("a");
    s.note_pending_ask("a");
    assert_eq!(s.status_of("a"), Some(Status::Attention));

    // Answer path: first answer keeps attention (a question is still waiting),
    // only the last answer resumes the agent to working.
    c.set(T0 + 5_000);
    s.note_ask_answered("a");
    assert_eq!(
        s.status_of("a"),
        Some(Status::Attention),
        "still one ask pending → stay attention"
    );
    s.note_ask_answered("a");
    assert_eq!(
        s.status_of("a"),
        Some(Status::Working),
        "last ask answered → working (§2)"
    );
}

#[test]
fn timing_out_one_of_two_asks_keeps_attention_until_the_last_clears() {
    // Same as above via the timeout/dismiss path (`note_ask_cleared`): the first
    // clear must not drop the override while the second ask is still pending.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0)); // idle
    s.note_pending_ask("a");
    s.note_pending_ask("a");
    assert_eq!(s.status_of("a"), Some(Status::Attention));

    s.note_ask_cleared("a");
    assert_eq!(
        s.status_of("a"),
        Some(Status::Attention),
        "still one ask pending → stay attention"
    );
    s.note_ask_cleared("a");
    assert_eq!(
        s.status_of("a"),
        Some(Status::Idle),
        "last ask cleared → recompute from hook state (idle)"
    );
}

#[test]
fn note_pending_ask_on_unknown_session_is_ignored() {
    let (mut s, _c) = store_at(T0);
    s.note_pending_ask("nope");
    assert_eq!(s.status_of("nope"), None);
}

#[test]
fn answering_an_ask_on_a_dead_session_does_not_resurrect_it() {
    // A session's `claude` process can vanish while an ask is still answerable
    // shell-side (the ask row lives in the shell's AskStore, not the engine).
    // Answering it must NOT flip the dead process back to `working` — that would
    // paint a phantom live row and a wrong worst-of tray change until the next
    // liveness tick re-marked it dead.
    let (mut s, c) = store_at(T0);
    s.on_event(&session_start_full(
        "a",
        "/p",
        Some("/t.jsonl"),
        Some(4242),
        None,
        T0,
    ));
    s.note_pending_ask("a"); // attention
    assert_eq!(s.status_of("a"), Some(Status::Attention));

    // The process is gone → the liveness poll marks the session dead.
    let procs = FakeProcessTable::new(); // pid 4242 absent = process gone
    c.set(T0 + 10_000);
    s.poll_liveness(&procs, |_| None);
    assert_eq!(s.status_of("a"), Some(Status::Dead));

    // Answering the still-pending ask leaves it dead, not resurrected.
    c.set(T0 + 12_000);
    s.note_ask_answered("a");
    assert_eq!(s.status_of("a"), Some(Status::Dead));
}
