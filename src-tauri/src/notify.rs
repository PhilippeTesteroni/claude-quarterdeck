//! Native notifications (SPEC §9): the two toast classes (standard / alert),
//! distinct system sounds, stable `AppUserModelID` (R-9.3), throttling
//! (R-9.4), and the `QUARTERDECK_FAKE_NOTIFIER=1` fake mode that appends
//! calls to `<data>/notifier-calls.jsonl` (R-3.2). Implements
//! [`deck_core::traits::Notifier`] via [`DesktopNotifier`].
//!
//! ## Design notes for the integrator (T7)
//!
//! `deck_core::traits::{Notifier, ToastKind}` got their real shape here (T1's
//! own doc comment on `traits.rs` explicitly deferred `Notifier`'s methods to
//! T5: "Methods are added by T5 alongside `notify.rs`"). The engine (T1)
//! itself never calls a `Notifier`; per that same comment it emits
//! `Effect::Toast`-style decisions that whatever composes the app (T7, or
//! T3's glue) should turn into `notifier.notify(kind, session_id, project,
//! detail, popup_visible_and_focused)` calls — `detail` means the session
//! task title for `Idle`, the notification message for `Attention`, or the
//! question text for `Ask` (see the trait doc comment). [`DesktopNotifier`]
//! also exposes a richer, owned-`String` inherent API
//! ([`DesktopNotifier::send`], [`ToastRequest`]) that's nicer to call from
//! Rust call sites (the demo example and tests use it) — both paths funnel
//! into the same throttle + compose + fire logic.
//!
//! [`data_dir`] duplicates the `QUARTERDECK_DATA_DIR` resolution that
//! `settings.rs` (T3) also owns per its module doc comment. It was
//! reimplemented locally so this module stays independently testable while
//! T3 is being built in parallel; T7 should make one call the other (or
//! extract a tiny shared helper) once both exist.
//!
//! Toast click routing (R-9.6, "opens the popup / ask window"): implemented on
//! both desktop platforms by firing real toasts through the OS-native backend
//! with a click callback that emits [`TOAST_CLICKED_EVENT`]
//! (`deck://toast-clicked`) carrying the toast's [`ToastKind`] + session id;
//! `lib.rs` listens for it and opens the popup (or the ask window for `Ask`
//! toasts). On Windows this is `tauri-winrt-notification` (the same WinRT
//! backend `tauri-plugin-notification` uses under the hood) with an
//! `.on_activated` handler. On macOS it is `mac-notification-sys` with
//! `wait_for_click`, run on a detached thread (the click wait blocks until the
//! user interacts with or dismisses the toast). Either path falls back to the
//! plugin if the native send fails, so the toast still shows
//! (R-9.1/9.2/9.3 never regress). On Windows, click delivery to our own
//! `on_activated` requires the toast to carry the app's registered
//! AppUserModelID (packaged builds); unregistered dev builds fall back to the
//! pre-registered PowerShell AUMID so toasts appear at all, but their
//! activation is owned by that AUMID, so click routing is a packaged-build
//! feature there. Linux (a §13 non-goal) keeps the plugin path, which exposes
//! no click callback.
//!
//! Alert icon (R-9.2, "red-badged icon variant where the platform allows"): the
//! alert toast class (`Attention`/`Ask`) carries the red status icon so it is
//! visually distinct from the quiet `Idle`/`Reminder` toasts. The OS toast APIs
//! reference an icon by file path/URI (not embedded bytes), so the bundled red
//! tray PNG is materialized once to a stable path under the data dir and reused
//! (see [`alert_icon_path`]).

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use deck_core::traits::Notifier;
pub use deck_core::traits::ToastKind;
use serde::{Deserialize, Serialize};
#[cfg_attr(not(windows), allow(unused_imports))]
use tauri::Emitter;
use tauri::{AppHandle, Runtime};
use tauri_plugin_notification::NotificationExt;

/// Tauri event emitted when a native toast is clicked/activated (R-9.6). Payload
/// is [`ToastClickPayload`]; `lib.rs` routes it to the popup or ask window.
pub const TOAST_CLICKED_EVENT: &str = "deck://toast-clicked";

/// Payload of [`TOAST_CLICKED_EVENT`]: which toast was clicked, so the shell can
/// open the ask window for `Ask` toasts and the popup for everything else.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToastClickPayload {
    pub kind: ToastKind,
    pub session_id: String,
}

/// Stable Windows AppUserModelID (R-9.3), matching `identifier` in
/// `tauri.conf.json`. `tauri-plugin-notification` reads
/// `app.config().identifier` and uses it as the toast's app id in packaged
/// builds; in `cargo run`/`tauri dev` (exe under `target/debug` or
/// `target/release`) it deliberately falls back to a well-known, already
/// registered id (`Toast::POWERSHELL_APP_ID`) instead, because an
/// unregistered custom AUMID is unreliable before the app has an installed
/// Start Menu shortcut. That fallback is why toasts work in *both* dev and
/// packaged modes (R-9.3) without any extra registration code here.
pub const APP_USER_MODEL_ID: &str = "pro.philippgross.quarterdeck";

