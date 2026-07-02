//! Public trait stubs implemented by the Tauri shell (`src-tauri`) and by test
//! fakes. Keeping them here lets the engine depend only on these abstractions
//! while `deck-core` stays GUI-free (SPEC R-3.2).
//!
//! The traits deliberately carry no methods yet; T1/T3/T5 add them alongside the
//! engine that consumes them.

/// Fires native notifications. The real implementation lives in
/// `src-tauri/src/notify.rs`; tests provide a fake that records calls.
pub trait Notifier {}

/// Injectable time source so status transitions are deterministically testable
/// (SPEC §2 references an "injectable clock").
pub trait Clock {}

/// Abstraction over the OS process table used for liveness checks (SPEC §6).
/// The real implementation is backed by `sysinfo` in the shell; tests use a
/// fake table.
pub trait ProcessTable {}
