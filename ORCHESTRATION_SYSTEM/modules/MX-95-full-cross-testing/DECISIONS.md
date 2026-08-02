# Decisions

## D-000: Inherit Root Contracts

Accepted: preserve the MemoryX concept and roadmap; use knowledge atoms and
source-backed evidence; keep one mutable owner per physical base; never use a
user-scoped or foreign base; treat stable session/prefix as an optimization.

## D-001: Separate Audit Ownership From Production Ownership

Accepted: MX-95 owns only its harness, inventory, ledger and evidence. It may
read production source to map APIs and execute the released binary, but it may
not edit `src/`, repository tests, release files, or other module contours.
Confirmed runtime defects are returned to the root developer for correction.

## D-002: Runtime Discovery Is Authoritative

Accepted: a run's `tools/list` output is the authoritative tool set. The
expected count of 47 is an acceptance assertion, not an inventory substitute.
Coverage passes only when the validator joins every discovered tool to one
executed direct case and at least one executed cross-tool sequence.

## D-003: Honest Partial Coverage

Accepted: unavailable, unsafe, or blocked cases remain explicit gaps. Schema
inspection, Rust unit tests, and structural orchestration validation may
support evidence but cannot be relabeled as an executed MCP tool test.

## D-004: Bounded Disposable Runtime Namespace

Accepted: every mutating runtime case uses an explicitly named project-local
base beginning `mx-95-disposable-20260802T054714306Z-`. The durable
`.memoryx/bases/mx-95-full-cross-testing` base is reserved for final
source-backed evidence. Existing KPA, HPF, user-scoped, and foreign module
bases and their owners are excluded from the test target set.

## D-005: Separate Core Coverage From Resilience Claims

Accepted: the 47/47 direct and 14-sequence core run does not by itself prove
crash/recovery, migration, deterministic output, or bounded response
semantics. Those claims require separately executed scenarios and observed
evidence joined into the final ledger. A process killed after an acknowledged
commit is only a structural recovery case and cannot close N5.

## D-006: Manual Compact Recovery Is Not Hook Evidence

Accepted: automatic model-context compression was observed after the core
run, but no platform invocation of the module `PreCompact.ps1` or
`PostCompact.ps1` was observed. The developer manually reloaded the required
orchestrator/module sources and refreshed durable state. This proves neither
hook wiring, hidden-context retention, cache reuse, nor model quality.

## D-007: Deterministic AnswerPack Bytes Are an Acceptance Gate

Accepted: equality is checked on the complete structured query result because
the published AnswerPack includes limitations as user-visible, machine-readable
output. Permuting an unordered set inside a limitation description is not
discarded as harmless telemetry. Twelve identical requests produced five
serialized results, so `MX95-001` is a confirmed production defect and the
global audit remains blocked.

## D-008: Defect Ownership Handoff

Accepted: MX-95 preserves the reproduction but does not edit production.
`MX-ROOT` owns triage/authorization, `MX-40` owns deterministic AnswerPack
construction, `MX-70` owns MCP byte-stability verification, and `MX-90` owns
release/conformance reuse. The likely unordered-set source anchor is guidance,
not a production fix or proof by inspection.

## D-009: Fresh Post-Fix Evidence Supersedes Runtime Acceptance Only

Accepted: run `20260802T062756339Z` is the sole authoritative runtime evidence
for the 2.0.5 post-fix packet. The earlier 2.0.4 run remains historical and is
not joined into the accepted ledger. Canonical concept and roadmap contracts
are unchanged; this decision only replaces a binary-specific audit result.

## D-010: Semantic Determinism Excludes Framing Byte Counters

Accepted: the post-fix gate compares the entire parsed AnswerPack while
excluding only `response_limits.emitted_bytes` and `original_bytes`, which
measure response framing size. Limit policies, truncation flags, retained item
counts, limitations including descriptions, claims, evidence, graph,
conflicts, coverage, snapshot and status remain compared. The 33 observations
had one semantic result and one description; three full variants differed
only in `emitted_bytes`. This explicit classification refines test accounting
without weakening SKF canonical determinism or hiding the byte variation.

## D-011: MX95-001 Post-Fix Handoff Accepted for the Audited Shape

Accepted: MemoryX 2.0.5 closes the observed `IncompleteEvidence` ordering
defect for the tested query/snapshot across 32 calls and reopen. MX-95 returns
the result to `MX-ROOT`, `MX-40`, and `MX-70`, and exposes it to `MX-90` for
conformance/release use. This is not total semantic acceptance and does not
close N5, hooks, compact/resume, cache reuse, or model quality.
