# AnswerPack

`AnswerPack` is the structured result returned by MemoryX query execution. It
is not a text completion. It binds an answer to a context, snapshot, proof
subgraph, evidence, rejected candidates, conflicts, and limitations.

## Important Fields

- `status`: deterministic answer status such as complete, partial, no match,
  insufficient evidence, conflicted, or policy blocked.
- `snapshot`: identity of the knowledge state used for the answer.
- `graph`: answer graph summary in CLI/MCP JSON output.
- `claims` and `claims_v2`: claim views with epistemic status, confidence,
  modality, polarity, evidence, and provenance fields.
- `evidence` and `evidence_records`: direct evidence references and source
  enriched evidence records.
- `coverage_report`: total/covered/uncovered gaps and source/evidence counts.
- `rejected_candidates`: candidates blocked by hard QueryContract constraints.
- `conflicts` and `conflict_sets`: visible conflict summaries and branch sets.
- `query_trace`: retrieval action trace from the planner.
- `proposed_text`: renderer or LLM proposals that are not validated claims.

## MCP Explanation Tools

`explain_answer_graph` returns a compact explanation payload:

```json
{"name":"explain_answer_graph","arguments":{"query_text":"Find facts about MemoryX persistence","ctx_id":0}}
```

`get_provenance_path` returns the proof-grade provenance chain for one atom:

```json
{"name":"get_provenance_path","arguments":{"atom_id":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}}
```

## Contract Boundary

`AnswerPack` may contain `proposed_text`, but proposed text is separated from
validated claims. Consumers should treat `claims`, `claims_v2`, `evidence`,
`coverage_report`, and `provenance` as the factual surfaces.

