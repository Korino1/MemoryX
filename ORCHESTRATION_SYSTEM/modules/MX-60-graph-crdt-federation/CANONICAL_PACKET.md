# MX-60 Canonical Packet

Module: **Graph, CRDT and Federation**

## Responsibility

Own graph storage and traversal, CRDT convergence, replication, snapshots and federation over claims, provenance and metadata.

## Canonical Authorities

- `Concept/SKF.txt#16-federation`
- `Concept/SKF.txt#19-merkle-integrity-crdt-replication-and-repair`
- `Concept/SKF-1.1 Implementer-Ready Spec.txt`

The module must read the relevant canonical passages and the current roadmap
before changing implementation. It must stop on a concept conflict rather than
silently reinterpret the concept or mark an open roadmap gate complete.

## Primary Ownership

- `src/graph/`
- `src/crdt/`
- `src/federation/`

## Shared Surfaces

- `snapshot identity with MX-10 and MX-20`
- `federated QueryContract planning with MX-40`
- `wire schemas with MX-80`

Shared surfaces require an explicit handoff recorded in `DECISIONS.md` and in
the EvidenceReturn of every affected module.

## Forbidden Ownership

- `src/bin/memoryx.rs`
- `src/context/`

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
