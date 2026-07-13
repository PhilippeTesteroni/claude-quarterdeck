//! Window management: the frameless popup anchored to the tray with hide-on-blur
//! / Esc (SPEC R-7.1), draggable + pin-on-top + true auto-height (SPEC R-14),
//! the always-on-top ask window that does not steal keyboard focus on
//! appear (SPEC R-8.3, `WS_EX_NOACTIVATE`-equivalent) and is itself draggable
//! (SPEC R-18.2), and the pinned popup's lamp mode: a fixed ~56x56 always-on-top
//! traffic-light square with mode/position persistence (SPEC §25).
//!
//! Filled in by T3; extended for the v1.1 addendum (F1: §14, §18) and the v1.2
//! lamp mode (F7: §25).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{
    AppHandle, LogicalPosition, LogicalSize, Manager, PhysicalPosition, Rect, WebviewWindow,
    WindowEvent,
};

use crate::settings::PopupMode;

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

/// Lamp mode's fixed square size, logical px (SPEC R-25.1 "~56x56 logical px").
const LAMP_SIZE: f64 = 56.0;

/// Ask-window logical width and the min/cap heights (SPEC §35.2 auto-size): the
/// always-on-top ask window sizes to its content — a short perm/question is as
/// compact as the 140 floor, a large §29 form (or a long perm input) grows up to
/// the 640 cap, beyond which the content area scrolls. Mirrors the popup's
/// R-14.3 grow-then-scroll band; width stays at the declared 420 (`tauri.conf.json`).
const ASK_W: f64 = 420.0;
const ASK_MIN_H: f64 = 140.0;
const ASK_MAX_H: f64 = 640.0;

/// Minimum gap between `popupPos` persist writes while the user drags the
/// pinned popup (SPEC R-25.2). A drag fires many `Moved` events per second;
/// writing `settings.json` on every one would hammer disk for no benefit — only
/// the settled position matters for restoring geometry across a restart, and a
/// drag that ends within this gap of the last write still has its FINAL
/// position captured by the very next `Moved` event once the gap has elapsed
/// (a real drag keeps firing `Moved` for as long as the mouse keeps moving).
const POPUP_POS_PERSIST_GAP: Duration = Duration::from_millis(250);

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

/// SPEC R-25.2: mirrors `settings.json`'s `popupMode`. Read by
/// [`resize_popup_to_content`] (a lamp is a fixed square, not content-sized)
/// and by the popup's `Moved` handler (position is only worth persisting while
/// pinned, independent of mode, but kept alongside `POPUP_PINNED` since both
/// gate the same window-geometry decisions).
static POPUP_MODE: Mutex<PopupMode> = Mutex::new(PopupMode::List);

/// Last time [`maybe_persist_popup_pos`] actually wrote to disk (SPEC R-25.2
/// debounce, see [`POPUP_POS_PERSIST_GAP`]).
static LAST_POPUP_POS_PERSIST: Mutex<Option<Instant>> = Mutex::new(None);

fn set_popup_mode_flag(mode: PopupMode) {
    if let Ok(mut guard) = POPUP_MODE.lock() {
        *guard = mode;
    }
}

fn popup_mode() -> PopupMode {
    POPUP_MODE.lock().map(|g| *g).unwrap_or_default()
}

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

/// Called from the popup's `WindowEvent::Moved` handler with the window (to
/// read its scale factor for the logical-position persist) and the raw
/// physical position the OS reported.
fn note_move_event(popup: &WebviewWindow, position: PhysicalPosition<i32>) {
    let mut pending = PENDING_PROGRAMMATIC_MOVES
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let user_initiated = is_user_initiated_move(&mut pending);
    drop(pending);
    if !user_initiated {
        return;
    }
    set_popup_user_moved(true);
    // R-25.2: position is only worth persisting while pinned — an unpinned
    // popup always re-anchors to the tray on open (R-14.2), so its position is
    // never meaningful to restore across a restart.
    if popup_pinned() {
        maybe_persist_popup_pos(popup, position);
    }
}

