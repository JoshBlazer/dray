//! The lease loop: take a job, prove it, record what happened.
//!
//! # The three ways an attempt ends
//!
//! A worker that only handled success and failure would be wrong in a way that
//! is hard to see and expensive to debug, because the third case looks like the
//! other two:
//!
//! - **The proof finished.** Record it, or record why it failed.
//! - **The lease was lost.** Another worker owns the job now. This worker must
//!   record *nothing* — writing a result here would overwrite the new owner's
//!   work, or resurrect a job someone else already failed.
//! - **Shutdown arrived.** Finish if the proof is nearly done, otherwise hand
//!   the job back so the next worker need not wait out the lease TTL.
//!
//! # Why the heartbeat can abandon work
//!
//! Long proofs outlive short leases, so a worker renews while it proves. If a
//! renewal is *refused* — meaning the lease expired and was reaped, and someone
//! else may hold the job — the honest response is to stop. Continuing would
//! produce a proof this worker no longer has the right to record, and at-least-
//! once delivery means the new owner is already producing one of its own.
//!
//! A renewal that *errors* is different and is deliberately not treated as
//! loss. A database blip should not throw away a proof in progress; the attempt
//! is bounded by the wall clock regardless, and if the database really is gone
//! the reaper will return the job when it comes back.
//!
//! # Why the heartbeat must die with the attempt
//!
//! `tokio::spawn` detaches: dropping a `JoinHandle` leaves the task running.
//! For a heartbeat that is not a leak, it is a correctness failure. A worker
//! whose future is dropped mid-attempt — cancelled by a `select!`, a timeout,
//! or a supervisor — would leave a task behind renewing the lease for work
//! that has stopped happening. The job would look permanently healthy to every
//! other worker: never expired, so never reaped, so never retried, so lost.
//!
//! [`TaskGuard`] exists for that reason alone. Every spawned task in this
//! module is owned by one, so cancelling a worker cancels everything it
//! started.

use std::time::Duration;

use dray_store::{Job, Store};
use tokio::sync::watch;
use uuid::Uuid;

use crate::{
    backoff::Backoff,
    prover::{self, ProveError, Proven, ProverConfig},
};

/// A spawned task that is aborted when the guard is dropped.
///
/// See the module documentation: a detached heartbeat outliving its attempt
/// silently loses jobs, so nothing here is allowed to detach.
#[derive(Debug)]
struct TaskGuard<T>(tokio::task::JoinHandle<T>);

impl<T: Send + 'static> TaskGuard<T> {
    fn spawn(future: impl std::future::Future<Output = T> + Send + 'static) -> Self {
        Self(tokio::spawn(future))
    }

    fn handle(&mut self) -> &mut tokio::task::JoinHandle<T> {
        &mut self.0
    }
}

impl<T> Drop for TaskGuard<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// How a single attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A proof was produced and recorded.
    Proved(Uuid),
    /// The attempt failed and the result was recorded.
    Failed {
        id: Uuid,
        kind: dray_core::FailureKind,
        /// `Some` if a retry was requested. The store still decides whether the
        /// job actually gets one — the attempt budget may be spent.
        retry_in: Option<Duration>,
    },
    /// The lease was lost mid-attempt. Nothing was recorded, deliberately.
    LeaseLost(Uuid),
    /// Shutdown arrived before the proof finished; the lease was released.
    Abandoned(Uuid),
}

impl Outcome {
    #[must_use]
    pub fn job_id(&self) -> Uuid {
        match self {
            Outcome::Proved(id)
            | Outcome::Failed { id, .. }
            | Outcome::LeaseLost(id)
            | Outcome::Abandoned(id) => *id,
        }
    }
}

/// Everything about a worker that is not the store or the prover.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Identifies this worker in leases, attempts, and the transition log.
    pub worker_id: String,
    /// How long a lease is granted for.
    pub lease_ttl: Duration,
    /// How often to renew during a proof.
    pub heartbeat_interval: Duration,
    /// How long to wait before asking again when the queue is empty.
    pub poll_interval: Duration,
    /// How long a proof in progress may run after shutdown is requested.
    pub shutdown_grace: Duration,
    /// How often to return expired leases to the queue.
    pub reap_interval: Duration,
    pub backoff: Backoff,
}

