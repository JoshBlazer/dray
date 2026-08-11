# Dray — Build Specification

> **Dray** *(n.)* — a low, sturdy cart without sides, used for hauling heavy loads
> over short distances. Work too heavy to happen where it is needed is done
> elsewhere and carried across.

---

## 0. How to use this document

**Read this section first and follow it literally.**

You are an LLM coding agent building this project from an empty repository to a
finished, public, portfolio-grade artifact. This document is your contract.

### Your role

- You are the **implementing engineer**. The human is the reviewer and the
  decision-maker on anything ambiguous.
- Work **phase by phase, in order**. Do not begin Phase N+1 until Phase N's exit
  criteria are met and its tests pass.
- **Never skip tests to make progress.** A phase with failing tests is not done.
- When the spec is ambiguous or you discover it is wrong, **stop and ask**. Do not
  silently invent requirements. Record the question in `PROGRESS.md` under
  *Open Questions* and surface it to the human.
- Prefer **boring, well-understood technology**. This project is evidence of
  engineering judgement, not novelty appetite.

### Progress tracking (required)

Create and maintain **`PROGRESS.md`** at the repository root from Phase 0 onward.
It is the single source of truth for state across sessions. You will lose context
between sessions; this file is how you recover it.

Update it **at the end of every working session** and **at every phase boundary**.

```markdown
# Dray — Progress

**Current phase:** 3 — Proof Worker Pool
**Last updated:** YYYY-MM-DD
**Build status:** green | red (reason)

## Phase status
| Phase | Name | Status | Exit criteria met | Notes |
|-------|------|--------|-------------------|-------|
| 0 | Foundations | done | yes | |
| 1 | Circuits | done | yes | Noir 1.x, bb backend |
| 2 | Ingest & Queue | in progress | no | retry policy unfinished |

## What works right now
- (Plain-English list a stranger could verify by cloning and running.)

## What does not work yet
- (Be honest. Known bugs, stubs, unimplemented paths.)

## Decisions made
| Date | Decision | Rationale | Alternatives rejected |
|------|----------|-----------|----------------------|

## Open questions (for the human)
- [ ] ...

## Next actions
1. ...
2. ...
```

**Rules for `PROGRESS.md`:**
- Never delete history from *Decisions made*. Append only.
- If you mark something `done`, a reviewer must be able to verify it by cloning
  the repo and running a documented command. If they cannot, it is not done.
- Do not describe intentions as accomplishments.

### Two other files you must maintain

- **`DECISIONS.md`** — Architecture Decision Records. One short entry per
  significant choice (proving backend, queue design, batching strategy). Format:
  Context / Decision / Consequences. Cross-reference from `PROGRESS.md`.
- **`README.md`** — written for a stranger evaluating the author's ability.
  See §10. Start it in Phase 0 and keep it truthful at all times.

---

## 1. What Dray is

Dray is a **distributed off-chain proof generation and relaying network**.

Zero-knowledge proof generation is computationally expensive — far too heavy to
run comfortably in a browser or on a phone. Dray moves that work off the client:
clients submit circuit inputs, a pool of Rust worker nodes generates proofs in
parallel, and a relayer submits the resulting proofs to an on-chain verifier
contract.

### The problem it solves

A dApp that wants client-side privacy faces a bad trade: prove in the browser and
accept 10–60 second freezes on mid-range hardware, or prove on a single server and
create a bottleneck and a single point of failure. Dray is the third option — a
durable, horizontally scalable proving tier with at-least-once delivery
guarantees and on-chain settlement.

### What it demonstrates (the portfolio purpose)

This project exists to prove four things simultaneously, which very few engineers
can show together:

1. **ZK circuit engineering** — writing and compiling real Noir circuits, and
   generating matching on-chain verifiers.
2. **Distributed systems design** — durable queueing, idempotency, worker
   coordination, failure recovery, backpressure.
3. **Off-chain to on-chain flow** — the full cryptographic path from client input
   to verified on-chain state change.