/// Debounced disk-persist of the popup's current logical position (SPEC
/// R-25.2), called on every genuine user drag while pinned. See
/// [`POPUP_POS_PERSIST_GAP`]/[`should_persist_popup_pos`] for the debounce.
fn maybe_persist_popup_pos(popup: &WebviewWindow, position: PhysicalPosition<i32>) {
    let now = Instant::now();
    let should_persist = {
        let mut guard = LAST_POPUP_POS_PERSIST
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let persist = should_persist_popup_pos(*guard, now, POPUP_POS_PERSIST_GAP);
        if persist {
            *guard = Some(now);
        }
        persist
    };
    if !should_persist {
        return;
    }
    let Ok(scale) = popup.scale_factor() else {
        return;
    };
    let logical = position.to_logical::<f64>(scale);
    let pos = crate::settings::PopupPos {
        x: logical.x,
        y: logical.y,
    };
    let value = crate::ipc::SettingValue::Text(pos.to_setting_string());
    // §48: persist under the CURRENT mode's own key so the lamp and the list
    // keep independent remembered positions — a list resize/clamp writing its
    // settled spot must never corrupt where the lamp returns to.
    let key = popup_pos_setting_key(popup_mode());
    if let Err(err) = crate::settings::set_setting(&crate::settings::data_dir(), key, value) {
        tracing::warn!(error = %err, key, "failed to persist popup position (R-25.2/§48)");
    }
}

/// Pure clamp for the popup's grow-then-scroll band (SPEC R-14.3): content
/// shorter than the 160 floor keeps that floor (the v1.0 460 floor is
/// removed — an empty popup may be compact); taller content grows up to 560,
/// beyond which the window stays at 560 and the content area scrolls.
fn popup_target_height(content_px: f64) -> f64 {
    content_px.clamp(POPUP_MIN_H, POPUP_MAX_H)
}

/// Pure clamp for the ask window's grow-then-scroll band (SPEC §35.2 auto-size):
/// content shorter than the 140 floor keeps that floor (a compact perm/question),
/// taller content grows up to 640, beyond which the window stays at 640 and the
/// content area scrolls. Mirror of [`popup_target_height`] for the ask window.
fn ask_target_height(content_px: f64) -> f64 {
    content_px.clamp(ASK_MIN_H, ASK_MAX_H)
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

/// SPEC R-25.1: a lamp is a fixed ~56x56 square, not content-sized — a
/// content-driven resize report (from the list layout's measurement) must be
/// ignored while collapsed, or it would fight the fixed size `set_popup_mode`
/// just applied. Kept pure (mode as a plain arg) so it's unit-testable without
/// a live window.
fn should_skip_content_resize(mode: PopupMode) -> bool {
    mode == PopupMode::Lamp
}

/// SPEC R-25.2 "Unpin while in lamp mode → expand to list + revert to v1.0
/// tray-anchored behavior": whether an unpin should ALSO force the popup back
/// to list mode. The collapse button that reaches lamp mode is only shown
/// while pinned (R-25.2), so unpinning is the only way out of lamp mode that
/// doesn't already go through an explicit expand click.
pub fn should_force_list_on_unpin(pinned: bool, mode: PopupMode) -> bool {
    !pinned && mode == PopupMode::Lamp
}

/// The (min, max) logical size band to apply for a given popup mode (SPEC
/// R-25.1/R-25.2): a lamp is pinned to an exact 56x56 square; list restores the
/// v1.0/R-14.3 360-wide, 160..=560-tall band (the actual height within that
/// band is then set by the next content-driven [`resize_popup_to_content`]).
/// Kept pure so the ordering logic in [`set_popup_mode`] is unit-testable
/// without a live window.
fn popup_size_band(mode: PopupMode) -> ((f64, f64), (f64, f64)) {
    match mode {
        PopupMode::Lamp => ((LAMP_SIZE, LAMP_SIZE), (LAMP_SIZE, LAMP_SIZE)),
        PopupMode::List => ((POPUP_W, POPUP_MIN_H), (POPUP_W, POPUP_MAX_H)),
    }
}

/// Whether the new MIN size must be applied before the new MAX (SPEC R-25.2
/// lamp<->list transitions). Applying them in the wrong order can ask the OS
/// for a transient `min > max` window state — shrinking (new max smaller than
/// the current one) must lower min first; growing (new max larger) must raise
/// max first. Kept pure so it's unit-testable without a live window.
fn min_before_max(current_max_w: f64, new_max_w: f64) -> bool {
    new_max_w <= current_max_w
}

/// SPEC R-25.2 debounce decision for [`maybe_persist_popup_pos`]: has enough
/// time passed since the last `popupPos` disk write? Kept pure (plain
/// `Instant`s, no static access) so it's unit-testable without a live window or
/// real sleeping.
fn should_persist_popup_pos(last: Option<Instant>, now: Instant, gap: Duration) -> bool {
    match last {
        None => true,
        Some(t) => now.duration_since(t) >= gap,
    }
}

/// SPEC §48: the persisted-position settings key for a given popup mode. The
/// lamp and the list each remember their own spot, so expanding the list (whose
/// §39 clamp / content resize can nudge it, then persist the moved spot) never
/// corrupts where the lamp returns to on collapse. `Lamp` -> `lampPos`, `List`
/// -> `popupPos` (the back-compat v1.0/v1.2 list key). Kept pure so the
/// mode->field selection is unit-testable without a live window.
fn popup_pos_setting_key(mode: PopupMode) -> &'static str {
    match mode {
        PopupMode::Lamp => "lampPos",
        PopupMode::List => "popupPos",
    }
}

