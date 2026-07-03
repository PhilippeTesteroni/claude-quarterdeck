//! Manual notification demo (SPEC §9 / T5 acceptance criteria).
//!
//! Fires one `Idle`-class toast (system default sound), waits, then fires one
//! `Attention`-class toast (distinct alert sound) so a human can confirm on
//! this machine that both classes actually appear with different sounds.
//! Not part of the automated test suite — run manually via
//! `scripts/demo-toasts.cmd` (handles two Windows-only `cargo run --example`
//! quirks, see that script's comments) or directly with:
//!
//! ```text
//! cd src-tauri
//! cargo run --example demo_toasts
//! ```
//!
//! Set `QUARTERDECK_FAKE_NOTIFIER=1` to instead append the same two calls to
//! `<data>/notifier-calls.jsonl` without showing real toasts (R-3.2).
//!
//! Fires the toasts from a background thread spawned in `setup` (so the
//! toasts fire once, deterministically, without depending on any window
//! event) and exits via `std::process::exit` once done: `Builder::build`
//! alone (skipping `run`) was observed to hang indefinitely creating the
//! webview with no event loop pumping messages, and `AppHandle::exit`'s async
//! `RunEvent::ExitRequested` round trip was observed to not reliably
//! terminate the process either — a direct `std::process::exit` after `run`
//! has started is the combination that was verified to work.

use std::io::Write;
use std::time::Duration;

use quarterdeck_lib::notify::{compose, DesktopNotifier, ToastKind, ToastRequest};

/// `std::process::exit` (used below for a guaranteed, immediate exit) skips
/// the normal stdio teardown, so a redirected/piped stdout can otherwise lose
/// buffered output. Flush explicitly after anything worth seeing.
fn flushed_println(s: &str) {
    println!("{s}");
    let _ = std::io::stdout().flush();
}

fn fire_demo_toasts(app: tauri::AppHandle) {
    let notifier = DesktopNotifier::new(app);

    let idle = ToastRequest {
        kind: ToastKind::Idle,
        session_id: "demo-idle".into(),
        project: "Quarterdeck".into(),
        detail: "Demo task: write the T5 report".into(),
        assistant_body: None,
    };
    let (title, body) = compose(&idle);
    flushed_println("[1/2] Firing IDLE toast (system default sound):");
    flushed_println(&format!("      title: {title:?}"));
    flushed_println(&format!("      body:  {body:?}"));
    let sent = notifier.send(idle, false);
    flushed_println(&format!("      sent = {sent}"));

    flushed_println("Waiting 5s so the idle toast is visible before the next one...");
    std::thread::sleep(Duration::from_secs(5));

    let attention = ToastRequest {
        kind: ToastKind::Attention,
        session_id: "demo-attention".into(),
        project: "Quarterdeck".into(),
        detail: "Allow Bash to run `rm -rf build`?".into(),
        assistant_body: None,
    };
    let (title, body) = compose(&attention);
    flushed_println("[2/2] Firing ATTENTION toast (distinct alert sound):");
    flushed_println(&format!("      title: {title:?}"));
    flushed_println(&format!("      body:  {body:?}"));
    let sent = notifier.send(attention, false);
    flushed_println(&format!("      sent = {sent}"));

    flushed_println("Waiting 5s so the attention toast is visible, then exiting...");
    std::thread::sleep(Duration::from_secs(5));
    flushed_println("Demo done, exiting.");
    std::process::exit(0);
}

fn main() {
    // Surface any `tracing::warn!` from notify.rs (e.g. a failed `.show()`
    // call) on stdout for this manual run.
    tracing_subscriber::fmt::init();

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let handle = app.handle().clone();
            std::thread::spawn(move || fire_demo_toasts(handle));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the demo-toasts example");
}
