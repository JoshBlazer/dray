//! Thin binary over [`dray_worker`].
//!
//! Everything of substance lives in the library so the integration tests can
//! drive a worker in-process (ADR-006). The lease loop lands here next.

fn main() {
    println!("{} skeleton", dray_worker::COMPONENT);
}
