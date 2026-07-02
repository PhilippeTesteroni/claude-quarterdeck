//! Window management: the frameless popup anchored to the tray with hide-on-blur
//! / Esc (SPEC R-7.1), and the always-on-top ask window that does not steal
//! keyboard focus on appear (SPEC R-8.3, `WS_EX_NOACTIVATE`-equivalent).
//!
//! Filled in by T3.

use std::sync::Mutex;

use tauri::{AppHandle, LogicalSize, Manager, PhysicalPosition, Rect, WebviewWindow, WindowEvent};

/// Label of the popup window declared in `tauri.conf.json`.
pub const POPUP_LABEL: &str = "popup";
/// Label of the always-on-top ask window declared in `tauri.conf.json`.
pub const ASK_LABEL: &str = "ask";

/// Gap, in physical pixels, kept between the tray icon and the anchored
/// popup (SPEC R-7.1 "anchored to tray icon").
const ANCHOR_GAP_PX: i32 = 6;

/// Popup logical width and the base/cap heights (SPEC R-7.1: "360×460
/// (max-height 560 then scroll)"): it grows with content from 460 up to a
/// 560 cap, beyond which the content area scrolls.
const POPUP_W: f64 = 360.0;
const POPUP_MIN_H: f64 = 460.0;
const POPUP_MAX_H: f64 = 560.0;

/// The tray-icon rect the popup was last anchored to. Kept so a content-driven
/// [`resize_popup_to_content`] can re-anchor the (possibly visible) popup
/// without waiting for a fresh tray click — otherwise growing the window would
/// extend it over the taskbar/tray instead of away from it.
static LAST_TRAY_RECT: Mutex<Option<Rect>> = Mutex::new(None);

fn remember_tray_rect(rect: Rect) {
    if let Ok(mut guard) = LAST_TRAY_RECT.lock() {
        *guard = Some(rect);
    }
}

fn last_tray_rect() -> Option<Rect> {
    LAST_TRAY_RECT.lock().ok().and_then(|guard| *guard)
}

/// Pure clamp for the popup's grow-then-scroll band (SPEC R-7.1): content
/// shorter than 460 keeps the base height; taller content grows up to 560,
/// beyond which the window stays at 560 and the content area scrolls.
fn popup_target_height(content_px: f64) -> f64 {
    content_px.clamp(POPUP_MIN_H, POPUP_MAX_H)
}

fn popup_window(app: &AppHandle) -> Result<WebviewWindow, String> {
    app.get_webview_window(POPUP_LABEL)
        .ok_or_else(|| format!("window `{POPUP_LABEL}` not found (composed by T7)"))
}

fn ask_window(app: &AppHandle) -> Result<WebviewWindow, String> {
    app.get_webview_window(ASK_LABEL)
        .ok_or_else(|| format!("window `{ASK_LABEL}` not found (composed by T7)"))
}

/// Registers hide-on-blur and hide-on-Escape for the popup (SPEC R-7.1). Call
/// once at startup (composed by T7); the popup is created once and only
/// hidden/shown afterwards (SPEC R-3.6), so both listeners stay attached for
/// the app's lifetime — no need to re-register on every show.
pub fn setup_popup_behavior(app: &AppHandle) -> Result<(), String> {
    let popup = popup_window(app)?;

    let on_event = popup.clone();
    popup.on_window_event(move |event| match event {
        WindowEvent::Focused(false) => {
            let _ = on_event.hide();
        }
        // R-3.6: the popup is created once and only hidden/shown — never
        // destroyed. A native close (the OS window `closable: false` flag is
        // best-effort and does not cover every accelerator, e.g. Alt+F4) would
        // otherwise destroy the webview and permanently break the tray, since
        // `popup_window()` would return `None` on every subsequent click.
        // Prevent the destroy and hide instead.
        WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            let _ = on_event.hide();
        }
        _ => {}
    });

    // Esc-to-hide: injected via the global Tauri JS API
    // (`app.withGlobalTauri = true` in tauri.conf.json) so this module alone
    // satisfies the AC without touching `ui/**` (T4-owned). T4's own
    // popup.ts may add the same handler directly for defense in depth; both
    // are idempotent (hiding an already-hidden window is a no-op).
    popup
        .eval(
            "window.addEventListener('keydown', function (event) { \
                if (event.key === 'Escape' && window.__TAURI__ && window.__TAURI__.window) { \
                    window.__TAURI__.window.getCurrentWindow().hide(); \
                } \
            });",
        )
        .map_err(|err| err.to_string())
}