4. **Production instincts** — observability, resource bounding, graceful
   degradation, honest documentation.

> **Note to the implementing agent:** the author has already built `Sluice`, a
> distributed job scheduler in Go with Postgres + Redis and etcd leader election.
> Dray's queueing tier should be recognisably in that lineage — the same
> engineering values, applied to a new domain. Do not copy code; do reuse the
> reasoning.

---

## 2. Definition of done

Dray is complete when **all** of the following are true:

- [ ] A stranger can clone the repo and run the full stack locally with one
      documented command (`docker compose up` plus a seed script).
- [ ] A client can submit a proof request over HTTP and receive, asynchronously, a
      verified on-chain transaction hash on a public testnet.
- [ ] At least two distinct circuits are supported, demonstrating the system is
      circuit-agnostic rather than hardcoded to one.
- [ ] Killing a worker mid-proof loses no jobs; the job is retried and completes.
- [ ] Submitting the same request twice produces one on-chain settlement, not two.
- [ ] The verifier contract is deployed to a public testnet with a verified source
      and its address is in the README.
- [ ] Prometheus metrics and OpenTelemetry traces cover the full request path.
- [ ] Test suite passes in CI: unit, integration, property, and end-to-end.
- [ ] README contains an architecture diagram, a demo (asciinema or short video),
      and an honest *Limitations* section.
- [ ] `PROGRESS.md` shows all phases `done` with verifiable exit criteria.

**Explicit non-goals.** Do not build: a mainnet deployment, a token, a staking or
slashing mechanism, a decentralised trust model between untrusted provers, a
custom proving system, or a general-purpose blockchain indexer. Dray is a
trusted-operator proving tier. Say so plainly in the README — scoping honestly is
part of what the project demonstrates.

---

## 3. Technology choices

| Layer | Choice | Rationale |
|---|---|---|
| Circuits | **Noir** (`nargo`) | Author already claims Noir; ergonomic; first-class Solidity verifier generation |
| Proving backend | **Barretenberg** (`bb`) | Default Noir backend; UltraHonk / Plonk |
| Workers | **Rust** (tokio) | Author's target language; native process control and resource bounding |
| API & scheduler | **Rust** (axum) *or* **Go** | Pick one and record in `DECISIONS.md`. Rust keeps the stack single-language; Go replays Sluice's lineage more directly |
| Durable store | **PostgreSQL** | Source of truth for jobs; transactional state transitions |
| Hot path / leasing | **Redis** | Worker leases, rate limiting, ephemeral coordination |
| Chain | **EVM testnet** (Base Sepolia or Sepolia) | Noir generates Solidity verifiers directly |
| Contracts | **Solidity** + **Foundry** | Foundry's fuzzing is useful for the verifier harness |
| Observability | **Prometheus** + **OpenTelemetry** | Consistent with author's prior work |
| Containers | **Docker Compose** | One-command local bring-up |
| CI | **GitHub Actions** | Tests, lint, contract tests, circuit compilation |

**Constraint:** every component must run locally with no paid API keys. A
reviewer who cannot run it will not be impressed by it.

---

## 4. Architecture

### 4.1 Component overview

```
                  ┌──────────────┐
   client ───────►│  Ingest API  │  validate, hash, dedupe, enqueue
                  └──────┬───────┘
                         │
                  ┌──────▼───────┐
                  │  Job Store   │  PostgreSQL — durable source of truth
                  │  + Redis     │  leases, backpressure, rate limits
                  └──────┬───────┘
                         │  lease
        ┌────────────────┼────────────────┐
        │                │                │
  ┌─────▼─────┐    ┌─────▼─────┐    ┌─────▼─────┐
  │  Worker   │    │  Worker   │    │  Worker   │   Rust + nargo/bb
  │  (proof)  │    │  (proof)  │    │  (proof)  │   sandboxed, bounded
  └─────┬─────┘    └─────┬─────┘    └─────┬─────┘
        └────────────────┼────────────────┘
                         │  proof + public inputs
                  ┌──────▼───────┐
                  │   Relayer    │  batch, nonce mgmt, gas policy, submit
                  └──────┬───────┘
                         │
                  ┌──────▼───────┐
                  │   Verifier   │  Solidity, on-chain
                  │   Contract   │
                  └──────────────┘
```

