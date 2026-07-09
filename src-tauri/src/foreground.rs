//! Foreground-window sampling for focus-aware suppression (SPEC §17, R-17.1).
//!
//! Samples the foreground window's root process chain so the shell can tell
//! whether the window the user is looking at belongs to a session's terminal
//! (R-17.2). Windows: `GetForegroundWindow` → pid → walk the parent chain
//! (via a short `powershell` snippet, no new native deps). macOS: the frontmost
//! app pid via `osascript` (best-effort, compile-gated). Tests bypass sampling
//! entirely via the `QUARTERDECK_FAKE_FOREGROUND` env var (a comma-separated
//! pid list), so foreground behaviour is deterministically drivable.

/// Env override used by tests (and the e2e suite) to force the foreground
/// process chain without a real window: a comma-separated list of pids.
pub const FAKE_FOREGROUND_ENV: &str = "QUARTERDECK_FAKE_FOREGROUND";

/// Whether a session's terminal is the foreground window (R-17.2): true iff any
/// of the session's terminal pids appears in the sampled foreground chain. Pure,
/// so the matching rule is unit-testable without a live window.
#[must_use]
pub fn session_is_foreground(terminal_pids: &[u32], foreground_chain: &[u32]) -> bool {
    !terminal_pids.is_empty() && terminal_pids.iter().any(|p| foreground_chain.contains(p))
}

/// Parse the `QUARTERDECK_FAKE_FOREGROUND` override, if set. `Some(vec)` (which
/// may be empty, meaning "nothing is foreground") when the var is present,
/// `None` when it is absent (→ real sampling).
#[must_use]
pub fn fake_foreground_override() -> Option<Vec<u32>> {
    let raw = std::env::var(FAKE_FOREGROUND_ENV).ok()?;
    Some(
        raw.split(',')
            .filter_map(|s| s.trim().parse::<u32>().ok())
            .collect(),
    )
}

/// Sample the foreground window's process chain (R-17.1). Honors the
/// `QUARTERDECK_FAKE_FOREGROUND` override first (tests), else samples the OS.
#[must_use]
pub fn sample_foreground_pids() -> Vec<u32> {
    if let Some(fake) = fake_foreground_override() {
        return fake;
    }
    #[cfg(windows)]
    {
        sample_windows()
    }
    #[cfg(target_os = "macos")]
    {
        sample_macos()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Vec::new()
    }
}

/// The PowerShell snippet that prints the foreground window's pid and its
/// ancestor chain, one pid per line (R-17.1). Pure/const so a test can assert
/// its shape without spawning a shell.
#[cfg(windows)]
const FOREGROUND_SCRIPT: &str = r#"$ErrorActionPreference = 'Stop'
$sig = @'
using System;
using System.Runtime.InteropServices;
public static class QdFg {
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
}
'@
Add-Type -TypeDefinition $sig -ErrorAction Stop | Out-Null
$h = [QdFg]::GetForegroundWindow()
if ($h -eq [IntPtr]::Zero) { exit 0 }
$fgPid = [uint32]0
[QdFg]::GetWindowThreadProcessId($h, [ref]$fgPid) | Out-Null
if ($fgPid -eq 0) { exit 0 }
$procs = @{}
Get-CimInstance -ClassName Win32_Process -ErrorAction SilentlyContinue |
  ForEach-Object { $procs[[int]$_.ProcessId] = $_ }
$walk = [int]$fgPid
for ($i = 0; $i -lt 40; $i++) {
  Write-Output $walk
  $p = $procs[$walk]
  if ($null -eq $p) { break }
  $pp = [int]$p.ParentProcessId
  if ($pp -le 0 -or $pp -eq $walk) { break }
  $walk = $pp
}
"#;

#[cfg(windows)]
fn sample_windows() -> Vec<u32> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    use crate::util::CommandNoWindow;

    let child = Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", "-"])
        .no_console_window()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(err) => {
            tracing::debug!(error = %err, "failed to spawn foreground sampler");
            return Vec::new();
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(FOREGROUND_SCRIPT.as_bytes());
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .collect()
}

#[cfg(target_os = "macos")]
fn sample_macos() -> Vec<u32> {
    use std::process::Command;
    // Frontmost app's pid via System Events (best-effort, R-17.1).
    let script =
        "tell application \"System Events\" to get unix id of first process whose frontmost is true";
    let out = Command::new("osascript").args(["-e", script]).output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse::<u32>()
            .map(|p| vec![p])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_is_foreground_is_pid_intersection() {
        assert!(session_is_foreground(&[100, 200], &[50, 200, 9]));
        assert!(!session_is_foreground(&[100, 200], &[50, 9]));
        // No terminal pids → never foreground (nothing to match).
        assert!(!session_is_foreground(&[], &[100]));
        // Empty foreground → never foreground.
        assert!(!session_is_foreground(&[100], &[]));
    }

    #[test]
    fn fake_override_parses_pid_list() {
        // Uses a process-global env var; run serially with a guard to avoid
        // racing other env-touching tests in this crate.
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());

        std::env::set_var(FAKE_FOREGROUND_ENV, "111, 222,notanumber, 333");
        let pids = fake_foreground_override().unwrap();
        assert_eq!(pids, vec![111, 222, 333]);
        // An empty value → "nothing foreground" (Some(empty)), not None.
        std::env::set_var(FAKE_FOREGROUND_ENV, "");
        assert_eq!(fake_foreground_override(), Some(Vec::new()));
        std::env::remove_var(FAKE_FOREGROUND_ENV);
        assert!(fake_foreground_override().is_none());
    }
}
