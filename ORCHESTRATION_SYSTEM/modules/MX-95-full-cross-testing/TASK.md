# Task: Full MCP Tool and Cross-Tool Audit

Status: `COMPLETE`

## Post-Fix Reactivation

- Resume the bound session after the root developer's `MX95-001` correction.
- Audit `target/release/memoryx.exe` version `2.0.5`, SHA-256
  `7E9B26731625830B1A715429DE87D502FD9169B7F8E1FF3E8969939BBBD7A969`,
  size `6165504` bytes.
- Start a fresh run ID and recapture authoritative `tools/list`, all three MCP
  protocol lifecycles, 47 direct cases, cross-tool sequences, resilience and
  fail-closed validator controls.
- Require at least 32 identical queries plus a process reopen to produce one
  stable semantic result and one stable `IncompleteEvidence` description.
- Update durable module state and EvidenceReturn. If any runtime defect remains,
  block and hand it off rather than editing production files.
- Keep real hook lifecycle, real compact-resume, cache reuse, model quality,
  total semantic acceptance and N5 explicitly unproven/open.

## Active Run Checkpoint

- Post-fix authoritative run: `20260802T062756339Z`.
- Checked binary: MemoryX 2.0.5, 6,165,504 bytes, SHA-256
  `7E9B26731625830B1A715429DE87D502FD9169B7F8E1FF3E8969939BBBD7A969`.
- Fresh discovery observed 47 tools and 47 unique names; all three protocol
  lifecycles passed with silent initialized notifications and clean exits.
- Fresh core attempt 01 passed 47/47 direct tools, 66 calls, and 14 meaningful
  cross-tool sequences.
- Thirteen resilience cases passed, including response limits, clean reopen,
  deliberate relation/context migration with idempotent repeat, and
  post-commit recovery after killing only the owned child.
- The post-fix gate executed 32 identical queries plus one after process
  reopen: one semantic result and one `IncompleteEvidence` description were
  observed. Full results had three framing-byte variants solely in
  `response_limits.emitted_bytes`; byte counters are retained as an explicit
  non-semantic limitation.
- Nine validator controls passed, including rejection of missing/duplicate
  tools, missing direct/cross evidence, nondeterministic semantics, and fewer
  than 32 determinism repetitions.
- Durable post-fix evidence is registered under atom
  `d00b222f813e35229cadd073f2fefa950e8fc4d2b1201c8996b0e79de767cb10`.

## Historical Run Checkpoint

- Authoritative run: `20260802T054714306Z`.
- The first live `tools/list` observed 47 tools and 47 unique names; the
  inventory is derived from that response rather than from the expected count.
- Core attempt 03 executed 47/47 direct cases, 66 observed calls, and 14
  meaningful cross-tool sequences without touching a foreign base or process.
- Resilience observation, fail-closed validator mutation controls, and
  MemoryX-native evidence registration are complete.
- Confirmed production defect `MX95-001` blocks the deterministic-result gate.
  Runtime expansion stopped under the task stop condition; the reproduction
  and owner handoff are preserved inside this contour.
- Automatic model-context compression was observed after the core run. No
  platform compact hook execution, cache reuse, or model-quality claim is made.

## Objective

Build and execute a machine-verifiable audit of the complete MCP tool surface
published by the checked MemoryX 2.0.5 binary. The first observed `tools/list`
response is authoritative for the run and is expected to contain 47 unique
tools. Do not hard-code success from that expectation.

## Owned Work

- Create the inventory, coverage ledger, isolated MCP harness, test cases, run
  reports, and EvidenceReturn only inside this module contour.
- Use the project-local physical module base
  `.memoryx/bases/mx-95-full-cross-testing` for durable module evidence.
- Test runtime state only in explicitly named disposable project-local bases;
  never use user-scoped, KPA, HPF, or another module's base.
- Do not edit production Rust, repository tests, release artifacts, or another
  module's files. Return every suspected runtime defect as a reproducible
  handoff to the root developer.

## Required Inventory

For every unique tool returned by `tools/list`, record:

- tool name and stable schema digest;
- purpose and mutating/read-only classification;
- MCP request example;
- CLI mapping or explicit `not_exposed` with rationale;
- Rust/library mapping or explicit `transport_only` with source anchor;
- direct test case ID and observed status;
- at least one meaningful cross-tool sequence ID and observed status;
- coverage categories exercised and unresolved limitations.

The ledger and validator must fail closed when a discovered tool is absent,
duplicated, lacks a direct case, lacks a cross-tool case, refers to a missing
case, or is marked passed without observed evidence.

## Coverage Classes

Exercise positive, negative, boundary, stateful and cross-call behavior plus
reopen/restart, live-owner, concurrency, crash/recovery, migration,
idempotence, provenance, conflict, QueryContract and response-limit semantics.
Not every class applies independently to every tool, but every tool requires a
direct case and one semantically meaningful cross-tool sequence.

## First Pass Gates

1. Exact MCP lifecycle succeeds for every supported protocol version and the
   initialized notification produces no response.
2. Observed tools are unique and inventory count exactly equals observed
   `tools/list` count.
3. Every observed tool has truthful MCP, CLI and library mappings.
4. Every observed tool has an executed direct case and executed cross-tool
   sequence, or the global coverage gate remains failed.
5. Reopen/restart and owner/concurrency cases leave no orphan process and no
   foreign process is stopped.
6. Integrity and deterministic result checks pass on test bases not
   intentionally damaged by a negative fixture.
7. EvidenceReturn names commands and observed results, MemoryX provenance,
   unresolved gaps, and the next smallest step.

## Stop Conditions

- Stop a case before it can touch a non-module or foreign base.
- Do not claim all-tool coverage from schemas, source inspection, or existing
  Rust tests alone.
- On a confirmed production defect, preserve its fixture/log, mark acceptance
  blocked, and return the defect to the root developer without editing `src/`.
- N5 remains an open roadmap gate; structural crash tests cannot close it.
