//! Ingest HTTP API.
//!
//! Accepts proof requests, validates them against the target circuit's declared
//! input schema, canonicalises and hashes the inputs for deduplication, and
//! enqueues durable jobs. See `DRAY_BUILD_SPEC.md` §5 Phase 2.
//!
//! The crate is a library with a thin binary on top so that integration tests
//! can build the router and drive it directly, without a socket.

pub mod api;
pub mod config;
pub mod validate;

/// Name of this component, as it appears in logs, metrics, and traces.
pub const COMPONENT: &str = "dray-api";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_is_named() {
        assert_eq!(COMPONENT, "dray-api");
    }
}
