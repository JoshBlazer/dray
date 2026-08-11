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