/// SPEC §48: the saved position to restore when switching INTO `mode` — the
/// list's own remembered spot (`popup_pos`) for list, the lamp's (`lamp_pos`)
/// for lamp. `None` means "no saved position for that mode yet", in which case
/// the caller keeps the current grow/anchor behavior instead of snapping to a
/// stale coordinate. Kept pure so the selection is unit-testable without disk.
pub(crate) fn saved_pos_for_mode(
    mode: PopupMode,
    list_pos: Option<crate::settings::PopupPos>,
    lamp_pos: Option<crate::settings::PopupPos>,
) -> Option<crate::settings::PopupPos> {
    match mode {
        PopupMode::Lamp => lamp_pos,
        PopupMode::List => list_pos,
    }
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
        // window where the user put it. SPEC R-25.2: a genuine user drag while
        // pinned also persists the position (debounced) for a restart to
        // restore.
        WindowEvent::Moved(position) => {
            note_move_event(&on_event, *position);
        }
        _ => {}
    });

    // Esc-to-hide: injected via the global Tauri JS API
    // (`app.withGlobalTauri = true` in tauri.conf.json) so this module alone
    // satisfies the AC without touching `ui/**` (T4-owned). T4's own
    // popup.ts may add the same handler directly for defense in depth; both
    // are idempotent (hiding an already-hidden window is a no-op).
    //
    // SPEC R-25.3 "Blur/Esc never hide the lamp (it is the point of
    // pinning)": `window.__qdPopupMode` is a plain global `popup.ts` keeps in
    // sync with `SettingsState.popupMode` on every render (there's no Rust
    // state to read from inside this injected string, R-3.4's "logic in Rust"
    // is honored by Rust owning the actual `popup_mode()` truth — this is only
    // a display mirror of a decision already made server-side).
    popup
        .eval(
            "window.addEventListener('keydown', function (event) { \
                if (event.key === 'Escape' && window.__qdPopupMode !== 'lamp' && window.__TAURI__ && window.__TAURI__.window) { \
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

/// Applies the SPEC R-25.2 lamp/list mode switch: persists the flag (read by
/// [`resize_popup_to_content`]) and re-applies the window's min/max size
/// constraints before setting an exact size for lamp (a fixed 56x56 square,
/// R-25.1) or leaving list's actual height to the next content-driven
/// [`resize_popup_to_content`] call (the frontend re-renders and re-measures
/// on the very same state push that carries this mode change).
///
/// Min/max are updated in the order [`min_before_max`] says is safe: the
/// window's own `min <= max` invariant would otherwise be violated for one
/// native call in between (Tauri/the OS clamps or rejects that transient
/// state depending on the backend), so shrinking to the lamp lowers min
/// first, growing back to the list raises max first.
pub fn set_popup_mode(app: &AppHandle, mode: PopupMode) -> Result<(), String> {
    let popup = popup_window(app)?;
    // Tauri's `WebviewWindow` exposes only setters for min/max size, not a
    // getter — track the previous band via our own mirrored `POPUP_MODE`
    // static (read before overwriting it) instead of querying the live window.
    let (_, (previous_max_w, _)) = popup_size_band(popup_mode());
    set_popup_mode_flag(mode);

    let ((min_w, min_h), (max_w, max_h)) = popup_size_band(mode);
    let min = LogicalSize::new(min_w, min_h);
    let max = LogicalSize::new(max_w, max_h);
    if min_before_max(previous_max_w, max_w) {
        popup.set_min_size(Some(min)).map_err(|e| e.to_string())?;
        popup.set_max_size(Some(max)).map_err(|e| e.to_string())?;
    } else {
        popup.set_max_size(Some(max)).map_err(|e| e.to_string())?;
        popup.set_min_size(Some(min)).map_err(|e| e.to_string())?;
    }

    if mode == PopupMode::Lamp {
        // Fixed size, no content to measure — apply it immediately rather than
        // waiting on a `resize_popup` report the (now-hidden) list layout will
        // never send.
        popup.set_size(min).map_err(|e| e.to_string())?;
    }
    // §48: each mode remembers its own position — restore the target mode's
    // saved spot (list -> `popup_pos`, lamp -> `lamp_pos`) so an expand→collapse
    // round trip returns the lamp exactly where the user left it instead of
    // dragging it onto the list's (resize/clamp-nudged) coordinates. Marked
    // programmatic so the resulting `Moved` event isn't mistaken for a fresh
    // user drag (R-14.1). A mode with no saved position yet is left where it is
    // (grow/anchor as before). The §39 clamp below still runs, so a restored
    // spot that is now off-screen (a monitor arrangement change) is pulled back
    // on.
    let settings = crate::settings::load(&crate::settings::data_dir());
    if let Some(pos) = saved_pos_for_mode(mode, settings.popup_pos, settings.lamp_pos) {
        note_programmatic_move();
        popup
            .set_position(LogicalPosition::new(pos.x, pos.y))
            .map_err(|e| e.to_string())?;
    }
    // §39: keep the window on-screen across the transition. The lamp<->list
    // switch changes the size band from a fixed top-left; the list's full
    // height lands via the next `resize_popup_to_content` (which re-clamps
    // too), but clamp here so the lamp square itself never straddles an edge.
    clamp_popup_onto_screen(&popup)?;
    Ok(())
}

/// Restores a persisted popup position at startup (SPEC R-25.2), for a pinned
/// (and possibly collapsed) popup that was manually positioned before the app
/// last quit. Best-effort: swallows a missing window rather than failing
/// startup over a cosmetic restore. Marks the position as user-moved so the
/// first content-driven resize (R-14.3) grows in place instead of snapping
/// back to the tray, matching what a live drag would have done.
pub fn restore_popup_position(app: &AppHandle, pos: crate::settings::PopupPos) {
    if let Ok(popup) = popup_window(app) {
        let _ = popup.set_position(LogicalPosition::new(pos.x, pos.y));
        set_popup_user_moved(true);
        // §39: a position persisted under a different monitor arrangement (a
        // display unplugged / resolution changed since the last quit) may now
        // be off-screen — clamp it back onto the current work area.
        let _ = clamp_popup_onto_screen(&popup);
    }
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
    // R-25.1: a lamp is a fixed-size square; `set_popup_mode` already applied
    // its exact size, so a stale/late content-height report from the (hidden)
    // list layout must not fight it.
    if should_skip_content_resize(popup_mode()) {
        return Ok(());
    }
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
    } else {
        // §39: growing in place (pinned or user-moved) keeps the top-left
        // fixed — `anchor_near_tray` isn't producing a fresh on-screen
        // position here — so a window that grew near a screen edge (a lamp
        // expanding back to the full list, R-25.2) can spill off the work
        // area. Re-clamp its top-left back on-screen.
        clamp_popup_onto_screen(&popup)?;
    }
    Ok(())
}

