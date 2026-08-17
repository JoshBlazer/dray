//! Merkle membership.
//!
//! Proves that a leaf derived from a private secret belongs to a Merkle tree
//! with a given public root, without revealing which leaf it is.
//!
//! Public inputs, in ABI order:
//!   0. `root`      — the Merkle root the leaf is claimed to belong to
//!   1. `nullifier` — returned by the circuit, derived from the secret
//!
//! **The nullifier is the last public input by convention, shared with every
//! other Dray circuit.** `DraySettlement.sol` reads it from that position
//! without knowing which circuit produced the proof, which is what keeps the
//! settlement contract circuit-agnostic. See ADR-008.
//!
//! It is *returned* rather than passed in because it is a Pedersen hash of the
//! secret. A caller supplying it would have to compute that hash themselves,
//! which means running the very proving stack Dray exists to run for them.
//! Returning it means the circuit derives it and the ABI publishes it.
//!
//! Because the nullifier is a deterministic function of the secret, proving
//! membership twice with the same secret yields the same nullifier, and the
//! settlement contract rejects the second one. It reveals nothing about the
//! leaf or the secret to anyone who does not already know the secret.

/// Depth of the Merkle tree: 2^20 = 1,048,576 leaves.
///
/// Changing this changes the circuit, and so changes the verification key and
/// the generated Solidity verifier. It is not a runtime parameter.
pub global TREE_DEPTH: u32 = 20;

/// Domain separator for nullifier derivation — the ASCII bytes of
/// "dray_membership_nullifier". Without it, a secret reused across two Dray
/// circuits would derive the same nullifier in both, and settling one would
/// block the other.
pub global NULLIFIER_DOMAIN: Field = 0x647261795f6d656d626572736869705f6e756c6c6966696572;

fn main(
    root: pub Field,
    secret: Field,
    leaf_index: Field,
    siblings: [Field; TREE_DEPTH],
) -> pub Field {
    let leaf = leaf_commitment(secret);
    assert(compute_merkle_root(leaf, leaf_index, siblings) == root, "merkle root mismatch");
    derive_nullifier(secret)
}

/// The leaf stored in the tree is a commitment to the secret, never the secret
/// itself — the tree is public, so a raw secret in a leaf would be no secret.
pub fn leaf_commitment(secret: Field) -> Field {
    std::hash::pedersen_hash([secret])
}

/// Deterministic in the secret, which is exactly what makes it useful as a
/// double-spend guard.
pub fn derive_nullifier(secret: Field) -> Field {
    std::hash::pedersen_hash([secret, NULLIFIER_DOMAIN])
}

/// Walks the authentication path from leaf to root.
///
/// `leaf_index` selects, bit by bit from the leaf upwards, whether the running
/// node is the left or the right child at each level. Decomposing it into
/// exactly `TREE_DEPTH` bits also constrains it to be less than 2^TREE_DEPTH,
/// so an out-of-range index cannot satisfy the circuit.
pub fn compute_merkle_root(leaf: Field, leaf_index: Field, siblings: [Field; TREE_DEPTH]) -> Field {
    let path: [bool; TREE_DEPTH] = leaf_index.to_le_bits();
    let mut node = leaf;
    for level in 0..TREE_DEPTH {
        let sibling = siblings[level];
        let (left, right) = if path[level] {
            (sibling, node)
        } else {
            (node, sibling)
        };
        node = std::hash::pedersen_hash([left, right]);
    }
    node
}

// ---------------------------------------------------------------------------
// Tests
//
// Valid witnesses must be accepted; every way of tampering with one must be
// rejected. The failure cases matter more than the happy path — a circuit that
// accepts everything also "passes" a happy-path test.
// ---------------------------------------------------------------------------

#[test]
fn accepts_a_valid_witness() {
    let secret = 42;
    let leaf_index = 5;
    let siblings = [7; TREE_DEPTH];

    let root = compute_merkle_root(leaf_commitment(secret), leaf_index, siblings);
    let nullifier = main(root, secret, leaf_index, siblings);
    assert(nullifier == derive_nullifier(secret), "returned the wrong nullifier");
}

