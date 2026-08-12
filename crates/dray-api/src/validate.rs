//! Request validation.
//!
//! Kept separate from the HTTP layer and free of I/O so the rules can be tested
//! without a server or a database.
//!
//! Validation runs *before* anything is persisted, and its job is to make the
//! rest of the system's assumptions true. A worker should never receive a job
//! whose inputs cannot possibly satisfy the circuit, because discovering that
//! costs a lease, a subprocess, and an attempt.

use serde_json::Value;

/// Limits on what a client may submit.
///
/// These exist because a malicious or broken client is expected, not
/// hypothetical. Every one of them bounds work the server would otherwise do on
/// the client's behalf.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Largest accepted request body, in bytes.
    pub max_body_bytes: usize,
    /// Longest accepted circuit identifier.
    pub max_circuit_id_len: usize,
    /// Longest accepted idempotency key.
    pub max_idempotency_key_len: usize,
    /// Most elements permitted in any single input array.
    pub max_array_len: usize,
    /// Longest accepted string value inside the inputs.
    pub max_string_len: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            // Generous next to the largest legitimate request — a depth-20
            // Merkle path is a few kilobytes — and small enough that a flood of
            // them cannot exhaust memory.
            max_body_bytes: 256 * 1024,
            max_circuit_id_len: 128,
            max_idempotency_key_len: 255,
            max_array_len: 4096,
            max_string_len: 4096,
        }
    }
}