/// Env var toggling the fake notifier (R-3.2): when set to `"1"`, calls are
/// appended as JSON lines to `<data>/notifier-calls.jsonl` instead of firing
/// a real OS toast, for e2e assertions.
pub const FAKE_NOTIFIER_ENV: &str = "QUARTERDECK_FAKE_NOTIFIER";

/// Data-root override (SPEC R-3.3). Required for test isolation; see the
/// module-level note above about the duplication with `settings.rs`.
pub const DATA_DIR_ENV: &str = "QUARTERDECK_DATA_DIR";

/// A toast to fire for a session (SPEC §8/§9). [`ToastKind`] (re-exported
/// from `deck_core::traits`) picks which of the R-9.1/R-9.2/R-8.4/R-2.3 copy
/// templates applies.
#[derive(Debug, Clone)]
pub struct ToastRequest {
    pub kind: ToastKind,
    /// Throttle key (R-9.4) and click-routing key (R-9.6).
    pub session_id: String,
    pub project: String,
    /// Meaning depends on `kind`: the session's task title for `Idle`
    /// (R-5.2 precedence already applied by the caller; empty/blank renders
    /// as `"(no title)"`), the notification message for `Attention`, or the
    /// question text for `Ask`. Ignored for `Reminder`, whose body is fixed
    /// (R-2.3 gives no custom copy).
    pub detail: String,
}

/// Builds the exact `(title, body)` toast copy per R-9.1/R-9.2/R-8.4/R-2.3.
/// Pure and OS-free so the exact strings are unit-testable without firing a
/// real toast.
pub fn compose(req: &ToastRequest) -> (String, String) {
    match req.kind {
        ToastKind::Idle => {
            let title = format!("{} finished", req.project);
            let session_title = if req.detail.trim().is_empty() {
                "(no title)"
            } else {
                req.detail.trim()
            };
            let body = format!("{session_title} Waiting for new instructions.");
            (title, body)
        }
        ToastKind::Attention => {
            let title = format!("{} needs you", req.project);
            (title, req.detail.clone())
        }
        ToastKind::Ask => {
            let title = format!(
                "{} asks: {}",
                req.project,
                truncate_ellipsis(&req.detail, 60)
            );
            (title, req.detail.clone())
        }
        ToastKind::Reminder => {
            let title = format!("{} still waiting", req.project);
            (title, "Still waiting for your instructions.".to_string())
        }
    }
}

/// Truncates `s` to at most `max_chars` grapheme clusters, appending `…` when it
/// was actually shortened, matching the `"<question…>"` shape from R-8.4. Reuses
/// [`deck_core::naming::truncate_graphemes`] so the ask-toast title is truncated
/// grapheme-cluster-safe (Unicode-aware for R-5.3-style text): a ZWJ emoji
/// sequence or flag straddling the boundary is dropped whole, never severed
/// mid-cluster.
fn truncate_ellipsis(s: &str, max_chars: usize) -> String {
    deck_core::naming::truncate_graphemes(s.trim(), max_chars)
}

fn sound_for(kind: ToastKind) -> &'static str {
    match kind {
        // `Reminder` is an informational nudge, not an alert (the engine's
        // own draft marks it non-alert) — same non-urgent channel as `Idle`.
        ToastKind::Idle | ToastKind::Reminder => idle_sound(),
        ToastKind::Attention | ToastKind::Ask => attention_sound(),
    }
}

/// Whether a toast kind is in the alert class (R-9.2): `Attention` (permission /
/// elicitation) and `Ask`. These get the distinct alert sound and the
/// red-badged icon; `Idle`/`Reminder` do not.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn is_alert(kind: ToastKind) -> bool {
    matches!(kind, ToastKind::Attention | ToastKind::Ask)
}

/// Red status icon bytes (R-9.2 "red-badged icon variant"), embedded from the
/// same tray asset the attention tray icon uses. Path is relative to this
/// source file (`src-tauri/src/`), so `../../assets` reaches the repo-root
/// `assets/`.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
const ALERT_ICON_PNG: &[u8] = include_bytes!("../../assets/tray/red-32.png");

/// Materializes the embedded red alert icon to a stable on-disk path under the
/// data dir and returns it (R-9.2). The OS toast APIs want a file path/URI, not
/// embedded bytes, so this writes the PNG once and reuses it thereafter.
/// Returns `None` (icon simply omitted) if the file can't be written.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn alert_icon_path() -> Option<PathBuf> {
    let dir = data_dir().join("toast-icons");
    let path = dir.join("alert-32.png");
    if path.exists() {
        return Some(path);
    }
    fs::create_dir_all(&dir).ok()?;
    fs::write(&path, ALERT_ICON_PNG).ok()?;
    Some(path)
}

