//! Integration tests against a real PostgreSQL.
//!
//! Gated behind the `integration-tests` feature so that `cargo test` on a fresh
//! clone passes without Docker. Skipping at runtime when `DATABASE_URL` is
//! unset would be worse than not running at all — a broken query would look
//! like a green build.
//!
//! Run with:
//!
//! ```sh
//! make up
//! DATABASE_URL=postgres://dray:dray@localhost:5432/dray \
//!     cargo test -p dray-store --features integration-tests
//! ```
//!
//! Isolation between tests comes from each test registering its own circuit.
//! Because job identity is `hash(circuit_id || inputs)`, a unique circuit id
//! gives a private job namespace without needing a database per test.

#![cfg(feature = "integration-tests")]

use std::sync::atomic::{AtomicU64, Ordering};

use dray_core::{JobEvent, JobState};
use dray_store::{Circuit, Enqueued, Store, StoreError};
use serde_json::json;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn database_url() -> String {
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run the integration tests")
}

async fn store() -> Store {
    let store = Store::connect(&database_url(), 16)
        .await
        .expect("could not connect to Postgres");
    store.migrate().await.expect("migrations failed");
    store
}

/// Registers a circuit unique to this test and returns its id.
async fn fresh_circuit(store: &Store) -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let id = format!("test_circuit_{}_{n}", std::process::id());
    store
        .upsert_circuit(&Circuit {
            id: id.clone(),
            display_name: "test".into(),
            input_schema: json!({"type": "object"}),
            verifier_address: None,
            enabled: true,
        })
        .await
        .expect("could not register circuit");
    id
}

// ---------------------------------------------------------------------------
// Deduplication — the property idempotency rests on
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_new_request_creates_a_job() {
    let store = store().await;
    let circuit = fresh_circuit(&store).await;

    let (job, outcome) = store
        .enqueue(&circuit, &json!({"secret": "42"}), Some("key-1"), 3)
        .await
        .unwrap();

    assert_eq!(outcome, Enqueued::Created);
    assert_eq!(job.state, JobState::Queued);
    assert_eq!(job.attempts, 0);
    assert_eq!(job.circuit_id, circuit);
    assert_eq!(job.idempotency_key.as_deref(), Some("key-1"));
}

#[tokio::test]
async fn resubmitting_identical_inputs_returns_the_same_job() {
    let store = store().await;
    let circuit = fresh_circuit(&store).await;
    let inputs = json!({"secret": "42", "leaf_index": "5"});

    let (first, a) = store.enqueue(&circuit, &inputs, None, 3).await.unwrap();
    let (second, b) = store.enqueue(&circuit, &inputs, None, 3).await.unwrap();

    assert_eq!(a, Enqueued::Created);
    assert_eq!(b, Enqueued::Duplicate);
    assert_eq!(
        first.id, second.id,
        "a duplicate must not create a second job"
    );
}

/// Key order is not semantic in JSON, so these are the same request. If
/// canonicalisation were skipped, this would create two jobs and the system
/// would prove — and settle — the same thing twice.
#[tokio::test]
async fn reordered_keys_deduplicate_to_one_job() {
    let store = store().await;
    let circuit = fresh_circuit(&store).await;

    let (first, _) = store
        .enqueue(&circuit, &json!({"a": "1", "b": "2"}), None, 3)
        .await
        .unwrap();
    let (second, outcome) = store
        .enqueue(&circuit, &json!({"b": "2", "a": "1"}), None, 3)
        .await
        .unwrap();

    assert_eq!(outcome, Enqueued::Duplicate);
    assert_eq!(first.id, second.id);
}

#[tokio::test]
async fn different_inputs_create_different_jobs() {
    let store = store().await;
    let circuit = fresh_circuit(&store).await;

    let (a, _) = store
        .enqueue(&circuit, &json!({"v": "1"}), None, 3)
        .await
        .unwrap();
    let (b, outcome) = store
        .enqueue(&circuit, &json!({"v": "2"}), None, 3)
        .await
        .unwrap();

    assert_eq!(outcome, Enqueued::Created);
    assert_ne!(a.id, b.id);
}

