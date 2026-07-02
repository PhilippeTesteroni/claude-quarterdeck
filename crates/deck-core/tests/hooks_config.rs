//! Fixture-driven tests for `deck_core::hooks_config` (SPEC §4, R-4.1, R-4.2,
//! testing strategy §11: "merge/uninstall vs fixtures (missing, empty, foreign
//! hooks, malformed, BOM, CRLF); backups capped at 3").
//!
//! Every test operates on a `tempfile::TempDir` copy of a fixture so the repo
//! fixtures under `fixtures/settings/` are never mutated (and no real
//! `~/.claude` is ever touched).

use deck_core::hooks_config::{
    self, create_backup, install_hooks, merge_hooks, strip_hooks, uninstall_hooks,
    HooksConfigError, HOOK_EVENTS, HOOK_TIMEOUT_SECS, MARKER, NOTIFICATION_MATCHER,
};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// A hook command line that carries the `quarterdeck` marker.
const CMD: &str =
    "powershell.exe -NoProfile -ExecutionPolicy Bypass -File \"C:/Users/t/quarterdeck/hooks/quarterdeck-hook.ps1\"";

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("settings")
}

/// Copy a repo fixture into a fresh temp dir and return `(dir, settings_path)`.
fn stage_fixture(name: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let dst = dir.path().join("settings.json");
    let src = fixtures_dir().join(name);
    fs::copy(&src, &dst).unwrap_or_else(|e| panic!("copy {}: {e}", src.display()));
    (dir, dst)
}

fn read_json(path: &Path) -> Value {
    let bytes = fs::read(path).expect("read settings");
    // tolerate a BOM only when re-reading a fixture we wrote ourselves
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    serde_json::from_slice(bytes).expect("valid json out")
}

/// Entries under `hooks.<event>` in a parsed settings doc.
fn event_entries<'a>(root: &'a Value, event: &str) -> &'a Vec<Value> {
    root["hooks"][event].as_array().unwrap_or_else(|| {
        panic!("hooks.{event} missing or not an array in {root}");
    })
}

