#!/usr/bin/env bash
#
# The whole system, end to end: an HTTP request becomes a settled proof on
# chain.
#
# Starts Anvil, deploys the settlement stack, runs the API, a worker, and a
# relayer against the local Postgres, submits one proof request over HTTP, and
# waits for it to be verified on chain. Then it checks the nullifier directly
# with `cast`, so the final assertion comes from the chain rather than from
# Dray's own record of it.
#
# This is the claim the README makes, executed. If it passes, a stranger who
# ran `make up && make setup-zk && make e2e` has watched a proof request become
# an on-chain settlement.
#
# Usage: make e2e

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# Anvil's first two well-known development accounts. Published in its own
# documentation and worthless; never usable anywhere but a local chain.
OWNER_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
RELAYER_KEY="0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"
RELAYER_ADDRESS="0x70997970C51812dc3A010C7d01b50e0d17dc79C8"

ANVIL_PORT="${ANVIL_PORT:-8545}"
RPC_URL="http://127.0.0.1:${ANVIL_PORT}"
API_PORT="${API_PORT:-8080}"
API_URL="http://127.0.0.1:${API_PORT}"

DATABASE_URL="${DATABASE_URL:-postgres://dray:dray@localhost:5432/dray_e2e}"
ADMIN_URL="${ADMIN_URL:-postgres://dray:dray@localhost:5432/dray}"
REDIS_URL="${REDIS_URL:-redis://localhost:6379}"

SCRATCH="$(mktemp -d)"
LOGS="$SCRATCH/logs"; mkdir -p "$LOGS"