impl WorkerConfig {
    /// Defaults derived from measured proving cost: roughly 2.5 s per proof,
    /// bounded at 120 s.
    ///
    /// The lease TTL is comfortably longer than the proving wall clock, so a
    /// *healthy* proof can never lose its lease — a lease that expires under a
    /// worker doing its job correctly would convert good work into duplicate
    /// work. The heartbeat runs several times per TTL so a single missed
    /// renewal is not fatal.
    #[must_use]
    pub fn new(worker_id: impl Into<String>) -> Self {
        Self {
            worker_id: worker_id.into(),
            lease_ttl: Duration::from_secs(180),
            heartbeat_interval: Duration::from_secs(30),
            poll_interval: Duration::from_millis(500),
            shutdown_grace: Duration::from_secs(30),
            reap_interval: Duration::from_secs(30),
            backoff: Backoff::default(),
        }
    }
}

/// A worker: leases jobs from the store and proves them until told to stop.
#[derive(Debug)]
pub struct Worker {
    store: Store,
    config: WorkerConfig,
    prover: ProverConfig,
}

impl Worker {
    #[must_use]
    pub fn new(store: Store, config: WorkerConfig, prover: ProverConfig) -> Self {
        Self {
            store,
            config,
            prover,
        }
    }

    #[must_use]
    pub fn worker_id(&self) -> &str {
        &self.config.worker_id
    }

    /// Run until `shutdown` fires, returning every attempt made.
    ///
    /// Errors reaching the store are logged and retried rather than returned:
    /// a worker that exited because the database hiccuped would turn a blip
    /// into an outage, and every job it held would have to wait out its lease.
    pub async fn run(&self, mut shutdown: Shutdown) -> Vec<Outcome> {
        let mut outcomes = Vec::new();

        // Every worker reaps. A worker that is killed cannot return its own
        // leases — that is the entire point of a lease — so recovery has to
        // come from whoever is still alive. Making it every worker's job rather
        // than a dedicated process means a single surviving worker is enough,
        // and reaping is idempotent and `SKIP LOCKED`, so running it several
        // times over is harmless.
        let _reaper = TaskGuard::spawn(reap_loop(
            self.store.clone(),
            self.config.worker_id.clone(),
            self.config.reap_interval,
        ));

        loop {
            if shutdown.is_requested() {
                break;
            }

            let leased = tokio::select! {
                biased;
                () = shutdown.requested() => break,
                leased = self.store.lease_next(&self.config.worker_id, self.config.lease_ttl) => leased,
            };

            match leased {
                Ok(Some(job)) => {
                    let id = job.id;
                    let outcome = self.attempt(job, &mut shutdown).await;
                    tracing::info!(job = %id, outcome = ?outcome, "attempt finished");
                    outcomes.push(outcome);
                }
                Ok(None) => {
                    // Nothing to do. Sleep, but stay interruptible — a worker
                    // that ignored shutdown for the length of a poll would make
                    // every deploy that much slower.
                    tokio::select! {
                        biased;
                        () = shutdown.requested() => break,
                        () = tokio::time::sleep(self.config.poll_interval) => {}
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "could not lease; will retry");
                    tokio::select! {
                        biased;
                        () = shutdown.requested() => break,
                        () = tokio::time::sleep(self.config.poll_interval) => {}
                    }
                }
            }
        }

        tracing::info!(
            worker = %self.config.worker_id,
            attempts = outcomes.len(),
            "worker stopped"
        );
        outcomes
    }

    /// Prove one leased job, heartbeating throughout.
    async fn attempt(&self, job: Job, shutdown: &mut Shutdown) -> Outcome {
        let id = job.id;

        if let Err(err) = self.store.begin_proving(id, &self.config.worker_id).await {
            // The job was leased a moment ago, so this is either a lost lease
            // or a database problem. Either way there is nothing to record
            // against a job this worker may no longer own.
            tracing::warn!(job = %id, error = %err, "could not start proving");
            return Outcome::LeaseLost(id);
        }

        let mut heartbeat = TaskGuard::spawn(heartbeat(
            self.store.clone(),
            id,
            self.config.worker_id.clone(),
            self.config.lease_ttl,
            self.config.heartbeat_interval,
        ));

        // The label separates this attempt's scratch directory from any other,
        // including an earlier attempt at the same job on this worker.
        let label = format!("{id}-{}", job.attempts);
        let proving = prover::prove(&job.circuit_id, &job.inputs, &label, &self.prover);
        tokio::pin!(proving);

        let outcome = tokio::select! {
            biased;

            // Checked first: if the lease is gone, the proof is worthless.
            _ = heartbeat.handle() => {
                tracing::warn!(job = %id, "lease lost while proving; abandoning the attempt");
                Outcome::LeaseLost(id)
            }

            result = &mut proving => self.record(&job, result).await,

            () = shutdown.requested() => {
                match tokio::time::timeout(self.config.shutdown_grace, &mut proving).await {
                    Ok(result) => {
                        tracing::info!(job = %id, "finished during the shutdown grace period");
                        self.record(&job, result).await
                    }
                    Err(_) => {
                        tracing::info!(job = %id, "releasing the lease on shutdown");
                        if let Err(err) =
                            self.store.release_lease(id, &self.config.worker_id).await
                        {
                            tracing::warn!(job = %id, error = %err, "could not release the lease");
                        }
                        Outcome::Abandoned(id)
                    }
                }
            }
        };

        outcome
    }

