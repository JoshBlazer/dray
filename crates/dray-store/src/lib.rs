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
     created_at, updated_at";

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
