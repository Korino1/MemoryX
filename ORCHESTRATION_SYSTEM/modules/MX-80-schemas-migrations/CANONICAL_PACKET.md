# MX-80 Canonical Packet

Module: **Schemas, Migrations and Interoperability**

## Responsibility

Own published schemas, version compatibility, durable format validation, legacy migration contracts and interoperable import/export envelopes.

## Canonical Authorities

- `Concept/SKF-1.1 Implementer-Ready Spec.txt`
- `README.md`

The module must read the relevant canonical passages and the current roadmap
before changing implementation. It must stop on a concept conflict rather than
silently reinterpret the concept or mark an open roadmap gate complete.

## Primary Ownership

- `schemas/`

## Shared Surfaces

- `serialization types across src`
- `migration implementations with MX-10 and MX-20`
- `MCP schemas with MX-70`

Shared surfaces require an explicit handoff recorded in `DECISIONS.md` and in
the EvidenceReturn of every affected module.

## Forbidden Ownership

- `src/query/solver.rs`
- `src/context/manager.rs`

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
