//! The live session registry: Claude Code's undocumented internal
//! `~/.claude/sessions/*.json` files (SPEC §15, R-15.1).
//!
//! Each file describes one live session — `{pid, sessionId, cwd, name, status,
//! kind, entrypoint, ...}` — and is the authoritative source for the current
//! session `name` (R-15.2 title precedence), a live pid for liveness
//! (R-15.3), and registry-driven discovery of rows whose transcript is
//! missing/stale (R-15.3).
//!
//! The format is explicitly internal and unstable, so **everything here is
//! parsed defensively**: any missing field is tolerated, an unreadable or
//! malformed file is skipped (logged, never quarantined — this is a foreign,
//! read-only source we don't own), and format drift must never crash or panic.
//! The base directory is overridable via `QUARTERDECK_SESSIONS_DIR` for tests,
//! otherwise it resolves from the same claude-dir override logic as discovery
//! (`QUARTERDECK_CLAUDE_DIR` → `~/.claude`), joined with `sessions`.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::discovery::claude_dir_from_env;
use crate::engine::{SessionStore, Status};

/// A defensively-parsed entry from one `~/.claude/sessions/*.json` file.
/// Every field is optional except `session_id` (a registry entry with no id is
/// useless — we can't match or key a row on it — so it is skipped at read time).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistryEntry {
    /// The session id this entry describes (matched against known rows, R-15.2).
    pub session_id: String,
    /// Live host-process PID, feeds liveness directly for registry-known
    /// sessions (R-15.3) — no ancestor walk needed.
    pub pid: Option<u32>,
    pub cwd: Option<String>,
    /// The session's current display name (`/rename`-able), highest-precedence
    /// title source (R-15.2).
    pub name: Option<String>,
    /// How the registry `name` was set (`nameSource` field): `"user"` for an
    /// explicit Claude-side `/rename`, `"derived"`/absent for the auto-generated
    /// `phily-XX` handle. Drives the §34 title precedence — a user-set name wins
    /// over the transcript `aiTitle`, a derived one loses to it (R-34).
    pub name_source: Option<String>,
    /// Raw status string (`busy`, `idle`, …). Mapped to an engine [`Status`] by
    /// [`registry_status_to_engine`].
    pub status: Option<String>,
    pub kind: Option<String>,
    pub entrypoint: Option<String>,
    /// Session start time (epoch ms), best-effort. Used by later seeding rules
    /// (R-22); parsed here so the whole registry is read once per poll.
    pub started_at_ms: Option<u64>,
    /// Last-update time (epoch ms), best-effort. Freshness signal for the
    /// busy-override (R-21) and discovery seeding (R-22).
    pub updated_at_ms: Option<u64>,
}

/// Map a registry `status` string to an engine [`Status`] (R-15.3): `busy` →
/// `working`, anything else (including `idle`, `waiting`, or missing) → `idle`.
/// Case-insensitive. (The `idle`/`waiting` distinction from an absent status is
/// not carried here — it's a plain busy/not-busy read; the §44 demote uses
/// [`registry_status_is_quiescent`] when it needs the explicit-quiescent signal.)
#[must_use]
pub fn registry_status_to_engine(status: Option<&str>) -> Status {
    match status
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("busy") => Status::Working,
        _ => Status::Idle,
    }
}

/// True iff the registry `status` string is an EXPLICIT quiescent state —
/// `idle` or `waiting` (SPEC §44, R-44). Distinct from a missing/unknown status
/// (which reads as non-busy for the [`registry_status_to_engine`] override but
/// is NOT an authoritative "the turn ended" signal). Claude Code writes one of
/// these when a turn finishes or is ESC-interrupted; the interrupt fires no Stop
/// hook, so this is the only signal that the wedged `working` hook status should
/// demote. Case-insensitive.
#[must_use]
pub fn registry_status_is_quiescent(status: Option<&str>) -> bool {
    matches!(
        status
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("idle") | Some("waiting")
    )
}

/// Resolve the sessions directory: `QUARTERDECK_SESSIONS_DIR` if set, else
/// `<claude_dir>/sessions` (claude dir via `QUARTERDECK_CLAUDE_DIR` → `~/.claude`).
#[must_use]
pub fn sessions_dir_from_env() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("QUARTERDECK_SESSIONS_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    Some(claude_dir_from_env()?.join("sessions"))
}

