//! Operator CLI (`dray`).
//!
//! Inspect a job, replay a failed job, drain a worker, show queue stats.
//! See `DRAY_BUILD_SPEC.md` §5 Phase 5.
//!
//! Phase 0: skeleton only. No subcommands yet.

/// Name of this component, as it appears in logs, metrics, and traces.
const COMPONENT: &str = "dray-cli";

fn main() {
    println!(
        "dray {} ({COMPONENT}) — Phase 0 skeleton, no subcommands yet. Depends on {} and {}.",
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
        assert_eq!(COMPONENT, "dray-cli");
    }
}
