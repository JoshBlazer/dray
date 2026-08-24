//! Whole-worker tests: real Postgres, real `nargo`, real `bb`.
//!
//! Leasing, retries, heartbeats, and shutdown are not observable without a
//! store behind them, and proving is not observable without the toolchain. So
//! these tests need both, and are gated behind `integration-tests` for the same
//! reason the store's Postgres tests are: a suite that silently skipped would
//! look exactly like one that passed.
//!
//! Run with:
//!
//! ```sh
//! make up && make setup-zk
//! DATABASE_URL=postgres://dray:dray@localhost:5432/dray_test \
//!     cargo test -p dray-worker --features integration-tests
//! ```
//!
//! Each test gets its own database. `lease_next` is global by design — a worker
//! asks for *any* job, not a job in some namespace — so two tests sharing a
//! database would steal each other's work and fail in ways that look like
//! concurrency bugs in the code under test.

#![cfg(feature = "integration-tests")]

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use dray_core::JobState;
use dray_store::{Circuit, Store};
use dray_worker::{
    prover::{self, Artifacts, ProverConfig},
    worker::{Outcome, Shutdown, Worker, WorkerConfig, shutdown},
};
use serde_json::json;

const MEMBERSHIP_NULLIFIER: &str =
    "04eed209841c67fdb32a39da5ee53038c72465da539eaa32c5964797ba7ab646";
const REFERENCE_ROOT: &str = "0x089175ccc891f80d0f76bc5c6f7a239c2a78069ddf64478b68410c7d6b4c7320";

fn admin_url() -> String {
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run the integration tests")
}

fn circuits_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../circuits")
        .canonicalize()
        .expect("the circuits workspace should exist")
}

fn membership_inputs(secret: &str) -> serde_json::Value {
    json!({
        "root": REFERENCE_ROOT,
        "secret": secret,
        "leaf_index": "5",
        "siblings": vec!["7"; 20],
    })
}

/// The one witness that actually satisfies the membership circuit.
fn valid_inputs() -> serde_json::Value {
    membership_inputs("42")
}

