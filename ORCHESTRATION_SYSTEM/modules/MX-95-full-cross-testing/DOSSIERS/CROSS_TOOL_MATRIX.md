# Stateful Cross-Tool Test Matrix

ID: `CROSS_TOOL_MATRIX`
Kind: `technical`
Owner: `MX-95`

## Scope

positive, negative, boundary, stateful, reopen, owner, concurrency, recovery, migration and idempotence sequences

## Contract

- Canonical requirements must be cited before this dossier is changed.
- Definitions, invariants, preconditions, postconditions, and failure behavior
  must be explicit for an activated task.
- Retrieval candidates are not proof; accepted claims require evidence and
  provenance.
- Unknown or disputed semantics remain unresolved rather than being guessed.

## Historical 2.0.4 State

Core attempt 03 executed 66 observed calls in 14 meaningful passed sequences:

- `SQ-ATOM-LIFECYCLE`, `SQ-BASE`, `SQ-CONFLICT`, `SQ-CONTEXT`, `SQ-ENTITY`;
- `SQ-IDEMPOTENCE`, `SQ-INTEGRITY`, `SQ-LIVE-OWNER`;
- `SQ-NEGATIVE-BOUNDARY`, `SQ-PROVENANCE`, `SQ-QUERY-CONTRACT`;
- `SQ-RELATION`, `SQ-REOPEN`, `SQ-SEARCH-GRAPH`.

`runs/20260802T054714306Z/full-attempt-03/sequences.json` is the
machine-readable join. It covers all 47 tools at least once in a cross-tool
sequence. The same run observed live-owner proxy mutation, a direct competing
writer rejected with exit 73, graceful reopen, exact idempotent ingest and
source attachment, conflict rejection, and clean integrity.

Resilience attempt 01 additionally observed:

- response limits `max_bytes=2048`, `max_items=1`, emitted bytes 1131, with
  both byte and item truncation reported;
- a deliberately removed relation/context projection detected by audit,
  selected by dry-run, repaired durably, then unchanged by repeat apply;
- post-commit owned-child process death followed by lexical count 1 and valid
  integrity with one checked atom.

These three checks passed. Identical query result determinism failed and blocks
the global audit. Post-commit process death is a structural recovery test only;
no in-flight N5 persistence-boundary claim is made.

## Required Evidence

- source references and MemoryX provenance;
- affected symbols or formats;
- focused tests with observed output;
- compatibility and non-regression analysis;
- unresolved risks and the next falsifying test.

## Safety Boundary

All mutable bases use the project-local `mx-95-disposable-*` namespace.
Seven pre-existing foreign MemoryX processes were observed and none was
stopped or modified. Every created child was closed or deliberately killed by
its own retained process handle, and each shared-host resource lease was
released.

## Post-Fix 2.0.5 State

Fresh core attempt 01 passed the same 14 semantic sequence families across all
47 discovered tools. Resilience attempt 01 passed 13 cases covering
provenance, same-process/reopen query equality, response limits, clean
integrity, migration/idempotence, and post-commit owned-process recovery.

The dedicated post-fix gate executed 32 identical queries and one after
reopen. Parsed semantic results and `IncompleteEvidence.description` were each
unique. Three full response variants differed solely in framing byte counters;
the accepted semantic projection excludes only those counters and documents
them as an unresolved limitation.

No owned process or resource lease remains. The seven foreign HPF owners
observed before the run were not stopped or modified.
