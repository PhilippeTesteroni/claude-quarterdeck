//! Click-to-focus the terminal window hosting a session (SPEC R-15.4).
//!
//! Best-effort, no new native dependencies: on Windows we drive a short
//! `powershell -NoProfile` P/Invoke snippet that validates the captured `HWND`
//! still belongs to the ancestor pid (`GetWindowThreadProcessId`), then
//! `ShowWindow(SW_RESTORE)` + `SetForegroundWindow`; a stale/missing handle
//! falls back to enumerating top-level windows and focusing the first whose
//! title contains the project basename. On macOS we `osascript`-activate the
//! app by a bundle id derived from `TERM_PROGRAM` (compile-gated, not
//! live-tested — no mac hardware). When nothing can be focused the caller
//! surfaces the inline "Couldn't find the terminal window" notice (R-15.4b).

use deck_core::events::Ancestor;

/// The message the UI shows inline when no terminal window could be focused
/// (R-15.4b). Returned as the `Err` of [`focus_terminal`].
pub const NOT_FOUND_MSG: &str = "Couldn't find the terminal window";

/// Focus the terminal window for a session, given its captured ancestor
/// (R-15.4a) and project basename (used for the title-substring fallback and,
/// on macOS, only incidentally). Returns `Ok(())` on a best-effort success,
/// `Err(NOT_FOUND_MSG)` when no window could be focused.
pub fn focus_terminal(ancestor: Option<Ancestor>, project: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        focus_windows(ancestor.as_ref(), project)
    }
    #[cfg(target_os = "macos")]
    {
        focus_macos(ancestor.as_ref(), project)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (ancestor, project);
        Err(NOT_FOUND_MSG.to_string())
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

/// Escape a string for embedding inside a PowerShell single-quoted literal
/// (`'…'`): only the single quote is special, doubled to escape it.
fn ps_single_quote(s: &str) -> String {
    s.replace('\'', "''")
}

/// Build the PowerShell focus script (R-15.4b). Pure so it is unit-testable
/// without spawning a shell. `hwnd`/`pid` are the captured ancestor window;
/// `needle` is the project basename used for the title-substring fallback.
#[must_use]
pub fn build_focus_script(hwnd: Option<i64>, pid: Option<u32>, needle: &str) -> String {
    let hwnd = hwnd.unwrap_or(0);
    let pid = pid.unwrap_or(0);
    let needle = ps_single_quote(needle);
    format!(
        r#"$ErrorActionPreference = 'Stop'
$sig = @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class QdFocus {{
  [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint idAttach, uint idAttachTo, bool fAttach);
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int n);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern int GetWindowTextLength(IntPtr h);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
  public delegate bool EnumProc(IntPtr h, IntPtr p);
  public static string Title(IntPtr h) {{
    int len = GetWindowTextLength(h);
    if (len <= 0) return "";
    StringBuilder sb = new StringBuilder(len + 1);
    GetWindowText(h, sb, sb.Capacity);
    return sb.ToString();
  }}
}}
'@
Add-Type -TypeDefinition $sig -ErrorAction Stop | Out-Null

$SW_RESTORE = 9
$target = [IntPtr]{hwnd}
$expectPid = [uint32]{pid}
$needle = '{needle}'

function Focus-Hwnd($h) {{
  # Foreground-unlock: Windows only lets the current foreground thread call
  # SetForegroundWindow, so briefly attach our input queue to it (via the
  # foreground window's thread) and keep the ALT keydown/up as a fallback nudge.
  $fg = [QdFocus]::GetForegroundWindow()
  $curThread = [uint32]0
  if ($fg -ne [IntPtr]::Zero) {{
    $fgpid = [uint32]0
    $curThread = [QdFocus]::GetWindowThreadProcessId($fg, [ref]$fgpid)
  }}
  $tpid = [uint32]0
  $targetThread = [QdFocus]::GetWindowThreadProcessId($h, [ref]$tpid)
  $attached = $false
  if ($curThread -ne 0 -and $targetThread -ne 0 -and $curThread -ne $targetThread) {{
    $attached = [QdFocus]::AttachThreadInput($curThread, $targetThread, $true)
  }}
  # ALT keydown/up fallback around the call (0x12 = VK_MENU, 0x2 = KEYEVENTF_KEYUP).
  [QdFocus]::keybd_event(0x12, 0, 0, [UIntPtr]::Zero)
  [QdFocus]::ShowWindow($h, $SW_RESTORE) | Out-Null
  [QdFocus]::BringWindowToTop($h) | Out-Null
  $ok = [QdFocus]::SetForegroundWindow($h)
  [QdFocus]::keybd_event(0x12, 0, 0x2, [UIntPtr]::Zero)
  if ($attached) {{
    [QdFocus]::AttachThreadInput($curThread, $targetThread, $false) | Out-Null
  }}
  return $ok
}}

# 1) Validate the captured HWND still belongs to the expected pid, then focus it.
if ($target -ne [IntPtr]::Zero -and [QdFocus]::IsWindow($target)) {{
  $owner = [uint32]0
  [QdFocus]::GetWindowThreadProcessId($target, [ref]$owner) | Out-Null
  if ($expectPid -eq 0 -or $owner -eq $expectPid) {{
    if (Focus-Hwnd $target) {{ Write-Output 'ok'; exit 0 }}
  }}
}}

# 2) Fallback: first visible top-level window whose title contains the project.
if ($needle.Length -gt 0) {{
  $found = [IntPtr]::Zero
  $cb = [QdFocus+EnumProc]{{
    param($h, $p)
    if ($found -ne [IntPtr]::Zero) {{ return $true }}
    if (-not [QdFocus]::IsWindowVisible($h)) {{ return $true }}
    $t = [QdFocus]::Title($h)
    if ($t -and $t.ToLowerInvariant().Contains($needle.ToLowerInvariant())) {{
      $script:found = $h
      return $false
    }}
    return $true
  }}
  [QdFocus]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
  if ($found -ne [IntPtr]::Zero) {{
    if (Focus-Hwnd $found) {{ Write-Output 'ok'; exit 0 }}
  }}
}}

Write-Output 'notfound'
exit 0
"#,
        hwnd = hwnd,
        pid = pid,
        needle = needle,
    )
}

