# Compact Context

Module: `MX-95` / `full-cross-testing`
Session: `019fc0f9-f099-79a2-be80-ebe515628fa5` (`BOUND`)
Model: `gpt-5.6-sol`
Reasoning: `xhigh`
Active task: completed 2.0.5 post-fix audit and accepted handoff for run `20260802T062756339Z`

Recovery order:

1. `CANONICAL_PACKET.md`
2. `TASK.md`
3. `PLAN.md`
4. `PROGRESS.md`
5. `DECISIONS.md`
6. `ACCEPTANCE.md`
7. `state/RECOVERY.json`
8. `MEMORYX_CONTRACT.json`
9. `DOSSIERS/TOOL_INVENTORY.md`
10. `DOSSIERS/CROSS_TOOL_MATRIX.md`
11. `DOSSIERS/TEST_EVIDENCE.md`

Current checkpoint:

- Binary: MemoryX 2.0.5, 6,165,504 bytes, SHA-256 `7E9B26731625830B1A715429DE87D502FD9169B7F8E1FF3E8969939BBBD7A969`.
- Authoritative surface: 47 observed / 47 unique tools; three lifecycles pass.
- Core attempt 01: 47/47 direct tools, 66 calls, 14 cross-tool sequences.
- Resilience attempt 01: 13/13 cases passed.
- Determinism attempt 02: 32 same-process + 1 reopen, one semantic result,
  one `IncompleteEvidence` description; three full variants only in framing
  byte counters.
- Final validator passed; nine mutation controls passed.
- Durable MemoryX atom: `d00b222f813e35229cadd073f2fefa950e8fc4d2b1201c8996b0e79de767cb10`, provenance source IDs 6-17.
- Evidence root: `runs/20260802T062756339Z`.
- Next: MX-ROOT/MX-40/MX-70/MX-90 consume the EvidenceReturn. Any binary or
  surface change requires a new authoritative run.

Automatic model-context compression was observed after the core run. Required
sources were manually reloaded from durable state. No platform
PreCompact/PostCompact invocation, compact/resume correctness, cache reuse,
hidden-context retention, or model-quality proof is claimed. N5 remains open.