/// Positions the popup near the tray icon and shows/focuses it, or hides it
/// if it's already visible (SPEC R-7.1: click toggles).
pub fn toggle_popup(app: &AppHandle, tray_rect: Rect) -> Result<(), String> {
    remember_tray_rect(tray_rect);
    let popup = popup_window(app)?;
    let visible = popup.is_visible().map_err(|err| err.to_string())?;
    if visible {
        return popup.hide().map_err(|err| err.to_string());
    }
    anchor_near_tray(&popup, tray_rect)?;
    popup.show().map_err(|err| err.to_string())?;
    popup.set_focus().map_err(|err| err.to_string())
}

/// Shows and focuses the popup near the tray icon (used by toast-click routing,
/// SPEC R-9.6). Unlike [`toggle_popup`] it never hides an already-open popup —
/// a toast click should always bring the deck forward.
pub fn open_popup(app: &AppHandle) -> Result<(), String> {
    let popup = popup_window(app)?;
    if let Some(rect) = last_tray_rect() {
        let _ = anchor_near_tray(&popup, rect);
    }
    popup.show().map_err(|err| err.to_string())?;
    popup.set_focus().map_err(|err| err.to_string())
}

/// Resizes the popup to fit `content_px` logical pixels of content, clamped to
/// the 460..=560 band (SPEC R-7.1 grow-then-scroll). A visible popup is
/// re-anchored so it grows away from the tray edge rather than over it. The
/// frontend measures its own content height and drives this via the
/// `resize_popup` command (R-3.4: logic stays in Rust, the view just reports a
/// number).
pub fn resize_popup_to_content(app: &AppHandle, content_px: f64) -> Result<(), String> {
    let popup = popup_window(app)?;
    let target_h = popup_target_height(content_px);
    let scale = popup.scale_factor().map_err(|err| err.to_string())?;
    let current_h = popup.inner_size().map_err(|err| err.to_string())?.height as f64 / scale;
    // Avoid churn: only resize when the height meaningfully changes.
    if (current_h - target_h).abs() < 1.0 {
        return Ok(());
    }
    popup
        .set_size(LogicalSize::new(POPUP_W, target_h))
        .map_err(|err| err.to_string())?;
    if popup.is_visible().unwrap_or(false) {
        if let Some(rect) = last_tray_rect() {
            let _ = anchor_near_tray(&popup, rect);
        }
    }
    Ok(())
}

/// Pure geometry for [`anchor_near_tray`]: given the tray icon's rect, the
/// popup's own size, and the monitor's work area (all in physical pixels),
/// returns the top-left position to place the popup at. Kept side-effect
/// free so it's unit-testable without a live window.
///
/// Anchors above the tray icon when it sits in the lower half of the work
/// area (Windows taskbar), below it otherwise (macOS menu bar); clamps
/// horizontally so the popup never runs off either edge of the screen.
fn compute_anchor_position(
    tray_pos: (i32, i32),
    tray_size: (u32, u32),
    popup_size: (u32, u32),
    work_area: ((i32, i32), (u32, u32)),
) -> (i32, i32) {
    let (tray_x, tray_y) = tray_pos;
    let (tray_w, _tray_h) = tray_size;
    let (popup_w, popup_h) = popup_size;
    let ((work_x, work_y), (work_w, work_h)) = work_area;

    let tray_center_x = tray_x + tray_w as i32 / 2;
    let max_x = (work_x + work_w as i32 - popup_w as i32).max(work_x);
    let x = (tray_center_x - popup_w as i32 / 2).clamp(work_x, max_x);

    let work_mid_y = work_y + work_h as i32 / 2;
    let y = if tray_y > work_mid_y {
        // Tray sits in the lower half (Windows taskbar) — anchor above it.
        (tray_y - popup_h as i32 - ANCHOR_GAP_PX).max(work_y)
    } else {
        // Tray sits in the upper half (macOS menu bar) — anchor below it.
        let (_, tray_h) = tray_size;
        let max_y = (work_y + work_h as i32 - popup_h as i32).max(work_y);
        (tray_y + tray_h as i32 + ANCHOR_GAP_PX).min(max_y)
    };

    (x, y)
}

