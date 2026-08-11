//! Ingest HTTP API.
//!
//! Accepts proof requests, validates them against the target circuit's declared
//! input schema, canonicalises and hashes the inputs for deduplication, and
//! enqueues durable jobs. See `DRAY_BUILD_SPEC.md` §5 Phase 2.
//!
//! Phase 0: skeleton only. No HTTP server yet.

/// Name of this component, as it appears in logs, metrics, and traces.
const COMPONENT: &str = "dray-api";

fn main() {
    println!(
        "{COMPONENT} {} — Phase 0 skeleton, not yet serving. Depends on {} and {}.",
        env!("CARGO_PKG_VERSION"),
        dray_core::COMPONENT,
        dray_store::COMPONENT,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_is_named() {
        assert_eq!(COMPONENT, "dray-api");
    }
}
