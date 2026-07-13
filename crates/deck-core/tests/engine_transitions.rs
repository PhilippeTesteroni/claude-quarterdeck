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
    // R-2.3 / §47: `idle_prompt` leaves the status idle and fires NO toast. The
    // "still waiting" reminder is retired — it always duplicated the just-shown
    // Stop "finished" toast, so `idle_prompt` is now inert.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&stop("a", T0 + 10)); // now idle
    let fx = s.on_event(&notification("a", "idle_prompt", None, T0 + 20));
    assert_eq!(s.status_of("a"), Some(Status::Idle));
    assert!(fx.is_empty(), "§47: idle_prompt produces no toast decision");
}

#[test]
fn idle_prompt_while_working_emits_nothing() {
    // A stray idle_prompt that arrives while the session is working (not idle)
    // is inert — no status change, no reminder.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&prompt("a", "go", T0 + 10)); // working
    let fx = s.on_event(&notification("a", "idle_prompt", None, T0 + 20));
    assert_eq!(s.status_of("a"), Some(Status::Working));
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
fn late_stop_after_session_end_does_not_resurrect_the_row() {
    // R-2.5: SessionEnd always wins. A debounce-reordered trailing Stop must not
    // revive a cleanly-ended session as an idle ghost.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&prompt("a", "go", T0 + 5));
    s.on_event(&session_end("a", "logout", T0 + 10));
    assert!(!s.contains("a"));
    let fx = s.on_event(&stop("a", T0 + 20));
    assert!(!s.contains("a"), "late Stop must not re-create the row");
    assert!(fx.is_empty(), "and must fire no toast");
}

#[test]
fn late_attention_notification_after_session_end_is_ignored() {
    // The worst case in the finding: a late permission-prompt Notification
    // resurrecting an ended session as ATTENTION *and* firing a false "needs
    // you" alert. Both must be suppressed.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&session_end("a", "other", T0 + 10));
    let fx = s.on_event(&notification(
        "a",
        "permission_prompt",
        Some("Allow rm -rf?"),
        T0 + 20,
    ));
    assert!(!s.contains("a"), "ended session must stay removed");
    assert_eq!(s.status_of("a"), None);
    assert!(
        fx.is_empty(),
        "no phantom attention toast for an ended session"
    );
}

#[test]
fn genuinely_later_session_start_resumes_an_ended_id() {
    // A resume / id reuse strictly after the end busts the tombstone and
    // re-creates the row (so the guard doesn't permanently blacklist an id).
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&session_end("a", "clear", T0 + 10));
    assert!(!s.contains("a"));
    s.on_event(&session_start("a", "/p", T0 + 1_000));
    assert_eq!(
        s.status_of("a"),
        Some(Status::Idle),
        "later SessionStart resumes the row"
    );
    // ...and subsequent events for the resumed session apply normally.
    s.on_event(&prompt("a", "go again", T0 + 1_010));
    assert_eq!(s.status_of("a"), Some(Status::Working));
}

#[test]
fn stale_session_end_after_a_newer_same_id_start_does_not_wipe_the_live_row() {
    // Mirror image of the tombstone guard: the live ingest burst (watcher.rs
    // drains a HashSet in nondeterministic order) can apply a genuinely-newer
    // reused-id SessionStart(ts=tr+40) BEFORE a SessionEnd(ts=tr) that coalesced
    // into the same debounce window. The stale End must be ignored — otherwise it
    // deletes the just-recreated live row and tombstones the id at the older ts,
    // dropping every subsequent event for the live session.
    let (mut s, _c) = store_at(T0);
    // Original session lifecycle for id "a".
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&prompt("a", "first task", T0 + 5));
    // Reused-id restart lands first in the reordered burst (newer ts).
    s.on_event(&session_start("a", "/p", T0 + 40));
    assert_eq!(s.status_of("a"), Some(Status::Idle));
    // The older End arrives second and must NOT wipe the live row.
    s.on_event(&session_end("a", "clear", T0 + 10));
    assert!(
        s.contains("a"),
        "a stale reordered SessionEnd must not delete the live re-created row"
    );
    assert_eq!(s.status_of("a"), Some(Status::Idle));
    // And the id is not tombstoned, so subsequent live events still apply.
    s.on_event(&prompt("a", "second task", T0 + 50));
    assert_eq!(
        s.status_of("a"),
        Some(Status::Working),
        "later events for the live session must still be honored"
    );
}

