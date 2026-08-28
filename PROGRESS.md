# Dray — Progress

**Current phase:** 4 — Relayer and on-chain settlement (in progress)
**Last updated:** 2026-08-28
**Build status:** green — `make build`, `make test`, and `make lint` pass
locally, and CI is green on a fresh clone across all five jobs.
Repository: <https://github.com/JoshBlazer/dray>

## Phase status

| Phase | Name | Status | Exit criteria met | Notes |
|-------|------|--------|-------------------|-------|
| 0 | Foundations | done | yes | CI green on a fresh clone (run 31593551054). One caveat on `make up` — see below |
| 1 | Circuits and on-chain verification | done | yes | `make e2e-circuits` proves both circuits and settles them on Anvil |
| 2 | Ingest API and durable job store | done | yes | Verified in CI against real Postgres; not yet locally (no Docker) |
| 3 | Proof worker pool | done | yes | 100 jobs across 4 workers proved exactly once; the same run with workers killed throughout loses nothing |
| 4 | Relayer and on-chain settlement | in progress | no | Settles on a local chain, survives reorgs and nonce gaps. The exit criterion needs a funded Base Sepolia account |
| 5 | Observability, operations, hardening | not started | no | |
| 6 | Documentation, demo, release | not started | no | |

## What works right now

Verified by execution on this machine, via the Makefile targets a reviewer
would actually run:

- `make setup` succeeds — toolchain components present, dependencies fetched.
- `make build` compiles all six crates.
- `make test` passes: 76 tests across the workspace, 0 failures.
- `make lint` is clean — `cargo fmt --check` and `cargo clippy -D warnings`.
- `make versions` reports the installed proving toolchain matching the ADR-002
  pins exactly.

And on CI, from a fresh clone, five jobs:

- **Build and test** — the workspace unit and property suites.
- **Format and clippy** — `-D warnings`.
- **Dependencies start healthy** — runs `docker compose up -d --wait`, so
  Postgres and Redis reaching a healthy state is a verified fact, not a claim.
- **Migrations and store integration** — applies the migrations to a real
  Postgres, diffs the Postgres enums against the Rust ones, runs the seed
  script, then the store and API integration suites.
- **Circuits and contracts** — installs the pinned toolchain, runs the Noir and
  Foundry suites, asserts the regenerated verifiers match what is committed,
  and finishes with the full Anvil end-to-end.

`make up` has now been run locally as well: Postgres and Redis both reach
healthy on the author's workstation, not only on CI.

### Phase 1 — the cryptographic path, end to end

`make e2e-circuits` runs the whole thing: compile both circuits, solve
witnesses, generate real proofs, regenerate the Solidity verifiers from the
resulting keys, start Anvil, deploy, settle both proofs as actual transactions,
and confirm each nullifier is consumed on chain. It passes.

- `make circuits` — 21 Noir tests pass (10 membership, 11 range proof).
- `make contracts` — 25 Foundry tests pass, including 4 fuzz campaigns at 256
  runs each against real proofs.
- Both circuits settle through one `DraySettlement` with no circuit-specific
  code path, which is what "circuit-agnostic" has to mean to be worth claiming.
- Replay is rejected on chain, not just in a unit test.

**Measured proving cost** (4 cores, 7.7 GB RAM — a modest box, and throughput
numbers must be quoted with that caveat):

| Circuit | ACIR opcodes | VK gen | VK peak RSS | Prove | Prove peak RSS | Proof |
|---|---|---|---|---|---|---|
| `membership` (depth 20) | 414 | 1.19 s | 36 MB | 2.47 s | 42 MB | 8,384 B |
| `range_proof` | 33 | 0.94 s | 32 MB | 1.89 s | 40 MB | 8,384 B |

Reproduce with `make prove`.

**Measured on-chain cost**, from the Foundry gas report: a single `settle` call
costs roughly **3.01 M gas**, essentially all of it UltraHonk verification.
That number is why ADR-004's deferral of batching is worth revisiting in v1.1 —
at 3 M gas per proof, amortising the ~21 k transaction overhead is negligible,
but an aggregation circuit would be transformative.

Both generated verifiers are ~18.1 KB of runtime bytecode, comfortably inside
the 24,576 B EIP-170 limit but not by so much that it can be ignored — a
substantially larger circuit could exceed it.

### Phase 2 — ingest API and durable job store