fn anchor_near_tray(popup: &WebviewWindow, tray_rect: Rect) -> Result<(), String> {
    let monitor = popup
        .current_monitor()
        .map_err(|err| err.to_string())?
        .or(popup.primary_monitor().map_err(|err| err.to_string())?);
    let Some(monitor) = monitor else {
        // No monitor info available (e.g. a headless CI runner) — leave the
        // window wherever it last was rather than failing the toggle.
        return Ok(());
    };

    let scale = monitor.scale_factor();
    let tray_pos = tray_rect.position.to_physical::<i32>(scale);
    let tray_size = tray_rect.size.to_physical::<u32>(scale);
    let popup_size = popup.outer_size().map_err(|err| err.to_string())?;
    let work = monitor.work_area();

    let (x, y) = compute_anchor_position(
        (tray_pos.x, tray_pos.y),
        (tray_size.width, tray_size.height),
        (popup_size.width, popup_size.height),
        (
            (work.position.x, work.position.y),
            (work.size.width, work.size.height),
        ),
    );

    popup
        .set_position(PhysicalPosition::new(x, y))
        .map_err(|err| err.to_string())
}

/// Shows the ask window centered on the display under the cursor, without
/// taking keyboard focus (SPEC R-8.3): the window is declared
/// `"focus": false` + `"alwaysOnTop": true` in `tauri.conf.json` (the
/// `WS_EX_NOACTIVATE`-equivalent, non-activating-panel behaviour applied at
/// creation), and this function deliberately never calls `set_focus()` — the
/// user takes focus explicitly on first click/Tab, per spec.
pub fn show_ask_window(app: &AppHandle) -> Result<(), String> {
    let ask = ask_window(app)?;
    // Already up: don't re-center + re-show on every enqueue. A rapid burst of
    // asks otherwise thrashes WebView2 show()/center()/hide() churn (a native
    // fault risk), and re-centering would also yank the window out from under a
    // user who is mid-answer. The queue is FIFO and the window stays put (R-8.3);
    // new asks surface via the pushed state snapshot + the "N more waiting" badge.
    if ask.is_visible().unwrap_or(false) {
        return Ok(());
    }
    center_on_active_display(&ask)?;
    ask.show().map_err(|err| err.to_string())
}

/// Hides the ask window (after an answer, dismissal, or timeout).
pub fn hide_ask_window(app: &AppHandle) -> Result<(), String> {
    ask_window(app)?.hide().map_err(|err| err.to_string())
}

/// Pure geometry for [`center_on_active_display`]: centers a window of
/// `window_size` within `work_area` (both physical pixels).
fn compute_center_position(
    window_size: (u32, u32),
    work_area: ((i32, i32), (u32, u32)),
) -> (i32, i32) {
    let (win_w, win_h) = window_size;
    let ((work_x, work_y), (work_w, work_h)) = work_area;
    let x = work_x + (work_w as i32 - win_w as i32) / 2;
    let y = work_y + (work_h as i32 - win_h as i32) / 2;
    (x, y)
}

