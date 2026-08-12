//! Configuration, read from the environment.
//!
//! Every setting has a default that works against `make up`, so a reviewer can
//! start the service without first assembling a config file.
//!
//! The parsing is separated from the reading: [`parse_positive`] and
//! [`parse_flag`] take the raw value and know nothing about the environment.
//! That is what makes them testable without `set_var`, which mutates
//! process-global state and would make two tests running concurrently flake.

use crate::validate::Limits;

/// Runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub bind_address: String,
    pub max_db_connections: u32,
    pub migrate_on_start: bool,
    pub max_queue_depth: i64,
    pub default_max_attempts: i32,
    pub log_level: String,
    pub log_json: bool,
    pub limits: Limits,
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
}

impl Config {
    /// Read configuration from the environment.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if a numeric setting is unparseable or
    /// nonsensical. Failing at start-up is deliberate: a queue-depth limit of
    /// zero would refuse every request, and discovering that from traffic is
    /// worse than discovering it from a refusal to boot.
    pub fn from_env() -> Result<Self, ConfigError> {
        let read = |name: &str| std::env::var(name).ok();

        Ok(Self {
            database_url: read("DATABASE_URL")
                .unwrap_or_else(|| "postgres://dray:dray@localhost:5432/dray".to_owned()),
            bind_address: read("DRAY_API_BIND").unwrap_or_else(|| "0.0.0.0:8080".to_owned()),
            max_db_connections: u32::try_from(parse_positive(
                "DRAY_API_DB_CONNECTIONS",
                read("DRAY_API_DB_CONNECTIONS").as_deref(),
                16,
            )?)
            .unwrap_or(u32::MAX),
            migrate_on_start: parse_flag(read("DRAY_API_MIGRATE_ON_START").as_deref(), true),
            max_queue_depth: parse_positive(
                "DRAY_API_MAX_QUEUE_DEPTH",
                read("DRAY_API_MAX_QUEUE_DEPTH").as_deref(),
                10_000,
            )?,
            default_max_attempts: i32::try_from(parse_positive(
                "DRAY_MAX_ATTEMPTS",
                read("DRAY_MAX_ATTEMPTS").as_deref(),
                3,
            )?)
            .unwrap_or(i32::MAX),
            log_level: read("RUST_LOG").unwrap_or_else(|| "info,dray_api=debug".to_owned()),
            log_json: parse_flag(read("DRAY_LOG_JSON").as_deref(), false),
            limits: Limits {
                max_body_bytes: usize::try_from(parse_positive(
                    "DRAY_API_MAX_BODY_BYTES",
                    read("DRAY_API_MAX_BODY_BYTES").as_deref(),
                    256 * 1024,
                )?)
                .unwrap_or(usize::MAX),
                ..Limits::default()
            },
        })
    }
}

/// Parse a setting that must be a positive integer.
///
/// # Errors
///
/// Returns [`ConfigError::NotANumber`] for unparseable input and
/// [`ConfigError::NotPositive`] for zero or negative values. Neither falls back
/// to the default: silently ignoring what an operator asked for is worse than
/// refusing to start.
pub fn parse_positive(
    name: &'static str,
    raw: Option<&str>,
    default: i64,
) -> Result<i64, ConfigError> {
    let Some(raw) = raw else {
        return Ok(default);
    };

    let value = raw
        .trim()
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

/// Parse a boolean setting, accepting the spellings people actually write.
#[must_use]
pub fn parse_flag(raw: Option<&str>, default: bool) -> bool {
    match raw {
        Some(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        None => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_value_uses_the_default() {
        assert_eq!(parse_positive("X", None, 42).unwrap(), 42);
    }

    #[test]
    fn a_present_value_overrides_the_default() {
        assert_eq!(parse_positive("X", Some("7"), 42).unwrap(), 7);
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        // People paste values out of dashboards and docs.
        assert_eq!(parse_positive("X", Some("  9  "), 42).unwrap(), 9);
    }

    /// Falling back to the default here would silently ignore what the operator
    /// asked for, and the first sign of it would be behaviour nobody configured.
    #[test]
    fn unparseable_input_fails_rather_than_defaulting() {
        assert!(matches!(
            parse_positive("X", Some("not-a-number"), 42),
            Err(ConfigError::NotANumber { .. })
        ));
    }

    #[test]
    fn zero_and_negatives_are_rejected() {
        // A queue-depth limit of zero refuses every request; a connection pool
        // of zero can never serve one. Neither is ever intended.
        for bad in ["0", "-1", "-9999"] {
            assert!(
                matches!(
                    parse_positive("X", Some(bad), 42),
                    Err(ConfigError::NotPositive { .. })
                ),
                "{bad:?} should have been rejected"
            );
        }
    }

    #[test]
    fn error_messages_name_the_setting() {
        let Err(err) = parse_positive("DRAY_API_MAX_QUEUE_DEPTH", Some("0"), 1) else {
            panic!("expected a rejection");
        };
        assert!(
            err.to_string().contains("DRAY_API_MAX_QUEUE_DEPTH"),
            "operator needs to know which setting: {err}"
        );
    }

    #[test]
    fn flags_default_when_unset() {
        assert!(parse_flag(None, true));
        assert!(!parse_flag(None, false));
    }

    #[test]
    fn flags_accept_the_usual_spellings() {
        for truthy in ["1", "true", "TRUE", "True", "yes", "on", " true "] {
            assert!(parse_flag(Some(truthy), false), "{truthy:?} should be true");
        }
        for falsy in ["0", "false", "no", "off", "", "nonsense"] {
            assert!(!parse_flag(Some(falsy), true), "{falsy:?} should be false");
        }
    }
}
