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
**Status:** accepted

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