/// Why a request was rejected.
///
/// Each variant carries enough detail for a client to fix the request without
/// guessing. An error that only says "invalid input" turns a five-second fix
/// into a support conversation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("circuit id must not be empty")]
    EmptyCircuitId,

    #[error("circuit id is {len} characters, exceeding the limit of {limit}")]
    CircuitIdTooLong { len: usize, limit: usize },

    #[error(
        "circuit id {id:?} contains characters outside [a-z0-9_-]; \
         circuit ids appear in metrics and paths, so they are kept plain"
    )]
    CircuitIdNotSlug { id: String },

    #[error("idempotency key is {len} characters, exceeding the limit of {limit}")]
    IdempotencyKeyTooLong { len: usize, limit: usize },

    #[error("inputs must be a JSON object, found {found}")]
    InputsNotAnObject { found: &'static str },

    #[error("array at {path} has {len} elements, exceeding the limit of {limit}")]
    ArrayTooLong {
        path: String,
        len: usize,
        limit: usize,
    },

    #[error("string at {path} is {len} characters, exceeding the limit of {limit}")]
    StringTooLong {
        path: String,
        len: usize,
        limit: usize,
    },

    #[error("inputs do not match the schema for this circuit: {detail}")]
    SchemaMismatch { detail: String },

    #[error("inputs could not be canonicalised: {0}")]
    Canonical(#[from] dray_core::CanonicalError),
}

/// Describes the shape of a JSON value for error messages.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Check a circuit identifier.
///
/// # Errors
///
/// Returns a [`ValidationError`] describing the first problem found.
pub fn check_circuit_id(id: &str, limits: &Limits) -> Result<(), ValidationError> {
    if id.is_empty() {
        return Err(ValidationError::EmptyCircuitId);
    }
    if id.len() > limits.max_circuit_id_len {
        return Err(ValidationError::CircuitIdTooLong {
            len: id.len(),
            limit: limits.max_circuit_id_len,
        });
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(ValidationError::CircuitIdNotSlug { id: id.to_owned() });
    }
    Ok(())
}

/// Check an optional idempotency key.
///
/// # Errors
///
/// Returns [`ValidationError::IdempotencyKeyTooLong`] if it exceeds the limit.
pub fn check_idempotency_key(key: Option<&str>, limits: &Limits) -> Result<(), ValidationError> {
    if let Some(key) = key {
        if key.len() > limits.max_idempotency_key_len {
            return Err(ValidationError::IdempotencyKeyTooLong {
                len: key.len(),
                limit: limits.max_idempotency_key_len,
            });
        }
    }
    Ok(())
}

/// Check the structural limits on submitted inputs.
///
/// Runs before schema validation because these bounds are about resource use
/// rather than correctness: a 10-million-element array should be rejected on
/// sight, not after a schema walk over all of it.
///
/// # Errors
///
/// Returns a [`ValidationError`] describing the first violation found.
pub fn check_input_limits(inputs: &Value, limits: &Limits) -> Result<(), ValidationError> {
    if !inputs.is_object() {
        return Err(ValidationError::InputsNotAnObject {
            found: type_name(inputs),
        });
    }
    walk(inputs, "$", limits)
}

fn walk(value: &Value, path: &str, limits: &Limits) -> Result<(), ValidationError> {
    match value {
        Value::String(s) => {
            if s.chars().count() > limits.max_string_len {
                return Err(ValidationError::StringTooLong {
                    path: path.to_owned(),
                    len: s.chars().count(),
                    limit: limits.max_string_len,
                });
            }
        }
        Value::Array(items) => {
            if items.len() > limits.max_array_len {
                return Err(ValidationError::ArrayTooLong {
                    path: path.to_owned(),
                    len: items.len(),
                    limit: limits.max_array_len,
                });
            }
            for (i, item) in items.iter().enumerate() {
                walk(item, &format!("{path}[{i}]"), limits)?;
            }
        }
        Value::Object(map) => {
            for (key, val) in map {
                walk(val, &format!("{path}.{key}"), limits)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Validate inputs against the circuit's declared JSON Schema.
///
/// The schema lives in the `circuits` table rather than in this code, which is
/// what keeps the API circuit-agnostic: adding a circuit is a row, not a
/// release.
///
/// # Errors
///
/// Returns [`ValidationError::SchemaMismatch`] listing what did not match.
pub fn check_against_schema(inputs: &Value, schema: &Value) -> Result<(), ValidationError> {
    let validator = jsonschema::validator_for(schema).map_err(|e| {
        // A malformed schema is an operator error, not a client one, but the
        // client still has to be told why their request failed.
        ValidationError::SchemaMismatch {
            detail: format!("the circuit's schema is itself invalid: {e}"),
        }
    })?;

    let problems: Vec<String> = validator
        .iter_errors(inputs)
        .map(|error| {
            let location = error.instance_path().to_string();
            if location.is_empty() {
                error.to_string()
            } else {
                format!("{location}: {error}")
            }
        })
        .collect();

    if problems.is_empty() {
        Ok(())
    } else {
        Err(ValidationError::SchemaMismatch {
            detail: problems.join("; "),
        })
    }
}

/// Run every check, in the order that rejects cheapest-first.
///
/// # Errors
///
/// Returns the first [`ValidationError`] encountered.
pub fn validate_submission(
    circuit_id: &str,
    inputs: &Value,
    idempotency_key: Option<&str>,
    schema: &Value,
    limits: &Limits,
) -> Result<(), ValidationError> {
    check_circuit_id(circuit_id, limits)?;
    check_idempotency_key(idempotency_key, limits)?;
    check_input_limits(inputs, limits)?;
    check_against_schema(inputs, schema)?;

    // Canonicalisation can fail on its own terms — a float, a duplicate key,
    // excessive nesting. Doing it here means the failure is reported as a
    // client error at the boundary rather than surfacing later as a store
    // error with no request context attached.
    dray_core::canonicalise(inputs)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn limits() -> Limits {
        Limits::default()
    }

    fn permissive_schema() -> Value {
        json!({"type": "object"})
    }

    #[test]
    fn accepts_a_well_formed_submission() {
        let schema = json!({
            "type": "object",
            "required": ["secret", "leaf_index"],
            "properties": {
                "secret": {"type": "string"},
                "leaf_index": {"type": "string"},
            },
        });
        let inputs = json!({"secret": "42", "leaf_index": "5"});
        assert!(validate_submission("membership", &inputs, Some("k"), &schema, &limits()).is_ok());
    }

    #[test]
    fn rejects_an_empty_circuit_id() {
        assert_eq!(
            check_circuit_id("", &limits()),
            Err(ValidationError::EmptyCircuitId)
        );
    }

    #[test]
    fn rejects_a_circuit_id_that_is_not_a_slug() {
        // Circuit ids end up in metric labels and log lines. Allowing arbitrary
        // text there invites both confusion and injection.
        for bad in ["Membership", "member ship", "member/ship", "member;drop"] {
            assert!(
                matches!(
                    check_circuit_id(bad, &limits()),
                    Err(ValidationError::CircuitIdNotSlug { .. })
                ),
                "{bad:?} should have been rejected"
            );
        }
    }

    #[test]
    fn accepts_conventional_circuit_ids() {
        for good in ["membership", "range_proof", "range-proof-2", "v2_circuit"] {
            assert!(check_circuit_id(good, &limits()).is_ok(), "{good:?}");
        }
    }

    #[test]
    fn rejects_an_overlong_circuit_id() {
        let long = "a".repeat(limits().max_circuit_id_len + 1);
        assert!(matches!(
            check_circuit_id(&long, &limits()),
            Err(ValidationError::CircuitIdTooLong { .. })
        ));
    }

    #[test]
    fn rejects_an_overlong_idempotency_key() {
        let long = "k".repeat(limits().max_idempotency_key_len + 1);
        assert!(matches!(
            check_idempotency_key(Some(&long), &limits()),
            Err(ValidationError::IdempotencyKeyTooLong { .. })
        ));
    }

    #[test]
    fn an_absent_idempotency_key_is_fine() {
        assert!(check_idempotency_key(None, &limits()).is_ok());
    }

    #[test]
    fn rejects_inputs_that_are_not_an_object() {
        for bad in [json!([1, 2]), json!("str"), json!(1), json!(null)] {
            assert!(
                matches!(
                    check_input_limits(&bad, &limits()),
                    Err(ValidationError::InputsNotAnObject { .. })
                ),
                "{bad} should have been rejected"
            );
        }
    }

    #[test]
    fn rejects_an_oversized_array() {
        let big = json!({"siblings": vec![0u8; limits().max_array_len + 1]});
        assert!(matches!(
            check_input_limits(&big, &limits()),
            Err(ValidationError::ArrayTooLong { .. })
        ));
    }

    #[test]
    fn rejects_an_oversized_string() {
        let big = json!({"secret": "x".repeat(limits().max_string_len + 1)});
        assert!(matches!(
            check_input_limits(&big, &limits()),
            Err(ValidationError::StringTooLong { .. })
        ));
    }

    #[test]
    fn limit_errors_name_the_offending_path() {
        let nested = json!({"outer": {"inner": ["x".repeat(limits().max_string_len + 1)]}});
        let Err(ValidationError::StringTooLong { path, .. }) =
            check_input_limits(&nested, &limits())
        else {
            panic!("expected a string-length rejection");
        };
        assert_eq!(path, "$.outer.inner[0]");
    }

    #[test]
    fn rejects_inputs_missing_a_required_field() {
        let schema = json!({
            "type": "object",
            "required": ["secret"],
            "properties": {"secret": {"type": "string"}},
        });
        let Err(ValidationError::SchemaMismatch { detail }) =
            check_against_schema(&json!({}), &schema)
        else {
            panic!("expected a schema mismatch");
        };
        assert!(
            detail.contains("secret"),
            "error should name the field: {detail}"
        );
    }

    #[test]
    fn rejects_a_field_of_the_wrong_type() {
        let schema = json!({
            "type": "object",
            "properties": {"leaf_index": {"type": "string"}},
        });
        assert!(matches!(
            check_against_schema(&json!({"leaf_index": 5}), &schema),
            Err(ValidationError::SchemaMismatch { .. })
        ));
    }

    /// The membership circuit's real shape, as the API will hold it.
    #[test]
    fn enforces_the_membership_circuit_shape() {
        let schema = json!({
            "type": "object",
            "required": ["secret", "leaf_index", "siblings"],
            "additionalProperties": false,
            "properties": {
                "secret": {"type": "string"},
                "leaf_index": {"type": "string"},
                "siblings": {
                    "type": "array",
                    "items": {"type": "string"},
                    "minItems": 20,
                    "maxItems": 20,
                },
            },
        });

        let good = json!({
            "secret": "42",
            "leaf_index": "5",
            "siblings": vec!["7"; 20],
        });
        assert!(check_against_schema(&good, &schema).is_ok());

        // A path of the wrong length cannot satisfy a depth-20 tree, so
        // rejecting it here saves a worker a pointless proving attempt.
        let short = json!({
            "secret": "42",
            "leaf_index": "5",
            "siblings": vec!["7"; 19],
        });
        assert!(check_against_schema(&short, &schema).is_err());

        // Unexpected fields are refused: they usually mean the client is
        // talking to the wrong circuit.
        let extra = json!({
            "secret": "42",
            "leaf_index": "5",
            "siblings": vec!["7"; 20],
            "surprise": "1",
        });
        assert!(check_against_schema(&extra, &schema).is_err());
    }

    #[test]
    fn rejects_floats_at_the_boundary() {
        // Canonicalisation refuses floats. Running it during validation means
        // the client gets a 400 explaining why, rather than a 500 later.
        let result = validate_submission(
            "membership",
            &json!({"value": 1.5}),
            None,
            &permissive_schema(),
            &limits(),
        );
        assert!(matches!(result, Err(ValidationError::Canonical(_))));
    }

    #[test]
    fn a_malformed_circuit_schema_is_reported_not_panicked() {
        let nonsense = json!({"type": "not-a-real-type"});
        assert!(matches!(
            check_against_schema(&json!({}), &nonsense),
            Err(ValidationError::SchemaMismatch { .. })
        ));
    }

    #[test]
    fn cheap_checks_run_before_expensive_ones() {
        // A bad circuit id must be rejected without walking a large input body.
        let huge = json!({"siblings": vec![0u8; limits().max_array_len + 1]});
        assert_eq!(
            validate_submission("BAD ID", &huge, None, &permissive_schema(), &limits()),
            Err(ValidationError::CircuitIdNotSlug {
                id: "BAD ID".into()
            })
        );
    }
}
