//! Quarterdeck Tauri shell.
//!
//! T0 wires the skeleton: the tray icon, the two windows (declared in
//! `tauri.conf.json` — a hidden-on-blur popup and an always-on-top ask window),
//! and the notification + autostart plugins. The module bodies are scaffolded
//! empty and filled in by later tasks (see `TASKS.md`):
//!
//! * [`tray`], [`windows`], [`ipc`], [`settings`], [`watcher`] — T3
//! * [`notify`] — T5
//! * [`mcp_server`] — T6
//!
//! Composition (invoke handlers, event wiring, spool replay) is finalized by T7
//! in this file and `main.rs`.

pub mod ipc;
pub mod mcp_server;
pub mod notify;
pub mod settings;
pub mod tray;
pub mod watcher;
pub mod windows;

use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;
use tauri_plugin_autostart::MacosLauncher;

/// Builds and runs the Quarterdeck application.
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            // Placeholder neutral (gray) tray icon; T3 swaps the five status
            // variants at runtime (SPEC R-2.6).
            let icon = tauri::include_image!("../assets/tray/gray-32.png");
            TrayIconBuilder::with_id("quarterdeck-tray")
                .icon(icon)
                .tooltip("Quarterdeck")
                .on_tray_icon_event(|tray, event| {
                    // Left-click toggles the popup into view. Real anchoring and
                    // hide-on-blur behaviour lands in T3 (SPEC R-7.1).
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        if let Some(popup) = tray.app_handle().get_webview_window("popup") {
                            let _ = popup.show();
                            let _ = popup.set_focus();
                        }
                    }
                })
                .build(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Quarterdeck");
}
