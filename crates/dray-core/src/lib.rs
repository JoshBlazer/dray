//! Domain types and the job state machine.
//!
//! This crate is deliberately free of I/O so the state machine can be tested
//! exhaustively as pure functions. See `DRAY_BUILD_SPEC.md` §4.2 and §5 Phase 2.
//!
//! Phase 0: skeleton only. The state machine lands in Phase 2.

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
