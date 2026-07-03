//! Settings persistence: `<data>/settings.json` load/save preserving unknown
//! keys (SPEC R-10.1), and resolution of the data root (`QUARTERDECK_DATA_DIR`
//! override, SPEC R-3.3).
//!
//! Filled in by T3.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::ipc::SettingValue;

/// Resolves the Quarterdeck data root (SPEC R-3.3):
/// `%APPDATA%/quarterdeck` on Windows, `~/Library/Application
/// Support/quarterdeck` on macOS, always overridable via
/// `QUARTERDECK_DATA_DIR` (required for test isolation and used by the live
/// smoke / E2E suites).
pub fn data_dir() -> PathBuf {
    resolve_data_dir(std::env::var("QUARTERDECK_DATA_DIR").ok())
}

/// Pure form of [`data_dir`] so the override logic is testable without
/// mutating process-global environment variables (which would race across
/// parallel `cargo test` threads).
fn resolve_data_dir(override_dir: Option<String>) -> PathBuf {
    match override_dir.filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => platform_data_dir(),
    }
}

#[cfg(target_os = "windows")]
fn platform_data_dir() -> PathBuf {
    std::env::var("APPDATA")
        .map(|appdata| PathBuf::from(appdata).join("quarterdeck"))
        .unwrap_or_else(|_| std::env::temp_dir().join("quarterdeck"))
}

#[cfg(target_os = "macos")]
fn platform_data_dir() -> PathBuf {
    std::env::var("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support/quarterdeck"))
        .unwrap_or_else(|_| std::env::temp_dir().join("quarterdeck"))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn platform_data_dir() -> PathBuf {
    // Non-goal platform (SPEC §13: Linux tray out of scope); still gives
    // isolated tests and `cargo check` a sane fallback.
    std::env::temp_dir().join("quarterdeck")
}

/// `<data>/spool/` — hook events waiting to be consumed (SPEC §3.3, §3.5).
pub fn spool_dir() -> PathBuf {
    data_dir().join("spool")
}

/// `<data>/spool-quarantine/` — malformed spool files (SPEC R-3.5).
pub fn spool_quarantine_dir() -> PathBuf {
    data_dir().join("spool-quarantine")
}

/// `<data>/asks/` — pending MCP `ask_user` requests (SPEC §8).
pub fn asks_dir() -> PathBuf {
    data_dir().join("asks")
}

/// `<data>/answers/` — answers written for the blocked MCP call to consume
/// (SPEC R-8.7).
pub fn answers_dir() -> PathBuf {
    data_dir().join("answers")
}

/// `<data>/hooks/` — copies of the hook scripts installed at a stable path
/// (SPEC R-4.4).
pub fn hooks_dir() -> PathBuf {
    data_dir().join("hooks")
}

/// `<data>/logs/` — rotated `quarterdeck.log` (SPEC R-10.4).
pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

/// `<dir>/settings.json`.
pub fn settings_path(dir: &Path) -> PathBuf {
    dir.join("settings.json")
}

fn default_true() -> bool {
    true
}

/// Persisted user settings (SPEC R-10.1). Known keys are typed fields;
/// anything else lands in `extra` and is re-serialized untouched so a
/// hand-edited or newer/older-build file never loses data across a save.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default = "default_true")]
    pub notify_idle: bool,
    #[serde(default = "default_true")]
    pub notify_attention: bool,
    #[serde(default)]
    pub notify_reminder: bool,
    #[serde(default)]
    pub launch_at_login: bool,
    #[serde(default)]
    pub onboarding_done: bool,
    /// Popup pin-on-top state (SPEC R-14.2): persists across restarts so a
    /// pinned popup stays pinned. Defaults off (v1.0 anchor/hide-on-blur
    /// behavior).
    #[serde(default)]
    pub popup_pinned: bool,
    /// Unknown top-level keys, preserved verbatim (SPEC R-10.1).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            notify_idle: true,
            notify_attention: true,
            notify_reminder: false,
            launch_at_login: false,
            onboarding_done: false,
            popup_pinned: false,
            extra: Map::new(),
        }
    }
}

