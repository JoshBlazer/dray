//! The Redis lease mirror, against a real Redis.
//!
//! Gated behind `integration-tests` like the Postgres suite, and for the same
//! reason: a mirror that silently ran no tests would look exactly like one that
//! worked.
//!
//! Run with:
//!
//! ```sh
//! make up
//! DATABASE_URL=postgres://dray:dray@localhost:5432/dray_test \
//! REDIS_URL=redis://localhost:6379 \
//!     cargo test -p dray-store --features integration-tests
//! ```

#![cfg(feature = "integration-tests")]

use std::time::Duration;

use dray_store::{LeaseCache, Liveness};

fn redis_url() -> String {
    std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_owned())
}

async fn cache() -> LeaseCache {
    LeaseCache::connect(&redis_url())
        .await
        .expect("could not connect to Redis; run `make up`")
}

#[tokio::test]
async fn a_recorded_lease_is_visible_to_another_connection() {
    let writer = cache().await;
    let reader = cache().await;
    let id = uuid::Uuid::new_v4();

    assert_eq!(reader.liveness(id).await, Liveness::Free);

    writer.record(id, "worker-1", Duration::from_secs(60)).await;

    assert_eq!(
        reader.liveness(id).await,
        Liveness::Held("worker-1".to_owned()),
        "the mirror is shared state; another worker must see the lease"
    );

    writer.forget(id).await;
    assert_eq!(reader.liveness(id).await, Liveness::Free);
}

/// A worker that dies without releasing anything must not leave the mirror
/// claiming the job for ever. Redis expiry is what makes the mirror converge on
/// the truth with nothing tidying up after it.
#[tokio::test]
async fn a_mirrored_lease_expires_on_its_own() {
    let cache = cache().await;
    let id = uuid::Uuid::new_v4();

    cache
        .record(id, "doomed-worker", Duration::from_secs(1))
        .await;
    assert!(matches!(cache.liveness(id).await, Liveness::Held(_)));

    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert_eq!(
        cache.liveness(id).await,
        Liveness::Free,
        "a lease nobody released should have expired by itself"
    );
}

/// A sub-second TTL rounds to zero seconds, and `SET ... EX 0` is an error
/// rather than an immediate expiry — so a short lease would silently fail to
/// be mirrored at all.
#[tokio::test]
async fn a_sub_second_lease_is_still_mirrored() {
    let cache = cache().await;
    let id = uuid::Uuid::new_v4();

    cache.record(id, "brief", Duration::from_millis(200)).await;

    assert!(
        matches!(cache.liveness(id).await, Liveness::Held(_)),
        "a short TTL must not round down into not being recorded"
    );
    cache.forget(id).await;
}

/// The invariant the whole module rests on: an unreachable Redis reports
/// `Unknown`, never `Free`. Reporting `Free` would hand every in-flight job to
/// a second worker the moment the cache went down.
#[tokio::test]
async fn an_unreachable_mirror_reports_unknown_rather_than_free() {
    // Port 1 is reserved and nothing listens on it.
    let Ok(broken) = LeaseCache::connect("redis://127.0.0.1:1").await else {
        // Connecting failed outright, which is also a safe outcome — the caller
        // never gets a cache that lies.
        return;
    };

    let id = uuid::Uuid::new_v4();
    assert_eq!(
        broken.liveness(id).await,
        Liveness::Unknown,
        "an outage must be distinguishable from an absent lease"
    );
    assert!(!broken.liveness(id).await.is_definitely_free());
}

/// Writes to an unreachable mirror must not panic or block. Postgres holds the
/// truth; the cache being down is not the job's problem.
#[tokio::test]
async fn writing_to_an_unreachable_mirror_is_harmless() {
    let Ok(broken) = LeaseCache::connect("redis://127.0.0.1:1").await else {
        return;
    };

    let id = uuid::Uuid::new_v4();
    tokio::time::timeout(Duration::from_secs(10), async {
        broken.record(id, "worker-1", Duration::from_secs(60)).await;
        broken.forget(id).await;
    })
    .await
    .expect("best-effort writes must not hang the caller");
}

