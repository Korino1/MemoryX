# Decisions

## D-001: Ten Product Domains And One Test Contour

Accepted ten non-overlapping product domains: concept canon; CAS/storage;
journal/crash-recovery/N5; contexts/TMS; query/proof; provenance/history;
graph/CRDT/federation; runtime/MCP; schemas/migrations; and
conformance/release. MX-95 is the independent full/cross-tool testing contour
and does not own production Rust. Co-located `src/store/api.rs` work is owned
by named semantic surfaces and requires explicit handoffs.

## D-002: Files Govern, MemoryX Structures Evidence

Mandatory rules remain in versioned files. MemoryX is the structured database
for evidence, provenance, conflicts, query, and recovery, not the only copy of
governance. Each contour receives one separate repo-local physical base and
allows one mutable owner.

## D-003: Real-Only Immutable Sessions

All contours use immutable `gpt-5.6-sol` with `xhigh`; `max` is forbidden.
`session_id.txt` starts at zero bytes and is written only after a canonical
UUID is observed in a real `thread.started` event. A bound contour resumes only
with `codex exec resume <UUID>` while reasserting model/reasoning.

## D-004: Honest Validation Boundary

Structural checks may prove files, schemas, paths, hook syntax, session format,
and initialized bases. They cannot prove a real Codex hook lifecycle, compact
recovery, cache reuse, model quality, semantic correctness, or N5 completion.

## D-005: Concept And Roadmap Non-Regression

The orchestration system does not change the MemoryX concept or roadmap. The
relation/context repair restores an existing projection invariant and remains
additive and fail-closed. N5 remains open; any future concept change requires a
separate explicit proposal.

## D-006: Recovery Fails On Post-Checkpoint Edits

PostCompact verifies the saved hash of every task, plan, progress, decision,
and compact-context file. A changed document invalidates the recovery packet;
the orchestrator must update all required documents and record a new
PreCompact checkpoint instead of restoring mixed state.

## D-007: Full Cross-Tool Testing Is An Independent Contour

MX-95 owns runtime surface discovery, direct/cross-tool coverage ledgers,
resilience controls, and defect handoff without owning production Rust. Its
real session UUID is recorded only after a `thread.started` event and all later
work resumes that same session. A passed MX-95 packet is bounded evidence, not
proof of total concept completion, platform hook behavior, or N5.

## D-008: Session Registry Mirrors Binding Files

`session_registry.json` is machine-readable orchestration state, not a second
session authority. Aggregate validation compares every entry to the real
`session_id.txt`, immutable model and reasoning profile, and manifest domain.
Any mismatch fails closed.

## D-009: English Is The Inter-Agent Protocol Language

All prompts, task packets, plans, progress and decision narratives, handoffs,
EvidenceReturn narratives, recovery instructions, and stable prefixes shared
between agents use English. User-facing language remains user-selected; the
root orchestrator translates before persistence or delegation.

The machine gate rejects non-ASCII letters and combining marks except the
single immutable technical path listed in `INTER_AGENT_COMMUNICATION.md`, and
requires explicit English declarations. This deterministic check does not
claim semantic classification of arbitrary ASCII prose. The policy changes no
MemoryX runtime, session, ownership, base, concept, or roadmap invariant.
Negative persisted-surface tests use only verified temporary paths below the
ignored repository `target/` directory and remove only their owned fixture.
