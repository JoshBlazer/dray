//! PostgreSQL and Redis adapters.
//!
//! Postgres is the durable source of truth for job state; every transition
//! happens inside a transaction. Redis holds leases and other ephemeral
//! coordination state and may be rebuilt from Postgres at any time — it is
//! never truth. See `DRAY_BUILD_SPEC.md` §4.2.
//!
//! Phase 0: skeleton only. Schema and adapters land in Phase 2.

/// Name of this component, as it appears in logs, metrics, and traces.
pub const COMPONENT: &str = "dray-store";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_is_named() {
        assert_eq!(COMPONENT, "dray-store");
    }

    #[test]
    fn links_against_core() {
        assert_eq!(dray_core::COMPONENT, "dray-core");
    }
}
