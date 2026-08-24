//! Proving worker: leases jobs, runs `nargo`/`bb` under strict resource
//! bounds, and records the result.
//!
//! The crate is a library with a thin binary over it (ADR-006) so that the
//! integration tests can drive a worker directly — the chaos test has to be
//! able to kill one mid-proof, which means owning its lifetime rather than
//! shelling out to a process it can only observe from outside.
//!
//! The pieces, in the order a job meets them:
//!
//! 1. [`prover`] turns validated job inputs into a proof, in a scratch
//!    directory that is removed on every exit path.
//! 2. [`bounded`] runs each subprocess under a wall clock, an address-space
//!    ceiling, and a CPU quota, so a pathological input is a metered failure
//!    rather than a dead machine.
//! 3. [`backoff`] decides how long to wait before a retryable failure is tried
//!    again, with jitter, so a shared outage does not synchronise the fleet.

pub mod backoff;
pub mod bounded;
pub mod config;
pub mod metrics;
pub mod prover;
mod task;
pub mod worker;

/// Component name used in logs and metrics.
pub const COMPONENT: &str = "dray-worker";
