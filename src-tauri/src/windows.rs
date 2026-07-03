//! Window management: the frameless popup anchored to the tray with hide-on-blur
//! / Esc (SPEC R-7.1), draggable + pin-on-top + true auto-height (SPEC R-14),
//! and the always-on-top ask window that does not steal keyboard focus on
//! appear (SPEC R-8.3, `WS_EX_NOACTIVATE`-equivalent) and is itself draggable
//! (SPEC R-18.2).
//!
//! Filled in by T3; extended for the v1.1 addendum (F1: §14, §18).

use std::sync::Mutex;

use tauri::{AppHandle, LogicalSize, Manager, PhysicalPosition, Rect, WebviewWindow, WindowEvent};

/// Label of the popup window declared in `tauri.conf.json`.
pub const POPUP_LABEL: &str = "popup";
/// Label of the always-on-top ask window declared in `tauri.conf.json`.
pub const ASK_LABEL: &str = "ask";

/// Gap, in physical pixels, kept between the tray icon and the anchored
/// popup (SPEC R-7.1 "anchored to tray icon").
const ANCHOR_GAP_PX: i32 = 6;

/// Popup logical width and the min/cap heights (SPEC R-14.3: "true
/// auto-height ... clamp(header + watchline + rows + footer, 160, 560)"): the
/// v1.0 460 floor is removed — an empty popup may be as compact as 160 — and
/// it still grows with content up to the 560 cap, beyond which the content
/// area scrolls (R-7.1).
const POPUP_W: f64 = 360.0;
const POPUP_MIN_H: f64 = 160.0;
const POPUP_MAX_H: f64 = 560.0;

/// The tray-icon rect the popup was last anchored to. Kept so a content-driven
/// [`resize_popup_to_content`] can re-anchor the (possibly visible) popup
/// without waiting for a fresh tray click — otherwise growing the window would
/// extend it over the taskbar/tray instead of away from it.
static LAST_TRAY_RECT: Mutex<Option<Rect>> = Mutex::new(None);

/// SPEC R-14.2 pin-on-top state, mirrored from `settings.json`'s
/// `popupPinned` by [`set_popup_pinned`]. Read by the hide-on-blur handler and
/// by [`toggle_popup`]/[`open_popup`] (a pinned popup does not re-anchor to
/// the tray on open — it stays wherever the user left it).
static POPUP_PINNED: Mutex<bool> = Mutex::new(false);

/// SPEC R-14.1/R-14.3: whether the user has manually dragged the popup since
/// it was last anchored to the tray. While true, a content-driven resize must
/// keep the window's top edge fixed and never re-anchor to the tray — see
/// [`should_reanchor_on_resize`]. Reset to `false` whenever the popup is
/// freshly anchored (a fresh open re-establishes "not moved").
static POPUP_USER_MOVED: Mutex<bool> = Mutex::new(false);

/// Count of position changes `windows.rs` itself is about to make (via
/// [`anchor_near_tray`]) that haven't yet been observed as a `Moved` window
/// event. Lets the popup's `Moved` handler tell a genuine user drag (R-14.1)
/// apart from our own re-anchoring, without which every programmatic
/// `set_position` would be mistaken for a user move and permanently disable
/// tray-following.
static PENDING_PROGRAMMATIC_MOVES: Mutex<u32> = Mutex::new(0);

fn remember_tray_rect(rect: Rect) {
    if let Ok(mut guard) = LAST_TRAY_RECT.lock() {
        *guard = Some(rect);
    }
}

fn last_tray_rect() -> Option<Rect> {
    LAST_TRAY_RECT.lock().ok().and_then(|guard| *guard)
}

fn set_popup_pinned_flag(pinned: bool) {
    if let Ok(mut guard) = POPUP_PINNED.lock() {
        *guard = pinned;
    }
}

fn popup_pinned() -> bool {
    POPUP_PINNED.lock().map(|g| *g).unwrap_or(false)
}

fn set_popup_user_moved(moved: bool) {
    if let Ok(mut guard) = POPUP_USER_MOVED.lock() {
        *guard = moved;
    }
}

fn popup_user_moved() -> bool {
    POPUP_USER_MOVED.lock().map(|g| *g).unwrap_or(false)
}

/// Called immediately before every programmatic `set_position` on the popup
/// (i.e. inside [`anchor_near_tray`]) so the next `Moved` event is attributed
/// to us, not the user.
fn note_programmatic_move() {
    if let Ok(mut n) = PENDING_PROGRAMMATIC_MOVES.lock() {
        *n += 1;
    }
}

