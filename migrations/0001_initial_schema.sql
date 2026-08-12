-- Dray initial schema.
--
-- Postgres is the source of truth for job state. Every transition happens
-- inside a transaction here; Redis holds leases for fast liveness checks and
-- can be rebuilt from this schema at any time.
--
-- Two design points are load-bearing:
--
--   1. `job_state` is a real Postgres enum, not a string. An invalid state is
--      rejected by the database, not merely by the application.
--   2. `jobs.job_hash` carries a UNIQUE constraint. Deduplication is enforced
--      by the database under concurrency, not by a read-then-write in the
--      application, which would race.

-- ---------------------------------------------------------------------------
-- Enums
-- ---------------------------------------------------------------------------

-- Mirrors dray_core::job::JobState. The two must agree; `JobState::as_str` is
-- the contract between them.
CREATE TYPE job_state AS ENUM (
    'queued',
    'leased',
    'proving',
    'proved',
    'submitting',
    'settled',
    'failed',
    'rejected'
);

-- Mirrors dray_core::job::JobEvent.
CREATE TYPE job_event AS ENUM (
    'leased',
    'proving_started',
    'proof_succeeded',
    'submission_started',
    'settlement_confirmed',
    'lease_expired',
    'retry_scheduled',
    'attempts_exhausted',
    'permanently_failed',
    'validation_rejected',
    'reorged'
);

CREATE TYPE attempt_outcome AS ENUM ('succeeded', 'failed', 'abandoned');

-- ---------------------------------------------------------------------------
-- updated_at maintenance
-- ---------------------------------------------------------------------------

CREATE FUNCTION set_updated_at() RETURNS trigger AS $$
BEGIN
    NEW.updated_at := now();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- ---------------------------------------------------------------------------
-- circuits
-- ---------------------------------------------------------------------------

-- Registering a circuit is data, not code. The API validates submitted inputs
-- against `input_schema` rather than against anything hardcoded, which is what
-- keeps the system circuit-agnostic at its boundary as well as on chain.
CREATE TABLE circuits (
    id                TEXT PRIMARY KEY,
    display_name      TEXT        NOT NULL,

    -- JSON Schema describing acceptable inputs for this circuit.
    input_schema      JSONB       NOT NULL,

    -- Deployed verifier, and the identifier DraySettlement dispatches on.
    -- Null until the circuit has been deployed to the target chain.
    verifier_address  TEXT,
    settlement_id     BYTEA,

    -- Circuits are disabled rather than deleted: jobs reference them, and
    -- history should stay readable after a circuit is retired.
    enabled           BOOLEAN     NOT NULL DEFAULT TRUE,

    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT circuits_id_not_blank CHECK (length(trim(id)) > 0),
    CONSTRAINT circuits_settlement_id_is_32_bytes
        CHECK (settlement_id IS NULL OR length(settlement_id) = 32)
);

CREATE TRIGGER circuits_set_updated_at
    BEFORE UPDATE ON circuits
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- ---------------------------------------------------------------------------
-- jobs
-- ---------------------------------------------------------------------------

CREATE TABLE jobs (
    id                UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    circuit_id        TEXT        NOT NULL REFERENCES circuits (id),

    -- SHA-256(circuit_id || 0x00 || canonical_inputs). The canonical identity
    -- of the work. UNIQUE is what makes submitting twice produce one job even
    -- when both requests arrive at the same instant on different connections.
    job_hash          BYTEA       NOT NULL UNIQUE,

    -- Client-supplied, recorded for correlation and support. Deliberately NOT
    -- the deduplication key: two clients could pick the same key for different
    -- work, and one client could submit identical work under two keys. The
    -- content hash is the identity that actually means something.
    idempotency_key   TEXT,

    inputs            JSONB       NOT NULL,

    state             job_state   NOT NULL DEFAULT 'queued',

    attempts          INTEGER     NOT NULL DEFAULT 0,
    max_attempts      INTEGER     NOT NULL DEFAULT 3,
    last_error        TEXT,

    -- Lease bookkeeping. Postgres is authoritative; Redis mirrors this.
    leased_by         TEXT,
    lease_expires_at  TIMESTAMPTZ,

    -- Results, populated once proving succeeds.
    proof             BYTEA,
    public_inputs     BYTEA,

    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT jobs_job_hash_is_32_bytes CHECK (length(job_hash) = 32),
    CONSTRAINT jobs_attempts_non_negative CHECK (attempts >= 0),
    CONSTRAINT jobs_max_attempts_positive CHECK (max_attempts > 0),

    -- A lease is either fully present or fully absent. A half-set lease means
    -- either an unowned job that looks held, or a held job that expires never.
    CONSTRAINT jobs_lease_is_all_or_nothing CHECK (
        (leased_by IS NULL AND lease_expires_at IS NULL)
        OR (leased_by IS NOT NULL AND lease_expires_at IS NOT NULL)
    ),

    -- A job past proving must have a proof. Enforced here so that a bug in the
    -- worker cannot leave a `proved` job with nothing to submit.
    CONSTRAINT jobs_proved_states_have_a_proof CHECK (
        state NOT IN ('proved', 'submitting', 'settled')
        OR (proof IS NOT NULL AND public_inputs IS NOT NULL)
    )
);

