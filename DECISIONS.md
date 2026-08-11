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
