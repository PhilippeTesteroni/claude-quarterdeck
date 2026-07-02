//! Spool-directory lifecycle: drain, quarantine, 24 h freshness, 5000 cap
//! (R-3.5). Uses real temp dirs (never the developer's data dir).

use std::fs;
use std::path::Path;

use deck_core::events::{
    drain_spool, enforce_quarantine_cap, enforce_spool_cap, ingest_file, read_and_parse,
    sweep_stray_spool_files, MAX_QUARANTINE_FILES, MAX_SPOOL_FILES, MAX_SPOOL_FILE_BYTES,
};
use tempfile::tempdir;

const NOW: u64 = 1_751_000_000_000;

fn write(dir: &Path, name: &str, body: &str) {
    fs::write(dir.join(name), body).unwrap();
}

fn real_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
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
fn enforce_spool_cap_trims_overflow_on_the_live_path() {
    // R-3.5 cap enforcement on the running-app path (not just startup replay).
    let tmp = tempdir().unwrap();
    let spool = tmp.path().join("spool");
    fs::create_dir_all(&spool).unwrap();

    let over = MAX_SPOOL_FILES + 3;
    for i in 0..over {
        fs::write(spool.join(format!("e{i:05}.json")), good("s", NOW - 1000)).unwrap();
    }
    let removed = enforce_spool_cap(&spool).unwrap();
    assert_eq!(removed, 3, "exactly the overflow is trimmed");
    assert_eq!(fs::read_dir(&spool).unwrap().count(), MAX_SPOOL_FILES);
    // A second call is a no-op once at/under the cap.
    assert_eq!(enforce_spool_cap(&spool).unwrap(), 0);
}

#[test]
fn enforce_spool_cap_missing_dir_is_noop() {
    let tmp = tempdir().unwrap();
    assert_eq!(enforce_spool_cap(&tmp.path().join("nope")).unwrap(), 0);
}

#[test]
fn enforce_quarantine_cap_trims_including_collision_renamed_files() {
    // The quarantine dir also gets a size ceiling; collision-renamed files
    // (`*.json.1`) must count toward the cap, not slip past a `.json` filter.
    let tmp = tempdir().unwrap();
    let quar = tmp.path().join("spool-quarantine");
    fs::create_dir_all(&quar).unwrap();

    let over = MAX_QUARANTINE_FILES + 5;
    for i in 0..over {
        // Half plain `.json`, half collision-suffixed to exercise both.
        let name = if i % 2 == 0 {
            format!("bad{i:05}.json")
        } else {
            format!("bad{i:05}.json.1")
        };
        fs::write(quar.join(name), b"garbage").unwrap();
    }
    let removed = enforce_quarantine_cap(&quar).unwrap();
    assert_eq!(removed, 5);
    assert_eq!(fs::read_dir(&quar).unwrap().count(), MAX_QUARANTINE_FILES);
}

#[test]
fn sweep_stray_spool_files_quarantines_old_non_json_leftovers() {
    // A hook killed between its atomic tmp-write and the rename leaves a
    // `<id>.json.tmp` (or a no-extension leftover). It's never consumed by the
    // drain/ingest paths and never counted by the spool cap, so R-3.5 hygiene
    // sweeps it (once it's clearly not an in-flight write) into quarantine.
    let tmp = tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let quar = tmp.path().join("spool-quarantine");
    fs::create_dir_all(&spool).unwrap();

    write(&spool, "crashed-write.json.tmp", &good("s1", NOW));
    write(&spool, "no-extension-file", "leftover");
    // A real, live event must be left untouched.
    write(&spool, "live.json", &good("s2", NOW));

    // Age is measured against the files' real on-disk mtime, so drive `now_ms`
    // from the wall clock (the fixed `NOW` constant predates the real mtimes and
    // would underflow to age 0). With a 60s grace and files aged well past it,
    // both strays are swept.
    let swept = sweep_stray_spool_files(&spool, &quar, real_now_ms() + 120_000, 60_000).unwrap();
    assert_eq!(swept, 2, "both stray files swept");
    assert!(!spool.join("crashed-write.json.tmp").exists());
    assert!(!spool.join("no-extension-file").exists());
    assert!(spool.join("live.json").exists(), "valid .json untouched");
    assert_eq!(
        fs::read_dir(&quar).unwrap().count(),
        2,
        "strays landed in quarantine, not the void"
    );
}

#[test]
fn sweep_stray_spool_files_spares_in_flight_writes() {
    // A `.tmp` younger than the grace window could still be an in-flight atomic
    // write; it must not be stolen mid-write.
    let tmp = tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let quar = tmp.path().join("spool-quarantine");
    fs::create_dir_all(&spool).unwrap();
    write(&spool, "inflight.json.tmp", &good("s1", NOW));

    // now ≈ file mtime (age ~0) < 60s grace → left alone.
    let swept = sweep_stray_spool_files(&spool, &quar, real_now_ms(), 60_000).unwrap();
    assert_eq!(swept, 0);
    assert!(spool.join("inflight.json.tmp").exists());
}

#[test]
fn sweep_stray_spool_files_missing_dir_is_noop() {
    let tmp = tempdir().unwrap();
    assert_eq!(
        sweep_stray_spool_files(&tmp.path().join("nope"), &tmp.path().join("q"), NOW, 60_000)
            .unwrap(),
        0
    );
}

#[test]
fn ingest_file_treats_a_vanished_file_as_a_noop() {
    // The startup re-drain (closing the replay→watch gap) can consume a file the
    // watcher also reports; a path that no longer exists must be Ok(None), never
    // quarantined as "malformed".
    let tmp = tempdir().unwrap();
    let spool = tmp.path().join("spool");
    let quar = tmp.path().join("spool-quarantine");
    fs::create_dir_all(&spool).unwrap();
    let gone = spool.join("already-gone.json");
    let ev = ingest_file(&gone, &quar, NOW).unwrap();
    assert!(ev.is_none());
    assert!(!quar.exists(), "a vanished file is not quarantined");
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
