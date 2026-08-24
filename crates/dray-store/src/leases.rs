//! The Redis mirror of Postgres lease state.
//!
//! # Redis is a cache here, never truth
//!
//! Every lease already lives in Postgres, inside the same transaction as the
//! state change that created it. This mirror exists so a liveness question —
//! "is anyone still working on this job?" — can be answered without a Postgres
//! round trip, on a path that may be asked many times a second.
//!
//! That makes the failure mode the important part of the design. **Every
//! operation here is best-effort and returns nothing to fail on.** A Redis
//! outage must not fail a job, refuse a lease, or stop a proof; it must only
//! make liveness checks fall back to Postgres, which was authoritative all
//! along. The spec's invariant is explicit: Postgres state is always
//! recoverable without Redis.
//!
//! Two consequences worth stating, because both are easy to get wrong:
//!
//! - [`Liveness::Unknown`] is a distinct answer from "not live". A caller that
//!   collapsed the two would treat a Redis outage as *every lease having
//!   expired*, and would hand every in-flight job to a second worker — turning
//!   a cache outage into a stampede of duplicated proving.
//! - Redis expiry is not what returns a job to the queue. The reaper does that,
//!   from Postgres. If the key vanishes early the job is still leased; if it
//!   lingers, the reaper still takes the job back on time.

use std::time::Duration;

use uuid::Uuid;

/// Prefix for every key this module owns, so a shared Redis stays legible and
/// [`LeaseCache::rebuild`] can find what it put there.
const KEY_PREFIX: &str = "dray:lease:";

/// What the mirror can say about a lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Liveness {
    /// Someone holds this lease, and this is who.
    Held(String),
    /// The mirror is reachable and has no record of this lease.
    Free,
    /// The mirror could not answer. **Not** the same as [`Liveness::Free`]:
    /// treating it as free would hand every in-flight job to another worker
    /// during a Redis outage.
    Unknown,
}

impl Liveness {
    /// Whether this answer alone justifies taking the job.
    ///
    /// Only [`Liveness::Free`] does. `Unknown` means ask Postgres.
    #[must_use]
    pub fn is_definitely_free(&self) -> bool {
        matches!(self, Liveness::Free)
    }
}

/// A best-effort mirror of lease state in Redis.
#[derive(Clone)]
pub struct LeaseCache {
    connection: redis::aio::ConnectionManager,
}

impl std::fmt::Debug for LeaseCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseCache").finish_non_exhaustive()
    }
}

impl LeaseCache {
    /// Connect to Redis.
    ///
    /// This is the one operation that reports failure, because it happens at
    /// start-up where an operator can act on it. Everything afterwards is
    /// best-effort.
    ///
    /// `ConnectionManager` reconnects on its own, which is what lets the rest
    /// of the API stay infallible: a dropped connection heals without the
    /// caller ever hearing about it.
    ///
    /// # Errors
    ///
    /// Returns the underlying Redis error if the URL is unusable or the server
    /// cannot be reached.
    pub async fn connect(url: &str) -> Result<Self, redis::RedisError> {
        let client = redis::Client::open(url)?;
        let connection = redis::aio::ConnectionManager::new(client).await?;
        Ok(Self { connection })
    }

    fn key(id: Uuid) -> String {
        format!("{KEY_PREFIX}{id}")
    }

    /// Mirror a lease, with the same TTL Postgres granted.
    ///
    /// Redis expires the key on its own, so a worker that dies without
    /// releasing anything leaves no stale entry behind — the mirror converges
    /// on the truth even when nothing tidies up after it.
    pub async fn record(&self, id: Uuid, worker_id: &str, ttl: Duration) {
        // A sub-second TTL rounds to zero, and `SET ... EX 0` is an error
        // rather than an immediate expiry. One second is the smallest honest
        // value.
        let seconds = ttl.as_secs().max(1);

        let mut connection = self.connection.clone();
        let result: Result<(), redis::RedisError> = redis::cmd("SET")
            .arg(Self::key(id))
            .arg(worker_id)
            .arg("EX")
            .arg(seconds)
            .query_async(&mut connection)
            .await;

        Self::note(result, "recording a lease");
    }

    /// Drop a lease from the mirror, on release or completion.
    ///
    /// Failure is harmless: the key expires on its own, and Postgres is what
    /// decides whether the job is available.
    pub async fn forget(&self, id: Uuid) {
        let mut connection = self.connection.clone();
        let result: Result<(), redis::RedisError> = redis::cmd("DEL")
            .arg(Self::key(id))
            .query_async(&mut connection)
            .await;

        Self::note(result, "forgetting a lease");
    }

    /// Ask who holds a lease, without touching Postgres.
    pub async fn liveness(&self, id: Uuid) -> Liveness {
        let mut connection = self.connection.clone();
        let result: Result<Option<String>, redis::RedisError> = redis::cmd("GET")
            .arg(Self::key(id))
            .query_async(&mut connection)
            .await;

        match result {
            Ok(Some(worker)) => Liveness::Held(worker),
            Ok(None) => Liveness::Free,
            Err(err) => {
                tracing::warn!(error = %err, "lease liveness unavailable; falling back to Postgres");
                Liveness::Unknown
            }
        }
    }

    /// Rebuild the mirror from Postgres.
    ///
    /// Redis in this project runs without persistence *deliberately*, so that
    /// the recovery path is exercised in development rather than assumed. This
    /// is that path: after a Redis restart the mirror is empty, and every lease
    /// Postgres still holds is written back with the TTL remaining on it.
    ///
    /// Returns how many leases were restored.
    ///
    /// # Errors
    ///
    /// Propagates database failures. A Redis failure is not an error here for
    /// the same reason it is not elsewhere — the mirror is allowed to be
    /// incomplete.
    pub async fn rebuild(&self, store: &crate::Store) -> Result<usize, crate::StoreError> {
        let live = store.live_leases().await?;
        let count = live.len();

        for (id, worker_id, remaining) in live {
            self.record(id, &worker_id, remaining).await;
        }

        tracing::info!(count, "rebuilt the lease mirror from Postgres");
        Ok(count)
    }

    fn note(result: Result<(), redis::RedisError>, what: &str) {
        if let Err(err) = result {
            // Deliberately a warning, not an error: nothing is broken from the
            // system's point of view, because Postgres still holds the truth.
            tracing::warn!(error = %err, "{what} in the lease mirror failed; continuing");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_namespaced_and_identify_the_job() {
        let id = Uuid::new_v4();
        let key = LeaseCache::key(id);

        assert!(key.starts_with(KEY_PREFIX), "{key}");
        assert!(key.contains(&id.to_string()), "{key}");
    }

    /// The distinction this whole module depends on. Collapsing `Unknown` into
    /// `Free` would turn a Redis outage into every in-flight job being handed
    /// to a second worker.
    #[test]
    fn an_unavailable_mirror_does_not_report_a_lease_as_free() {
        assert!(Liveness::Free.is_definitely_free());
        assert!(!Liveness::Unknown.is_definitely_free());
        assert!(!Liveness::Held("worker-1".to_owned()).is_definitely_free());
    }
}
