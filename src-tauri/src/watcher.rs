//! Spool watcher: a `notify`-rs file watcher that streams debounced spool file
//! paths to a channel for the engine to consume (SPEC §3.1, §3.5).
//!
//! Filled in by T3.

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// Default debounce window between a spool-directory change and the moment
/// its path is forwarded downstream: long enough to coalesce the burst of
/// events a single atomic tmp-then-rename write produces, short enough to
/// stay well inside the hooks' 2s "typical" budget (SPEC R-4.3).
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(250);

/// A running spool watcher: keeps the underlying OS watcher alive for as long
/// as it's held, and exposes a channel of debounced, deduplicated file paths.
pub struct SpoolWatcher {
    // Never read directly — kept alive so the OS watch isn't torn down.
    _watcher: RecommendedWatcher,
    /// Debounced spool file paths, one per distinct path per quiet period.
    pub paths: Receiver<PathBuf>,
}

impl SpoolWatcher {
    /// Watches `dir` non-recursively and forwards changed file paths,
    /// coalesced over `debounce`, to the returned channel. Creates `dir` if
    /// it doesn't exist yet (a fresh install has no spool directory).
    pub fn spawn(dir: &Path, debounce: Duration) -> notify::Result<Self> {
        std::fs::create_dir_all(dir).map_err(notify::Error::io)?;

        let (raw_tx, raw_rx) = channel::<PathBuf>();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let Ok(event) = res else { return };
            if !is_relevant(&event.kind) {
                return;
            }
            for path in event.paths {
                let _ = raw_tx.send(path);
            }
        })?;
        watcher.watch(dir, RecursiveMode::NonRecursive)?;

        let (out_tx, out_rx) = channel::<PathBuf>();
        thread::Builder::new()
            .name("quarterdeck-spool-watcher-debounce".to_string())
            .spawn(move || debounce_loop(raw_rx, out_tx, debounce))
            .expect("failed to spawn the spool watcher debounce thread");

        Ok(Self {
            _watcher: watcher,
            paths: out_rx,
        })
    }

    /// Convenience wrapper watching `<data>/spool/` with [`DEFAULT_DEBOUNCE`]
    /// (SPEC §3.3 layout). T7 owns actually spawning this at startup.
    pub fn spawn_for_spool_dir() -> notify::Result<Self> {
        Self::spawn(&crate::settings::spool_dir(), DEFAULT_DEBOUNCE)
    }
}

/// Only file arrivals/changes carry new spool (or answer) data to ingest.
///
/// `Access` events are pure noise. `Remove` events are excluded specifically to
/// avoid a self-echo loop (SPEC R-3.5): the engine *deletes* every spool file it
/// consumes and *moves* every malformed file to quarantine, and those deletions
/// are themselves reported on the watched directory. Forwarding them re-queues a
/// now-nonexistent path, whose `NotFound` read turns into a bogus "malformed
/// spool file" quarantine+log — indistinguishable from a real malformed hook
/// payload, defeating the diagnostic value of that log. A removed file can never
/// be ingested, so dropping `Remove` loses nothing.
fn is_relevant(kind: &EventKind) -> bool {
    !matches!(kind, EventKind::Access(_) | EventKind::Remove(_))
}

fn debounce_loop(raw_rx: Receiver<PathBuf>, out_tx: Sender<PathBuf>, debounce: Duration) {
    let mut pending: HashSet<PathBuf> = HashSet::new();
    let mut deadline: Option<Instant> = None;

    loop {
        let timeout = match deadline {
            Some(d) => d
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1)),
            // No pending events: block "indefinitely" (bounded so the loop
            // still notices a disconnected sender promptly enough in tests).
            None => Duration::from_secs(3600),
        };

        match raw_rx.recv_timeout(timeout) {
            Ok(path) => {
                pending.insert(path);
                deadline = Some(Instant::now() + debounce);
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(d) = deadline {
                    if Instant::now() >= d {
                        if !flush(&mut pending, &out_tx) {
                            return;
                        }
                        deadline = None;
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                flush(&mut pending, &out_tx);
                return;
            }
        }
    }
}

/// Sends every pending path once and clears the set. Returns `false` if the
/// receiving end is gone, signalling the debounce thread to stop.
fn flush(pending: &mut HashSet<PathBuf>, out_tx: &Sender<PathBuf>) -> bool {
    for path in pending.drain() {
        if out_tx.send(path).is_err() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "quarterdeck-watcher-test-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Drains everything currently queued (blocking briefly for the first
    /// item, then draining without blocking) into a sorted, deduplicated Vec
    /// of file names — easier to assert against than raw paths.
    fn drain_names(rx: &Receiver<PathBuf>, first_wait: Duration) -> Vec<String> {
        let mut names = Vec::new();
        if let Ok(path) = rx.recv_timeout(first_wait) {
            names.push(path.file_name().unwrap().to_string_lossy().to_string());
        }
        while let Ok(path) = rx.recv_timeout(Duration::from_millis(50)) {
            names.push(path.file_name().unwrap().to_string_lossy().to_string());
        }
        names.sort();
        names
    }

    #[test]
    fn forwards_a_created_file_after_the_debounce_window() {
        let dir = unique_dir("create");
        let watcher = SpoolWatcher::spawn(&dir, Duration::from_millis(50)).unwrap();

        fs::write(dir.join("event-1.json"), b"{}").unwrap();

        let names = drain_names(&watcher.paths, Duration::from_secs(3));
        assert_eq!(names, vec!["event-1.json".to_string()]);
    }

    #[test]
    fn coalesces_repeated_writes_to_the_same_path_into_one_notification() {
        let dir = unique_dir("coalesce");
        let watcher = SpoolWatcher::spawn(&dir, Duration::from_millis(150)).unwrap();

        let target = dir.join("event-1.json");
        for i in 0..3 {
            fs::write(&target, format!("{{\"n\":{i}}}")).unwrap();
            thread::sleep(Duration::from_millis(20));
        }

        let names = drain_names(&watcher.paths, Duration::from_secs(3));
        assert_eq!(names, vec!["event-1.json".to_string()]);
    }

    #[test]
    fn reports_multiple_distinct_paths_written_within_one_window() {
        let dir = unique_dir("multi");
        let watcher = SpoolWatcher::spawn(&dir, Duration::from_millis(150)).unwrap();

        fs::write(dir.join("event-a.json"), b"{}").unwrap();
        fs::write(dir.join("event-b.json"), b"{}").unwrap();

        let names = drain_names(&watcher.paths, Duration::from_secs(3));
        assert_eq!(
            names,
            vec!["event-a.json".to_string(), "event-b.json".to_string()]
        );
    }

    #[test]
    fn spawn_creates_a_missing_spool_directory() {
        let dir = std::env::temp_dir().join(format!(
            "quarterdeck-watcher-test-missing-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        assert!(!dir.exists());

        let _watcher = SpoolWatcher::spawn(&dir, DEFAULT_DEBOUNCE).unwrap();
        assert!(dir.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_relevant_ignores_access_and_remove_events() {
        assert!(!is_relevant(&EventKind::Access(
            notify::event::AccessKind::Any
        )));
        // Remove is dropped so the engine's own consume/quarantine deletions
        // don't self-echo as bogus "malformed" events (SPEC R-3.5).
        assert!(!is_relevant(&EventKind::Remove(
            notify::event::RemoveKind::Any
        )));
        assert!(is_relevant(&EventKind::Create(
            notify::event::CreateKind::Any
        )));
        assert!(is_relevant(&EventKind::Modify(
            notify::event::ModifyKind::Any
        )));
        assert!(is_relevant(&EventKind::Any));
    }
}
