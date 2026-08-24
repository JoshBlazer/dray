-- Submission attempts are counted separately from proving attempts.
--
-- Both are "attempts at this job", but they are attempts at different work with
-- different failure modes, and sharing one counter conflates them. A job that
-- needed two tries to prove would arrive at the relayer with two thirds of its
-- budget already spent, and could exhaust itself on a single RPC blip — having
-- produced a perfectly valid proof that then never reaches the chain.
--
-- They are also recoverable from each other in neither direction: re-proving
-- costs seconds of CPU, resubmitting costs a nonce and real gas. The state
-- machine already refuses to discard a proof on a retryable submission failure
-- (`Proved | Submitting + RetryScheduled -> Proved`); this is the same
-- distinction in the accounting.

ALTER TABLE jobs ADD COLUMN submission_attempts INTEGER NOT NULL DEFAULT 0;

ALTER TABLE jobs ADD CONSTRAINT jobs_submission_attempts_non_negative
    CHECK (submission_attempts >= 0);

COMMENT ON COLUMN jobs.submission_attempts IS
    'Times a relayer has taken this job for submission. Counted at lease time, '
    'like jobs.attempts, because a relayer killed mid-submission never reports '
    'anything. Budgeted against max_attempts, independently of proving.';

-- The relayer's work queue: proofs waiting to go on chain, oldest first.
--
-- Partial and ordered exactly like `jobs_queue`, and for the same reason —
-- `proved` is a small fraction of the table once the system has been running,
-- and the relayer only ever scans that fraction.
--
-- `jobs_awaiting_submission` already existed for this, but did not account for
-- backoff. A relayer waiting out an RPC outage would have had every one of its
-- backed-off jobs scanned and discarded on every lease.
DROP INDEX jobs_awaiting_submission;
CREATE INDEX jobs_awaiting_submission ON jobs (retry_after NULLS FIRST, created_at)
    WHERE state = 'proved';

-- `retry_after` now applies to two queues, not one.
--
-- The original constraint tied it to `queued` because that was the only queue
-- there was. A relayer backing off after a transient RPC failure needs the same
-- durable delay for exactly the same reason: a delay held in the process that
-- failed is no delay at all, because the next relayer to ask takes the job
-- immediately.
ALTER TABLE jobs DROP CONSTRAINT jobs_retry_after_only_when_queued;
ALTER TABLE jobs ADD CONSTRAINT jobs_retry_after_only_when_waiting CHECK (
    retry_after IS NULL OR state IN ('queued', 'proved')
);