impl Settings {
    /// Builds settings from an already-parsed JSON value, tolerating individual
    /// wrong-typed known fields (SPEC R-10.1). A single bad known key (e.g.
    /// `"notifyIdle": "yes"` from a hand-edit or a newer/older build) falls
    /// back to *that field's* default without discarding the other known fields
    /// or any unknown keys — a wrong type must never silently wipe consent state
    /// (`onboardingDone`/`launchAtLogin`) or unknown keys back to defaults.
    /// Returns `None` only when the top level isn't a JSON object at all.
    fn from_json_value(value: Value) -> Option<Self> {
        let Value::Object(mut map) = value else {
            return None;
        };
        // Known keys map to typed fields: take the value if it's the right type,
        // else fall back to this field's default and drop the bad value (it's a
        // known key, never an "unknown key to preserve" — dropping it lets a
        // later save rewrite it cleanly).
        fn take_bool(map: &mut Map<String, Value>, key: &str, default: bool) -> bool {
            match map.remove(key) {
                Some(Value::Bool(b)) => b,
                _ => default,
            }
        }
        Some(Self {
            notify_idle: take_bool(&mut map, "notifyIdle", true),
            notify_attention: take_bool(&mut map, "notifyAttention", true),
            notify_reminder: take_bool(&mut map, "notifyReminder", false),
            launch_at_login: take_bool(&mut map, "launchAtLogin", false),
            onboarding_done: take_bool(&mut map, "onboardingDone", false),
            popup_pinned: take_bool(&mut map, "popupPinned", false),
            extra: map,
        })
    }

    /// Applies a single `set_setting` intent (IPC contract): known keys map to
    /// typed fields, anything else is preserved verbatim in `extra`.
    pub fn apply(&mut self, key: &str, value: SettingValue) {
        match key {
            "notifyIdle" => self.notify_idle = value.as_bool_lossy(),
            "notifyAttention" => self.notify_attention = value.as_bool_lossy(),
            "notifyReminder" => self.notify_reminder = value.as_bool_lossy(),
            "launchAtLogin" => self.launch_at_login = value.as_bool_lossy(),
            "onboardingDone" => self.onboarding_done = value.as_bool_lossy(),
            "popupPinned" => self.popup_pinned = value.as_bool_lossy(),
            other => {
                self.extra.insert(other.to_string(), value.into());
            }
        }
    }
}

/// Strip a leading UTF-8 BOM (`EF BB BF`) if present. Windows editors
/// (Notepad's "UTF-8", PowerShell's default `-Encoding utf8` on 5.x) prepend
/// one, and `serde_json` rejects it with "expected value at line 1 column 1".
/// Quarterdeck surfaces the `<data>` dir path for manual inspection, so a
/// hand-edited settings.json landing here is a real, reachable case (R-10.1
/// "unknown keys preserved" — a BOM must not silently wipe them). Mirrors
/// `hooks_config::strip_bom` for the sibling `~/.claude/settings.json`.
fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// Loads settings from `<dir>/settings.json`. Missing or unparseable files
/// fall back to defaults rather than crashing the shell (mirrors the spool's
/// "never crash on malformed input" posture, SPEC R-3.5).
pub fn load(dir: &Path) -> Settings {
    let Ok(text) = fs::read_to_string(settings_path(dir)) else {
        return Settings::default();
    };
    // Parse to a generic JSON value first, then map known keys leniently
    // (R-10.1): only broken *syntax* falls back wholesale to defaults; a
    // well-formed object with a wrong-typed known field keeps every other known
    // field and all unknown keys instead of being wiped.
    match serde_json::from_str::<Value>(strip_bom(&text)) {
        Ok(value) => Settings::from_json_value(value).unwrap_or_else(|| {
            tracing::warn!("settings.json is not a JSON object, using defaults");
            Settings::default()
        }),
        Err(err) => {
            tracing::warn!(error = %err, "settings.json unparseable, using defaults");
            Settings::default()
        }
    }
}

/// Saves settings atomically (tmp file + rename, SPEC R-10.1).
pub fn save(dir: &Path, settings: &Settings) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(settings).map_err(io::Error::other)?;
    atomic_write(&settings_path(dir), &json)
}

