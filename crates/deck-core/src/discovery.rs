//! Cold-start discovery of sessions from `~/.claude/projects/*/*.jsonl`
//! transcripts (SPEC R-5.4). The base dir is overridable via
//! `QUARTERDECK_CLAUDE_DIR` for test isolation.
//!
//! On startup, after spool replay, we scan recent transcripts and synthesise
//! `inferred` rows for sessions we don't already know about, so the deck shows
//! agents that were already running before Quarterdeck launched.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::engine::{SessionStore, Status};
use crate::naming;

/// Only transcripts modified within this window are considered live (R-5.4).
pub const DISCOVERY_MAX_AGE_MS: u64 = 6 * 60 * 60 * 1000;

/// A transcript touched within this window is inferred as `working`, else `idle`
/// (R-5.4).
pub const DISCOVERY_WORKING_WINDOW_MS: u64 = 30 * 1000;

/// An inferred session reconstructed from a transcript (R-5.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSession {
    pub id: String,
    pub cwd: Option<String>,
    pub transcript_path: String,
    pub status: Status,
    pub title: String,
    /// Transcript mtime in epoch millis (drives liveness for the PID-less row).
    pub mtime_ms: u64,
}

/// Resolve the Claude config dir: `QUARTERDECK_CLAUDE_DIR` if set, else
/// `~/.claude` (`%USERPROFILE%\.claude` on Windows).
#[must_use]
pub fn claude_dir_from_env() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("QUARTERDECK_CLAUDE_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    let home = std::env::var_os("USERPROFILE")
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var_os("HOME").filter(|s| !s.is_empty()))?;
    Some(PathBuf::from(home).join(".claude"))
}

fn mtime_ms(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(
        modified
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    )
}

/// Scan `<claude_dir>/projects/*/*.jsonl` and return inferred sessions for every
/// transcript that is (a) recent (mtime < 6 h) and (b) not already known.
#[must_use]
pub fn discover_sessions(
    claude_dir: &Path,
    known: &HashSet<String>,
    now_ms: u64,
) -> Vec<DiscoveredSession> {
    let projects_dir = claude_dir.join("projects");
    let mut found = Vec::new();

    let Ok(project_dirs) = fs::read_dir(&projects_dir) else {
        return found;
    };
    for project in project_dirs.flatten() {
        let ppath = project.path();
        if !ppath.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&ppath) else {
            continue;
        };
        for file in files.flatten() {
            let fpath = file.path();
            if fpath.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = fpath
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            if known.contains(id) {
                continue;
            }
            let Some(mtime) = mtime_ms(&fpath) else {
                continue;
            };
            // R-5.4: only transcripts touched within the last 6 h (strict
            // `<6 h`: a transcript exactly at the boundary is excluded).
            if now_ms.saturating_sub(mtime) >= DISCOVERY_MAX_AGE_MS {
                continue;
            }
            let status = if now_ms.saturating_sub(mtime) < DISCOVERY_WORKING_WINDOW_MS {
                Status::Working
            } else {
                Status::Idle
            };
            let cwd = naming::transcript_cwd(&fpath);
            let fallback = naming::transcript_first_user_text(&fpath);
            let title = naming::title_from_sources(None, None, fallback.as_deref());
            found.push(DiscoveredSession {
                id: id.to_string(),
                cwd,
                transcript_path: fpath.to_string_lossy().into_owned(),
                status,
                title,
                mtime_ms: mtime,
            });
        }
    }

    // Most-recently-active first, for stable ordering.
    found.sort_by(|a, b| b.mtime_ms.cmp(&a.mtime_ms).then_with(|| a.id.cmp(&b.id)));
    found
}

/// Discover and merge inferred rows straight into `store` (skips already-known
/// sessions). Returns the number of rows inserted.
pub fn merge_into_store(store: &mut SessionStore, claude_dir: &Path, now_ms: u64) -> usize {
    let known = store.known_ids();
    let discovered = discover_sessions(claude_dir, &known, now_ms);
    let mut inserted = 0;
    for d in discovered {
        if !store.contains(&d.id) {
            store.add_inferred(
                d.id,
                d.cwd,
                Some(d.transcript_path),
                d.status,
                d.title,
                d.mtime_ms,
            );
            inserted += 1;
        }
    }
    inserted
}
