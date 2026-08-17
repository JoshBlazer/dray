#!/usr/bin/env bash
#
# Phase 1 end-to-end: circuit input -> proof -> on-chain verification.
#
# Compiles both circuits, generates real proofs with Barretenberg, regenerates
# the Solidity verifiers from the resulting verification keys, starts a local
# Anvil chain, deploys the settlement stack, and settles both proofs as actual
# transactions. Then it submits one of them twice to prove the nullifier set
# rejects the replay.
#
# No service tier is involved — that is the point. This establishes that the
# cryptographic path works before any distributed machinery is built on top.
#
# Usage: make e2e-circuits

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CIRCUITS_DIR="$REPO_ROOT/circuits"
CONTRACTS_DIR="$REPO_ROOT/contracts"
VERIFIER_DIR="$CONTRACTS_DIR/src/verifiers"

# Anvil's first well-known development account. This key is public, published in
# Anvil's own documentation, and worthless. It must never be used anywhere but a
# local chain.
ANVIL_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
ANVIL_PORT="${ANVIL_PORT:-8545}"
RPC_URL="http://127.0.0.1:${ANVIL_PORT}"

CIRCUITS=(membership range_proof)

log() { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
die() { printf '\n\033[1;31mFAILED: %s\033[0m\n' "$*" >&2; exit 1; }

ANVIL_PID=""
cleanup() {
    if [[ -n "$ANVIL_PID" ]] && kill -0 "$ANVIL_PID" 2>/dev/null; then
        kill "$ANVIL_PID" 2>/dev/null || true
        wait "$ANVIL_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

for tool in nargo bb forge anvil cast; do
    command -v "$tool" >/dev/null || die "$tool not found. Run: make setup-zk"
done

# ---------------------------------------------------------------------------
log "Compiling circuits and solving witnesses"
# ---------------------------------------------------------------------------
cd "$CIRCUITS_DIR"
nargo compile
for circuit in "${CIRCUITS[@]}"; do
    ( cd "$CIRCUITS_DIR/$circuit" && nargo execute >/dev/null )
    echo "  $circuit: witness solved"
done

# ---------------------------------------------------------------------------
log "Generating proofs and Solidity verifiers"
# ---------------------------------------------------------------------------
mkdir -p "$VERIFIER_DIR"
for circuit in "${CIRCUITS[@]}"; do
    out="$CIRCUITS_DIR/target/$circuit"
    mkdir -p "$out"

    # -t evm selects the keccak transcript the Solidity verifier expects. The
    # same target must be used for the key, the proof, and the verifier, or they
    # will not agree.
    bb write_vk -t evm -b "$CIRCUITS_DIR/target/$circuit.json" -o "$out" >/dev/null
    bb prove -t evm \
        -b "$CIRCUITS_DIR/target/$circuit.json" \
        -w "$CIRCUITS_DIR/target/$circuit.gz" \
        -k "$out/vk" -o "$out" --verify >/dev/null
    bb write_solidity_verifier -t evm -k "$out/vk" -o "$VERIFIER_DIR/$circuit.sol" >/dev/null

    echo "  $circuit: proof $(stat -c%s "$out/proof") bytes, verified natively"
done

# ---------------------------------------------------------------------------
log "Running the contract test suite against those proofs"
# ---------------------------------------------------------------------------
cd "$CONTRACTS_DIR"
forge test

# ---------------------------------------------------------------------------
log "Starting Anvil on port $ANVIL_PORT"
# ---------------------------------------------------------------------------
anvil --port "$ANVIL_PORT" --silent &
ANVIL_PID=$!

for _ in $(seq 1 50); do
    if cast block-number --rpc-url "$RPC_URL" >/dev/null 2>&1; then break; fi
    sleep 0.2
done
cast block-number --rpc-url "$RPC_URL" >/dev/null 2>&1 || die "Anvil did not become ready"
echo "  Anvil ready (pid $ANVIL_PID)"

# ---------------------------------------------------------------------------
log "Deploying the settlement stack"
# ---------------------------------------------------------------------------
export PRIVATE_KEY="$ANVIL_KEY"
deploy_log="$(forge script script/Deploy.s.sol:Deploy \
    --rpc-url "$RPC_URL" --broadcast --skip-simulation 2>&1)" || {
        echo "$deploy_log"; die "deployment failed"; }

SETTLEMENT="$(grep -oE 'DRAY_SETTLEMENT=0x[0-9a-fA-F]{40}' <<<"$deploy_log" | head -1 | cut -d= -f2)"
[[ -n "$SETTLEMENT" ]] || { echo "$deploy_log"; die "could not determine settlement address"; }
echo "  DraySettlement deployed at $SETTLEMENT"

# ---------------------------------------------------------------------------
log "Settling both proofs on chain"
# ---------------------------------------------------------------------------
export DRAY_SETTLEMENT="$SETTLEMENT"
for circuit in "${CIRCUITS[@]}"; do
    DRAY_CIRCUIT="$circuit" forge script script/SettleProof.s.sol:SettleProof \
        --rpc-url "$RPC_URL" --broadcast --skip-simulation \
        | grep -E 'circuit:|proof:|nullifier:|settled|replay' || die "$circuit failed to settle"
done

# ---------------------------------------------------------------------------
log "Confirming settlement state on chain"
# ---------------------------------------------------------------------------
for circuit in "${CIRCUITS[@]}"; do
    # The nullifier is the *last* public input, not the first (ADR-008), and
    # the circuits publish different numbers of them, so take it from the end.
    nullifier="0x$(tail -c 32 "$CIRCUITS_DIR/target/$circuit/public_inputs" | xxd -p -c 32)"
    used="$(cast call "$SETTLEMENT" "nullifierUsed(bytes32)(bool)" "$nullifier" --rpc-url "$RPC_URL")"
    [[ "$used" == "true" ]] || die "$circuit nullifier $nullifier is not marked consumed on chain"
    echo "  $circuit: nullifier consumed on chain"
done

printf '\n\033[1;32m==> e2e-circuits passed: both circuits proved and verified on chain\033[0m\n\n'
