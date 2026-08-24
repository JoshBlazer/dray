//! Chain submission: nonce management, gas policy, and confirmation tracking.
//!
//! A library with a thin binary over it (ADR-006), so the integration tests can
//! drive a relayer in-process against Anvil — forcing a reorg and watching the
//! relayer respond needs control over its lifetime, not just its output.

pub mod chain;
pub mod failure;
pub mod gas;
pub mod nonce;

/// Component name used in logs and metrics.
pub const COMPONENT: &str = "dray-relayer";
