//! End-to-end HTTP tests against a real database.
//!
//! These drive the actual router with `tower::ServiceExt::oneshot`, so requests
//! pass through real extractors, real handlers, and a real Postgres. No mocks:
//! a mocked store would not have caught the enum-decoding bug that the store's
//! own integration tests did.
//!
//! Gated behind the `integration-tests` feature for the same reason as
//! `dray-store`'s — a runtime skip when `DATABASE_URL` is unset would let a
//! broken handler pass for green.
//!
//! ```sh
//! make up
//! DATABASE_URL=postgres://dray:dray@localhost:5432/dray \
//!     cargo test -p dray-api --features integration-tests
//! ```

#![cfg(feature = "integration-tests")]
// Clippy's `allow-unwrap-in-tests` covers `#[test]` functions but not the
// helpers around them, and these helpers are test scaffolding: a panic in
// `body_json` is a failing test, which is exactly what should happen.
#![allow(clippy::unwrap_used)]

use std::sync::atomic::{AtomicU64, Ordering};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use dray_api::{
    api::{AppState, router},
    validate::Limits,
};
use dray_store::{Circuit, Store};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// The membership circuit's real input schema.
fn membership_schema() -> Value {
    json!({
        "type": "object",
        "required": ["secret", "leaf_index", "siblings"],
        "additionalProperties": false,
        "properties": {
            "secret": {"type": "string"},
            "leaf_index": {"type": "string"},
            "siblings": {
                "type": "array",
                "items": {"type": "string"},
                "minItems": 20,
                "maxItems": 20,
            },
        },
    })
}

fn valid_inputs() -> Value {
    json!({
        "secret": "42",
        "leaf_index": "5",
        "siblings": vec!["7"; 20],
    })
}

async fn harness() -> (Router, String) {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set to run the integration tests");
    let store = Store::connect(&url, 8).await.expect("could not connect");
    store.migrate().await.expect("migrations failed");

    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let circuit_id = format!("http_test_{}_{n}", std::process::id());
    store
        .upsert_circuit(&Circuit {
            id: circuit_id.clone(),
            display_name: "http test".into(),
            input_schema: membership_schema(),
            verifier_address: None,
            enabled: true,
        })
        .await
        .expect("could not register circuit");

    let app = router(AppState {
        store,
        limits: Limits::default(),
        max_queue_depth: 10_000,
        default_max_attempts: 3,
    });

    (app, circuit_id)
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("response should be JSON")
}

fn post(circuit_id: &str, inputs: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/proofs")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"circuit_id": circuit_id, "inputs": inputs}).to_string(),
        ))
        .unwrap()
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_valid_submission_is_accepted() {
    let (app, circuit) = harness().await;

    let response = app.oneshot(post(&circuit, valid_inputs())).await.unwrap();

    // 202, not 200: the work is durable but not done.
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let body = body_json(response).await;
    assert_eq!(body["state"], "queued");
    assert_eq!(body["created"], true);
    assert!(body["job_id"].is_string());
    assert_eq!(
        body["job_hash"].as_str().unwrap().len(),
        64,
        "job_hash should be a hex-encoded SHA-256"
    );
}

#[tokio::test]
async fn a_duplicate_submission_returns_the_same_job() {
    let (app, circuit) = harness().await;

    let first = body_json(
        app.clone()
            .oneshot(post(&circuit, valid_inputs()))
            .await
            .unwrap(),
    )
    .await;

    let response = app.oneshot(post(&circuit, valid_inputs())).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "a repeat is not an error"
    );

    let second = body_json(response).await;
    assert_eq!(first["job_id"], second["job_id"]);
    assert_eq!(
        second["created"], false,
        "the client should learn it was a repeat"
    );
}

#[tokio::test]
async fn a_submitted_job_can_be_read_back() {
    let (app, circuit) = harness().await;

    let submitted = body_json(
        app.clone()
            .oneshot(post(&circuit, valid_inputs()))
            .await
            .unwrap(),
    )
    .await;
    let job_id = submitted["job_id"].as_str().unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/proofs/{job_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["job_id"], job_id);
    assert_eq!(body["state"], "queued");
    assert_eq!(body["attempts"], 0);
    assert_eq!(body["circuit_id"], circuit);
    assert!(
        body["proof_size_bytes"].is_null(),
        "nothing has been proved yet"
    );
}

