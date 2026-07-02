//! Non-destructive merge/uninstall of Quarterdeck hook entries in
//! `~/.claude/settings.json` (SPEC §4, R-4.1, R-4.2).
//!
//! Design goals, all traced to the spec:
//!
//! * **Value-preserving** — we parse the settings file into a
//!   [`serde_json::Value`] and mutate only the pieces we own. Every foreign
//!   key, foreign hook, matcher, comment-free field, etc. is round-tripped
//!   untouched (R-4.1 "preserve all foreign hooks").
//! * **Refuse on unparseable** — a malformed settings file yields
//!   [`HooksConfigError::Unparseable`] and we never write, so we can surface a
//!   visible error instead of clobbering the user's config (R-4.1).
//! * **Timestamped backups, capped at 3** — before the first write we copy the
//!   existing file to `settings.json.quarterdeck-backup-<ts>` and prune the
//!   family down to the three newest (R-4.1).
//! * **Atomic write** — we render to a sibling temp file and `rename` it over
//!   the target so a crash mid-write can never leave a half-written config.
//! * **Idempotent** — our entry is added to an event only when no entry there
//!   already carries the [`MARKER`]; re-running install is a no-op.
//! * **Robust input** — a UTF-8 BOM is stripped before parsing, CRLF line
//!   endings parse fine (JSON whitespace), and empty/whitespace-only files are
//!   treated as an empty object.
//!
//! The hook *command line* embedded into each entry is produced by
//! [`command_line`]; the scripts it points at live in `hooks/` (T2) and are
//! copied to `<data>/hooks/` at install time by the shell (R-4.4).