/// Parse a single registry file's bytes into a [`RegistryEntry`]. Returns `None`
/// when the JSON is unusable (not an object, or no resolvable session id) — the
/// caller skips it. `filename_stem` is the file name without extension, used as
/// a fallback session id when the file omits an explicit `sessionId` field
/// (Claude Code names each file after the session id).
#[must_use]
pub fn parse_entry(bytes: &[u8], filename_stem: Option<&str>) -> Option<RegistryEntry> {
    // Tolerate a leading UTF-8 BOM (a naive writer can emit one).
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let Value::Object(map) = value else {
        return None;
    };

    let str_field = |keys: &[&str]| -> Option<String> {
        for k in keys {
            if let Some(s) = map.get(*k).and_then(Value::as_str) {
                let s = s.trim();
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
        None
    };

    let session_id = str_field(&["sessionId", "session_id", "id"]).or_else(|| {
        filename_stem
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })?;

    let pid = map.get("pid").and_then(numeric_u32).filter(|&p| p != 0);

    Some(RegistryEntry {
        session_id,
        pid,
        cwd: str_field(&["cwd", "workingDirectory", "working_directory"]),
        name: str_field(&["name", "title", "sessionTitle", "session_title"]),
        name_source: str_field(&["nameSource", "name_source"]),
        status: str_field(&["status", "state"]),
        kind: str_field(&["kind", "type"]),
        entrypoint: str_field(&["entrypoint", "entryPoint"]),
        started_at_ms: map
            .get("startedAt")
            .or_else(|| map.get("started_at"))
            .and_then(numeric_ms),
        updated_at_ms: map
            .get("updatedAt")
            .or_else(|| map.get("updated_at"))
            .or_else(|| map.get("lastActivityAt"))
            .and_then(numeric_ms),
    })
}

/// Coerce a JSON value to a `u32` PID, tolerating a numeric string.
fn numeric_u32(v: &Value) -> Option<u32> {
    if let Some(n) = v.as_u64() {
        return u32::try_from(n).ok();
    }
    v.as_str()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .and_then(|n| u32::try_from(n).ok())
}

/// Coerce a JSON value to epoch **milliseconds**, tolerating epoch seconds and
/// numeric strings (R-4.5-style leniency). A value below `1e11` is read as
/// seconds and scaled up. ISO-8601 strings are not parsed here (returned `None`)
/// — the registry is polled every 10 s, so a best-effort numeric read is enough
/// and a drifting string shape simply falls back to other seeding sources.
fn numeric_ms(v: &Value) -> Option<u64> {
    let raw = if let Some(n) = v.as_f64() {
        n
    } else {
        v.as_str()?.trim().parse::<f64>().ok()?
    };
    if !raw.is_finite() || raw <= 0.0 {
        return None;
    }
    let ms = if raw < 1e11 { raw * 1000.0 } else { raw };
    if ms >= u64::MAX as f64 {
        None
    } else {
        Some(ms as u64)
    }
}

/// Read every `*.json` file in `sessions_dir` into [`RegistryEntry`]s, skipping
/// anything unreadable/malformed (logged, never fatal, R-15.1). A missing
/// directory yields an empty list. Entries are returned in file-name order for
/// determinism.
#[must_use]
pub fn read_registry(sessions_dir: &Path) -> Vec<RegistryEntry> {
    let Ok(entries) = fs::read_dir(sessions_dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    files.sort();

    let mut out = Vec::new();
    for path in files {
        let stem = path.file_stem().and_then(|s| s.to_str());
        match fs::read(&path) {
            Ok(bytes) => match parse_entry(&bytes, stem) {
                Some(entry) => out.push(entry),
                None => {
                    tracing::debug!(?path, "skipping unusable registry file (R-15.1)");
                }
            },
            Err(err) => {
                tracing::debug!(?path, %err, "skipping unreadable registry file (R-15.1)");
            }
        }
    }
    out
}

/// Registry-driven discovery (R-15.3): create inferred rows for registry entries
/// whose session is not already known (its transcript was missing/stale, so
/// R-5.4 transcript discovery didn't already create it). Status maps from the
/// registry `status` field (busy → working, else idle); the registry pid is set
/// so liveness uses the pid path directly. Returns the number of rows inserted.
///
/// Run AFTER transcript discovery (R-5.4) at cold start, so registry only fills
/// in sessions transcripts couldn't.
pub fn merge_registry_into_store(
    store: &mut SessionStore,
    entries: &[RegistryEntry],
    now_ms: u64,
) -> usize {
    let mut inserted = 0;
    for e in entries {
        if store.contains(&e.session_id) {
            continue;
        }
        let status = registry_status_to_engine(e.status.as_deref());
        let title = e.name.clone().unwrap_or_default();
        // Best activity estimate for the seeded row: registry updatedAt when
        // present, else "now" (a live registry entry is, by definition, current).
        let activity = e.updated_at_ms.unwrap_or(now_ms);
        store.add_inferred(
            e.session_id.clone(),
            e.cwd.clone(),
            None,
            status,
            title,
            activity,
        );
        // Feed liveness the registry pid + surface the registry name (R-15.2/3).
        store.apply_registry_entry(e);
        inserted += 1;
    }
    inserted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_entry() {
        let json = r#"{
            "sessionId": "abc-123",
            "pid": 4242,
            "cwd": "C:/Проекты/агент",
            "name": "Fix the build",
            "nameSource": "user",
            "status": "busy",
            "kind": "cli",
            "entrypoint": "claude",
            "startedAt": 1720000000000,
            "updatedAt": 1720000005000
        }"#;
        let e = parse_entry(json.as_bytes(), Some("abc-123")).unwrap();
        assert_eq!(e.session_id, "abc-123");
        assert_eq!(e.pid, Some(4242));
        assert_eq!(e.cwd.as_deref(), Some("C:/Проекты/агент"));
        assert_eq!(e.name.as_deref(), Some("Fix the build"));
        assert_eq!(e.name_source.as_deref(), Some("user"));
        assert_eq!(e.status.as_deref(), Some("busy"));
        assert_eq!(e.started_at_ms, Some(1720000000000));
        assert_eq!(e.updated_at_ms, Some(1720000005000));
        assert_eq!(
            registry_status_to_engine(e.status.as_deref()),
            Status::Working
        );
    }

    #[test]
    fn tolerates_missing_fields_and_falls_back_to_filename_id() {
        // Only a name — every other field absent. sessionId falls back to the
        // file stem. Must not panic or drop the entry (R-15.1 defensive parse).
        let e = parse_entry(br#"{"name":"just a name"}"#, Some("sess-from-filename")).unwrap();
        assert_eq!(e.session_id, "sess-from-filename");
        assert_eq!(e.name.as_deref(), Some("just a name"));
        assert_eq!(e.pid, None);
        assert_eq!(e.status, None);
        // No status → idle.
        assert_eq!(registry_status_to_engine(e.status.as_deref()), Status::Idle);
    }

    #[test]
    fn rejects_malformed_and_non_object_json() {
        assert!(parse_entry(b"{ this is not json", Some("x")).is_none());
        assert!(parse_entry(b"[1,2,3]", Some("x")).is_none());
        assert!(parse_entry(b"\"a string\"", Some("x")).is_none());
        // An object with no id and no filename stem → unusable.
        assert!(parse_entry(br#"{"name":"x"}"#, None).is_none());
        assert!(parse_entry(br#"{"name":"x"}"#, Some("")).is_none());
    }

    #[test]
    fn tolerates_numeric_strings_and_epoch_seconds() {
        let e = parse_entry(
            br#"{"sessionId":"s","pid":"777","startedAt":"1720000000","updatedAt":1720000001}"#,
            None,
        )
        .unwrap();
        assert_eq!(e.pid, Some(777));
        // Epoch seconds scale up to ms.
        assert_eq!(e.started_at_ms, Some(1_720_000_000_000));
        assert_eq!(e.updated_at_ms, Some(1_720_000_001_000));
    }

    #[test]
    fn name_source_is_parsed_and_defaults_to_none() {
        // Explicit derived source is carried through.
        let derived =
            parse_entry(br#"{"sessionId":"s","name":"phily-42","nameSource":"derived"}"#, None)
                .unwrap();
        assert_eq!(derived.name_source.as_deref(), Some("derived"));
        // Absent nameSource → None (the derived default is inferred app-side).
        let absent = parse_entry(br#"{"sessionId":"s","name":"phily-42"}"#, None).unwrap();
        assert_eq!(absent.name_source, None);
    }

    #[test]
    fn quiescent_classifier_recognizes_idle_and_waiting_only() {
        // §44 (R-44): idle/waiting are explicit quiescent signals; busy and a
        // missing/unknown status are not (case-insensitive, whitespace-tolerant).
        assert!(registry_status_is_quiescent(Some("idle")));
        assert!(registry_status_is_quiescent(Some("  WAITING ")));
        assert!(!registry_status_is_quiescent(Some("busy")));
        assert!(!registry_status_is_quiescent(Some("something-else")));
        assert!(!registry_status_is_quiescent(None));
        // busy/not-busy read is unchanged: waiting maps to idle like any non-busy.
        assert_eq!(registry_status_to_engine(Some("waiting")), Status::Idle);
    }

    #[test]
    fn zero_pid_is_dropped() {
        let e = parse_entry(br#"{"sessionId":"s","pid":0}"#, None).unwrap();
        assert_eq!(e.pid, None);
    }

    #[test]
    fn read_registry_skips_bad_files_and_reads_good_ones() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("good.json"),
            br#"{"sessionId":"good-1","name":"A","status":"busy"}"#,
        )
        .unwrap();
        fs::write(dir.path().join("bad.json"), b"{ not json").unwrap();
        // A non-json sibling is ignored entirely.
        fs::write(dir.path().join("notes.txt"), b"ignore").unwrap();

        let entries = read_registry(dir.path());
        assert_eq!(entries.len(), 1, "only the one valid file is read");
        assert_eq!(entries[0].session_id, "good-1");
    }

    #[test]
    fn read_registry_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-sessions");
        assert!(read_registry(&missing).is_empty());
    }

    #[test]
    fn sessions_dir_prefers_the_override() {
        // Avoid mutating the process env in a way that races other tests: this
        // just documents that a set override wins. We can't safely set env here
        // under the parallel harness, so assert the fallback shape instead when
        // the override is unset (the common test path sets QUARTERDECK_SESSIONS_DIR
        // per-process in the integration suite).
        if std::env::var_os("QUARTERDECK_SESSIONS_DIR").is_none() && claude_dir_from_env().is_some()
        {
            let dir = sessions_dir_from_env().unwrap();
            assert!(dir.ends_with("sessions"));
        }
    }
}
