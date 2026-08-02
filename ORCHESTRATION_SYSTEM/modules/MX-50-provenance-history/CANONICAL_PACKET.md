# MX-50 Canonical Packet

Module: **Provenance, Evidence and History**

## Inter-Agent Communication

Inter-agent language: English only.

Prompts, task packets, plans, progress and decision narratives, handoffs,
EvidenceReturn narratives, and compact recovery instructions must be English.
User-facing text may follow the user's language but must be translated before
it enters an inter-agent artifact. Exact technical identifiers listed by the
root language contract remain unchanged.

## Responsibility

Own source registration, evidence identity, multi-source atom attachment, provenance traversal, supersession, tombstones and durable history semantics.

## Canonical Authorities

- `Concept/Расширение.txt`
- `Concept/SKF.txt#20-ingest`

The module must read the relevant canonical passages and the current roadmap
before changing implementation. It must stop on a concept conflict rather than
silently reinterpret the concept or mark an open roadmap gate complete.

## Primary Ownership

- `src/ingest/`

## Shared Surfaces

- `source, provenance and history methods in src/store/api.rs`
- `src/cas/evidence.rs encoding with MX-10`
- `AnswerGraph evidence projection with MX-40`

Shared surfaces require an explicit handoff recorded in `DECISIONS.md` and in
the EvidenceReturn of every affected module.

## Forbidden Ownership

- `src/context/`
- `src/crdt/`

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
