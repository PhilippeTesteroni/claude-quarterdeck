//! Window management: the frameless popup anchored to the tray with hide-on-blur
//! / Esc (SPEC R-7.1), and the always-on-top ask window that does not steal
//! keyboard focus on appear (SPEC R-8.3, `WS_EX_NOACTIVATE`-equivalent).
//!
//! Filled in by T3.

use tauri::{AppHandle, Manager, PhysicalPosition, Rect, WebviewWindow, WindowEvent};

/// Label of the popup window declared in `tauri.conf.json`.
pub const POPUP_LABEL: &str = "popup";
/// Label of the always-on-top ask window declared in `tauri.conf.json`.
pub const ASK_LABEL: &str = "ask";

/// Gap, in physical pixels, kept between the tray icon and the anchored
/// popup (SPEC R-7.1 "anchored to tray icon").
const ANCHOR_GAP_PX: i32 = 6;

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

    let on_blur = popup.clone();
    popup.on_window_event(move |event| {
        if let WindowEvent::Focused(false) = event {
            let _ = on_blur.hide();
        }
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
    let popup = popup_window(app)?;
    let visible = popup.is_visible().map_err(|err| err.to_string())?;
    if visible {
        return popup.hide().map_err(|err| err.to_string());
    }
    anchor_near_tray(&popup, tray_rect)?;
    popup.show().map_err(|err| err.to_string())?;
    popup.set_focus().map_err(|err| err.to_string())
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
    let cursor = window.cursor_position().map_err(|err| err.to_string())?;
    let monitor = window
        .monitor_from_point(cursor.x, cursor.y)
        .map_err(|err| err.to_string())?
        .or(window.current_monitor().map_err(|err| err.to_string())?)
        .or(window.primary_monitor().map_err(|err| err.to_string())?);
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
}