#[test]
fn accepts_the_leftmost_and_rightmost_leaves() {
    let secret = 1;
    let siblings = [3; TREE_DEPTH];

    let first = 0;
    let root_first = compute_merkle_root(leaf_commitment(secret), first, siblings);
    let _ = main(root_first, secret, first, siblings);

    // 2^TREE_DEPTH - 1: every path bit set, so the leaf is the right child the
    // whole way up.
    let last = 1048575;
    let root_last = compute_merkle_root(leaf_commitment(secret), last, siblings);
    let _ = main(root_last, secret, last, siblings);
}

#[test(should_fail_with = "merkle root mismatch")]
fn rejects_a_root_that_was_not_derived_from_the_path() {
    let secret = 42;
    let leaf_index = 5;
    let siblings = [7; TREE_DEPTH];

    let root = compute_merkle_root(leaf_commitment(secret), leaf_index, siblings);
    let _ = main(root + 1, secret, leaf_index, siblings);
}

#[test(should_fail_with = "merkle root mismatch")]
fn rejects_a_tampered_sibling() {
    let secret = 42;
    let leaf_index = 5;
    let mut siblings = [7; TREE_DEPTH];

    let root = compute_merkle_root(leaf_commitment(secret), leaf_index, siblings);
    siblings[3] = 999;
    let _ = main(root, secret, leaf_index, siblings);
}

#[test(should_fail_with = "merkle root mismatch")]
fn rejects_the_right_leaf_at_the_wrong_index() {
    let secret = 42;
    let siblings = [7; TREE_DEPTH];

    let root = compute_merkle_root(leaf_commitment(secret), 5, siblings);
    let _ = main(root, secret, 6, siblings);
}

#[test(should_fail_with = "merkle root mismatch")]
fn rejects_a_secret_that_is_not_in_the_tree() {
    let secret = 42;
    let leaf_index = 5;
    let siblings = [7; TREE_DEPTH];

    let root = compute_merkle_root(leaf_commitment(secret), leaf_index, siblings);
    // Correct root and path, but a secret whose leaf is not the one committed.
    let _ = main(root, 43, leaf_index, siblings);
}

/// The nullifier is an output, so it cannot be forged by lying on the way in —
/// there is no input to lie about. What must hold instead is that two proofs
/// from the same secret publish the *same* nullifier, because that identity is
/// the whole basis of double-spend rejection at settlement.
#[test]
fn the_same_secret_publishes_the_same_nullifier_from_different_positions() {
    let secret = 42;
    let siblings = [7; TREE_DEPTH];

    let root_here = compute_merkle_root(leaf_commitment(secret), 5, siblings);
    let root_there = compute_merkle_root(leaf_commitment(secret), 9, siblings);

    let from_here = main(root_here, secret, 5, siblings);
    let from_there = main(root_there, secret, 9, siblings);

    assert(from_here == from_there, "nullifier depends on position");
}

#[test]
fn the_nullifier_is_deterministic_and_secret_dependent() {
    assert(derive_nullifier(42) == derive_nullifier(42), "not deterministic");
    assert(derive_nullifier(42) != derive_nullifier(43), "collides across secrets");
    // Domain separation: the nullifier must not simply be the leaf commitment,
    // or publishing it would identify the leaf.
    assert(derive_nullifier(42) != leaf_commitment(42), "nullifier reveals the leaf");
}

/// Emits the canonical witness committed in `Prover.toml`, plus the nullifier
/// that witness should produce.
///
/// Pedersen hashes cannot be computed by hand, so this is how `root` was
/// produced. The nullifier is no longer an input, but it is printed anyway
/// because the settlement tests need to know what value to expect on chain.
/// Run `nargo test --show-output print_reference_witness` to regenerate both
/// after any change to the hashing or the tree depth.
#[test]
fn print_reference_witness() {
    let secret = 42;
    let leaf_index = 5;
    let siblings = [7; TREE_DEPTH];

    let nullifier = derive_nullifier(secret);
    let root = compute_merkle_root(leaf_commitment(secret), leaf_index, siblings);

    println("-- membership reference witness --");
    println(f"nullifier = {nullifier}");
    println(f"root      = {root}");
}

#[test]
fn sibling_order_is_load_bearing() {
    // Index 0 places the leaf on the left, index 1 on the right. If the circuit
    // ignored the path bits, these would collide and membership could be forged
    // for a leaf that is not actually at the claimed position.
    let leaf = leaf_commitment(42);
    let siblings = [7; TREE_DEPTH];
    assert(
        compute_merkle_root(leaf, 0, siblings) != compute_merkle_root(leaf, 1, siblings),
        "path bits are not constraining the hash order",
    );
}
