//! Proving real jobs with a real `nargo` and a real `bb`.
//!
//! Gated behind the `proving-tests` feature so `cargo test` on a fresh clone
//! passes without the ZK toolchain. Like the store's Postgres tests, these fail
//! loudly rather than skipping when their prerequisites are missing: a proving
//! pipeline that silently ran no tests would look exactly like one that worked.
//!
//! Run with:
//!
//! ```sh
//! make setup-zk
//! cargo test -p dray-worker --features proving-tests
//! ```
//!
//! Nothing here uses a stub. The point of these tests is that the worker's
//! `Prover.toml`, the compiled circuit, the verification key, and Barretenberg
//! all agree — which is precisely the thing a mock cannot tell you.

#![cfg(feature = "proving-tests")]

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use dray_worker::{
    bounded::Bounds,
    prover::{self, Artifacts, ProveError, ProverConfig},
};
use serde_json::json;

/// Expected nullifiers for the committed reference witnesses.
///
/// These are the values the circuits' own `print_reference_witness` tests emit
/// and the values `e2e-circuits.sh` settles on chain. Asserting them here is
/// what makes this an end-to-end test rather than a "something came out"
/// test: the worker must produce the *same* proof the rest of the system was
/// built against.
const MEMBERSHIP_NULLIFIER: &str =
    "04eed209841c67fdb32a39da5ee53038c72465da539eaa32c5964797ba7ab646";
const RANGE_PROOF_NULLIFIER: &str =
    "2df5db123edce847869340551204d5b3d6c18588b8a904e6a803c43c694cd01c";

const REFERENCE_ROOT: &str = "0x089175ccc891f80d0f76bc5c6f7a239c2a78069ddf64478b68410c7d6b4c7320";

fn circuits_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/dray-worker.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../circuits")
        .canonicalize()
        .expect("the circuits workspace should exist")
}

fn membership_inputs() -> serde_json::Value {
    json!({
        "root": REFERENCE_ROOT,
        "secret": "42",
        "leaf_index": "5",
        "siblings": vec!["7"; 20],
    })
}