// --- Windows sounds (R-9.1/R-9.2) ---------------------------------------
//
// `tauri-plugin-notification` forwards this string to
// `tauri-winrt-notification`'s `Sound::from_str`, which renders it into the
// toast XML as `<audio src="ms-winsoundevent:Notification.<name>" />`.
// Crucially, `Sound::Default` (the string `"Default"`) renders *no* `<audio>`
// element at all, which is what makes Windows play its actual default toast
// sound (R-9.1) — leaving the sound entirely unset instead renders
// `<audio silent="true" />` and produces a silent toast, so we always pass an
// explicit sound string.
#[cfg(target_os = "windows")]
fn idle_sound() -> &'static str {
    "Default"
}

#[cfg(target_os = "windows")]
fn attention_sound() -> &'static str {
    // "Reminder" (ms-winsoundevent:Notification.Reminder) is a two-tone
    // alert-class sound distinct from the plain default chime, and less
    // jarring than e.g. "SMS" or the looping alarm sounds (R-9.2: "least
    // obnoxious").
    "Reminder"
}

// --- macOS sounds (R-9.1/R-9.2) -----------------------------------------
//
// `notify-rust`'s macOS backend passes this string straight through to
// `NSUserNotification.soundName`. `NSUserNotificationDefaultSoundName` is the
// literal string value of Apple's constant of the same name (there is no way
// to reference the Foundation symbol directly from outside Objective-C), and
// selects the standard system notification sound (R-9.1). `Basso` is one of
// the system alert sounds (`/System/Library/Sounds/Basso.aiff`) explicitly
// named in R-9.2 as an acceptable `Basso`/`Sosumi`-class alert sound.
#[cfg(target_os = "macos")]
fn idle_sound() -> &'static str {
    "NSUserNotificationDefaultSoundName"
}

#[cfg(target_os = "macos")]
fn attention_sound() -> &'static str {
    "Basso"
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn idle_sound() -> &'static str {
    ""
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn attention_sound() -> &'static str {
    ""
}

/// Injectable time source for [`Throttle`], local to this module. `deck-core`
/// still only scaffolds a method-less `Clock` trait (see module docs), so
/// this is deliberately independent rather than a premature dependency on an
/// unfinished shared abstraction.
pub trait ThrottleClock {
    fn now(&self) -> Instant;
}

/// The real wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl ThrottleClock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// R-9.4 throttle: per `(session_id, kind)`, at most one toast fires per 10 s
/// window (bursts within the window collapse to the first firing); a toast
/// is suppressed outright when the popup is visible AND focused. `Ask`
/// toasts bypass both rules ("Ask toasts never suppressed").
pub struct Throttle<C: ThrottleClock = SystemClock> {
    clock: C,
    window: Duration,
    last_fired: HashMap<(String, ToastKind), Instant>,
}

impl Default for Throttle<SystemClock> {
    fn default() -> Self {
        Self::new()
    }
}

impl Throttle<SystemClock> {
    /// The production throttle: real clock, 10 s window (R-9.4).
    pub fn new() -> Self {
        Self::with_clock(SystemClock, Duration::from_secs(10))
    }
}

impl<C: ThrottleClock> Throttle<C> {
    pub fn with_clock(clock: C, window: Duration) -> Self {
        Self {
            clock,
            window,
            last_fired: HashMap::new(),
        }
    }

    /// Returns whether a toast for `(session_id, kind)` should actually fire
    /// right now, recording the firing time when it does.
    pub fn allow(
        &mut self,
        session_id: &str,
        kind: ToastKind,
        popup_visible_and_focused: bool,
    ) -> bool {
        if kind == ToastKind::Ask {
            return true;
        }
        if popup_visible_and_focused {
            return false;
        }
        let key = (session_id.to_string(), kind);
        let now = self.clock.now();
        if let Some(&last) = self.last_fired.get(&key) {
            if now.duration_since(last) < self.window {
                return false;
            }
        }
        // Bound the map before recording this firing: drop every entry whose
        // window has fully elapsed. Such an entry no longer throttles anything (a
        // fresh toast for its key would pass), so evicting it changes no decision
        // — it just stops `last_fired` accreting one permanent entry per distinct
        // (session, kind) ever toasted across a weeks-long run (each Claude Code
        // session has a fresh UUID). After any firing the map holds only keys
        // fired within the last `window`, so its size tracks live activity rather
        // than lifetime session count.
        let window = self.window;
        self.last_fired
            .retain(|_, &mut last| now.duration_since(last) < window);
        self.last_fired.insert(key, now);
        true
    }
}

fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var(DATA_DIR_ENV) {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    platform_data_dir()
}

// NOTE: these mirror `settings::platform_data_dir` exactly — including the
// `temp_dir()` fallback when the platform var is unset. Reached from the alert
// toast hot path (`alert_icon_path`) whenever `QUARTERDECK_DATA_DIR` is unset, so
// a stripped environment (a service/SYSTEM context, a sanitized launch) with no
// `%APPDATA%`/`$HOME` must degrade, not panic the firing thread (R-3.5 "never
// crash on the unexpected"), which an `.expect()` here would do.
#[cfg(target_os = "windows")]
fn platform_data_dir() -> PathBuf {
    std::env::var("APPDATA")
        .map(|appdata| PathBuf::from(appdata).join("quarterdeck"))
        .unwrap_or_else(|_| std::env::temp_dir().join("quarterdeck"))
}

