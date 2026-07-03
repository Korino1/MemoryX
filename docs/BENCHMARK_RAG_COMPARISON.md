# RAG Comparison Benchmark

This benchmark scaffold is for honest comparison, not marketing claims. It
does not contain results or superiority claims.

For the broader product-level benchmark plan covering accuracy, recall,
groundedness, latency, support deflection, conflicts, temporal correctness, and
RAG weakness coverage, see `docs/BENCHMARK_EFFECTIVENESS_PLAN.md`.

## Dataset

Seed cases live in `benchmarks/rag_comparison_cases.json` and cover:

- conflicting claims;
- temporal change;
- multi-hop facts;
- missing evidence / unsupported factual statement.

## Metrics

- `hard_constraint_accuracy`: whether MUST/MUST_NOT constraints are enforced.
- `conflict_visibility`: whether contradictions are visible in the answer.
- `provenance_completeness`: whether evidence/provenance paths are returned.
- `gap_reporting`: whether missing evidence is exposed instead of fabricated.
- `reproducibility`: whether same snapshot and contract give stable logical
  output.

## Run Scaffold

Build first:

```powershell
cargo +nightly build --release --features mcp
```

Run:

```powershell
powershell -ExecutionPolicy Bypass -File benchmarks/run_rag_comparison.ps1
```

The script writes JSONL scaffold records under `benchmarks/results/`. Before
publishing benchmark numbers, populate each case with the same source facts for
all compared systems, freeze the dataset, and store raw outputs.

## Rules

- Do not publish claims without raw outputs and exact commands.
- Do not compare MemoryX against an intentionally weak RAG baseline.
- Do report failures and unsupported cases.
- Do keep query contracts and answer snapshots with the result files.
