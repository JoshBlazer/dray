//! Proving worker.
//!
//! Leases jobs with a TTL, generates proofs by invoking `nargo`/`bb` in a
//! subprocess bounded by a wall-clock timeout, a memory ceiling, and a CPU
//! quota, then reports the result. Exceeding a bound is a normal, recoverable
//! failure. See `DRAY_BUILD_SPEC.md` §4.3(c) and §5 Phase 3.
//!
//! Phase 0: skeleton only. No leasing or proving yet.

/// Name of this component, as it appears in logs, metrics, and traces.
const COMPONENT: &str = "dray-worker";

fn main() {
    println!(
        "{COMPONENT} {} — Phase 0 skeleton, not yet leasing. Depends on {} and {}.",
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
        assert_eq!(COMPONENT, "dray-worker");
    }
}