#[cfg(target_os = "macos")]
fn platform_data_dir() -> PathBuf {
    std::env::var("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support/quarterdeck"))
        .unwrap_or_else(|_| std::env::temp_dir().join("quarterdeck"))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn platform_data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".quarterdeck")
}

fn fake_notifier_enabled() -> bool {
    is_truthy(std::env::var(FAKE_NOTIFIER_ENV).ok().as_deref())
}

fn is_truthy(v: Option<&str>) -> bool {
    matches!(v, Some("1"))
}

/// One line of `<data>/notifier-calls.jsonl` (R-3.2). Field shape is
/// Quarterdeck-internal (no spec-mandated schema) — kept flat,
/// self-describing, and camelCase to match the rest of the app's JSON
/// (`ipc.rs`) for e2e assertions.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FakeCallRecord<'a> {
    v: u8,
    at_ms: u128,
    kind: ToastKind,
    session_id: &'a str,
    project: &'a str,
    title: &'a str,
    body: &'a str,
    sound: &'a str,
}

/// Appends one JSON line describing a would-be toast to
/// `<dir>/notifier-calls.jsonl`, creating `dir` if needed. Pure I/O with an
/// explicit directory (no env reads), so it is directly unit-testable with a
/// scratch directory instead of mutating process-global env vars.
fn append_fake_call(
    dir: &Path,
    req: &ToastRequest,
    title: &str,
    body: &str,
    sound: &str,
) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let record = FakeCallRecord {
        v: 1,
        at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        kind: req.kind,
        session_id: &req.session_id,
        project: &req.project,
        title,
        body,
        sound,
    };
    let line = serde_json::to_string(&record).unwrap_or_default();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("notifier-calls.jsonl"))?;
    writeln!(file, "{line}")
}

/// Fires native toasts via `tauri-plugin-notification` (SPEC §9), honoring
/// the R-9.4 throttle and the `QUARTERDECK_FAKE_NOTIFIER=1` fake mode
/// (R-3.2).
pub struct DesktopNotifier<R: Runtime> {
    app: AppHandle<R>,
    throttle: Mutex<Throttle>,
}

/// Real implementation of [`deck_core::traits::Notifier`] (SPEC R-3.2): the
/// engine/shell call [`Notifier::notify`] with borrowed strings; this
/// forwards into [`DesktopNotifier::send`], which callers within this crate
/// (the demo example, tests) can also use directly with an owned
/// [`ToastRequest`].
impl<R: Runtime> Notifier for DesktopNotifier<R> {
    fn notify(
        &self,
        kind: ToastKind,
        session_id: &str,
        project: &str,
        detail: &str,
        popup_visible_and_focused: bool,
    ) -> bool {
        self.send(
            ToastRequest {
                kind,
                session_id: session_id.to_string(),
                project: project.to_string(),
                detail: detail.to_string(),
            },
            popup_visible_and_focused,
        )
    }
}