    /// Write the result of a finished attempt to the store.
    async fn record(&self, job: &Job, result: Result<Proven, ProveError>) -> Outcome {
        let id = job.id;

        match result {
            Ok(proven) => {
                let duration_ms = i64::try_from(proven.duration.as_millis()).unwrap_or(i64::MAX);
                let peak = proven.peak_memory_kb.and_then(|kb| i64::try_from(kb).ok());

                match self
                    .store
                    .record_proof(
                        id,
                        &self.config.worker_id,
                        &proven.proof,
                        &proven.public_inputs,
                        duration_ms,
                        peak,
                    )
                    .await
                {
                    Ok(_) => Outcome::Proved(id),
                    Err(err) => {
                        // The proof exists but could not be stored. The lease
                        // will expire and another worker will redo the work —
                        // wasteful, but at-least-once is the contract and a
                        // silently dropped proof is not.
                        tracing::error!(job = %id, error = %err, "proved but could not record");
                        Outcome::LeaseLost(id)
                    }
                }
            }

            Err(failure) => {
                let kind = failure.kind();
                let attempt = u32::try_from(job.attempts.max(1)).unwrap_or(u32::MAX);
                let retry_in = match kind {
                    dray_core::FailureKind::Permanent => None,
                    dray_core::FailureKind::Transient => {
                        Some(self.config.backoff.delay_random(attempt))
                    }
                };

                tracing::warn!(
                    job = %id,
                    kind = ?kind,
                    label = failure.metric_label(),
                    retry_in = ?retry_in,
                    error = %failure,
                    "attempt failed"
                );

                if let Err(err) = self
                    .store
                    .record_failure(
                        id,
                        &self.config.worker_id,
                        kind,
                        &failure.to_string(),
                        retry_in,
                    )
                    .await
                {
                    tracing::error!(job = %id, error = %err, "could not record the failure");
                    return Outcome::LeaseLost(id);
                }

                Outcome::Failed { id, kind, retry_in }
            }
        }
    }
}

/// Renew a lease until it is refused.
///
/// Returns only when the lease is definitively lost. An error is not loss: see
/// the module documentation for why a database blip must not discard a proof
/// that is still running.
async fn heartbeat(store: Store, id: Uuid, worker_id: String, ttl: Duration, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    // `interval` fires immediately on the first tick; the lease was just
    // granted, so skip it.
    ticker.tick().await;

    loop {
        ticker.tick().await;
        match store.renew_lease(id, &worker_id, ttl).await {
            Ok(true) => tracing::debug!(job = %id, "lease renewed"),
            Ok(false) => return,
            Err(err) => {
                tracing::warn!(job = %id, error = %err, "lease renewal failed; will retry");
            }
        }
    }
}

/// Return expired leases to the queue, for ever.
///
/// Errors are logged and the loop continues. The reaper is a recovery
/// mechanism; a reaper that gave up on the first database error would be
/// missing during exactly the incident it exists for.
async fn reap_loop(store: Store, worker_id: String, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);

    loop {
        ticker.tick().await;
        match store.reap_expired_leases(&worker_id).await {
            Ok(reaped) if !reaped.is_empty() => {
                tracing::info!(count = reaped.len(), "returned expired leases to the queue");
            }
            Ok(_) => {}
            Err(err) => tracing::warn!(error = %err, "reaping failed; will retry"),
        }
    }
}

// ---------------------------------------------------------------------------
// Shutdown
// ---------------------------------------------------------------------------

/// Tells workers to stop taking new work.
///
/// Cloneable, so one signal can stop a whole pool.
#[derive(Debug, Clone)]
pub struct Shutdown(watch::Receiver<bool>);

/// Fires a [`Shutdown`].
#[derive(Debug, Clone)]
pub struct ShutdownHandle(watch::Sender<bool>);

/// Create a linked handle and signal.
#[must_use]
pub fn shutdown() -> (ShutdownHandle, Shutdown) {
    let (tx, rx) = watch::channel(false);
    (ShutdownHandle(tx), Shutdown(rx))
}

impl ShutdownHandle {
    /// Request shutdown. Idempotent.
    pub fn trigger(&self) {
        let _ = self.0.send(true);
    }
}