- `dray-core` — the job state machine, 40 tests. All 88 (state, event) pairs
  covered: 24 legal transitions verified, 64 illegal ones confirmed rejected.
  The legal table is written longhand in the test rather than derived from the
  implementation, so the two disagree when either changes. Eight property tests
  cover arbitrary event sequences.
- `dray-store` — schema, migrations, and the job repository. 19 integration
  tests against real Postgres, including the spec's fifty-concurrent-identical-
  submissions requirement and an eight-worker lease race where exactly one must
  win.
- `dray-api` — `POST /v1/proofs`, `GET /v1/proofs/{id}`, and health/readiness
  probes. 76 unit tests plus 14 HTTP integration tests driving the real router
  against real Postgres.

Two guarantees live in the database rather than in application code: `job_state`
is a real Postgres enum, and `jobs.job_hash` is `UNIQUE`. CI diffs the Postgres
enums against the Rust ones, so drift fails the build.

**Verified locally as well as in CI.** `make up`, `make seed`, and
`make test-integration` all run on the author's workstation: 17 store and 14
API integration tests against a real Postgres, plus a live end-to-end check —
the API accepted a membership request, returned the same `job_id` with
`created: false` on a byte-identical resubmission, and rejected a 19-sibling
path with a 400 naming the field and the constraint.

The toolchain is version-pinned per ADR-002, and `make setup-zk` has been run
from scratch to confirm it installs exactly those versions rather than latest:
`nargo` 1.0.0-beta.22, `bb` 5.0.0-nightly.20260522, `forge`/`anvil` 1.7.1.

### Toolchain notes worth keeping

Relevant to Phase 3, when the worker takes over invoking these tools:

- **`bb` 5.x requires the verification key before proving.** `bb prove` fails
  with *"Unable to open file: ./target/vk"* unless `bb write_vk` has run first.
  The worker's proving sequence must account for this; the vk is per-circuit
  and should be generated once at circuit registration, not per job.
- **`-t evm` must be passed consistently** to `write_vk`, `prove`, and
  `write_solidity_verifier`. It selects the keccak transcript the Solidity
  verifier expects; mixing targets produces a proof the on-chain verifier
  rejects, with no obvious diagnostic.
- **The generated verifier reverts on an invalid proof**, it does not return
  `false`. Any caller that needs an answer rather than an exception must wrap
  it in `try`/`catch` — see the `wouldSettle` decision below.
- The scheme is **UltraHonk**, and `-t evm` selects its ZK variant. The
  verifier appends 8 pairing-point public inputs internally, so
  `NUMBER_OF_PUBLIC_INPUTS` reads 10 and 11 while the circuits declare 2 and 3.
  The `verify` call takes only the declared ones.
- `/usr/bin/time -f "%e %M"` is how wall time and peak RSS were captured; the
  worker's memory metrics can be validated against it.
- **This development machine has 4 threads and 7 GB RAM.** `bb` reports the
  thread count it uses. Capacity planning and the worker's default resource
  bounds should be derived from measurement on this box, and the constraint
  stated when quoting throughput numbers.

## What does not work yet

- **The worker and relayer are still skeletons.** A component name, a doc
  comment, and one trivial test each. Nothing leases a job, nothing proves one,
  nothing submits one. A job accepted by the API today sits in `queued`
  forever — the API and store are real, but nothing consumes the queue yet.
- **The operator CLI is a skeleton.** No subcommands.
- **Nothing has touched a public testnet.** All on-chain work so far is against
  a local Anvil instance. No deployed addresses, no transaction hashes.
- **Proving is driven by shell scripts, not by a worker.** `make prove` shells
  out to `nargo` and `bb` with no timeout, no memory ceiling, and no CPU quota.
  Those bounds are Phase 3's central task and the resource-bounding metrics do
  not exist yet.
- **No leasing, no Redis.** `dray-store` talks only to Postgres. Redis is in
  the Compose file and nothing uses it.
- **No metrics, no traces.** Phase 5.
- `migrations/`, `tests/`, and `docs/` are still empty directories.
- `make e2e` deliberately exits non-zero with a message; it lands in Phase 4.

### Known sharp edges from Phase 1

