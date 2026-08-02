# Progress

State: `PASSED_WITH_LIMITATIONS`

- MemoryX 2.0.6 run `20260802T085223672Z` is now authoritative.
- Candidate identity passed: 6,298,112 bytes, SHA-256
  `D9B07DB4F64610A4117AFBB4D4BA498A45398C9F35CB36AB06741B4463A01703`.
- Lifecycle discovery passed with 47 unique tools for `2025-11-25`,
  `2025-06-18`, and `2024-11-05`; every owned child exited cleanly.
- Core coverage passed 47/47 direct tools, 66 calls, and 14 cross-tool
  sequences. Eighteen resilience cases passed.
- The new disposable tombstone fixture observed
  `relation_atom_tombstoned`, rejected unresolved apply, kept dry-run
  mutation-free, created and validated backup/replay evidence, applied an
  explicit restore once, repeated idempotently, and reopened consistently.
- Determinism remained one semantic result and one `IncompleteEvidence`
  description across 32+1 queries. Nine validator controls passed.
- Durable evidence atom
  `d476c5593e6bfdff38215e7e3f8bd162ca08609fad809ead9ae6913751ad40ad`
  is linked to source IDs 30-35; module-base integrity passed.
- This run does not choose recovery semantics for KPA records and does not
  close N5 or the existing live limitations.

- Post-fix run `20260802T062756339Z` is authoritative for MemoryX 2.0.5.
- Binary identity passed: version 2.0.5, 6,165,504 bytes, SHA-256
  `7E9B26731625830B1A715429DE87D502FD9169B7F8E1FF3E8969939BBBD7A969`.
- Fresh lifecycle discovery passed for `2025-11-25`, `2025-06-18`, and
  `2024-11-05`: 47 observed, 47 unique, silent initialized notification,
  exit 0, and no orphan for every owned surface child.
- Fresh core attempt 01 passed 47/47 direct tools, 66 observed calls, and 14
  cross-tool sequences. Primary and reopened owners exited 0; direct writer
  contention returned the expected exit 73 while live-owner proxying worked.
- Resilience attempt 01 passed all 13 cases. Clean-base integrity checked 16
  valid atoms with zero invalid/missing and consistent relation projections.
  Response limits enforced 2,048 bytes/1 item. The deliberately damaged
  migration fixture was detected, repaired and idempotent on repeat. The
  committed crash fixture reopened with lexical count 1 and valid integrity.
- Post-fix determinism attempt 02 passed 32 same-process queries plus one
  after reopen: one semantic projection and one stable
  `IncompleteEvidence.description`. Three full AnswerPack variants differed
  only in response framing byte counters; `emitted_bytes` and
  `original_bytes` are explicitly excluded from the semantic projection.
- The strengthened coverage validator passed with 47 ledger tools, 66 calls,
  14 sequences, 13 resilience cases, 32+1 determinism observations, one
  semantic result and one description.
- Nine mutation controls passed. Validator-only synthetic resilience data is
  not runtime evidence.
- Durable evidence atom
  `d00b222f813e35229cadd073f2fefa950e8fc4d2b1201c8996b0e79de767cb10`
  has provenance source IDs 6-17; IDs 6-11 came from a committed attempt whose
  local parser failed to unwrap `text-result`, and IDs 12-17 are the verified
  retry set. Module-base integrity passed and both owners exited cleanly.
- The root developer's dirty `Cargo.toml`, `Cargo.lock`, and
  `src/store/api.rs` changes were observed but not edited by MX-95. No foreign
  process or base was stopped or mutated.
- Run evidence: `runs/20260802T062756339Z`.

Historical 2.0.4 evidence remains below for traceability; it is not reused as
post-fix acceptance evidence.

- Real session `019fc0f9-f099-79a2-be80-ebe515628fa5` is bound.
- Checked release and installed binaries both identify as MemoryX 2.0.4 with
  SHA-256 `43A7F9D0D042FD20B4888D596F77525E60F91BA3B30657989F0D7E4E15FBEC11`.
- Run `20260802T054714306Z` captured the first authoritative live
  `tools/list`: 47 tools, 47 unique names, surface digest
  `77a3e319d6b56ee5aaa8e8d70f4467ec1b2b9853aab1eb75a8b2b55cda553027`.
- Exact MCP lifecycle passed for protocol versions `2025-11-25`,
  `2025-06-18`, and `2024-11-05`; initialized notifications produced no
  response and each owned child exited cleanly.
- Core attempt 03 passed 47/47 executed direct cases, 66 observed calls, and
  14 cross-tool sequences. It covered live-owner proxying, writer contention,
  graceful reopen, idempotence, provenance, conflict rejection,
  QueryContract behavior, and clean integrity verification.
- Attempts 01 and 02 are preserved as harness-development failures (duplicate
  base alias and invalid graph-pattern syntax), not production defects.
- Seven foreign MemoryX processes observed before testing were not stopped or
  modified. Every runtime base used the `mx-95-disposable-*` project-local
  namespace, and the resource lease was released.
- Resilience attempt 01 observed response-limit enforcement, deliberate
  relation-context migration with idempotent repeat, and clean recovery of an
  acknowledged atom after forced death of the owned child. These three cases
  pass in `resilience-blocked-report.json`.
- Deterministic result acceptance failed. A read-only 12-call reproduction
  produced five different AnswerPack digests and five orderings of the same
  `IncompleteEvidence` gap set. This is confirmed production defect
  `MX95-001`; source inspection anchors the likely cause to an unordered
  `HashSet` formatted at `src/store/api.rs:4114-4134`.
- The final validator correctly exits 1 because the resilience report is not
  passed, while still confirming the 47-tool/66-call/14-sequence joins.
- Seven isolated mutation controls all behaved as expected: the clean
  validator-only join passed, and six missing/duplicate/evidence mutations
  failed closed. Its synthetic resilience record is not runtime evidence.
- Durable evidence atom
  `8ff47be284f251f57d26740b87f828c1793f4f5c163c37dd85f179e05238480a`
  is linked to source IDs 1-5 in `.memoryx/bases/mx-95-full-cross-testing`;
  final base integrity was valid and its owner exited cleanly.
- Runtime expansion stopped after confirming the defect. No production Rust,
  repository tests, release artifact, or foreign module file was edited.
- An automatic context compression occurred after this progress. Recovery was
  performed from durable files; no platform hook execution is claimed.
- No hook/compact lifecycle, cache reuse, model quality, total MemoryX semantic
  acceptance, or N5 completion is claimed.
