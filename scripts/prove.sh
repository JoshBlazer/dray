#!/usr/bin/env bash
#
# Generates a proof and a Solidity verifier for every circuit, and records how
# long each step took and how much memory it used.
#
# The measurements are not incidental. Proving cost is what determines how many
# workers Dray needs for a given request rate, and the memory ceiling the worker
# enforces has to be derived from a real number rather than guessed. Those
# figures belong in docs/BENCHMARKS.md.
#
# Usage: make prove

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CIRCUITS_DIR="$REPO_ROOT/circuits"
VERIFIER_DIR="$REPO_ROOT/contracts/src/verifiers"

CIRCUITS=(membership range_proof)

for tool in nargo bb; do
    command -v "$tool" >/dev/null || { echo "$tool not found. Run: make setup-zk" >&2; exit 1; }
done

mkdir -p "$VERIFIER_DIR"
cd "$CIRCUITS_DIR"
nargo compile

printf '\n%-14s %10s %10s %10s %10s %8s\n' \
    CIRCUIT VK_WALL VK_PEAK PROVE_WALL PROVE_PEAK PROOF_B

for circuit in "${CIRCUITS[@]}"; do
    out="$CIRCUITS_DIR/target/$circuit"
    mkdir -p "$out"
    ( cd "$CIRCUITS_DIR/$circuit" && nargo execute >/dev/null )

    vk_stats=$(/usr/bin/time -f "%e %M" bb write_vk -t evm \
        -b "$CIRCUITS_DIR/target/$circuit.json" -o "$out" 2>&1 >/dev/null | tail -1)

    prove_stats=$(/usr/bin/time -f "%e %M" bb prove -t evm \
        -b "$CIRCUITS_DIR/target/$circuit.json" \
        -w "$CIRCUITS_DIR/target/$circuit.gz" \
        -k "$out/vk" -o "$out" --verify 2>&1 >/dev/null | tail -1)

    bb write_solidity_verifier -t evm -k "$out/vk" -o "$VERIFIER_DIR/$circuit.sol" >/dev/null 2>&1

    printf '%-14s %9ss %9sMB %9ss %9sMB %8s\n' \
        "$circuit" \
        "$(cut -d' ' -f1 <<<"$vk_stats")" \
        "$(( $(cut -d' ' -f2 <<<"$vk_stats") / 1024 ))" \
        "$(cut -d' ' -f1 <<<"$prove_stats")" \
        "$(( $(cut -d' ' -f2 <<<"$prove_stats") / 1024 ))" \
        "$(stat -c%s "$out/proof")"
done

printf '\nMeasured on: %s cores, %s RAM\n' \
    "$(nproc)" "$(free -h | awk '/^Mem:/{print $2}')"
