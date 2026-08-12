# Dray — Progress

**Current phase:** 2 — Ingest API and durable job store (complete) → 3 — Worker pool
**Last updated:** 2026-08-12
**Build status:** green — `make build`, `make test`, and `make lint` pass
locally, and CI is green on a fresh clone across all five jobs.
Repository: <https://github.com/JoshBlazer/dray>

## Phase status

| Phase | Name | Status | Exit criteria met | Notes |
|-------|------|--------|-------------------|-------|
| 0 | Foundations | done | yes | CI green on a fresh clone (run 31593551054). One caveat on `make up` — see below |
| 1 | Circuits and on-chain verification | done | yes | `make e2e-circuits` proves both circuits and settles them on Anvil |
| 2 | Ingest API and durable job store | done | yes | Verified in CI against real Postgres; not yet locally (no Docker) |
| 3 | Proof worker pool | not started | no | Chaos tests need local Docker |
| 4 | Relayer and on-chain settlement | not started | no | |
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

**Caveat on `make up`.** The Compose stack is verified by CI on a fresh clone,
but the `make up` target itself has never been executed on the author's
workstation, because Docker is broken there (see below). The target is a
one-line wrapper around the exact command CI runs. Flagged rather than glossed.

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

**Verified in CI, not locally.** Everything database-backed runs in the
`Migrations and store integration` job. The author's workstation still has no
Docker, so `make test-integration` has never been executed there.

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

- **Docker is unavailable on the author's workstation**, so `make up` has never
  been run there. This is an environment fault, not a project one — CI proves
  the Compose stack comes up healthy from a fresh clone. Diagnosis:
  `/mnt/wsl/docker-desktop/cli-tools` is a read-only loopback ISO mount
  (`/dev/loop0`) that is empty, so `/usr/bin/docker` — a symlink into it —
  returns an I/O error. Restarting Docker Desktop did not clear it. The fix
  chosen is to install Docker natively inside WSL rather than depend on
  Desktop's integration; systemd is PID 1 here, so it will run as a service.
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

- **The ADR-003 convention is not compiler-enforced.** A new circuit that
  declares its public inputs in the wrong order would have `DraySettlement`
  read some other field as a nullifier. `test_public_input_layout_matches_adr_003`
  asserts it for the two circuits that exist; there is nothing stopping a third
  from getting it wrong. This belongs in `docs/DESIGN.md`.
- **Verifier bytecode is 18.1 KB against a 24,576 B limit.** Roughly 6.4 KB of
  headroom. A materially larger circuit could exceed EIP-170, and the failure
  would appear at deployment, not at compile time.
- **`bb` proving is unsandboxed today.** It is invoked directly by scripts.
  Nothing yet stops a pathological input from exhausting memory.

## Blocked on

1. **Docker on the author's workstation** — needs
   `sudo apt-get install -y docker.io docker-compose-v2 && sudo systemctl
   enable --now docker`. Not blocking Phase 0, which CI closed out, but it will
   block Phase 3's chaos tests and Phase 4's Anvil integration tests, which
   have to run locally.

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
| 2026-08-12 | ADR-003: every circuit declares `nullifier` as public input 0 | Lets `DraySettlement` find the nullifier without knowing the circuit, which is what makes it genuinely circuit-agnostic | Per-circuit adapters in the contract (a code change per circuit), or a nullifier passed alongside the proof (unbound, so forgeable) |
| 2026-08-12 | ADR-004: batching deferred to v1.1 | Decided by the human. Keeps the v1.0 contract surface small and the replay tests focused | Batching in v1.0, which would mean choosing between an aggregation circuit and a verification loop under time pressure |
| 2026-08-12 | Pedersen for hashing, not Poseidon | Poseidon is not in this Noir's stdlib and would mean an external dependency; `std::hash::pedersen_hash` is built in | Poseidon2 via an external package, adding a dependency to pin and audit for no benefit at this stage |
| 2026-08-12 | `wouldSettle` catches verifier reverts and returns false | Found by a fuzz test: the generated verifier reverts rather than returning false, which made pre-flight useless for its one job | Letting the revert propagate, which would give the relayer an exception instead of an answer |

## Open questions (for the human)

- [x] ~~ADR-001 — confirm Rust for the API tier.~~ Answered 2026-08-11: Rust
      with axum. See ADR-001.
- [ ] Target testnet: Base Sepolia or Ethereum Sepolia? *(Needed by Phase 4.)*
- [x] ~~Is proof batching in scope for v1.0?~~ Answered 2026-08-12: deferred to
      v1.1. See ADR-004. `DraySettlement.sol` exposes single-proof settlement
      only.
- [ ] Single trusted relayer operator, or a small permissioned set? *(Needed by
      Phase 4.)*

## Next actions

Phase 2 is closed. Phase 3 — the proof worker pool — is next, and it is the
phase the spec calls out as carrying the single most important engineering
detail: resource bounding.

1. Lease acquisition with `SELECT ... FOR UPDATE SKIP LOCKED`, mirrored into
   Redis for fast liveness checks. Redis becomes load-bearing for the first
   time — and must stay rebuildable from Postgres.
2. Lease renewal heartbeat during long proofs; expiry returns the job to
   `queued`. The state machine already models this; nothing drives it yet.
3. Subprocess execution of `nargo`/`bb` under a wall-clock timeout, a memory
   ceiling, and a CPU quota, with a scratch directory cleaned up on every exit
   path including panic.
4. Attempt accounting with exponential backoff and jitter. `classify_failure`
   in `dray-core` already decides retry-versus-fail; the worker has to call it.
5. Graceful shutdown on SIGTERM: stop leasing, finish or cleanly abandon the
   current job, release leases.
6. Prometheus metrics: queue depth, lease age, proving duration, peak memory,
   timeouts, OOMs, attempt distribution.

**Exit criterion:** a chaos test that randomly kills workers during a 100-job
run, after which every job has settled exactly once in the store and none are
lost.

Two things carried forward from earlier phases that Phase 3 depends on:

- `bb` needs the verification key on disk before it will prove, so the worker
  must generate the vk once per circuit at registration rather than per job.
- Measured proving cost is 1.9–2.5 s and ~42 MB peak RSS per proof on a
  4-core box. The default memory ceiling and timeout should be derived from
  those numbers rather than guessed, with enough headroom that a normal proof
  never trips them.

**Docker is required for this phase.** The chaos and contention tests need a
real Postgres and Redis locally; CI alone cannot substitute, because killing
worker processes mid-proof is the point of the exercise.
