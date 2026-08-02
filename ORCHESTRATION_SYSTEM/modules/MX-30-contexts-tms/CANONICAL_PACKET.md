# MX-30 Canonical Packet

Module: **Contexts, TMS, Claims and Conflicts**

## Inter-Agent Communication

Inter-agent language: English only.

Prompts, task packets, plans, progress and decision narratives, handoffs,
EvidenceReturn narratives, and compact recovery instructions must be English.
User-facing text may follow the user's language but must be translated before
it enters an inter-agent artifact. Exact technical identifiers listed by the
root language contract remain unchanged.

## Responsibility

Own context lineage, active claim projection, relation state, branching, conflict lifecycle, invariants and sourced transitions.

## Canonical Authorities

- `Concept/SKF.txt#3-context-model`
- `Concept/SKF.txt#10-tms-conflicts-and-branches`
- `docs/LIVE_OWNER_CONTROL.md`

The module must read the relevant canonical passages and the current roadmap
before changing implementation. It must stop on a concept conflict rather than
silently reinterpret the concept or mark an open roadmap gate complete.

## Primary Ownership

- `src/context/`
- `src/invariants/`

## Shared Surfaces

- `claim, relation and context methods in src/store/api.rs`
- `MCP context and relation tools with MX-70`
- `claim schemas with MX-80`

Shared surfaces require an explicit handoff recorded in `DECISIONS.md` and in
the EvidenceReturn of every affected module.

## Forbidden Ownership

- `src/cas/io.rs`
- `src/federation/`

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