impl Shutdown {
    /// Whether shutdown has already been requested.
    #[must_use]
    pub fn is_requested(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once shutdown is requested.
    ///
    /// A dropped handle counts as a shutdown request. The alternative is a
    /// future that never completes, which would hang the worker on exactly the
    /// path meant to stop it.
    pub async fn requested(&mut self) {
        loop {
            if *self.0.borrow_and_update() {
                return;
            }
            if self.0.changed().await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_starts_unrequested() {
        let (_handle, signal) = shutdown();
        assert!(!signal.is_requested());
    }

    #[tokio::test]
    async fn triggering_wakes_a_waiter() {
        let (handle, mut signal) = shutdown();

        let waiter = tokio::spawn(async move {
            signal.requested().await;
            true
        });

        handle.trigger();
        assert!(waiter.await.expect("waiter should not panic"));
    }

    /// A worker that started waiting *after* the trigger must not miss it.
    /// Otherwise a shutdown racing a job boundary would hang the process.
    #[tokio::test]
    async fn a_late_waiter_sees_an_earlier_trigger() {
        let (handle, mut signal) = shutdown();
        handle.trigger();

        assert!(signal.is_requested());
        tokio::time::timeout(Duration::from_secs(1), signal.requested())
            .await
            .expect("a late waiter should return immediately");
    }

    #[tokio::test]
    async fn triggering_twice_is_harmless() {
        let (handle, mut signal) = shutdown();
        handle.trigger();
        handle.trigger();

        tokio::time::timeout(Duration::from_secs(1), signal.requested())
            .await
            .expect("should still resolve");
    }

    /// One signal has to stop a whole pool, so every clone must see it.
    #[tokio::test]
    async fn every_clone_sees_the_trigger() {
        let (handle, signal) = shutdown();
        let mut clones: Vec<Shutdown> = (0..4).map(|_| signal.clone()).collect();

        handle.trigger();

        for (index, clone) in clones.iter_mut().enumerate() {
            tokio::time::timeout(Duration::from_secs(1), clone.requested())
                .await
                .unwrap_or_else(|_| panic!("clone {index} missed the trigger"));
        }
    }

    /// Dropping the last handle must not leave workers waiting forever on the
    /// one path whose whole purpose is to make them stop.
    #[tokio::test]
    async fn a_dropped_handle_counts_as_shutdown() {
        let (handle, mut signal) = shutdown();
        drop(handle);

        tokio::time::timeout(Duration::from_secs(1), signal.requested())
            .await
            .expect("a dropped handle should release waiters");
    }

    /// The bug the chaos test found: a detached heartbeat outlived the worker
    /// that started it and went on renewing the lease for work that had
    /// stopped, so the job was never reaped and never came back.
    #[tokio::test]
    async fn a_guarded_task_stops_when_its_guard_is_dropped() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        let ticks = Arc::new(AtomicUsize::new(0));

        let guard = {
            let ticks = Arc::clone(&ticks);
            TaskGuard::spawn(async move {
                loop {
                    ticks.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
        };

        tokio::time::sleep(Duration::from_millis(80)).await;
        let while_alive = ticks.load(Ordering::SeqCst);
        assert!(while_alive > 0, "the task should have run at all");

        drop(guard);

        tokio::time::sleep(Duration::from_millis(80)).await;
        let after_drop = ticks.load(Ordering::SeqCst);
        assert!(
            after_drop <= while_alive + 1,
            "the task kept running after its guard was dropped: {while_alive} -> {after_drop}"
        );
    }

    #[test]
    fn an_outcome_reports_its_job() {
        let id = Uuid::new_v4();
        assert_eq!(Outcome::Proved(id).job_id(), id);
        assert_eq!(Outcome::LeaseLost(id).job_id(), id);
        assert_eq!(Outcome::Abandoned(id).job_id(), id);
        assert_eq!(
            Outcome::Failed {
                id,
                kind: dray_core::FailureKind::Transient,
                retry_in: None,
            }
            .job_id(),
            id
        );
    }

    /// The lease has to outlast a healthy proof by a clear margin. A lease that
    /// expired under a worker doing its job correctly would turn good work into
    /// duplicate work, which is the failure mode leasing exists to prevent.
    #[test]
    fn the_default_lease_outlasts_the_proving_wall_clock() {
        let config = WorkerConfig::new("w");
        let bounds = crate::bounded::Bounds::for_proving();

        assert!(
            config.lease_ttl > bounds.wall_clock,
            "lease TTL {:?} must exceed the proving wall clock {:?}",
            config.lease_ttl,
            bounds.wall_clock
        );
        assert!(
            config.heartbeat_interval * 3 <= config.lease_ttl,
            "the heartbeat should run several times per lease so one missed \
             renewal is not fatal"
        );
    }
}