#[tokio::test]
async fn an_unknown_job_is_a_404() {
    let (app, _) = harness().await;

    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/proofs/{}", uuid::Uuid::new_v4()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(response).await["error"], "job_not_found");
}

#[tokio::test]
async fn an_unknown_circuit_is_a_404() {
    let (app, _) = harness().await;

    let response = app
        .oneshot(post("no_such_circuit", valid_inputs()))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(response).await["error"], "unknown_circuit");
}

#[tokio::test]
async fn inputs_that_break_the_schema_are_rejected() {
    let (app, circuit) = harness().await;

    // 19 siblings cannot satisfy a depth-20 tree. Rejecting it here saves a
    // worker a lease, a subprocess, and a doomed attempt.
    let short = json!({
        "secret": "42",
        "leaf_index": "5",
        "siblings": vec!["7"; 19],
    });

    let response = app.oneshot(post(&circuit, short)).await.unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["error"], "invalid_request");
    assert!(
        body["message"].as_str().unwrap().contains("schema"),
        "the client should be told what was wrong: {}",
        body["message"]
    );
}

#[tokio::test]
async fn unexpected_fields_are_rejected() {
    let (app, circuit) = harness().await;

    let mut inputs = valid_inputs();
    inputs["surprise"] = json!("1");

    let response = app.oneshot(post(&circuit, inputs)).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_float_input_is_rejected_with_a_client_error() {
    let (app, circuit) = harness().await;

    // Floats break canonicalisation and therefore idempotency. The client must
    // see a 400 explaining it, not a 500.
    let response = app
        .oneshot(post(
            &circuit,
            json!({"secret": 1.5, "leaf_index": "5", "siblings": vec!["7"; 20]}),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn malformed_json_is_rejected() {
    let (app, _) = harness().await;

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/proofs")
                .header("content-type", "application/json")
                .body(Body::from("{not json"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn inputs_that_are_not_an_object_are_rejected() {
    let (app, circuit) = harness().await;

    let response = app.oneshot(post(&circuit, json!([1, 2, 3]))).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_disabled_circuit_returns_conflict() {
    let (app, circuit) = harness().await;

    let url = std::env::var("DATABASE_URL").unwrap();
    let store = Store::connect(&url, 2).await.unwrap();
    store
        .upsert_circuit(&Circuit {
            id: circuit.clone(),
            display_name: "http test".into(),
            input_schema: membership_schema(),
            verifier_address: None,
            enabled: false,
        })
        .await
        .unwrap();

    let response = app.oneshot(post(&circuit, valid_inputs())).await.unwrap();

    // 409 rather than 404: the circuit exists, it just is not taking work. A
    // client should stop retrying but not conclude it used the wrong name.
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(body_json(response).await["error"], "circuit_disabled");
}

#[tokio::test]
async fn health_does_not_depend_on_the_database() {
    let (app, _) = harness().await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn readiness_reports_queue_depth() {
    let (app, _) = harness().await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_json(response).await["queue_depth"].is_number());
}

/// Fifty concurrent identical requests through the full HTTP stack must yield
/// one job. The store proves the database constraint holds; this proves nothing
/// above it undoes that.
#[tokio::test]
async fn concurrent_identical_requests_create_one_job() {
    let (app, circuit) = harness().await;

    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..50 {
        let app = app.clone();
        let circuit = circuit.clone();
        tasks.spawn(async move {
            let response = app.oneshot(post(&circuit, valid_inputs())).await.unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            body_json(response).await
        });
    }

    let mut ids = Vec::new();
    let mut created = 0;
    while let Some(body) = tasks.join_next().await {
        let body = body.expect("task panicked");
        if body["created"] == true {
            created += 1;
        }
        ids.push(body["job_id"].as_str().unwrap().to_owned());
    }

    assert_eq!(ids.len(), 50);
    assert_eq!(
        created, 1,
        "exactly one request should have created the job"
    );
    assert!(
        ids.iter().all(|id| *id == ids[0]),
        "all must name the same job"
    );
}