- **The nullifier convention is not compiler-enforced.** A new circuit that
  returned a second value, or accepted its nullifier as a parameter, would have
  `DraySettlement` read some other field as a nullifier.
  `test_public_input_layout_matches_adr_008` asserts it for the two circuits
  that exist; there is nothing stopping a third from getting it wrong. Note
  that returning the nullifier (ADR-008) makes this harder to get wrong than
  the original index-0 rule did, because the position is now a consequence of
  the signature rather than an ordering the author has to remember.
- **Verifier bytecode is 18.1 KB against a 24,576 B limit.** Roughly 6.4 KB of
  headroom. A materially larger circuit could exceed EIP-170, and the failure
  would appear at deployment, not at compile time.
- **`bb` proving is unsandboxed today.** It is invoked directly by scripts.
  Nothing yet stops a pathological input from exhausting memory.

## Blocked on

*Nothing is currently blocked.*

*Resolved 2026-08-12: Docker. Two false diagnoses preceded the real one. The
daemon was never down: `docker.io` was installed and `dockerd` was running and
healthy the whole time. Docker Desktop's WSL integration — which **was**
enabled — replaced the systemd-created `/run/docker.sock` ten minutes after
`dockerd` bound it, leaving a socket file nothing listened on while `dockerd`
kept the now-unreachable original. Hence `ECONNREFUSED` rather than `EACCES`,
and hence a healthy daemon that no client could reach. `systemctl restart
docker.socket docker.service` rebound the path. Separately, a dead Desktop
symlink at `/usr/local/lib/docker/cli-plugins/docker-compose` shadowed the real
plugin — that search path is consulted before `/usr/libexec` — which is why
`docker compose` reported "unknown command" while the plugin binary ran fine on
its own. Fixed with a user-level symlink in `~/.docker/cli-plugins`.*

*Resolved 2026-08-12: the GitHub remote — <https://github.com/JoshBlazer/dray>,
pushed, CI green on the first run.*

*Resolved 2026-08-11: the C toolchain and `make` — `build-essential` installed
gcc 15.2.0, libc6-dev, and GNU Make 4.4.1. `make build`, `make test`, and
`make lint` now all pass.*

## Decisions made

