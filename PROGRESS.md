# Dray — Progress

**Current phase:** 3 — Proof worker pool (in progress)
**Last updated:** 2026-08-17
**Build status:** green — `make build`, `make test`, and `make lint` pass
locally, and CI is green on a fresh clone across all five jobs.
Repository: <https://github.com/JoshBlazer/dray>

## Phase status

| Phase | Name | Status | Exit criteria met | Notes |
|-------|------|--------|-------------------|-------|
| 0 | Foundations | done | yes | CI green on a fresh clone (run 31593551054). One caveat on `make up` — see below |
| 1 | Circuits and on-chain verification | done | yes | `make e2e-circuits` proves both circuits and settles them on Anvil |
| 2 | Ingest API and durable job store | done | yes | Verified in CI against real Postgres; not yet locally (no Docker) |
| 3 | Proof worker pool | in progress | no | Leasing, resource bounds, and the proving pipeline done; lease loop, shutdown, metrics, and the chaos test remain |
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

Phase 3 — the proof worker pool — is under way. It is the phase the spec calls
out as carrying the single most important engineering detail: resource
bounding.

Done:

1. **Lease acquisition** with `SELECT ... FOR UPDATE SKIP LOCKED`, plus
   renewal and reaping. 12 store integration tests, headlined by twenty
   workers racing for ten jobs. The attempt counter increments *at lease
   time*, because a worker killed mid-proof never reports anything and a
   counter advanced only on completion would let a poison job retry for ever.
2. **Resource-bounded subprocess execution** — wall clock, `RLIMIT_AS`, and
   `RLIMIT_CPU`, applied through `sh`'s `ulimit` before `exec` so the
   workspace's `unsafe_code = "forbid"` survives. Scratch cleanup lives in
   `Drop`, so it runs on panic too. 16 tests driving real failures rather than
   mocks.
3. **The proving pipeline** — `Prover.toml` from job inputs, `nargo execute`,
   `bb prove -t evm`, both bounded. Each job gets a private copy of the circuit
   package; `nargo` uses fixed paths inside the package directory, so a shared
   one would let concurrent jobs overwrite each other's witnesses and produce a
   proof recorded against the wrong inputs. 10 integration tests against the
   real toolchain assert the worker reproduces the exact nullifiers
   `e2e-circuits.sh` settles on chain.
4. **Exponential backoff with jitter**, drawn from `[base/2, base]` so a shared
   outage does not resynchronise the fleet on every subsequent attempt.

Remaining:

5. The lease loop itself: lease, heartbeat during the proof, record the result.
   Losing a renewal must abandon the work rather than race the new owner.
6. Graceful shutdown on SIGTERM: stop leasing, finish or cleanly abandon the
   current job, release leases so the next worker need not wait out the TTL.
7. Prometheus metrics: queue depth, lease age, proving duration, peak memory,
   timeouts, OOMs, attempt distribution.
8. Mirror lease TTL into Redis for fast liveness checks. Redis becomes
   load-bearing for the first time — and must stay rebuildable from Postgres.

**Exit criterion:** a chaos test that randomly kills workers during a 100-job
run, after which every job has settled exactly once in the store and none are
lost.

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
