//! Native notifications (SPEC §9): the two toast classes (standard / alert),
//! distinct system sounds, stable `AppUserModelID` (R-9.3), throttling (R-9.4),
//! and the `QUARTERDECK_FAKE_NOTIFIER=1` fake mode that appends calls to
//! `<data>/notifier-calls.jsonl` (R-3.2). Implements
//! [`deck_core::traits::Notifier`].
//!
//! Filled in by T5.
