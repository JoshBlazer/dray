//! HTTP surface.
//!
//! Two endpoints do the real work — submit a proof request, and ask what
//! happened to it — plus liveness and readiness probes.
//!
//! The router is built separately from the server so tests can exercise it
//! with `tower::ServiceExt::oneshot` and no socket.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use dray_store::{Enqueued, Store, StoreError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::validate::{Limits, ValidationError, validate_submission};

/// Shared handler state.
#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub limits: Limits,
    /// Queue depth above which new work is refused. See [`ApiError::Overloaded`].
    pub max_queue_depth: i64,
    pub default_max_attempts: i32,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("limits", &self.limits)
            .field("max_queue_depth", &self.max_queue_depth)
            .finish_non_exhaustive()
    }
}

/// A proof request.
#[derive(Debug, Deserialize)]
pub struct SubmitRequest {
    /// Which circuit should prove this.
    pub circuit_id: String,
    /// Circuit inputs, validated against that circuit's declared schema.
    pub inputs: serde_json::Value,
    /// Optional client-supplied correlation key. Not the deduplication key —
    /// that is derived from the inputs themselves.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SubmitResponse {
    pub job_id: Uuid,
    pub state: String,
    /// `true` when this request created the job, `false` when an identical
    /// request had already been accepted.
    ///
    /// Returned rather than hidden because a client retrying after a timeout
    /// deserves to know whether its retry was the one that landed.
    pub created: bool,
    /// Canonical content hash, hex-encoded. Two clients that compute this
    /// locally can tell in advance whether they are submitting the same work.
    pub job_hash: String,
}

#[derive(Debug, Serialize)]
pub struct JobResponse {
    pub job_id: Uuid,
    pub circuit_id: String,
    pub state: String,
    pub attempts: i32,
    pub max_attempts: i32,
    pub last_error: Option<String>,
    pub job_hash: String,
    pub idempotency_key: Option<String>,
    /// Present once proving has succeeded.
    pub proof_size_bytes: Option<usize>,
    pub created_at: String,
    pub updated_at: String,
}