### 4.2 Job lifecycle

```
queued ──► leased ──► proving ──► proved ──► submitting ──► settled
   │          │          │           │            │
   │          └──────────┴───────────┘            │
   │              (lease expiry → queued)         │
   │                                              │
   └──► rejected (validation)      failed ◄───────┘ (permanent)
```

State transitions happen **only** inside Postgres transactions. Redis holds
leases and may be rebuilt from Postgres at any time — treat it as a cache, never
as truth.

### 4.3 The four hard problems (and required approaches)

**a) Idempotency.** Each request carries a client-supplied `idempotency_key`. The
canonical job identity is `hash(circuit_id || canonicalised_inputs)`. Store a
unique index on it. A duplicate submission returns the existing job. On-chain,
the verifier contract must also reject replayed proofs via a nullifier or
consumed-commitment set — belt and braces, because at-least-once delivery means
the relayer *will* occasionally submit twice.

**b) Lease-based work distribution.** Workers lease jobs with a TTL rather than
holding locks. A crashed worker's lease expires and the job returns to `queued`.
Attempt count increments; jobs exceeding `max_attempts` move to `failed` with the
last error retained. This gives at-least-once delivery without leader election —
a deliberate simplification over Sluice, and one worth explaining in
`DECISIONS.md`.

**c) Resource bounding.** Proof generation can exhaust memory and hang. Every
proving subprocess must run with a wall-clock timeout, a memory ceiling, and a
CPU quota. Exceeding any bound is a normal, recoverable failure — not a crash.
Emit metrics for each. This is the single most important engineering detail in
the project; reviewers will look for it.

**d) Nonce and gas management.** The relayer is a single-writer to its own
account nonce. Serialise submissions per account. Implement gas price policy with
retry-with-bump on stuck transactions and a ceiling. Handle reorgs by confirming
to N blocks before marking `settled`.

### 4.4 Repository layout

```
dray/
├── README.md
├── PROGRESS.md
├── DECISIONS.md
├── docker-compose.yml
├── Makefile
├── circuits/
│   ├── membership/          # Noir circuit 1
│   └── range_proof/         # Noir circuit 2
├── contracts/               # Foundry project
│   ├── src/
│   │   ├── DrayVerifier.sol       # generated verifier (vendored)
│   │   └── DraySettlement.sol     # nullifier set + settlement logic
│   └── test/
├── crates/
│   ├── dray-api/            # ingest HTTP API
│   ├── dray-core/           # domain types, job state machine
│   ├── dray-store/          # Postgres + Redis adapters
│   ├── dray-worker/         # proving worker
│   ├── dray-relayer/        # chain submission
│   └── dray-cli/            # operator CLI
├── migrations/
├── tests/                   # cross-crate integration + e2e
└── .github/workflows/
```

---

## 5. Phases

Each phase lists **Goal**, **Tasks**, **Tests**, and **Exit criteria**. Do not
proceed past a phase whose exit criteria are unmet. Update `PROGRESS.md` at every
boundary.

### Phase 0 — Foundations

**Goal.** A repository that builds, tests, and lints on CI from day one.

**Tasks.**
1. Initialise the Cargo workspace with the crate skeleton from §4.4.
2. `docker-compose.yml` with Postgres and Redis; healthchecks on both.
3. `Makefile` targets: `setup`, `build`, `test`, `lint`, `up`, `down`, `e2e`.
4. GitHub Actions: build, `clippy -D warnings`, `cargo fmt --check`, test.
5. Create `PROGRESS.md`, `DECISIONS.md`, and a skeleton `README.md`.
6. ADR-001: language choice for the API/scheduler tier.

