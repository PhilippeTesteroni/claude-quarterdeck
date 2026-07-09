//! Persistence for user session-name overrides (SPEC §27, R-27.3): the flat
//! `<data>/session-names.json` map `{ "<sessionId>": "<name>" }` behind the
//! rename-by-double-click feature.
//!
//! The engine's [`deck_core::engine::SessionStore::overrides`] map is the live
//! authority; this module only loads it at startup and writes it back (atomic,
//! via [`crate::settings::atomic_write`]) whenever a rename or an end-of-session
//! prune marks it dirty. Deliberately NOT overloaded onto `settings.json` and
//! never touching Claude Code's own (foreign, read-only) registry files.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

/// `<dir>/session-names.json`.
pub fn session_names_path(dir: &Path) -> PathBuf {
    dir.join("session-names.json")
}

/// Strip a leading UTF-8 BOM (`EF BB BF`) if present — Windows editors prepend
/// one and `serde_json` would otherwise reject the file. Mirrors
/// `settings::strip_bom` for the sibling `settings.json`.
fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// Load the override map from `<dir>/session-names.json`. A missing, unparseable,
/// or wrong-shaped file degrades to an empty map rather than crashing the shell
/// (mirrors the settings/spool "never crash on malformed input" posture) — a bad
/// hand-edit just means "no remembered names", never a startup failure. Non-string
/// values are dropped individually so one bad entry can't discard the rest.
#[must_use]
pub fn load(dir: &Path) -> HashMap<String, String> {
    let Ok(text) = std::fs::read_to_string(session_names_path(dir)) else {
        return HashMap::new();
    };
    match serde_json::from_str::<serde_json::Value>(strip_bom(&text)) {
        Ok(serde_json::Value::Object(map)) => map
            .into_iter()
            .filter_map(|(k, v)| {
                let name = v.as_str().map(str::trim).filter(|s| !s.is_empty())?;
                Some((k, name.to_string()))
            })
            .collect(),
        Ok(_) => {
            tracing::warn!("session-names.json is not a JSON object, ignoring");
            HashMap::new()
        }
        Err(err) => {
            tracing::warn!(error = %err, "session-names.json unparseable, ignoring");
            HashMap::new()
        }
    }
}

/// Atomically persist the override map to `<dir>/session-names.json` (R-27.3).
pub fn save(dir: &Path, overrides: &HashMap<String, String>) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(overrides).map_err(io::Error::other)?;
    crate::settings::atomic_write(&session_names_path(dir), &json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "quarterdeck-names-test-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn load_missing_file_returns_empty_map() {
        assert!(load(&unique_dir("missing")).is_empty());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = unique_dir("roundtrip");
        let mut map = HashMap::new();
        map.insert("s1".to_string(), "My renamed session".to_string());
        map.insert("s2".to_string(), "Другое имя".to_string());
        save(&dir, &map).unwrap();
        assert_eq!(load(&dir), map);
        // Atomic write leaves no stray tmp files behind.
        let leftover: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftover.is_empty(), "leftover tmp files: {leftover:?}");
    }

    #[test]
    fn load_malformed_file_returns_empty_never_crashes() {
        let dir = unique_dir("malformed");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(session_names_path(&dir), b"{ not json").unwrap();
        assert!(load(&dir).is_empty());
    }

    #[test]
    fn load_drops_non_string_and_blank_entries_but_keeps_the_rest() {
        let dir = unique_dir("mixed");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            session_names_path(&dir),
            br#"{"good":"Keep me","numeric":5,"blank":"   "}"#,
        )
        .unwrap();
        let loaded = load(&dir);
        assert_eq!(loaded.get("good").map(String::as_str), Some("Keep me"));
        assert!(!loaded.contains_key("numeric"));
        assert!(!loaded.contains_key("blank"));
    }

    #[test]
    fn load_tolerates_a_utf8_bom() {
        let dir = unique_dir("bom");
        std::fs::create_dir_all(&dir).unwrap();
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(br#"{"s1":"named"}"#);
        std::fs::write(session_names_path(&dir), bytes).unwrap();
        assert_eq!(load(&dir).get("s1").map(String::as_str), Some("named"));
    }
}