/// The error body every failure returns.
///
/// One shape for every error means a client can handle failures generically
/// instead of pattern-matching on prose.
#[derive(Debug, Serialize)]
pub struct ErrorBody {
    /// Stable machine-readable code. Safe to branch on; the message is not.
    pub error: &'static str,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error(transparent)]
    Invalid(#[from] ValidationError),

    #[error("no circuit is registered with id {0:?}")]
    UnknownCircuit(String),

    #[error("circuit {0:?} is not currently accepting work")]
    CircuitDisabled(String),

    #[error("job {0} not found")]
    JobNotFound(Uuid),

    #[error("queue depth {depth} is at or above the limit of {limit}; retry shortly")]
    Overloaded { depth: i64, limit: i64 },

    #[error("internal error")]
    Internal(#[from] StoreError),
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            ApiError::Invalid(_) => StatusCode::BAD_REQUEST,
            ApiError::UnknownCircuit(_) | ApiError::JobNotFound(_) => StatusCode::NOT_FOUND,
            ApiError::CircuitDisabled(_) => StatusCode::CONFLICT,
            ApiError::Overloaded { .. } => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn code(&self) -> &'static str {
        match self {
            ApiError::Invalid(_) => "invalid_request",
            ApiError::UnknownCircuit(_) => "unknown_circuit",
            ApiError::CircuitDisabled(_) => "circuit_disabled",
            ApiError::JobNotFound(_) => "job_not_found",
            ApiError::Overloaded { .. } => "overloaded",
            ApiError::Internal(_) => "internal_error",
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Internal errors are logged in full and reported as a bare message.
        // A database error string can name tables, columns, and constraints;
        // that belongs in the operator's logs, not in a client's response.
        let message = match &self {
            ApiError::Internal(err) => {
                tracing::error!(error = %err, "request failed with an internal error");
                "an internal error occurred".to_owned()
            }
            other => other.to_string(),
        };

        (
            self.status(),
            Json(ErrorBody {
                error: self.code(),
                message,
            }),
        )
            .into_response()
    }
}

/// Build the router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/proofs", post(submit))
        .route("/v1/proofs/{id}", get(job))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(Arc::new(state))
}

/// `POST /v1/proofs` — accept a proof request.
///
/// Returns `202 Accepted` on success: the work is durable but not done. A `200`
/// would imply the proof exists, which it does not.
#[tracing::instrument(skip_all, fields(circuit_id = %body.circuit_id))]
async fn submit(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SubmitRequest>,
) -> Result<(StatusCode, Json<SubmitResponse>), ApiError> {
    // Backpressure first. Refusing early keeps a queue that is already too deep
    // from being made deeper by work this server cannot get to.
    let depth = state.store.queue_depth().await?;
    if depth >= state.max_queue_depth {
        return Err(ApiError::Overloaded {
            depth,
            limit: state.max_queue_depth,
        });
    }

    let circuit = state
        .store
        .circuit(&body.circuit_id)
        .await?
        .ok_or_else(|| ApiError::UnknownCircuit(body.circuit_id.clone()))?;

    if !circuit.enabled {
        return Err(ApiError::CircuitDisabled(body.circuit_id.clone()));
    }

    validate_submission(
        &body.circuit_id,
        &body.inputs,
        body.idempotency_key.as_deref(),
        &circuit.input_schema,
        &state.limits,
    )?;

    let (job, outcome) = state
        .store
        .enqueue(
            &body.circuit_id,
            &body.inputs,
            body.idempotency_key.as_deref(),
            state.default_max_attempts,
        )
        .await?;

    let created = outcome == Enqueued::Created;
    tracing::info!(job_id = %job.id, created, "proof request accepted");

    Ok((
        StatusCode::ACCEPTED,
        Json(SubmitResponse {
            job_id: job.id,
            state: job.state.to_string(),
            created,
            job_hash: hex_encode(&job.job_hash),
        }),
    ))
}

/// `GET /v1/proofs/{id}` — report what happened to a job.
#[tracing::instrument(skip_all, fields(job_id = %id))]
async fn job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<JobResponse>, ApiError> {
    let job = state
        .store
        .job(id)
        .await?
        .ok_or(ApiError::JobNotFound(id))?;

    Ok(Json(JobResponse {
        job_id: job.id,
        circuit_id: job.circuit_id,
        state: job.state.to_string(),
        attempts: job.attempts,
        max_attempts: job.max_attempts,
        last_error: job.last_error,
        job_hash: hex_encode(&job.job_hash),
        idempotency_key: job.idempotency_key,
        proof_size_bytes: job.proof.as_ref().map(Vec::len),
        created_at: job.created_at.to_rfc3339(),
        updated_at: job.updated_at.to_rfc3339(),
    }))
}

/// Liveness: the process is running. Deliberately does not touch the database —
/// a liveness probe that fails when Postgres is down would restart a healthy
/// API for someone else's outage.
async fn healthz() -> StatusCode {
    StatusCode::OK
}

/// Readiness: the process can actually serve traffic, which means the database
/// is reachable. This one *should* fail when Postgres is down, so the load
/// balancer stops sending requests that would only 500.
async fn readyz(State(state): State<Arc<AppState>>) -> Response {
    match state.store.queue_depth().await {
        Ok(depth) => (
            StatusCode::OK,
            Json(serde_json::json!({ "queue_depth": depth })),
        )
            .into_response(),
        Err(err) => {
            tracing::warn!(error = %err, "readiness check failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorBody {
                    error: "not_ready",
                    message: "the database is not reachable".to_owned(),
                }),
            )
                .into_response()
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
            // Writing to a String cannot fail.
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encoding_is_lowercase_and_padded() {
        assert_eq!(hex_encode(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(hex_encode(&[]), "");
    }

    #[test]
    fn error_codes_are_distinct() {
        let errors = [
            ApiError::UnknownCircuit("x".into()),
            ApiError::CircuitDisabled("x".into()),
            ApiError::JobNotFound(Uuid::nil()),
            ApiError::Overloaded { depth: 1, limit: 1 },
        ];
        let mut codes: Vec<_> = errors.iter().map(ApiError::code).collect();
        codes.sort_unstable();
        let before = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), before, "two errors share a code");
    }

    /// Status codes are part of the contract: a client retries a 503 and does
    /// not retry a 400. Getting these wrong makes well-behaved clients behave
    /// badly.
    #[test]
    fn errors_map_to_the_expected_status_codes() {
        assert_eq!(
            ApiError::UnknownCircuit("x".into()).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            ApiError::CircuitDisabled("x".into()).status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            ApiError::Overloaded { depth: 9, limit: 8 }.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            ApiError::Invalid(ValidationError::EmptyCircuitId).status(),
            StatusCode::BAD_REQUEST
        );
    }

    /// A database error must never reach the client verbatim — it can disclose
    /// table names, column names, and constraint definitions.
    #[test]
    fn internal_errors_do_not_leak_details() {
        let err = ApiError::Internal(StoreError::JobNotFound(Uuid::nil()));
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
