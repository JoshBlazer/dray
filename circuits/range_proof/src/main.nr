//! Range proof.
//!
//! Proves that a private value lies within a public inclusive range, without
//! revealing the value. The obvious application is an age or balance threshold:
//! prove you are between 18 and 150, or that a balance covers a payment,
//! without disclosing the number itself.
//!
//! Public inputs, in ABI order:
//!   0. `min`       — inclusive lower bound
//!   1. `max`       — inclusive upper bound
//!   2. `nullifier` — returned by the circuit, binds this proof to one
//!                    (secret, value) pair
//!
//! This circuit exists alongside `membership` to force the system to be
//! genuinely circuit-agnostic. It shares only one thing with it: the
//! convention that **the last public input is the nullifier**, which is what
//! lets `DraySettlement.sol` handle both without knowing either. Note that the
//! two circuits have different numbers of public inputs, so "last" is the only
//! position that can be shared. See ADR-008.
//!
//! `u64` rather than `Field` for the bounds and the value is deliberate.
//! Comparison on a `Field` is not meaningful — the field wraps, so "less than"
//! has no consistent ordering — whereas `u64` comparison is a genuine range
//! constraint. It also bounds the value to 64 bits for free.

/// Domain separator — the ASCII bytes of "dray_range_nullifier". Keeps a
/// secret reused across Dray circuits from deriving the same nullifier twice.
pub global NULLIFIER_DOMAIN: Field = 0x647261795f72616e67655f6e756c6c6966696572;

fn main(min: pub u64, max: pub u64, value: u64, secret: Field) -> pub Field {
    // An inverted range would otherwise be vacuously unsatisfiable in a way
    // that looks like an ordinary proof failure. Fail on the cause, not the
    // symptom.
    assert(min <= max, "inverted range");

    assert(value >= min, "value below minimum");
    assert(value <= max, "value above maximum");

    derive_nullifier(secret, value)
}

/// Binds the nullifier to both the secret and the value.
///
/// Including the value means one secret can be used to prove distinct values
/// without the settlements colliding, while re-proving the *same* value with
/// the same secret is correctly caught as a replay.
pub fn derive_nullifier(secret: Field, value: u64) -> Field {
    std::hash::pedersen_hash([secret, value as Field, NULLIFIER_DOMAIN])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn accepts_a_value_inside_the_range() {
    let secret = 12345;
    let value = 42;
    let nullifier = main(18, 150, value, secret);
    assert(nullifier == derive_nullifier(secret, value), "returned the wrong nullifier");
}

#[test]
fn accepts_both_inclusive_boundaries() {
    let secret = 12345;

    let at_min = 18;
    let _ = main(18, 150, at_min, secret);

    let at_max = 150;
    let _ = main(18, 150, at_max, secret);
}

#[test]
fn accepts_a_single_point_range() {
    let secret = 7;
    let value = 99;
    let _ = main(99, 99, value, secret);
}

#[test]
fn accepts_the_full_u64_range() {
    let secret = 7;
    let value = 18446744073709551615;
    let _ = main(0, 18446744073709551615, value, secret);
}

#[test(should_fail_with = "value below minimum")]
fn rejects_a_value_below_the_minimum() {
    let secret = 12345;
    let value = 17;
    let _ = main(18, 150, value, secret);
}

#[test(should_fail_with = "value above maximum")]
fn rejects_a_value_above_the_maximum() {
    let secret = 12345;
    let value = 151;
    let _ = main(18, 150, value, secret);
}

#[test(should_fail_with = "inverted range")]
fn rejects_an_inverted_range() {
    let secret = 12345;
    let value = 50;
    let _ = main(150, 18, value, secret);
}

/// The nullifier is an output, so there is no incoming value to forge. What
/// has to hold instead is that it is *bound to the witness*: two different
/// in-range values under one secret must publish different nullifiers, or a
/// prover could settle twice by simply proving a neighbouring value.
#[test]
fn different_values_under_one_secret_publish_different_nullifiers() {
    let secret = 12345;

    // Both are inside [18, 150], so nothing but the binding distinguishes them.
    let for_41 = main(18, 150, 41, secret);
    let for_42 = main(18, 150, 42, secret);

    assert(for_41 != for_42, "nullifier ignores the value");
}

#[test]
fn the_published_nullifier_is_independent_of_the_declared_range() {
    let secret = 12345;
    let value = 42;

    // The range is a claim about the value, not part of its identity. If the
    // nullifier moved with the bounds, a prover could re-settle the same value
    // indefinitely just by widening the range it proves against.
    let narrow = main(40, 45, value, secret);
    let wide = main(0, 18446744073709551615, value, secret);

    assert(narrow == wide, "nullifier depends on the range");
}

/// Emits the canonical witness committed in `Prover.toml`. See the equivalent
/// test in the membership circuit for why this exists.
#[test]
fn print_reference_witness() {
    let secret = 12345;
    let value = 42;

    let nullifier = derive_nullifier(secret, value);

    println("-- range_proof reference witness --");
    println(f"nullifier = {nullifier}");
}

#[test]
fn the_nullifier_binds_to_both_secret_and_value() {
    assert(derive_nullifier(1, 42) == derive_nullifier(1, 42), "not deterministic");
    assert(derive_nullifier(1, 42) != derive_nullifier(2, 42), "ignores the secret");
    assert(derive_nullifier(1, 42) != derive_nullifier(1, 43), "ignores the value");
}