/// The recovery path the spec's invariant demands: Redis runs without
/// persistence here on purpose, so after a restart the mirror is empty and has
/// to be rebuilt from the side that never lost anything.
#[tokio::test]
async fn the_mirror_can_be_rebuilt_from_postgres() {
    let store = isolated_store("rebuild").await;
    let cache = cache().await;

    let ids = seed_jobs(&store, 3).await;

    // Two of the three are leased.
    let first = store
        .lease_next("worker-1", Duration::from_secs(120))
        .await
        .unwrap()
        .unwrap();
    let second = store
        .lease_next("worker-2", Duration::from_secs(120))
        .await
        .unwrap()
        .unwrap();

    // Simulate a Redis that lost everything.
    for id in &ids {
        cache.forget(*id).await;
    }
    assert_eq!(cache.liveness(first.id).await, Liveness::Free);

    let restored = cache.rebuild(&store).await.expect("rebuild should succeed");
    assert_eq!(restored, 2, "only the leased jobs should be restored");

    assert_eq!(
        cache.liveness(first.id).await,
        Liveness::Held("worker-1".to_owned())
    );
    assert_eq!(
        cache.liveness(second.id).await,
        Liveness::Held("worker-2".to_owned())
    );

    let unleased = ids
        .iter()
        .find(|id| **id != first.id && **id != second.id)
        .expect("a third job");
    assert_eq!(
        cache.liveness(*unleased).await,
        Liveness::Free,
        "a queued job must not be advertised as held"
    );

    for id in &ids {
        cache.forget(*id).await;
    }
}

/// An expired lease belongs to the reaper. Writing it back into the mirror
/// would advertise a holder for a job that is about to be taken off them.
#[tokio::test]
async fn rebuilding_skips_leases_that_have_already_expired() {
    let store = isolated_store("rebuild_expired").await;
    let cache = cache().await;

    let ids = seed_jobs(&store, 1).await;
    store
        .lease_next("doomed", Duration::from_secs(0))
        .await
        .unwrap()
        .unwrap();

    let restored = cache.rebuild(&store).await.expect("rebuild should succeed");

    assert_eq!(restored, 0, "an expired lease is not live");
    assert_eq!(cache.liveness(ids[0]).await, Liveness::Free);
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

use dray_store::{Circuit, Store};

fn database_url() -> String {
    std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run the integration tests")
}

async fn isolated_store(label: &str) -> Store {
    let admin_url = database_url();
    let db = format!("dray_mirror_{}_{label}", std::process::id());

    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("could not connect to Postgres");
    sqlx::query(&format!(r#"DROP DATABASE IF EXISTS "{db}" WITH (FORCE)"#))
        .execute(&admin)
        .await
        .expect("could not drop the test database");
    sqlx::query(&format!(r#"CREATE DATABASE "{db}""#))
        .execute(&admin)
        .await
        .expect("could not create the test database");
    admin.close().await;

    let (prefix, _) = admin_url
        .rsplit_once('/')
        .expect("DATABASE_URL should contain a database name");
    let store = Store::connect(&format!("{prefix}/{db}"), 4)
        .await
        .expect("could not connect to the test database");
    store.migrate().await.expect("migrations failed");
    store
}

async fn seed_jobs(store: &Store, count: usize) -> Vec<uuid::Uuid> {
    store
        .upsert_circuit(&Circuit {
            id: "c".into(),
            display_name: "c".into(),
            input_schema: serde_json::json!({"type": "object"}),
            verifier_address: None,
            enabled: true,
        })
        .await
        .expect("circuit");

    let mut ids = Vec::with_capacity(count);
    for n in 0..count {
        let (job, _) = store
            .enqueue("c", &serde_json::json!({"n": n}), None, 3)
            .await
            .expect("enqueue");
        ids.push(job.id);
    }
    ids
}
