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
    std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set to run the integration tests")
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
    assert_eq!(first.id, second.id, "a duplicate must not create a second job");
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

    let (a, _) = store.enqueue(&circuit, &json!({"v": "1"}), None, 3).await.unwrap();
    let (b, outcome) = store.enqueue(&circuit, &json!({"v": "2"}), None, 3).await.unwrap();

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
    assert_eq!(created, 1, "exactly one submission should have created the job");
    assert_eq!(duplicates, 49);

    let first = ids[0];
    assert!(ids.iter().all(|&id| id == first), "all callers must get the same job");
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
    let found = reconnected.job(job_id).await.unwrap().expect("job vanished");

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

    let found = store.job_by_hash(&hash).await.unwrap().expect("not found by hash");
    assert_eq!(found.id, job.id);
}

// ---------------------------------------------------------------------------
// State transitions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_happy_path_walks_the_state_machine() {
    let store = store().await;
    let circuit = fresh_circuit(&store).await;
    let (job, _) = store.enqueue(&circuit, &json!({"walk": 1}), None, 3).await.unwrap();

    for (event, expected) in [
        (JobEvent::Leased, JobState::Leased),
        (JobEvent::ProvingStarted, JobState::Proving),
    ] {
        let updated = store.apply_event(job.id, event, Some("worker-1"), None).await.unwrap();
        assert_eq!(updated.state, expected, "after {event}");
    }
}

#[tokio::test]
async fn an_illegal_transition_is_rejected_and_changes_nothing() {
    let store = store().await;
    let circuit = fresh_circuit(&store).await;
    let (job, _) = store.enqueue(&circuit, &json!({"illegal": 1}), None, 3).await.unwrap();

    // A queued job has not been proved, so it cannot be submitted.
    let result = store.apply_event(job.id, JobEvent::SubmissionStarted, None, None).await;
    assert!(matches!(result, Err(StoreError::IllegalTransition(_))));

    let unchanged = store.job(job.id).await.unwrap().unwrap();
    assert_eq!(unchanged.state, JobState::Queued, "rejected event must not mutate state");
}

/// A worker dying must not leave its name on a job that has returned to the
/// queue, or the operator CLI would report a job as held by a process that no
/// longer exists.
#[tokio::test]
async fn losing_a_lease_clears_the_lease_holder() {
    let store = store().await;
    let circuit = fresh_circuit(&store).await;
    let (job, _) = store.enqueue(&circuit, &json!({"lease": 1}), None, 3).await.unwrap();

    store.apply_event(job.id, JobEvent::Leased, Some("worker-1"), None).await.unwrap();
    let expired = store
        .apply_event(job.id, JobEvent::LeaseExpired, Some("reaper"), None)
        .await
        .unwrap();

    assert_eq!(expired.state, JobState::Queued);
    assert_eq!(expired.leased_by, None, "lease holder should have been cleared");
}

#[tokio::test]
async fn transitions_are_recorded_for_audit() {
    let store = store().await;
    let circuit = fresh_circuit(&store).await;
    let (job, _) = store.enqueue(&circuit, &json!({"audit": 1}), None, 3).await.unwrap();

    store.apply_event(job.id, JobEvent::Leased, Some("w"), None).await.unwrap();
    store.apply_event(job.id, JobEvent::ProvingStarted, Some("w"), None).await.unwrap();
    store
        .apply_event(job.id, JobEvent::LeaseExpired, Some("reaper"), Some("worker died"))
        .await
        .unwrap();

    let history = store.transitions(job.id).await.unwrap();
    assert_eq!(
        history,
        vec![
            (JobState::Queued, JobEvent::Leased, JobState::Leased),
            (JobState::Leased, JobEvent::ProvingStarted, JobState::Proving),
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
    let (job, _) = store.enqueue(&circuit, &json!({"race": 1}), None, 3).await.unwrap();

    let mut tasks = tokio::task::JoinSet::new();
    for worker in 0..8 {
        let store = store.clone();
        let id = job.id;
        tasks.spawn(async move {
            store.apply_event(id, JobEvent::Leased, Some(&format!("worker-{worker}")), None).await
        });
    }

    let mut winners = 0;
    while let Some(result) = tasks.join_next().await {
        if result.expect("task panicked").is_ok() {
            winners += 1;
        }
    }

    assert_eq!(winners, 1, "exactly one worker should have won the lease");
    assert_eq!(store.job(job.id).await.unwrap().unwrap().state, JobState::Leased);
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

    let result = store.enqueue(&circuit, &json!({"value": 1.5}), None, 3).await;
    assert!(matches!(result, Err(StoreError::Canonical(_))));
}

/// The database enforces the proof invariant independently of the application:
/// a job cannot claim to be proved with no proof attached.
#[tokio::test]
async fn the_database_refuses_a_proved_job_without_a_proof() {
    let store = store().await;
    let circuit = fresh_circuit(&store).await;
    let (job, _) = store.enqueue(&circuit, &json!({"constraint": 1}), None, 3).await.unwrap();

    let result = sqlx::query("UPDATE jobs SET state = 'proved' WHERE id = $1")
        .bind(job.id)
        .execute(store.pool())
        .await;

    assert!(result.is_err(), "check constraint should have refused this");
}

#[tokio::test]
async fn queue_depth_counts_queued_jobs() {
    let store = store().await;
    let circuit = fresh_circuit(&store).await;

    let before = store.queue_depth().await.unwrap();
    store.enqueue(&circuit, &json!({"depth": 1}), None, 3).await.unwrap();
    let after = store.queue_depth().await.unwrap();

    assert!(after > before, "queue depth should have grown");
}
