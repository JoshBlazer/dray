//! PostgreSQL adapters.
//!
//! Postgres is the durable source of truth for job state; every transition
//! happens inside a transaction. Redis holds leases and other ephemeral
//! coordination state and may be rebuilt from Postgres at any time — it is
//! never truth. See `DRAY_BUILD_SPEC.md` §4.2.
//!
//! # Why runtime-checked queries
//!
//! This crate uses `sqlx::query` rather than the `query!` macros. The macros
//! verify SQL against a live database *at compile time*, which is genuinely
//! valuable — but it makes `cargo build` require either a running Postgres or a
//! committed offline cache. Neither is guaranteed on a fresh clone, and a build
//! that fails without a database contradicts the project's own promise that a
//! stranger can clone and build it.
//!
//! The cost is that a malformed query is caught by the integration tests rather
//! than by the compiler, which is why those tests exercise every statement in
//! this module rather than only the happy path. Revisiting this once the schema
//! settles — by committing a `.sqlx` cache — is recorded as a follow-up.

use std::str::FromStr;

use dray_core::{JobEvent, JobState, transition};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgPoolOptions};
use uuid::Uuid;

/// Migrations, embedded in the binary so a service applies its own schema on
/// start in dev without needing the repository on disk.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// Name of this component, as it appears in logs, metrics, and traces.
pub const COMPONENT: &str = "dray-store";