/// The spec's explicit requirement: fifty concurrent identical submissions must
/// produce exactly one job.
///
/// This is the test that a read-then-write implementation fails. Every task
/// checks "does it exist?" at the same moment, every task sees no, and every
/// task inserts. Only a database-level unique constraint makes this safe, and
/// only running it concurrently proves it.
#[tokio::test]
async fn fifty_concurrent_identical_submissions_create_one_job() {
    let store = store().await;
    let circuit = fresh_circuit(&store).await;
    let inputs = json!({"secret": "concurrent", "n": 50});

    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..50 {
        let store = store.clone();
        let circuit = circuit.clone();
        let inputs = inputs.clone();
        tasks.spawn(async move { store.enqueue(&circuit, &inputs, None, 3).await });
    }

    let mut ids = Vec::new();
    let mut created = 0;
    let mut duplicates = 0;
    while let Some(result) = tasks.join_next().await {
        let (job, outcome) = result.expect("task panicked").expect("enqueue failed");
        ids.push(job.id);
        match outcome {
            Enqueued::Created => created += 1,
            Enqueued::Duplicate => duplicates += 1,
        }
    }

    assert_eq!(ids.len(), 50, "every submission should have got an answer");
    assert_eq!(
        created, 1,
        "exactly one submission should have created the job"
    );
    assert_eq!(duplicates, 49);

    let first = ids[0];
    assert!(
        ids.iter().all(|&id| id == first),
        "all callers must get the same job"
    );
}

// ---------------------------------------------------------------------------
// Durability
// ---------------------------------------------------------------------------

/// Reconnecting is the closest thing to a service restart that a test can do,
/// and it is what the exit criterion "requests persist across a full service
/// restart" actually means: nothing is held in process memory.
#[tokio::test]
async fn jobs_survive_a_reconnect() {
    let circuit;
    let job_id;
    {
        let store = store().await;
        circuit = fresh_circuit(&store).await;
        let (job, _) = store
            .enqueue(&circuit, &json!({"durable": true}), Some("k"), 3)
            .await
            .unwrap();
        job_id = job.id;
    }

    let reconnected = store().await;
    let found = reconnected
        .job(job_id)
        .await
        .unwrap()
        .expect("job vanished");

    assert_eq!(found.id, job_id);
    assert_eq!(found.state, JobState::Queued);
    assert_eq!(found.inputs, json!({"durable": true}));
}

#[tokio::test]
async fn a_job_can_be_found_by_its_content_hash() {
    let store = store().await;
    let circuit = fresh_circuit(&store).await;
    let inputs = json!({"find": "me"});

    let (job, _) = store.enqueue(&circuit, &inputs, None, 3).await.unwrap();
    let hash = dray_core::job_hash(&circuit, &inputs).unwrap();

    let found = store
        .job_by_hash(&hash)
        .await
        .unwrap()
        .expect("not found by hash");
    assert_eq!(found.id, job.id);
}

// ---------------------------------------------------------------------------
// State transitions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_happy_path_walks_the_state_machine() {
    let store = store().await;
    let circuit = fresh_circuit(&store).await;
    let (job, _) = store
        .enqueue(&circuit, &json!({"walk": 1}), None, 3)
        .await
        .unwrap();

    for (event, expected) in [
        (JobEvent::Leased, JobState::Leased),
        (JobEvent::ProvingStarted, JobState::Proving),
    ] {
        let updated = store
            .apply_event(job.id, event, Some("worker-1"), None)
            .await
            .unwrap();
        assert_eq!(updated.state, expected, "after {event}");
    }
}

#[tokio::test]
async fn an_illegal_transition_is_rejected_and_changes_nothing() {
    let store = store().await;
    let circuit = fresh_circuit(&store).await;
    let (job, _) = store
        .enqueue(&circuit, &json!({"illegal": 1}), None, 3)
        .await
        .unwrap();

    // A queued job has not been proved, so it cannot be submitted.
    let result = store
        .apply_event(job.id, JobEvent::SubmissionStarted, None, None)
        .await;
    assert!(matches!(result, Err(StoreError::IllegalTransition(_))));

    let unchanged = store.job(job.id).await.unwrap().unwrap();
    assert_eq!(
        unchanged.state,
        JobState::Queued,
        "rejected event must not mutate state"
    );
}

/// A worker dying must not leave its name on a job that has returned to the
/// queue, or the operator CLI would report a job as held by a process that no
/// longer exists.
#[tokio::test]
async fn losing_a_lease_clears_the_lease_holder() {
    let store = store().await;
    let circuit = fresh_circuit(&store).await;
    let (job, _) = store
        .enqueue(&circuit, &json!({"lease": 1}), None, 3)
        .await
        .unwrap();

    store
        .apply_event(job.id, JobEvent::Leased, Some("worker-1"), None)
        .await
        .unwrap();
    let expired = store
        .apply_event(job.id, JobEvent::LeaseExpired, Some("reaper"), None)
        .await
        .unwrap();

    assert_eq!(expired.state, JobState::Queued);
    assert_eq!(
        expired.leased_by, None,
        "lease holder should have been cleared"
    );
}

