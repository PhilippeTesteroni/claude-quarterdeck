//! Small cross-cutting helpers for the Tauri shell.
//!
//! §26.1: spawning a console subprocess (`powershell.exe` for foreground
//! sampling / click-to-focus, `claude` for MCP registration) from the GUI
//! process flashes a black console window for a beat. [`CommandNoWindow`]
//! suppresses it with the Windows `CREATE_NO_WINDOW` creation flag; the other
//! platforms get a no-op passthrough so call sites stay platform-agnostic.

use std::process::Command;

/// The Windows `CREATE_NO_WINDOW` process-creation flag (§26.1). Named so the
/// intent (and the magic `0x08000000`) is greppable.
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Extension for [`Command`] that stops a spawned console subprocess from
/// flashing a window (§26.1). On Windows it sets `CREATE_NO_WINDOW`; elsewhere
/// it is a no-op passthrough so the builder chain stays identical on every
/// platform. Behaviour (stdin pipe, stdout capture, exit code) is unchanged.
pub trait CommandNoWindow {
    fn no_console_window(&mut self) -> &mut Self;
}

impl CommandNoWindow for Command {
    fn no_console_window(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            self.creation_flags(CREATE_NO_WINDOW);
        }
        self
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn create_no_window_flag_value() {
        // The flag the helper applies must be exactly CREATE_NO_WINDOW.
        assert_eq!(CREATE_NO_WINDOW, 0x0800_0000);
    }

    #[test]
    fn no_console_window_still_spawns() {
        // The flagged command runs (the flag is valid and applied to the real
        // spawn), just without a console window flashing (§26.1).
        let status = Command::new("cmd")
            .args(["/C", "exit", "0"])
            .no_console_window()
            .status()
            .expect("flagged command spawns");
        assert!(status.success());
    }
}