fn range_proof_inputs() -> serde_json::Value {
    json!({"min": "18", "max": "150", "value": "42", "secret": "12345"})
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A prepared artefact directory plus somewhere to put scratch, both removed
/// when the guard drops.
struct Harness {
    config: ProverConfig,
    _artifacts_dir: tempfile::TempDir,
    scratch_dir: tempfile::TempDir,
}

impl Harness {
    async fn build() -> Self {
        for tool in ["nargo", "bb"] {
            assert!(
                which(tool),
                "{tool} not found on PATH. These tests need the real toolchain: run `make setup-zk`"
            );
        }

        let artifacts_dir = tempfile::tempdir().expect("tempdir");
        let scratch_dir = tempfile::tempdir().expect("tempdir");

        let mut config = ProverConfig::new(
            Artifacts::at(artifacts_dir.path()),
            scratch_dir.path().to_path_buf(),
        );

        let circuits = ["membership".to_owned(), "range_proof".to_owned()];
        let artifacts = prover::prepare(&circuits_dir(), &circuits, artifacts_dir.path(), &config)
            .await
            .expect("preparing artefacts should succeed");

        config.artifacts = artifacts;

        Self {
            config,
            _artifacts_dir: artifacts_dir,
            scratch_dir,
        }
    }

    fn scratch_entries(&self) -> Vec<PathBuf> {
        std::fs::read_dir(self.scratch_dir.path())
            .expect("scratch root should be readable")
            .map(|e| e.expect("entry").path())
            .collect()
    }
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// The happy path, for both circuits
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn proves_the_membership_reference_witness() {
    let harness = Harness::build().await;

    let proven = prover::prove(
        "membership",
        &membership_inputs(),
        "job-membership",
        &harness.config,
    )
    .await
    .expect("membership should prove");

    assert_eq!(proven.proof.len(), 8384, "unexpected proof size");
    assert_eq!(
        proven.public_inputs.len(),
        64,
        "membership publishes (root, nullifier)"
    );

    let nullifier = proven.nullifier().expect("should have a nullifier");
    assert_eq!(
        hex(&nullifier),
        MEMBERSHIP_NULLIFIER,
        "the worker produced a different nullifier than the rest of the system settles"
    );

    // The root is the declared public parameter, so it comes first.
    assert_eq!(
        format!("0x{}", hex(&proven.public_inputs[..32])),
        REFERENCE_ROOT,
        "public input 0 should be the root"
    );

    assert!(proven.duration > Duration::ZERO);
}

#[tokio::test(flavor = "multi_thread")]
async fn proves_the_range_proof_reference_witness() {
    let harness = Harness::build().await;

    let proven = prover::prove(
        "range_proof",
        &range_proof_inputs(),
        "job-range",
        &harness.config,
    )
    .await
    .expect("range_proof should prove");

    assert_eq!(
        proven.public_inputs.len(),
        96,
        "range_proof publishes (min, max, nullifier)"
    );

    let nullifier = proven.nullifier().expect("should have a nullifier");
    assert_eq!(hex(&nullifier), RANGE_PROOF_NULLIFIER);

    // Reading the nullifier from the end is what makes this work across two
    // circuits with different public input counts (ADR-008).
    assert_ne!(
        hex(&proven.public_inputs[..32]),
        RANGE_PROOF_NULLIFIER,
        "the nullifier must not be first"
    );
}

/// The whole point of being circuit-agnostic: one worker, two circuits, no
/// per-circuit code path.
#[tokio::test(flavor = "multi_thread")]
async fn one_worker_proves_both_circuits() {
    let harness = Harness::build().await;

    let membership = prover::prove(
        "membership",
        &membership_inputs(),
        "both-a",
        &harness.config,
    )
    .await
    .expect("membership should prove");
    let range = prover::prove(
        "range_proof",
        &range_proof_inputs(),
        "both-b",
        &harness.config,
    )
    .await
    .expect("range_proof should prove");

    assert_ne!(
        membership.nullifier(),
        range.nullifier(),
        "distinct domain separators must give distinct nullifiers, or one \
         circuit would block the other in the shared nullifier set"
    );
}

// ---------------------------------------------------------------------------
// Failure paths
// ---------------------------------------------------------------------------

/// Inputs that do not satisfy the circuit must fail *permanently*. Retrying a
/// witness `nargo` has already rejected only burns a lease and a subprocess to
/// reach the same answer.
#[tokio::test(flavor = "multi_thread")]
async fn an_unsatisfiable_witness_fails_permanently() {
    let harness = Harness::build().await;

    let mut inputs = membership_inputs();
    inputs["root"] = json!("0x01");

    let err = prover::prove("membership", &inputs, "job-bad-root", &harness.config)
        .await
        .expect_err("a wrong root must not prove");

    assert_eq!(err.kind(), dray_core::FailureKind::Permanent, "{err}");
    assert!(
        matches!(err, ProveError::Witness(_)),
        "should fail at witness generation, not proving: {err}"
    );
    assert!(
        err.to_string().contains("merkle root mismatch"),
        "the circuit's own assertion should reach the operator: {err}"
    );
}

/// A missing field is caught by `nargo`, not by the worker — the API's schema
/// validation is the first line of defence, and this is the second.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_input_fails_permanently() {
    let harness = Harness::build().await;

    let mut inputs = membership_inputs();
    inputs.as_object_mut().expect("object").remove("secret");

    let err = prover::prove("membership", &inputs, "job-missing", &harness.config)
        .await
        .expect_err("a missing input must not prove");

    assert_eq!(err.kind(), dray_core::FailureKind::Permanent, "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unregistered_circuit_is_refused_before_any_work() {
    let harness = Harness::build().await;

    let err = prover::prove(
        "no_such_circuit",
        &json!({}),
        "job-unknown",
        &harness.config,
    )
    .await
    .expect_err("an unknown circuit must not prove");

    assert!(matches!(err, ProveError::UnknownCircuit(_)), "{err}");
    assert!(
        harness.scratch_entries().is_empty(),
        "nothing should have been created for an unknown circuit"
    );
}

/// A wall clock short enough that even witness generation cannot finish. The
/// job must fail transiently — the bound was hit, not the circuit — and the
/// worker must stay usable afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn a_bound_that_is_too_tight_fails_transiently_and_cleans_up() {
    let mut harness = Harness::build().await;
    harness.config.bounds = Bounds {
        wall_clock: Duration::from_millis(50),
        ..Bounds::for_proving()
    };

    let err = prover::prove(
        "membership",
        &membership_inputs(),
        "job-tight",
        &harness.config,
    )
    .await
    .expect_err("a 50ms wall clock cannot prove anything");

    assert_eq!(err.kind(), dray_core::FailureKind::Transient, "{err}");
    assert_eq!(err.metric_label(), "timeout");

    assert!(
        harness.scratch_entries().is_empty(),
        "scratch survived a timeout: {:?}",
        harness.scratch_entries()
    );

    // The worker is still able to do useful work with sane bounds.
    harness.config.bounds = Bounds::for_proving();
    prover::prove(
        "membership",
        &membership_inputs(),
        "job-after",
        &harness.config,
    )
    .await
    .expect("the worker should still prove after a bound was hit");
}

// ---------------------------------------------------------------------------
// Isolation
// ---------------------------------------------------------------------------

/// Scratch must not survive success either, or a busy worker fills its disk and
/// then fails every job it touches.
#[tokio::test(flavor = "multi_thread")]
async fn scratch_is_removed_after_a_successful_proof() {
    let harness = Harness::build().await;

    prover::prove(
        "membership",
        &membership_inputs(),
        "job-clean",
        &harness.config,
    )
    .await
    .expect("should prove");

    assert!(
        harness.scratch_entries().is_empty(),
        "scratch survived: {:?}",
        harness.scratch_entries()
    );
}

/// The reason each job copies the package: two jobs proving the same circuit
/// concurrently must not share a `Prover.toml` or a witness file. If they did,
/// the failure would not be a crash — it would be a proof recorded against the
/// wrong job's inputs, which is far worse.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_jobs_on_one_circuit_do_not_share_a_witness() {
    let harness = Harness::build().await;

    // Same circuit, different secrets, so a shared witness would show up as
    // two identical nullifiers.
    let mut first_inputs = membership_inputs();
    first_inputs["secret"] = json!("42");

    let mut second_inputs = membership_inputs();
    second_inputs["secret"] = json!("43");
    // Leaf 43 lives under a different root; compute nothing here, just accept
    // that this one fails. What matters is that it fails *on its own inputs*.
    let (first, second) = tokio::join!(
        prover::prove("membership", &first_inputs, "race-a", &harness.config),
        prover::prove("membership", &second_inputs, "race-b", &harness.config),
    );

    let first = first.expect("the valid witness should prove");
    assert_eq!(
        hex(&first.nullifier().expect("nullifier")),
        MEMBERSHIP_NULLIFIER,
        "a concurrent job corrupted this one's witness"
    );

    let err = second.expect_err("secret 43 is not in the tree at this root");
    assert!(
        err.to_string().contains("merkle root mismatch"),
        "the second job should fail on its own inputs, not on the first's: {err}"
    );

    assert!(harness.scratch_entries().is_empty(), "scratch leaked");
}

/// The client's private inputs must never be written into the repository —
/// only into scratch, which is removed.
#[tokio::test(flavor = "multi_thread")]
async fn proving_does_not_write_into_the_circuits_workspace() {
    let harness = Harness::build().await;

    let committed = circuits_dir().join("membership/Prover.toml");
    let before = std::fs::read(&committed).expect("the reference witness should exist");

    let secret = "31337";
    let mut inputs = membership_inputs();
    inputs["secret"] = json!(secret);
    let _ = prover::prove("membership", &inputs, "job-nowrite", &harness.config).await;

    let after = std::fs::read(&committed).expect("still readable");
    assert_eq!(before, after, "the committed Prover.toml was modified");

    let text = String::from_utf8_lossy(&after);
    assert!(
        !text.contains(secret),
        "a client secret was written into the repository"
    );
}
