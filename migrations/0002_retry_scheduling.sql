-- Durable retry scheduling.
--
-- Backoff has to live in the database, not in the worker that failed. A worker
-- that slept locally before retrying would achieve nothing: it releases the
-- job on failure, and the next worker to call `lease_next` picks it up
-- immediately. The delay is only real if every worker can see it.
--
-- It also has to survive the worker. The common reason to back off is that
-- something shared is unhealthy, and the same event that failed the job often
-- kills the process holding the timer.

ALTER TABLE jobs ADD COLUMN retry_after TIMESTAMPTZ;

COMMENT ON COLUMN jobs.retry_after IS
    'Earliest time this job may be leased again. NULL means immediately. Set '
    'from now() in the database rather than from a worker clock, so a worker '
    'with a skewed clock cannot schedule a retry in the past or the far future.';

-- The queue scan now has to skip jobs that are queued but not yet due, so the
-- index has to cover the ordering *and* the eligibility test. Without
-- `retry_after` in the index, a backlog of backed-off jobs would be scanned
-- and discarded on every lease.
--
-- `retry_after NULLS FIRST` puts never-failed jobs ahead of waiting ones at
-- equal timestamps, which is the order the queue index already implied.
DROP INDEX jobs_queue;
CREATE INDEX jobs_queue ON jobs (retry_after NULLS FIRST, created_at)
    WHERE state = 'queued';

-- A retry that is scheduled but whose job is not queued would be invisible and
-- permanent: nothing clears it, and nothing would ever lease the job to find
-- out. Cheaper to refuse the state than to write a reaper for it.
ALTER TABLE jobs ADD CONSTRAINT jobs_retry_after_only_when_queued CHECK (
    retry_after IS NULL OR state = 'queued'
);