**Tests.** A trivial passing test in each crate, proving the harness works.

**Exit criteria.** CI is green on a fresh clone. `make up` starts dependencies and
`make test` passes with no local setup beyond Docker and Rust.

---

### Phase 1 — Circuits and on-chain verification

**Goal.** Prove the cryptographic path works end to end, in isolation, before any
distributed machinery exists. This phase de-risks the whole project.

**Tasks.**
1. Write circuit 1 — **Merkle membership**: prove a leaf belongs to a tree with a
   given root, without revealing the leaf. Public inputs: root, nullifier.
2. Write circuit 2 — **range proof**: prove a private value lies in `[min, max]`.
   Two circuits force the system to be circuit-agnostic from the start.
3. Generate proofs locally with `nargo` + `bb`. Record proving time and peak
   memory for each — you will need these numbers for capacity planning and for
   the README.
4. Generate the Solidity verifier for each circuit.
5. Write `DraySettlement.sol`: holds the verifier reference, maintains a
   nullifier set, rejects replays, emits a settlement event.
6. Deploy to a local Anvil chain.

**Tests.**
- Circuit unit tests in Noir (`nargo test`), covering valid and invalid witnesses.
- Foundry tests: valid proof verifies; tampered proof reverts; replayed nullifier
  reverts; malformed calldata reverts.
- Foundry fuzz test over public input space.
- A committed script that goes input → proof → on-chain verification on Anvil.

**Exit criteria.** `make e2e-circuits` proves and verifies both circuits against a
local chain, with no service tier involved. Proving time and memory are recorded
in `PROGRESS.md`.

> **Do not proceed until this phase is fully green.** Everything downstream
> assumes this works. Discovering a circuit problem in Phase 5 is expensive.

---

### Phase 2 — Ingest API and durable job store

**Goal.** Accept, validate, deduplicate, and durably persist proof requests.

**Tasks.**
1. Schema: `jobs`, `job_attempts`, `circuits`, `settlements`. Unique index on
   canonical job hash. Explicit state enum. `updated_at` triggers.
2. Migrations, applied automatically on service start in dev.
3. Implement the job state machine in `dray-core` as pure functions —
   `transition(state, event) -> Result<state>`. Keep it free of I/O so it can be
   exhaustively tested.
4. `POST /v1/proofs` — validate against the circuit's declared input schema,
   canonicalise inputs, hash, dedupe, enqueue. Return `202` with a job ID.
5. `GET /v1/proofs/{id}` — status, attempts, result, settlement tx hash.
6. Input validation: strict size caps, type checks, reject unknown circuits.
7. Structured logging with request IDs.

**Tests.**
- Unit: exhaustive state machine transition table, including every illegal
  transition.
- Unit: input canonicalisation is stable under key reordering and whitespace.
- Integration (real Postgres): duplicate submissions create exactly one job;
  concurrent identical submissions likewise (run 50 in parallel).
- Integration: oversized and malformed payloads are rejected with useful errors.
- Property test: any sequence of valid events leaves the job in a legal state.

**Exit criteria.** Requests persist across a full service restart. Concurrent
duplicate submissions provably create one job.

---

### Phase 3 — Proof worker pool

**Goal.** Workers lease jobs, generate proofs under strict resource bounds, and
survive being killed.

**Tasks.**
1. Lease acquisition: `SELECT ... FOR UPDATE SKIP LOCKED` in Postgres, with the
   lease TTL mirrored into Redis for fast liveness checks.
2. Lease renewal heartbeat during long proofs; expiry returns the job to `queued`.
3. Subprocess execution of `nargo`/`bb` with:
   - wall-clock timeout,
   - memory ceiling (cgroups or `setrlimit`),
   - CPU quota,
   - a scratch directory cleaned up on every exit path, including panic.
