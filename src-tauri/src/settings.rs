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

/// `<data>/perms/` — pending permission requests written by the
/// `PermissionRequest` hook (SPEC §16, R-16.1).
pub fn perms_dir() -> PathBuf {
    data_dir().join("perms")
}

/// `<data>/perm-answers/` — decisions the deck writes for the blocked
/// `PermissionRequest` hook to poll (SPEC R-16.1).
pub fn perm_answers_dir() -> PathBuf {
    data_dir().join("perm-answers")
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

/// Popup display mode (SPEC §25, R-25.2): `List` is the v1.0/v1.1 popup; `Lamp`
/// is the compact ~56x56 always-on-top traffic light (R-25.1). Persisted so a
/// pinned+collapsed popup reopens collapsed after a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PopupMode {
    #[default]
    List,
    Lamp,
}

impl PopupMode {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "list" => Some(Self::List),
            "lamp" => Some(Self::Lamp),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Lamp => "lamp",
        }
    }
}

/// Persisted popup position (SPEC R-25.2 `popupPos`), logical pixels. Written
/// only while the popup is pinned (an unpinned popup always re-anchors to the
/// tray on open, R-14.2, so its position is never meaningful to restore).
/// Read back at startup to put a pinned (and possibly collapsed) popup back
/// where the user left it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PopupPos {
    pub x: f64,
    pub y: f64,
}

impl PopupPos {
    /// Parses the `"x,y"` wire form used by the generic `set_setting` channel
    /// (SPEC R-25.2; `SettingValue` only carries bool/string, so the shell-side
    /// writer in `windows.rs` encodes the position as a comma-joined string).
    /// Returns `None` for anything malformed rather than panicking — a stray
    /// hand-edit of `settings.json` must degrade to "no remembered position",
    /// never crash the load path (mirrors the rest of this module's posture).
    fn parse(s: &str) -> Option<Self> {
        let (x, y) = s.split_once(',')?;
        Some(Self {
            x: x.trim().parse().ok()?,
            y: y.trim().parse().ok()?,
        })
    }

    pub fn to_setting_string(self) -> String {
        format!("{},{}", self.x, self.y)
    }
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
    /// Take over Claude Code permission prompts into the deck (SPEC §16,
    /// R-16.4). Default ON (after onboarding consent, R-25.4). Drives whether
    /// the installer adds the `PermissionRequest` hook; toggling it add/removes
    /// only that entry.
    #[serde(default = "default_true")]
    pub takeover_permissions: bool,
    /// Show per-session token usage on rows (SPEC §23, R-23.5). Default ON. When
    /// off, the incremental transcript reader is idle and the row usage line +
    /// the finished-toast "last words" body (R-24.1) are suppressed.
    #[serde(default = "default_true")]
    pub show_token_stats: bool,
    /// Popup display mode (SPEC §25, R-25.2): `list` (default) or `lamp` (the
    /// compact traffic-light square, R-25.1). Only reachable while pinned; see
    /// [`crate::windows::should_force_list_on_unpin`].
    #[serde(default)]
    pub popup_mode: PopupMode,
    /// Last user-dragged popup position while pinned (SPEC R-25.2), restored at
    /// startup so a pinned (possibly collapsed) popup reopens where it was left.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub popup_pos: Option<PopupPos>,
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
            takeover_permissions: true,
            show_token_stats: true,
            popup_mode: PopupMode::List,
            popup_pos: None,
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
        // `popupMode`: an unrecognized/wrong-typed value falls back to the field
        // default (`List`), mirroring `take_bool`'s "reset just this field" policy
        // rather than discarding the rest of the file.
        let popup_mode = match map.remove("popupMode") {
            Some(Value::String(s)) => PopupMode::parse(&s).unwrap_or_default(),
            _ => PopupMode::default(),
        };
        // `popupPos`: stored on disk as `{"x":..,"y":..}` (the derived `Serialize`
        // shape); a missing/malformed value is simply "no remembered position",
        // never a load failure.
        let popup_pos = match map.remove("popupPos") {
            Some(Value::Object(mut obj)) => {
                let x = obj.remove("x").and_then(|v| v.as_f64());
                let y = obj.remove("y").and_then(|v| v.as_f64());
                x.zip(y).map(|(x, y)| PopupPos { x, y })
            }
            _ => None,
        };

