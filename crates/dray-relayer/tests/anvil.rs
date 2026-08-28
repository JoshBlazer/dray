//! The relayer against a real chain.
//!
//! Anvil, a real deployment of `DraySettlement`, real proofs from
//! `circuits/target/`, and a real Postgres. Gated behind `integration-tests`
//! like the rest of the workspace: a suite that silently skipped would look
//! exactly like one that passed.
//!
//! Run with:
//!
//! ```sh
//! make up && make setup-zk && make prove
//! DATABASE_URL=postgres://dray:dray@localhost:5432/dray_test \
//!     cargo test -p dray-relayer --features integration-tests
//! ```
//!
//! Anvil gets a fresh port per test and is killed on drop, so tests do not
//! share a chain. They must not: consuming a nullifier is global, and two tests
//! sharing a chain would settle each other's proofs.

#![cfg(feature = "integration-tests")]

use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU16, Ordering},
    time::Duration,
};

use alloy::{
    primitives::{Address, B256, U256},
    providers::{Provider, ProviderBuilder},
};
use dray_core::JobState;
use dray_relayer::{
    chain::Chain,
    relayer::{Outcome, Relayer, RelayerConfig, shutdown},
};
use dray_store::{Circuit, Store};
use serde_json::json;

/// Anvil's well-known development accounts. Published in its own documentation
/// and worthless; they must never be used anywhere but a local chain.
const OWNER_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const RELAYER_KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const RELAYER_ADDRESS: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
const SECOND_RELAYER_KEY: &str =
    "0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
const SECOND_RELAYER_ADDRESS: &str = "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC";
/// A funded account that is deliberately *not* authorised as a relayer.
const STRANGER_KEY: &str = "0x7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6";

static NEXT_PORT: AtomicU16 = AtomicU16::new(9600);

/// Relayer logs on demand: `RUST_LOG=dray_relayer=debug cargo test ... -- --nocapture`.
fn init_tracing() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
    });
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root should exist")
}

