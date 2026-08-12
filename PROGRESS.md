# Dray — Progress

**Current phase:** 0 — Foundations (complete) → 1 — Circuits
**Last updated:** 2026-08-12
**Build status:** green — `make build`, `make test`, and `make lint` pass
locally, and CI is green on a fresh clone across all three jobs.
Repository: <https://github.com/JoshBlazer/dray>

## Phase status

| Phase | Name | Status | Exit criteria met | Notes |
|-------|------|--------|-------------------|-------|
| 0 | Foundations | done | yes | CI green on a fresh clone (run 31593551054). One caveat on `make up` — see below |
| 1 | Circuits and on-chain verification | not started | no | Toolchain already installed and proof-verified during Phase 0 |
| 2 | Ingest API and durable job store | not started | no | |
| 3 | Proof worker pool | not started | no | |
| 4 | Relayer and on-chain settlement | not started | no | |
| 5 | Observability, operations, hardening | not started | no | |
| 6 | Documentation, demo, release | not started | no | |

## What works right now

Verified by execution on this machine, via the Makefile targets a reviewer
would actually run:

- `make setup` succeeds — toolchain components present, dependencies fetched.
- `make build` compiles all six crates.
- `make test` passes: 7 tests across 6 crates, 0 failures.
- `make lint` is clean — `cargo fmt --check` and `cargo clippy -D warnings`.
- `make versions` reports the installed proving toolchain matching the ADR-002
  pins exactly.

And on CI, from a fresh clone (run 31593551054, all three jobs green):

- Build and test — 11 s.
- Format and clippy — 14 s.
- Dependencies start healthy — 18 s. This runs `docker compose up -d --wait`,
  so Postgres and Redis reaching a healthy state is now a verified fact rather
  than a claim.

**Caveat on `make up`.** The Compose stack is verified by CI on a fresh clone,
but the `make up` target itself has never been executed on the author's
workstation, because Docker is broken there (see below). The target is a
one-line wrapper around the exact command CI runs. Flagged rather than glossed.

The proving toolchain is installed, version-pinned per ADR-002, and validated
end to end on a throwaway circuit outside this repository:

- `nargo` 1.0.0-beta.22, `bb` 5.0.0-nightly.20260522, `forge`/`anvil` 1.7.1 all
  execute.
- `nargo execute` → `bb write_vk` → `bb prove` → `bb verify` completes and
  reports **"Proof verified successfully"**.

That is the whole list. Everything else below is written but unexecuted.

### Toolchain notes worth keeping

Learned while validating, and relevant to Phase 1 and Phase 3:

- **`bb` 5.x requires the verification key before proving.** `bb prove` fails
  with *"Unable to open file: ./target/vk"* unless `bb write_vk` has run first.
  The worker's proving sequence must account for this; the vk is per-circuit
  and should be generated once at circuit registration, not per job.
- The default scheme is **UltraHonk**.
- Reference numbers for a trivial circuit (`assert(x != y)`), which exist only
  to bound expectations — the real Phase 1 measurements will be far larger:
  `write_vk` 2.26 s / 29 MB peak RSS, `prove` 0.19 s / 20 MB peak RSS, proof
  size 14,656 bytes.
- `/usr/bin/time -v` is available and is how peak RSS above was captured; the
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
- Every crate is a skeleton: a component name, a doc comment describing what
  will live there, and one trivial test. No HTTP server, no schema, no state
  machine, no proving, no chain code.
- `circuits/`, `contracts/`, `migrations/`, `tests/`, and `docs/` are empty
  directories.
- `make e2e` deliberately exits non-zero with a message; it lands in Phase 4.

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

## Open questions (for the human)

- [x] ~~ADR-001 — confirm Rust for the API tier.~~ Answered 2026-08-11: Rust
      with axum. See ADR-001.
- [ ] Target testnet: Base Sepolia or Ethereum Sepolia? *(Needed by Phase 4.)*
- [ ] Is proof batching in scope for v1.0, or deferred to v1.1? *(Needed by
      Phase 4; affects the settlement contract's interface, so worth answering
      before Phase 1 finalises `DraySettlement.sol`.)*
- [ ] Single trusted relayer operator, or a small permissioned set? *(Needed by
      Phase 4.)*

## Next actions

Phase 0 is closed. Phase 1 — circuits and on-chain verification — is next, and
the spec is emphatic that it must be fully green before anything downstream
starts, because discovering a circuit problem in Phase 5 is expensive.

1. Write the Merkle membership circuit: prove a leaf belongs to a tree with a
   given root without revealing the leaf. Public inputs: root, nullifier.
2. Write the range proof circuit: prove a private value lies in `[min, max]`.
   Two circuits are what force the system to be circuit-agnostic from the start.
3. `nargo test` on both, covering valid *and* invalid witnesses.
4. Measure proving time and peak memory for each, and record them here. These
   numbers drive capacity planning and belong in the README.
5. Generate the Solidity verifier for each circuit.
6. Write `DraySettlement.sol` — verifier reference, nullifier set, replay
   rejection, settlement event.
7. Foundry tests: valid proof verifies, tampered proof reverts, replayed
   nullifier reverts, malformed calldata reverts, plus a fuzz test over the
   public input space.
8. `make e2e-circuits` — input → proof → on-chain verification against Anvil,
   with no service tier involved.

Docker is not needed for Phase 1 (Anvil runs natively), so the local Docker gap
does not block this work. It will block Phase 3.
