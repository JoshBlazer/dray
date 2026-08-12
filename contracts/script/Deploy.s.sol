// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Script, console} from "forge-std/Script.sol";
import {DraySettlement} from "../src/DraySettlement.sol";
import {HonkVerifier as MembershipVerifier} from "../src/verifiers/membership.sol";
import {HonkVerifier as RangeProofVerifier} from "../src/verifiers/range_proof.sol";

/// @notice Deploys the Dray settlement stack and registers both circuits.
///
/// @dev Used for local Anvil in Phase 1 and, unchanged, for the public testnet
/// in Phase 4 — the only difference is the RPC URL and the signing key.
///
/// The deployer becomes the owner and is authorised as a relayer, which is
/// convenient locally. On a real network the relayer should be a separate key,
/// set with `setRelayer` after deployment; the owner key can then stay cold.
contract Deploy is Script {
    bytes32 internal constant MEMBERSHIP = keccak256("dray.circuit.membership");
    bytes32 internal constant RANGE_PROOF = keccak256("dray.circuit.range_proof");

    function run() external {
        uint256 deployerKey = vm.envUint("PRIVATE_KEY");
        address deployer = vm.addr(deployerKey);

        vm.startBroadcast(deployerKey);

        address membershipVerifier = address(new MembershipVerifier());
        address rangeProofVerifier = address(new RangeProofVerifier());

        DraySettlement settlement = new DraySettlement(deployer);
        settlement.registerCircuit(MEMBERSHIP, membershipVerifier);
        settlement.registerCircuit(RANGE_PROOF, rangeProofVerifier);
        settlement.setRelayer(deployer, true);

        vm.stopBroadcast();

        console.log("DRAY_SETTLEMENT=%s", address(settlement));
        console.log("DRAY_MEMBERSHIP_VERIFIER=%s", membershipVerifier);
        console.log("DRAY_RANGE_PROOF_VERIFIER=%s", rangeProofVerifier);
        console.log("DRAY_OWNER=%s", deployer);
    }
}
