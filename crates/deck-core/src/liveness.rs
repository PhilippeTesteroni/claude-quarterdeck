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
/// * Without a PID (R-6.2 / R-15.3): dead iff the freshest activity signal —
///   the transcript mtime and/or the live-registry `updatedAt` — is stale > 6 h,
///   or no signal exists at all.
#[must_use]
pub fn is_dead(input: &LivenessInput, procs: &impl ProcessTable, now_ms: u64) -> bool {
    match input.claude_pid {
        Some(pid) => match procs.process_name(pid) {
            Some(name) => !is_claude_process(&name),
            None => true,
        },
        None => {
            // R-6.2 / R-15.3: a PID-less session stays alive while *some*
            // activity signal is fresh. A registry-discovered row (R-15.3) may
            // have no transcript yet — its registry `updatedAt` is the liveness
            // anchor (the registry is re-polled every 10 s, and once the entry
            // vanishes the shell clears this back to `None`, so the row then
            // falls through to dead). Take the most recent of the two signals;
            // dead iff that is stale > 6 h, or neither exists.
            let freshest = input
                .transcript_mtime_ms
                .into_iter()
                .chain(input.registry_updated_at_ms)
                .max();
            match freshest {
                Some(activity) => now_ms.saturating_sub(activity) > LIVENESS_STALE_MS,
                None => true,
            }
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
