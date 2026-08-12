//! Canonical input encoding and job identity.
//!
//! Idempotency depends entirely on two clients submitting semantically
//! identical inputs producing byte-identical bytes to hash. JSON does not give
//! that for free: object key order is insignificant, whitespace is
//! insignificant, and numbers have many spellings. Canonicalisation removes
//! that freedom before hashing.
//!
//! The rules, deliberately strict:
//!
//! - Object keys are sorted by their Unicode scalar values.
//! - No insignificant whitespace.
//! - Duplicate keys are rejected rather than silently resolved.
//! - **Floating-point numbers are rejected outright.**
//!
//! That last rule deserves justification. A general JSON canonicaliser has to
//! pin down float formatting (RFC 8785 defers to ECMAScript's algorithm), which
//! is fiddly and easy to get subtly wrong. Circuit inputs are field elements,
//! integers, and hex strings — a float in one is already a bug. Rejecting them
//! removes an entire class of canonicalisation hazard at no cost to any input
//! this system should ever accept.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

/// Why an input could not be canonicalised.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CanonicalError {
    #[error(
        "floating-point numbers are not accepted in circuit inputs (at {path}); \
             use an integer or a decimal string"
    )]
    FloatNotAllowed { path: String },

    #[error("duplicate object key {key:?} at {path}")]
    DuplicateKey { path: String, key: String },

    #[error("input nests {depth} levels deep, exceeding the limit of {limit}")]
    TooDeep { depth: usize, limit: usize },
}

/// Maximum nesting depth. Deep nesting is not useful for circuit inputs and is
/// a cheap way to make a parser do a lot of work, so it is bounded.
pub const MAX_DEPTH: usize = 32;

/// Produce the canonical byte encoding of a JSON value.
///
/// # Errors
///
/// See [`CanonicalError`].
pub fn canonicalise(value: &serde_json::Value) -> Result<String, CanonicalError> {
    let mut out = String::new();
    write_canonical(value, &mut out, "$", 0)?;
    Ok(out)
}

fn write_canonical(
    value: &serde_json::Value,
    out: &mut String,
    path: &str,
    depth: usize,
) -> Result<(), CanonicalError> {
    if depth > MAX_DEPTH {
        return Err(CanonicalError::TooDeep {
            depth,
            limit: MAX_DEPTH,
        });
    }

    match value {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),

        serde_json::Value::Number(n) => {
            // `serde_json` classifies any number that is not representable as
            // u64/i64 as a float, which is exactly the set being rejected.
            if let Some(u) = n.as_u64() {
                out.push_str(&u.to_string());
            } else if let Some(i) = n.as_i64() {
                out.push_str(&i.to_string());
            } else {
                return Err(CanonicalError::FloatNotAllowed {
                    path: path.to_owned(),
                });
            }
        }

        serde_json::Value::String(s) => write_json_string(s, out),

        serde_json::Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out, &format!("{path}[{i}]"), depth + 1)?;
            }
            out.push(']');
        }

        serde_json::Value::Object(map) => {
            // serde_json's default Map preserves insertion order, so sorting
            // here is what actually makes key order insignificant.
            let mut sorted: BTreeMap<&str, &serde_json::Value> = BTreeMap::new();
            for (key, val) in map {
                if sorted.insert(key.as_str(), val).is_some() {
                    return Err(CanonicalError::DuplicateKey {
                        path: path.to_owned(),
                        key: key.clone(),
                    });
                }
            }

            out.push('{');
            for (i, (key, val)) in sorted.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_string(key, out);
                out.push(':');
                write_canonical(val, out, &format!("{path}.{key}"), depth + 1)?;
            }
            out.push('}');
        }
    }

    Ok(())
}

/// Writes a JSON string with the minimal escaping the grammar requires.
///
/// Minimal and *deterministic* — two encoders that differ on whether to escape
/// `/` or use `A` for `A` would break idempotency for identical inputs.
fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The canonical identity of a job: `SHA-256(circuit_id || 0x00 || inputs)`.
///
/// The zero byte is a separator, so that a circuit named `"ab"` with inputs
/// `"c"` cannot collide with one named `"a"` with inputs `"bc"`. Without it,
/// two different jobs could share an identity and one would be silently
/// deduplicated away as a copy of the other.
///
/// # Errors
///
/// Propagates any [`CanonicalError`] from encoding the inputs.
pub fn job_hash(circuit_id: &str, inputs: &serde_json::Value) -> Result<[u8; 32], CanonicalError> {
    let canonical = canonicalise(inputs)?;

    let mut hasher = Sha256::new();
    hasher.update(circuit_id.as_bytes());
    hasher.update([0u8]);
    hasher.update(canonical.as_bytes());
    Ok(hasher.finalize().into())
}

