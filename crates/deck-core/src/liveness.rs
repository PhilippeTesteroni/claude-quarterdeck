//! Liveness checks (SPEC §6): PID-backed sessions verified against a
//! [`crate::traits::ProcessTable`] every 10 s; inferred sessions expire when
//! their transcript is untouched for more than 6 h.
//!
//! Filled in by T1.