/// Serializes the whole read-modify-write in [`set_setting`]. `load`/`save` are a
/// plain read-then-overwrite of the whole file with no per-key merge, so two
/// concurrent `set_setting` calls for *different* keys whose `[load..save]`
/// windows overlap would otherwise race: the call that saves last writes back its
/// own stale copy of the other key, silently discarding that update (and the JS
/// `invoke('set_setting', …)` promise still resolves, so the UI never learns the
/// toggle it "saved" didn't persist). A process-global lock is sufficient because
/// every settings write goes through this one function.
static SET_SETTING_LOCK: Mutex<()> = Mutex::new(());

/// Loads, applies one `set_setting` intent, and atomically saves. Returns the
/// resulting settings so callers (e.g. T7's autostart wiring) can react to
/// the change immediately. The read-modify-write is serialized (see
/// [`SET_SETTING_LOCK`]) so concurrent updates to different keys can't clobber
/// each other.
pub fn set_setting(dir: &Path, key: &str, value: SettingValue) -> io::Result<Settings> {
    let _guard = SET_SETTING_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut settings = load(dir);
    settings.apply(key, value);
    save(dir, &settings)?;
    Ok(settings)
}

/// Writes `contents` to `path` via a temp-file-then-rename so readers never
/// observe a partial file. Shared by every module that persists to `<data>/*`
/// (settings, ask answers, hook installer backups).
pub fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("quarterdeck");
    // Includes the PID + a monotonic counter so concurrent writers in the same
    // process (tests) never collide on the tmp file name.
    let tmp_path = parent.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        next_tmp_seq()
    ));
    fs::write(&tmp_path, contents)?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