-- The queue scan: oldest queued job first. Partial, because the queue is the
-- only thing this index needs to serve and queued jobs are a small fraction of
-- the table once the system has been running a while.
CREATE INDEX jobs_queue ON jobs (created_at) WHERE state = 'queued';

-- Finds leases to reap. Also partial: only in-flight jobs have one.
CREATE INDEX jobs_expiring_leases ON jobs (lease_expires_at)
    WHERE state IN ('leased', 'proving');

-- Relayer's work queue.
CREATE INDEX jobs_awaiting_submission ON jobs (created_at) WHERE state = 'proved';

CREATE INDEX jobs_by_circuit ON jobs (circuit_id, created_at DESC);
CREATE INDEX jobs_by_idempotency_key ON jobs (idempotency_key)
    WHERE idempotency_key IS NOT NULL;

CREATE TRIGGER jobs_set_updated_at
    BEFORE UPDATE ON jobs
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- ---------------------------------------------------------------------------
-- job_attempts
-- ---------------------------------------------------------------------------

-- One row per attempt, retained even after success. Without this, a job that
-- succeeded on its fourth try is indistinguishable from one that succeeded
-- immediately, and the attempt distribution metric has nothing behind it.
CREATE TABLE job_attempts (
    id             BIGINT          GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    job_id         UUID            NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    attempt_number INTEGER         NOT NULL,
    worker_id      TEXT            NOT NULL,

    outcome        attempt_outcome,
    error          TEXT,

    -- Resource usage, for capacity planning and for spotting a circuit whose
    -- cost has drifted.
    duration_ms    BIGINT,
    peak_memory_kb BIGINT,

    started_at     TIMESTAMPTZ     NOT NULL DEFAULT now(),
    finished_at    TIMESTAMPTZ,

    CONSTRAINT job_attempts_unique_per_job UNIQUE (job_id, attempt_number),
    CONSTRAINT job_attempts_number_positive CHECK (attempt_number > 0),
    CONSTRAINT job_attempts_finished_after_started
        CHECK (finished_at IS NULL OR finished_at >= started_at)
);

CREATE INDEX job_attempts_by_job ON job_attempts (job_id, attempt_number);

-- ---------------------------------------------------------------------------
-- job_transitions
-- ---------------------------------------------------------------------------

-- An append-only audit of every state change. This is what makes "where did
-- this job actually go" answerable after the fact, which matters most during
-- the incident where the answer is not obvious.
CREATE TABLE job_transitions (
    id          BIGINT      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    job_id      UUID        NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    from_state  job_state   NOT NULL,
    event       job_event   NOT NULL,
    to_state    job_state   NOT NULL,
    actor       TEXT,
    detail      TEXT,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX job_transitions_by_job ON job_transitions (job_id, occurred_at);

-- ---------------------------------------------------------------------------
-- settlements
-- ---------------------------------------------------------------------------

CREATE TABLE settlements (
    id             UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    job_id         UUID        NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,

    tx_hash        BYTEA       NOT NULL,
    nullifier      BYTEA       NOT NULL,

    block_number   BIGINT,
    confirmations  INTEGER     NOT NULL DEFAULT 0,

    -- Gas actually paid, for docs/BENCHMARKS.md.
    gas_used       BIGINT,
    effective_gas_price NUMERIC(78, 0),

    -- Set when a reorg removes this settlement. The row is kept rather than
    -- deleted: "this settled and then un-settled" is exactly the history worth
    -- being able to read back.
    reorged_at     TIMESTAMPTZ,

    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT settlements_tx_hash_is_32_bytes CHECK (length(tx_hash) = 32),
    CONSTRAINT settlements_nullifier_is_32_bytes CHECK (length(nullifier) = 32),
    CONSTRAINT settlements_confirmations_non_negative CHECK (confirmations >= 0)
);

-- At most one live settlement per nullifier. The on-chain nullifier set is the
-- real guarantee; this is the local mirror of it, and it catches a double
-- submission before it costs gas rather than after.
CREATE UNIQUE INDEX settlements_one_live_per_nullifier
    ON settlements (nullifier) WHERE reorged_at IS NULL;

CREATE INDEX settlements_by_job ON settlements (job_id);
CREATE INDEX settlements_awaiting_confirmation ON settlements (created_at)
    WHERE reorged_at IS NULL AND block_number IS NULL;

CREATE TRIGGER settlements_set_updated_at
    BEFORE UPDATE ON settlements
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
