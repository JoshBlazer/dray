//! Chain submission relayer.
//!
//! Single writer to its own account nonce. Serialises submissions, applies a
//! gas policy with bump-and-replace on stuck transactions, and tracks
//! confirmations to N blocks before marking a job settled. See
//! `DRAY_BUILD_SPEC.md` §4.3(d) and §5 Phase 4.
//!
//! Phase 0: skeleton only. No chain connection yet.

/// Name of this component, as it appears in logs, metrics, and traces.
const COMPONENT: &str = "dray-relayer";

fn main() {
    println!(
        "{COMPONENT} {} — Phase 0 skeleton, not yet submitting. Depends on {} and {}.",
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
        assert_eq!(COMPONENT, "dray-relayer");
    }
}