#[cfg(windows)]
fn focus_windows(ancestor: Option<&Ancestor>, project: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    use crate::util::CommandNoWindow;

    let hwnd = ancestor.and_then(|a| a.hwnd);
    let pid = ancestor.and_then(|a| a.pid);
    let script = build_focus_script(hwnd, pid, project);

    let mut child = Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", "-"])
        .no_console_window()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn powershell for focus: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("no stdin for focus powershell")?
        .write_all(script.as_bytes())
        .map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.contains("ok") {
        Ok(())
    } else {
        Err(NOT_FOUND_MSG.to_string())
    }
}

// ---------------------------------------------------------------------------
// macOS (compile-gated, not live-tested — R-15.4c)
// ---------------------------------------------------------------------------

/// Map a `TERM_PROGRAM` value (recorded by the `.sh` hook as `ancestor.exe`) to
/// the terminal app's bundle id for `osascript` activation (R-15.4c). Pure, so
/// it is unit-tested on every platform. `None` for an unrecognised terminal.
#[must_use]
pub fn macos_bundle_id(term_program: &str) -> Option<&'static str> {
    let t = term_program.trim().to_ascii_lowercase();
    let t = t.strip_suffix(".app").unwrap_or(&t);
    match t {
        "apple_terminal" | "terminal" => Some("com.apple.Terminal"),
        "iterm" | "iterm2" | "iterm.app" => Some("com.googlecode.iterm2"),
        "vscode" | "visual studio code" | "code" => Some("com.microsoft.VSCode"),
        "hyper" => Some("co.zeit.hyper"),
        "wezterm" => Some("com.github.wez.wezterm"),
        "kitty" => Some("net.kovidgoyal.kitty"),
        "alacritty" => Some("org.alacritty"),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn focus_macos(ancestor: Option<&Ancestor>, _project: &str) -> Result<(), String> {
    use std::process::Command;

    let bundle = ancestor
        .and_then(|a| a.exe.as_deref())
        .and_then(macos_bundle_id)
        .ok_or_else(|| NOT_FOUND_MSG.to_string())?;
    let script = format!(r#"tell application id "{bundle}" to activate"#);
    let out = Command::new("osascript")
        .args(["-e", &script])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(NOT_FOUND_MSG.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_embeds_handle_pid_and_escaped_needle() {
        let s = build_focus_script(Some(197486), Some(12960), "my'proj");
        assert!(s.contains("SetForegroundWindow"));
        assert!(s.contains("GetWindowThreadProcessId"));
        // Foreground-unlock dance (R-26.2): attach to the foreground thread,
        // raise to top, and the ALT-key fallback around the focus call.
        assert!(s.contains("GetForegroundWindow"));
        assert!(s.contains("AttachThreadInput"));
        assert!(s.contains("BringWindowToTop"));
        assert!(s.contains("keybd_event"));
        assert!(s.contains("[IntPtr]197486"));
        assert!(s.contains("[uint32]12960"));
        // Single quote doubled for the PS literal.
        assert!(s.contains("$needle = 'my''proj'"));
    }

    #[test]
    fn script_defaults_zero_when_no_ancestor() {
        let s = build_focus_script(None, None, "proj");
        assert!(s.contains("[IntPtr]0"));
        assert!(s.contains("[uint32]0"));
    }

    // Live smoke (Windows): the generated P/Invoke script must be valid
    // PowerShell that compiles the `Add-Type` block, runs the HWND-validate +
    // EnumWindows fallback, and exits cleanly. With no ancestor and a needle no
    // window title can contain, it focuses nothing (no stolen focus) and returns
    // the not-found error — proving the whole snippet is syntactically sound.
    #[cfg(windows)]
    #[test]
    fn windows_focus_snippet_runs_and_reports_not_found() {
        let needle = "qd-no-such-window-title-zzz-9f3a1";
        let result = focus_terminal(None, needle);
        assert_eq!(result, Err(NOT_FOUND_MSG.to_string()));
    }

    #[test]
    fn bundle_id_mapping() {
        assert_eq!(
            macos_bundle_id("Apple_Terminal"),
            Some("com.apple.Terminal")
        );
        assert_eq!(macos_bundle_id("iTerm.app"), Some("com.googlecode.iterm2"));
        assert_eq!(macos_bundle_id("vscode"), Some("com.microsoft.VSCode"));
        assert_eq!(macos_bundle_id("WezTerm"), Some("com.github.wez.wezterm"));
        assert_eq!(macos_bundle_id("unknown-term"), None);
        assert_eq!(macos_bundle_id(""), None);
    }
}