use serde_json::{json, Map, Value};
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The five Claude Code hook events Quarterdeck subscribes to (SPEC §4,
/// `docs/hooks-facts.md`). Order is the natural session lifecycle; it is also
/// the order entries are appended in a freshly created config.
pub const HOOK_EVENTS: [&str; 5] = [
    "SessionStart",
    "UserPromptSubmit",
    "Notification",
    "Stop",
    "SessionEnd",
];

/// Matcher applied to our `Notification` entry so only the notification types
/// we care about invoke the hook (R-4.1, R-2 status table).
pub const NOTIFICATION_MATCHER: &str = "permission_prompt|idle_prompt|elicitation_dialog";

/// Per-hook timeout in seconds written into every entry (R-4.1).
pub const HOOK_TIMEOUT_SECS: u64 = 10;

/// Substring that identifies an entry as ours. It always appears in the hook
/// command because the script path lives under a `quarterdeck` directory
/// (R-4.2 "entries whose command contains `quarterdeck`").
pub const MARKER: &str = "quarterdeck";

/// How many timestamped backups to retain (R-4.1 "keep 3").
pub const BACKUP_KEEP: usize = 3;

/// Errors from reading/merging/writing the settings file.
#[derive(Debug, thiserror::Error)]
pub enum HooksConfigError {
    /// The settings file exists but is not valid JSON. We refuse to touch it.
    #[error("settings file is not valid JSON: {0}")]
    Unparseable(String),
    /// The settings JSON parsed, but its shape is incompatible (root or a
    /// `hooks`/event slot is not the object/array we require). Refuse rather
    /// than overwrite unexpected user data.
    #[error("settings JSON has an unexpected shape (not an object where one was required)")]
    UnexpectedShape,
    /// Filesystem error.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// Serializing the merged settings back to JSON failed.
    #[error("failed to serialize settings: {0}")]
    Serialize(serde_json::Error),
}

/// Outcome of an install/uninstall operation.
#[derive(Debug, Clone, Default)]
pub struct HooksChange {
    /// Whether the settings file was actually rewritten.
    pub changed: bool,
    /// Path of the backup created before writing, if any (only when the file
    /// pre-existed and a change was made).
    pub backup: Option<PathBuf>,
    /// Events an entry was added to (install only).
    pub events_added: Vec<String>,
    /// Number of entries removed (uninstall only).
    pub entries_removed: usize,
}

/// Build the hook command line for the current platform (R-4.4).
///
/// Windows uses `powershell.exe -NoProfile -ExecutionPolicy Bypass -File
/// "<path>"` with forward slashes so it survives both documented Windows hook
/// shells (Git Bash and PowerShell). Other platforms invoke the `.sh` directly.
/// The path necessarily contains `quarterdeck`, satisfying the [`MARKER`].
pub fn command_line(hook_script: &Path) -> String {
    let p = hook_script.display().to_string().replace('\\', "/");
    if cfg!(windows) {
        format!("powershell.exe -NoProfile -ExecutionPolicy Bypass -File \"{p}\"")
    } else {
        format!("\"{p}\"")
    }
}

/// Install (or repair) Quarterdeck hooks in the settings file at
/// `settings_path`, using `command` as the hook command line.
///
/// A missing file is treated as an empty config and created. If nothing needs
/// changing (already installed on every event) the file is left untouched and
/// [`HooksChange::changed`] is `false`.
pub fn install_hooks(settings_path: &Path, command: &str) -> Result<HooksChange, HooksConfigError> {
    let mut root = read_settings(settings_path)?;
    let events_added = merge_hooks(&mut root, command)?;
    if events_added.is_empty() {
        return Ok(HooksChange::default());
    }
    let backup = create_backup(settings_path, &now_backup_ts(), BACKUP_KEEP)?;
    write_settings_atomic(settings_path, &root)?;
    Ok(HooksChange {
        changed: true,
        backup,
        events_added,
        entries_removed: 0,
    })
}

/// Remove exactly the hook entries whose command contains `marker`
/// (typically [`MARKER`]); all foreign content is preserved (R-4.2).
///
/// A missing/empty file yields a no-op. An unparseable file refuses (returns
/// [`HooksConfigError::Unparseable`]) rather than clobber.
pub fn uninstall_hooks(
    settings_path: &Path,
    marker: &str,
) -> Result<HooksChange, HooksConfigError> {
    let mut root = read_settings(settings_path)?;
    let entries_removed = strip_hooks(&mut root, marker);
    if entries_removed == 0 {
        return Ok(HooksChange::default());
    }
    let backup = create_backup(settings_path, &now_backup_ts(), BACKUP_KEEP)?;
    write_settings_atomic(settings_path, &root)?;
    Ok(HooksChange {
        changed: true,
        backup,
        events_added: Vec::new(),
        entries_removed,
    })
}

/// Merge our hook entries into an already-parsed settings `root`, returning the
/// list of events an entry was newly added to. Idempotent per event: if an
/// entry carrying [`MARKER`] already exists for an event, that event is skipped.
///
/// Returns [`HooksConfigError::UnexpectedShape`] if `root`, its `hooks` value,
/// or an event slot is present but not the object/array we require — we refuse
/// rather than overwrite unexpected data.
pub fn merge_hooks(root: &mut Value, command: &str) -> Result<Vec<String>, HooksConfigError> {
    let obj = root
        .as_object_mut()
        .ok_or(HooksConfigError::UnexpectedShape)?;

    let hooks_val = obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks_obj = hooks_val
        .as_object_mut()
        .ok_or(HooksConfigError::UnexpectedShape)?;

    let mut added = Vec::new();
    for event in HOOK_EVENTS {
        let arr_val = hooks_obj
            .entry(event.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let arr = arr_val
            .as_array_mut()
            .ok_or(HooksConfigError::UnexpectedShape)?;

        if arr.iter().any(|e| entry_has_marker(e, MARKER)) {
            continue;
        }
        arr.push(make_entry(event, command));
        added.push(event.to_string());
    }
    Ok(added)
}

/// Remove every hook entry whose command contains `marker` from a parsed
/// settings `root`, returning how many entries were removed. Event arrays we
/// empty (and the top-level `hooks` object if it empties) are pruned so an
/// uninstall can restore a pristine config; foreign content is never touched.
pub fn strip_hooks(root: &mut Value, marker: &str) -> usize {
    let Some(obj) = root.as_object_mut() else {
        return 0;
    };
    let Some(hooks_val) = obj.get_mut("hooks") else {
        return 0;
    };
    let Some(hooks_obj) = hooks_val.as_object_mut() else {
        return 0;
    };

    let mut removed = 0usize;
    let mut emptied_events: Vec<String> = Vec::new();

    for (event, val) in hooks_obj.iter_mut() {
        let Some(arr) = val.as_array_mut() else {
            continue;
        };
        let before = arr.len();
        arr.retain(|e| !entry_has_marker(e, marker));
        let removed_here = before - arr.len();
        removed += removed_here;
        if removed_here > 0 && arr.is_empty() {
            emptied_events.push(event.clone());
        }
    }

    for event in emptied_events {
        hooks_obj.remove(&event);
    }
    if removed > 0 && hooks_obj.is_empty() {
        obj.remove("hooks");
    }
    removed
}

/// Create a timestamped backup of `settings_path` (if it exists) and prune the
/// backup family to the `keep` newest. Returns the backup path, or `None` when
/// there was no file to back up.
///
/// `ts` must be lexicographically sortable in chronological order for pruning
/// to be correct; [`now_backup_ts`] guarantees this (fixed-width nanoseconds).
pub fn create_backup(settings_path: &Path, ts: &str, keep: usize) -> io::Result<Option<PathBuf>> {
    if !settings_path.exists() {
        return Ok(None);
    }
    let dir = settings_path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = settings_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings.json".to_string());

    let backup_name = format!("{file_name}.{MARKER}-backup-{ts}");
    let backup_path = dir.join(backup_name);
    fs::copy(settings_path, &backup_path)?;
    prune_backups(dir, &file_name, keep)?;
    Ok(Some(backup_path))
}

// --- internals -------------------------------------------------------------

fn make_entry(event: &str, command: &str) -> Value {
    let hook = json!({
        "type": "command",
        "command": command,
        "timeout": HOOK_TIMEOUT_SECS,
    });
    let mut entry = Map::new();
    if event == "Notification" {
        entry.insert("matcher".to_string(), json!(NOTIFICATION_MATCHER));
    }
    entry.insert("hooks".to_string(), Value::Array(vec![hook]));
    Value::Object(entry)
}

fn entry_has_marker(entry: &Value, marker: &str) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|hooks| {
            hooks.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| c.contains(marker))
            })
        })
}

