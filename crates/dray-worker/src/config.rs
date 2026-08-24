//! Configuration, read from the environment.
//!
//! Every setting has a default that works against `make up` and a repository
//! checkout, so a reviewer can start a worker without first assembling a config
//! file.
//!
//! As in `dray-api`, parsing is separated from reading the environment
//! ([`parse_positive`], [`parse_list`]). `std::env::set_var` mutates
//! process-global state and is `unsafe` in this edition, so tests that needed
//! it would be both unsound and flaky under a concurrent test runner.

use std::{path::PathBuf, time::Duration};

use crate::{backoff::Backoff, bounded::Bounds, worker::WorkerConfig};

/// Runtime configuration for a worker process.
#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub max_db_connections: u32,
    /// Noir workspace to prepare artefacts from.
    pub circuits_dir: PathBuf,
    /// Where prepared artefacts are written.
    pub artifacts_dir: PathBuf,
    /// Parent directory for per-job scratch.
    pub scratch_dir: PathBuf,
    /// Circuits this worker can prove.
    pub circuits: Vec<String>,
    pub worker: WorkerConfig,
    pub bounds: Bounds,
    /// Where to serve `/metrics`.
    pub metrics_bind: String,
    pub log_level: String,
    pub log_json: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{name} must be a number, found {value:?}: {source}")]
    NotANumber {
        name: &'static str,
        value: String,
        source: std::num::ParseIntError,
    },

    #[error("{name} must be greater than zero, found {value}")]
    NotPositive { name: &'static str, value: i64 },

    #[error("{name} must list at least one circuit")]
    EmptyList { name: &'static str },
}

impl Config {
    /// Read configuration from the environment.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if a setting is unparseable or nonsensical.
    /// Refusing to start is deliberate: a worker with no circuits would lease
    /// jobs it can only fail, and finding that out from the failure rate is
    /// worse than finding it out from a refusal to boot.
    pub fn from_env() -> Result<Self, ConfigError> {
        let read = |name: &str| std::env::var(name).ok();

        let lease_ttl = parse_positive(
            "DRAY_WORKER_LEASE_TTL",
            read("DRAY_WORKER_LEASE_TTL").as_deref(),
            180,
        )?;
        let heartbeat = parse_positive(
            "DRAY_WORKER_HEARTBEAT",
            read("DRAY_WORKER_HEARTBEAT").as_deref(),
            30,
        )?;
        let wall_clock = parse_positive(
            "DRAY_WORKER_PROOF_TIMEOUT",
            read("DRAY_WORKER_PROOF_TIMEOUT").as_deref(),
            120,
        )?;

        let mut worker =
            WorkerConfig::new(read("DRAY_WORKER_ID").unwrap_or_else(default_worker_id));
        worker.lease_ttl = Duration::from_secs(lease_ttl.unsigned_abs());
        worker.heartbeat_interval = Duration::from_secs(heartbeat.unsigned_abs());
        worker.poll_interval = Duration::from_millis(
            parse_positive(
                "DRAY_WORKER_POLL_MS",
                read("DRAY_WORKER_POLL_MS").as_deref(),
                500,
            )?
            .unsigned_abs(),
        );
        worker.shutdown_grace = Duration::from_secs(
            parse_positive(
                "DRAY_WORKER_SHUTDOWN_GRACE",
                read("DRAY_WORKER_SHUTDOWN_GRACE").as_deref(),
                30,
            )?
            .unsigned_abs(),
        );
        worker.reap_interval = Duration::from_secs(
            parse_positive("DRAY_WORKER_REAP", read("DRAY_WORKER_REAP").as_deref(), 30)?
                .unsigned_abs(),
        );
        worker.backoff = Backoff {
            base: Duration::from_secs(
                parse_positive(
                    "DRAY_WORKER_BACKOFF_BASE",
                    read("DRAY_WORKER_BACKOFF_BASE").as_deref(),
                    1,
                )?
                .unsigned_abs(),
            ),
            max: Duration::from_secs(
                parse_positive(
                    "DRAY_WORKER_BACKOFF_MAX",
                    read("DRAY_WORKER_BACKOFF_MAX").as_deref(),
                    60,
                )?
                .unsigned_abs(),
            ),
        };

        let bounds = Bounds {
            wall_clock: Duration::from_secs(wall_clock.unsigned_abs()),
            address_space_kb: parse_positive(
                "DRAY_WORKER_MEMORY_KB",
                read("DRAY_WORKER_MEMORY_KB").as_deref(),
                4 * 1024 * 1024,
            )?
            .unsigned_abs(),
            cpu_seconds: parse_positive(
                "DRAY_WORKER_CPU_SECONDS",
                read("DRAY_WORKER_CPU_SECONDS").as_deref(),
                100,
            )?
            .unsigned_abs(),
        };

        Ok(Self {
            database_url: read("DATABASE_URL")
                .unwrap_or_else(|| "postgres://dray:dray@localhost:5432/dray".to_owned()),
            max_db_connections: u32::try_from(parse_positive(
                "DRAY_WORKER_DB_CONNECTIONS",
                read("DRAY_WORKER_DB_CONNECTIONS").as_deref(),
                8,
            )?)
            .unwrap_or(8),
            circuits_dir: read("DRAY_CIRCUITS_DIR")
                .map_or_else(|| PathBuf::from("circuits"), PathBuf::from),
            artifacts_dir: read("DRAY_ARTIFACTS_DIR").map_or_else(
                || std::env::temp_dir().join("dray-artifacts"),
                PathBuf::from,
            ),
            scratch_dir: read("DRAY_SCRATCH_DIR")
                .map_or_else(|| std::env::temp_dir().join("dray-scratch"), PathBuf::from),
            circuits: parse_list(
                "DRAY_CIRCUITS",
                read("DRAY_CIRCUITS").as_deref(),
                &["membership", "range_proof"],
            )?,
            worker,
            bounds,
            metrics_bind: read("DRAY_WORKER_METRICS_BIND")
                .unwrap_or_else(|| "0.0.0.0:9090".to_owned()),
            log_level: read("DRAY_LOG_LEVEL").unwrap_or_else(|| "info".to_owned()),
            log_json: read("DRAY_LOG_JSON")
                .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true")),
        })
    }
}

/// A worker identifier that is stable for the life of the process and unlikely
/// to collide with another worker's.
///
/// The identity matters: it is what `renew_lease` checks, so two workers
/// sharing one would be able to renew each other's leases and the
/// lease-ownership guarantee would quietly stop holding.
fn default_worker_id() -> String {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "worker".to_owned());
    format!("{host}-{}", std::process::id())
}

