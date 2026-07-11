//! Liveness checks (SPEC §6): PID-backed sessions verified against a
//! [`crate::traits::ProcessTable`]; inferred/PID-less sessions expire when their
//! transcript is untouched for more than 6 h.
//!
//! Pure decision logic only — the actual polling cadence and the `sysinfo`
//! implementation of [`ProcessTable`] live in the shell (T3). This keeps the
//! rule testable with a fake process table.

use crate::traits::ProcessTable;

/// A PID-less session is declared `dead` once its transcript has been untouched
/// for longer than this (R-6.2).
pub const LIVENESS_STALE_MS: u64 = 6 * 60 * 60 * 1000;

/// §38 lingering-row tighten: a PID-less row whose ONLY activity signal is a
/// live-registry `updatedAt` is declared `dead` once that is stale beyond this
/// (much shorter than [`LIVENESS_STALE_MS`]). The registry is re-polled every
/// 10 s and a live session's `~/.claude/sessions/<id>.json` is heartbeated far
/// more often than 15 min, so a registry file untouched this long is an
/// abandoned/undeleted ghost — Claude Code does not always remove it on exit,
/// and the old 6 h transcript grace kept such a pid-less ghost row on the deck
/// for hours. A row with a live pid (the common case) is unaffected — it is
/// verified against the process table, not this window — as is a row whose
/// transcript is genuinely fresh (that still honors the 6 h R-6.2 grace).
pub const REGISTRY_STALE_MS: u64 = 15 * 60 * 1000;

/// The inputs a liveness check needs about one session.
#[derive(Debug, Clone, Copy)]
pub struct LivenessInput {
    /// Nearest-ancestor Claude PID captured at `SessionStart` (R-4.3), if known.
    pub claude_pid: Option<u32>,
    /// Last-modified time of the session transcript, epoch millis, if known.
    pub transcript_mtime_ms: Option<u64>,
    /// Live-registry `updatedAt` (epoch ms) for a registry-known session
    /// (R-15.3), when present. A registry-discovered row can be PID-less AND
    /// carry no transcript yet; the registry file's freshness is then the only
    /// activity signal, so it must count for the R-6.2 staleness window instead
    /// of the row being declared dead on the very next tick.
    pub registry_updated_at_ms: Option<u64>,
}

/// Decide whether a session is dead.
///
/// * With a PID (R-6.1): dead iff no live process has that PID, or its name no
///   longer matches `claude|node|bun` (PID reuse guard).
/// * Without a PID (R-6.2 / R-15.3 / §38): dead iff neither activity signal is
///   fresh — the transcript mtime within the 6 h R-6.2 grace, OR the live-registry
///   `updatedAt` within the much shorter [`REGISTRY_STALE_MS`] ghost-file window.
#[must_use]
pub fn is_dead(input: &LivenessInput, procs: &impl ProcessTable, now_ms: u64) -> bool {
    match input.claude_pid {
        Some(pid) => match procs.process_name(pid) {
            Some(name) => !is_claude_process(&name),
            None => true,
        },
        None => {
            // R-6.2 / R-15.3 / §38: a PID-less session stays alive while *some*
            // activity signal is fresh, but the transcript and the registry file
            // are held to DIFFERENT windows. A transcript is only touched on real
            // model activity, so its untouched-grace stays the generous 6 h
            // (R-6.2). A registry file, by contrast, is heartbeated every few
            // seconds while the session lives (re-polled every 10 s, and once the
            // entry vanishes the shell clears this back to `None`), so a registry
            // `updatedAt` stale past the short [`REGISTRY_STALE_MS`] window is an
            // abandoned/undeleted ghost file — not activity — and no longer keeps
            // the row alive (§38 lingering-row fix). Dead iff neither is fresh.
            let transcript_fresh = input
                .transcript_mtime_ms
                .is_some_and(|m| now_ms.saturating_sub(m) <= LIVENESS_STALE_MS);
            let registry_fresh = input
                .registry_updated_at_ms
                .is_some_and(|m| now_ms.saturating_sub(m) <= REGISTRY_STALE_MS);
            !(transcript_fresh || registry_fresh)
        }
    }
}

/// Whether a process name looks like a Claude Code host process. Matches the
/// `claude|node|bun` set the hook uses for its ancestor walk (R-4.3),
/// case-insensitively and ignoring a `.exe` suffix on Windows.
#[must_use]
pub fn is_claude_process(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    matches!(stem, "claude" | "node" | "bun")
}