fn next_tmp_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "quarterdeck-settings-test-{tag}-{}-{}",
            std::process::id(),
            next_tmp_seq()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn resolve_data_dir_prefers_override() {
        let resolved = resolve_data_dir(Some("C:/qd-data".to_string()));
        assert_eq!(resolved, PathBuf::from("C:/qd-data"));
    }

    #[test]
    fn resolve_data_dir_ignores_empty_override() {
        let resolved = resolve_data_dir(Some(String::new()));
        assert_eq!(resolved, platform_data_dir());
    }

    #[test]
    fn resolve_data_dir_falls_back_without_override() {
        assert_eq!(resolve_data_dir(None), platform_data_dir());
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let dir = unique_dir("missing");
        let settings = load(&dir);
        assert_eq!(settings, Settings::default());
        assert!(settings.notify_idle);
        assert!(settings.notify_attention);
        assert!(!settings.notify_reminder);
    }

    #[test]
    fn load_malformed_file_returns_defaults_never_crashes() {
        let dir = unique_dir("malformed");
        fs::create_dir_all(&dir).unwrap();
        fs::write(settings_path(&dir), b"{ not json").unwrap();
        assert_eq!(load(&dir), Settings::default());
    }

    #[test]
    fn load_wrong_typed_known_field_only_resets_that_field_preserves_the_rest() {
        // SPEC R-10.1: a syntactically-valid settings.json with a single
        // wrong-typed known field (e.g. `"notifyIdle": "yes"`) must NOT wipe the
        // whole struct back to defaults. Only that one field resets; every other
        // known field and all unknown keys survive.
        let dir = unique_dir("wrong-typed");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            settings_path(&dir),
            br#"{"notifyIdle":"yes","onboardingDone":true,"launchAtLogin":true,"customKeepMe":"must-survive"}"#,
        )
        .unwrap();

        let loaded = load(&dir);
        // The broken field falls back to its own default.
        assert!(loaded.notify_idle, "wrong-typed notifyIdle → field default");
        // Consent-derived state is NOT silently reset.
        assert!(loaded.onboarding_done, "onboardingDone preserved");
        assert!(loaded.launch_at_login, "launchAtLogin preserved");
        // Unknown keys preserved.
        assert_eq!(
            loaded.extra.get("customKeepMe"),
            Some(&Value::String("must-survive".to_string())),
            "unknown key survives a wrong-typed known field"
        );
        // The wrong-typed known key is not leaked into `extra` (it would
        // otherwise serialize twice alongside the typed field).
        assert!(!loaded.extra.contains_key("notifyIdle"));
    }

    #[test]
    fn load_tolerates_a_utf8_bom_and_preserves_all_keys() {
        // A BOM-prefixed but otherwise valid settings.json (a common Windows
        // editor artifact) must NOT be treated as unparseable and wiped to
        // defaults (SPEC R-10.1: known + unknown keys preserved).
        let dir = unique_dir("bom");
        fs::create_dir_all(&dir).unwrap();
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(
            br#"{"launchAtLogin": true, "onboardingDone": true, "futureFlag": "keep"}"#,
        );
        fs::write(settings_path(&dir), bytes).unwrap();

        let loaded = load(&dir);
        assert!(loaded.launch_at_login, "known key survives the BOM");
        assert!(loaded.onboarding_done);
        assert_eq!(
            loaded.extra.get("futureFlag"),
            Some(&Value::String("keep".to_string())),
            "unknown key preserved across a BOM-prefixed load"
        );
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = unique_dir("roundtrip");
        let settings = Settings {
            notify_reminder: true,
            onboarding_done: true,
            ..Default::default()
        };
        save(&dir, &settings).unwrap();
        assert_eq!(load(&dir), settings);
        // Atomic write leaves no stray tmp files behind.
        let leftover: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftover.is_empty(), "leftover tmp files: {leftover:?}");
    }

    #[test]
    fn unknown_keys_are_preserved_across_a_set_setting_round_trip() {
        let dir = unique_dir("unknown-keys");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            settings_path(&dir),
            br#"{"notifyIdle": true, "notifyAttention": true, "futureFeatureFlag": "beta"}"#,
        )
        .unwrap();

        let updated = set_setting(&dir, "notifyReminder", SettingValue::Bool(true)).unwrap();
        assert!(updated.notify_reminder);
        assert_eq!(
            updated.extra.get("futureFeatureFlag"),
            Some(&Value::String("beta".to_string()))
        );

        // Reload from disk to make sure the unknown key actually made it back
        // out to the file, not just the in-memory struct.
        let reloaded = load(&dir);
        assert_eq!(
            reloaded.extra.get("futureFeatureFlag"),
            Some(&Value::String("beta".to_string()))
        );
    }

    #[test]
    fn apply_sets_known_boolean_fields() {
        let mut settings = Settings::default();
        settings.apply("launchAtLogin", SettingValue::Bool(true));
        assert!(settings.launch_at_login);
        settings.apply("notifyIdle", SettingValue::Bool(false));
        assert!(!settings.notify_idle);
        settings.apply("popupPinned", SettingValue::Bool(true));
        assert!(settings.popup_pinned);
    }

    #[test]
    fn popup_pinned_defaults_off_and_persists() {
        // SPEC R-14.2: pin state persists across restarts, default off (v1.0
        // anchor/hide-on-blur behavior).
        assert!(!Settings::default().popup_pinned);

        let dir = unique_dir("popup-pinned");
        let updated = set_setting(&dir, "popupPinned", SettingValue::Bool(true)).unwrap();
        assert!(updated.popup_pinned);
        assert!(load(&dir).popup_pinned, "pin state survives a reload");
    }

    #[test]
    fn apply_stores_unrecognized_keys_in_extra() {
        let mut settings = Settings::default();
        settings.apply("someNewSetting", SettingValue::Text("x".to_string()));
        assert_eq!(
            settings.extra.get("someNewSetting"),
            Some(&Value::String("x".to_string()))
        );
    }

    #[test]
    fn data_dir_helpers_nest_under_the_root() {
        let dir = PathBuf::from("C:/qd-data");
        assert_eq!(
            resolve_data_dir(Some("C:/qd-data".to_string())),
            dir.clone()
        );
        // Spot check the subpath shape (SPEC §3.3 layout) directly rather than
        // via env vars.
        assert_eq!(dir.join("spool"), PathBuf::from("C:/qd-data/spool"));
        assert_eq!(dir.join("answers"), PathBuf::from("C:/qd-data/answers"));
    }
}
