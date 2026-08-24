# Dray — Architecture Decision Records

One entry per significant choice. Append only; supersede rather than edit.

Status values: `proposed` (awaiting the human's confirmation), `accepted`,
`superseded by ADR-NNN`.

---

## ADR-001 — Language for the API and scheduler tier

**Date:** 2026-08-11
**Status:** accepted — confirmed by the human on 2026-08-11 (spec §9, question 1)

### Context

The build spec (§3) leaves the ingest API and scheduler tier open: Rust (axum)
or Go. The trade-off it names is that Rust keeps the stack single-language,
while Go replays the lineage of the author's prior project `Sluice` more
directly.

Two further considerations bear on the choice. First, the repository layout the
spec mandates in §4.4 already places `dray-api` inside `crates/`, alongside the
worker and relayer — the spec's own structure assumes Rust. Second, the job
state machine in `dray-core` is consumed by the API, the worker, and the
relayer alike; in a single-language stack it is one tested module rather than a
Rust implementation plus a Go reimplementation kept in sync by discipline.

### Decision

Use **Rust (axum)** for the API and scheduler tier.

The state machine lives once, in `dray-core`, and is shared by every tier. CI
runs one toolchain. The worker must be Rust regardless — it needs native
subprocess control and resource bounding — so this avoids a second language for
one service.

### Consequences

- One toolchain in CI; one lint and format configuration; one dependency audit.
- No serialisation boundary or duplicated domain types between API and worker.
- The Sluice lineage is expressed through design rather than language: durable
  Postgres state, transactional transitions, lease-based distribution. That
  reasoning is reused; no code is.
- Go's goroutine-per-request model and its standard library's HTTP ergonomics
  are given up. Axum on tokio is a close substitute and the team-of-one cost of
  the switch is low.
- Cheap to reverse until Phase 2 begins, at which point the API acquires real
  handlers, migrations, and integration tests. Revisiting after that is
  expensive.

---

## ADR-002 — Pin the Noir and Barretenberg versions explicitly

**Date:** 2026-08-11
**Status:** accepted

### Context

The spec (§3) names Noir and Barretenberg but not versions. Installing the
toolchain the obvious way — `noirup` then `bbup` with no arguments — produced a
broken pair: `noirup` installed Noir `1.0.0-beta.26`, and `bbup` then failed
with *"No version specified and couldn't determine version from noir"*.

The cause is that `bbup` resolves a backend by looking up the Noir version in
Aztec's `bb-versions.json` compatibility map, and that map's newest entry is
`1.0.0-beta.22`. It lags the Noir release train. Noir is pre-1.0 and its
serialisation format between `nargo` and `bb` is not yet stable, so an
arbitrary pairing is not merely unsupported — it can fail at proof generation
or, worse, at Solidity verifier generation, deep into Phase 1.

Left unpinned, a stranger cloning this repository gets whichever Noir is
current on the day, and quite possibly no compatible `bb` at all. The spec's
first definition-of-done item is that a stranger can clone and run the stack.

### Decision

Pin both, to the newest pair the compatibility map actually documents:

| Tool | Version |
|---|---|
| `nargo` | `1.0.0-beta.22` |
| `bb` | `5.0.0-nightly.20260522` |
| `forge` / `anvil` | `1.7.1` |

The versions become constants in `make setup`, which installs exactly these
rather than latest, and are stated in the README's prerequisites.

### Consequences

- Reproducible builds for anyone cloning the repository, which is a stated
  requirement rather than a nicety.
- Deliberately behind the Noir release train. Upgrading is a conscious act:
  bump both versions together, re-run the circuit tests, and regenerate the
  Solidity verifiers, since a verifier is tied to the backend that produced it.
- A `bb` nightly is not an ideal thing to depend on, but it is what the
  compatibility map specifies for this Noir release; the alternative is an
  undocumented pairing, which is worse.
- Verified working before Phase 1 began: `nargo execute` → `bb write_vk` →
  `bb prove` → `bb verify` completes and reports the proof valid.

---

## ADR-003 — Nullifier is public input 0 for every circuit

**Date:** 2026-08-12
**Status:** superseded by [ADR-008](#adr-008--nullifier-is-the-last-public-input-and-is-returned-not-supplied)

### Context

The spec requires Dray to be circuit-agnostic — supporting at least two
circuits specifically to prove the system is not hardcoded to one — and
separately requires the settlement contract to reject replayed proofs via a
nullifier set.

These pull against each other. The contract must read a nullifier out of a
proof's public inputs, but the public inputs of the membership circuit
(`root`) and the range proof (`min`, `max`) have nothing else in common. A
contract that switched on circuit identity to find the nullifier would need
changing every time a circuit was added, which is the opposite of
circuit-agnostic.

### Decision

Every Dray circuit declares `nullifier` as its **first** public input.

`DraySettlement.sol` reads `publicInputs[0]` as the nullifier without knowing
or caring which circuit produced the proof. Everything after index 0 is
circuit-specific and opaque to the contract; the verifier checks it.

Each circuit derives its nullifier under its own domain separator — the ASCII
bytes of `dray_membership_nullifier` and `dray_range_nullifier` respectively —
so one secret reused across circuits does not collide in the shared nullifier
set and block itself.

### Consequences

- Adding a circuit needs no settlement contract change: register a verifier
  address, and the nullifier is found at a known offset.
- The convention is load-bearing but invisible to the compiler. A new circuit
  that declares its public inputs in the wrong order would have the contract
  treat some other field as a nullifier. Mitigated by documenting it at the top
  of every circuit and asserting it in the Foundry tests, but it is a real
  sharp edge and belongs in `docs/DESIGN.md`.
- The nullifier set is global rather than per-circuit. Domain separation is
  what keeps that safe, so the domain separators are a correctness requirement,
  not decoration.

---

## ADR-004 — Proof batching deferred to v1.1

**Date:** 2026-08-12
**Status:** accepted — decided by the human (spec §9, question 3)

### Context

The spec lists batching multiple proofs into one transaction as an optional
Phase 4 task, and §9 asks whether it is in scope for v1.0. It affects the
settlement contract's interface, so it is cheaper to settle before
`DraySettlement.sol` exists than after.

### Decision

Defer batching to v1.1. `DraySettlement.sol` exposes single-proof settlement
only.

### Consequences

- The contract surface stays small, which keeps the replay and authorisation
  tests focused on what actually has to be right in v1.0.
- The gas-saving measurement the spec wanted from batching will not appear in
  `docs/BENCHMARKS.md` for v1.0. Per-settlement gas will, and that is the
  baseline any future batching would be measured against.
- UltraHonk proofs are individually large and verification is the dominant
  cost, so batching would mean either an aggregation circuit or a loop over
  verifications — the former is a substantial project, the latter saves only
  the per-transaction overhead. Deferring avoids committing to that choice
  under time pressure.
- Reversible at a cost: adding `settleBatch` later is additive to the contract
  rather than a rewrite, but it would need a redeploy.

---

## ADR-005 — Runtime-checked SQL rather than sqlx's compile-time macros

**Date:** 2026-08-12
**Status:** accepted

### Context

`sqlx` offers `query!` macros that verify SQL against a live database at
compile time — genuinely valuable, and the usual recommendation. But they
require either a running Postgres during `cargo build` or a committed `.sqlx`
offline cache, and generating that cache needs a database at least once.

The project's first definition-of-done item is that a stranger can clone the
repository and run it. A build that fails without Docker running contradicts
that directly.

### Decision

Use `sqlx::query` with runtime-checked SQL. Cover the cost with integration
tests that exercise every statement in `dray-store` against a real Postgres in
CI, gated behind a feature flag rather than skipped when `DATABASE_URL` is
absent.

### Consequences

- `cargo build` and `cargo test` work on a fresh clone with no database.
- Malformed SQL is caught by tests rather than the compiler. This is not
  theoretical: the first CI run with a real database found that every job-
  reading query was broken, because `SELECT *` returns the `job_state` enum and
  sqlx will not decode that into a `String`. The compiler and 43 unit tests had
  all passed.
- The mitigation is that those integration tests are **not allowed to skip**.
  A runtime skip on missing `DATABASE_URL` would have let that bug ship green,
  which is precisely the failure this project is supposed to demonstrate
  avoiding.
- Revisit once the schema settles: committing a `.sqlx` cache would restore
  compile-time checking while keeping offline builds working. Worth doing
  before Phase 5.

---

## ADR-006 — The API is a library with a thin binary

**Date:** 2026-08-12
**Status:** accepted

### Context

Integration tests need to build the axum router and drive it with
`tower::ServiceExt::oneshot`. A crate that is only a binary does not expose its
modules to `tests/`, so the first attempt used `include!` to pull the sources
into the test — which duplicates compilation, confuses error messages, and
breaks in obscure ways as soon as two modules refer to each other.

### Decision

`dray-api` is a library crate (`src/lib.rs`) with a thin binary
(`src/main.rs`) that only reads configuration, wires middleware, and serves.

### Consequences

- Integration tests import `dray_api::api::router` like any other consumer,
  and exercise real extractors and real handlers rather than a copy of them.
- Handler logic is testable without binding a socket.
- The same split will apply to the worker and relayer in Phases 3 and 4, where
  the chaos tests need to drive internals directly.

---

## ADR-007 — Validation happens at the API boundary, against schemas held as data

**Date:** 2026-08-12
**Status:** accepted

### Context

Each circuit accepts a different input shape: `membership` wants a secret, an
index, and exactly twenty siblings; `range_proof` wants a value and a secret.
Something has to reject a request that cannot possibly satisfy the circuit.

The obvious approach is a per-circuit validator in the API. That makes the API
the one place in the system that stops being circuit-agnostic — the settlement
contract dispatches on data (ADR-003), but adding a circuit would still mean an
API release.

### Decision

Store each circuit's JSON Schema in the `circuits` table and validate against
it at ingest. Adding a circuit is a row, not a release.

Validation runs in cheapest-first order: identifier shape, then size limits,
then schema, then canonicalisation. A rejected circuit id must not cost a walk
over a large body.

### Consequences

- The API stays circuit-agnostic, matching the contract.
- Impossible requests are rejected before they cost a lease, a subprocess, and
  a proving attempt — which at ~2.5 s per proof is the expensive resource.
- Canonicalisation runs during validation, so a float or a duplicate key is
  reported to the client as a 400 with an explanation rather than surfacing
  later as a store error with no request context.
- A malformed schema is an operator error but still surfaces to the client, as
  a schema-mismatch message saying so. Better than a 500 with nothing useful.
- The schemas are currently registered by whatever seeds the database. Keeping
  them in step with the circuits themselves is not yet automated, and that gap
  is recorded in `PROGRESS.md`.

---

## ADR-008 — Nullifier is the last public input, and is returned, not supplied

**Date:** 2026-08-17
**Status:** accepted, supersedes ADR-003

### Context

ADR-003 required every circuit to declare `nullifier` as its first public
input, and `DraySettlement.sol` read `publicInputs[0]`. That held for Phases 1
and 2 because nothing ever had to *construct* a witness — the reference
`Prover.toml` files were committed by hand, with the nullifier value copied out
of a test that printed it.

Phase 3 broke that assumption immediately. The worker builds `Prover.toml` from
the job inputs a client submitted, and a nullifier is a Pedersen hash over
private data. Supplying it as an input means somebody has to compute that hash
before the proof exists:

- **The client cannot.** Computing it means running Noir, or reimplementing
  Pedersen over BN254 against the exact domain separator the circuit uses. The
  entire premise of Dray is that a client submits inputs and gets a proof back
  without operating a proving stack. Requiring one to *place an order* is
  self-defeating.
- **The worker should not.** It could reimplement Pedersen in Rust, but then
  two independent implementations would have to agree exactly, forever. Any
  divergence surfaces as `nullifier mismatch` after a full proving attempt has
  already been paid for — the most expensive place to discover a bug.

The layout was also concealing a second defect: the registered input schema for
`membership` omitted `root`, so no submittable job could ever have produced a
solvable witness. Nothing caught it, because no code path had yet tried.

### Decision

Every Dray circuit **returns** its nullifier rather than accepting it, and the
settlement contract reads it from the **last** public input.

```noir
fn main(root: pub Field, secret: Field, ...) -> pub Field {
    assert(compute_merkle_root(...) == root, "merkle root mismatch");
    derive_nullifier(secret)
}
```

Noir orders the public input vector as declared public parameters first, then
the return value, so the nullifier lands at the end. Verified against real
proofs rather than assumed: `membership` publishes `(root, nullifier)` and
`range_proof` publishes `(min, max, nullifier)`, 2 and 3 field elements
respectively.

That the two lengths differ is what makes the convention meaningful. "Last" is
the only position two circuits with different public-input counts can share; a
fixed index only ever worked because the nullifier sat before the part that
varies.

Domain separation is unchanged. Each circuit still derives its nullifier under
its own separator, so the shared nullifier set cannot let one circuit block
another.

### Consequences

- A client submits only what it actually knows: its secret, its position, its
  authentication path, and the public root it is proving against. No hashing
  required, and no proving stack.
- The worker writes `Prover.toml` directly from validated job inputs, with no
  derivation step and no second Pedersen implementation to keep in sync.
- A forged nullifier is now impossible by construction rather than by an
  assertion. There is no input to lie about; the circuit computes it.
- The contract reads `publicInputs[publicInputs.length - 1]`. The empty-vector
  check that already existed is now load-bearing rather than defensive, since
  it is what stops the subtraction underflowing.
- Circuit tests that asserted `nullifier mismatch` had nothing left to reject,
  so they were replaced by tests of the property that actually matters now:
  that the *published* nullifier is bound to the witness — same secret gives
  the same nullifier from any tree position, different values under one secret
  give different nullifiers, and the declared range does not perturb it.
- Verification keys changed, so both Solidity verifiers were regenerated. The
  CI job that diffs regenerated verifiers against the committed ones is what
  keeps that from drifting.
- `membership`'s registered schema gained the `root` it was missing.
- Cost: the circuits, the contract, both scripts and 25 contract tests had to
  change, mid-Phase-3, to fix a convention two phases old. It was worth it —
  the alternative was a proving tier that could not construct a witness — but
  it is the clearest evidence so far for building the thin end-to-end slice
  early. A single job proved through the real worker in Phase 1 would have
  caught this before anything was built on top of it.

---

## ADR-009 — Redis mirrors leases; every operation on it is best-effort

**Date:** 2026-08-24
**Status:** accepted

### Context

The spec calls for the lease TTL to be mirrored into Redis for fast liveness
checks, and separately states the invariant that **Postgres state is always
recoverable without Redis**. Those two pull in opposite directions if the mirror
is allowed to become load-bearing, which is the usual way a cache stops being a
cache.

The specific hazard is not that Redis goes down. It is what a caller does when
it cannot get an answer. A liveness check has three possible outcomes and only
two obvious ones:

- someone holds this lease,
- nobody holds this lease,
- **I could not find out.**

Collapse the third into the second and a Redis outage becomes indistinguishable
from every lease in the system having expired simultaneously. Every in-flight
job would be handed to a second worker, so a cache outage would turn into a
fleet-wide stampede of duplicated proving — while the jobs were all still being
proved perfectly well by their original owners.

### Decision

`LeaseCache` mirrors lease state into Redis under `dray:lease:<job_id>`, and
every operation on it is infallible from the caller's point of view.

- `record` and `forget` return nothing. A failure is logged at warning level and
  the job proceeds. Postgres already holds the truth, written in the same
  transaction as the state change.
- `liveness` returns a three-valued `Liveness`, and `Unknown` is a distinct
  variant from `Free`. `is_definitely_free` is the only thing that authorises
  acting, and only `Free` satisfies it.
- Connecting is the one fallible operation, because it happens at start-up where
  an operator can act on it. A worker that cannot reach Redis logs and runs
  without a mirror.
- Redis expiry is *not* the mechanism that returns a job to the queue. The
  reaper does that, from Postgres. If a key vanishes early the job is still
  leased; if it lingers, the reaper still takes the job back on time.

Compose runs Redis with persistence disabled, deliberately, so the recovery path
is exercised in development instead of assumed. `LeaseCache::rebuild` is that
path: after a restart the mirror is empty, and every lease Postgres still holds
is written back with the time remaining on it. Expired leases are skipped —
writing one back would advertise a holder for a job about to be taken off them.

### Consequences

- The mirror can be wrong, and nothing breaks when it is. It is a hint that
  saves a Postgres round trip, never an authority.
- `Liveness::Unknown` forces the caller to decide what to do about not knowing,
  rather than letting the type quietly answer for it.
- A worker with no `REDIS_URL` is fully functional. That is tested directly, by
  pointing a worker at a dead Redis and requiring it to prove a job anyway.
- Ordering is fixed on renewal: Postgres first, then the mirror. The other order
  could leave the mirror advertising a lease Postgres had just refused to
  extend.
- Cost: one more dependency in the hot path, for a saving that is currently
  theoretical — nothing yet asks a liveness question often enough to need it.
  The mirror is built because the spec requires it and because the recovery path
  is worth having in place before something depends on it, not because a
  measurement showed Postgres was too slow.

---

## ADR-010 — Base Sepolia as the target testnet

**Date:** 2026-08-24
**Status:** accepted

### Context

The spec left the choice between Base Sepolia and Ethereum Sepolia open for the
human to make. It was deferred until Phase 4 because nothing before it touched a
public chain.

### Decision

Base Sepolia. Decided by the human.

### Consequences

- Base is an OP Stack L2, so blocks arrive roughly every 2 seconds against
  Ethereum's 12. Confirmation depth is a count of blocks, so the same N is a
  much shorter wall-clock wait — which makes the confirmation-tracking tests
  practical to run for real rather than only against Anvil.
- Gas is cheap enough that the batching deferred in ADR-004 stays deferred
  without embarrassment. The gas saving batching would buy is worth measuring
  in v1.1, but it is not what makes this system interesting.
- L2s reorg differently from L1: sequencer-driven reordering is rare but the
  chain is not final until it is proven on L1. Treating `settled` as
  non-terminal was already the design (see the state machine); on an L2 it is
  not a theoretical nicety.
- The verifier contract is EVM-equivalent bytecode, so nothing about the
  circuits or the settlement contract changes. Only the RPC endpoint, chain id,
  and gas policy differ.

---

## ADR-011 — A small permissioned set of relayers, not a single operator

**Date:** 2026-08-24
**Status:** accepted

### Context

The spec offered "a single trusted operator (simpler, honest)" or "a small
permissioned set (more impressive, considerably more work)". Decided by the
human: the permissioned set.

`DraySettlement` already carries the on-chain half — an `isRelayer` mapping and
`setRelayer`, both owner-gated — so authorisation itself needs no contract
change. The work is entirely off chain, and it is not where it first appears to
be.

### Decision

Several relayer processes, each holding **its own key and therefore its own
nonce**, all authorised by the settlement contract.

The hard part is not authorisation. It is that N relayers share one queue of
`proved` jobs. Two relayers picking up the same job would both submit, and both
would pay: the first settles, the second reverts on the nullifier set having
already consumed it. Correct, because the contract is the second line of
defence — but it burns real gas to discover something the database already knew.

So relayers lease their work exactly as workers do, through the same
`FOR UPDATE SKIP LOCKED` machinery, with the queue being `proved` jobs rather
than `queued` ones. Leasing is what keeps each job to one relayer; the nullifier
set stays the backstop it was designed to be rather than the primary mechanism.

Nonce management is unaffected, and this is the point of one key per process:
each relayer is the single writer to its own account's nonce. Sharing one key
across processes would need distributed nonce allocation, which is the genuinely
hard version of this problem and buys nothing here.

### Consequences

- One relayer can be lost without settlement stopping, which a single operator
  cannot offer. The leases of a dead relayer expire and its jobs return to the
  `proved` queue, reusing the reaper that already exists.
- Each relayer needs its own funded account on Base Sepolia, and each must be
  registered with `setRelayer`. Adding one is two operations, not a deploy.
- Gas accounting becomes per-relayer. `settlements` records the transaction, so
  attribution comes from the chain rather than from a separate ledger.
- Cost, stated plainly: leasing a second job class, a second reaper path, and
  tests for two relayers contending. That is the "considerably more work" the
  spec warned about, and it is real.
- What this is *not*: decentralisation. The owner still controls the relayer
  set, and the contract says so in its own documentation. A permissioned set is
  redundancy, not trustlessness, and claiming otherwise would be dishonest.