log()  { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
note() { printf '    %s\n' "$*"; }
die()  { printf '\n\033[1;31mFAILED: %s\033[0m\n' "$*" >&2; dump_logs; exit 1; }

dump_logs() {
    for f in "$LOGS"/*.log; do
        [[ -f "$f" ]] || continue
        printf '\n----- %s (last 30 lines) -----\n' "$(basename "$f")" >&2
        tail -30 "$f" >&2
    done
}

PIDS=()
cleanup() {
    for pid in "${PIDS[@]:-}"; do
        [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
    rm -rf "$SCRATCH"
}
trap cleanup EXIT INT TERM

for tool in nargo bb forge anvil cast cargo curl; do
    command -v "$tool" >/dev/null || die "$tool not found. Run: make setup && make setup-zk"
done

# Database work goes through the `dray` CLI rather than psql, so this needs only
# Docker and Rust — which is what the README promises. Requiring a Postgres
# client package would quietly make that untrue.
DRAY="./target/debug/dray"

# ---------------------------------------------------------------------------
log "Building the services"
# ---------------------------------------------------------------------------
cargo build --quiet -p dray-api -p dray-worker -p dray-relayer -p dray-cli
note "dray-api, dray-worker, dray-relayer, dray built"

# ---------------------------------------------------------------------------
log "Preparing a clean database"
# ---------------------------------------------------------------------------
# A dedicated database, so the run is reproducible and does not inherit
# whatever a previous session left behind.
DATABASE_URL="$ADMIN_URL" "$DRAY" reset dray_e2e
DATABASE_URL="$DATABASE_URL" "$DRAY" migrate
DATABASE_URL="$DATABASE_URL" "$DRAY" exec scripts/seed-circuits.sql
note "migrations applied and circuits registered"

# ---------------------------------------------------------------------------
log "Starting Anvil on port $ANVIL_PORT"
# ---------------------------------------------------------------------------
anvil --port "$ANVIL_PORT" --silent > "$LOGS/anvil.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 50); do
    cast block-number --rpc-url "$RPC_URL" >/dev/null 2>&1 && break
    sleep 0.2
done
cast block-number --rpc-url "$RPC_URL" >/dev/null 2>&1 || die "Anvil did not become ready"
note "chain id $(cast chain-id --rpc-url "$RPC_URL")"

# ---------------------------------------------------------------------------
log "Deploying the settlement stack"
# ---------------------------------------------------------------------------
deploy_log="$(cd contracts && PRIVATE_KEY="$OWNER_KEY" DRAY_RELAYERS="$RELAYER_ADDRESS" \
    forge script script/Deploy.s.sol:Deploy \
        --rpc-url "$RPC_URL" --broadcast --skip-simulation 2>&1)" \
    || { echo "$deploy_log" >&2; die "deployment failed"; }

SETTLEMENT="$(grep -oE 'DRAY_SETTLEMENT=0x[0-9a-fA-F]{40}' <<<"$deploy_log" | head -1 | cut -d= -f2)"
[[ -n "$SETTLEMENT" ]] || { echo "$deploy_log" >&2; die "could not read the settlement address"; }
note "DraySettlement at $SETTLEMENT"
note "relayer $RELAYER_ADDRESS authorised"

# ---------------------------------------------------------------------------
log "Starting the API, a worker, and a relayer"
# ---------------------------------------------------------------------------
DATABASE_URL="$DATABASE_URL" DRAY_API_BIND="0.0.0.0:$API_PORT" \
    ./target/debug/dray-api > "$LOGS/api.log" 2>&1 &
PIDS+=($!)

DATABASE_URL="$DATABASE_URL" REDIS_URL="$REDIS_URL" \
    DRAY_ARTIFACTS_DIR="$SCRATCH/artifacts" DRAY_SCRATCH_DIR="$SCRATCH/jobs" \
    DRAY_WORKER_METRICS_BIND="0.0.0.0:9190" \
    ./target/debug/dray-worker > "$LOGS/worker.log" 2>&1 &
PIDS+=($!)

DATABASE_URL="$DATABASE_URL" DRAY_RPC_URL="$RPC_URL" \
    DRAY_RELAYER_KEY="$RELAYER_KEY" DRAY_SETTLEMENT="$SETTLEMENT" \
    DRAY_RELAYER_CONFIRMATIONS=1 DRAY_RELAYER_CONFIRM_POLL=1 \
    ./target/debug/dray-relayer > "$LOGS/relayer.log" 2>&1 &
PIDS+=($!)

for _ in $(seq 1 100); do
    curl -fsS "$API_URL/healthz" >/dev/null 2>&1 && break
    sleep 0.3
done
curl -fsS "$API_URL/healthz" >/dev/null 2>&1 || die "the API did not become ready"
note "API ready on $API_URL"

# The worker compiles circuits and writes verification keys before it leases
# anything, which takes a few seconds on a cold artefact directory.
note "waiting for the worker to prepare circuit artefacts"
for _ in $(seq 1 200); do
    grep -q "preparing circuit artefacts" "$LOGS/worker.log" 2>/dev/null && break
    sleep 0.3
done

# ---------------------------------------------------------------------------
log "Submitting a proof request over HTTP"
# ---------------------------------------------------------------------------
# The committed reference witness: secret 42 at leaf 5, all siblings 7, under
# the root those inputs actually produce.
REQUEST='{
  "circuit_id": "membership",
  "inputs": {
    "root": "0x089175ccc891f80d0f76bc5c6f7a239c2a78069ddf64478b68410c7d6b4c7320",
    "secret": "42",
    "leaf_index": "5",
    "siblings": ["7","7","7","7","7","7","7","7","7","7",
                 "7","7","7","7","7","7","7","7","7","7"]
  }
}'

response="$(curl -fsS -X POST "$API_URL/v1/proofs" \
    -H 'content-type: application/json' -d "$REQUEST")" \
    || die "the API rejected the request"

JOB_ID="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["job_id"])' <<<"$response")"
[[ -n "$JOB_ID" ]] || die "no job id in the response: $response"
note "job $JOB_ID accepted"

# Submitting twice must return the same job: identity is the hash of the
# canonical inputs, so a client retrying after a timeout cannot duplicate work.
duplicate="$(curl -fsS -X POST "$API_URL/v1/proofs" \
    -H 'content-type: application/json' -d "$REQUEST")"
DUPLICATE_ID="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["job_id"])' <<<"$duplicate")"
[[ "$DUPLICATE_ID" == "$JOB_ID" ]] \
    || die "a duplicate submission created a second job: $DUPLICATE_ID"
note "a duplicate submission returned the same job"

# ---------------------------------------------------------------------------
log "Waiting for the proof to be generated and settled"
# ---------------------------------------------------------------------------
STATE=""
for _ in $(seq 1 400); do
    job_json="$(DATABASE_URL="$DATABASE_URL" "$DRAY" job "$JOB_ID" 2>/dev/null || true)"
    STATE="$(python3 -c 'import json,sys
raw = sys.stdin.read().strip()
print(json.loads(raw)["state"] if raw else "")' <<<"$job_json")"
    case "$STATE" in
        settled) break ;;
        failed|rejected)
            die "the job ended in state '\''$STATE'\'': $job_json" ;;
    esac
    sleep 0.5
done
[[ "$STATE" == "settled" ]] || die "the job is still '$STATE' after 200 seconds"

settlement_json="$(DATABASE_URL="$DATABASE_URL" "$DRAY" settlement "$JOB_ID")" \
    || die "no settlement was recorded for $JOB_ID"

read -r TX_HASH BLOCK GAS_USED NULLIFIER <<<"$(python3 -c '
import json, sys
s = json.load(sys.stdin)
print(s["tx_hash"], s["block_number"], s["gas_used"], s["nullifier"])' <<<"$settlement_json")"

note "settled in block $BLOCK using $GAS_USED gas"
note "transaction $TX_HASH"
note "nullifier   $NULLIFIER"

# ---------------------------------------------------------------------------
log "Confirming on chain, independently of Dray's own record"
# ---------------------------------------------------------------------------
# `cast` reports status as "true"/"false" on some versions and "1"/"0" on
# others, so both spellings of success are accepted rather than one guessed at.
receipt_status="$(cast receipt "$TX_HASH" status --rpc-url "$RPC_URL" 2>/dev/null | tr -d '[:space:]')"
case "$receipt_status" in
    1|true) note "transaction succeeded on chain" ;;
    *) die "the transaction did not succeed on chain (status '$receipt_status')" ;;
esac

used="$(cast call "$SETTLEMENT" "nullifierUsed(bytes32)(bool)" "$NULLIFIER" \
    --rpc-url "$RPC_URL" | tr -d '[:space:]')"
[[ "$used" == "true" ]] || die "the nullifier is not marked consumed on chain"
note "nullifier consumed on chain"

printf '\n\033[1;32m==> e2e passed: an HTTP request became a settled proof on chain\033[0m\n\n'
printf '    job          %s\n' "$JOB_ID"
printf '    transaction  %s\n' "$TX_HASH"
printf '    block        %s\n' "$BLOCK"
printf '    gas used     %s\n' "$GAS_USED"
printf '    settlement   %s\n\n' "$SETTLEMENT"
