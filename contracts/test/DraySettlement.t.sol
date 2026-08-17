// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {DraySettlement, IVerifier} from "../src/DraySettlement.sol";

// Both generated verifiers declare a contract named `HonkVerifier`. The
// generated files are left byte-for-byte as `bb` emitted them — vendored, not
// edited — so the collision is resolved with import aliases here rather than by
// renaming the artefacts.
import {HonkVerifier as MembershipVerifier} from "../src/verifiers/membership.sol";
import {HonkVerifier as RangeProofVerifier} from "../src/verifiers/range_proof.sol";

/// @notice Exercises the full cryptographic path against real proofs.
///
/// @dev The fixtures are the actual output of `bb prove`, read from
/// `circuits/target/`, not hand-written vectors. A test suite that verified
/// synthetic proofs would prove nothing about whether Noir, Barretenberg, and
/// the generated Solidity verifier agree — which is the single most important
/// thing Phase 1 has to establish.
///
/// Run `make prove` first if the fixtures are absent.
contract DraySettlementTest is Test {
    DraySettlement internal settlement;
    address internal membershipVerifier;
    address internal rangeProofVerifier;

    address internal constant OWNER = address(0xA11CE);
    address internal constant RELAYER = address(0xB0B);
    address internal constant STRANGER = address(0xBAD);

    bytes32 internal constant MEMBERSHIP = keccak256("dray.circuit.membership");
    bytes32 internal constant RANGE_PROOF = keccak256("dray.circuit.range_proof");

    // Loaded from disk in setUp.
    bytes internal membershipProof;
    bytes32[] internal membershipInputs;
    bytes internal rangeProofProof;
    bytes32[] internal rangeProofInputs;

    /// @dev The nullifier is the last public input, whatever the circuit's
    /// shape (ADR-008). Reading it through this helper rather than a literal
    /// index is what lets the same assertions cover both circuits, which have
    /// different numbers of public inputs.
    function _nullifierOf(bytes32[] memory publicInputs) internal pure returns (bytes32) {
        return publicInputs[publicInputs.length - 1];
    }

    function setUp() public {
        membershipProof = _readProof("membership");
        membershipInputs = _readPublicInputs("membership");
        rangeProofProof = _readProof("range_proof");
        rangeProofInputs = _readPublicInputs("range_proof");

        membershipVerifier = address(new MembershipVerifier());
        rangeProofVerifier = address(new RangeProofVerifier());

        settlement = new DraySettlement(OWNER);
        vm.startPrank(OWNER);
        settlement.registerCircuit(MEMBERSHIP, membershipVerifier);
        settlement.registerCircuit(RANGE_PROOF, rangeProofVerifier);
        settlement.setRelayer(RELAYER, true);
        vm.stopPrank();
    }

    // -----------------------------------------------------------------------
    // The happy path, for both circuits
    // -----------------------------------------------------------------------

    function test_valid_membership_proof_settles() public {
        vm.prank(RELAYER);
        settlement.settle(MEMBERSHIP, membershipProof, membershipInputs);

        assertTrue(
            settlement.nullifierUsed(_nullifierOf(membershipInputs)), "nullifier not consumed"
        );
    }

    function test_valid_range_proof_settles() public {
        vm.prank(RELAYER);
        settlement.settle(RANGE_PROOF, rangeProofProof, rangeProofInputs);

        assertTrue(
            settlement.nullifierUsed(_nullifierOf(rangeProofInputs)), "nullifier not consumed"
        );
    }

    /// @dev The point of having two circuits: one contract, no circuit-specific
    /// code path, both settle.
    function test_both_circuits_settle_through_one_contract() public {
        vm.startPrank(RELAYER);
        settlement.settle(MEMBERSHIP, membershipProof, membershipInputs);
        settlement.settle(RANGE_PROOF, rangeProofProof, rangeProofInputs);
        vm.stopPrank();

        assertTrue(settlement.nullifierUsed(_nullifierOf(membershipInputs)));
        assertTrue(settlement.nullifierUsed(_nullifierOf(rangeProofInputs)));
    }

    function test_settlement_emits_the_event() public {
        vm.expectEmit(true, true, true, true);
        emit DraySettlement.Settled(
            MEMBERSHIP, _nullifierOf(membershipInputs), RELAYER, membershipInputs
        );

        vm.prank(RELAYER);
        settlement.settle(MEMBERSHIP, membershipProof, membershipInputs);
    }

    /// @dev Confirms the ADR-008 convention actually holds in the artefacts,
    /// rather than only in the documentation. Membership publishes
    /// (root, nullifier); the range proof publishes (min, max, nullifier).
    ///
    /// The differing lengths are the point. If both circuits published the same
    /// number of public inputs, a fixed index would work by accident and this
    /// suite would not notice when it stopped working.
    function test_public_input_layout_matches_adr_008() public view {
        assertEq(membershipInputs.length, 2, "membership should publish 2 inputs");
        assertEq(rangeProofInputs.length, 3, "range_proof should publish 3 inputs");

        // min = 18, max = 150 from the committed Prover.toml, declared before
        // the returned nullifier.
        assertEq(uint256(rangeProofInputs[0]), 18, "min is not public input 0");
        assertEq(uint256(rangeProofInputs[1]), 150, "max is not public input 1");

        // Distinct domain separators must give distinct nullifiers, or the
        // shared nullifier set would let one circuit block the other.
        assertTrue(
            _nullifierOf(membershipInputs) != _nullifierOf(rangeProofInputs),
            "nullifiers collide across circuits"
        );
    }

    // -----------------------------------------------------------------------
    // Replay resistance
    // -----------------------------------------------------------------------

    function test_replayed_nullifier_reverts() public {
        vm.startPrank(RELAYER);
        settlement.settle(MEMBERSHIP, membershipProof, membershipInputs);

        vm.expectRevert(
            abi.encodeWithSelector(
                DraySettlement.NullifierAlreadyUsed.selector, _nullifierOf(membershipInputs)
            )
        );
        settlement.settle(MEMBERSHIP, membershipProof, membershipInputs);
        vm.stopPrank();
    }

    /// @dev At-least-once delivery means this is the normal case, not an attack:
    /// the relayer submits, the RPC times out, the transaction actually landed,
    /// and the relayer retries. Exactly one settlement must result.
    function test_duplicate_submission_settles_exactly_once() public {
        vm.startPrank(RELAYER);
        settlement.settle(MEMBERSHIP, membershipProof, membershipInputs);

        vm.recordLogs();
        try settlement.settle(MEMBERSHIP, membershipProof, membershipInputs) {
            fail();
        } catch {}
        assertEq(vm.getRecordedLogs().length, 0, "second submission emitted an event");
        vm.stopPrank();
    }

    // -----------------------------------------------------------------------
    // Invalid proofs
    // -----------------------------------------------------------------------

    function test_tampered_proof_does_not_settle() public {
        bytes memory tampered = membershipProof;
        tampered[64] = bytes1(uint8(tampered[64]) ^ 0x01);

        vm.prank(RELAYER);
        vm.expectRevert();
        settlement.settle(MEMBERSHIP, tampered, membershipInputs);
    }

    function test_tampered_public_input_does_not_settle() public {
        bytes32[] memory tampered = membershipInputs;
        tampered[1] = bytes32(uint256(tampered[1]) + 1); // a different Merkle root

        vm.prank(RELAYER);
        vm.expectRevert();
        settlement.settle(MEMBERSHIP, membershipProof, tampered);
    }

    /// @dev A valid proof presented to the wrong circuit's verifier.
    function test_proof_from_another_circuit_does_not_settle() public {
        vm.prank(RELAYER);
        vm.expectRevert();
        settlement.settle(MEMBERSHIP, rangeProofProof, rangeProofInputs);
    }

    function test_malformed_calldata_does_not_settle() public {
        vm.startPrank(RELAYER);

        vm.expectRevert();
        settlement.settle(MEMBERSHIP, hex"", membershipInputs);

        vm.expectRevert();
        settlement.settle(MEMBERSHIP, hex"deadbeef", membershipInputs);

        bytes memory truncated = new bytes(membershipProof.length / 2);
        for (uint256 i = 0; i < truncated.length; i++) {
            truncated[i] = membershipProof[i];
        }
        vm.expectRevert();
        settlement.settle(MEMBERSHIP, truncated, membershipInputs);

        vm.stopPrank();
    }

    function test_empty_public_inputs_revert() public {
        vm.prank(RELAYER);
        vm.expectRevert(DraySettlement.MissingPublicInputs.selector);
        settlement.settle(MEMBERSHIP, membershipProof, new bytes32[](0));
    }

    function test_wrong_number_of_public_inputs_reverts() public {
        bytes32[] memory tooMany = new bytes32[](3);
        tooMany[0] = membershipInputs[0];
        tooMany[1] = membershipInputs[1];
        // A third entry also moves where the nullifier is read from, so this
        // covers the length check and the convention at once.
        tooMany[2] = bytes32(uint256(1));

        vm.prank(RELAYER);
        vm.expectRevert();
        settlement.settle(MEMBERSHIP, membershipProof, tooMany);
    }

    // -----------------------------------------------------------------------
    // Authorisation
    // -----------------------------------------------------------------------

    function test_unauthorised_sender_cannot_settle() public {
        vm.prank(STRANGER);
        vm.expectRevert(DraySettlement.NotAuthorisedRelayer.selector);
        settlement.settle(MEMBERSHIP, membershipProof, membershipInputs);
    }

    function test_revoked_relayer_cannot_settle() public {
        vm.prank(OWNER);
        settlement.setRelayer(RELAYER, false);

        vm.prank(RELAYER);
        vm.expectRevert(DraySettlement.NotAuthorisedRelayer.selector);
        settlement.settle(MEMBERSHIP, membershipProof, membershipInputs);
    }

    function test_unknown_circuit_reverts() public {
        bytes32 unknown = keccak256("dray.circuit.nope");

        vm.prank(RELAYER);
        vm.expectRevert(abi.encodeWithSelector(DraySettlement.UnknownCircuit.selector, unknown));
        settlement.settle(unknown, membershipProof, membershipInputs);
    }

    function test_only_owner_administers() public {
        vm.startPrank(STRANGER);

        vm.expectRevert(DraySettlement.NotOwner.selector);
        settlement.registerCircuit(keccak256("x"), membershipVerifier);

        vm.expectRevert(DraySettlement.NotOwner.selector);
        settlement.setRelayer(STRANGER, true);

        vm.expectRevert(DraySettlement.NotOwner.selector);
        settlement.transferOwnership(STRANGER);

        vm.stopPrank();
    }

    function test_circuit_registration_is_not_an_upsert() public {
        vm.prank(OWNER);
        vm.expectRevert(
            abi.encodeWithSelector(DraySettlement.CircuitAlreadyRegistered.selector, MEMBERSHIP)
        );
        settlement.registerCircuit(MEMBERSHIP, rangeProofVerifier);
    }

    function test_deregistered_circuit_stops_settling() public {
        vm.prank(OWNER);
        settlement.deregisterCircuit(MEMBERSHIP);

        vm.prank(RELAYER);
        vm.expectRevert(abi.encodeWithSelector(DraySettlement.UnknownCircuit.selector, MEMBERSHIP));
        settlement.settle(MEMBERSHIP, membershipProof, membershipInputs);
    }

    // -----------------------------------------------------------------------
    // Pre-flight
    // -----------------------------------------------------------------------

    function test_wouldSettle_reports_validity_without_consuming() public {
        assertTrue(settlement.wouldSettle(MEMBERSHIP, membershipProof, membershipInputs));
        assertFalse(
            settlement.nullifierUsed(_nullifierOf(membershipInputs)),
            "pre-flight consumed a nullifier"
        );

        vm.prank(RELAYER);
        settlement.settle(MEMBERSHIP, membershipProof, membershipInputs);

        // Now that it is consumed, pre-flight must say so — this is how the
        // relayer distinguishes "already landed" from "genuinely broken".
        assertFalse(settlement.wouldSettle(MEMBERSHIP, membershipProof, membershipInputs));
    }

    function test_wouldSettle_is_false_for_unknown_circuit() public view {
        assertFalse(
            settlement.wouldSettle(
                keccak256("dray.circuit.nope"), membershipProof, membershipInputs
            )
        );
    }

    // -----------------------------------------------------------------------
    // Fuzz — over the public input space
    // -----------------------------------------------------------------------

    /// @dev A proof is bound to its exact public inputs. Substituting any other
    /// root must fail, or membership in one tree would prove membership in
    /// another.
    function testFuzz_arbitrary_root_does_not_verify(bytes32 root) public {
        vm.assume(root != membershipInputs[0]);

        bytes32[] memory inputs = new bytes32[](2);
        inputs[0] = root;
        inputs[1] = membershipInputs[1];

        assertFalse(settlement.wouldSettle(MEMBERSHIP, membershipProof, inputs));
    }

    /// @dev The nullifier is a public input, so it is covered by the proof.
    /// If an attacker could swap in a fresh nullifier, the replay guard would be
    /// worthless — they would simply mint a new one per submission. That the
    /// circuit now derives it rather than accepting it does not change this:
    /// the derived value is still committed to by the proof.
    function testFuzz_arbitrary_nullifier_does_not_verify(bytes32 nullifier) public {
        vm.assume(nullifier != membershipInputs[1]);

        bytes32[] memory inputs = new bytes32[](2);
        inputs[0] = membershipInputs[0];
        inputs[1] = nullifier;

        assertFalse(settlement.wouldSettle(MEMBERSHIP, membershipProof, inputs));
    }

    /// @dev The range bounds are public, so a proof for [18,150] must not be
    /// reusable to claim membership of a range the prover never proved.
    function testFuzz_arbitrary_range_does_not_verify(uint64 min, uint64 max) public {
        vm.assume(min != 18 || max != 150);

        bytes32[] memory inputs = new bytes32[](3);
        inputs[0] = bytes32(uint256(min));
        inputs[1] = bytes32(uint256(max));
        inputs[2] = rangeProofInputs[2];

        assertFalse(settlement.wouldSettle(RANGE_PROOF, rangeProofProof, inputs));
    }

    /// @dev No sender other than an authorised relayer may settle, whoever they are.
    function testFuzz_only_relayers_settle(address sender) public {
        vm.assume(sender != RELAYER);

        vm.prank(sender);
        vm.expectRevert(DraySettlement.NotAuthorisedRelayer.selector);
        settlement.settle(MEMBERSHIP, membershipProof, membershipInputs);
    }

    // -----------------------------------------------------------------------
    // Fixture loading
    // -----------------------------------------------------------------------

    function _readProof(string memory circuit) internal view returns (bytes memory) {
        return vm.readFileBinary(string.concat("../circuits/target/", circuit, "/proof"));
    }

    /// @dev `bb` writes public inputs as raw concatenated 32-byte field elements.
    function _readPublicInputs(string memory circuit) internal view returns (bytes32[] memory) {
        bytes memory raw =
            vm.readFileBinary(string.concat("../circuits/target/", circuit, "/public_inputs"));
        require(raw.length % 32 == 0, "public_inputs is not a whole number of field elements");

        bytes32[] memory inputs = new bytes32[](raw.length / 32);
        for (uint256 i = 0; i < inputs.length; i++) {
            bytes32 word;
            for (uint256 j = 0; j < 32; j++) {
                word = bytes32((uint256(word) << 8) | uint256(uint8(raw[i * 32 + j])));
            }
            inputs[i] = word;
        }
        return inputs;
    }
}