/// Bounds the number of threads parked in `mac-notification-sys` `send()`
/// waiting for a toast click (R-9.6). Each `wait_for_click(true)` send blocks
/// its thread until the user interacts with (or dismisses) that specific toast;
/// a user who ignores toasts (they linger in Notification Center) would
/// otherwise leak one parked thread per toast for the life of the process — an
/// unbounded per-toast thread leak over a long macOS session with parallel
/// agents. Past the cap, `fire_macos` fires without waiting so the thread
/// returns immediately, losing only click-to-open routing for toasts fired
/// while already at the cap (R-9.6 is best-effort) — never the toast itself.
#[cfg(target_os = "macos")]
static MACOS_TOAST_WAITERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Maximum concurrently-parked macOS click-waiter threads (see
/// [`MACOS_TOAST_WAITERS`]).
#[cfg(target_os = "macos")]
const MACOS_MAX_TOAST_WAITERS: usize = 8;

/// RAII release of a reserved [`MACOS_TOAST_WAITERS`] slot: decrements on the
/// waiter thread's exit (including via panic in `send()`), so the count can't
/// drift upward and permanently wedge the cap.
#[cfg(target_os = "macos")]
struct MacosWaiterGuard(bool);

#[cfg(target_os = "macos")]
impl Drop for MacosWaiterGuard {
    fn drop(&mut self) {
        if self.0 {
            MACOS_TOAST_WAITERS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

impl<R: Runtime> DesktopNotifier<R> {
    pub fn new(app: AppHandle<R>) -> Self {
        Self {
            app,
            throttle: Mutex::new(Throttle::new()),
        }
    }

    /// Fires `req` unless throttled/suppressed per R-9.4.
    /// `popup_visible_and_focused` should reflect the popup window's current
    /// state at call time. Returns whether a toast was actually sent (for
    /// logging/tests).
    pub fn send(&self, req: ToastRequest, popup_visible_and_focused: bool) -> bool {
        let allowed = self
            .throttle
            .lock()
            .expect("throttle mutex poisoned")
            .allow(&req.session_id, req.kind, popup_visible_and_focused);
        if !allowed {
            tracing::debug!(
                session_id = %req.session_id,
                kind = ?req.kind,
                "toast throttled/suppressed (R-9.4)"
            );
            return false;
        }
        self.fire(&req);
        true
    }

    fn fire(&self, req: &ToastRequest) {
        let (title, body) = compose(req);
        let sound = sound_for(req.kind);

        if fake_notifier_enabled() {
            if let Err(err) = append_fake_call(&data_dir(), req, &title, &body, sound) {
                tracing::warn!(?err, "failed to append fake notifier call");
            }
            return;
        }

        #[cfg(windows)]
        self.fire_windows(req, &title, &body, sound);
        #[cfg(target_os = "macos")]
        self.fire_macos(req, &title, &body, sound);
        #[cfg(not(any(windows, target_os = "macos")))]
        self.fire_via_plugin(req.kind, &title, &body, sound);
    }

    /// The proven `tauri-plugin-notification` path: always shows a toast, but
    /// exposes no click callback. Used on Linux (§13 non-goal) and as the
    /// Windows/macOS fallback when the native click-routing path can't fire
    /// (R-9.1/9.2/9.3). Attaches the red alert icon for alert kinds (R-9.2).
    fn fire_via_plugin(&self, kind: ToastKind, title: &str, body: &str, sound: &str) {
        let mut builder = self
            .app
            .notification()
            .builder()
            .title(title.to_string())
            .body(body.to_string())
            .sound(sound);
        if is_alert(kind) {
            if let Some(icon) = alert_icon_path() {
                builder = builder.icon(icon.to_string_lossy().into_owned());
            }
        }
        if let Err(err) = builder.show() {
            tracing::warn!(?err, "failed to show toast");
        }
    }

    /// Windows path (R-9.6): fire via `tauri-winrt-notification` with an
    /// `on_activated` handler emitting [`TOAST_CLICKED_EVENT`]; fall back to the
    /// plugin if WinRT can't show it, so the toast is never lost.
    #[cfg(windows)]
    fn fire_windows(&self, req: &ToastRequest, title: &str, body: &str, sound: &str) {
        if self.fire_windows_winrt(req, title, body, sound).is_err() {
            self.fire_via_plugin(req.kind, title, body, sound);
        }
    }

    #[cfg(windows)]
    fn fire_windows_winrt(
        &self,
        req: &ToastRequest,
        title: &str,
        body: &str,
        sound: &str,
    ) -> tauri_winrt_notification::Result<()> {
        use tauri_winrt_notification::{IconCrop, Sound, Toast};

        // Match the plugin's AppUserModelID behavior (R-9.3): a packaged/installed
        // build uses its own identifier so click activation reaches us; an
        // unregistered dev build falls back to the pre-registered PowerShell
        // AUMID so the toast shows at all (click routing then belongs to that
        // AUMID — a packaged-build feature).
        let app_id = if is_dev_exe() {
            Toast::POWERSHELL_APP_ID.to_string()
        } else {
            self.app.config().identifier.clone()
        };

        let payload = ToastClickPayload {
            kind: req.kind,
            session_id: req.session_id.clone(),
        };
        let app = self.app.clone();

        let mut toast = Toast::new(&app_id).title(title).text1(body);
        // R-9.2: red-badged icon for the alert (Attention/Ask) toast class.
        let alert_icon = if is_alert(req.kind) {
            alert_icon_path()
        } else {
            None
        };
        if let Some(icon) = &alert_icon {
            toast = toast.icon(icon, IconCrop::Square, "Quarterdeck alert");
        }
        if let Ok(s) = Sound::try_from(sound) {
            toast = toast.sound(Some(s));
        }
        toast
            .on_activated(move |_action| {
                // R-9.6: click opens the popup (or ask window for ask toasts).
                let _ = app.emit(TOAST_CLICKED_EVENT, payload.clone());
                Ok(())
            })
            .show()?;
        Ok(())
    }

    /// macOS path (R-9.6): fire via `mac-notification-sys` with `wait_for_click`
    /// so a click routes to [`TOAST_CLICKED_EVENT`] (opening the popup, or the
    /// ask window for `Ask` toasts) — the platform analog of the Windows
    /// `.on_activated` handler. `send()` blocks until the user clicks or
    /// dismisses the toast, so it runs on a detached thread. If the native send
    /// fails (e.g. an unregistered bundle id in a dev build), it falls back to
    /// the plugin so the toast is never lost (R-9.1/9.2/9.3). The red alert icon
    /// (R-9.2) is attached for alert kinds.
    #[cfg(target_os = "macos")]
    fn fire_macos(&self, req: &ToastRequest, title: &str, body: &str, sound: &str) {
        use mac_notification_sys::{set_application, Notification, NotificationResponse};

        let app = self.app.clone();
        let payload = ToastClickPayload {
            kind: req.kind,
            session_id: req.session_id.clone(),
        };
        let bundle_id = self.app.config().identifier.clone();
        let title = title.to_string();
        let body = body.to_string();
        let sound = sound.to_string();
        let icon = if is_alert(req.kind) {
            alert_icon_path().map(|p| p.to_string_lossy().into_owned())
        } else {
            None
        };

        // Reserve a click-waiter slot if one is free (R-9.6). Past the cap, fire
        // without waiting so this thread returns promptly instead of parking
        // forever on an ignored toast (see `MACOS_TOAST_WAITERS`).
        let wait_for_click = MACOS_TOAST_WAITERS.load(std::sync::atomic::Ordering::Relaxed)
            < MACOS_MAX_TOAST_WAITERS;
        if wait_for_click {
            MACOS_TOAST_WAITERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        std::thread::spawn(move || {
            // Releases the reserved waiter slot when this thread exits.
            let _waiter_guard = MacosWaiterGuard(wait_for_click);
            // Best-effort: attribute the toast to Quarterdeck's bundle id (ignore
            // errors — a non-bundled dev binary may not be registered, in which
            // case `send()` below fails and we fall back to the plugin).
            let _ = set_application(&bundle_id);

            let response = {
                let mut notification = Notification::new();
                notification
                    .title(&title)
                    .message(&body)
                    .wait_for_click(wait_for_click);
                if !sound.is_empty() {
                    notification.sound(sound.as_str());
                }
                if let Some(ref icon_path) = icon {
                    notification.app_icon(icon_path);
                }
                notification.send()
            };

            match response {
                Ok(NotificationResponse::Click) | Ok(NotificationResponse::ActionButton(_)) => {
                    // R-9.6: click opens the popup (or ask window for ask toasts).
                    let _ = app.emit(TOAST_CLICKED_EVENT, payload);
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        "mac-notification-sys send failed; falling back to plugin"
                    );
                    let mut builder = app
                        .notification()
                        .builder()
                        .title(title)
                        .body(body)
                        .sound(sound);
                    if let Some(icon_path) = icon {
                        builder = builder.icon(icon_path);
                    }
                    if let Err(err) = builder.show() {
                        tracing::warn!(?err, "plugin fallback toast also failed");
                    }
                }
            }
        });
    }
}

/// True when the running executable is an un-installed dev build
/// (`target/debug` or `target/release`), mirroring the plugin's own check so
/// the AppUserModelID decision matches (R-9.3).
#[cfg(windows)]
fn is_dev_exe() -> bool {
    use std::path::MAIN_SEPARATOR as SEP;
    match std::env::current_exe() {
        Ok(exe) => {
            let dir = exe
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            dir.ends_with(&format!("{SEP}target{SEP}debug"))
                || dir.ends_with(&format!("{SEP}target{SEP}release"))
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    // --- compose() — exact copy per R-9.1/R-9.2/R-8.4 --------------------

    #[test]
    fn idle_toast_copy_matches_r91() {
        let req = ToastRequest {
            kind: ToastKind::Idle,
            session_id: "s1".into(),
            project: "quarterdeck".into(),
            detail: "Refactor auth module".into(),
        };
        let (title, body) = compose(&req);
        assert_eq!(title, "quarterdeck finished");
        assert_eq!(body, "Refactor auth module Waiting for new instructions.");
    }

    #[test]
    fn idle_toast_falls_back_to_no_title() {
        let req = ToastRequest {
            kind: ToastKind::Idle,
            session_id: "s1".into(),
            project: "quarterdeck".into(),
            detail: "   ".into(),
        };
        let (_, body) = compose(&req);
        assert_eq!(body, "(no title) Waiting for new instructions.");
    }

    #[test]
    fn attention_toast_copy_matches_r92() {
        let req = ToastRequest {
            kind: ToastKind::Attention,
            session_id: "s1".into(),
            project: "quarterdeck".into(),
            detail: "Allow Bash to run `rm -rf build`?".into(),
        };
        let (title, body) = compose(&req);
        assert_eq!(title, "quarterdeck needs you");
        assert_eq!(body, "Allow Bash to run `rm -rf build`?");
    }

    #[test]
    fn ask_toast_copy_matches_r84_and_truncates() {
        let long_question = "a".repeat(120);
        let req = ToastRequest {
            kind: ToastKind::Ask,
            session_id: "s1".into(),
            project: "quarterdeck".into(),
            detail: long_question.clone(),
        };
        let (title, body) = compose(&req);
        assert!(title.starts_with("quarterdeck asks: "));
        assert!(title.ends_with('…'));
        assert!(title.chars().count() < long_question.chars().count());
        assert_eq!(body, long_question);
    }

    #[test]
    fn ask_toast_truncation_never_severs_a_zwj_emoji_sequence() {
        // R-5.3 Unicode safety on the toast hot path: a ZWJ family emoji
        // (👨‍👩‍👧‍👦, seven scalars = one grapheme cluster) straddling the 60-char
        // ask-title cap must be dropped whole, never severed into a lone prefix
        // or a dangling ZWJ. Build a question whose cut would land inside it.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
        let detail = format!("{}{family} tail", "a".repeat(58));
        let req = ToastRequest {
            kind: ToastKind::Ask,
            session_id: "s1".into(),
            project: "quarterdeck".into(),
            detail,
        };
        let (title, _) = compose(&req);
        assert!(title.ends_with('…'));
        // Cluster-aligned truncation can never leave a trailing ZWJ before the
        // ellipsis (a whole family emoji has internal joiners, so we can't assert
        // `!contains ZWJ`).
        assert!(
            !title.trim_end_matches('…').ends_with('\u{200D}'),
            "no dangling ZWJ joiner in the toast title: {title:?}"
        );
        let whole = title.contains(family);
        let none = !title.contains('\u{1F468}')
            && !title.contains('\u{1F469}')
            && !title.contains('\u{1F467}')
            && !title.contains('\u{1F466}');
        assert!(
            whole || none,
            "family emoji whole or dropped, never split: {title:?}"
        );
    }

    #[test]
    fn ask_toast_short_question_not_truncated() {
        let req = ToastRequest {
            kind: ToastKind::Ask,
            session_id: "s1".into(),
            project: "quarterdeck".into(),
            detail: "Use Postgres or SQLite?".into(),
        };
        let (title, _) = compose(&req);
        assert_eq!(title, "quarterdeck asks: Use Postgres or SQLite?");
    }

    #[test]
    fn reminder_toast_copy_matches_r23() {
        let req = ToastRequest {
            kind: ToastKind::Reminder,
            session_id: "s1".into(),
            project: "quarterdeck".into(),
            detail: "ignored for reminders".into(),
        };
        let (title, body) = compose(&req);
        assert_eq!(title, "quarterdeck still waiting");
        assert_eq!(body, "Still waiting for your instructions.");
    }

    #[test]
    fn idle_and_attention_sounds_are_distinct() {
        assert_ne!(idle_sound(), attention_sound());
        assert!(!idle_sound().is_empty());
        assert!(!attention_sound().is_empty());
    }

    #[test]
    fn ask_reuses_attention_sound_channel() {
        assert_eq!(sound_for(ToastKind::Ask), sound_for(ToastKind::Attention));
    }

    #[test]
    fn reminder_reuses_idle_sound_channel() {
        assert_eq!(sound_for(ToastKind::Reminder), sound_for(ToastKind::Idle));
    }

    // --- throttle (R-9.4), with a fake clock -----------------------------

    #[derive(Clone)]
    struct FakeClock(Rc<Cell<Instant>>);

    impl FakeClock {
        fn new() -> Self {
            Self(Rc::new(Cell::new(Instant::now())))
        }

        fn advance(&self, d: Duration) {
            self.0.set(self.0.get() + d);
        }
    }

    impl ThrottleClock for FakeClock {
        fn now(&self) -> Instant {
            self.0.get()
        }
    }

    #[test]
    fn first_toast_for_session_always_fires() {
        let clock = FakeClock::new();
        let mut throttle = Throttle::with_clock(clock, Duration::from_secs(10));
        assert!(throttle.allow("s1", ToastKind::Idle, false));
    }

    #[test]
    fn burst_within_window_collapses_to_one() {
        let clock = FakeClock::new();
        let mut throttle = Throttle::with_clock(clock.clone(), Duration::from_secs(10));
        assert!(throttle.allow("s1", ToastKind::Attention, false));
        clock.advance(Duration::from_secs(1));
        assert!(!throttle.allow("s1", ToastKind::Attention, false));
        clock.advance(Duration::from_secs(5));
        assert!(!throttle.allow("s1", ToastKind::Attention, false));
    }

    #[test]
    fn fires_again_after_window_elapses() {
        let clock = FakeClock::new();
        let mut throttle = Throttle::with_clock(clock.clone(), Duration::from_secs(10));
        assert!(throttle.allow("s1", ToastKind::Idle, false));
        clock.advance(Duration::from_secs(10));
        assert!(throttle.allow("s1", ToastKind::Idle, false));
    }

    #[test]
    fn different_sessions_throttle_independently() {
        let clock = FakeClock::new();
        let mut throttle = Throttle::with_clock(clock, Duration::from_secs(10));
        assert!(throttle.allow("s1", ToastKind::Idle, false));
        assert!(throttle.allow("s2", ToastKind::Idle, false));
    }

    #[test]
    fn different_kinds_for_same_session_throttle_independently() {
        let clock = FakeClock::new();
        let mut throttle = Throttle::with_clock(clock, Duration::from_secs(10));
        assert!(throttle.allow("s1", ToastKind::Idle, false));
        assert!(throttle.allow("s1", ToastKind::Attention, false));
    }

    #[test]
    fn suppressed_when_popup_visible_and_focused() {
        let clock = FakeClock::new();
        let mut throttle = Throttle::with_clock(clock, Duration::from_secs(10));
        assert!(!throttle.allow("s1", ToastKind::Idle, true));
        assert!(!throttle.allow("s1", ToastKind::Attention, true));
    }

    #[test]
    fn ask_toasts_never_suppressed_or_throttled() {
        let clock = FakeClock::new();
        let mut throttle = Throttle::with_clock(clock.clone(), Duration::from_secs(10));
        // Even with the popup visible+focused...
        assert!(throttle.allow("s1", ToastKind::Ask, true));
        // ...and in a tight burst with no time advancing...
        assert!(throttle.allow("s1", ToastKind::Ask, true));
        assert!(throttle.allow("s1", ToastKind::Ask, false));
        let _ = clock; // silence unused warning if the above changes
    }

    #[test]
    fn last_fired_map_stays_bounded_as_sessions_churn() {
        // Regression: `last_fired` must not accrete one permanent entry per
        // distinct (session, kind) ever toasted. Drive many one-shot sessions,
        // advancing past the window between each, and assert the map's size
        // tracks the live window (here: ~1 entry) rather than the total count.
        let clock = FakeClock::new();
        let mut throttle = Throttle::with_clock(clock.clone(), Duration::from_secs(10));
        for i in 0..1000 {
            let sid = format!("session-{i}");
            assert!(throttle.allow(&sid, ToastKind::Idle, false));
            // Each session ends and its window fully elapses before the next.
            clock.advance(Duration::from_secs(11));
        }
        assert!(
            throttle.last_fired.len() <= 1,
            "expired throttle entries must be evicted, got {}",
            throttle.last_fired.len()
        );
    }

    #[test]
    fn eviction_does_not_relax_an_active_throttle() {
        // Pruning expired keys must not drop a key that is still inside its
        // window: s1's second toast must still be suppressed even though many
        // other (expired) sessions fired in between.
        let clock = FakeClock::new();
        let mut throttle = Throttle::with_clock(clock.clone(), Duration::from_secs(10));
        assert!(throttle.allow("s1", ToastKind::Idle, false));
        clock.advance(Duration::from_secs(3));
        // A different session fires, triggering a prune pass.
        assert!(throttle.allow("s2", ToastKind::Idle, false));
        // s1 is still within its 10s window → still throttled.
        assert!(!throttle.allow("s1", ToastKind::Idle, false));
    }

    // --- fake-notifier env parsing ---------------------------------------

    #[test]
    fn only_exact_string_one_enables_fake_mode() {
        assert!(is_truthy(Some("1")));
        assert!(!is_truthy(Some("true")));
        assert!(!is_truthy(Some("0")));
        assert!(!is_truthy(Some("")));
        assert!(!is_truthy(None));
    }

    // --- fake-notifier jsonl writer ---------------------------------------

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "quarterdeck-notify-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        dir
    }

    #[test]
    fn append_fake_call_writes_one_jsonl_line_per_call() {
        let dir = scratch_dir("jsonl");
        let req = ToastRequest {
            kind: ToastKind::Attention,
            session_id: "sess-42".into(),
            project: "dream-book".into(),
            detail: "Allow write access?".into(),
        };
        let (title, body) = compose(&req);

        append_fake_call(&dir, &req, &title, &body, "Reminder").expect("first append");
        append_fake_call(&dir, &req, &title, &body, "Reminder").expect("second append");

        let contents = fs::read_to_string(dir.join("notifier-calls.jsonl")).expect("read jsonl");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);

        let parsed: serde_json::Value = serde_json::from_str(lines[0]).expect("valid json line");
        assert_eq!(parsed["v"], 1);
        assert_eq!(parsed["kind"], "attention");
        assert_eq!(parsed["sessionId"], "sess-42");
        assert_eq!(parsed["sound"], "Reminder");
        assert_eq!(parsed["title"], "dream-book needs you");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn append_fake_call_creates_missing_data_dir() {
        let dir = scratch_dir("mkdir");
        assert!(!dir.exists());
        let req = ToastRequest {
            kind: ToastKind::Idle,
            session_id: "s1".into(),
            project: "quarterdeck".into(),
            detail: "Task".into(),
        };
        append_fake_call(&dir, &req, "t", "b", "Default").expect("append creates dir");
        assert!(dir.join("notifier-calls.jsonl").exists());
        fs::remove_dir_all(&dir).ok();
    }

    // --- data dir resolution ----------------------------------------------

    #[test]
    fn data_dir_env_override_wins() {
        // Serialize with the other cross-module `QUARTERDECK_DATA_DIR` mutator
        // (mcp_server's serve test) so the parallel harness can't race us.
        let _env = crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY (test-only): the lock above makes this the sole owner of the
        // env var for the whole body.
        let dir = scratch_dir("datadir-env");
        unsafe {
            std::env::set_var(DATA_DIR_ENV, &dir);
        }
        let resolved = data_dir();
        unsafe {
            std::env::remove_var(DATA_DIR_ENV);
        }
        assert_eq!(resolved, dir);
    }
}