/// A range proof that always succeeds, distinct per `secret`.
///
/// The load and chaos tests use this rather than membership because every
/// secret yields a *valid* witness with its own nullifier, so a hundred
/// genuinely different jobs can be generated without computing a hundred Merkle
/// roots. A load test in which every job fails immediately would measure
/// almost nothing.
fn range_inputs(secret: usize) -> serde_json::Value {
    json!({"min": "18", "max": "150", "value": "42", "secret": secret.to_string()})
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// A private database, prepared artefacts, and a scratch root.
struct Fixture {
    store: Store,
    prover: ProverConfig,
    _artifacts_dir: tempfile::TempDir,
    _scratch_dir: tempfile::TempDir,
}

impl Fixture {
    async fn build(label: &str) -> Self {
        for tool in ["nargo", "bb"] {
            assert!(
                which(tool),
                "{tool} not found on PATH. These tests need the real toolchain: run `make setup-zk`"
            );
        }

        let store = isolated_store(label).await;

        let artifacts_dir = tempfile::tempdir().expect("tempdir");
        let scratch_dir = tempfile::tempdir().expect("tempdir");

        let mut prover = ProverConfig::new(
            Artifacts::at(artifacts_dir.path()),
            scratch_dir.path().to_path_buf(),
        );
        prover.artifacts = prover::prepare(
            &circuits_dir(),
            &["membership".to_owned(), "range_proof".to_owned()],
            artifacts_dir.path(),
            &prover,
        )
        .await
        .expect("preparing artefacts should succeed");

        for (id, name) in [
            ("membership", "Merkle membership"),
            ("range_proof", "Range proof"),
        ] {
            store
                .upsert_circuit(&Circuit {
                    id: id.into(),
                    display_name: name.into(),
                    input_schema: json!({"type": "object"}),
                    verifier_address: None,
                    enabled: true,
                })
                .await
                .expect("registering the circuit should succeed");
        }

        Self {
            store,
            prover,
            _artifacts_dir: artifacts_dir,
            _scratch_dir: scratch_dir,
        }
    }

    async fn enqueue(&self, inputs: serde_json::Value) -> uuid::Uuid {
        self.enqueue_on("membership", inputs, 3).await
    }

    async fn enqueue_on(
        &self,
        circuit: &str,
        inputs: serde_json::Value,
        max_attempts: i32,
    ) -> uuid::Uuid {
        let (job, _) = self
            .store
            .enqueue(circuit, &inputs, None, max_attempts)
            .await
            .expect("enqueue should succeed");
        job.id
    }

    fn worker(&self, id: &str) -> Worker {
        let mut config = WorkerConfig::new(id);
        // Short enough that a test can watch a lease expire without waiting
        // three minutes for it.
        config.lease_ttl = Duration::from_secs(20);
        config.heartbeat_interval = Duration::from_secs(2);
        config.poll_interval = Duration::from_millis(50);
        config.shutdown_grace = Duration::from_secs(30);
        config.reap_interval = Duration::from_millis(200);
        Worker::new(self.store.clone(), config, self.prover.clone())
    }
}

async fn isolated_store(label: &str) -> Store {
    let admin_url = admin_url();
    let db = format!("dray_worker_{}_{label}", std::process::id());

    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("could not connect to Postgres");
    sqlx::query(&format!(r#"DROP DATABASE IF EXISTS "{db}" WITH (FORCE)"#))
        .execute(&admin)
        .await
        .expect("could not drop the test database");
    sqlx::query(&format!(r#"CREATE DATABASE "{db}""#))
        .execute(&admin)
        .await
        .expect("could not create the test database");
    admin.close().await;

    let (prefix, _) = admin_url
        .rsplit_once('/')
        .expect("DATABASE_URL should contain a database name");
    let store = Store::connect(&format!("{prefix}/{db}"), 16)
        .await
        .expect("could not connect to the test database");
    store.migrate().await.expect("migrations failed");
    store
}

/// Run a worker until something else triggers its shutdown.
///
/// Bounded by a deadline rather than waiting indefinitely: a worker that stops
/// leasing is a bug this suite has to report, not hang on.
async fn run_bounded(worker: &Worker, signal: Shutdown) -> Vec<Outcome> {
    tokio::time::timeout(Duration::from_secs(180), worker.run(signal))
        .await
        .expect("the worker did not stop within the deadline")
}

// ---------------------------------------------------------------------------
// The happy path
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_worker_leases_proves_and_records_a_job() {
    let fixture = Fixture::build("happy").await;
    let id = fixture.enqueue(valid_inputs()).await;

    let (handle, signal) = shutdown();
    let worker = fixture.worker("worker-1");

    // Stop the worker as soon as the job leaves the queue.
    let watcher = {
        let store = fixture.store.clone();
        tokio::spawn(async move {
            for _ in 0..600 {
                let job = store.job(id).await.expect("job lookup").expect("job");
                if matches!(job.state, JobState::Proved | JobState::Failed) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            handle.trigger();
        })
    };

    let outcomes = run_bounded(&worker, signal).await;
    watcher.await.expect("watcher should not panic");

    assert_eq!(
        outcomes.len(),
        1,
        "expected exactly one attempt: {outcomes:?}"
    );
    assert_eq!(outcomes[0], Outcome::Proved(id));

    let job = fixture.store.job(id).await.unwrap().unwrap();
    assert_eq!(job.state, JobState::Proved);
    assert_eq!(job.attempts, 1);
    assert!(job.leased_by.is_none(), "the lease should be released");

    let proof = job.proof.expect("a proved job must carry its proof");
    assert_eq!(proof.len(), 8384);

    // The nullifier the relayer will settle must be the one the rest of the
    // system was built against.
    let public_inputs =
        sqlx::query_scalar::<_, Option<Vec<u8>>>("SELECT public_inputs FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(fixture.store.pool())
            .await
            .expect("query")
            .expect("public inputs");

    assert_eq!(public_inputs.len(), 64);
    assert_eq!(hex(&public_inputs[32..]), MEMBERSHIP_NULLIFIER);
}

// ---------------------------------------------------------------------------
// Failure and retry
// ---------------------------------------------------------------------------

/// A witness the circuit rejects must fail permanently and stop consuming
/// attempts. Retrying it would burn a lease and a subprocess to reach the same
/// answer.
#[tokio::test(flavor = "multi_thread")]
async fn an_unsatisfiable_job_fails_permanently_on_the_first_attempt() {
    let fixture = Fixture::build("permanent").await;
    let id = fixture.enqueue(membership_inputs("43")).await;

    let (handle, signal) = shutdown();
    let worker = fixture.worker("worker-1");

    let watcher = {
        let store = fixture.store.clone();
        tokio::spawn(async move {
            for _ in 0..600 {
                let job = store.job(id).await.expect("lookup").expect("job");
                if job.state == JobState::Failed {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            handle.trigger();
        })
    };

    let outcomes = run_bounded(&worker, signal).await;
    watcher.await.expect("watcher");

    assert_eq!(outcomes.len(), 1, "should not have retried: {outcomes:?}");
    assert!(
        matches!(
            outcomes[0],
            Outcome::Failed {
                kind: dray_core::FailureKind::Permanent,
                retry_in: None,
                ..
            }
        ),
        "{:?}",
        outcomes[0]
    );

    let job = fixture.store.job(id).await.unwrap().unwrap();
    assert_eq!(job.state, JobState::Failed);
    assert_eq!(job.attempts, 1, "a permanent failure must not retry");
    assert!(
        job.last_error
            .as_deref()
            .is_some_and(|e| e.contains("merkle root mismatch")),
        "the circuit's own message should reach the operator: {:?}",
        job.last_error
    );
    assert!(job.retry_after.is_none());
}

/// A bound that no proof can meet is transient, so the job must go back to the
/// queue with a delay rather than fail outright.
#[tokio::test(flavor = "multi_thread")]
async fn a_transient_failure_schedules_a_retry() {
    let mut fixture = Fixture::build("transient").await;
    fixture.prover.bounds.wall_clock = Duration::from_millis(50);

    let id = fixture.enqueue(valid_inputs()).await;

    let (handle, signal) = shutdown();
    let worker = fixture.worker("worker-1");

    let watcher = {
        let store = fixture.store.clone();
        tokio::spawn(async move {
            for _ in 0..600 {
                let job = store.job(id).await.expect("lookup").expect("job");
                if job.attempts >= 1 && job.state == JobState::Queued {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            handle.trigger();
        })
    };

    let outcomes = run_bounded(&worker, signal).await;
    watcher.await.expect("watcher");

    assert!(!outcomes.is_empty());
    assert!(
        matches!(
            outcomes[0],
            Outcome::Failed {
                kind: dray_core::FailureKind::Transient,
                retry_in: Some(_),
                ..
            }
        ),
        "{:?}",
        outcomes[0]
    );

    let job = fixture.store.job(id).await.unwrap().unwrap();
    assert_eq!(job.state, JobState::Queued, "should be retryable");
    assert!(
        job.retry_after.is_some(),
        "a transient failure should back off, not spin"
    );
}

// ---------------------------------------------------------------------------
// Shutdown
// ---------------------------------------------------------------------------

/// A worker told to stop before it takes anything must take nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_worker_that_is_already_shutting_down_leases_nothing() {
    let fixture = Fixture::build("preshutdown").await;
    let id = fixture.enqueue(valid_inputs()).await;

    let (handle, signal) = shutdown();
    handle.trigger();

    let outcomes = fixture.worker("worker-1").run(signal).await;

    assert!(outcomes.is_empty(), "{outcomes:?}");
    let job = fixture.store.job(id).await.unwrap().unwrap();
    assert_eq!(job.state, JobState::Queued);
    assert_eq!(job.attempts, 0, "the job was never touched");
}

/// Shutdown mid-proof with a grace period long enough to finish: the work must
/// be kept, not thrown away.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_lets_a_proof_in_flight_finish() {
    let fixture = Fixture::build("graceful").await;
    let id = fixture.enqueue(valid_inputs()).await;

    let (handle, signal) = shutdown();
    let worker = fixture.worker("worker-1");

    // Fire shutdown once the job is being proved, so the grace path is the one
    // under test rather than the "nothing leased yet" path.
    let watcher = {
        let store = fixture.store.clone();
        tokio::spawn(async move {
            for _ in 0..600 {
                let job = store.job(id).await.expect("lookup").expect("job");
                if matches!(job.state, JobState::Proving | JobState::Proved) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            handle.trigger();
        })
    };

    let outcomes = worker.run(signal).await;
    watcher.await.expect("watcher");

    assert_eq!(outcomes, vec![Outcome::Proved(id)], "{outcomes:?}");

    let job = fixture.store.job(id).await.unwrap().unwrap();
    assert_eq!(job.state, JobState::Proved);
    assert!(job.proof.is_some());
}

/// Shutdown mid-proof with a grace period too short: the job must be handed
/// back immediately rather than left to wait out its lease.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_releases_a_job_it_cannot_finish() {
    let fixture = Fixture::build("abandon").await;
    let id = fixture.enqueue(valid_inputs()).await;

    let (handle, signal) = shutdown();

    let mut config = WorkerConfig::new("worker-1");
    config.lease_ttl = Duration::from_secs(120);
    config.heartbeat_interval = Duration::from_secs(5);
    config.poll_interval = Duration::from_millis(50);
    config.shutdown_grace = Duration::ZERO;
    let worker = Worker::new(fixture.store.clone(), config, fixture.prover.clone());

    let watcher = {
        let store = fixture.store.clone();
        tokio::spawn(async move {
            for _ in 0..600 {
                let job = store.job(id).await.expect("lookup").expect("job");
                if job.state == JobState::Proving {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            handle.trigger();
        })
    };

    let outcomes = worker.run(signal).await;
    watcher.await.expect("watcher");

    assert_eq!(outcomes, vec![Outcome::Abandoned(id)], "{outcomes:?}");

    let job = fixture.store.job(id).await.unwrap().unwrap();
    assert_eq!(
        job.state,
        JobState::Queued,
        "an abandoned job must go straight back on the queue"
    );
    assert!(
        job.leased_by.is_none(),
        "the lease must be released, not left to expire"
    );
    assert_eq!(job.attempts, 1, "the attempt still counts");
}

// ---------------------------------------------------------------------------
// Contention and chaos
//
// These are the phase's stated exit criteria: 100 jobs across 4 workers with no
// loss and no duplication, and the same run with workers being killed
// throughout. They use `range_proof` so every job is a real, distinct, valid
// proof — a hundred jobs that all fail in 0.3s would exercise the queue but not
// the thing the queue is for.
// ---------------------------------------------------------------------------

const LOAD: usize = 100;

/// Count how many times each job entered a terminal state, according to the
/// transition log. Anything other than one, for any job, is either a loss or a
/// duplicate settlement.
async fn terminal_counts(store: &Store, ids: &[uuid::Uuid]) -> Vec<(uuid::Uuid, i64)> {
    let mut counts = Vec::with_capacity(ids.len());
    for id in ids {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM job_transitions
             WHERE job_id = $1 AND to_state IN ('proved', 'failed')",
        )
        .bind(*id)
        .fetch_one(store.pool())
        .await
        .expect("query");
        counts.push((*id, count));
    }
    counts
}

/// Trigger shutdown once every job has reached a terminal state, or after
/// `patience`, whichever comes first.
fn watch_until_settled(
    store: Store,
    ids: Vec<uuid::Uuid>,
    handle: dray_worker::worker::ShutdownHandle,
    patience: Duration,
) -> tokio::task::JoinHandle<usize> {
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + patience;
        loop {
            let settled: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM jobs
                 WHERE id = ANY($1) AND state IN ('proved', 'failed')",
            )
            .bind(&ids)
            .fetch_one(store.pool())
            .await
            .expect("query");

            if settled as usize == ids.len() || tokio::time::Instant::now() >= deadline {
                handle.trigger();
                return settled as usize;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
}

/// The load test: 100 jobs, 4 workers, zero loss and zero duplication.
#[tokio::test(flavor = "multi_thread")]
async fn a_hundred_jobs_across_four_workers_are_proved_exactly_once() {
    let fixture = Fixture::build("load").await;

    let mut ids = Vec::with_capacity(LOAD);
    for n in 0..LOAD {
        ids.push(fixture.enqueue_on("range_proof", range_inputs(n), 3).await);
    }
    assert_eq!(fixture.store.queue_depth().await.unwrap(), LOAD as i64);

    let (handle, signal) = shutdown();
    let watcher = watch_until_settled(
        fixture.store.clone(),
        ids.clone(),
        handle,
        Duration::from_secs(600),
    );

    let workers: Vec<_> = (0..4)
        .map(|n| Arc::new(fixture.worker(&format!("worker-{n}"))))
        .collect();

    let runs: Vec<_> = workers
        .iter()
        .map(|worker| {
            let worker = Arc::clone(worker);
            let signal = signal.clone();
            tokio::spawn(async move { worker.run(signal).await })
        })
        .collect();

    let mut all = Vec::new();
    for run in runs {
        all.extend(run.await.expect("worker should not panic"));
    }
    let settled = watcher.await.expect("watcher");
    assert_eq!(settled, LOAD, "not every job settled within the deadline");

    // Every job proved — not merely reached a terminal state. These witnesses
    // are all valid, so anything else is a defect rather than a job problem.
    let proved = all
        .iter()
        .filter(|outcome| matches!(outcome, Outcome::Proved(_)))
        .count();
    assert_eq!(proved, LOAD, "only {proved} of {LOAD} proved: {all:?}");

    // No duplication: each job was attempted exactly once across the pool.
    let mut seen = std::collections::HashSet::new();
    for outcome in &all {
        assert!(
            seen.insert(outcome.job_id()),
            "job {} was attempted by more than one worker",
            outcome.job_id()
        );
    }

    // And the store agrees, independently of what the workers reported.
    for (id, count) in terminal_counts(&fixture.store, &ids).await {
        assert_eq!(count, 1, "job {id} settled {count} times, not once");
    }

    let mut by_worker = std::collections::HashMap::new();
    for id in &ids {
        let worker: Option<String> = sqlx::query_scalar(
            "SELECT worker_id FROM job_attempts WHERE job_id = $1 ORDER BY attempt_number LIMIT 1",
        )
        .bind(*id)
        .fetch_one(fixture.store.pool())
        .await
        .expect("query");
        *by_worker
            .entry(worker.unwrap_or_default())
            .or_insert(0_usize) += 1;
    }
    eprintln!("work distribution across the pool: {by_worker:?}");
    assert!(
        by_worker.len() > 1,
        "one worker took everything, so this did not test contention: {by_worker:?}"
    );
}

/// The exit criterion: kill workers repeatedly during a 100-job run, and
/// confirm every job still settles exactly once.
///
/// Killing is modelled by dropping the worker's future rather than by killing a
/// process, which is the harsher test of the store's invariants: a dropped
/// future stops mid-attempt with no chance to release its lease or record
/// anything, exactly like a SIGKILL. Recovery has to come from lease expiry and
/// from whichever workers are still alive.
///
/// `max_attempts` is generous because a kill spends an attempt without the job
/// having done anything wrong. The property under test is loss and duplication,
/// not the attempt budget, which has its own tests.
#[tokio::test(flavor = "multi_thread")]
async fn a_hundred_jobs_survive_workers_being_killed_throughout() {
    let fixture = Fixture::build("chaos").await;

    let mut ids = Vec::with_capacity(LOAD);
    for n in 0..LOAD {
        ids.push(fixture.enqueue_on("range_proof", range_inputs(n), 25).await);
    }

    // A short lease so a killed worker's jobs come back quickly.
    let store = fixture.store.clone();
    let prover = fixture.prover.clone();
    let build_worker = move |name: String| {
        let mut config = WorkerConfig::new(name);
        config.lease_ttl = Duration::from_secs(5);
        config.heartbeat_interval = Duration::from_secs(1);
        config.poll_interval = Duration::from_millis(50);
        config.shutdown_grace = Duration::from_secs(30);
        config.reap_interval = Duration::from_millis(250);
        Worker::new(store.clone(), config, prover.clone())
    };

    // Two workers run to completion; two are killed and restarted throughout,
    // so there is always both a survivor to reap and a casualty to recover
    // from. A run in which everything dies at once would only test the reaper.
    let (handle, signal) = shutdown();
    let watcher = watch_until_settled(
        fixture.store.clone(),
        ids.clone(),
        handle,
        Duration::from_secs(900),
    );

    let survivors: Vec<_> = (0..2)
        .map(|n| {
            let worker = build_worker(format!("survivor-{n}"));
            let signal = signal.clone();
            tokio::spawn(async move { worker.run(signal).await })
        })
        .collect();

    // Deterministic but uneven kill timings — no RNG, so a failure reproduces.
    let chaos = {
        let signal = signal.clone();
        let build = build_worker;
        tokio::spawn(async move {
            let mut kills = 0_usize;
            for round in 0..12_u64 {
                if signal.is_requested() {
                    break;
                }

                // Uneven but deterministic lifetimes: no RNG, so a failure
                // reproduces exactly rather than only sometimes.
                let lifetime = Duration::from_millis(300 + (round % 5) * 250);

                // `_keep` holds the doomed worker's shutdown handle open, so it
                // never stops cooperatively. When the timeout fires the future
                // is dropped outright — no graceful path, no lease released,
                // no result recorded. That is the point.
                let (_keep, live) = shutdown();
                let worker = build(format!("doomed-{round}"));

                if tokio::time::timeout(lifetime, worker.run(live))
                    .await
                    .is_err()
                {
                    kills += 1;
                }
            }
            kills
        })
    };

    let kills = chaos.await.expect("chaos driver should not panic");
    let settled = watcher.await.expect("watcher");

    let mut all = Vec::new();
    for run in survivors {
        all.extend(run.await.expect("worker should not panic"));
    }

    eprintln!("killed {kills} workers mid-flight; {settled} of {LOAD} jobs settled");
    assert!(
        kills > 0,
        "no worker was actually killed, so nothing was tested"
    );
    assert_eq!(
        settled, LOAD,
        "jobs were lost: only {settled} of {LOAD} settled"
    );

    // Nothing lost, nothing settled twice, nothing still holding a lease.
    for id in &ids {
        let job = fixture.store.job(*id).await.unwrap().unwrap();
        assert_eq!(
            job.state,
            JobState::Proved,
            "job {id} did not prove: it is {:?} with {} attempts",
            job.state,
            job.attempts,
        );
        assert!(
            job.leased_by.is_none(),
            "job {id} is terminal but still shows a lease holder"
        );
        assert!(job.proof.is_some(), "job {id} proved but carries no proof");
    }

    for (id, count) in terminal_counts(&fixture.store, &ids).await {
        assert_eq!(
            count, 1,
            "job {id} reached a terminal state {count} times, not once"
        );
    }

    // The run has to have been genuinely disrupted, or it proves nothing about
    // chaos — some job must have been attempted more than once.
    let retried: i64 =
        sqlx::query_scalar("SELECT count(*) FROM jobs WHERE id = ANY($1) AND attempts > 1")
            .bind(&ids)
            .fetch_one(fixture.store.pool())
            .await
            .expect("query");
    assert!(
        retried > 0,
        "no job was ever retried, so the kills did not interrupt any work"
    );
    eprintln!("{retried} of {LOAD} jobs needed more than one attempt");
}

// ---------------------------------------------------------------------------
// Metrics
//
// Instruments that are never asserted against are decoration: they compile,
// they render, and they can quietly count the wrong thing for months. These
// drive real work through a real worker and check the numbers that come out.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_successful_job_is_counted_with_its_cost() {
    let fixture = Fixture::build("metrics_ok").await;
    let id = fixture.enqueue_on("range_proof", range_inputs(1), 3).await;

    let (handle, signal) = shutdown();
    let worker = fixture.worker("metrics-worker");
    let metrics = worker.metrics();

    let watcher = watch_until_settled(
        fixture.store.clone(),
        vec![id],
        handle,
        Duration::from_secs(120),
    );
    worker.run(signal).await;
    watcher.await.expect("watcher");

    let rendered = metrics.render();

    assert!(
        rendered.contains("outcome=\"proved\"} 1"),
        "the proof was not counted: {rendered}"
    );
    assert_eq!(
        metrics.proving_duration.count(),
        1,
        "proving duration was not observed"
    );
    assert_eq!(
        metrics.attempts.count(),
        1,
        "the attempt count was not observed"
    );

    // Peak memory comes from the subprocess and is optional, but on this
    // platform it should have been measured — a silently absent measurement
    // would leave the memory ceiling unmonitored.
    assert_eq!(
        metrics.peak_memory_kb.count(),
        1,
        "peak memory was not measured, so the address-space ceiling is unmonitored"
    );

    assert!(
        rendered.contains("dray_worker_failures_total"),
        "failure counters should be present at zero: {rendered}"
    );
}

/// A timeout and an out-of-memory kill must land in different counters. Their
/// remedies are opposites, so an operator reading one aggregate number would be
/// misled half the time.
#[tokio::test(flavor = "multi_thread")]
async fn a_timeout_is_counted_as_a_timeout_and_not_as_something_else() {
    let mut fixture = Fixture::build("metrics_timeout").await;
    fixture.prover.bounds.wall_clock = Duration::from_millis(50);

    let id = fixture.enqueue_on("range_proof", range_inputs(2), 1).await;

    let (handle, signal) = shutdown();
    let worker = fixture.worker("metrics-worker");
    let metrics = worker.metrics();

    let watcher = watch_until_settled(
        fixture.store.clone(),
        vec![id],
        handle,
        Duration::from_secs(120),
    );
    worker.run(signal).await;
    watcher.await.expect("watcher");

    let rendered = metrics.render();

    assert!(
        rendered.contains("reason=\"timeout\"} 1"),
        "the timeout was not counted as one: {rendered}"
    );
    assert!(
        rendered.contains("reason=\"oom\"} 0"),
        "a timeout was miscounted as an out-of-memory kill: {rendered}"
    );
    assert!(rendered.contains("outcome=\"failed\"} 1"), "{rendered}");
    assert_eq!(
        metrics.proving_duration.count(),
        0,
        "a failed attempt must not be recorded as proving time, or the \
         distribution would be dominated by work that never produced a proof"
    );
}

/// Queue depth and lease age are properties of the queue, so they are sampled
/// rather than counted. A sampler that never ran would leave both at zero,
/// which reads identically to a healthy idle system.
#[tokio::test(flavor = "multi_thread")]
async fn queue_depth_is_sampled_from_the_store() {
    let fixture = Fixture::build("metrics_gauges").await;

    for n in 0..5 {
        fixture
            .enqueue_on("range_proof", range_inputs(100 + n), 3)
            .await;
    }

    let mut config = WorkerConfig::new("gauge-worker");
    config.lease_ttl = Duration::from_secs(60);
    config.poll_interval = Duration::from_millis(50);
    config.sample_interval = Duration::from_millis(100);
    config.reap_interval = Duration::from_secs(60);

    let worker = Worker::new(fixture.store.clone(), config, fixture.prover.clone());
    let metrics = worker.metrics();

    let (handle, signal) = shutdown();
    let stopper = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        handle.trigger();
    });

    worker.run(signal).await;
    stopper.await.expect("stopper");

    assert!(
        metrics.queue_depth.get() > 0,
        "queue depth was never sampled: an unsampled gauge reads exactly like \
         an empty queue"
    );
}
