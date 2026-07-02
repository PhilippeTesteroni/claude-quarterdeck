//! R-5.4 cold-start discovery from `~/.claude/projects/*/*.jsonl`.

mod common;

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use common::*;
use deck_core::discovery::{discover_sessions, merge_into_store, DISCOVERY_MAX_AGE_MS};
use deck_core::engine::Status;
use tempfile::tempdir;

/// Create `<claude_dir>/projects/<slug>/<session>.jsonl` with `body` and return
/// (claude_dir, that file's actual mtime in epoch ms).
fn make_transcript(slug: &str, session: &str, body: &str) -> (tempfile::TempDir, u64) {
    let dir = tempdir().unwrap();
    let proj = dir.path().join("projects").join(slug);
    fs::create_dir_all(&proj).unwrap();
    let file = proj.join(format!("{session}.jsonl"));
    fs::write(&file, body).unwrap();
    let mtime = mtime_ms(&file);
    (dir, mtime)
}

fn mtime_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

const USER_LINE: &str =
    "{\"type\":\"user\",\"cwd\":\"C:/Проекты/агент\",\"message\":{\"role\":\"user\",\"content\":\"Почини сборку\"}}\n";

#[test]
fn discovers_fresh_transcript_as_working_within_30s() {
    let (dir, mtime) = make_transcript("C--Проекты-агент", "sess-1", USER_LINE);
    let now = mtime + 10_000; // 10s after last activity (< 30s window) → working
    let found = discover_sessions(dir.path(), &HashSet::new(), now);
    assert_eq!(found.len(), 1);
    let d = &found[0];
    assert_eq!(d.id, "sess-1");
    assert_eq!(d.status, Status::Working);
    assert_eq!(d.cwd.as_deref(), Some("C:/Проекты/агент"));
    assert_eq!(d.title, "Почини сборку");
}

#[test]
fn discovers_older_activity_as_idle() {
    let (dir, mtime) = make_transcript("slug", "sess-idle", USER_LINE);
    let now = mtime + 2 * 60 * 1000; // 2 min after → idle (still < 6h)
    let found = discover_sessions(dir.path(), &HashSet::new(), now);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].status, Status::Idle);
}

#[test]
fn skips_transcripts_older_than_6h() {
    let (dir, mtime) = make_transcript("slug", "sess-stale", USER_LINE);
    let now = mtime + DISCOVERY_MAX_AGE_MS + 1;
    let found = discover_sessions(dir.path(), &HashSet::new(), now);
    assert!(found.is_empty());
}

#[test]
fn skips_already_known_sessions() {
    let (dir, mtime) = make_transcript("slug", "known", USER_LINE);
    let now = mtime + 1000;
    let mut known = HashSet::new();
    known.insert("known".to_string());
    let found = discover_sessions(dir.path(), &known, now);
    assert!(found.is_empty());
}

#[test]
fn ignores_non_jsonl_files_and_missing_dir() {
    let dir = tempdir().unwrap();
    let proj = dir.path().join("projects").join("slug");
    fs::create_dir_all(&proj).unwrap();
    fs::write(proj.join("notes.txt"), "hi").unwrap();
    let found = discover_sessions(dir.path(), &HashSet::new(), 1_000_000);
    assert!(found.is_empty());

    // A claude dir with no projects/ subdir → empty, no panic.
    let empty = tempdir().unwrap();
    assert!(discover_sessions(empty.path(), &HashSet::new(), 1_000_000).is_empty());
}

#[test]
fn merge_into_store_inserts_inferred_rows_and_skips_known() {
    let (dir, mtime) = make_transcript("C--Проекты-агент", "disc-1", USER_LINE);
    let now = mtime + 5_000;
    let (mut store, _c) = store_at(now);

    let inserted = merge_into_store(&mut store, dir.path(), now);
    assert_eq!(inserted, 1);
    assert_eq!(store.status_of("disc-1"), Some(Status::Working));
    assert_eq!(store.title_of("disc-1").as_deref(), Some("Почини сборку"));

    let view = store.view();
    assert_eq!(view.len(), 1);
    assert!(view[0].inferred);
    assert_eq!(view[0].project, "агент"); // basename of the discovered cwd

    // Re-running discovery does not duplicate the now-known session.
    let again = merge_into_store(&mut store, dir.path(), now + 1000);
    assert_eq!(again, 0);
    assert_eq!(store.len(), 1);
}

#[test]
fn discovery_orders_most_recent_first() {
    let dir = tempdir().unwrap();
    let proj = dir.path().join("projects").join("slug");
    fs::create_dir_all(&proj).unwrap();
    let a = proj.join("older.jsonl");
    let b = proj.join("newer.jsonl");
    fs::write(&a, USER_LINE).unwrap();
    fs::write(&b, USER_LINE).unwrap();

    // Both mtimes are ~now; pick a `now` far enough that both are fresh. Order is
    // by mtime desc then id — assert both discovered and no panic on ties.
    let now = mtime_ms(&b).max(mtime_ms(&a)) + 1000;
    let found = discover_sessions(dir.path(), &HashSet::new(), now);
    assert_eq!(found.len(), 2);
    let ids: HashSet<&str> = found.iter().map(|d| d.id.as_str()).collect();
    assert!(ids.contains("older") && ids.contains("newer"));
}
