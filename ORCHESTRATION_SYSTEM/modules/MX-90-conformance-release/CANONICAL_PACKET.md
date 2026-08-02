# MX-90 Canonical Packet

Module: **Conformance, Benchmarks and Release**

## Responsibility

Own independent conformance suites, adversarial and migration fixtures, honest benchmarks, CI gates, packaging and release evidence without editing production Rust.

## Canonical Authorities

- `README.md`
- `CONCEPT_EXTENSION_IMPLEMENTATION_PLAN.md`
- `ORCHESTRATION_PLAN.md`

The module must read the relevant canonical passages and the current roadmap
before changing implementation. It must stop on a concept conflict rather than
silently reinterpret the concept or mark an open roadmap gate complete.

## Primary Ownership

- `tests/conformance/`
- `benches/`
- `benchmarks/`
- `.github/workflows/`
- `CHANGELOG.md`
- `SECURITY.md`

## Shared Surfaces

- `repository test files remain owned by their production modules`
- `release documentation with MX-70 and MX-80`

Shared surfaces require an explicit handoff recorded in `DECISIONS.md` and in
the EvidenceReturn of every affected module.

## Forbidden Ownership

- `src/`

## Immutable Execution

- Model: `gpt-5.6-sol`
- Reasoning effort: `xhigh`
- `max`: forbidden
- Bound sessions resume only through `codex exec resume <UUID>` with model and
  reasoning reasserted.
- An empty `session_id.txt` means `UNBOUND`; no script may invent a UUID.

## Non-Regression

Atoms, contexts and conflicts, Heptapod backward+forward reasoning,
FixedPointSolver, minimal AnswerGraph, provenance federation, CAS/Merkle,
CRDT/WAL/repair, full MCP, and explicit local storage scopes cannot be removed
or bypassed. N5 remains open until its own acceptance evidence passes.
