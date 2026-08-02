# MX-95 Canonical Packet

Module: **Full and Cross-Tool Testing**

## Inter-Agent Communication

Inter-agent language: English only.

Prompts, task packets, plans, progress and decision narratives, handoffs,
EvidenceReturn narratives, and compact recovery instructions must be English.
User-facing text may follow the user's language but must be translated before
it enters an inter-agent artifact. Exact technical identifiers listed by the
root language contract remain unchanged.

## Responsibility

Own the fail-closed inventory and isolated direct/cross-tool runtime audit of every published MemoryX MCP operation without editing production Rust.

## Canonical Authorities

- `Concept/SKF.txt`
- `Concept/SKF-1.1 Implementer-Ready Spec.txt`
- `Concept/Расширение.txt`
- `CONCEPT_EXTENSION_IMPLEMENTATION_PLAN.md`
- `ORCHESTRATION_PLAN.md`
- `README.md`
- `docs/LIVE_OWNER_CONTROL.md`

The module must read the relevant canonical passages and the current roadmap
before changing implementation. It must stop on a concept conflict rather than
silently reinterpret the concept or mark an open roadmap gate complete.

## Primary Ownership

- `ORCHESTRATION_SYSTEM/modules/MX-95-full-cross-testing/`

## Shared Surfaces

- `MCP tool contracts with MX-70`
- `schemas and migrations with MX-80`
- `conformance and release handoff with MX-90`

Shared surfaces require an explicit handoff recorded in `DECISIONS.md` and in
the EvidenceReturn of every affected module.

## Forbidden Ownership

- `src/`
- `tests/`
- `benches/`
- `benchmarks/`
- `.github/workflows/`

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
