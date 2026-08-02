# Acceptance

Current state: `PASSED_WITH_LIMITATIONS`

MemoryX 2.0.6 reactivation:

- [x] Exact 2.0.6 candidate identity was captured.
- [x] All 47 discovered tools retained direct and cross-tool evidence.
- [x] The explicit relation-tombstone fixture failed closed without a decision.
- [x] Tombstone dry-run wrote no journal or backup.
- [x] Explicit restore created verified backup/replay evidence and was
      idempotent across repeat and reopen.
- [x] Determinism and nine fail-closed validator controls remained passing.
- [ ] No KPA restore/retire decision is made by this acceptance record.
- [ ] N5 and previously declared live gates remain open.

- [x] Real module session is observed and bound by `invoke_module.ps1`.
- [x] Runtime `tools/list` is captured and contains 47 unique tools.
- [x] Inventory contains exactly the discovered tool set and schema digests.
- [x] Every tool has truthful CLI/library mappings.
- [x] Every tool has one executed direct case with raw evidence.
- [x] Every tool participates in one executed meaningful cross-tool sequence.
- [x] Positive, negative, boundary, stateful, restart, owner/concurrency,
      recovery, migration, idempotence, provenance, conflict and query-output
      classes are represented by executed cases.
- [x] Ledger validator fails on missing tools, direct cases and cross cases.
- [x] Test bases are project-local and no foreign process/base was touched.
- [x] Module MemoryX evidence has registered sources and provenance.
- [x] EvidenceReturn validates and unresolved defects/gaps remain explicit.

Blocking gate:

- [x] At least 32 identical query requests plus process reopen return one
      semantic result and one stable `IncompleteEvidence` description.
      Observed: 32+1, one semantic result, one description. Full framing
      output had three `emitted_bytes` variants, recorded as a limitation.

Post-fix evidence:

- [x] MemoryX 2.0.5 identity matches the required version, size and SHA-256.
- [x] The accepted ledger and determinism-aware validator pass.
- [x] Nine validator mutation controls fail closed as expected.
- [x] New source-backed MemoryX provenance and integrity pass.

Structural validation cannot check these boxes automatically. N5 completion,
real compact recovery, cache reuse and model quality are separate live gates.
