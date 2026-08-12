//! Merkle membership.
//!
//! Proves that a leaf derived from a private secret belongs to a Merkle tree
//! with a given public root, without revealing which leaf it is.
//!
//! Public inputs, in ABI order:
//!   0. `nullifier` — derived deterministically from the secret
//!   1. `root`      — the Merkle root the leaf is claimed to belong to
//!
//! **The nullifier is public input 0 by convention, shared with every other
//! Dray circuit.** `DraySettlement.sol` reads it at that fixed index without
//! knowing which circuit produced the proof, which is what keeps the
//! settlement contract circuit-agnostic. See ADR-003.
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
    nullifier: pub Field,
    root: pub Field,
    secret: Field,
    leaf_index: Field,
    siblings: [Field; TREE_DEPTH],
) {
    let leaf = leaf_commitment(secret);
    assert(compute_merkle_root(leaf, leaf_index, siblings) == root, "merkle root mismatch");
    assert(derive_nullifier(secret) == nullifier, "nullifier mismatch");
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
pub fn compute_merkle_root(
    leaf: Field,
    leaf_index: Field,
    siblings: [Field; TREE_DEPTH],
) -> Field {
    let path: [bool; TREE_DEPTH] = leaf_index.to_le_bits();
    let mut node = leaf;
    for level in 0..TREE_DEPTH {
        let sibling = siblings[level];
        let (left, right) = if path[level] { (sibling, node) } else { (node, sibling) };
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
    main(derive_nullifier(secret), root, secret, leaf_index, siblings);
}

#[test]
fn accepts_the_leftmost_and_rightmost_leaves() {
    let secret = 1;
    let siblings = [3; TREE_DEPTH];

    let first = 0;
    let root_first = compute_merkle_root(leaf_commitment(secret), first, siblings);
    main(derive_nullifier(secret), root_first, secret, first, siblings);

    // 2^TREE_DEPTH - 1: every path bit set, so the leaf is the right child the
    // whole way up.
    let last = 1048575;
    let root_last = compute_merkle_root(leaf_commitment(secret), last, siblings);
    main(derive_nullifier(secret), root_last, secret, last, siblings);
}

#[test(should_fail_with = "merkle root mismatch")]
fn rejects_a_root_that_was_not_derived_from_the_path() {
    let secret = 42;
    let leaf_index = 5;
    let siblings = [7; TREE_DEPTH];

    let root = compute_merkle_root(leaf_commitment(secret), leaf_index, siblings);
    main(derive_nullifier(secret), root + 1, secret, leaf_index, siblings);
}

#[test(should_fail_with = "merkle root mismatch")]
fn rejects_a_tampered_sibling() {
    let secret = 42;
    let leaf_index = 5;
    let mut siblings = [7; TREE_DEPTH];

    let root = compute_merkle_root(leaf_commitment(secret), leaf_index, siblings);
    siblings[3] = 999;
    main(derive_nullifier(secret), root, secret, leaf_index, siblings);
}

#[test(should_fail_with = "merkle root mismatch")]
fn rejects_the_right_leaf_at_the_wrong_index() {
    let secret = 42;
    let siblings = [7; TREE_DEPTH];

    let root = compute_merkle_root(leaf_commitment(secret), 5, siblings);
    main(derive_nullifier(secret), root, secret, 6, siblings);
}

#[test(should_fail_with = "merkle root mismatch")]
fn rejects_a_secret_that_is_not_in_the_tree() {
    let secret = 42;
    let leaf_index = 5;
    let siblings = [7; TREE_DEPTH];

    let root = compute_merkle_root(leaf_commitment(secret), leaf_index, siblings);
    // Correct root and path, but a secret whose leaf is not the one committed.
    main(derive_nullifier(43), root, 43, leaf_index, siblings);
}

#[test(should_fail_with = "nullifier mismatch")]
fn rejects_a_nullifier_not_derived_from_the_secret() {
    let secret = 42;
    let leaf_index = 5;
    let siblings = [7; TREE_DEPTH];

    let root = compute_merkle_root(leaf_commitment(secret), leaf_index, siblings);
    main(derive_nullifier(secret) + 1, root, secret, leaf_index, siblings);
}

#[test]
fn the_nullifier_is_deterministic_and_secret_dependent() {
    assert(derive_nullifier(42) == derive_nullifier(42), "not deterministic");
    assert(derive_nullifier(42) != derive_nullifier(43), "collides across secrets");
    // Domain separation: the nullifier must not simply be the leaf commitment,
    // or publishing it would identify the leaf.
    assert(derive_nullifier(42) != leaf_commitment(42), "nullifier reveals the leaf");
}

/// Emits the canonical witness committed in `Prover.toml`.
///
/// Pedersen hashes cannot be computed by hand, so this is how those values
/// were produced. Run `nargo test --show-output print_reference_witness` to
/// regenerate them after any change to the hashing or the tree depth.
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