fn read_settings(path: &Path) -> Result<Value, HooksConfigError> {
    match fs::read(path) {
        Ok(bytes) => parse_settings_bytes(&bytes),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Value::Object(Map::new())),
        Err(e) => Err(e.into()),
    }
}

fn parse_settings_bytes(bytes: &[u8]) -> Result<Value, HooksConfigError> {
    let bytes = strip_bom(bytes);
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Object(Map::new()));
    }
    match serde_json::from_slice::<Value>(bytes) {
        Ok(v @ Value::Object(_)) => Ok(v),
        Ok(_) => Err(HooksConfigError::UnexpectedShape),
        Err(e) => Err(HooksConfigError::Unparseable(e.to_string())),
    }
}

fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

fn write_settings_atomic(path: &Path, root: &Value) -> Result<(), HooksConfigError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;

    let mut json = serde_json::to_string_pretty(root).map_err(HooksConfigError::Serialize)?;
    json.push('\n');

    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings.json".to_string());
    let tmp = dir.join(format!(".{stem}.qd-tmp-{}", now_backup_ts()));

    fs::write(&tmp, json.as_bytes())?;
    // `rename` is atomic and replaces the destination on both Windows
    // (MoveFileEx w/ REPLACE_EXISTING) and Unix.
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e.into())
        }
    }
}

fn prune_backups(dir: &Path, file_name: &str, keep: usize) -> io::Result<()> {
    let prefix = format!("{file_name}.{MARKER}-backup-");
    let mut backups: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix))
        })
        .collect();
    backups.sort();
    if backups.len() > keep {
        for old in &backups[..backups.len() - keep] {
            let _ = fs::remove_file(old);
        }
    }
    Ok(())
}

/// Fixed-width (30 digit) nanoseconds-since-epoch, so backup file names sort
/// lexicographically in chronological order.
fn now_backup_ts() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:030}")
}
