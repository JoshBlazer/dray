// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Script, console} from "forge-std/Script.sol";
import {DraySettlement} from "../src/DraySettlement.sol";

/// @notice Settles a real proof against a deployed `DraySettlement`.
///
/// @dev This is the on-chain half of `make e2e-circuits`. It reads the proof
/// and public inputs that `bb prove` wrote to disk and submits them as an
/// actual transaction — no mocks, no synthetic vectors.
///
/// It then submits the *same* proof a second time and asserts that the
/// transaction reverts. Replay resistance is a property of the deployed system,
/// not of a unit test, so it is worth proving here too.
///
/// Environment:
///   PRIVATE_KEY       signer, must be an authorised relayer
///   DRAY_SETTLEMENT   deployed settlement address
///   DRAY_CIRCUIT      "membership" or "range_proof"
contract SettleProof is Script {
    function run() external {
        uint256 relayerKey = vm.envUint("PRIVATE_KEY");
        DraySettlement settlement = DraySettlement(vm.envAddress("DRAY_SETTLEMENT"));
        string memory circuit = vm.envString("DRAY_CIRCUIT");

        bytes32 circuitId = keccak256(abi.encodePacked("dray.circuit.", circuit));
        bytes memory proof = vm.readFileBinary(
            string.concat(vm.projectRoot(), "/../circuits/target/", circuit, "/proof")
        );
        bytes32[] memory publicInputs = _readPublicInputs(circuit);

        console.log("circuit:    %s", circuit);
        console.log("proof:      %s bytes", proof.length);
        console.log("nullifier:  %s", vm.toString(publicInputs[publicInputs.length - 1]));

        require(
            settlement.wouldSettle(circuitId, proof, publicInputs),
            "pre-flight says this proof will not settle"
        );

        vm.broadcast(relayerKey);
        settlement.settle(circuitId, proof, publicInputs);

        require(
            settlement.nullifierUsed(publicInputs[publicInputs.length - 1]),
            "nullifier was not consumed"
        );
        console.log("settled, nullifier consumed");

        // Replay must now be impossible.
        require(
            !settlement.wouldSettle(circuitId, proof, publicInputs),
            "pre-flight still reports the spent proof as settleable"
        );

        vm.prank(vm.addr(relayerKey));
        try settlement.settle(circuitId, proof, publicInputs) {
            revert("replay succeeded, nullifier set is not working");
        } catch {
            console.log("replay correctly rejected");
        }
    }

    function _readPublicInputs(string memory circuit) internal view returns (bytes32[] memory) {
        bytes memory raw = vm.readFileBinary(
            string.concat(vm.projectRoot(), "/../circuits/target/", circuit, "/public_inputs")
        );
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
