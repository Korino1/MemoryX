# Reproducible MemoryX and RAG Comparison

This benchmark is a presentation-ready measurement harness, not a result or a
claim that MemoryX is universally better than RAG. It freezes one corpus, one
set of queries, and one set of limits for both systems, records raw outputs,
and calculates only metrics supported by those records.

The included corpus is **synthetic benchmark material**, not a statement about
MemoryX production behavior. The result templates deliberately contain no
answers, scores, latencies, or success claims.

## Scope

The fixture covers the functional cases in which MemoryX is intended to add
controls beyond ordinary retrieval:

| Suite | Required observable behavior |
| --- | --- |
| `conflicts` | Contradictory sources remain visible instead of being silently merged. |
| `temporal` | Current output does not present superseded material as current. |
| `multi_hop` | Required evidence spans more than one record. |
| `constraints` | Explicit negative and context constraints are respected. |
| `missing_evidence` | An unsupported premise is safely reported, not invented. |
| `provenance` | The answer exposes the required evidence identifier. |
| `contexts` | A selected project context does not leak a conflicting project fact. |

These are bounded scenario tests. They do not establish general reasoning
quality, production support deflection, or performance on an external corpus.

## Frozen Inputs

- [Corpus](../benchmarks/rag_comparison/corpus.jsonl) is the same input for all
  systems. Each record has a stable evidence id, context, and provenance path.
- [Cases](../benchmarks/rag_comparison/cases.jsonl) supplies exactly the same
  user query and `retrieval_limit`, `answer_max_chars`, and `timeout_ms` to
  both adapters.
- [Schema](../benchmarks/rag_comparison/schema.json) describes JSONL corpus,
  case, and result records. The runner also verifies cross-record references,
  duplicate ids, frozen hashes, case coverage, common query text, and common
  limits.

`prepare` writes a run manifest with SHA-256 digests. `score` rejects result
files whose run id, corpus digest, case digest, query, or limits do not match
that manifest. This prevents a comparison from mixing a stronger corpus or
larger retrieval budget into one side after the run starts.

## Metrics

| Metric | Calculation | Scoring boundary |
| --- | --- | --- |
| Accuracy | Mean human `judgement.accuracy` in `[0, 1]` | Human review; null is unscored. |
| Recall | Required evidence ids found in `retrieved_evidence_ids` | Automatic; cases with no required evidence are unscored. |
| Groundedness | `grounded_factual_statements / factual_statements` | Human statement audit; zero factual statements is unscored, not 100%. |
| End-to-end latency | Runner stopwatch around the complete adapter callback | Automatic p50/p95/p99; includes callback startup and output writing. |
| Deflection / closed-case rate | Human `closed_case` for support-like cases | Never inferred from `status` or answer text. |
| Functional scenario rate | Boolean observation for each case's tagged scenario | Per scenario; null is unscored. |
| Reproducibility | Same frozen inputs with repeated adapter-provided logical output hashes | Requires a separately recorded repeat; never inferred from one response. |

The runner does not use an LLM judge. If one is used to assist reviewers, save
its model, prompt, output, and human override beside the raw results; a human
reviewer remains the final authority for Accuracy, Groundedness, and closure.

## Runner

Requires PowerShell 7. Run from the repository root:

```powershell
pwsh -NoLogo -File benchmarks/run_rag_comparison.ps1 -Mode validate
pwsh -NoLogo -File benchmarks/run_rag_comparison.ps1 -Mode selftest
pwsh -NoLogo -File benchmarks/run_rag_comparison.ps1 -Mode prepare -RunDirectory benchmarks/rag_comparison/runs/demo-001
```

The self-test creates temporary null/unscored records only. It tests schema,
fixture validation, paired-result validation, null handling, aggregation, and
Markdown generation. It does not run MemoryX, RAG, or publish any benchmark
value.

### Adapter Contract

There is deliberately no built-in RAG implementation: embedding model,
chunking, reranking, and generator must be declared by the experimenter rather
than silently replaced by a weak baseline. `record` invokes one command per
case, once for `memoryx` and once for `rag`:

```powershell
pwsh -NoLogo -File benchmarks/run_rag_comparison.ps1 -Mode record `
  -RunDirectory benchmarks/rag_comparison/runs/demo-001 `
  -System memoryx `
  -AdapterCommand 'pwsh -NoLogo -File path/to/memoryx-adapter.ps1'

pwsh -NoLogo -File benchmarks/run_rag_comparison.ps1 -Mode record `
  -RunDirectory benchmarks/rag_comparison/runs/demo-001 `
  -System rag `
  -AdapterCommand 'pwsh -NoLogo -File path/to/rag-adapter.ps1'
```

For each callback, the runner sets:

```text
MX_BENCHMARK_INPUT   absolute JSON input path
MX_BENCHMARK_OUTPUT  absolute JSON output path the callback must create
MX_BENCHMARK_SYSTEM  memoryx or rag
```

The input contains the frozen corpus path/digest, full case, exact query,
limits, required evidence ids, and `required_provenance: true`. The callback
must write exactly one JSON object to `MX_BENCHMARK_OUTPUT` with at least:

```json
{
  "status": "answered",
  "answer": "actual system output, or null when none was produced",
  "retrieved_evidence_ids": ["corpus-id"],
  "provenance_evidence_ids": ["corpus-id"],
  "supporting_claim_ids": [],
  "logical_output_hash": "optional stable hash for repeated-run checks",
  "observations": {
    "conflict_visible": null,
    "temporal_current": null,
    "multi_hop_complete": null,
    "constraints_respected": null,
    "missing_evidence_safe": null,
    "provenance_complete": null,
    "context_isolated": null
  },
  "judgement": {
    "accuracy": null,
    "factual_statements": null,
    "grounded_factual_statements": null,
    "closed_case": null,
    "notes": null
  },
  "adapter_metadata": {
    "implementation": "declare real model, chunking, top-k, reranker, and version here"
  }
}
```

Use `null` until a real answer or human judgement exists. The runner measures
and owns `latency_ms.end_to_end`; callbacks cannot supply a faster latency.
The callback command is intentionally supplied by the operator and is executed
as PowerShell, so use a reviewed local script rather than untrusted text. A
callback failure or missing/invalid output becomes a raw `status: "error"`
record with the runner-measured latency and error message; it is not silently
dropped. A callback must enforce the supplied `timeout_ms`; the runner records
an overrun but does not pretend it can safely terminate every arbitrary command
form.

### MemoryX Adapter

Prepare a clean project-scoped MemoryX base from **the same** corpus records,
with their stable ids mapped to atoms/evidence and with contexts, conflicts,
and temporal relations represented explicitly. Do not add system-only sources
or use the repository's broader knowledge base.

The supported MemoryX integration surfaces are:

```powershell
cargo +nightly build --release --locked --all-features
target/release/memoryx.exe serve --stdio
```

An adapter may call the MCP `query` tool with `query_text` or a validated
`QueryContract`, then read its structured `AnswerPack`/`AnswerGraph` fields to
fill `status`, retrieved ids, provenance ids, claim ids, and observations. Use
`explain_answer_graph` when needed to expose proof/provenance. Record the
binary revision, base preparation command, MCP request, model if any, and
MemoryX query limits in `adapter_metadata` or a sibling run note.

CLI output may also be used if the checked binary exposes the required
structured query fields. Verify the installed binary's `--help` and JSON
output before the run; this benchmark does not assume an unverified CLI syntax.

### Ordinary RAG Adapter

The RAG callback must index only `corpus.jsonl` and receive the runner's exact
query/limits. It must declare in `adapter_metadata`:

- chunking strategy and chunk size/overlap;
- embedding model and version;
- retrieval and reranking strategy;
- `top_k`, generation model/version, prompt, and output cap;
- hardware/build/runtime configuration.

Map every returned chunk to the source corpus id(s), so Recall and provenance
can be checked fairly. If the RAG system cannot expose that mapping, mark the
affected Recall/Provenance observations `null`; do not replace them with a
favorable estimate.

### Score And Presentation Report

```powershell
pwsh -NoLogo -File benchmarks/run_rag_comparison.ps1 -Mode score `
  -RunDirectory benchmarks/rag_comparison/runs/demo-001

pwsh -NoLogo -File benchmarks/run_rag_comparison.ps1 -Mode report `
  -RunDirectory benchmarks/rag_comparison/runs/demo-001
```

For the `reproducibility` case, prepare and record a second run from unchanged
inputs, then supply its files to the first run's scorer. Its different run id
is allowed, but hashes, query text, and limits must still match exactly:

```powershell
pwsh -NoLogo -File benchmarks/run_rag_comparison.ps1 -Mode score `
  -RunDirectory benchmarks/rag_comparison/runs/demo-001 `
  -MemoryXRepeatResults benchmarks/rag_comparison/runs/demo-002/results/memoryx.jsonl `
  -RagRepeatResults benchmarks/rag_comparison/runs/demo-002/results/rag.jsonl
```

This creates `reports/score.json` and `reports/presentation-report.md` under
the run directory. The report preserves `null` metrics as `unscored`; it does
not calculate a winner, statistical significance, or an overall composite.

## Human Review Protocol

Review raw answers blind to system name where practical. For each result,
record only evidence-backed decisions:

- Accuracy: `1.0` correct and usable, `0.5` materially correct but needs a
  small correction, `0.0` incorrect/misleading/unsupported.
- Factual statements: count answer statements that assert facts.
- Grounded factual statements: count only statements supported by the supplied
  provenance/evidence identifiers.
- Closed case: `true` only when the support-like task is correct, grounded,
  actionable, contains required cautions, and genuinely needs no human follow-up.
- Functional observations: use `true` or `false` only after inspecting the
  raw output; retain `null` when the adapter cannot expose enough information.

Before presentation, preserve raw adapter input/output files, result JSONL,
run manifest, reviewer notes, exact commands, hardware details, and failures.
State the corpus scope and any null/unscored cells alongside every chart or
summary. Do not publish a comparative claim if either system was not run on the
same frozen manifest.