fn database_url() -> String {
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run the integration tests")
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// An Anvil process, killed when the guard drops.
struct Anvil {
    child: Child,
    port: u16,
}

impl Anvil {
    async fn start() -> Self {
        let port = NEXT_PORT.fetch_add(1, Ordering::SeqCst);

        let child = Command::new("anvil")
            .args(["--port", &port.to_string(), "--silent"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("anvil should start; run `make setup-zk`");

        let anvil = Self { child, port };

        // Wait for it to answer, rather than sleeping a guessed interval.
        let mut last_error = String::from("never attempted");
        for _ in 0..100 {
            match ProviderBuilder::new().connect(&anvil.rpc_url()).await {
                Ok(provider) => match provider.get_block_number().await {
                    Ok(_) => return anvil,
                    Err(err) => last_error = format!("get_block_number: {err}"),
                },
                Err(err) => last_error = format!("connect: {err}"),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("anvil did not become ready on port {port}: {last_error}");
    }

    fn rpc_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for Anvil {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A chain, a deployment, a database, and the means to make proved jobs.
struct Harness {
    anvil: Anvil,
    settlement: Address,
    store: Store,
}

impl Harness {
    async fn build(label: &str) -> Self {
        init_tracing();

        for tool in ["anvil", "forge"] {
            assert!(
                which(tool),
                "{tool} not found on PATH. These tests need Foundry: run `make setup-zk`"
            );
        }
        assert!(
            repo_root()
                .join("circuits/target/membership/proof")
                .is_file(),
            "no proofs on disk. Run `make prove` first — these tests settle real proofs"
        );

        let anvil = Anvil::start().await;
        let settlement = deploy(&anvil.rpc_url());
        let store = isolated_store(label).await;

        for (id, name) in [
            ("membership", "Merkle membership"),
            ("range_proof", "Range proof"),
        ] {
            store
                .upsert_circuit(&Circuit {
                    id: id.into(),
                    display_name: name.into(),
                    input_schema: json!({"type": "object"}),
                    verifier_address: Some(format!("{settlement:?}")),
                    enabled: true,
                })
                .await
                .expect("registering the circuit should succeed");
        }

        Self {
            anvil,
            settlement,
            store,
        }
    }

    /// Insert a job already in `proved`, carrying a real proof from disk.
    ///
    /// `variant` only varies the canonical inputs, so each call is a distinct
    /// job. The proof itself is the committed reference witness, which is what
    /// makes these settlements real.
    async fn proved_job(&self, circuit: &str, variant: u64) -> uuid::Uuid {
        let root = repo_root().join("circuits/target").join(circuit);
        let proof = std::fs::read(root.join("proof")).expect("proof on disk");
        let public_inputs = std::fs::read(root.join("public_inputs")).expect("public inputs");

        let (job, _) = self
            .store
            .enqueue(circuit, &json!({"fixture": variant}), None, 3)
            .await
            .expect("enqueue");

        self.store
            .lease_next("worker-1", Duration::from_secs(60))
            .await
            .expect("lease")
            .expect("a job to lease");
        self.store
            .begin_proving(job.id, "worker-1")
            .await
            .expect("begin proving");
        self.store
            .record_proof(job.id, "worker-1", &proof, &public_inputs, 2500, None)
            .await
            .expect("record proof");

        job.id
    }

    async fn relayer(&self, key: &str, id: &str) -> Relayer {
        let chain = Chain::connect(&self.anvil.rpc_url(), key, self.settlement)
            .await
            .expect("should connect to anvil");
        Relayer::new(self.store.clone(), chain, self.config(id))
    }

    fn config(&self, id: &str) -> RelayerConfig {
        let mut config = RelayerConfig::new(id);
        // Anvil auto-mines, so one confirmation is immediate.
        config.confirmations = 1;
        config.confirm_poll_interval = Duration::from_millis(50);
        config.poll_interval = Duration::from_millis(50);
        config.reap_interval = Duration::from_millis(200);
        // Short, so the nonce-gap and stuck-transaction paths are reachable
        // inside a test rather than after half a minute of waiting.
        config.stuck_after = Duration::from_secs(3);
        config.heartbeat_interval = Duration::from_secs(2);
        config
    }

    async fn provider(&self) -> impl Provider {
        ProviderBuilder::new()
            .connect(&self.anvil.rpc_url())
            .await
            .expect("should connect")
    }

    /// Ask the chain directly, rather than trusting the relayer's own record.
    async fn nullifier_used(&self, nullifier: B256) -> bool {
        Chain::connect(&self.anvil.rpc_url(), OWNER_KEY, self.settlement)
            .await
            .expect("connect")
            .nullifier_used(nullifier)
            .await
            .expect("query")
    }

    async fn nullifier_of(&self, id: uuid::Uuid) -> B256 {
        let job = self.store.job(id).await.expect("lookup").expect("job");
        let bytes = job.public_inputs.expect("public inputs");
        let inputs = dray_relayer::chain::public_inputs_from_bytes(&bytes).expect("well formed");
        *inputs.last().expect("at least one public input")
    }
}

/// Deploy the settlement stack with `forge script`, exactly as an operator
/// would, and read the address back out of its log.
fn deploy(rpc_url: &str) -> Address {
    let output = Command::new("forge")
        .current_dir(repo_root().join("contracts"))
        .args([
            "script",
            "script/Deploy.s.sol:Deploy",
            "--rpc-url",
            rpc_url,
            "--broadcast",
            "--skip-simulation",
        ])
        .env("PRIVATE_KEY", OWNER_KEY)
        .env(
            "DRAY_RELAYERS",
            format!("{RELAYER_ADDRESS},{SECOND_RELAYER_ADDRESS}"),
        )
        .output()
        .expect("forge script should run");

    let log = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "deployment failed:\n{log}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let address = log
        .lines()
        .find_map(|line| line.trim().strip_prefix("DRAY_SETTLEMENT="))
        .unwrap_or_else(|| panic!("no settlement address in the deploy log:\n{log}"));

    address.trim().parse().expect("a valid address")
}

async fn isolated_store(label: &str) -> Store {
    let admin_url = database_url();
    let db = format!("dray_relayer_{}_{label}", std::process::id());

    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("could not connect to Postgres");
    sqlx::query(&format!(r#"DROP DATABASE IF EXISTS "{db}" WITH (FORCE)"#))
        .execute(&admin)
        .await
        .expect("drop");
    sqlx::query(&format!(r#"CREATE DATABASE "{db}""#))
        .execute(&admin)
        .await
        .expect("create");
    admin.close().await;

    let (prefix, _) = admin_url
        .rsplit_once('/')
        .expect("DATABASE_URL should contain a database name");
    let store = Store::connect(&format!("{prefix}/{db}"), 8)
        .await
        .expect("connect");
    store.migrate().await.expect("migrations");
    store
}

/// Run a relayer until `id` reaches a terminal state, then stop it.
async fn settle_one(relayer: &Relayer, store: &Store, id: uuid::Uuid) -> Vec<Outcome> {
    run_until(relayer, store, id, |state| {
        matches!(state, JobState::Settled | JobState::Failed)
    })
    .await
}

async fn run_until(
    relayer: &Relayer,
    store: &Store,
    id: uuid::Uuid,
    done: fn(JobState) -> bool,
) -> Vec<Outcome> {
    let (handle, signal) = shutdown();

    let watcher = {
        let store = store.clone();
        tokio::spawn(async move {
            for _ in 0..1200 {
                let job = store.job(id).await.expect("lookup").expect("job");
                if done(job.state) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            handle.trigger();
        })
    };

    let outcomes = tokio::time::timeout(Duration::from_secs(180), relayer.run(signal))
        .await
        .expect("the relayer did not stop within the deadline");
    watcher.await.expect("watcher");
    outcomes
}

// ---------------------------------------------------------------------------
// The happy path
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_proof_is_settled_on_chain_and_recorded() {
    let harness = Harness::build("happy").await;
    let id = harness.proved_job("membership", 1).await;
    let nullifier = harness.nullifier_of(id).await;

    assert!(
        !harness.nullifier_used(nullifier).await,
        "the nullifier should be unspent before we start"
    );

    let relayer = harness.relayer(RELAYER_KEY, "relayer-1").await;
    relayer.preflight().await.expect("should be authorised");

    let outcomes = settle_one(&relayer, &harness.store, id).await;
    assert_eq!(outcomes, vec![Outcome::Settled(id)], "{outcomes:?}");

    let job = harness.store.job(id).await.unwrap().unwrap();
    assert_eq!(job.state, JobState::Settled);
    assert!(job.leased_by.is_none(), "a settled job is nobody's work");

    // The chain agrees, independently of what the relayer recorded.
    assert!(
        harness.nullifier_used(nullifier).await,
        "the nullifier was not consumed on chain"
    );

    let settlement = harness
        .store
        .latest_settlement(id)
        .await
        .unwrap()
        .expect("a settlement should be recorded");
    assert_eq!(settlement.tx_hash.len(), 32);
    assert!(settlement.block_number.is_some_and(|n| n > 0));
    assert!(
        settlement.gas_used.is_some_and(|gas| gas > 100_000),
        "verifying a Honk proof should cost real gas: {:?}",
        settlement.gas_used
    );
    assert!(settlement.reorged_at.is_none());

    eprintln!(
        "settled in block {:?} using {:?} gas",
        settlement.block_number, settlement.gas_used
    );
}

/// Being circuit-agnostic has to hold through the relayer too: one relayer, two
/// circuits, no per-circuit code path.
#[tokio::test(flavor = "multi_thread")]
async fn one_relayer_settles_both_circuits() {
    let harness = Harness::build("both").await;
    let membership = harness.proved_job("membership", 1).await;
    let range = harness.proved_job("range_proof", 2).await;

    let relayer = harness.relayer(RELAYER_KEY, "relayer-1").await;
    settle_one(&relayer, &harness.store, membership).await;
    settle_one(&relayer, &harness.store, range).await;

    for id in [membership, range] {
        let job = harness.store.job(id).await.unwrap().unwrap();
        assert_eq!(job.state, JobState::Settled, "job {id} did not settle");
        assert!(harness.nullifier_used(harness.nullifier_of(id).await).await);
    }
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

/// At-least-once delivery makes this the normal case, not an attack: the same
/// proof reaches the relayer twice. It must settle once and recognise the
/// second as already done — not fail it, and not pay to submit it again.
#[tokio::test(flavor = "multi_thread")]
async fn a_proof_that_is_already_settled_is_recognised_not_resubmitted() {
    let harness = Harness::build("replay").await;
    let first = harness.proved_job("membership", 1).await;

    let relayer = harness.relayer(RELAYER_KEY, "relayer-1").await;
    settle_one(&relayer, &harness.store, first).await;
    assert_eq!(
        harness.store.job(first).await.unwrap().unwrap().state,
        JobState::Settled
    );

    // A second job carrying the same proof, and therefore the same nullifier.
    let second = harness.proved_job("membership", 2).await;
    assert_eq!(
        harness.nullifier_of(second).await,
        harness.nullifier_of(first).await,
        "the fixture should reuse the same proof"
    );

    let outcomes = settle_one(&relayer, &harness.store, second).await;

    assert_eq!(
        outcomes,
        vec![Outcome::AlreadySettled(second)],
        "a consumed nullifier means done, not broken: {outcomes:?}"
    );
    assert_eq!(
        harness.store.job(second).await.unwrap().unwrap().state,
        JobState::Settled,
        "the job is settled — by the first transaction"
    );
    assert!(
        harness
            .store
            .latest_settlement(second)
            .await
            .unwrap()
            .is_none(),
        "no settlement row should be invented for a transaction this relayer \
         did not send"
    );
}

// ---------------------------------------------------------------------------
// Reorgs
// ---------------------------------------------------------------------------

/// Why `settled` is not a terminal state. A reorg removes the settlement, the
/// nullifier becomes free again, and the job must go back on the submit queue
/// keeping its proof — which is still perfectly valid.
#[tokio::test(flavor = "multi_thread")]
async fn a_reorg_returns_a_settled_job_to_the_submit_queue() {
    let harness = Harness::build("reorg").await;
    let id = harness.proved_job("membership", 1).await;
    let nullifier = harness.nullifier_of(id).await;

    let provider = harness.provider().await;

    // Snapshot before settling, so reverting unwinds it exactly as a reorg
    // would: the transaction never happened.
    let snapshot: U256 = provider
        .raw_request("evm_snapshot".into(), ())
        .await
        .expect("anvil should support snapshots");

    let relayer = harness.relayer(RELAYER_KEY, "relayer-1").await;
    settle_one(&relayer, &harness.store, id).await;
    assert!(harness.nullifier_used(nullifier).await);
    assert_eq!(
        harness.store.job(id).await.unwrap().unwrap().state,
        JobState::Settled
    );

    let reverted: bool = provider
        .raw_request("evm_revert".into(), (snapshot,))
        .await
        .expect("revert should succeed");
    assert!(reverted);
    assert!(
        !harness.nullifier_used(nullifier).await,
        "the reorg should have freed the nullifier"
    );

    // The relayer's reorg watcher must notice and put the job back.
    run_until(&relayer, &harness.store, id, |state| {
        state == JobState::Proved
    })
    .await;

    let job = harness.store.job(id).await.unwrap().unwrap();
    assert_eq!(
        job.state,
        JobState::Proved,
        "a reorged settlement must return the job to the submit queue"
    );
    assert!(
        job.proof.is_some(),
        "the proof survives a reorg; only its place on the chain was lost"
    );

    let settlement = harness.store.latest_settlement(id).await.unwrap().unwrap();
    assert!(
        settlement.reorged_at.is_some(),
        "the settlement row should be stamped, not deleted"
    );
}

/// And having gone back, it settles again.
#[tokio::test(flavor = "multi_thread")]
async fn a_reorged_job_settles_again_on_resubmission() {
    let harness = Harness::build("resubmit").await;
    let id = harness.proved_job("membership", 1).await;
    let nullifier = harness.nullifier_of(id).await;

    let provider = harness.provider().await;
    let snapshot: U256 = provider
        .raw_request("evm_snapshot".into(), ())
        .await
        .expect("snapshot");

    let relayer = harness.relayer(RELAYER_KEY, "relayer-1").await;
    settle_one(&relayer, &harness.store, id).await;

    let _: bool = provider
        .raw_request("evm_revert".into(), (snapshot,))
        .await
        .expect("revert");

    // Returned by hand, so this test is about resubmission rather than about
    // the watcher, which has its own test above.
    let tx_hash = harness
        .store
        .latest_settlement(id)
        .await
        .unwrap()
        .unwrap()
        .tx_hash;
    harness
        .store
        .record_reorg(id, "relayer-1", &tx_hash)
        .await
        .expect("record reorg");

    let outcomes = settle_one(&relayer, &harness.store, id).await;

    assert!(
        outcomes.iter().any(|o| matches!(o, Outcome::Settled(_))),
        "it should have settled again: {outcomes:?}"
    );
    assert!(
        harness.nullifier_used(nullifier).await,
        "the nullifier should be consumed once more"
    );
    assert_eq!(
        harness.store.job(id).await.unwrap().unwrap().state,
        JobState::Settled
    );
}

// ---------------------------------------------------------------------------
// Authorisation
// ---------------------------------------------------------------------------

/// An unauthorised relayer's every submission reverts. Saying so once at
/// start-up beats letting an operator infer it from the failure rate.
#[tokio::test(flavor = "multi_thread")]
async fn an_unauthorised_relayer_is_refused_at_startup() {
    let harness = Harness::build("unauthorised").await;
    let relayer = harness.relayer(STRANGER_KEY, "stranger").await;

    let err = relayer
        .preflight()
        .await
        .expect_err("an unauthorised relayer should not pass pre-flight");

    assert!(
        err.contains("not an authorised relayer"),
        "the message should say what is wrong: {err}"
    );
    assert!(err.contains("setRelayer"), "and how to fix it: {err}");
}

/// The permissioned set (ADR-011): more than one key can settle.
#[tokio::test(flavor = "multi_thread")]
async fn every_relayer_in_the_set_can_settle() {
    let harness = Harness::build("set").await;

    let first = harness.relayer(RELAYER_KEY, "relayer-1").await;
    let second = harness.relayer(SECOND_RELAYER_KEY, "relayer-2").await;

    first.preflight().await.expect("relayer-1 authorised");
    second.preflight().await.expect("relayer-2 authorised");

    let a = harness.proved_job("membership", 1).await;
    let b = harness.proved_job("range_proof", 2).await;

    settle_one(&first, &harness.store, a).await;
    settle_one(&second, &harness.store, b).await;

    for id in [a, b] {
        assert_eq!(
            harness.store.job(id).await.unwrap().unwrap().state,
            JobState::Settled,
            "job {id} did not settle"
        );
    }
}

// ---------------------------------------------------------------------------
// Nonce recovery
// ---------------------------------------------------------------------------

/// A relayer's nonce can drift from the chain's — a transaction landed that it
/// did not know about, or it restarted. Waiting does not fix that; only asking
/// the chain does.
#[tokio::test(flavor = "multi_thread")]
async fn a_relayer_recovers_from_a_nonce_gap() {
    let harness = Harness::build("nonce").await;
    let id = harness.proved_job("membership", 1).await;

    let observer = Chain::connect(&harness.anvil.rpc_url(), RELAYER_KEY, harness.settlement)
        .await
        .expect("connect");
    let before = observer.pending_nonce().await.expect("nonce");

    // Spend the relayer account's next nonce behind its back.
    let signer: alloy::signers::local::PrivateKeySigner =
        RELAYER_KEY.trim_start_matches("0x").parse().expect("key");
    let provider = ProviderBuilder::new()
        .wallet(alloy::network::EthereumWallet::from(signer))
        .connect(&harness.anvil.rpc_url())
        .await
        .expect("connect");

    provider
        .send_transaction(
            alloy::rpc::types::TransactionRequest::default()
                .to(Address::ZERO)
                .value(U256::from(1)),
        )
        .await
        .expect("transfer should broadcast")
        .get_receipt()
        .await
        .expect("transfer should mine");

    assert_eq!(
        observer.pending_nonce().await.expect("nonce"),
        before + 1,
        "the nonce should have moved without the relayer's knowledge"
    );

    let relayer = harness.relayer(RELAYER_KEY, "relayer-1").await;
    let outcomes = settle_one(&relayer, &harness.store, id).await;

    assert!(
        outcomes.iter().any(|o| matches!(o, Outcome::Settled(_))),
        "the relayer should have read the chain and settled: {outcomes:?}"
    );
}

// ---------------------------------------------------------------------------
// Transient failures
// ---------------------------------------------------------------------------

/// An RPC that is not there is a transient failure. The job must return to
/// `proved` with its proof intact and a delay before the next attempt — never
/// be discarded.
#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_rpc_is_transient_and_keeps_the_proof() {
    let harness = Harness::build("rpc_down").await;
    let id = harness.proved_job("membership", 1).await;

    // Port 1 is reserved; nothing listens there.
    let Ok(dead) = Chain::connect("http://127.0.0.1:1", RELAYER_KEY, harness.settlement).await
    else {
        // Refusing to connect at all is also correct: nothing was submitted.
        assert_eq!(
            harness.store.job(id).await.unwrap().unwrap().state,
            JobState::Proved
        );
        return;
    };

    let relayer = Relayer::new(harness.store.clone(), dead, harness.config("offline"));

    let outcomes = run_until(&relayer, &harness.store, id, |state| {
        // Back on the queue after a failed attempt.
        state == JobState::Proved
    })
    .await;

    assert!(
        outcomes.iter().any(|o| matches!(
            o,
            Outcome::Failed {
                kind: dray_core::FailureKind::Transient,
                ..
            }
        )),
        "an unreachable RPC must be transient: {outcomes:?}"
    );

    let job = harness.store.job(id).await.unwrap().unwrap();
    assert_eq!(
        job.state,
        JobState::Proved,
        "a proof must not be discarded because an endpoint was down"
    );
    assert!(job.proof.is_some());
    assert!(
        job.retry_after.is_some(),
        "and it should back off rather than spin"
    );
}