#[test]
fn later_stamped_stop_before_the_genuine_session_end_still_removes_the_row() {
    // R-2.5 regression: on session exit Stop and SessionEnd fire as two concurrent
    // hook processes; PowerShell cold-start jitter can stamp the Stop a LATER
    // received_at than the End yet write it first, so the watcher applies them in
    // the order (Stop@end+5, then End@end). The reordered-End guard must key on
    // this row's SessionStart, not its last-applied event — otherwise it mistakes
    // the genuine End for a stale reorder and leaves an idle ghost that R-2.5 says
    // should be gone ("SessionEnd always wins immediately").
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&prompt("a", "go", T0 + 1));
    let end = T0 + 100;
    s.on_event(&stop("a", end + 5)); // later-stamped Stop, applied first
    assert_eq!(s.status_of("a"), Some(Status::Idle));
    s.on_event(&session_end("a", "logout", end)); // genuine End, earlier stamp
    assert!(
        !s.contains("a"),
        "a real SessionEnd must remove the row even when a later-stamped event preceded it"
    );
    assert_eq!(s.status_of("a"), None);
}

#[test]
fn session_end_removes_a_start_less_row_regardless_of_ts() {
    // A row first materialized by a Stop (no SessionStart ⇒ `started_at_ms` is 0,
    // there is no incarnation-start to protect) must still be removable by a later
    // SessionEnd whose ts predates the Stop's — the reused-id guard only defends a
    // row that actually saw a SessionStart.
    let (mut s, _c) = store_at(T0);
    s.on_event(&stop("a", T0 + 5));
    assert!(s.contains("a"));
    s.on_event(&session_end("a", "logout", T0));
    assert!(
        !s.contains("a"),
        "End removes a start-less row even with an earlier ts"
    );
}

#[test]
fn session_end_with_equal_or_newer_ts_still_removes_the_row() {
    // The guard only ignores an End *strictly older* than the last applied event;
    // an End at the same ts (or newer) still wins immediately (R-2.5).
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&prompt("a", "go", T0 + 5));
    s.on_event(&session_end("a", "logout", T0 + 5));
    assert!(!s.contains("a"), "End at equal ts still removes the row");
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
fn throttle_is_per_status_kind_not_global() {
    // R-9.4: the throttle is per (session, kind). A prior idle toast must NOT
    // swallow a genuinely-different, more-urgent attention toast within 10 s.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&prompt("a", "go", T0 + 1));
    let idle_fx = s.on_event(&stop("a", T0 + 100)); // idle toast
    assert_eq!(toast_kinds(&idle_fx), [ToastKind::Idle]);
    // Within the same 10 s window, a real attention transition still fires.
    let att_fx = s.on_event(&notification("a", "permission_prompt", Some("q"), T0 + 200));
    assert_eq!(
        toast_kinds(&att_fx),
        [ToastKind::Attention],
        "a different kind is not throttled by the prior idle toast"
    );
}

#[test]
fn suppressed_toast_refund_releases_the_throttle_slot() {
    // R-9.4 + R-9.5: a toast the shell suppresses (its per-type toggle is off)
    // must NOT consume the throttle window. The engine stamps on emit; the shell
    // calls `refund_toast` on suppression, so the next same-kind toast still
    // fires once the toggle is re-enabled — even within the 10 s window.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));

    // First turn finishes → Idle toast emitted (engine stamps the slot).
    s.on_event(&prompt("a", "go", T0 + 1));
    let first = s.on_event(&stop("a", T0 + 100));
    assert_eq!(toast_kinds(&first), [ToastKind::Idle]);
    let Effect::Toast(decision) = &first[0];
    assert_eq!(decision.at_ms, T0 + 100);

    // Shell found notifyIdle OFF → no toast shown → refund the slot.
    s.refund_toast("a", ToastKind::Idle, decision.at_ms);

    // A second turn finishing ~1 s later (well within 10 s) still fires, because
    // the suppressed one never really spent the window.
    s.on_event(&prompt("a", "again", T0 + 1_100));
    let second = s.on_event(&stop("a", T0 + 1_200));
    assert_eq!(
        toast_kinds(&second),
        [ToastKind::Idle],
        "a refunded (never-shown) toast must not throttle the next same-kind toast"
    );
}