/// The columns [`Job`] is built from.
///
/// Spelled out rather than `SELECT *` for two reasons. `state` is a Postgres
/// enum and has to be cast to text explicitly — sqlx will not decode a
/// `job_state` into a `String`, and `SELECT *` gives no opportunity to say so.
/// And an explicit list means adding a column to the table cannot silently
/// change what this code reads.
const JOB_COLUMNS: &str = "id, circuit_id, job_hash, idempotency_key, inputs, \
     state::text AS state, attempts, max_attempts, last_error, leased_by, proof, \
     retry_after, created_at, updated_at";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("migration failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error("job {0} not found")]
    JobNotFound(Uuid),

    #[error("circuit {0:?} is not registered")]
    CircuitNotFound(String),

    #[error("circuit {0:?} is registered but disabled")]
    CircuitDisabled(String),

    #[error(transparent)]
    IllegalTransition(#[from] dray_core::IllegalTransition),

    #[error("could not canonicalise inputs: {0}")]
    Canonical(#[from] dray_core::CanonicalError),

    #[error("database returned an unrecognised value: {0}")]
    Corrupt(String),
}

/// A registered circuit.
#[derive(Debug, Clone)]
pub struct Circuit {
    pub id: String,
    pub display_name: String,
    pub input_schema: serde_json::Value,
    pub verifier_address: Option<String>,
    pub enabled: bool,
}

/// A proof request and everything known about its progress.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: Uuid,
    pub circuit_id: String,
    pub job_hash: Vec<u8>,
    pub idempotency_key: Option<String>,
    pub inputs: serde_json::Value,
    pub state: JobState,
    pub attempts: i32,
    pub max_attempts: i32,
    pub last_error: Option<String>,
    pub leased_by: Option<String>,
    pub proof: Option<Vec<u8>>,
    /// Earliest time this job may be leased again; `None` means immediately.
    /// Set by [`Store::record_failure`] when a retry is scheduled.
    pub retry_after: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl Job {
    fn from_row(row: &sqlx::postgres::PgRow) -> Result<Self, StoreError> {
        let state: String = row.try_get("state")?;
        Ok(Job {
            id: row.try_get("id")?,
            circuit_id: row.try_get("circuit_id")?,
            job_hash: row.try_get("job_hash")?,
            idempotency_key: row.try_get("idempotency_key")?,
            inputs: row.try_get("inputs")?,
            state: JobState::from_str(&state)
                .map_err(|e| StoreError::Corrupt(format!("job.state: {e}")))?,
            attempts: row.try_get("attempts")?,
            max_attempts: row.try_get("max_attempts")?,
            last_error: row.try_get("last_error")?,
            leased_by: row.try_get("leased_by")?,
            proof: row.try_get("proof")?,
            retry_after: row.try_get("retry_after")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

/// The outcome of submitting a proof request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enqueued {
    /// This request created a new job.
    Created,
    /// An identical request already existed; this is that job.
    ///
    /// Not an error. A client retrying after a timeout should get the same job
    /// back, not a duplicate and not a failure.
    Duplicate,
}

/// Handle to the durable store.
#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// Connect, with a bounded pool.
    ///
    /// # Errors
    ///
    /// Fails if the database is unreachable or rejects the credentials.
    pub async fn connect(database_url: &str, max_connections: u32) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(database_url)
            .await?;
        Ok(Self { pool })
    }

    /// Wrap an existing pool. Used by tests that manage their own connections.
    #[must_use]
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Apply any outstanding migrations.
    ///
    /// # Errors
    ///
    /// Fails if a migration errors or if the recorded history diverges from the
    /// migrations on disk.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Circuits
    // -----------------------------------------------------------------------

    /// Register a circuit, or update it if it already exists.
    ///
    /// # Errors
    ///
    /// Propagates database failures.
    pub async fn upsert_circuit(&self, circuit: &Circuit) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO circuits (id, display_name, input_schema, verifier_address, enabled)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (id) DO UPDATE SET
                 display_name     = EXCLUDED.display_name,
                 input_schema     = EXCLUDED.input_schema,
                 verifier_address = EXCLUDED.verifier_address,
                 enabled          = EXCLUDED.enabled",
        )
        .bind(&circuit.id)
        .bind(&circuit.display_name)
        .bind(&circuit.input_schema)
        .bind(&circuit.verifier_address)
        .bind(circuit.enabled)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Look up a circuit by identifier.
    ///
    /// # Errors
    ///
    /// Propagates database failures.
    pub async fn circuit(&self, id: &str) -> Result<Option<Circuit>, StoreError> {
        let row = sqlx::query(
            "SELECT id, display_name, input_schema, verifier_address, enabled
             FROM circuits WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        row.map(|row| {
            Ok(Circuit {
                id: row.try_get("id")?,
                display_name: row.try_get("display_name")?,
                input_schema: row.try_get("input_schema")?,
                verifier_address: row.try_get("verifier_address")?,
                enabled: row.try_get("enabled")?,
            })
        })
        .transpose()
    }

    // -----------------------------------------------------------------------
    // Jobs
    // -----------------------------------------------------------------------

    /// Accept a proof request, deduplicating by canonical content hash.
    ///
    /// Idempotency is enforced by the database, not by this function. The
    /// `INSERT ... ON CONFLICT DO NOTHING` followed by a `SELECT` is atomic in
    /// a way that "check whether it exists, then insert" is not: fifty
    /// concurrent identical requests all reach the insert, exactly one wins,
    /// and the other forty-nine read back the winner's row.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::CircuitNotFound`] or [`StoreError::CircuitDisabled`]
    /// if the target circuit cannot accept work, and propagates canonicalisation
    /// and database failures.
    pub async fn enqueue(
        &self,
        circuit_id: &str,
        inputs: &serde_json::Value,
        idempotency_key: Option<&str>,
        max_attempts: i32,
    ) -> Result<(Job, Enqueued), StoreError> {
        let circuit = self
            .circuit(circuit_id)
            .await?
            .ok_or_else(|| StoreError::CircuitNotFound(circuit_id.to_owned()))?;
        if !circuit.enabled {
            return Err(StoreError::CircuitDisabled(circuit_id.to_owned()));
        }

        let hash = dray_core::job_hash(circuit_id, inputs)?;

        let insert = format!(
            "INSERT INTO jobs (circuit_id, job_hash, idempotency_key, inputs, max_attempts)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (job_hash) DO NOTHING
             RETURNING {JOB_COLUMNS}"
        );
        let inserted = sqlx::query(&insert)
            .bind(circuit_id)
            .bind(hash.as_slice())
            .bind(idempotency_key)
            .bind(inputs)
            .bind(max_attempts)
            .fetch_optional(&self.pool)
            .await?;

        if let Some(row) = inserted {
            return Ok((Job::from_row(&row)?, Enqueued::Created));
        }

        // Lost the race, or a genuine repeat submission. Either way the correct
        // answer is the job that already exists.
        let select = format!("SELECT {JOB_COLUMNS} FROM jobs WHERE job_hash = $1");
        let existing = sqlx::query(&select)
            .bind(hash.as_slice())
            .fetch_one(&self.pool)
            .await?;

        Ok((Job::from_row(&existing)?, Enqueued::Duplicate))
    }

    /// Fetch a job by id.
    ///
    /// # Errors
    ///
    /// Propagates database failures.
    pub async fn job(&self, id: Uuid) -> Result<Option<Job>, StoreError> {
        let sql = format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = $1");
        let row = sqlx::query(&sql)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(Job::from_row).transpose()
    }

    /// Fetch a job by its canonical content hash.
    ///
    /// # Errors
    ///
    /// Propagates database failures.
    pub async fn job_by_hash(&self, hash: &[u8]) -> Result<Option<Job>, StoreError> {
        let sql = format!("SELECT {JOB_COLUMNS} FROM jobs WHERE job_hash = $1");
        let row = sqlx::query(&sql)
            .bind(hash)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(Job::from_row).transpose()
    }

    /// Apply an event to a job, moving it through the state machine.
    ///
    /// The row is locked with `SELECT ... FOR UPDATE` for the duration, so two
    /// actors racing to transition the same job serialise rather than
    /// interleave. The loser sees the winner's state and its event is rejected
    /// as illegal — which is the correct outcome, and far better than both
    /// succeeding.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::JobNotFound`] for an unknown id and
    /// [`StoreError::IllegalTransition`] if the event has no meaning in the
    /// job's current state.
    pub async fn apply_event(
        &self,
        id: Uuid,
        event: JobEvent,
        actor: Option<&str>,
        detail: Option<&str>,
    ) -> Result<Job, StoreError> {
        let mut tx = self.pool.begin().await?;
        let job = Self::apply_event_in_tx(&mut tx, id, event, actor, detail).await?;
        tx.commit().await?;
        Ok(job)
    }

    async fn apply_event_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
        event: JobEvent,
        actor: Option<&str>,
        detail: Option<&str>,
    ) -> Result<Job, StoreError> {
        let lock = format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = $1 FOR UPDATE");
        let current = sqlx::query(&lock)
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(StoreError::JobNotFound(id))?;

        let job = Job::from_row(&current)?;
        let next = transition(job.state, event)?;

        // Clearing the lease on any transition out of an in-flight state is
        // what stops a dead worker's name lingering on a job that has already
        // gone back to the queue.
        let keeps_lease = next.is_in_flight();

        let update = format!(
            "UPDATE jobs SET
                 state            = $2::job_state,
                 leased_by        = CASE WHEN $3::boolean THEN leased_by ELSE NULL END,
                 lease_expires_at = CASE WHEN $3::boolean THEN lease_expires_at ELSE NULL END,
                 last_error       = COALESCE($4::text, last_error)
             WHERE id = $1
             RETURNING {JOB_COLUMNS}"
        );
        let updated = sqlx::query(&update)
            .bind(id)
            .bind(next.as_str())
            .bind(keeps_lease)
            .bind(detail)
            .fetch_one(&mut **tx)
            .await?;

        sqlx::query(
            "INSERT INTO job_transitions (job_id, from_state, event, to_state, actor, detail)
             VALUES ($1, $2::job_state, $3::job_event, $4::job_state, $5, $6)",
        )
        .bind(id)
        .bind(job.state.as_str())
        .bind(event.as_str())
        .bind(next.as_str())
        .bind(actor)
        .bind(detail)
        .execute(&mut **tx)
        .await?;

        Job::from_row(&updated)
    }

    // -----------------------------------------------------------------------
    // Leasing
    // -----------------------------------------------------------------------

    /// Take the oldest queued job, if there is one, and lease it.
    ///
    /// `FOR UPDATE SKIP LOCKED` is what makes this safe with many workers
    /// polling at once. Each worker locks a different row and skips any row a
    /// peer already holds, so N workers pulling concurrently get N distinct
    /// jobs rather than contending for the same one. Without `SKIP LOCKED` they
    /// would serialise behind the head of the queue and the pool would not
    /// scale; without `FOR UPDATE` two workers could read the same row and both
    /// believe they own it.
    ///
    /// The attempt counter increments here, at lease time, not on success.
    /// A worker that is killed mid-proof never gets to report anything, so an
    /// attempt counted only on completion would never be counted at all and a
    /// poison job would be retried forever.
    ///
    /// # Errors
    ///
    /// Propagates database failures.
    pub async fn lease_next(
        &self,
        worker_id: &str,
        lease_ttl: std::time::Duration,
    ) -> Result<Option<Job>, StoreError> {
        let mut tx = self.pool.begin().await?;

        // `retry_after` is compared against the database clock, not the
        // worker's. Workers disagree about the time; the queue must not.
        let candidate = sqlx::query(
            "SELECT id FROM jobs
             WHERE state = 'queued'
               AND (retry_after IS NULL OR retry_after <= now())
             ORDER BY created_at
             FOR UPDATE SKIP LOCKED
             LIMIT 1",
        )
        .fetch_optional(&mut *tx)
        .await?;

        let Some(row) = candidate else {
            tx.rollback().await?;
            return Ok(None);
        };
        let id: Uuid = row.try_get("id")?;

        // The state machine decides; this function only persists what it says.
        let next = transition(JobState::Queued, JobEvent::Leased)?;

        let update = format!(
            "UPDATE jobs SET
                 state            = $2::job_state,
                 leased_by        = $3,
                 lease_expires_at = now() + make_interval(secs => $4),
                 attempts         = attempts + 1,
                 retry_after      = NULL
             WHERE id = $1
             RETURNING {JOB_COLUMNS}"
        );
        let leased = sqlx::query(&update)
            .bind(id)
            .bind(next.as_str())
            .bind(worker_id)
            .bind(lease_ttl.as_secs_f64())
            .fetch_one(&mut *tx)
            .await?;

        let job = Job::from_row(&leased)?;

        sqlx::query(
            "INSERT INTO job_attempts (job_id, attempt_number, worker_id)
             VALUES ($1, $2, $3)
             ON CONFLICT (job_id, attempt_number) DO NOTHING",
        )
        .bind(id)
        .bind(job.attempts)
        .bind(worker_id)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO job_transitions (job_id, from_state, event, to_state, actor)
             VALUES ($1, 'queued'::job_state, 'leased'::job_event, $2::job_state, $3)",
        )
        .bind(id)
        .bind(next.as_str())
        .bind(worker_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(Some(job))
    }

    /// Extend a lease the caller already holds.
    ///
    /// The `leased_by` check is the point: a worker whose lease has already
    /// expired and been reaped must not be able to reclaim the job by
    /// heartbeating, because another worker may now own it. Returns `false`
    /// when the lease was lost, which the caller should treat as an instruction
    /// to abandon the work in progress.
    ///
    /// # Errors
    ///
    /// Propagates database failures.
    pub async fn renew_lease(
        &self,
        id: Uuid,
        worker_id: &str,
        lease_ttl: std::time::Duration,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE jobs
             SET lease_expires_at = now() + make_interval(secs => $3)
             WHERE id = $1
               AND leased_by = $2
               AND state IN ('leased', 'proving')",
        )
        .bind(id)
        .bind(worker_id)
        .bind(lease_ttl.as_secs_f64())
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() == 1)
    }

    /// Return jobs whose leases have expired to the queue.
    ///
    /// This is what turns a dead worker into an ordinary, recoverable event.
    /// Any worker may run it; it is idempotent and safe to run concurrently
    /// because each row is locked with `SKIP LOCKED` before being touched.
    ///
    /// Returns the ids reaped, for logging and metrics.
    ///
    /// # Errors
    ///
    /// Propagates database failures.
    pub async fn reap_expired_leases(&self, reaper: &str) -> Result<Vec<Uuid>, StoreError> {
        let mut tx = self.pool.begin().await?;

        let expired = sqlx::query(
            "SELECT id, state::text AS state FROM jobs
             WHERE state IN ('leased', 'proving')
               AND lease_expires_at < now()
             FOR UPDATE SKIP LOCKED",
        )
        .fetch_all(&mut *tx)
        .await?;

        let mut reaped = Vec::with_capacity(expired.len());
        for row in &expired {
            let id: Uuid = row.try_get("id")?;
            let state_name: String = row.try_get("state")?;
            let from = JobState::from_str(&state_name)
                .map_err(|e| StoreError::Corrupt(format!("job.state: {e}")))?;
            let next = transition(from, JobEvent::LeaseExpired)?;

            sqlx::query(
                "UPDATE jobs SET
                     state            = $2::job_state,
                     leased_by        = NULL,
                     lease_expires_at = NULL
                 WHERE id = $1",
            )
            .bind(id)
            .bind(next.as_str())
            .execute(&mut *tx)
            .await?;

            sqlx::query(
                "UPDATE job_attempts
                 SET outcome = 'abandoned', finished_at = now()
                 WHERE job_id = $1 AND outcome IS NULL",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;

            sqlx::query(
                "INSERT INTO job_transitions (job_id, from_state, event, to_state, actor, detail)
                 VALUES ($1, $2::job_state, 'lease_expired'::job_event, $3::job_state, $4, $5)",
            )
            .bind(id)
            .bind(from.as_str())
            .bind(next.as_str())
            .bind(reaper)
            .bind("lease expired without renewal")
            .execute(&mut *tx)
            .await?;

            reaped.push(id);
        }

        tx.commit().await?;
        Ok(reaped)
    }

    /// Mark a leased job as proving.
    ///
    /// # Errors
    ///
    /// Propagates database failures and illegal transitions.
    pub async fn begin_proving(&self, id: Uuid, worker_id: &str) -> Result<Job, StoreError> {
        self.apply_event(id, JobEvent::ProvingStarted, Some(worker_id), None)
            .await
    }

    /// Record a successful proof and move the job to `proved`.
    ///
    /// Proof and public inputs are written in the same transaction as the state
    /// change, because the schema refuses a `proved` job with no proof — the
    /// two cannot be allowed to disagree even briefly.
    ///
    /// # Errors
    ///
    /// Propagates database failures and illegal transitions.
    pub async fn record_proof(
        &self,
        id: Uuid,
        worker_id: &str,
        proof: &[u8],
        public_inputs: &[u8],
        duration_ms: i64,
        peak_memory_kb: Option<i64>,
    ) -> Result<Job, StoreError> {
        let mut tx = self.pool.begin().await?;

        let current =
            sqlx::query("SELECT state::text AS state, attempts FROM jobs WHERE id = $1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(StoreError::JobNotFound(id))?;

        let state_name: String = current.try_get("state")?;
        let from = JobState::from_str(&state_name)
            .map_err(|e| StoreError::Corrupt(format!("job.state: {e}")))?;
        let attempts: i32 = current.try_get("attempts")?;
        let next = transition(from, JobEvent::ProofSucceeded)?;

        let update = format!(
            "UPDATE jobs SET
                 state            = $2::job_state,
                 proof            = $3,
                 public_inputs    = $4,
                 leased_by        = NULL,
                 lease_expires_at = NULL
             WHERE id = $1
             RETURNING {JOB_COLUMNS}"
        );
        // Bind order must match the placeholders above: $1 id, $2 state,
        // $3 proof, $4 public inputs.
        let updated = sqlx::query(&update)
            .bind(id)
            .bind(next.as_str())
            .bind(proof)
            .bind(public_inputs)
            .fetch_one(&mut *tx)
            .await?;

        sqlx::query(
            "UPDATE job_attempts SET
                 outcome        = 'succeeded',
                 finished_at    = now(),
                 duration_ms    = $3,
                 peak_memory_kb = $4
             WHERE job_id = $1 AND attempt_number = $2",
        )
        .bind(id)
        .bind(attempts)
        .bind(duration_ms)
        .bind(peak_memory_kb)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO job_transitions (job_id, from_state, event, to_state, actor)
             VALUES ($1, $2::job_state, 'proof_succeeded'::job_event, $3::job_state, $4)",
        )
        .bind(id)
        .bind(from.as_str())
        .bind(next.as_str())
        .bind(worker_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Job::from_row(&updated)
    }

    /// Record a failed attempt, letting the retry policy decide what happens.
    ///
    /// # Errors
    ///
    /// Propagates database failures and illegal transitions.
    pub async fn record_failure(
        &self,
        id: Uuid,
        worker_id: &str,
        kind: dray_core::FailureKind,
        error: &str,
        retry_in: Option<std::time::Duration>,
    ) -> Result<Job, StoreError> {
        let mut tx = self.pool.begin().await?;

        let current = sqlx::query(
            "SELECT state::text AS state, attempts, max_attempts FROM jobs WHERE id = $1 FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::JobNotFound(id))?;

        let state_name: String = current.try_get("state")?;
        let from = JobState::from_str(&state_name)
            .map_err(|e| StoreError::Corrupt(format!("job.state: {e}")))?;
        let attempts: i32 = current.try_get("attempts")?;
        let max_attempts: i32 = current.try_get("max_attempts")?;

        let event = dray_core::classify_failure(
            kind,
            attempts.max(0).unsigned_abs(),
            max_attempts.max(1).unsigned_abs(),
        );
        let next = transition(from, event)?;

        // The delay is applied only when the job is actually going back on the
        // queue. A `failed` or `rejected` job carries no retry, and the schema
        // refuses one — a scheduled retry on a terminal job would be invisible
        // and permanent, since nothing would ever lease it to clear the field.
        let retry_seconds = if next == JobState::Queued {
            retry_in.map(|d| d.as_secs_f64())
        } else {
            None
        };

        let update = format!(
            "UPDATE jobs SET
                 state            = $2::job_state,
                 last_error       = $3,
                 leased_by        = NULL,
                 lease_expires_at = NULL,
                 retry_after      = CASE
                     WHEN $4::double precision IS NULL THEN NULL
                     ELSE now() + make_interval(secs => $4::double precision)
                 END
             WHERE id = $1
             RETURNING {JOB_COLUMNS}"
        );
        let updated = sqlx::query(&update)
            .bind(id)
            .bind(next.as_str())
            .bind(error)
            .bind(retry_seconds)
            .fetch_one(&mut *tx)
            .await?;

        sqlx::query(
            "UPDATE job_attempts SET outcome = 'failed', finished_at = now(), error = $3
             WHERE job_id = $1 AND attempt_number = $2",
        )
        .bind(id)
        .bind(attempts)
        .bind(error)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO job_transitions (job_id, from_state, event, to_state, actor, detail)
             VALUES ($1, $2::job_state, $3::job_event, $4::job_state, $5, $6)",
        )
        .bind(id)
        .bind(from.as_str())
        .bind(event.as_str())
        .bind(next.as_str())
        .bind(worker_id)
        .bind(error)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Job::from_row(&updated)
    }

    /// Release a lease without failing the job, returning it to the queue.
    ///
    /// Used by graceful shutdown: a worker told to stop should hand its job
    /// back immediately rather than make the next worker wait out the lease TTL.
    ///
    /// # Errors
    ///
    /// Propagates database failures and illegal transitions.
    pub async fn release_lease(&self, id: Uuid, worker_id: &str) -> Result<Job, StoreError> {
        self.apply_event(
            id,
            JobEvent::LeaseExpired,
            Some(worker_id),
            Some("released on worker shutdown"),
        )
        .await
    }

    /// How many jobs are waiting to be leased. Feeds backpressure and the
    /// queue-depth metric.
    ///
    /// # Errors
    ///
    /// Propagates database failures.
    pub async fn queue_depth(&self) -> Result<i64, StoreError> {
        let row = sqlx::query("SELECT count(*) AS depth FROM jobs WHERE state = 'queued'")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get("depth")?)
    }

    /// Count of jobs in each state, for the operator CLI and dashboards.
    ///
    /// # Errors
    ///
    /// Propagates database failures.
    pub async fn state_counts(&self) -> Result<Vec<(JobState, i64)>, StoreError> {
        let rows = sqlx::query("SELECT state::text AS state, count(*) AS n FROM jobs GROUP BY 1")
            .fetch_all(&self.pool)
            .await?;

        rows.iter()
            .map(|row| {
                let name: String = row.try_get("state")?;
                let state = JobState::from_str(&name)
                    .map_err(|e| StoreError::Corrupt(format!("job.state: {e}")))?;
                Ok((state, row.try_get("n")?))
            })
            .collect()
    }

    /// The recorded transition history for a job, oldest first.
    ///
    /// # Errors
    ///
    /// Propagates database failures.
    pub async fn transitions(
        &self,
        id: Uuid,
    ) -> Result<Vec<(JobState, JobEvent, JobState)>, StoreError> {
        let rows = sqlx::query(
            "SELECT from_state::text AS f, event::text AS e, to_state::text AS t
             FROM job_transitions WHERE job_id = $1 ORDER BY occurred_at, id",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;

        rows.iter()
            .map(|row| {
                let f: String = row.try_get("f")?;
                let e: String = row.try_get("e")?;
                let t: String = row.try_get("t")?;
                let parse_state = |s: &str| {
                    JobState::from_str(s).map_err(|err| StoreError::Corrupt(err.to_string()))
                };
                let event = JobEvent::ALL
                    .into_iter()
                    .find(|candidate| candidate.as_str() == e)
                    .ok_or_else(|| StoreError::Corrupt(format!("job_event: {e}")))?;
                Ok((parse_state(&f)?, event, parse_state(&t)?))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_is_named() {
        assert_eq!(COMPONENT, "dray-store");
    }

    #[test]
    fn links_against_core() {
        assert_eq!(dray_core::COMPONENT, "dray-core");
    }

    /// The embedded migrations must be loadable without a database. Catches a
    /// missing or malformed migration file at unit-test time rather than at
    /// service start.
    #[test]
    fn migrations_are_embedded() {
        assert!(
            !MIGRATOR.migrations.is_empty(),
            "no migrations were embedded; check the migrate! path"
        );
    }
}
