# Dray

> **Status: in development, Phase 0 of 6.** Nothing below is claimed to work
> unless `PROGRESS.md` records it as verified. See
> [PROGRESS.md](PROGRESS.md) for what actually runs today.

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

*(Concrete measured numbers replace this paragraph's hand-waving in Phase 1,
once proving time and peak memory are recorded for both circuits.)*

## Architecture

*(Diagram lands in Phase 6 as committed Mermaid source. The prose sketch below
holds until then.)*

Client → **Ingest API** (validate, canonicalise, hash, dedupe, enqueue) →
**Postgres** (durable job state) **+ Redis** (leases, backpressure) →
**Worker pool** (Noir/Barretenberg proving under strict resource bounds) →
**Relayer** (nonce management, gas policy, confirmation tracking) →
**Verifier contract** (Solidity, on chain).

## Quickstart

Prerequisites: Docker, and a Rust toolchain with a C linker.

```bash
git clone <repo> && cd dray
make setup    # verify prerequisites, install rustfmt and clippy
make up       # start Postgres and Redis, wait for healthy
make test     # run the suite
```

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
   recoverable failure that is metered, not a crash.
4. **Nonce and gas management.** The relayer is the single writer to its
   account nonce. Submissions are serialised, stuck transactions are bumped and
   replaced under a ceiling, and settlement is confirmed to N blocks to survive
   reorgs.

## Benchmarks

*(Phase 6, in `docs/BENCHMARKS.md`: proving time and peak memory per circuit,
throughput at N workers, gas per settlement, and the saving from batching. Real
measured numbers with methodology — none exist yet.)*

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
make build    # build the workspace
make test     # unit and integration tests
make lint     # cargo fmt --check and clippy -D warnings
make down     # stop dependencies
make clean    # stop dependencies and drop their volumes
```

Adding a circuit is documented in Phase 6, once the second circuit has proven
the interface is genuinely circuit-agnostic.

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