/// [`job_hash`], hex-encoded for logs, URLs, and database columns.
///
/// # Errors
///
/// Propagates any [`CanonicalError`] from encoding the inputs.
pub fn job_hash_hex(
    circuit_id: &str,
    inputs: &serde_json::Value,
) -> Result<String, CanonicalError> {
    Ok(hex::encode(job_hash(circuit_id, inputs)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("test input should be valid JSON")
    }

    #[test]
    fn key_order_is_insignificant() {
        let a = parse(r#"{"secret":"42","index":"5"}"#);
        let b = parse(r#"{"index":"5","secret":"42"}"#);
        assert_eq!(canonicalise(&a).unwrap(), canonicalise(&b).unwrap());
    }

    #[test]
    fn whitespace_is_insignificant() {
        let a = parse(r#"{"a":1,"b":[2,3]}"#);
        let b = parse("{\n  \"a\" : 1,\n  \"b\" : [ 2, 3 ]\n}");
        assert_eq!(canonicalise(&a).unwrap(), canonicalise(&b).unwrap());
    }

    #[test]
    fn nested_key_order_is_insignificant() {
        let a = parse(r#"{"outer":{"z":1,"a":{"y":2,"b":3}}}"#);
        let b = parse(r#"{"outer":{"a":{"b":3,"y":2},"z":1}}"#);
        assert_eq!(canonicalise(&a).unwrap(), canonicalise(&b).unwrap());
    }

    /// Arrays are ordered data. Reordering them changes meaning — a Merkle
    /// authentication path in the wrong order is a different path.
    #[test]
    fn array_order_is_significant() {
        let a = parse(r#"{"siblings":["1","2"]}"#);
        let b = parse(r#"{"siblings":["2","1"]}"#);
        assert_ne!(canonicalise(&a).unwrap(), canonicalise(&b).unwrap());
    }

    #[test]
    fn canonical_output_is_compact_and_sorted() {
        let v = parse(r#"{ "b" : 2 , "a" : 1 }"#);
        assert_eq!(canonicalise(&v).unwrap(), r#"{"a":1,"b":2}"#);
    }

    #[test]
    fn floats_are_rejected() {
        let v = parse(r#"{"value":1.5}"#);
        assert!(matches!(
            canonicalise(&v),
            Err(CanonicalError::FloatNotAllowed { .. })
        ));
    }

    #[test]
    fn float_rejection_names_the_offending_path() {
        let v = parse(r#"{"outer":{"inner":[0,2.5]}}"#);
        let Err(CanonicalError::FloatNotAllowed { path }) = canonicalise(&v) else {
            panic!("expected a float rejection");
        };
        assert_eq!(path, "$.outer.inner[1]");
    }

    #[test]
    fn large_integers_survive_exactly() {
        // u64::MAX is the range proof's upper bound. If canonicalisation
        // routed integers through f64 this would lose precision and two
        // distinct values would hash the same.
        let v = parse(r#"{"value":18446744073709551615}"#);
        assert_eq!(
            canonicalise(&v).unwrap(),
            r#"{"value":18446744073709551615}"#
        );
    }

    #[test]
    fn negative_integers_are_preserved() {
        let v = parse(r#"{"a":-9223372036854775808}"#);
        assert_eq!(canonicalise(&v).unwrap(), r#"{"a":-9223372036854775808}"#);
    }

    #[test]
    fn control_characters_are_escaped_consistently() {
        let v = json!({"a": "line\nbreak\ttab"});
        assert_eq!(canonicalise(&v).unwrap(), r#"{"a":"line\nbreak\ttab"}"#);
    }

    #[test]
    fn quotes_and_backslashes_round_trip() {
        let v = json!({"a": "he said \"hi\" \\ bye"});
        let canonical = canonicalise(&v).unwrap();
        assert_eq!(
            parse(&canonical),
            v,
            "canonical form must still be valid JSON"
        );
    }

    #[test]
    fn non_ascii_is_not_escaped() {
        let v = json!({"a": "café ☕"});
        assert_eq!(canonicalise(&v).unwrap(), "{\"a\":\"café ☕\"}");
    }

    #[test]
    fn excessive_nesting_is_rejected() {
        let mut v = json!(1);
        for _ in 0..(MAX_DEPTH + 2) {
            v = serde_json::Value::Array(vec![v]);
        }
        assert!(matches!(
            canonicalise(&v),
            Err(CanonicalError::TooDeep { .. })
        ));
    }

    #[test]
    fn nesting_at_the_limit_is_accepted() {
        let mut v = json!(1);
        for _ in 0..(MAX_DEPTH - 1) {
            v = serde_json::Value::Array(vec![v]);
        }
        assert!(canonicalise(&v).is_ok());
    }

    // -----------------------------------------------------------------------
    // Job identity
    // -----------------------------------------------------------------------

    #[test]
    fn reordered_keys_produce_the_same_job_hash() {
        let a = parse(r#"{"secret":"42","leaf_index":"5"}"#);
        let b = parse(r#"{"leaf_index":"5","secret":"42"}"#);
        assert_eq!(
            job_hash("membership", &a).unwrap(),
            job_hash("membership", &b).unwrap()
        );
    }

    #[test]
    fn the_same_inputs_to_different_circuits_differ() {
        let inputs = parse(r#"{"a":"1"}"#);
        assert_ne!(
            job_hash("membership", &inputs).unwrap(),
            job_hash("range_proof", &inputs).unwrap()
        );
    }

    /// The separator byte matters. Without it, `("ab", "c")` and `("a", "bc")`
    /// would hash identically and one job would be deduplicated into the other.
    #[test]
    fn the_circuit_separator_prevents_boundary_collisions() {
        let long = job_hash("ab", &json!("c")).unwrap();
        let short = job_hash("a", &json!("bc")).unwrap();
        assert_ne!(long, short);
    }

    #[test]
    fn the_hash_is_stable_across_runs() {
        // A golden value. If this changes, every existing job identity changes
        // with it, so the deduplication index would need rebuilding — which
        // should be a deliberate migration, not an accident.
        //
        // Verified against an independent implementation:
        //   python3 -c "import hashlib; print(hashlib.sha256(
        //       b'membership' + b'\x00' +
        //       b'{\"leaf_index\":\"5\",\"secret\":\"42\"}').hexdigest())"
        let inputs = parse(r#"{"leaf_index":"5","secret":"42"}"#);
        assert_eq!(
            job_hash_hex("membership", &inputs).unwrap(),
            "114d8b1efdce00b876608b1811bf4df3ce237af784b59d22f820dc905f7c4cc0"
        );
    }
}