/// Pure decision for the popup's `Moved` window event: does it represent a
/// genuine user drag (SPEC R-14.1), or one of our own anchor repositions?
/// `pending` is the outstanding count of moves we ourselves initiated; a
/// pending programmatic move is consumed (decremented) and reported as NOT a
/// user move, otherwise the observed move is the user's own drag. Kept pure
/// (no static access) so it's unit-testable without a live window.
fn is_user_initiated_move(pending: &mut u32) -> bool {
    if *pending > 0 {
        *pending -= 1;
        false
    } else {
        true
    }
}

/// Called from the popup's `WindowEvent::Moved` handler.
fn note_move_event() {
    let mut pending = PENDING_PROGRAMMATIC_MOVES
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if is_user_initiated_move(&mut pending) {
        drop(pending);
        set_popup_user_moved(true);
    }
}

/// Pure clamp for the popup's grow-then-scroll band (SPEC R-14.3): content
/// shorter than the 160 floor keeps that floor (the v1.0 460 floor is
/// removed — an empty popup may be compact); taller content grows up to 560,
/// beyond which the window stays at 560 and the content area scrolls.
fn popup_target_height(content_px: f64) -> f64 {
    content_px.clamp(POPUP_MIN_H, POPUP_MAX_H)
}

/// SPEC R-14.2: whether the popup should hide when it loses focus. Pinned
/// disables hide-on-blur entirely (the window stays until unpinned/Esc/tray
/// click); unpinned keeps the v1.0 behavior. Pure so it's unit-testable
/// without a live window.
fn should_hide_on_blur(pinned: bool) -> bool {
    !pinned
}

/// SPEC R-14.2: whether opening the popup (tray click / toast click) should
/// re-anchor it near the tray. Pinned popups stay wherever the user left them
/// ("Unpinned → v1.0 behavior: anchor to tray on open").
fn should_anchor_on_open(pinned: bool) -> bool {
    !pinned
}

/// SPEC R-14.3: whether a content-driven resize should re-anchor the popup
/// (so it grows away from the tray edge). Skipped when pinned (never
/// tray-anchored while pinned) or when the user has manually moved the
/// window (growth then keeps the TOP edge fixed instead, since `set_size`
/// alone never touches the window's position).
fn should_reanchor_on_resize(pinned: bool, user_moved: bool) -> bool {
    !pinned && !user_moved
}

fn popup_window(app: &AppHandle) -> Result<WebviewWindow, String> {
    if let Some(popup) = app.get_webview_window(POPUP_LABEL) {
        return Ok(popup);
    }
    rebuild_window(app, POPUP_LABEL)
}

fn ask_window(app: &AppHandle) -> Result<WebviewWindow, String> {
    if let Some(ask) = app.get_webview_window(ASK_LABEL) {
        return Ok(ask);
    }
    rebuild_window(app, ASK_LABEL)
}

/// Recreate a declaratively-configured window (`popup`/`ask`) that the Tauri
/// runtime failed to create at startup (SPEC R-3.6). The two windows are
/// declared in `tauri.conf.json` and built once by the runtime before
/// `setup()` runs; a transient WebView2 failure there (e.g. `ERROR_BUSY` /
/// "The requested resource is in use" while the per-app user-data folder is
/// briefly locked by AV/EDR, disk pressure, or process contention) would
/// otherwise leave the window missing for the whole session, so every tray
/// click / toast / ask surfacing silently no-ops. Rebuilding from the same
/// config on the next access retries creation once the contention has passed,
/// so the popup/ask window heals instead of staying permanently dead.
///
/// Must be called on the main thread (window creation requirement); all
/// accessor callers already route through the tray/main thread or `run_on_main`.
fn rebuild_window(app: &AppHandle, label: &str) -> Result<WebviewWindow, String> {
    let config = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == label)
        .cloned()
        .ok_or_else(|| format!("no declarative window config for `{label}`"))?;
    tracing::warn!(
        label,
        "webview window absent (startup creation likely failed); rebuilding from config (R-3.6)"
    );
    let window = tauri::WebviewWindowBuilder::from_config(app, &config)
        .map_err(|err| err.to_string())?
        .build()
        .map_err(|err| err.to_string())?;
    // A rebuilt popup must re-arm its hide-on-blur / Esc / close-guard listeners
    // (the ask window has no such behavior to re-attach).
    if label == POPUP_LABEL {
        attach_popup_behavior(&window)?;
    }
    Ok(window)
}