#[tokio::test]
async fn transitions_are_recorded_for_audit() {
    let store = store().await;
    let circuit = fresh_circuit(&store).await;
    let (job, _) = store
        .enqueue(&circuit, &json!({"audit": 1}), None, 3)
        .await
        .unwrap();

    store
        .apply_event(job.id, JobEvent::Leased, Some("w"), None)
        .await
        .unwrap();
    store
        .apply_event(job.id, JobEvent::ProvingStarted, Some("w"), None)
        .await
        .unwrap();
    store
        .apply_event(
            job.id,
            JobEvent::LeaseExpired,
            Some("reaper"),
            Some("worker died"),
        )
        .await
        .unwrap();

    let history = store.transitions(job.id).await.unwrap();
    assert_eq!(
        history,
        vec![
            (JobState::Queued, JobEvent::Leased, JobState::Leased),
            (
                JobState::Leased,
                JobEvent::ProvingStarted,
                JobState::Proving
            ),
            (JobState::Proving, JobEvent::LeaseExpired, JobState::Queued),
        ]
    );
}

/// Two actors racing to transition the same job must serialise. One wins; the
/// other's event is illegal against the new state and is rejected. Both
/// succeeding would mean the job took two paths at once.
#[tokio::test]
async fn concurrent_transitions_on_one_job_serialise() {
    let store = store().await;
    let circuit = fresh_circuit(&store).await;
    let (job, _) = store
        .enqueue(&circuit, &json!({"race": 1}), None, 3)
        .await
        .unwrap();

    let mut tasks = tokio::task::JoinSet::new();
    for worker in 0..8 {
        let store = store.clone();
        let id = job.id;
        tasks.spawn(async move {
            store
                .apply_event(
                    id,
                    JobEvent::Leased,
                    Some(&format!("worker-{worker}")),
                    None,
                )
                .await
        });
    }

    let mut winners = 0;
    while let Some(result) = tasks.join_next().await {
        if result.expect("task panicked").is_ok() {
            winners += 1;
        }
    }

    assert_eq!(winners, 1, "exactly one worker should have won the lease");
    assert_eq!(
        store.job(job.id).await.unwrap().unwrap().state,
        JobState::Leased
    );
}

// ---------------------------------------------------------------------------
// Circuit registration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_circuit_is_rejected() {
    let store = store().await;
    let result = store.enqueue("no_such_circuit", &json!({}), None, 3).await;
    assert!(matches!(result, Err(StoreError::CircuitNotFound(_))));
}

#[tokio::test]
async fn a_disabled_circuit_stops_accepting_work() {
    let store = store().await;
    let circuit = fresh_circuit(&store).await;

    store
        .upsert_circuit(&Circuit {
            id: circuit.clone(),
            display_name: "test".into(),
            input_schema: json!({"type": "object"}),
            verifier_address: None,
            enabled: false,
        })
        .await
        .unwrap();

    let result = store.enqueue(&circuit, &json!({"v": 1}), None, 3).await;
    assert!(matches!(result, Err(StoreError::CircuitDisabled(_))));
}

// ---------------------------------------------------------------------------
// Schema constraints
// ---------------------------------------------------------------------------

/// Floats are refused before they ever reach the database. Without this the
/// canonical form would be ambiguous and idempotency would silently weaken.
#[tokio::test]
async fn float_inputs_are_refused() {
    let store = store().await;
    let circuit = fresh_circuit(&store).await;

    let result = store
        .enqueue(&circuit, &json!({"value": 1.5}), None, 3)
        .await;
    assert!(matches!(result, Err(StoreError::Canonical(_))));
}

/// The database enforces the proof invariant independently of the application:
/// a job cannot claim to be proved with no proof attached.
#[tokio::test]
async fn the_database_refuses_a_proved_job_without_a_proof() {
    let store = store().await;
    let circuit = fresh_circuit(&store).await;
    let (job, _) = store
        .enqueue(&circuit, &json!({"constraint": 1}), None, 3)
        .await
        .unwrap();

    let result = sqlx::query("UPDATE jobs SET state = 'proved' WHERE id = $1")
        .bind(job.id)
        .execute(store.pool())
        .await;

    assert!(result.is_err(), "check constraint should have refused this");
}

