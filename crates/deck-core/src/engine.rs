//! The status engine: applies parsed events to session state and drives the
//! transition table in SPEC §2 (working / attention / idle / dead), including
//! the `attention -> working` recovery (R-2.2) and pending-ask forcing (R-2.4).
//!
//! Filled in by T1.
