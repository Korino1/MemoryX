# MX-10 Canonical Packet

Module: **CAS, Canonical IDs, Storage and Indexes**

## Responsibility

Own canonical identity, CAS encoding and IO, physical layout, location and retrieval indexes, compaction and portable storage performance.

## Canonical Authorities

- `Concept/SKF.txt`
- `Concept/SKF-1.1 Implementer-Ready Spec.txt`
- `docs/PORTABLE_CPU_BUILDS.md`

The module must read the relevant canonical passages and the current roadmap
before changing implementation. It must stop on a concept conflict rather than
silently reinterpret the concept or mark an open roadmap gate complete.

## Primary Ownership

- `src/cas/`
- `src/index/`
- `src/utils/io.rs`
- `src/utils/cpu.rs`

## Shared Surfaces

- `CAS evidence encoding with MX-50`
- `durability primitives with MX-20`
- `schema encodings with MX-80`

Shared surfaces require an explicit handoff recorded in `DECISIONS.md` and in
the EvidenceReturn of every affected module.

## Forbidden Ownership

- `src/query/`
- `src/bin/memoryx.rs`

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
