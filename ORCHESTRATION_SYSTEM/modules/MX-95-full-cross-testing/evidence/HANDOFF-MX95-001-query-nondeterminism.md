# Production Defect Handoff: MX95-001

Status: `CONFIRMED`  
Acceptance impact: `BLOCKING`  
Production owner: `MX-ROOT`, with implementation routing to `MX-40` and MCP
verification routing to `MX-70`

## Verified Runtime Fact

MemoryX 2.0.4 at SHA-256
`43A7F9D0D042FD20B4888D596F77525E60F91BA3B30657989F0D7E4E15FBEC11`
does not return byte-stable AnswerPacks for identical `query` calls against an
unchanged snapshot.

Twelve calls with the same arguments:

```json
{"name":"query","arguments":{"query_text":"mx95_provenance_marker","ctx_id":0}}
```

produced five result SHA-256 values and five permutations of the same three
gap names in `limitations[code=IncompleteEvidence].description`. Snapshot
fields, status, coverage and the set of gap names remained the same. The owned
MCP child exited with code 0.

## Reproduction

From the repository root:

```powershell
python -B ORCHESTRATION_SYSTEM/modules/MX-95-full-cross-testing/scripts/reproduce_query_nondeterminism.py `
  --binary target/release/memoryx.exe `
  --repo-root . `
  --base-name mx-95-disposable-20260802T054714306Z-a3-primary `
  --output <new-module-owned-output-directory> `
  --repetitions 12
```

Exit code 2 means the defect was reproduced by design. Preserved evidence is
under `runs/20260802T054714306Z/defects/MX95-001/`.

## Source Anchor and Inference

Verified source structure: `src/store/api.rs:4114` constructs
`gap_kinds: HashSet<&str>` and `src/store/api.rs:4133` formats it with debug
formatting. Logical inference: randomized hash iteration order causes the
observed permutations. The runtime reproduction is the proof; this inference
only identifies the smallest likely correction site.

A candidate correction is to sort the labels or use a deterministic ordered
set before formatting. This is a handoff suggestion, not an MX-95 production
edit. Regression acceptance should repeat identical query serialization
within one process and after reopen and require exact equality.

## Boundaries

- MX-95 did not edit `src/` or repository tests.
- No foreign process or base was touched.
- This defect does not establish any N5 failure; N5 remains open.
- Structural hook checks, compact recovery records, cache behavior and model
  quality are unrelated and unproved.
