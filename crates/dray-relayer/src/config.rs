//! Configuration, read from the environment.
//!
//! Parsing is separated from reading, as in the other services, so the rules
//! can be tested without `set_var` — which mutates process-global state and is
//! `unsafe` in this edition.
//!
//! # The one setting with no default
//!
//! `DRAY_RELAYER_KEY` is required. Every other setting has a default that works
//! against `make up` and a local Anvil, but a private key cannot have a
//! sensible default: any value that looked like one would be a published test
//! key, and a service that quietly falls back to a well-known key on a public
//! network is a way to lose money rather than a convenience.

use std::time::Duration;

use alloy::primitives::Address;

use crate::{gas::GasPolicy, relayer::RelayerConfig};

/// Runtime configuration for a relayer process.
#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub max_db_connections: u32,
    pub rpc_url: String,
    /// Hex private key for this relayer's account.
    pub private_key: String,
    pub settlement: Address,
    pub relayer: RelayerConfig,
    pub metrics_bind: String,
    pub log_level: String,
    pub log_json: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{name} must be set")]
    Missing { name: &'static str },

    #[error("{name} must be a number, found {value:?}: {source}")]
    NotANumber {
        name: &'static str,
        value: String,
        source: std::num::ParseIntError,
    },

    #[error("{name} must be greater than zero, found {value}")]
    NotPositive { name: &'static str, value: i64 },

    #[error("{name} must be an address, found {value:?}")]
    NotAnAddress { name: &'static str, value: String },
}

impl Config {
    /// Read configuration from the environment.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if a required setting is absent or a value is
    /// unusable. Refusing to start is deliberate: a relayer with no key, or
    /// pointed at no contract, can only fail every job it takes.
    pub fn from_env() -> Result<Self, ConfigError> {
        let read = |name: &str| std::env::var(name).ok();

        let private_key = read("DRAY_RELAYER_KEY").ok_or(ConfigError::Missing {
            name: "DRAY_RELAYER_KEY",
        })?;

        let settlement_raw = read("DRAY_SETTLEMENT").ok_or(ConfigError::Missing {
            name: "DRAY_SETTLEMENT",
        })?;
        let settlement = settlement_raw
            .trim()
            .parse()
            .map_err(|_| ConfigError::NotAnAddress {
                name: "DRAY_SETTLEMENT",
                value: settlement_raw.clone(),
            })?;

        let mut relayer =
            RelayerConfig::new(read("DRAY_RELAYER_ID").unwrap_or_else(default_relayer_id));
        relayer.confirmations = parse_positive(
            "DRAY_RELAYER_CONFIRMATIONS",
            read("DRAY_RELAYER_CONFIRMATIONS").as_deref(),
            5,
        )?
        .unsigned_abs();
        relayer.stuck_after = seconds(
            "DRAY_RELAYER_STUCK_AFTER",
            read("DRAY_RELAYER_STUCK_AFTER").as_deref(),
            30,
        )?;
        relayer.reorg_watch_window = seconds(
            "DRAY_RELAYER_REORG_WINDOW",
            read("DRAY_RELAYER_REORG_WINDOW").as_deref(),
            600,
        )?;
        relayer.lease_ttl = seconds(
            "DRAY_RELAYER_LEASE_TTL",
            read("DRAY_RELAYER_LEASE_TTL").as_deref(),
            300,
        )?;
        relayer.heartbeat_interval = seconds(
            "DRAY_RELAYER_HEARTBEAT",
            read("DRAY_RELAYER_HEARTBEAT").as_deref(),
            30,
        )?;
        relayer.confirm_poll_interval = seconds(
            "DRAY_RELAYER_CONFIRM_POLL",
            read("DRAY_RELAYER_CONFIRM_POLL").as_deref(),
            2,
        )?;
        relayer.reap_interval = seconds(
            "DRAY_RELAYER_REAP",
            read("DRAY_RELAYER_REAP").as_deref(),
            30,
        )?;
        relayer.shutdown_grace = seconds(
            "DRAY_RELAYER_SHUTDOWN_GRACE",
            read("DRAY_RELAYER_SHUTDOWN_GRACE").as_deref(),
            60,
        )?;
        relayer.gas = GasPolicy {
            max_fee_cap: parse_positive(
                "DRAY_RELAYER_MAX_FEE_WEI",
                read("DRAY_RELAYER_MAX_FEE_WEI").as_deref(),
                5_000_000_000,
            )?
            .unsigned_abs()
            .into(),
            max_priority_fee_cap: parse_positive(
                "DRAY_RELAYER_MAX_TIP_WEI",
                read("DRAY_RELAYER_MAX_TIP_WEI").as_deref(),
                1_000_000_000,
            )?
            .unsigned_abs()
            .into(),
            ..GasPolicy::default()
        };

        Ok(Self {
            database_url: read("DATABASE_URL")
                .unwrap_or_else(|| "postgres://dray:dray@localhost:5432/dray".to_owned()),
            max_db_connections: u32::try_from(parse_positive(
                "DRAY_RELAYER_DB_CONNECTIONS",
                read("DRAY_RELAYER_DB_CONNECTIONS").as_deref(),
                8,
            )?)
            .unwrap_or(8),
            rpc_url: read("DRAY_RPC_URL").unwrap_or_else(|| "http://127.0.0.1:8545".to_owned()),
            private_key,
            settlement,
            relayer,
            metrics_bind: read("DRAY_RELAYER_METRICS_BIND")
                .unwrap_or_else(|| "0.0.0.0:9091".to_owned()),
            log_level: read("DRAY_LOG_LEVEL").unwrap_or_else(|| "info".to_owned()),
            log_json: read("DRAY_LOG_JSON")
                .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true")),
        })
    }
}

/// An identifier that is stable for the life of the process and unlikely to
/// collide with another relayer's.
///
/// The identity is what `renew_lease` checks, so two relayers sharing one could
/// renew each other's leases and the ownership guarantee would quietly stop
/// holding.
fn default_relayer_id() -> String {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "relayer".to_owned());
    format!("{host}-{}", std::process::id())
}

fn seconds(name: &'static str, raw: Option<&str>, default: i64) -> Result<Duration, ConfigError> {
    Ok(Duration::from_secs(
        parse_positive(name, raw, default)?.unsigned_abs(),
    ))
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

    /// Zero confirmations would mark a job settled the instant it was mined,
    /// which is the same as not confirming at all. Refusing to start says so
    /// once rather than letting it show up as a reorg problem later.
    #[test]
    fn zero_and_negative_values_are_refused() {
        for value in ["0", "-1"] {
            assert!(parse_positive("X", Some(value), 7).is_err(), "{value}");
        }
    }

    #[test]
    fn a_non_numeric_value_names_itself_in_the_error() {
        let err = parse_positive("DRAY_RELAYER_CONFIRMATIONS", Some("five"), 5).unwrap_err();
        assert!(err.to_string().contains("five"), "{err}");
        assert!(
            err.to_string().contains("DRAY_RELAYER_CONFIRMATIONS"),
            "{err}"
        );
    }

    #[test]
    fn seconds_are_read_as_a_duration() {
        assert_eq!(
            seconds("X", Some("90"), 30).unwrap(),
            Duration::from_secs(90)
        );
        assert_eq!(seconds("X", None, 30).unwrap(), Duration::from_secs(30));
    }

    /// Two relayers sharing an id could renew each other's leases, voiding the
    /// guarantee that one job has one owner.
    #[test]
    fn the_default_relayer_id_distinguishes_the_process() {
        let id = default_relayer_id();
        assert!(
            id.contains(&std::process::id().to_string()),
            "relayer id {id} does not distinguish this process"
        );
    }
}
