//! Spool-directory lifecycle: drain, quarantine, 24 h freshness, 5000 cap
//! (R-3.5). Uses real temp dirs (never the developer's data dir).

use std::fs;
use std::path::Path;

use deck_core::events::{drain_spool, ingest_file, read_and_parse, MAX_SPOOL_FILE_BYTES};
use tempfile::tempdir;

const NOW: u64 = 1_751_000_000_000;

fn write(dir: &Path, name: &str, body: &str) {
    fs::write(dir.join(name), body).unwrap();
}

fn good(session: &str, ts: u64) -> String {
    format!(r#"{{"event":"Stop","receivedAt":{ts},"payload":{{"session_id":"{session}"}}}}"#)
}

#[test]
fn drains_good_events_in_receipt_order_and_leaves_them_on_disk() {
    let tmp = tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let quar = tmp.path().join("spool-quarantine");
    fs::create_dir_all(&spool).unwrap();

    write(&spool, "b.json", &good("s2", NOW - 100));
    write(&spool, "a.json", &good("s1", NOW - 5000));
    write(&spool, "c.json", &good("s3", NOW - 50));

    let out = drain_spool(&spool, &quar, NOW).unwrap();
    assert_eq!(out.events.len(), 3);
    assert_eq!(out.quarantined, 0);
    // Ascending by receipt time: s1 (oldest) then s2 then s3.
    let order: Vec<&str> = out
        .events
        .iter()
        .map(|i| i.event.session_id.as_str())
        .collect();
    assert_eq!(order, ["s1", "s2", "s3"]);
    // Good files remain for the caller to delete after applying.
    assert!(out.events.iter().all(|i| i.path.exists()));
}

#[test]
fn malformed_files_go_to_quarantine_not_crash() {
    let tmp = tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let quar = tmp.path().join("spool-quarantine");
    fs::create_dir_all(&spool).unwrap();

    write(&spool, "ok.json", &good("s1", NOW));
    write(&spool, "garbage.json", "}{ not json");
    write(
        &spool,
        "truncated.json",
        r#"{"event":"Stop","payload":{"session_id"#,
    );
    write(
        &spool,
        "no-session.json",
        r#"{"event":"Stop","payload":{}}"#,
    );
    write(&spool, "empty.json", "   ");

    let out = drain_spool(&spool, &quar, NOW).unwrap();
    assert_eq!(out.events.len(), 1);
    assert_eq!(out.events[0].event.session_id, "s1");
    assert_eq!(out.quarantined, 4);

    // The four bad files moved out of spool and into quarantine.
    assert!(!spool.join("garbage.json").exists());
    assert!(quar.join("garbage.json").exists());
    assert_eq!(fs::read_dir(&quar).unwrap().count(), 4);
}

#[test]
fn huge_file_is_quarantined() {
    let tmp = tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let quar = tmp.path().join("spool-quarantine");
    fs::create_dir_all(&spool).unwrap();

    let huge = "x".repeat((MAX_SPOOL_FILE_BYTES + 10) as usize);
    write(&spool, "huge.json", &huge);
    write(&spool, "ok.json", &good("s1", NOW));

    let out = drain_spool(&spool, &quar, NOW).unwrap();
    assert_eq!(out.events.len(), 1);
    assert_eq!(out.quarantined, 1);
    assert!(quar.join("huge.json").exists());
}

#[test]
fn events_older_than_24h_are_discarded_on_replay() {
    let tmp = tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let quar = tmp.path().join("spool-quarantine");
    fs::create_dir_all(&spool).unwrap();

    write(&spool, "fresh.json", &good("fresh", NOW - 1000));
    let day = 24 * 60 * 60 * 1000;
    write(&spool, "stale.json", &good("stale", NOW - day - 1));

    let out = drain_spool(&spool, &quar, NOW).unwrap();
    assert_eq!(out.events.len(), 1);
    assert_eq!(out.events[0].event.session_id, "fresh");
    assert_eq!(out.discarded_old, 1);
    assert!(!spool.join("stale.json").exists());
    assert!(!quar.join("stale.json").exists()); // discarded, not quarantined
}

#[test]
fn events_without_timestamp_are_never_discarded_as_old() {
    let tmp = tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let quar = tmp.path().join("spool-quarantine");
    fs::create_dir_all(&spool).unwrap();

    write(
        &spool,
        "no-ts.json",
        r#"{"event":"Stop","payload":{"session_id":"s"}}"#,
    );
    let out = drain_spool(&spool, &quar, NOW).unwrap();
    assert_eq!(out.events.len(), 1);
    assert_eq!(out.discarded_old, 0);
}

#[test]
fn missing_spool_dir_is_not_an_error() {
    let tmp = tempdir().unwrap();
    let spool = tmp.path().join("does-not-exist");
    let quar = tmp.path().join("q");
    let out = drain_spool(&spool, &quar, NOW).unwrap();
    assert_eq!(out.events.len(), 0);
}

#[test]
fn enforces_5000_file_cap_oldest_first() {
    let tmp = tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let quar = tmp.path().join("spool-quarantine");
    fs::create_dir_all(&spool).unwrap();

    // 5001 valid files → exactly one over the cap must be deleted.
    for i in 0..5001 {
        write(&spool, &format!("e{i:05}.json"), &good("s", NOW - 1000));
    }
    let out = drain_spool(&spool, &quar, NOW).unwrap();
    assert_eq!(out.capped, 1);
    assert_eq!(out.events.len(), 5000);
    // Total remaining files never exceeds the cap.
    assert!(fs::read_dir(&spool).unwrap().count() <= 5000);
}

#[test]
fn ingest_file_returns_event_for_good_file_and_none_for_bad() {
    let tmp = tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let quar = tmp.path().join("spool-quarantine");
    fs::create_dir_all(&spool).unwrap();

    let ok = spool.join("ok.json");
    fs::write(&ok, good("s1", NOW)).unwrap();
    let ev = ingest_file(&ok, &quar, NOW).unwrap();
    assert_eq!(ev.unwrap().session_id, "s1");
    assert!(ok.exists()); // caller deletes after applying

    let bad = spool.join("bad.json");
    fs::write(&bad, "garbage").unwrap();
    let none = ingest_file(&bad, &quar, NOW).unwrap();
    assert!(none.is_none());
    assert!(!bad.exists());
    assert!(quar.join("bad.json").exists());
}

#[test]
fn read_and_parse_size_guards_before_reading() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("huge.json");
    fs::write(&path, "y".repeat((MAX_SPOOL_FILE_BYTES + 1) as usize)).unwrap();
    assert!(read_and_parse(&path).is_err());
}

#[test]
fn quarantine_disambiguates_name_collisions() {
    let tmp = tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let quar = tmp.path().join("spool-quarantine");
    fs::create_dir_all(&spool).unwrap();
    fs::create_dir_all(&quar).unwrap();

    // A pre-existing quarantined file with the same name as the incoming bad one.
    fs::write(quar.join("dup.json"), "old").unwrap();
    let bad = spool.join("dup.json");
    fs::write(&bad, "garbage").unwrap();

    assert!(ingest_file(&bad, &quar, NOW).unwrap().is_none());
    // Both the old and the new (suffixed) file survive in quarantine.
    let count = fs::read_dir(&quar).unwrap().count();
    assert_eq!(count, 2);
}