/// `queue_depth` is global by design — it feeds backpressure, which cares about
/// total load rather than any one circuit.
///
/// That makes it awkward to assert on from a test suite sharing a database:
/// other tests enqueue and lease jobs concurrently, so the global figure moves
/// under us. An earlier version of this test compared a global before and after
/// and passed only by luck.
///
/// So the exact assertion is scoped to this test's own circuit, and the global
/// method is checked for the weaker property that actually holds under
/// concurrency: it must account for at least the jobs this test queued.
#[tokio::test]
async fn queue_depth_counts_queued_jobs() {
    let store = store().await;
    let circuit = fresh_circuit(&store).await;

    let scoped = |circuit: String| {
        let store = store.clone();
        async move {
            let row = sqlx::query(
                "SELECT count(*) AS n FROM jobs WHERE circuit_id = $1 AND state = 'queued'",
            )
            .bind(&circuit)
            .fetch_one(store.pool())
            .await
            .unwrap();
            sqlx::Row::try_get::<i64, _>(&row, "n").unwrap()
        }
    };

    assert_eq!(
        scoped(circuit.clone()).await,
        0,
        "a fresh circuit starts empty"
    );

    for i in 0..3 {
        store
            .enqueue(&circuit, &json!({"depth": i}), None, 3)
            .await
            .unwrap();
    }

    assert_eq!(
        scoped(circuit.clone()).await,
        3,
        "three queued jobs for this circuit"
    );
    assert!(
        store.queue_depth().await.unwrap() >= 3,
        "the global depth must account for at least this test's jobs"
    );
}

// ---------------------------------------------------------------------------
// Leasing
//
// The properties here are what make at-least-once delivery work without leader
// election, and they cannot be checked without a real database: the mechanism
// is `FOR UPDATE SKIP LOCKED` and row-level locking.
//
// Unlike the tests above, these need a database of their own. `lease_next`
// takes the oldest job in the *whole* queue — that is the point of it — so a
// unique circuit id gives no isolation here, and sibling tests running
// concurrently would lease each other's jobs. Each test below therefore gets
// its own database.
// ---------------------------------------------------------------------------

use std::time::Duration;

/// Creates a database private to one test, migrates it, and returns a store.
///
/// Databases are named after the process and the test, so a rerun reuses the
/// name rather than accumulating them.
async fn isolated_store(label: &str) -> Store {
    let admin_url = database_url();
    let db = format!("dray_lease_{}_{label}", std::process::id());

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
    let store = Store::connect(&format!("{prefix}/{db}"), 24)
        .await
        .expect("could not connect to the isolated database");
    store.migrate().await.expect("migrations failed");
    store
}

/// Registers a circuit in an isolated store and enqueues `count` jobs.
async fn seed_jobs(store: &Store, count: usize) -> Vec<uuid::Uuid> {
    store
        .upsert_circuit(&Circuit {
            id: "c".into(),
            display_name: "test".into(),
            input_schema: json!({"type": "object"}),
            verifier_address: None,
            enabled: true,
        })
        .await
        .expect("could not register circuit");

    let mut ids = Vec::with_capacity(count);
    for i in 0..count {
        let (job, _) = store
            .enqueue("c", &json!({"n": i}), None, 3)
            .await
            .expect("enqueue failed");
        ids.push(job.id);
    }
    ids
}