4. Attempt accounting; exponential backoff with jitter; `max_attempts` → `failed`.
5. Graceful shutdown: on SIGTERM, stop leasing, finish or cleanly abandon the
   current job, release leases.
6. Prometheus metrics: queue depth, lease age, proving duration histogram, peak
   memory, timeout count, OOM count, attempt distribution.

**Tests.**
- Integration: `SIGKILL` a worker mid-proof → job is retried and completes.
- Integration: two workers never hold the same job simultaneously (assert via a
  contention test with N workers and M jobs).
- Integration: a circuit input engineered to exceed the memory bound fails
  cleanly, is marked, and does not take the worker down.
- Integration: timeout path releases the lease and cleans scratch space.
- Load: 100 queued jobs across 4 workers complete with zero loss and zero
  duplicates.

**Exit criteria.** Chaos test passes — randomly kill workers during a 100-job run;
every job settles exactly once in the store, none are lost.

---

### Phase 4 — Relayer and on-chain settlement

**Goal.** Get proofs on chain, exactly once, under adverse conditions.

**Tasks.**
1. Single-writer nonce management per relayer account; serialised submission.
2. Gas policy: estimate, cap, bump-and-replace on stuck transactions.
3. Confirmation tracking to N blocks; reorg handling reverts `settled` → `proved`
   and resubmits.
4. Optional batching: multiple proofs per transaction where the contract supports
   it. Measure and record the gas saving — a concrete number belongs in the README.
5. Relayer authentication on chain: settlement accepts submissions only from
   authorised relayer addresses.
6. Failure taxonomy: distinguish permanent (invalid proof) from transient (RPC
   down, nonce gap) and retry only the latter.

**Tests.**
- Integration against Anvil: happy path settles and emits the expected event.
- Integration: forced reorg on Anvil → state correctly reverts and resubmits.
- Integration: RPC returning errors → transient classification, retry, eventual
  settlement.
- Integration: replayed proof → contract rejects, job marked appropriately, no
  double settlement.
- Integration: nonce gap recovery.

**Exit criteria.** A job submitted at the API arrives as a verified on-chain
settlement on a public testnet. Transaction hash recorded in `PROGRESS.md`.

---

### Phase 5 — Observability, operations, and hardening

**Goal.** Make it legible and operable — the part most portfolio projects skip,
and therefore the part that differentiates this one.

**Tasks.**
1. OpenTelemetry traces spanning ingest → lease → prove → submit → settle, with
   the job ID as a correlating attribute across all services.
2. Prometheus metrics for every tier; a committed Grafana dashboard JSON.
3. Health and readiness endpoints on all services.
4. Backpressure: reject new work with `503` above a configured queue depth.
5. Per-client rate limiting in Redis.
6. Operator CLI: inspect a job, replay a failed job, drain a worker, show queue
   stats.
7. Security pass: secret handling (never log keys), input size limits, dependency
   audit (`cargo audit`) wired into CI, and a documented threat model covering
   what a malicious client can and cannot do.

**Tests.**
- Integration: a trace for one job contains spans from all four tiers.
- Integration: backpressure triggers and recovers.
- Integration: rate limiting rejects correctly and resets.
- CI: `cargo audit` gates the build.

**Exit criteria.** A reviewer can start the stack, submit a job, and watch it
traverse every tier in Grafana and in a trace viewer.

---

### Phase 6 — Documentation, demo, and release

**Goal.** Convert working software into something that convinces a stranger in
five minutes.

**Tasks.**
1. README per §10.
2. Architecture diagram — committed source (Mermaid or D2), not just an image.
3. `docs/DESIGN.md`: the four hard problems from §4.3 and how each was solved.
4. `docs/BENCHMARKS.md`: proving times, memory, throughput at N workers, gas per
   settlement, batching saving. Real measured numbers, with methodology.
5. Demo: asciinema recording or a 2–3 minute video, submission through to the
   testnet explorer link.
