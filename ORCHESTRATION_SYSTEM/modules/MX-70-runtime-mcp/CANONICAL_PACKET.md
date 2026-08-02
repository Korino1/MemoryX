# MX-70 Canonical Packet

Module: **MCP, CLI and Live Runtime**

## Inter-Agent Communication

Inter-agent language: English only.

Prompts, task packets, plans, progress and decision narratives, handoffs,
EvidenceReturn narratives, and compact recovery instructions must be English.
User-facing text may follow the user's language but must be translated before
it enters an inter-agent artifact. Exact technical identifiers listed by the
root language contract remain unchanged.

## Responsibility

Own CLI and MCP transport, tool contracts, base selection, live-owner proxy, request bounds and exclusive writer lease behavior.

## Canonical Authorities

- `README.md`
- `docs/LIVE_OWNER_CONTROL.md`
- `CONCEPT_EXTENSION_IMPLEMENTATION_PLAN.md`

The module must read the relevant canonical passages and the current roadmap
before changing implementation. It must stop on a concept conflict rather than
silently reinterpret the concept or mark an open roadmap gate complete.

## Primary Ownership

- `src/bin/memoryx.rs`
- `src/store/base_lease.rs`
- `docs/LIVE_OWNER_CONTROL.md`

## Shared Surfaces

- `MCP-exposed store methods in src/store/api.rs`
- `tool schemas with MX-80`
- `runtime integration tests with MX-90`

Shared surfaces require an explicit handoff recorded in `DECISIONS.md` and in
the EvidenceReturn of every affected module.

## Forbidden Ownership

- `src/query/solver.rs`
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
