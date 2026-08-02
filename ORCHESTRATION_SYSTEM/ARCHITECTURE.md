# MemoryX Modular Orchestration Architecture

Status: canonical orchestration control-plane contract.

This system coordinates work on MemoryX. It does not replace the MemoryX
concept, implementation roadmap, source code, or release gates.

## Authority Order

When instructions conflict, modules stop and report the conflict in
`DECISIONS.md` and their EvidenceReturn. Authority is resolved in this order:

1. `Concept/SKF.txt` and `Concept/SKF-1.1 Implementer-Ready Spec.txt`.
2. `Concept/Расширение.txt` for the accepted extension direction.
3. `CONCEPT_EXTENSION_IMPLEMENTATION_PLAN.md`, including the open N5 gates.
4. Repository `AGENTS.md` and the root orchestration contracts.
5. `ORCHESTRATION_SYSTEM/INTER_AGENT_COMMUNICATION.md`.
6. `ORCHESTRATION_SYSTEM/manifest.json` and this architecture.
7. The selected module's canonical packet, task, plan, decisions, and dossier.

No orchestration artifact may silently change a higher-level authority. A
required concept change is a separate proposal and blocks implementation until
it is explicitly accepted.

## Root And Module Topology

The root orchestrator routes work through `manifest.json`. It owns cross-module
handoffs, ownership conflict resolution, shared-host coordination, root
validation, and the root project-local MemoryX base.

Every registry module is a complete autonomous contour. It has its own:

- immutable execution profile;
- real session binding state;
- canonical packet, task, plan, progress, decisions, acceptance, and compact
  recovery files;
- conceptual or mathematical contract dossiers;
- hooks and recovery state;
- evidence and logs directories;
- strict MemoryX contract;
- separate physical project-local MemoryX base.

Modules may be invoked independently, but they cannot expand their ownership
or mutate another module's base. Cross-module work is split into explicit
handoffs recorded by both affected modules and the root orchestrator.

## Inter-Agent Communication Contract

All autonomous-contour prompts, task packets, plans, progress and decision
narratives, cross-module handoffs, EvidenceReturn narratives, lifecycle-hook
recovery instructions, and wrapper stable prefixes are written in English.
User-facing language is independent and may follow the user; the orchestrator
must translate a bounded task before persisting or forwarding it.

The exact rule, technical-literal allowlist, deterministic lexical boundary,
and validation limitations are defined by `INTER_AGENT_COMMUNICATION.md`.
This communication policy is additive and does not alter model/session,
ownership, storage, evidence, concept, or roadmap authority.

## Ownership Boundaries

`manifest.json` is the registry and ownership authority. An `owned_paths`
entry grants primary responsibility, not permission to bypass code review.
`shared_surfaces` identifies co-located APIs where ownership is by named
semantic surface rather than by whole file. A module must not claim an entire
shared file merely because its symbols are stored there.

The domains are intentionally separated as follows:

| Module | Primary domain |
| --- | --- |
| MX-00 | Concept/SKF canon and concept-to-implementation traceability |
| MX-10 | CAS, canonical identities, physical storage and indexes |
| MX-20 | Operation journals, crash recovery, and the open N5 program |
| MX-30 | Contexts, TMS, claims, relations, branches, and conflicts |
| MX-40 | QueryContract, routing, fixed-point solving, and AnswerGraph |
| MX-50 | Sources, evidence, provenance, supersession, and history |
| MX-60 | Graph, CRDT, replication, and federation |
| MX-70 | MCP, CLI, live-owner control, scoped runtime, and leases |
| MX-80 | Schemas, durable format migration, and interoperability |
| MX-90 | Conformance, adversarial tests, benchmarks, CI, and release gates |
| MX-95 | Fail-closed inventory and isolated direct/cross-tool testing of every published MCP operation |

MX-00, MX-90, and MX-95 do not own production Rust implementation. MX-80 owns schema
and migration contracts, while the producer module still owns the semantics
that are encoded by a schema. MX-90 may add tests but returns implementation
defects to the production owner.

## Execution And Session Contract

Every module has the immutable profile:

- model: `gpt-5.6-sol`;
- reasoning effort: `xhigh`;
- `max` reasoning: forbidden.

`session_id.txt` is zero bytes until a real Codex session UUID is observed.
The build and validation scripts never invent a UUID. Once bound, a module is
continued only with:

```text
codex exec resume <UUID> -m gpt-5.6-sol -c model_reasoning_effort="xhigh"
```

The invocation script reasserts the model and reasoning effort on every start
and resume. A stable session and stable prompt prefix are cache optimizations;
they do not guarantee a cache hit, quality level, or preserved runtime state.
Changing a bound module's model or reasoning profile requires a new module
identity and an explicit root decision, not an in-place edit.

`session_registry.json` mirrors all contour binding files for root routing and
audit. It never creates or overrides a binding. Aggregate validation compares
the registry, manifest, immutable execution profile, and every
`session_id.txt`; any mismatch fails closed.

## Compact And Recovery Contract

Before compact, the module must update `TASK.md`, `PLAN.md`, `PROGRESS.md`, and
`DECISIONS.md`. `PreCompact.ps1` records their hashes, the observed session
binding, current acceptance state, and the recovery timestamp in
`state/RECOVERY.json`. It does not create semantic content.

`SessionStart.ps1` and `PostCompact.ps1` validate and emit the saved recovery
packet. Hooks restore only what was durably written to files and MemoryX; they
do not reconstruct unsaved reasoning, fabricate a UUID, prove that compact
occurred, or claim cache reuse.

## MemoryX Role And Storage Contract

Mandatory rules remain in versioned files. MemoryX is the structured database
for atoms, claims, evidence, provenance, branches, conflicts, queries, and
recovery records; it is not the sole copy of governance rules.

All orchestration bases are physical and repository-local:

- root: `.memoryx/bases/memoryx`;
- module: `.memoryx/bases/<module-base>`.

User-scoped bases, foreign project bases, and implicit base discovery are
forbidden for this orchestration system. One mutable owner may hold a physical
base. Other clients must use the supported live-owner protocol or remain
read-only through an explicit supported path; they must not start a second
writer.

Module contracts require source registration and provenance for accepted
evidence. Unknowns and conflicts remain explicit. A module may not turn a
retrieval candidate into proof or a model statement into verified evidence.

## EvidenceReturn

Every completed or interrupted invocation returns one JSON object conforming
to `schemas/evidence-return.schema.json`. It identifies:

- module, real session state, model, and reasoning effort;
- task and acceptance gates;
- changed artifacts;
- commands and observed results;
- MemoryX base and provenance references;
- unresolved conflicts and unknowns;
- the next smallest step.

An EvidenceReturn is a report, not proof by itself. Referenced commands,
artifacts, sources, and MemoryX records remain independently verifiable.

## Shared-Host Resource Contract

Heavy gates first acquire the repository-local resource coordination lease.
The coordinator records the owning process and observed machine load, waits
when configured thresholds are exceeded, and never terminates foreign
processes. A stale lease can be reclaimed only after its recorded process is
confirmed absent and the configured TTL has expired.

No module may kill another project's process, attach to a foreign base, or use
another module's physical base to avoid its own setup.

## Validation Boundary

Structural validation proves only that the registry, required files, JSON
contracts, paths, ownership declarations, session binding format, hook syntax,
and local base layout satisfy this control-plane contract.

Structural validation does not prove:

- that hooks ran in a real Codex lifecycle;
- that a real compact/resume restored useful context;
- that a session or prompt prefix reused cache;
- that the requested model produced high-quality work;
- that MemoryX semantic, crash, federation, or release gates passed;
- that N5 is complete.

Those are separate live or implementation gates and must be reported as
unverified until observed.

## Concept Non-Regression

This orchestration layer preserves and cannot bypass:

- knowledge atoms rather than text chunks;
- contexts, branches, conflicts, CTX_PROBE, and self-consistency;
- backward plus forward Heptapod reasoning;
- fixed-point answer assembly and minimal proof AnswerGraph;
- federation over claims, provenance, metadata, and snapshots;
- CAS integrity, Merkle verification, CRDT, WAL/snapshot, repair, and rebuild;
- complete MCP database operation and explicit local-first scope;
- portable CPU defaults.

The relation/context audit and repair work restores an existing projection
invariant. It is additive, explicit, idempotent, history-preserving, and
fail-closed; it does not redefine relation semantics or durable formats.
Solver, AnswerGraph, federation, CAS/CRDT, and scoped storage remain unchanged.
N5 crash atomicity remains open exactly as recorded in the implementation
roadmap. No orchestration status may relabel that gate as complete.
