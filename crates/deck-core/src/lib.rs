//! `deck-core` is the pure-Rust engine behind Quarterdeck.
//!
//! No Tauri or GUI dependencies live here (SPEC R-3.1). OS interactions are
//! expressed through the trait stubs in [`traits`] so the engine can be tested
//! with fakes (SPEC R-3.2).
//!
//! Module map:
//!
//! * [`events`], [`engine`], [`naming`], [`discovery`], [`liveness`] — event
//!   ingestion, the status engine, session naming, cold-start discovery, liveness
//! * [`hooks_config`] — Claude Code hook install/uninstall
//! * [`ask`] — engine-side ask lifecycle

pub mod ask;
pub mod discovery;
pub mod engine;
pub mod events;
pub mod hooks_config;
pub mod liveness;
pub mod naming;
pub mod registry;
pub mod traits;
pub mod usage;
