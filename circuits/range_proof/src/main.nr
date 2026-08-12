//! Range proof.
//!
//! Proves that a private value lies within a public inclusive range, without
//! revealing the value. The obvious application is an age or balance threshold:
//! prove you are between 18 and 150, or that a balance covers a payment,
//! without disclosing the number itself.
//!
//! Public inputs, in ABI order:
//!   0. `nullifier` — binds this proof to one (secret, value) pair
//!   1. `min`       — inclusive lower bound
//!   2. `max`       — inclusive upper bound
//!
//! This circuit exists alongside `membership` to force the system to be
//! genuinely circuit-agnostic. It shares only one thing with it: the
//! convention that **public input 0 is the nullifier**, which is what lets
//! `DraySettlement.sol` handle both without knowing either. See ADR-003.
//!
//! `u64` rather than `Field` for the bounds and the value is deliberate.
//! Comparison on a `Field` is not meaningful — the field wraps, so "less than"
//! has no consistent ordering — whereas `u64` comparison is a genuine range
//! constraint. It also bounds the value to 64 bits for free.

/// Domain separator — the ASCII bytes of "dray_range_nullifier". Keeps a
/// secret reused across Dray circuits from deriving the same nullifier twice.
pub global NULLIFIER_DOMAIN: Field = 0x647261795f72616e67655f6e756c6c6966696572;

fn main(nullifier: pub Field, min: pub u64, max: pub u64, value: u64, secret: Field) {
    // An inverted range would otherwise be vacuously unsatisfiable in a way
    // that looks like an ordinary proof failure. Fail on the cause, not the
    // symptom.
    assert(min <= max, "inverted range");

    assert(value >= min, "value below minimum");
    assert(value <= max, "value above maximum");

    assert(derive_nullifier(secret, value) == nullifier, "nullifier mismatch");
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
    main(derive_nullifier(secret, value), 18, 150, value, secret);
}

#[test]
fn accepts_both_inclusive_boundaries() {
    let secret = 12345;

    let at_min = 18;
    main(derive_nullifier(secret, at_min), 18, 150, at_min, secret);

    let at_max = 150;
    main(derive_nullifier(secret, at_max), 18, 150, at_max, secret);
}

#[test]
fn accepts_a_single_point_range() {
    let secret = 7;
    let value = 99;
    main(derive_nullifier(secret, value), 99, 99, value, secret);
}

#[test]
fn accepts_the_full_u64_range() {
    let secret = 7;
    let value = 18446744073709551615;
    main(derive_nullifier(secret, value), 0, 18446744073709551615, value, secret);
}

#[test(should_fail_with = "value below minimum")]
fn rejects_a_value_below_the_minimum() {
    let secret = 12345;
    let value = 17;
    main(derive_nullifier(secret, value), 18, 150, value, secret);
}

#[test(should_fail_with = "value above maximum")]
fn rejects_a_value_above_the_maximum() {
    let secret = 12345;
    let value = 151;
    main(derive_nullifier(secret, value), 18, 150, value, secret);
}

#[test(should_fail_with = "inverted range")]
fn rejects_an_inverted_range() {
    let secret = 12345;
    let value = 50;
    main(derive_nullifier(secret, value), 150, 18, value, secret);
}

#[test(should_fail_with = "nullifier mismatch")]
fn rejects_a_nullifier_not_derived_from_the_witness() {
    let secret = 12345;
    let value = 42;
    main(derive_nullifier(secret, value) + 1, 18, 150, value, secret);
}

#[test(should_fail_with = "nullifier mismatch")]
fn rejects_a_nullifier_derived_from_a_different_value() {
    let secret = 12345;
    // Both values are in range, so only the nullifier binding catches this.
    // Without it, a prover could reuse one proof's nullifier for another value.
    main(derive_nullifier(secret, 41), 18, 150, 42, secret);
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