        Some(Self {
            notify_idle: take_bool(&mut map, "notifyIdle", true),
            notify_attention: take_bool(&mut map, "notifyAttention", true),
            notify_reminder: take_bool(&mut map, "notifyReminder", false),
            launch_at_login: take_bool(&mut map, "launchAtLogin", false),
            onboarding_done: take_bool(&mut map, "onboardingDone", false),
            popup_pinned: take_bool(&mut map, "popupPinned", false),
            takeover_permissions: take_bool(&mut map, "takeoverPermissions", true),
            show_token_stats: take_bool(&mut map, "showTokenStats", true),
            popup_mode,
            popup_pos,
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
            "takeoverPermissions" => self.takeover_permissions = value.as_bool_lossy(),
            "showTokenStats" => self.show_token_stats = value.as_bool_lossy(),
            "popupMode" => {
                if let SettingValue::Text(s) = &value {
                    // An unrecognized string is ignored (keeps the current mode)
                    // rather than silently snapping back to `List` on a stray
                    // caller — unlike a wrong-TYPED value (bool instead of
                    // string), where falling back is the established policy.
                    if let Some(mode) = PopupMode::parse(s) {
                        self.popup_mode = mode;
                    }
                }
            }
            "popupPos" => {
                if let SettingValue::Text(s) = &value {
                    self.popup_pos = PopupPos::parse(s).or(self.popup_pos);
                }
            }
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
    fn takeover_permissions_defaults_on_and_persists() {
        // SPEC R-16.4: "Take over permission prompts" defaults ON. A settings
        // file with no `takeoverPermissions` key (a pre-v1.1 file) must load as
        // ON, and an explicit false must round-trip.
        assert!(Settings::default().takeover_permissions);

        let dir = unique_dir("takeover-missing");
        fs::create_dir_all(&dir).unwrap();
        fs::write(settings_path(&dir), br#"{"notifyIdle":true}"#).unwrap();
        assert!(
            load(&dir).takeover_permissions,
            "absent key defaults ON (R-16.4)"
        );

        let updated = set_setting(&dir, "takeoverPermissions", SettingValue::Bool(false)).unwrap();
        assert!(!updated.takeover_permissions);
        assert!(
            !load(&dir).takeover_permissions,
            "explicit off survives a reload"
        );
    }

    #[test]
    fn show_token_stats_defaults_on_and_persists() {
        // SPEC R-23.5: token stats default ON. A pre-v1.2 file with no
        // `showTokenStats` key must load as ON, and an explicit false round-trips.
        assert!(Settings::default().show_token_stats);

        let dir = unique_dir("token-stats-missing");
        fs::create_dir_all(&dir).unwrap();
        fs::write(settings_path(&dir), br#"{"notifyIdle":true}"#).unwrap();
        assert!(
            load(&dir).show_token_stats,
            "absent key defaults ON (R-23.5)"
        );

        let updated = set_setting(&dir, "showTokenStats", SettingValue::Bool(false)).unwrap();
        assert!(!updated.show_token_stats);
        assert!(
            !load(&dir).show_token_stats,
            "explicit off survives a reload"
        );
    }

    #[test]
    fn popup_mode_defaults_list_and_persists() {
        // SPEC R-25.2: default mode is `list`; a pre-v1.2 file with no
        // `popupMode` key must load as `list`.
        assert_eq!(Settings::default().popup_mode, PopupMode::List);

        let dir = unique_dir("popup-mode-missing");
        fs::create_dir_all(&dir).unwrap();
        fs::write(settings_path(&dir), br#"{"notifyIdle":true}"#).unwrap();
        assert_eq!(load(&dir).popup_mode, PopupMode::List);

        let updated =
            set_setting(&dir, "popupMode", SettingValue::Text("lamp".to_string())).unwrap();
        assert_eq!(updated.popup_mode, PopupMode::Lamp);
        assert_eq!(
            load(&dir).popup_mode,
            PopupMode::Lamp,
            "explicit lamp survives a reload"
        );
    }

    #[test]
    fn popup_mode_unrecognized_value_falls_back_to_list_on_load_but_keeps_current_on_apply() {
        // A hand-edited/garbage `popupMode` on disk (bad TYPE-of-value case) must
        // not crash the load path — it resets just this field, same as a
        // wrong-typed boolean (R-10.1's "single bad known key" policy).
        let dir = unique_dir("popup-mode-garbage");
        fs::create_dir_all(&dir).unwrap();
        fs::write(settings_path(&dir), br#"{"popupMode":"sideways"}"#).unwrap();
        assert_eq!(load(&dir).popup_mode, PopupMode::List);

        // `apply()` (the live `set_setting` path) instead keeps the CURRENT
        // in-memory mode on a garbage string, rather than silently reverting a
        // user's lamp mode back to list on a stray/misbehaving caller.
        let mut settings = Settings {
            popup_mode: PopupMode::Lamp,
            ..Settings::default()
        };
        settings.apply("popupMode", SettingValue::Text("sideways".to_string()));
        assert_eq!(
            settings.popup_mode,
            PopupMode::Lamp,
            "garbage value ignored, not reset"
        );
    }

    #[test]
    fn popup_pos_round_trips_through_the_comma_wire_form_and_disk_object() {
        // SPEC R-25.2 `popupPos`: `windows.rs` persists via the generic
        // `set_setting` channel as a comma string; on disk it's a `{x,y}` object
        // (the derived `Serialize` shape) that `from_json_value` reads back.
        assert_eq!(Settings::default().popup_pos, None);

        let dir = unique_dir("popup-pos");
        let updated = set_setting(
            &dir,
            "popupPos",
            SettingValue::Text("120.5,340".to_string()),
        )
        .unwrap();
        assert_eq!(updated.popup_pos, Some(PopupPos { x: 120.5, y: 340.0 }));

        let reloaded = load(&dir);
        assert_eq!(reloaded.popup_pos, Some(PopupPos { x: 120.5, y: 340.0 }));

        // The on-disk shape really is a `{x,y}` object, not the wire string.
        let raw = fs::read_to_string(settings_path(&dir)).unwrap();
        let value: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["popupPos"]["x"], 120.5);
        assert_eq!(value["popupPos"]["y"], 340.0);
    }

    #[test]
    fn popup_pos_malformed_wire_value_is_ignored_not_crashing() {
        let mut settings = Settings::default();
        settings.apply("popupPos", SettingValue::Text("not-a-position".to_string()));
        assert_eq!(settings.popup_pos, None, "malformed value leaves it unset");

        settings.popup_pos = Some(PopupPos { x: 1.0, y: 2.0 });
        settings.apply("popupPos", SettingValue::Text("garbage".to_string()));
        assert_eq!(
            settings.popup_pos,
            Some(PopupPos { x: 1.0, y: 2.0 }),
            "malformed value keeps the previous position rather than wiping it"
        );
    }

    #[test]
    fn popup_pos_to_setting_string_round_trips() {
        let pos = PopupPos { x: 12.0, y: -4.5 };
        let s = pos.to_setting_string();
        assert_eq!(s, "12,-4.5");
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