/// Does any entry under this event carry our marker?
fn has_our_entry(root: &Value, event: &str) -> bool {
    root["hooks"][event]
        .as_array()
        .map(|arr| {
            arr.iter().any(|e| {
                e["hooks"]
                    .as_array()
                    .map(|hs| {
                        hs.iter()
                            .any(|h| h["command"].as_str().is_some_and(|c| c.contains(MARKER)))
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn count_our_entries(root: &Value, event: &str) -> usize {
    root["hooks"][event]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter(|e| {
                    e["hooks"]
                        .as_array()
                        .map(|hs| {
                            hs.iter()
                                .any(|h| h["command"].as_str().is_some_and(|c| c.contains(MARKER)))
                        })
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0)
}

fn backup_files(dir: &Path) -> Vec<String> {
    let prefix = format!("settings.json.{MARKER}-backup-");
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(&prefix))
        .collect();
    v.sort();
    v
}

// --- missing ---------------------------------------------------------------

#[test]
fn install_into_missing_file_creates_all_five_events() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    assert!(!path.exists());

    let out = install_hooks(&path, CMD).expect("install");
    assert!(out.changed);
    assert_eq!(out.backup, None, "no backup when file did not exist");
    assert_eq!(out.events_added.len(), 5);

    let root = read_json(&path);
    for event in HOOK_EVENTS {
        assert!(has_our_entry(&root, event), "missing our entry on {event}");
        assert_eq!(count_our_entries(&root, event), 1);
    }
}

// --- empty (0 bytes) -------------------------------------------------------

#[test]
fn install_into_empty_file_treats_as_object_and_backs_up() {
    let (dir, path) = stage_fixture("empty.json");
    assert_eq!(fs::metadata(&path).unwrap().len(), 0);

    let out = install_hooks(&path, CMD).expect("install");
    assert!(out.changed);
    assert!(
        out.backup.is_some(),
        "existing (empty) file must be backed up"
    );

    let root = read_json(&path);
    for event in HOOK_EVENTS {
        assert!(has_our_entry(&root, event));
    }
    // exactly one backup was created
    assert_eq!(backup_files(dir.path()).len(), 1);
}

#[test]
fn install_into_empty_object() {
    let (_dir, path) = stage_fixture("empty-object.json");
    let out = install_hooks(&path, CMD).expect("install");
    assert!(out.changed);
    let root = read_json(&path);
    for event in HOOK_EVENTS {
        assert!(has_our_entry(&root, event));
    }
}

// --- foreign hooks on the same events --------------------------------------

#[test]
fn install_preserves_foreign_hooks_and_adds_alongside() {
    let (_dir, path) = stage_fixture("foreign-hooks.json");
    let before = read_json(&path);
    // sanity: fixture has a foreign Notification + Stop + an untouched PreToolUse
    assert_eq!(event_entries(&before, "Notification").len(), 1);
    assert_eq!(event_entries(&before, "Stop").len(), 1);
    assert_eq!(event_entries(&before, "PreToolUse").len(), 1);

    let out = install_hooks(&path, CMD).expect("install");
    assert!(out.changed);
    assert_eq!(out.events_added.len(), 5);

    let root = read_json(&path);

    // foreign Notification hook still present, ours added -> 2 entries
    let notif = event_entries(&root, "Notification");
    assert_eq!(notif.len(), 2, "foreign + ours");
    assert!(notif
        .iter()
        .any(|e| e["hooks"][0]["command"] == json!("/usr/local/bin/my-notify.sh")));
    assert!(has_our_entry(&root, "Notification"));

    // foreign Stop hook preserved, ours added
    let stop = event_entries(&root, "Stop");
    assert_eq!(stop.len(), 2);
    assert!(stop
        .iter()
        .any(|e| e["hooks"][0]["command"] == json!("afplay /System/Library/Sounds/Glass.aiff")));

    // PreToolUse is an event we do NOT touch -> unchanged, still 1
    let pre = event_entries(&root, "PreToolUse");
    assert_eq!(pre.len(), 1);
    assert_eq!(
        pre[0]["hooks"][0]["command"],
        json!("/opt/scripts/audit-bash.sh")
    );

    // foreign top-level key preserved
    assert_eq!(root["model"], json!("sonnet"));
}

// --- malformed -> refuse ---------------------------------------------------

#[test]
fn install_refuses_malformed_without_writing() {
    let (dir, path) = stage_fixture("malformed.json");
    let before = fs::read(&path).unwrap();

    let err = install_hooks(&path, CMD).expect_err("must refuse");
    assert!(
        matches!(err, HooksConfigError::Unparseable(_)),
        "expected Unparseable, got {err:?}"
    );

    // file byte-identical (never overwritten), and no backup written
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(backup_files(dir.path()).is_empty());
}

#[test]
fn uninstall_refuses_malformed_without_writing() {
    let (_dir, path) = stage_fixture("malformed.json");
    let before = fs::read(&path).unwrap();
    let err = uninstall_hooks(&path, MARKER).expect_err("must refuse");
    assert!(matches!(err, HooksConfigError::Unparseable(_)));
    assert_eq!(fs::read(&path).unwrap(), before);
}

// --- BOM -------------------------------------------------------------------

#[test]
fn install_handles_utf8_bom() {
    let (_dir, path) = stage_fixture("bom.json");
    // confirm the fixture really starts with a BOM
    assert_eq!(&fs::read(&path).unwrap()[..3], &[0xEF, 0xBB, 0xBF]);

    let out = install_hooks(&path, CMD).expect("install despite BOM");
    assert!(out.changed);

    let root = read_json(&path);
    assert_eq!(root["model"], json!("haiku"), "foreign key preserved");
    for event in HOOK_EVENTS {
        assert!(has_our_entry(&root, event));
    }
    // output is written without a BOM
    assert_ne!(&fs::read(&path).unwrap()[..3], &[0xEF, 0xBB, 0xBF]);
}

// --- CRLF ------------------------------------------------------------------

#[test]
fn install_handles_crlf_line_endings() {
    let (_dir, path) = stage_fixture("crlf.json");
    assert!(fs::read(&path).unwrap().windows(2).any(|w| w == b"\r\n"));

    let out = install_hooks(&path, CMD).expect("install despite CRLF");
    assert!(out.changed);

    let root = read_json(&path);
    // foreign Stop hook (from the CRLF fixture) preserved, ours added
    let stop = event_entries(&root, "Stop");
    assert_eq!(stop.len(), 2);
    assert!(stop
        .iter()
        .any(|e| e["hooks"][0]["command"] == json!("echo done")));
    assert!(has_our_entry(&root, "Stop"));
}

// --- idempotency -----------------------------------------------------------

#[test]
fn install_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("settings.json");

    let first = install_hooks(&path, CMD).expect("first");
    assert!(first.changed);
    assert_eq!(first.events_added.len(), 5);

    let second = install_hooks(&path, CMD).expect("second");
    assert!(!second.changed, "second install is a no-op");
    assert!(second.events_added.is_empty());
    assert_eq!(second.backup, None);

    let root = read_json(&path);
    for event in HOOK_EVENTS {
        assert_eq!(
            count_our_entries(&root, event),
            1,
            "no duplicate on {event}"
        );
    }
}

// --- entry shape -----------------------------------------------------------

#[test]
fn entries_carry_timeout_and_notification_matcher() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    install_hooks(&path, CMD).expect("install");
    let root = read_json(&path);

    for event in HOOK_EVENTS {
        let entry = &event_entries(&root, event)[0];
        assert_eq!(
            entry["hooks"][0]["timeout"],
            json!(HOOK_TIMEOUT_SECS),
            "timeout on {event}"
        );
        assert_eq!(entry["hooks"][0]["type"], json!("command"));
        assert_eq!(entry["hooks"][0]["command"], json!(CMD));
        if event == "Notification" {
            assert_eq!(entry["matcher"], json!(NOTIFICATION_MATCHER));
        } else {
            assert!(entry.get("matcher").is_none(), "no matcher on {event}");
        }
    }
}

// --- uninstall -------------------------------------------------------------

#[test]
fn uninstall_removes_only_ours_preserving_foreign() {
    let (_dir, path) = stage_fixture("foreign-hooks.json");
    install_hooks(&path, CMD).expect("install");

    let out = uninstall_hooks(&path, MARKER).expect("uninstall");
    assert!(out.changed);
    assert_eq!(out.entries_removed, 5, "one per event");

    let root = read_json(&path);
    // our entries gone everywhere
    for event in HOOK_EVENTS {
        assert!(!has_our_entry(&root, event), "ours lingered on {event}");
    }
    // foreign hooks fully preserved
    let notif = event_entries(&root, "Notification");
    assert_eq!(notif.len(), 1);
    assert_eq!(
        notif[0]["hooks"][0]["command"],
        json!("/usr/local/bin/my-notify.sh")
    );
    let stop = event_entries(&root, "Stop");
    assert_eq!(stop.len(), 1);
    let pre = event_entries(&root, "PreToolUse");
    assert_eq!(pre.len(), 1);
}

#[test]
fn uninstall_prunes_events_that_become_empty() {
    // fresh install into an empty config -> all five arrays are ours-only.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    install_hooks(&path, CMD).expect("install");

    let out = uninstall_hooks(&path, MARKER).expect("uninstall");
    assert!(out.changed);
    assert_eq!(out.entries_removed, 5);

    let root = read_json(&path);
    // hooks object emptied entirely -> removed, restoring a pristine object
    assert!(
        root.get("hooks").is_none(),
        "empty hooks object should be pruned: {root}"
    );
}

#[test]
fn uninstall_is_noop_when_nothing_ours() {
    let (_dir, path) = stage_fixture("foreign-hooks.json");
    let before = read_json(&path);
    let out = uninstall_hooks(&path, MARKER).expect("uninstall");
    assert!(!out.changed);
    assert_eq!(out.entries_removed, 0);
    assert_eq!(out.backup, None);
    assert_eq!(read_json(&path), before, "file untouched");
}

#[test]
fn uninstall_missing_file_is_noop() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    let out = uninstall_hooks(&path, MARKER).expect("uninstall");
    assert!(!out.changed);
    assert!(!path.exists());
}

// --- backups capped at 3 ---------------------------------------------------

#[test]
fn backups_capped_at_three() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    fs::write(&path, b"{}\n").unwrap();

    // create five backups with monotonic, fixed-width timestamps
    for i in 1..=5 {
        let ts = format!("{i:05}");
        let made = create_backup(&path, &ts, 3).expect("backup");
        assert!(made.is_some());
    }

    let backups = backup_files(dir.path());
    assert_eq!(backups.len(), 3, "capped at 3: {backups:?}");
    // the three newest survive
    assert_eq!(
        backups,
        vec![
            format!("settings.json.{MARKER}-backup-00003"),
            format!("settings.json.{MARKER}-backup-00004"),
            format!("settings.json.{MARKER}-backup-00005"),
        ]
    );
}

#[test]
fn create_backup_returns_none_when_file_absent() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    let made = create_backup(&path, "00001", 3).expect("backup");
    assert_eq!(made, None);
}

// --- pure merge/strip on Values --------------------------------------------

#[test]
fn merge_hooks_is_value_preserving() {
    let mut root = json!({
        "customTopLevel": { "keep": [1, 2, 3] },
        "hooks": { "Stop": [ { "hooks": [ { "type": "command", "command": "keep-me" } ] } ] }
    });
    let added = merge_hooks(&mut root, CMD).expect("merge");
    assert_eq!(added.len(), 5);

    // foreign top-level object untouched
    assert_eq!(root["customTopLevel"]["keep"], json!([1, 2, 3]));
    // foreign Stop hook still first, ours appended
    let stop = root["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop[0]["hooks"][0]["command"], json!("keep-me"));
    assert_eq!(stop.len(), 2);
}

#[test]
fn merge_refuses_unexpected_shape() {
    let mut root = json!({ "hooks": [] }); // hooks must be an object
    let err = merge_hooks(&mut root, CMD).expect_err("must refuse");
    assert!(matches!(err, HooksConfigError::UnexpectedShape));

    let mut root2 = json!({ "hooks": { "Stop": "not-an-array" } });
    let err2 = merge_hooks(&mut root2, CMD).expect_err("must refuse");
    assert!(matches!(err2, HooksConfigError::UnexpectedShape));
}

#[test]
fn strip_hooks_counts_and_prunes() {
    let mut root = json!({
        "hooks": {
            "Stop": [
                { "hooks": [ { "type": "command", "command": "x/quarterdeck/y" } ] },
                { "hooks": [ { "type": "command", "command": "foreign" } ] }
            ],
            "SessionEnd": [
                { "hooks": [ { "type": "command", "command": "quarterdeck-hook" } ] }
            ]
        }
    });
    let removed = strip_hooks(&mut root, MARKER);
    assert_eq!(removed, 2);
    // Stop kept its foreign entry
    assert_eq!(root["hooks"]["Stop"].as_array().unwrap().len(), 1);
    // SessionEnd emptied -> pruned
    assert!(root["hooks"].get("SessionEnd").is_none());
}

#[test]
fn command_line_contains_marker() {
    let cmd = hooks_config::command_line(Path::new(
        "C:/Users/x/AppData/Roaming/quarterdeck/hooks/quarterdeck-hook.ps1",
    ));
    assert!(cmd.contains(MARKER));
}
