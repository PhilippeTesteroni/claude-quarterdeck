//! `deck-core` is the pure-Rust engine behind Quarterdeck.
//!
//! No Tauri or GUI dependencies live here (SPEC R-3.1). OS interactions are
//! expressed through the trait stubs in [`traits`] so the engine can be tested
//! with fakes (SPEC R-3.2).
//!
//! The module bodies below are scaffolded empty by T0 and filled in by later
//! implementation tasks (see `TASKS.md`):
//!
//! * [`events`], [`engine`], [`naming`], [`discovery`], [`liveness`] — T1
//! * [`hooks_config`] — T2
//! * [`ask`] — engine-side ask lifecycle (T1/T6 seam)

pub mod ask;
pub mod discovery;
pub mod engine;
pub mod events;
pub mod hooks_config;
pub mod liveness;
pub mod naming;
pub mod traits;
