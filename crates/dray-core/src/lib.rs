//! Domain types and the job state machine.
//!
//! This crate is deliberately free of I/O so the state machine can be tested
//! exhaustively as pure functions. See `DRAY_BUILD_SPEC.md` §4.2 and §5 Phase 2.
//!
//! Two things live here, and both exist to make a guarantee elsewhere possible:
//!
//! - [`job`] — the state machine, which is what makes "no job is lost" checkable.
//! - [`canonical`] — input canonicalisation and job identity, which is what
//!   makes idempotency possible.

pub mod canonical;
pub mod job;

pub use canonical::{CanonicalError, canonicalise, job_hash, job_hash_hex};
pub use job::{
    FailureKind, IllegalTransition, JobEvent, JobState, can_transition, classify_failure,
    transition,
};

/// Name of this component, as it appears in logs, metrics, and traces.
pub const COMPONENT: &str = "dray-core";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_is_named() {
        assert_eq!(COMPONENT, "dray-core");
    }
}