#[tokio::test]
async fn leasing_an_empty_queue_returns_nothing() {
    let store = isolated_store("empty").await;
    assert!(
        store
            .lease_next("worker-1", Duration::from_secs(30))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn leasing_takes_the_oldest_job_and_counts_the_attempt() {
    let store = isolated_store("oldest").await;
    let ids = seed_jobs(&store, 3).await;

    let leased = store
        .lease_next("worker-1", Duration::from_secs(30))
        .await
        .unwrap()
        .expect("a job should have been available");

    assert_eq!(leased.id, ids[0], "the queue is oldest-first");
    assert_eq!(leased.state, JobState::Leased);
    assert_eq!(leased.leased_by.as_deref(), Some("worker-1"));
    assert_eq!(
        leased.attempts, 1,
        "the attempt is counted at lease time, not on completion — a worker \
         killed mid-proof never reports anything, so counting on success would \
         let a poison job retry forever"
    );
}

#[tokio::test]
async fn a_leased_job_is_not_offered_again() {
    let store = isolated_store("not_twice").await;
    seed_jobs(&store, 1).await;

    assert!(
        store
            .lease_next("worker-1", Duration::from_secs(30))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .lease_next("worker-2", Duration::from_secs(30))
            .await
            .unwrap()
            .is_none(),
        "the only job is already held"
    );
}

/// The core safety property of the pool: no two workers may hold the same job.
///
/// Twenty workers race for ten jobs. Every job must go to exactly one worker.
/// Without `FOR UPDATE SKIP LOCKED`, this is where duplicate proving work would
/// appear — and duplicate proving means duplicate settlement attempts.
#[tokio::test]
async fn no_two_workers_ever_hold_the_same_job() {
    let store = isolated_store("contention").await;
    let expected: std::collections::HashSet<_> = seed_jobs(&store, 10).await.into_iter().collect();

    let mut tasks = tokio::task::JoinSet::new();
    for worker in 0..20 {
        let store = store.clone();
        tasks.spawn(async move {
            let mut mine = Vec::new();
            loop {
                match store
                    .lease_next(&format!("worker-{worker}"), Duration::from_secs(60))
                    .await
                {
                    Ok(Some(job)) => mine.push(job.id),
                    Ok(None) => break,
                    Err(e) => panic!("lease failed: {e}"),
                }
            }
            mine
        });
    }

    let mut all = Vec::new();
    while let Some(result) = tasks.join_next().await {
        all.extend(result.expect("task panicked"));
    }

    let unique: std::collections::HashSet<_> = all.iter().copied().collect();
    assert_eq!(
        all.len(),
        unique.len(),
        "a job was leased to more than one worker"
    );
    assert_eq!(
        unique, expected,
        "every job should have been leased exactly once"
    );
}

#[tokio::test]
async fn only_the_holder_can_renew_a_lease() {
    let store = isolated_store("renew").await;
    let ids = seed_jobs(&store, 1).await;
    store
        .lease_next("holder", Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();

    assert!(
        store
            .renew_lease(ids[0], "holder", Duration::from_secs(60))
            .await
            .unwrap()
    );

    // Otherwise a worker whose lease was already reaped could keep heartbeating
    // a job that now belongs to somebody else.
    assert!(
        !store
            .renew_lease(ids[0], "impostor", Duration::from_secs(60))
            .await
            .unwrap(),
        "a non-holder must not be able to renew"
    );
}

/// A worker dying is an ordinary, recoverable event. This is at-least-once
/// delivery, and it is why no leader election is needed.
#[tokio::test]
async fn an_expired_lease_returns_the_job_to_the_queue() {
    let store = isolated_store("expiry").await;
    let ids = seed_jobs(&store, 1).await;
    store
        .lease_next("doomed-worker", Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();

    // As though the worker died a moment ago.
    sqlx::query("UPDATE jobs SET lease_expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(ids[0])
        .execute(store.pool())
        .await
        .unwrap();

    let reaped = store.reap_expired_leases("reaper").await.unwrap();
    assert_eq!(reaped, vec![ids[0]]);

    let recovered = store.job(ids[0]).await.unwrap().unwrap();
    assert_eq!(recovered.state, JobState::Queued);
    assert_eq!(
        recovered.leased_by, None,
        "a dead worker's name must not linger"
    );
    assert_eq!(
        recovered.attempts, 1,
        "the failed attempt still counts against the budget"
    );

    // And it is immediately available to somebody else.
    let released = store
        .lease_next("worker-2", Duration::from_secs(30))
        .await
        .unwrap()
        .expect("the reaped job should be leasable again");
    assert_eq!(released.id, ids[0]);
    assert_eq!(released.attempts, 2);
}

#[tokio::test]
async fn a_live_lease_is_not_reaped() {
    let store = isolated_store("live").await;
    seed_jobs(&store, 1).await;
    store
        .lease_next("healthy-worker", Duration::from_secs(300))
        .await
        .unwrap()
        .unwrap();

    assert!(
        store
            .reap_expired_leases("reaper")
            .await
            .unwrap()
            .is_empty(),
        "a lease that has not expired must be left alone"
    );
}

#[tokio::test]
async fn a_successful_proof_is_recorded_with_the_job() {
    let store = isolated_store("proved").await;
    let ids = seed_jobs(&store, 1).await;
    store
        .lease_next("worker-1", Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    store.begin_proving(ids[0], "worker-1").await.unwrap();

    let proved = store
        .record_proof(
            ids[0],
            "worker-1",
            b"proof-bytes",
            b"public-inputs",
            2470,
            Some(42_000),
        )
        .await
        .unwrap();

    assert_eq!(proved.state, JobState::Proved);
    assert_eq!(proved.proof.as_deref(), Some(b"proof-bytes".as_slice()));
    assert_eq!(proved.leased_by, None, "the lease is released once proved");

    // A proved job must never be handed back out for proving.
    assert!(
        store
            .lease_next("worker-2", Duration::from_secs(30))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_transient_failure_returns_the_job_for_retry() {
    let store = isolated_store("transient").await;
    let ids = seed_jobs(&store, 1).await;
    store
        .lease_next("worker-1", Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();

    let failed = store
        .record_failure(
            ids[0],
            "worker-1",
            dray_core::FailureKind::Transient,
            "bb exited 137",
            None,
        )
        .await
        .unwrap();

    assert_eq!(failed.state, JobState::Queued, "attempts remain");
    assert_eq!(failed.last_error.as_deref(), Some("bb exited 137"));
}

#[tokio::test]
async fn attempts_are_exhausted_after_the_budget() {
    let store = isolated_store("exhausted").await;
    store
        .upsert_circuit(&Circuit {
            id: "c".into(),
            display_name: "test".into(),
            input_schema: json!({"type": "object"}),
            verifier_address: None,
            enabled: true,
        })
        .await
        .unwrap();
    let (job, _) = store
        .enqueue("c", &json!({"budget": 1}), None, 2)
        .await
        .unwrap();

    for _ in 0..2 {
        store
            .lease_next("worker-1", Duration::from_secs(60))
            .await
            .unwrap()
            .expect("job should be leasable");
        store
            .record_failure(
                job.id,
                "worker-1",
                dray_core::FailureKind::Transient,
                "transient",
                None,
            )
            .await
            .unwrap();
    }

    assert_eq!(
        store.job(job.id).await.unwrap().unwrap().state,
        JobState::Failed,
        "a job must not be retried forever"
    );
}

/// Retrying a malformed input three times costs three times as much and gives
/// the same answer.
#[tokio::test]
async fn a_permanent_failure_is_not_retried() {
    let store = isolated_store("permanent").await;
    let ids = seed_jobs(&store, 1).await;
    store
        .lease_next("worker-1", Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();

    let failed = store
        .record_failure(
            ids[0],
            "worker-1",
            dray_core::FailureKind::Permanent,
            "witness does not satisfy the circuit",
            None,
        )
        .await
        .unwrap();

    assert_eq!(failed.state, JobState::Failed);
    assert_eq!(failed.attempts, 1, "it should not have burned the budget");
}

#[tokio::test]
async fn a_released_lease_returns_the_job_immediately() {
    let store = isolated_store("release").await;
    let ids = seed_jobs(&store, 1).await;
    store
        .lease_next("departing-worker", Duration::from_secs(300))
        .await
        .unwrap()
        .unwrap();

    let released = store
        .release_lease(ids[0], "departing-worker")
        .await
        .unwrap();

    // Graceful shutdown must not make the next worker wait out the whole TTL.
    assert_eq!(released.state, JobState::Queued);
    assert_eq!(released.leased_by, None);
    assert!(
        store
            .lease_next("worker-2", Duration::from_secs(30))
            .await
            .unwrap()
            .is_some()
    );
}

/// The audit trail must survive a full lease-fail-release cycle, because "where
/// did this job actually go" is the question that matters during an incident.
#[tokio::test]
async fn the_transition_history_records_the_whole_cycle() {
    let store = isolated_store("history").await;
    let ids = seed_jobs(&store, 1).await;

    store
        .lease_next("worker-1", Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    store.begin_proving(ids[0], "worker-1").await.unwrap();
    store
        .record_failure(
            ids[0],
            "worker-1",
            dray_core::FailureKind::Transient,
            "timed out",
            None,
        )
        .await
        .unwrap();
    store
        .lease_next("worker-2", Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    store.begin_proving(ids[0], "worker-2").await.unwrap();
    store
        .record_proof(ids[0], "worker-2", b"p", b"pi", 100, None)
        .await
        .unwrap();

    let history = store.transitions(ids[0]).await.unwrap();
    assert_eq!(
        history,
        vec![
            (JobState::Queued, JobEvent::Leased, JobState::Leased),
            (
                JobState::Leased,
                JobEvent::ProvingStarted,
                JobState::Proving
            ),
            (
                JobState::Proving,
                JobEvent::RetryScheduled,
                JobState::Queued
            ),
            (JobState::Queued, JobEvent::Leased, JobState::Leased),
            (
                JobState::Leased,
                JobEvent::ProvingStarted,
                JobState::Proving
            ),
            (
                JobState::Proving,
                JobEvent::ProofSucceeded,
                JobState::Proved
            ),
        ]
    );
}

// ---------------------------------------------------------------------------
// Retry scheduling
//
// Backoff is only real if it is durable. A worker that slept locally before
// retrying would change nothing: it releases the job on failure, and the next
// worker to call `lease_next` takes it immediately. These tests exercise the
// delay from the queue's side, which is the side that matters.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_scheduled_retry_is_not_leasable_before_it_is_due() {
    let store = isolated_store("retry_pending").await;
    let ids = seed_jobs(&store, 1).await;

    store
        .lease_next("worker-1", Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();

    let failed = store
        .record_failure(
            ids[0],
            "worker-1",
            dray_core::FailureKind::Transient,
            "bb exited 137",
            Some(Duration::from_secs(3600)),
        )
        .await
        .unwrap();

    assert_eq!(failed.state, JobState::Queued);
    assert!(
        failed.retry_after.is_some(),
        "a delayed retry should be recorded on the row"
    );

    assert!(
        store
            .lease_next("worker-2", Duration::from_secs(60))
            .await
            .unwrap()
            .is_none(),
        "a job backing off must not be handed to the next worker to ask"
    );

    // Still counted as queued depth: it is waiting, not gone. An operator
    // watching the queue should see it.
    assert_eq!(store.queue_depth().await.unwrap(), 1);
}

#[tokio::test]
async fn a_scheduled_retry_becomes_leasable_once_it_is_due() {
    let store = isolated_store("retry_due").await;
    let ids = seed_jobs(&store, 1).await;

    store
        .lease_next("worker-1", Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    store
        .record_failure(
            ids[0],
            "worker-1",
            dray_core::FailureKind::Transient,
            "transient",
            Some(Duration::from_millis(300)),
        )
        .await
        .unwrap();

    assert!(
        store
            .lease_next("worker-2", Duration::from_secs(60))
            .await
            .unwrap()
            .is_none(),
        "not due yet"
    );

    tokio::time::sleep(Duration::from_millis(600)).await;

    let leased = store
        .lease_next("worker-2", Duration::from_secs(60))
        .await
        .unwrap()
        .expect("should be leasable once the delay has elapsed");

    assert_eq!(leased.id, ids[0]);
    assert_eq!(leased.attempts, 2, "the retry is a second attempt");
    assert!(
        leased.retry_after.is_none(),
        "leasing must clear the schedule, or the field would outlive its meaning"
    );
}

/// A backed-off job must not block jobs that are ready. `SKIP LOCKED` handles
/// contention; this is the ordering case, and getting it wrong would stall the
/// whole queue behind one unlucky job.
#[tokio::test]
async fn a_job_backing_off_does_not_block_a_ready_one() {
    let store = isolated_store("retry_headline").await;
    let ids = seed_jobs(&store, 2).await;

    // Fail the *oldest* job, so it would sort first without the filter.
    store
        .lease_next("worker-1", Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    store
        .record_failure(
            ids[0],
            "worker-1",
            dray_core::FailureKind::Transient,
            "transient",
            Some(Duration::from_secs(3600)),
        )
        .await
        .unwrap();

    let leased = store
        .lease_next("worker-2", Duration::from_secs(60))
        .await
        .unwrap()
        .expect("the second job is ready and must still be served");

    assert_eq!(leased.id, ids[1]);
}

/// A terminal job has no retry, and the schema refuses one — a scheduled retry
/// on a job nothing will ever lease would be invisible and permanent.
#[tokio::test]
async fn a_permanent_failure_schedules_no_retry() {
    let store = isolated_store("retry_permanent").await;
    let ids = seed_jobs(&store, 1).await;

    store
        .lease_next("worker-1", Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();

    let failed = store
        .record_failure(
            ids[0],
            "worker-1",
            dray_core::FailureKind::Permanent,
            "witness does not satisfy the circuit",
            Some(Duration::from_secs(3600)),
        )
        .await
        .expect("a delay on a permanent failure should be ignored, not rejected");

    assert_eq!(failed.state, JobState::Failed);
    assert!(
        failed.retry_after.is_none(),
        "a terminal job must carry no retry schedule"
    );
}

/// Exhausting the attempt budget is also terminal, and takes the same path.
#[tokio::test]
async fn an_exhausted_job_schedules_no_retry() {
    let store = isolated_store("retry_exhausted").await;
    let ids = seed_jobs(&store, 1).await;

    let mut last = None;
    for _ in 0..3 {
        store
            .lease_next("worker-1", Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap();
        last = Some(
            store
                .record_failure(
                    ids[0],
                    "worker-1",
                    dray_core::FailureKind::Transient,
                    "transient",
                    // No delay, so the next attempt can be taken immediately.
                    None,
                )
                .await
                .unwrap(),
        );
    }

    let failed = last.expect("three attempts were made");
    assert_eq!(failed.state, JobState::Failed, "budget is three attempts");
    assert!(failed.retry_after.is_none());
}

/// The database's clock is authoritative. Two workers with skewed clocks must
/// agree on when a job is due, so the interval is computed from `now()` inside
/// the same statement rather than from a timestamp a worker calculated.
#[tokio::test]
async fn the_retry_deadline_is_measured_by_the_database() {
    let store = isolated_store("retry_clock").await;
    let ids = seed_jobs(&store, 1).await;

    store
        .lease_next("worker-1", Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();

    let before = chrono::Utc::now();
    let failed = store
        .record_failure(
            ids[0],
            "worker-1",
            dray_core::FailureKind::Transient,
            "transient",
            Some(Duration::from_secs(60)),
        )
        .await
        .unwrap();
    let after = chrono::Utc::now();

    let retry_after = failed.retry_after.expect("should be scheduled");
    assert!(
        retry_after >= before + chrono::Duration::seconds(59)
            && retry_after <= after + chrono::Duration::seconds(61),
        "retry_after {retry_after} is not about 60s from now ({before} .. {after})"
    );
}

/// A job whose lease expires has spent an attempt, and the budget has to be
/// honoured on that path too. Without this, an input that kills whatever worker
/// touches it would be handed out for ever, taking down one worker after
/// another — which is precisely what the attempt counter exists to bound.
#[tokio::test]
async fn reaping_fails_a_job_whose_attempts_are_exhausted() {
    let store = isolated_store("reap_exhausted").await;
    let ids = seed_jobs(&store, 1).await;

    // Lease and abandon three times, matching the default budget. A zero TTL
    // means the lease is already expired when the reaper looks.
    for expected_attempt in 1..=3 {
        let leased = store
            .lease_next("doomed-worker", Duration::from_secs(0))
            .await
            .unwrap()
            .expect("should still be leasable");
        assert_eq!(leased.attempts, expected_attempt);

        let reaped = store.reap_expired_leases("reaper").await.unwrap();
        assert_eq!(reaped, vec![ids[0]]);
    }

    let job = store.job(ids[0]).await.unwrap().unwrap();
    assert_eq!(
        job.state,
        JobState::Failed,
        "a job that has burned its whole budget on expired leases must stop \
         being handed out"
    );
    assert!(job.leased_by.is_none());
    assert!(
        job.last_error
            .as_deref()
            .is_some_and(|e| e.contains("attempts exhausted")),
        "the reason should say what happened: {:?}",
        job.last_error
    );

    assert!(
        store
            .lease_next("another-worker", Duration::from_secs(60))
            .await
            .unwrap()
            .is_none(),
        "a failed job must not be leasable"
    );
}

/// Below the budget, an expired lease is an ordinary return to the queue.
#[tokio::test]
async fn reaping_returns_a_job_with_attempts_left_to_the_queue() {
    let store = isolated_store("reap_retryable").await;
    let ids = seed_jobs(&store, 1).await;

    store
        .lease_next("doomed-worker", Duration::from_secs(0))
        .await
        .unwrap()
        .unwrap();

    let reaped = store.reap_expired_leases("reaper").await.unwrap();
    assert_eq!(reaped, vec![ids[0]]);

    let job = store.job(ids[0]).await.unwrap().unwrap();
    assert_eq!(job.state, JobState::Queued);
    assert_eq!(job.attempts, 1, "the attempt was still spent");
    assert!(
        job.retry_after.is_none(),
        "an expired lease should be retryable at once; the job may simply have \
         outlived its worker"
    );

    let history = store.transitions(ids[0]).await.unwrap();
    assert!(
        history
            .iter()
            .any(|(_, event, _)| *event == JobEvent::LeaseExpired),
        "the log should say the lease expired, not invent a decision: {history:?}"
    );
}