fn center_on_active_display(window: &WebviewWindow) -> Result<(), String> {
    // The cursor tells us which display the user is looking at, so the ask window
    // centers there. But `cursor_position()` (Win32 `GetCursorPos`) *errors* when
    // the process isn't attached to the active input desktop — a locked
    // workstation (Win+L / secure desktop), an RDP-disconnected session, or a
    // fast-user-switch. That is exactly the ask feature's target scenario (user
    // runs long agents, steps away, LOCKS the screen). A `?` here would make that
    // error fatal: `show_ask_window` would return `Err` before ever calling
    // `ask.show()`, so the always-on-top ask window (R-8.3) would never appear.
    // Treat a cursor error as "unknown display" and join the existing
    // current/primary fallback chain instead of propagating.
    let from_cursor = window
        .cursor_position()
        .ok()
        .and_then(|cursor| window.monitor_from_point(cursor.x, cursor.y).ok().flatten());
    let monitor = match from_cursor {
        Some(monitor) => Some(monitor),
        None => window
            .current_monitor()
            .map_err(|err| err.to_string())?
            .or(window.primary_monitor().map_err(|err| err.to_string())?),
    };
    let Some(monitor) = monitor else {
        return Ok(());
    };

    let size = window.outer_size().map_err(|err| err.to_string())?;
    let work = monitor.work_area();
    let (x, y) = compute_center_position(
        (size.width, size.height),
        (
            (work.position.x, work.position.y),
            (work.size.width, work.size.height),
        ),
    );

    window
        .set_position(PhysicalPosition::new(x, y))
        .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A 360x460 popup (SPEC R-7.1) on a 1920x1040 work area (1080p minus a
    // 40px taskbar), matching this machine's shape closely enough to catch
    // regressions.
    const WORK_AREA: ((i32, i32), (u32, u32)) = ((0, 0), (1920, 1040));
    const POPUP_SIZE: (u32, u32) = (360, 460);

    #[test]
    fn anchors_above_a_tray_icon_in_the_windows_taskbar() {
        // Tray icon near the bottom-right, as on Windows.
        let tray_pos = (1880, 1020);
        let tray_size = (16, 16);
        let (x, y) = compute_anchor_position(tray_pos, tray_size, POPUP_SIZE, WORK_AREA);
        assert!(y < tray_pos.1, "popup should sit above the tray icon");
        assert_eq!(y, tray_pos.1 - POPUP_SIZE.1 as i32 - ANCHOR_GAP_PX);
        // Horizontally centered on the tray icon, still on-screen.
        assert!(x >= 0 && x + POPUP_SIZE.0 as i32 <= WORK_AREA.1 .0 as i32);
    }

    #[test]
    fn anchors_below_a_tray_icon_in_the_macos_menu_bar() {
        let tray_pos = (1200, 4);
        let tray_size = (22, 22);
        let (_, y) = compute_anchor_position(tray_pos, tray_size, POPUP_SIZE, WORK_AREA);
        assert!(
            y > tray_pos.1,
            "popup should sit below a menu-bar tray icon"
        );
    }

    #[test]
    fn clamps_horizontally_so_the_popup_never_overflows_the_right_edge() {
        let tray_pos = (1918, 1020); // Right at the screen edge.
        let (x, _) = compute_anchor_position(tray_pos, (16, 16), POPUP_SIZE, WORK_AREA);
        assert!(x + POPUP_SIZE.0 as i32 <= WORK_AREA.1 .0 as i32);
    }

    #[test]
    fn clamps_horizontally_so_the_popup_never_overflows_the_left_edge() {
        let tray_pos = (2, 1020);
        let (x, _) = compute_anchor_position(tray_pos, (16, 16), POPUP_SIZE, WORK_AREA);
        assert!(x >= 0);
    }

    #[test]
    fn centers_within_the_work_area() {
        let (x, y) = compute_center_position((420, 260), WORK_AREA);
        assert_eq!(x, (1920 - 420) / 2);
        assert_eq!(y, (1040 - 260) / 2);
    }

    #[test]
    fn popup_height_grows_then_caps_at_560() {
        // SPEC R-7.1 "360×460 (max-height 560 then scroll)".
        assert_eq!(
            popup_target_height(120.0),
            POPUP_MIN_H,
            "short content → base 460"
        );
        assert_eq!(popup_target_height(500.0), 500.0, "grows with content");
        assert_eq!(
            popup_target_height(900.0),
            POPUP_MAX_H,
            "capped at 560, then scrolls"
        );
    }
}