/// Parse a positive integer setting, or fall back to `default`.
///
/// # Errors
///
/// Returns [`ConfigError`] if the value is not a number or is not positive.
pub fn parse_positive(
    name: &'static str,
    raw: Option<&str>,
    default: i64,
) -> Result<i64, ConfigError> {
    let Some(raw) = raw else { return Ok(default) };

    let value = raw
        .parse::<i64>()
        .map_err(|source| ConfigError::NotANumber {
            name,
            value: raw.to_owned(),
            source,
        })?;

    if value <= 0 {
        return Err(ConfigError::NotPositive { name, value });
    }
    Ok(value)
}

/// Parse a comma-separated list, or fall back to `default`.
///
/// # Errors
///
/// Returns [`ConfigError::EmptyList`] if the value contains no entries. An
/// empty list is almost certainly a mistake — a worker that can prove nothing
/// still leases jobs, and fails every one of them.
pub fn parse_list(
    name: &'static str,
    raw: Option<&str>,
    default: &[&str],
) -> Result<Vec<String>, ConfigError> {
    let Some(raw) = raw else {
        return Ok(default.iter().map(|s| (*s).to_owned()).collect());
    };

    let items: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect();

    if items.is_empty() {
        return Err(ConfigError::EmptyList { name });
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_value_uses_the_default() {
        assert_eq!(parse_positive("X", None, 7).unwrap(), 7);
    }

    #[test]
    fn a_present_value_overrides_the_default() {
        assert_eq!(parse_positive("X", Some("42"), 7).unwrap(), 42);
    }

    #[test]
    fn a_non_numeric_value_is_refused() {
        let err = parse_positive("X", Some("soon"), 7).unwrap_err();
        assert!(err.to_string().contains("soon"), "{err}");
    }

    /// A zero lease TTL would expire every lease instantly and a zero timeout
    /// would fail every proof. Refusing to start says so once, rather than
    /// letting the operator infer it from a hundred failures.
    #[test]
    fn zero_and_negative_values_are_refused() {
        for value in ["0", "-1"] {
            assert!(
                parse_positive("X", Some(value), 7).is_err(),
                "{value} should be refused"
            );
        }
    }

    #[test]
    fn a_list_falls_back_to_its_default() {
        assert_eq!(
            parse_list("X", None, &["a", "b"]).unwrap(),
            vec!["a".to_owned(), "b".to_owned()]
        );
    }

    #[test]
    fn a_list_is_split_and_trimmed() {
        assert_eq!(
            parse_list("X", Some("membership, range_proof"), &[]).unwrap(),
            vec!["membership".to_owned(), "range_proof".to_owned()]
        );
    }

    #[test]
    fn an_empty_list_is_refused() {
        assert!(parse_list("X", Some(""), &["a"]).is_err());
        assert!(parse_list("X", Some(" , , "), &["a"]).is_err());
    }

    /// Two workers sharing an id could renew each other's leases, which would
    /// quietly void the guarantee that one job has one owner.
    #[test]
    fn the_default_worker_id_includes_the_process() {
        let id = default_worker_id();
        assert!(
            id.contains(&std::process::id().to_string()),
            "worker id {id} does not distinguish this process"
        );
    }
}
