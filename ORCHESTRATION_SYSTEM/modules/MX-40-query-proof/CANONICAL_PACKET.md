# MX-40 Canonical Packet

Module: **Query Contracts and Proof Assembly**

## Inter-Agent Communication

Inter-agent language: English only.

Prompts, task packets, plans, progress and decision narratives, handoffs,
EvidenceReturn narratives, and compact recovery instructions must be English.
User-facing text may follow the user's language but must be translated before
it enters an inter-agent artifact. Exact technical identifiers listed by the
root language contract remain unchanged.

## Responsibility

Own QueryContract compilation, deterministic routing, backward-forward reasoning, fixed-point solving, budgets and minimal AnswerGraph construction.

## Canonical Authorities

- `Concept/SKF.txt#11-heptapod-style-two-way-reasoning`
- `Concept/SKF.txt#12-fixed-point-answering`
- `Concept/Расширение.txt`

The module must read the relevant canonical passages and the current roadmap
before changing implementation. It must stop on a concept conflict rather than
silently reinterpret the concept or mark an open roadmap gate complete.

## Primary Ownership

- `src/query/`

## Shared Surfaces

- `MemoryX answer/query entrypoints in src/store/api.rs`
- `source-bearing graph projection with MX-50`
- `query JSON schemas with MX-80`

Shared surfaces require an explicit handoff recorded in `DECISIONS.md` and in
the EvidenceReturn of every affected module.

## Forbidden Ownership

- `src/crdt/`
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
