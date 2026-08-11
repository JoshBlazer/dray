# Dray — Progress

**Current phase:** 0 — Foundations
**Last updated:** 2026-08-11
**Build status:** green locally — `make build`, `make test`, and `make lint` all
pass. Not yet green in CI, which has never run: there is no remote to push to.
`make up` is still unverified because Docker is unavailable on this machine.

## Phase status

| Phase | Name | Status | Exit criteria met | Notes |
|-------|------|--------|-------------------|-------|
| 0 | Foundations | in progress | no | Build, test, and lint verified locally. Outstanding: `make up` (no Docker) and CI green on a fresh clone (no remote) |
| 1 | Circuits and on-chain verification | not started | no | |
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

- **Docker is unavailable, so `make up` has never run.** `docker-compose.yml`
  is written but has never been started — "Postgres and Redis come up healthy"
  remains a claim, not a fact, and it is part of Phase 0's exit criteria.
  Diagnosis: `/mnt/wsl/docker-desktop/cli-tools` is a read-only loopback ISO
  mount (`/dev/loop0`) that is now empty, so `/usr/bin/docker` — a symlink into
  it — returns an I/O error. Docker Desktop was shut down on the Windows host
  and left the mount stale. Restarting Docker Desktop, after a
  `wsl --shutdown` if the mount persists, should restore it.
- **CI has never run.** The workflow is committed but there is no remote and no
  push, so the badge-green exit criterion is unmet.
- Every crate is a skeleton: a component name, a doc comment describing what
  will live there, and one trivial test. No HTTP server, no schema, no state
  machine, no proving, no chain code.
- `circuits/`, `contracts/`, `migrations/`, `tests/`, and `docs/` are empty
  directories.
- `make e2e` deliberately exits non-zero with a message; it lands in Phase 4.

## Blocked on

1. **Docker** — start Docker Desktop on the Windows host with WSL integration
   enabled for this distro (`wsl --shutdown` first if the stale mount
   persists), or install `docker.io` and `docker-compose-v2` inside WSL
   instead. Needed before `make up` can be verified.
2. **A GitHub remote** — needed before CI can be observed green, which is part
   of Phase 0's exit criteria.

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

1. Get Docker working, then verify `make up` brings Postgres and Redis to
   healthy.
2. Create the GitHub remote, push, and confirm CI is green on a fresh clone.
3. Wire the ADR-002 pinned versions into `make setup` so the toolchain installs
   reproducibly rather than by hand, and add them to the README prerequisites.
4. Only then mark Phase 0 done and begin Phase 1 (circuits). The toolchain
   install that would have been Phase 1's first task is already done and
   validated; Phase 1 starts directly at writing the Merkle membership circuit.