| Date | Decision | Rationale | Alternatives rejected |
|------|----------|-----------|----------------------|
| 2026-08-11 | ADR-001: Rust (axum) for the API and scheduler tier — **accepted** | Spec §4.4 already places `dray-api` under `crates/`; one shared `dray-core` state machine instead of two implementations kept in sync | Go (closer to the author's prior `Sluice`, but adds a second toolchain and duplicates the domain model) |
| 2026-08-11 | Redis runs with persistence disabled in Compose | Redis is a cache, never truth. Making it non-durable in dev forces the recovery path to be exercised rather than assumed | Default RDB snapshotting, which would quietly let Redis become load-bearing |
| 2026-08-11 | ADR-002: pin `nargo` 1.0.0-beta.22 and `bb` 5.0.0-nightly.20260522 | Unpinned install produced a broken pair — `bbup` could not resolve a backend for the Noir `noirup` had just installed. Reproducibility for a stranger cloning the repo is a stated requirement | Latest-of-both (breaks today), or an undocumented pairing (may fail at verifier generation, deep into Phase 1) |
| 2026-08-11 | Phase 0 crates carry no third-party dependencies | Keeps the harness itself trivially verifiable; dependencies arrive with the features that need them | Wiring axum, tokio, and sqlx up front, which would make a green build prove less |
| 2026-08-12 | ADR-003: every circuit declares `nullifier` as public input 0 — **superseded by ADR-008** | Lets `DraySettlement` find the nullifier without knowing the circuit, which is what makes it genuinely circuit-agnostic | Per-circuit adapters in the contract (a code change per circuit), or a nullifier passed alongside the proof (unbound, so forgeable) |
| 2026-08-12 | ADR-004: batching deferred to v1.1 | Decided by the human. Keeps the v1.0 contract surface small and the replay tests focused | Batching in v1.0, which would mean choosing between an aggregation circuit and a verification loop under time pressure |
| 2026-08-12 | Pedersen for hashing, not Poseidon | Poseidon is not in this Noir's stdlib and would mean an external dependency; `std::hash::pedersen_hash` is built in | Poseidon2 via an external package, adding a dependency to pin and audit for no benefit at this stage |
| 2026-08-12 | `wouldSettle` catches verifier reverts and returns false | Found by a fuzz test: the generated verifier reverts rather than returning false, which made pre-flight useless for its one job | Letting the revert propagate, which would give the relayer an exception instead of an answer |
| 2026-08-17 | ADR-008: circuits *return* the nullifier, so it is the last public input | Found while wiring the worker: a supplied nullifier is a Pedersen hash of private data, so requiring one means the client must run a proving stack to place an order. Also exposed that `membership`'s registered schema was missing `root`, making every submittable job unsolvable | Pedersen in Rust in the worker (two implementations to keep in exact agreement, diverging only after a proof has been paid for), or client-computed nullifiers (defeats the product) |
| 2026-08-24 | ADR-009: Redis mirrors leases, and `Liveness::Unknown` is distinct from `Free` | Collapsing them would make a Redis outage look like every lease expiring at once, handing every in-flight job to a second worker — a cache outage becoming a fleet-wide stampede of duplicated proving | A two-valued liveness check (fails exactly when it is most needed), or making Redis authoritative for leases (contradicts the spec's recoverability invariant) |
| 2026-08-24 | ADR-010: Base Sepolia as the target testnet | Decided by the human. ~2s blocks make a confirmation depth of N a short enough wait to test against the real chain, not only against Anvil | Ethereum Sepolia, where 12s blocks make confirmation tests slow enough that they stop being run |
| 2026-08-24 | ADR-011: a small permissioned set of relayers | Decided by the human. One relayer can be lost without settlement stopping, which a single operator cannot offer | A single trusted operator (simpler and honest, but no redundancy), or one shared key across processes (needs distributed nonce allocation, which is the genuinely hard version and buys nothing here) |
| 2026-08-28 | `proved` jobs are leased, like `queued` ones | With a set of relayers, two would otherwise submit the same proof: the first settles, the second reverts on a consumed nullifier. Correct, but it burns real gas to learn what the database already knew | Relying on the nullifier set alone, which makes the on-chain backstop the primary mechanism and pays for it every time |
| 2026-08-28 | Submission attempts are counted separately from proving attempts | A job that needed two tries to prove would otherwise arrive at the relayer with its budget two-thirds spent, and could exhaust itself on one RPC blip — discarding a valid proof | One shared counter, which conflates work that costs seconds of CPU with work that costs a nonce and real gas |
| 2026-08-28 | Reorgs are detected by asking whether the nullifier is still consumed | That is what a settlement *is* to this system. It stays correct when a transaction is re-mined into a different block, which needs no action | Tracking block hashes, which reports a "reorg" for a harmless re-mine and needs a schema column to store |

## Open questions (for the human)

- [x] ~~ADR-001 — confirm Rust for the API tier.~~ Answered 2026-08-11: Rust
      with axum. See ADR-001.
- [x] Target testnet: Base Sepolia or Ethereum Sepolia? **Base Sepolia**
      (ADR-010). ~2s blocks, so a confirmation depth of N is a short enough wait
      to test against the real chain rather than only against Anvil.
- [x] ~~Is proof batching in scope for v1.0?~~ Answered 2026-08-12: deferred to
      v1.1. See ADR-004. `DraySettlement.sol` exposes single-proof settlement
      only.
- [x] Single trusted relayer operator, or a small permissioned set? **A small
      permissioned set** (ADR-011). Each relayer holds its own key, so each is
      the single writer to its own nonce; they share the `proved` queue through
      the same leasing machinery the workers use.

## Next actions

Phase 4 — the relayer and on-chain settlement — is under way. Every task is
built and tested against a real chain; what remains is the exit criterion,
which needs something only the human can supply.

Done:

1. **Single-writer nonce management.** Each relayer holds its own key, so each
   owns one nonce sequence (ADR-011). The lock is held across the whole
   submission rather than just the allocation — otherwise two transactions can
   be broadcast out of order, and a permanent failure on the earlier one
   strands the later one for ever.
2. **Gas policy.** Estimate, cap, and bump-and-replace. Both EIP-1559 fees rise
   together, because nodes check both against their 10% replacement rule and a
   bump that raises only `maxFeePerGas` is refused while looking like it
   worked. A bump the ceiling would clamp below that threshold reports the
   ceiling instead of buying a round trip to be told "underpriced".
3. **Confirmation tracking and reorg handling.** Confirmed to N blocks, then
   *watched*, because a depth that makes a reorg unlikely does not make it
   impossible. A settlement that disappears returns the job to `proved` and is
   submitted again, keeping the proof.
4. **Batching** — deferred to v1.1 by ADR-004. Not built.
5. **Relayer authentication on chain.** `setRelayer` was already there; the
   deploy script now authorises a set of them from `DRAY_RELAYERS`, and a
   relayer that is not authorised refuses to start rather than producing a
   stream of identical reverts.
6. **Failure taxonomy.** Three outcomes, not two — the third being "already
   settled", which arrives disguised as a revert and would otherwise mark a
   settled job failed.

Remaining:

7. **Deploy to Base Sepolia and settle one job through the whole system.** The
   exit criterion. Needs a funded account on Base Sepolia, which is the one
   thing that cannot be produced from this side. See below.

**Exit criterion:** a job submitted at the API arrives as a verified on-chain
settlement on a public testnet, with the transaction hash recorded here.

### What is needed to close Phase 4

Two things from the human:

- **A funded Base Sepolia account** for the relayer. Base Sepolia ETH comes
  from a faucet; a settlement costs roughly 3.8M gas, so a small balance covers
  many runs.
- **An RPC endpoint.** `https://sepolia.base.org` is public and works; a keyed
  provider is steadier under load.

Then `make deploy` with `DRAY_RPC_URL` and `PRIVATE_KEY` set puts the stack on
chain, and the same three services that `make e2e` runs locally settle a real
job. The transaction hash lands in this file.

### What Phase 4 found

Four bugs, all surfaced by the Anvil suite rather than by review — three of
them mine, and each one silent in a different way:

- **`renew_lease` never covered `submitting`.** Every relayer heartbeat
  returned false, so a relayer abandoned its own work the moment a settlement
  took longer than one heartbeat interval. The fast tests passed because they
  finish before the first tick.
- **A replacement transaction was recorded as a second settlement**, which the
  schema's one-live-settlement-per-nullifier index correctly refused. A bumped
  or re-nonced transaction is the same settlement carried by a different hash.
- **The bump path never recorded its new hash at all.** Confirmation matches on
  `tx_hash`, so a bumped transaction would have confirmed nothing while the row
  went on naming a transaction that no longer existed.
- **`confirm_settlement` cleared `reorged_at`.** After a reorg the relayer
  rebuilds an identical transaction with an identical hash, so matching on hash
  alone resurrected the historical row.

Plus one from the ecosystem: alloy's `providers` feature builds a provider with
no transport, which fails at *runtime* with "no transports enabled" rather than
at compile time. That would have been a start-up failure in production.

### What Phase 3 found### What Phase 3 found

Three real defects, all caught by tests that drove real failures rather than
mocks:

- **A detached heartbeat loses jobs silently.** `tokio::spawn` detaches, so a
  worker whose future was dropped mid-attempt left a task behind renewing the
  lease for work that had stopped. The job looked permanently healthy to every
  other worker — never expired, so never reaped, never retried, simply lost. It
  was found sitting in `proving` with a lease five seconds in the future, 220
  seconds after its worker died.
- **Peak memory was never measured.** The field was documented as measured
  "when `/usr/bin/time` was available" and was hardcoded to `None`, so the
  memory ceiling derived from measurement in Phase 1 had nothing checking it was
  still right.
- **`f64::clamp` propagates NaN**, and `Duration::mul_f64` panics on it. A NaN
  jitter draw would have panicked a worker while it was scheduling a retry,
  failing far more jobs than whatever caused the retry.

## Next: Phase 4 — relayer and on-chain settlement

Two open questions need answers before it starts; both are in the section above.

### What Phase 3 has cost so far

Wiring the worker exposed a design error two phases old. ADR-003 had every
circuit accept its nullifier as public input 0, which was never exercised
because Phases 1 and 2 only ever used hand-committed reference witnesses. The
moment the worker had to *build* a witness from client inputs, it became clear
nobody could supply that nullifier: it is a Pedersen hash of private data, so
the client would need a proving stack to place an order and the worker would
need a second Pedersen implementation that agreed with Noir's exactly.

The same investigation found that `membership`'s registered input schema
omitted `root` entirely — no submittable job could ever have produced a
solvable witness.

Both are fixed under ADR-008: circuits return the nullifier, so it lands last
in the public input vector. It cost the circuits, the settlement contract, two
scripts, and 25 contract tests. The lesson is recorded here rather than only in
the ADR: a single job proved end to end during Phase 1 would have caught both
before anything was built on top of them.