6. Tag `v1.0.0`. Public testnet deployment addresses in the README.
7. Final `PROGRESS.md` update: all phases `done`, limitations documented honestly.

**Exit criteria.** Someone unfamiliar with the project can read the README, run
it, and explain what it does and why it is hard.

---

## 6. Testing strategy

| Level | Scope | Tooling | Gate |
|---|---|---|---|
| Circuit | Witness validity, constraint soundness | `nargo test` | Phase 1 |
| Contract unit | Verifier + settlement logic | Foundry | Phase 1 |
| Contract fuzz | Public input space, replay resistance | Foundry fuzz | Phase 1 |
| Rust unit | State machine, canonicalisation, policy | `cargo test` | Phase 2+ |
| Property | State machine invariants | `proptest` | Phase 2 |
| Integration | Real Postgres, Redis, Anvil | `cargo test --test` | Phase 2+ |
| Chaos | Worker kills, lease expiry, reorgs | Custom harness | Phase 3, 4 |
| Load | Throughput, no loss, no duplicates | Custom harness | Phase 3 |
| End-to-end | API → chain, public testnet | `make e2e` | Phase 4 |

**Invariants that must hold under every test.** Write these as explicit assertions
in the chaos and load harnesses:

1. No job is lost. Every accepted job reaches a terminal state.
2. No job settles twice on chain.
3. No two workers hold the same lease simultaneously.
4. Postgres state is always recoverable without Redis.
5. No proving subprocess outlives its bounds.

**Coverage is not a target.** Do not chase a percentage. Test the invariants, the
failure paths, and the state machine exhaustively; skip trivial getters.

---

## 7. What "good" looks like in review

A reviewer forms a judgement in about ninety seconds. Optimise for that.

- The README opens with what it does and why it is hard, not with installation.
- The architecture diagram is above the fold.
- `docs/BENCHMARKS.md` has real numbers. Measured beats claimed, every time.
- The *Limitations* section is honest and specific. This reads as senior; its
  absence reads as junior.
- Commit history shows coherent, reviewable increments — not one "initial commit"
  containing everything.
- CI badge is green and the badge links to real runs.

---

## 8. Session protocol for the implementing agent

At the **start** of every session:
1. Read `PROGRESS.md` fully.
2. Read `DECISIONS.md`.
3. Run `make test`. If red, fixing it is the session's first task.
4. State which phase you are in and what you intend to do.

At the **end** of every session:
1. Ensure the build is green, or record precisely why it is not.
2. Update `PROGRESS.md`: status, what works, what does not, decisions, next
   actions.
3. Commit with a message describing the change and the phase.

**Never** mark a phase done without running its tests. **Never** write to
`PROGRESS.md` that something works without having executed it.

---

## 9. Open questions for the human (resolve before Phase 2)

1. API/scheduler tier in Rust or Go? (Rust = one language; Go = closer to Sluice.)
2. Target testnet — Base Sepolia or Ethereum Sepolia?
3. Is batching in scope for v1.0, or deferred to v1.1?
4. Should the relayer be a single trusted operator (simpler, honest) or a small
   permissioned set (more impressive, considerably more work)?

Record answers in `DECISIONS.md` before proceeding.

---

## 10. README requirements

The README is the deliverable most likely to be read and least likely to be
written well. It must contain, in this order:

1. **One sentence** on what Dray is.
2. **The problem** — three or four sentences on why client-side proving is
   painful, with a concrete number.
3. **Architecture diagram**, inline.
4. **Quickstart** — clone to first settled proof, one command block.
5. **How it works** — the four hard problems and the approach to each.
6. **Benchmarks** — a small table, linking to `docs/BENCHMARKS.md`.
7. **Deployed addresses** — testnet, with explorer links.
8. **Limitations** — what it does not do, and what it would take. Include the
   trusted-operator model explicitly.
9. **Development** — how to run tests, how to add a circuit.

Do not pad it. Do not use marketing language. The reader is an engineer deciding
whether the author is worth interviewing.