#[test]
fn popup_suppressed_toast_refund_releases_the_throttle_slot() {
    // R-9.4: a toast the notifier suppresses because the popup is visible AND
    // focused shares the exact same refund path as the R-9.5 toggle-off case —
    // no toast actually shows, so the shell calls `refund_toast` and the slot is
    // released. Guards the composition the shell relies on (fire_effects): a
    // popup-suppressed same-kind toast must not silently spend the 10 s window
    // and drop the next legitimate one after the popup loses focus/closes.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&prompt("a", "go", T0 + 1));

    // First turn finishes → Idle toast emitted (engine stamps Idle @ T0+100).
    let first = s.on_event(&stop("a", T0 + 100));
    assert_eq!(toast_kinds(&first), [ToastKind::Idle]);
    let Effect::Toast(decision) = &first[0];
    assert_eq!(decision.kind, ToastKind::Idle);
    assert_eq!(decision.at_ms, T0 + 100);

    // The popup was visible AND focused, so the notifier suppressed the toast
    // (returned false) → the shell refunds the slot (R-9.4).
    s.refund_toast("a", ToastKind::Idle, decision.at_ms);

    // A new turn ending ~2.9 s later (well within 10 s) still fires its Idle
    // toast, because the suppressed one never really spent the window.
    s.on_event(&prompt("a", "again", T0 + 2_000));
    let second = s.on_event(&stop("a", T0 + 3_000));
    assert_eq!(
        toast_kinds(&second),
        [ToastKind::Idle],
        "a popup-suppressed (never-shown) toast must not throttle the next same-kind toast"
    );
}

#[test]
fn a_shown_toast_is_not_refunded_and_still_throttles_the_burst() {
    // The converse guard: when the shell does NOT refund (toast shown), the
    // R-9.4 throttle still collapses a same-kind burst within 10 s.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));

    s.on_event(&prompt("a", "go", T0 + 1));
    let first = s.on_event(&stop("a", T0 + 100));
    assert_eq!(toast_kinds(&first), [ToastKind::Idle]);
    // No refund: the toast was shown.
    s.on_event(&prompt("a", "again", T0 + 1_100));
    let second = s.on_event(&stop("a", T0 + 1_200));
    assert!(second.is_empty(), "a shown toast still throttles the burst");
}

#[test]
fn refund_only_clears_a_matching_stamp() {
    // `refund_toast` must be a no-op when the stamp was replaced by a newer
    // toast (defensive: it only releases the slot it was told about).
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start("a", "/p", T0));
    s.on_event(&prompt("a", "go", T0 + 1));
    s.on_event(&stop("a", T0 + 100)); // stamps Idle @ T0+100

    // A stale/wrong at_ms must not release the current Idle slot.
    s.refund_toast("a", ToastKind::Idle, T0 + 999);
    s.on_event(&prompt("a", "again", T0 + 200));
    let within = s.on_event(&stop("a", T0 + 5_000));
    assert!(
        within.is_empty(),
        "a non-matching refund must not release the throttle slot"
    );
}

#[test]
fn worst_status_ranks_idle_above_dead() {
    // R-2.6: worst-of ordering red > yellow > green > gray, i.e. idle outranks
    // dead in the tray aggregate.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start_full(
        "gone",
        "/p",
        Some("/t/gone.jsonl"),
        Some(4321),
        None,
        T0,
    ));
    let procs = FakeProcessTable::new(); // pid 4321 absent → dead
    s.poll_liveness(&procs, |_p| None);
    assert_eq!(s.status_of("gone"), Some(Status::Dead));
    // A lone dead session → worst is gray/dead.
    assert_eq!(s.worst_status(), Some(Status::Dead));
    // Add an idle session: idle must now win the worst-of.
    s.on_event(&session_start("live", "/p", T0));
    assert_eq!(
        s.worst_status(),
        Some(Status::Idle),
        "idle (green) outranks dead (gray)"
    );
}

#[test]
fn view_sorts_dead_last_below_idle() {
    // R-7.3 / R-2.6: dead rows sort to the very bottom, below idle. The three
    // live rows carry pids so the liveness poll that kills `gone` leaves them be.
    let (mut s, _c) = store_at(T0);
    s.on_event(&session_start_full("idle", "/p", None, Some(1), None, T0));
    s.on_event(&session_start_full(
        "work",
        "/p",
        None,
        Some(2),
        None,
        T0 + 1,
    ));
    s.on_event(&prompt("work", "x", T0 + 1));
    s.on_event(&session_start_full(
        "att",
        "/p",
        None,
        Some(3),
        None,
        T0 + 2,
    ));
    s.on_event(&notification("att", "permission_prompt", None, T0 + 2));
    s.on_event(&session_start_full(
        "gone",
        "/p",
        None,
        Some(999),
        None,
        T0 + 3,
    ));

    // Table keeps 1/2/3 alive; 999 is absent → dead.
    let procs = FakeProcessTable::new()
        .with(1, "node.exe")
        .with(2, "node.exe")
        .with(3, "node.exe");
    s.poll_liveness(&procs, |_p| None);
    assert_eq!(s.status_of("gone"), Some(Status::Dead));
    assert_eq!(s.status_of("att"), Some(Status::Attention));

    let view = s.view();
    let order: Vec<&str> = view.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(order, ["att", "work", "idle", "gone"]);
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
