# MCP Tool Inventory and API Mapping

ID: `TOOL_INVENTORY`
Kind: `technical`
Owner: `MX-95`

## Scope

authoritative tools/list discovery, MCP schema, CLI/library mapping and fail-closed coverage accounting

## Contract

- Canonical requirements must be cited before this dossier is changed.
- Definitions, invariants, preconditions, postconditions, and failure behavior
  must be explicit for an activated task.
- Retrieval candidates are not proof; accepted claims require evidence and
  provenance.
- Unknown or disputed semantics remain unresolved rather than being guessed.

## Historical 2.0.4 State

Run `20260802T054714306Z` is authoritative. Its first observed live
`tools/list` contains 47 entries and 47 unique names. The stable surface digest
is `77a3e319d6b56ee5aaa8e8d70f4467ec1b2b9853aab1eb75a8b2b55cda553027`.

The machine-readable inventory and mappings are in:

- `runs/20260802T054714306Z/surface.json`;
- `runs/20260802T054714306Z/surface.raw.json`;
- `runs/20260802T054714306Z/coverage-ledger.blocked.json`.

Every discovered tool has a schema SHA-256, purpose, mutability
classification, observed MCP request example, CLI mapping or explicit client
mapping, Rust dispatch/library anchor, one passed direct case, one or more
passed cross-tool sequence references, coverage categories, and limitations.
The 47 direct cases are joined to request/response hashes in
`full-attempt-03/calls.jsonl`; 47/47 direct rows passed.

The final global gate is `blocked`, not passed, because query result
determinism failed under `MX95-001`. This does not invalidate the observed
direct execution of the other tools.

## Required Evidence

- source references and MemoryX provenance;
- affected symbols or formats;
- focused tests with observed output;
- compatibility and non-regression analysis;
- unresolved risks and the next falsifying test.

## Validation Boundary

The validator joins the observed surface, ledger, direct-call log,
cross-sequence matrix, and resilience report. Its mutation controls passed for
missing/duplicate tools, absent direct cases, absent cross cases, missing
cross evidence, and a passed case lacking request/response hashes. The
synthetic resilience fixture used by those controls is explicitly validator
test data and is not runtime acceptance evidence.

## Post-Fix 2.0.5 State

Run `20260802T062756339Z` independently recaptured the authoritative surface:
47 observed tools, 47 unique names, no duplicates, and surface digest
`77a3e319d6b56ee5aaa8e8d70f4467ec1b2b9853aab1eb75a8b2b55cda553027`.

`coverage-ledger.accepted.json` contains exactly that discovered set. Each
tool has its observed schema digest, purpose/classification, real MCP request,
CLI or live-owner-client mapping, current Rust dispatch/library anchor,
passed direct case, passed cross-tool sequence, categories, and limitations.
The final validator joined 47 tools to 66 observed calls and 14 sequences with
no failures. The prior 2.0.4 ledger is historical and is not acceptance input.
