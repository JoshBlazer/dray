# Dray

[![CI](https://github.com/JoshBlazer/dray/actions/workflows/ci.yml/badge.svg)](https://github.com/JoshBlazer/dray/actions/workflows/ci.yml)

> **Status: in development, Phase 2 of 6.** Nothing below is claimed to work
> unless `PROGRESS.md` records it as verified. See
> [PROGRESS.md](PROGRESS.md) for what actually runs today.
>
> **Working now:** both circuits and their generated Solidity verifiers, with
> settlement on a local chain (`make e2e-circuits`); and the ingest API with a
> durable, deduplicating job store.
> **Not working yet:** nothing consumes the queue. There is no worker and no
> relayer, so an accepted job stays `queued`, and nothing has touched a public
> testnet.

A distributed off-chain proof generation and relaying network: clients submit
zero-knowledge circuit inputs over HTTP, a pool of Rust workers generates the
proofs in parallel, and a relayer settles them on chain.

## The problem

Generating a zero-knowledge proof is expensive enough that doing it in a browser
freezes the tab for tens of seconds on ordinary hardware. A dApp that wants
client-side privacy therefore picks between two bad options: prove in the
browser and make every user wait, or prove on one server and own a bottleneck
that is also a single point of failure. Dray is the third option — a durable,
horizontally scalable proving tier with at-least-once delivery and on-chain
settlement.

Concretely: a Merkle membership proof over a depth-20 tree takes **2.5 seconds
and 42 MB** on a four-core machine — and that is native code, with none of the
overhead a browser would add. Multiply by every user on every action.

## Architecture

*(Diagram lands in Phase 6 as committed Mermaid source. The prose sketch below
holds until then.)*

Client → **Ingest API** (validate, canonicalise, hash, dedupe, enqueue) →
**Postgres** (durable job state) **+ Redis** (leases, backpressure) →
**Worker pool** (Noir/Barretenberg proving under strict resource bounds) →
**Relayer** (nonce management, gas policy, confirmation tracking) →
**Verifier contract** (Solidity, on chain).

## Quickstart

Prerequisites: Docker, and a Rust toolchain with a C linker (`build-essential`
on Debian or Ubuntu).

```bash
git clone https://github.com/JoshBlazer/dray && cd dray
make setup            # verify prerequisites, install rustfmt and clippy
make up               # start Postgres and Redis, wait for healthy
make seed             # register the circuits and their input schemas
make test             # unit and property tests, no database needed
make test-integration # tests that need the live Postgres and Redis
make api              # run the ingest API on :8080
make worker           # run a proving worker (needs make setup-zk)
```

Then submit a proof request:

```bash
curl -X POST localhost:8080/v1/proofs -H 'content-type: application/json' -d '{
  "circuit_id": "membership",
  "inputs": {
    "root": "0x089175ccc891f80d0f76bc5c6f7a239c2a78069ddf64478b68410c7d6b4c7320",
    "secret": "42", "leaf_index": "5",
    "siblings": ["7","7","7","7","7","7","7","7","7","7",
                 "7","7","7","7","7","7","7","7","7","7"]
  }
}'
```

Note what is *not* in there: the nullifier. Circuits derive it and publish it as
the last public input, so a caller never has to compute a Pedersen hash to place
an order. See [ADR-008](DECISIONS.md).

It returns `202` with a job id. Submit it twice and the second response carries
the same id with `"created": false` — the job is identified by the hash of its
canonical inputs, so a retry after a timeout cannot create duplicate work.

With `make worker` running, that job is leased, proved, and stored with its
proof and public inputs within a few seconds. `make test-worker` runs the pool
under load and under chaos: 100 jobs across 4 workers, and the same run with
workers killed mid-proof.

**Nothing settles that proof on chain yet.** The relayer is Phase 4; today a
proved job stays `proved`.

For the circuits and contracts, `make setup-zk` installs the proving toolchain
at the exact pinned versions below; `make versions` reports what you have
against what is expected.

| Tool | Pinned version | Why pinned |
|---|---|---|
| `nargo` | 1.0.0-beta.22 | Noir is pre-1.0; its interchange format with the backend is not yet stable |
| `bb` | 5.0.0-nightly.20260522 | The backend Aztec's compatibility map pairs with that Noir release |
| `forge` / `anvil` | 1.7.1 | |

Installing latest-of-both does **not** work today — see
[ADR-002](DECISIONS.md) for the details.

`make e2e` — clone to first settled proof — arrives in Phase 4.

## How it works

The four problems this project exists to solve well. Each is written up
properly in `docs/DESIGN.md` in Phase 6.

1. **Idempotency.** Job identity is `hash(circuit_id || canonicalised_inputs)`
   under a unique index, so duplicate submissions return the existing job. The
   settlement contract independently rejects replayed proofs via a nullifier
   set — at-least-once delivery means the relayer will eventually submit twice.
2. **Lease-based work distribution.** Workers lease jobs with a TTL rather than
   holding locks. A crashed worker's lease expires and the job returns to the
   queue. No leader election is required.
3. **Resource bounding.** Every proving subprocess runs under a wall-clock
   timeout, a memory ceiling, and a CPU quota. Exceeding a bound is a normal,
   recoverable failure that is metered, not a crash. The limits are applied by
   `ulimit` before `exec` rather than by `pre_exec`, so the workspace's
   `unsafe_code = "forbid"` survives in the one service that runs untrusted
   input. Peak memory is sampled from the kernel's own high-water mark, so the
   ceilings can be checked against what proving actually costs rather than
   against what it cost once.
4. **Nonce and gas management.** The relayer is the single writer to its
   account nonce. Submissions are serialised, stuck transactions are bumped and
   replaced under a ceiling, and settlement is confirmed to N blocks to survive
   reorgs.

## Benchmarks

Measured on 4 cores and 7.7 GB RAM — a deliberately modest box. Reproduce with
`make prove`.

| Circuit | Constraints | Prove | Peak RSS | Proof | On-chain verify |
|---|---|---|---|---|---|
| `membership` (tree depth 20) | 414 ACIR opcodes | 2.47 s | 42 MB | 8,384 B | ~3.01 M gas |
| `range_proof` | 33 ACIR opcodes | 1.89 s | 40 MB | 8,384 B | ~3.01 M gas |

Two things worth noting. Proving cost is dominated by the proof system rather
than by circuit size — a 12× difference in constraints produces a 1.3×
difference in proving time — so capacity planning cannot assume small circuits
are cheap. And verification costs about 3 M gas regardless, which is what makes
proof aggregation the interesting optimisation rather than transaction
batching.

Throughput at N workers lands in Phase 3; `docs/BENCHMARKS.md` arrives in
Phase 6.

## Deployed addresses

*(Phase 4. Testnet verifier and settlement addresses with explorer links.)*

## Limitations

Written in full in Phase 6, but the central one is true by design and worth
stating now: **Dray is a trusted-operator proving tier.** The workers and the
relayer are run by one operator and are trusted to do their work honestly.
There is no decentralised trust model between untrusted provers, no staking, no
slashing, and no token. A malicious worker cannot forge a proof the verifier
contract would accept, but it can decline to do work, and the operator can
censor. Making that trust assumption unnecessary is a much larger project than
this one.

Also explicitly out of scope: mainnet deployment, a custom proving system, and
a general-purpose blockchain indexer.

## Development

```bash
make build            # build the workspace
make test             # unit and property tests
make test-integration # tests needing a live Postgres
make lint             # cargo fmt --check and clippy -D warnings
make down             # stop dependencies
make clean            # stop dependencies and drop their volumes
```

The integration tests use a separate `dray_test` database, so running them does
not litter your development data. `make reset-test-db` drops and recreates it.

```bash
make circuits       # compile the Noir circuits and run their tests
make prove          # generate proofs and regenerate the Solidity verifiers
make contracts      # forge build and forge test
make e2e-circuits   # the whole path: input -> proof -> settled on Anvil
```

### Adding a circuit

The settlement contract is circuit-agnostic, which imposes exactly one rule:
**every circuit must `return` its nullifier, and nothing else.** Noir places
return values after the declared public parameters, so the nullifier ends up
last in the public input vector, and `DraySettlement` reads
`publicInputs[publicInputs.length - 1]` without knowing which circuit produced
the proof. A circuit that returned a second value, or that accepted its
nullifier as a parameter instead, would have some unrelated field treated as
its nullifier.

Return it rather than accept it. A nullifier is a hash of private data, so a
circuit that takes one as input forces the *caller* to compute it — which means
running the proving stack Dray exists to run for them. Use a distinct domain
separator when deriving it, or a secret reused across circuits will collide in
the shared nullifier set. See [ADR-008](DECISIONS.md).

Then: add the package to `circuits/Nargo.toml`, add its name to the `CIRCUITS`
array in `scripts/prove.sh` and `scripts/e2e-circuits.sh`, and register the
generated verifier in `contracts/script/Deploy.s.sol`. No settlement contract
change is required.

## Repository map

| Path | What lives there |
|---|---|
| `circuits/` | Noir circuits — Merkle membership and range proof |
| `contracts/` | Foundry project: generated verifiers and settlement logic |
| `crates/dray-core/` | Domain types and the job state machine, pure and I/O-free |
| `crates/dray-store/` | Postgres and Redis adapters |
| `crates/dray-api/` | Ingest HTTP API |
| `crates/dray-worker/` | Proving worker |
| `crates/dray-relayer/` | Chain submission |
| `crates/dray-cli/` | Operator CLI (`dray`) |
| `migrations/` | Postgres schema migrations |
| `tests/` | Cross-crate integration and end-to-end tests |

`DRAY_BUILD_SPEC.md` is the build contract this repository is being written
against. `DECISIONS.md` records the architecture decisions made along the way.

## Licence

MIT.