/// Registers hide-on-blur and hide-on-Escape for the popup (SPEC R-7.1). Call
/// once at startup (composed by T7); the popup is created once and only
/// hidden/shown afterwards (SPEC R-3.6), so both listeners stay attached for
/// the app's lifetime — no need to re-register on every show. If the runtime
/// failed to create the popup at startup, this rebuilds it (which re-arms the
/// same behavior), so the tray isn't left permanently dead.
pub fn setup_popup_behavior(app: &AppHandle) -> Result<(), String> {
    match app.get_webview_window(POPUP_LABEL) {
        Some(popup) => attach_popup_behavior(&popup),
        None => rebuild_window(app, POPUP_LABEL).map(|_| ()),
    }
}

/// Attach the popup's hide-on-blur / Esc-to-hide / close-guard listeners. Split
/// out of [`setup_popup_behavior`] so [`rebuild_window`] can re-arm them on a
/// popup recreated after a startup WebView2 failure (R-3.6).
fn attach_popup_behavior(popup: &WebviewWindow) -> Result<(), String> {
    let on_event = popup.clone();
    popup.on_window_event(move |event| match event {
        // SPEC R-14.2: pinned disables hide-on-blur entirely.
        WindowEvent::Focused(false) => {
            if should_hide_on_blur(popup_pinned()) {
                let _ = on_event.hide();
            }
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
        // SPEC R-14.1/R-14.3: track user drags so a content-driven resize
        // knows whether to keep re-anchoring near the tray or leave the
        // window where the user put it.
        WindowEvent::Moved(_) => {
            note_move_event();
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

/// Applies the SPEC R-14.2 pin-on-top toggle: persists the flag (read by the
/// hide-on-blur handler and the open/resize anchoring above), sets/clears
/// native always-on-top, and gives the caller a visual state to reflect
/// (`ui/src/popup.ts` reads this back from `SettingsState.popupPinned`, R-3.4
/// keeps the decision itself in Rust). Invoked by `apply_setting_side_effect`
/// when the `popupPinned` setting changes.
pub fn set_popup_pinned(app: &AppHandle, pinned: bool) -> Result<(), String> {
    set_popup_pinned_flag(pinned);
    let popup = popup_window(app)?;
    popup
        .set_always_on_top(pinned)
        .map_err(|err| err.to_string())
}

/// Positions the popup near the tray icon and shows/focuses it, or hides it
/// if it's already visible (SPEC R-7.1: click toggles). SPEC R-14.2: while
/// pinned, opening does NOT re-anchor near the tray — the popup stays wherever
/// the user left it; an explicit tray click still toggles visibility either
/// way.
pub fn toggle_popup(app: &AppHandle, tray_rect: Rect) -> Result<(), String> {
    remember_tray_rect(tray_rect);
    let popup = popup_window(app)?;
    let visible = popup.is_visible().map_err(|err| err.to_string())?;
    if visible {
        return popup.hide().map_err(|err| err.to_string());
    }
    if should_anchor_on_open(popup_pinned()) {
        anchor_near_tray(&popup, tray_rect)?;
        set_popup_user_moved(false);
    }
    popup.show().map_err(|err| err.to_string())?;
    popup.set_focus().map_err(|err| err.to_string())
}

/// Shows and focuses the popup near the tray icon (used by toast-click routing,
/// SPEC R-9.6). Unlike [`toggle_popup`] it never hides an already-open popup —
/// a toast click should always bring the deck forward. SPEC R-14.2: skips the
/// re-anchor while pinned.
pub fn open_popup(app: &AppHandle) -> Result<(), String> {
    let popup = popup_window(app)?;
    if should_anchor_on_open(popup_pinned()) {
        if let Some(rect) = last_tray_rect() {
            let _ = anchor_near_tray(&popup, rect);
            set_popup_user_moved(false);
        }
    }
    popup.show().map_err(|err| err.to_string())?;
    popup.set_focus().map_err(|err| err.to_string())
}

/// Resizes the popup to fit `content_px` logical pixels of content, clamped to
/// the 160..=560 band (SPEC R-14.3 true auto-height, grow-then-scroll floor
/// lowered from the v1.0 460). A visible, unpinned, not-user-moved popup is
/// re-anchored so it grows away from the tray edge rather than over it; once
/// the user has manually dragged the window (R-14.1), or while pinned, the
/// re-anchor is skipped — `set_size` alone never moves the window's top-left,
/// so the top edge stays fixed and the window simply grows/shrinks downward
/// (R-14.3). The frontend measures its own content height and drives this via
/// the `resize_popup` command (R-3.4: logic stays in Rust, the view just
/// reports a number).
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
    if popup.is_visible().unwrap_or(false)
        && should_reanchor_on_resize(popup_pinned(), popup_user_moved())
    {
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

    // R-14.1/R-14.3: mark this reposition as ours before it fires, so the
    // popup's `Moved` handler doesn't mistake it for a user drag.
    note_programmatic_move();
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
        // SPEC R-14.3 "true auto-height ... clamp(..., 160, 560)": the v1.0
        // 460 floor is removed.
        assert_eq!(
            popup_target_height(60.0),
            POPUP_MIN_H,
            "short/empty content → the 160 floor, not the removed 460 one"
        );
        assert_eq!(popup_target_height(500.0), 500.0, "grows with content");
        assert_eq!(
            popup_target_height(900.0),
            POPUP_MAX_H,
            "capped at 560, then scrolls"
        );
    }

    #[test]
    fn popup_height_shrinks_back_when_rows_disappear() {
        // SPEC R-14.3 regression: "height shrinks back when rows disappear
        // (regression-tested: 50 rows → 0)". 50 rows of content (well past the
        // scroll cap) must clamp to 560, and once content collapses back to a
        // near-empty popup the target height must shrink right back down to
        // the (now-lower) 160 floor — never "stick" at the grown size.
        let fifty_rows = 40.0 * 50.0 + 90.0; // ~header+watchline+footer + 50 rows
        assert_eq!(
            popup_target_height(fifty_rows),
            POPUP_MAX_H,
            "50 rows caps at 560"
        );
        let zero_rows = 90.0; // header + watchline + footer only, no rows
        assert_eq!(
            popup_target_height(zero_rows),
            POPUP_MIN_H,
            "back to 0 rows shrinks to the 160 floor, not stuck at 560"
        );
    }

    #[test]
    fn hide_on_blur_is_disabled_only_while_pinned() {
        // SPEC R-14.2.
        assert!(should_hide_on_blur(false), "unpinned keeps v1.0 behavior");
        assert!(!should_hide_on_blur(true), "pinned disables hide-on-blur");
    }

    #[test]
    fn anchor_on_open_is_skipped_only_while_pinned() {
        // SPEC R-14.2 "Unpinned → v1.0 behavior (anchor to tray on open)".
        assert!(should_anchor_on_open(false));
        assert!(!should_anchor_on_open(true));
    }

    #[test]
    fn reanchor_on_resize_requires_unpinned_and_not_user_moved() {
        // SPEC R-14.3: re-anchoring on a content-driven resize only happens
        // when the popup is neither pinned nor manually moved by the user;
        // either condition alone is enough to skip it (grow-in-place instead).
        assert!(should_reanchor_on_resize(false, false));
        assert!(
            !should_reanchor_on_resize(true, false),
            "pinned skips re-anchor"
        );
        assert!(
            !should_reanchor_on_resize(false, true),
            "user-moved skips re-anchor (R-14.1 top-edge-fixed growth)"
        );
        assert!(!should_reanchor_on_resize(true, true));
    }

    #[test]
    fn programmatic_moves_are_not_mistaken_for_a_user_drag() {
        // SPEC R-14.1/R-14.3: `anchor_near_tray` marks its own reposition
        // before it fires; the resulting `Moved` event must be consumed as
        // "ours", not flagged as a user drag.
        let mut pending = 0u32;
        assert!(
            is_user_initiated_move(&mut pending),
            "with nothing pending, an observed move is the user's own drag"
        );

        pending = 1;
        assert!(
            !is_user_initiated_move(&mut pending),
            "a pending programmatic move consumes the flag instead of flagging a user drag"
        );
        assert_eq!(pending, 0, "the pending count is decremented once consumed");

        // A burst of N programmatic moves is consumed one at a time; only a
        // move beyond that count is the user's.
        pending = 2;
        assert!(!is_user_initiated_move(&mut pending));
        assert!(!is_user_initiated_move(&mut pending));
        assert!(is_user_initiated_move(&mut pending));
    }
}