/// Resizes the always-on-top ask window to fit `content_px` logical pixels of
/// content, clamped to the 140..=640 band (SPEC §35.2 auto-size — mirrors the
/// popup's R-14.3 grow-then-scroll). Height-only: `set_size` never touches the
/// window's top-left, so it grows/shrinks downward while its top edge — and its
/// centered-on-appear position (R-8.3) — stay put, never yanked out from under a
/// user who is mid-answer; always-on-top is a window flag a resize leaves alone.
/// The frontend measures its own content height and drives this via the
/// `resize_ask` command (R-3.4: the sizing logic stays in Rust, the view just
/// reports a number).
pub fn resize_ask_to_content(app: &AppHandle, content_px: f64) -> Result<(), String> {
    let ask = ask_window(app)?;
    let target_h = ask_target_height(content_px);
    let scale = ask.scale_factor().map_err(|err| err.to_string())?;
    let current_h = ask.inner_size().map_err(|err| err.to_string())?.height as f64 / scale;
    // Avoid churn: only resize when the height meaningfully changes.
    if (current_h - target_h).abs() < 1.0 {
        return Ok(());
    }
    ask.set_size(LogicalSize::new(ASK_W, target_h))
        .map_err(|err| err.to_string())
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

/// Pure geometry (SPEC §39): clamp a window's top-left so the whole window
/// stays inside the monitor work area. Mirrors the horizontal/vertical clamp in
/// [`compute_anchor_position`] but for an arbitrary window rect — used when a
/// mode restore / resize grows the popup from a fixed top-left (a lamp collapsed
/// near a screen edge expanding back to the full list, R-25.2), which would
/// otherwise push the bottom/right edge off-screen. The `.max(work_*)` floors
/// keep the top-left on-screen even for a window larger than the work area
/// (bottom/right may still overflow, but the origin stays reachable). All
/// physical pixels; kept side-effect free so it's unit-testable without a live
/// window.
fn clamp_rect_onto_work_area(
    pos: (i32, i32),
    size: (u32, u32),
    work_area: ((i32, i32), (u32, u32)),
) -> (i32, i32) {
    let (x, y) = pos;
    let (w, h) = size;
    let ((work_x, work_y), (work_w, work_h)) = work_area;

    let max_x = (work_x + work_w as i32 - w as i32).max(work_x);
    let max_y = (work_y + work_h as i32 - h as i32).max(work_y);
    (x.clamp(work_x, max_x), y.clamp(work_y, max_y))
}

/// SPEC §39: keep the popup fully on-screen after a mode restore / `set_size` /
/// `set_popup_mode` transition. `set_size` and the lamp<->list mode switch grow
/// the window from its fixed top-left, so a lamp collapsed near a screen edge
/// (R-25.2) would restore partly/fully off the work area when expanded back to
/// the list; this re-clamps the top-left back on. Best-effort: a missing monitor
/// (e.g. a headless CI runner) leaves the window where it is rather than failing
/// the transition, matching [`anchor_near_tray`]'s fallback.
fn clamp_popup_onto_screen(popup: &WebviewWindow) -> Result<(), String> {
    let monitor = popup
        .current_monitor()
        .map_err(|err| err.to_string())?
        .or(popup.primary_monitor().map_err(|err| err.to_string())?);
    let Some(monitor) = monitor else {
        return Ok(());
    };

    let pos = popup.outer_position().map_err(|err| err.to_string())?;
    let size = popup.outer_size().map_err(|err| err.to_string())?;
    let work = monitor.work_area();

    let (x, y) = clamp_rect_onto_work_area(
        (pos.x, pos.y),
        (size.width, size.height),
        (
            (work.position.x, work.position.y),
            (work.size.width, work.size.height),
        ),
    );

    if (x, y) != (pos.x, pos.y) {
        // R-14.1/R-14.3: this reposition is ours, not a user drag — mark it so
        // the popup's `Moved` handler doesn't disable tray-following.
        note_programmatic_move();
        popup
            .set_position(PhysicalPosition::new(x, y))
            .map_err(|err| err.to_string())?;
    }
    Ok(())
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
    fn clamp_leaves_an_already_on_screen_window_untouched() {
        // SPEC §39: a window comfortably inside the work area is not nudged.
        let pos = (200, 150);
        assert_eq!(
            clamp_rect_onto_work_area(pos, POPUP_SIZE, WORK_AREA),
            pos,
            "an on-screen rect must be returned unchanged"
        );
    }

    #[test]
    fn clamp_pulls_a_bottom_right_overflow_back_on_screen() {
        // SPEC §39: the exact regression — a lamp pinned near the bottom-right
        // corner expands back to the full 360x460 list, which from that
        // top-left would spill off the right and bottom edges. Clamp must pull
        // the top-left up/left so the whole window fits.
        let pos = (1900, 1030); // a ~56px lamp's origin, hard against the corner
        let (x, y) = clamp_rect_onto_work_area(pos, POPUP_SIZE, WORK_AREA);
        assert!(
            x + POPUP_SIZE.0 as i32 <= WORK_AREA.1 .0 as i32,
            "right edge must be on-screen"
        );
        assert!(
            y + POPUP_SIZE.1 as i32 <= WORK_AREA.1 .1 as i32,
            "bottom edge must be on-screen"
        );
        assert_eq!(x, 1920 - 360, "flush to the right work-area edge");
        assert_eq!(y, 1040 - 460, "flush to the bottom work-area edge");
    }

    #[test]
    fn clamp_pushes_a_top_left_overflow_back_on_screen() {
        // SPEC §39: a negative origin (off the top-left, e.g. after a monitor
        // arrangement change) snaps back to the work-area corner.
        let (x, y) = clamp_rect_onto_work_area((-40, -30), POPUP_SIZE, WORK_AREA);
        assert_eq!((x, y), (0, 0), "negative origin clamps to the work-area top-left");
    }

    #[test]
    fn clamp_keeps_the_origin_reachable_for_an_oversized_window() {
        // SPEC §39: a window taller/wider than the work area can't fit fully;
        // the `.max(work_*)` floor still keeps the top-left on-screen (origin
        // reachable) rather than clamping it to a negative value.
        let oversized = (WORK_AREA.1 .0 + 200, WORK_AREA.1 .1 + 200);
        let (x, y) = clamp_rect_onto_work_area((500, 500), oversized, WORK_AREA);
        assert_eq!((x, y), (0, 0), "oversized window pins its origin to the work-area corner");
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
    fn ask_height_grows_then_caps_at_640() {
        // SPEC §35.2 auto-size: short content keeps the 140 floor, taller content
        // grows with the form/perm, and a very tall render caps at 640 (then the
        // content area scrolls) — the ask analog of `popup_target_height`.
        assert_eq!(
            ask_target_height(80.0),
            ASK_MIN_H,
            "short/empty content → the 140 floor"
        );
        assert_eq!(ask_target_height(420.0), 420.0, "grows with content");
        assert_eq!(
            ask_target_height(900.0),
            ASK_MAX_H,
            "capped at 640, then scrolls"
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

    // --- SPEC §25 lamp mode ------------------------------------------------

    #[test]
    fn content_resize_is_skipped_only_in_lamp_mode() {
        assert!(!should_skip_content_resize(PopupMode::List));
        assert!(should_skip_content_resize(PopupMode::Lamp));
    }

    #[test]
    fn unpin_forces_list_only_when_currently_in_lamp_mode() {
        // SPEC R-25.2: unpinning while already in list mode is a no-op (there's
        // nothing to expand); unpinning while collapsed forces list.
        assert!(
            !should_force_list_on_unpin(true, PopupMode::Lamp),
            "still pinned: no-op"
        );
        assert!(
            !should_force_list_on_unpin(false, PopupMode::List),
            "already list: no-op"
        );
        assert!(
            should_force_list_on_unpin(false, PopupMode::Lamp),
            "unpinned + lamp: force list"
        );
    }

    #[test]
    fn popup_size_band_lamp_is_a_fixed_56_square() {
        assert_eq!(
            popup_size_band(PopupMode::Lamp),
            ((LAMP_SIZE, LAMP_SIZE), (LAMP_SIZE, LAMP_SIZE))
        );
    }

    #[test]
    fn popup_size_band_list_matches_the_r14_3_band() {
        assert_eq!(
            popup_size_band(PopupMode::List),
            ((POPUP_W, POPUP_MIN_H), (POPUP_W, POPUP_MAX_H))
        );
    }

    #[test]
    fn min_before_max_when_shrinking_to_the_lamp() {
        // Current max is the list's 360; shrinking to the lamp's 56 must lower
        // min first (raising max first while min is still 360 would ask for an
        // invalid transient `min(360) > max(56)`).
        assert!(min_before_max(POPUP_W, LAMP_SIZE));
    }

    #[test]
    fn max_before_min_when_growing_back_to_the_list() {
        // Current max is the lamp's 56; growing back to the list's 360 must
        // raise max first (lowering min to 360 first while max is still 56
        // would ask for the same invalid transient state).
        assert!(!min_before_max(LAMP_SIZE, POPUP_W));
    }

    #[test]
    fn min_before_max_is_safe_on_a_no_op_transition() {
        // Same mode twice (e.g. a redundant `set_setting` echo): min<=max
        // either way, so the order genuinely doesn't matter, but the function
        // must still return a definite answer, not panic.
        assert!(min_before_max(POPUP_W, POPUP_W));
    }

    #[test]
    fn popup_pos_setting_key_is_per_mode() {
        // SPEC §48: the `Moved` handler persists under the CURRENT mode's own
        // key so the lamp and the list keep independent remembered positions.
        assert_eq!(popup_pos_setting_key(PopupMode::List), "popupPos");
        assert_eq!(popup_pos_setting_key(PopupMode::Lamp), "lampPos");
    }

    #[test]
    fn saved_pos_for_mode_picks_the_target_modes_own_field() {
        use crate::settings::PopupPos;
        let list = Some(PopupPos { x: 10.0, y: 20.0 });
        let lamp = Some(PopupPos { x: 700.0, y: 900.0 });

        // Switching to list restores the list's spot; to lamp restores the
        // lamp's — never the other mode's coordinate.
        assert_eq!(saved_pos_for_mode(PopupMode::List, list, lamp), list);
        assert_eq!(saved_pos_for_mode(PopupMode::Lamp, list, lamp), lamp);

        // A mode with no saved position yet yields None (caller keeps its
        // grow/anchor behavior) even when the OTHER mode has one.
        assert_eq!(saved_pos_for_mode(PopupMode::Lamp, list, None), None);
        assert_eq!(saved_pos_for_mode(PopupMode::List, None, lamp), None);
    }

    #[test]
    fn popup_pos_persist_is_debounced_but_always_fires_on_the_first_move() {
        let t0 = Instant::now();
        assert!(
            should_persist_popup_pos(None, t0, POPUP_POS_PERSIST_GAP),
            "no prior write: always persist"
        );
        assert!(
            !should_persist_popup_pos(Some(t0), t0, POPUP_POS_PERSIST_GAP),
            "immediately after a write: debounced"
        );
        let just_under = t0 + POPUP_POS_PERSIST_GAP - Duration::from_millis(1);
        assert!(!should_persist_popup_pos(
            Some(t0),
            just_under,
            POPUP_POS_PERSIST_GAP
        ));
        let at_gap = t0 + POPUP_POS_PERSIST_GAP;
        assert!(
            should_persist_popup_pos(Some(t0), at_gap, POPUP_POS_PERSIST_GAP),
            "once the gap has fully elapsed, persist again"
        );
    }
}
