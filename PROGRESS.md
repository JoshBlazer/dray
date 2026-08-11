# Dray — Progress

**Current phase:** 0 — Foundations
**Last updated:** 2026-08-11
**Build status:** red — `cargo build` and `cargo test` have never been executed
on this machine. There is no C linker installed, so rustc cannot link. `cargo
fmt --check`, `cargo check`, and `cargo clippy -D warnings` all pass. See
*Blocked on* below.

## Phase status

| Phase | Name | Status | Exit criteria met | Notes |
|-------|------|--------|-------------------|-------|
| 0 | Foundations | in progress | no | Scaffolding written; build/test unverified locally, CI never run |
| 1 | Circuits and on-chain verification | not started | no | |
| 2 | Ingest API and durable job store | not started | no | |
| 3 | Proof worker pool | not started | no | |
| 4 | Relayer and on-chain settlement | not started | no | |
| 5 | Observability, operations, hardening | not started | no | |
| 6 | Documentation, demo, release | not started | no | |

## What works right now

Verified by execution on this machine:

- `cargo fmt --all --check` passes.
- `cargo check --workspace --all-targets` compiles all six crates.
- `cargo clippy --workspace --all-targets -- -D warnings` is clean.

That is the whole list. Everything else below is written but unexecuted.

## What does not work yet

- **`cargo build` and `cargo test` have not been run.** No C linker (`cc`) and
  no glibc development files are installed, so rustc cannot produce a binary.
  The trivial per-crate tests exist but have never passed, and Phase 0's exit
  criteria therefore are **not** met.
- **`make` is not installed**, so no Makefile target has been executed. The
  Makefile is written but unverified.
- **Docker is unavailable.** `/usr/bin/docker` symlinks into
  `/mnt/wsl/docker-desktop/` and returns an I/O error; Docker Desktop's WSL
  integration appears to be down. `docker-compose.yml` is written but has never
  been started, so "Postgres and Redis come up healthy" is a claim, not a fact.
- **CI has never run.** The workflow is committed but there is no remote and no
  push, so the badge-green exit criterion is unmet.
- Every crate is a skeleton: a component name, a doc comment describing what
  will live there, and one trivial test. No HTTP server, no schema, no state
  machine, no proving, no chain code.
- `circuits/`, `contracts/`, `migrations/`, `tests/`, and `docs/` are empty
  directories.
- `make e2e` deliberately exits non-zero with a message; it lands in Phase 4.

## Blocked on

1. **A C toolchain and `make`** — one command, needs root:
   `sudo apt-get update && sudo apt-get install -y build-essential`
   (`build-essential` brings gcc, libc6-dev, and make together.)
2. **Docker Desktop WSL integration** — start Docker Desktop on Windows and
   enable integration for this distro, or install `docker.io` and
   `docker-compose-v2` inside WSL. Needed before `make up` can be verified.
3. **A GitHub remote** — needed before CI can be observed green, which is part
   of Phase 0's exit criteria.

## Decisions made

| Date | Decision | Rationale | Alternatives rejected |
|------|----------|-----------|----------------------|
| 2026-08-11 | ADR-001: Rust (axum) for the API and scheduler tier — **accepted** | Spec §4.4 already places `dray-api` under `crates/`; one shared `dray-core` state machine instead of two implementations kept in sync | Go (closer to the author's prior `Sluice`, but adds a second toolchain and duplicates the domain model) |
| 2026-08-11 | Redis runs with persistence disabled in Compose | Redis is a cache, never truth. Making it non-durable in dev forces the recovery path to be exercised rather than assumed | Default RDB snapshotting, which would quietly let Redis become load-bearing |
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

1. Install `build-essential`, then run `make build` and `make test` and record
   the real result here.
2. Get Docker working, then verify `make up` brings Postgres and Redis to
   healthy.
3. Create the GitHub remote, push, and confirm CI is green on a fresh clone.
4. Only then mark Phase 0 done and begin Phase 1 (circuits) — installing
   `nargo`, `bb`, and Foundry is the first task there.
