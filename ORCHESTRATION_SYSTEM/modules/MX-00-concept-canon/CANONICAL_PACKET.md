# MX-00 Canonical Packet

Module: **Concept and SKF Canon**

## Responsibility

Govern the frozen MemoryX concept and trace implementation claims to canonical requirements without editing production Rust.

## Canonical Authorities

- `Concept/SKF.txt`
- `Concept/SKF-1.1 Implementer-Ready Spec.txt`
- `Concept/Расширение.txt`
- `CONCEPT_EXTENSION_IMPLEMENTATION_PLAN.md`

The module must read the relevant canonical passages and the current roadmap
before changing implementation. It must stop on a concept conflict rather than
silently reinterpret the concept or mark an open roadmap gate complete.

## Primary Ownership

- `Concept/`
- `CONCEPT_EXTENSION_IMPLEMENTATION_PLAN.md`

## Shared Surfaces

- `README concept claims`
- `cross-module concept conformance decisions`

Shared surfaces require an explicit handoff recorded in `DECISIONS.md` and in
the EvidenceReturn of every affected module.

## Forbidden Ownership

- `src/`
- `tests/`

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
