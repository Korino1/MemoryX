# Test Evidence and Defect Handoff

ID: `TEST_EVIDENCE`
Kind: `conceptual`
Owner: `MX-95`

## Scope

observed results, provenance, reproducibility, unresolved gaps and production-owner handoff

## Contract

- Canonical requirements must be cited before this dossier is changed.
- Definitions, invariants, preconditions, postconditions, and failure behavior
  must be explicit for an activated task.
- Retrieval candidates are not proof; accepted claims require evidence and
  provenance.
- Unknown or disputed semantics remain unresolved rather than being guessed.

## Historical 2.0.4 State

Acceptance is blocked by confirmed production defect `MX95-001`. Twelve
identical read-only `query` calls against the same base and snapshot produced
five distinct serialized results. The differences are five orderings of the
same uncovered-gap names inside the user-visible `IncompleteEvidence`
description.

Raw and normalized evidence:

- `runs/20260802T054714306Z/defects/MX95-001/reproduction.stdout.jsonl`;
- `runs/20260802T054714306Z/defects/MX95-001/reproduction-report.json`;
- `runs/20260802T054714306Z/resilience-attempt-02/determinism-first.stdout.jsonl`;
- `runs/20260802T054714306Z/resilience-blocked-report.json`;
- `evidence/HANDOFF-MX95-001-query-nondeterminism.md`.

Verified source anchor: `src/store/api.rs:4114` collects gap-kind labels into
`HashSet<&str>` and `src/store/api.rs:4133` formats that set with `{:?}`.
This source structure is consistent with the runtime permutations, but the
runtime reproduction—not source inspection—is the defect proof.

Durable MemoryX evidence was registered in
`.memoryx/bases/mx-95-full-cross-testing`: atom
`8ff47be284f251f57d26740b87f828c1793f4f5c163c37dd85f179e05238480a`
has five attached source records (source IDs 1 through 5), and final integrity
was valid.

## Required Evidence

- source references and MemoryX provenance;
- affected symbols or formats;
- focused tests with observed output;
- compatibility and non-regression analysis;
- unresolved risks and the next falsifying test.

## Handoffs

- `MX-ROOT`: production defect triage and scoped correction authorization.
- `MX-40`: deterministic AnswerPack construction and a repeat-serialization
  regression covering `IncompleteEvidence` ordering.
- `MX-70`: MCP `query` byte-stability verification after the MX-40 fix.
- `MX-90`: rerun this 47-tool audit as release/conformance evidence.

No `src/`, repository tests, release artifact, or foreign module file was
edited by MX-95.

## Post-Fix 2.0.5 Evidence

`MX95-001` is closed for the audited query shape and snapshot. The fresh
post-fix evidence is:

- `runs/20260802T062756339Z/postfix-run-summary.json`;
- `runs/20260802T062756339Z/coverage-ledger.accepted.json`;
- `runs/20260802T062756339Z/coverage-validation.accepted.json`;
- `runs/20260802T062756339Z/determinism-postfix-attempt-02/determinism-report.json`;
- `runs/20260802T062756339Z/resilience-attempt-01/resilience-report.json`;
- `runs/20260802T062756339Z/validator-controls-attempt-02/mutation-control-report.json`.

Durable MemoryX evidence is atom
`d00b222f813e35229cadd073f2fefa950e8fc4d2b1201c8996b0e79de767cb10`.
Its provenance contains source IDs 6-17. IDs 6-11 are the committed first
registration set whose local parser failed to unwrap a text result; IDs 12-17
are the verified retry set. Final integrity is valid.

Updated handoffs:

- `MX-ROOT`: accept the bounded post-fix runtime result and byte-counter
  limitation.
- `MX-40`: retain deterministic ordered limitation construction regression.
- `MX-70`: retain MCP lifecycle, response-limit and live-owner regressions.
- `MX-90`: consume the fresh ledger as conformance/release evidence without
  treating it as N5 or total semantic acceptance.
