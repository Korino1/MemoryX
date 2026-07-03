# MemoryX Effectiveness Benchmark Plan

This plan defines an honest benchmark for showing where MemoryX is useful and
where it is not. It is a plan, not a result report. Do not publish numbers until
the dataset, judge rubric, raw outputs, and exact commands are frozen.

## 1. Goal

Measure MemoryX against realistic knowledge-base tasks where classic RAG often
fails:

- conflicting or outdated facts;
- missing evidence;
- source-grounded answers;
- multi-hop reasoning across stored facts;
- project/support cases that should be closed without human intervention;
- reproducible answers from a fixed knowledge snapshot.

The benchmark must show both strengths and failures. A failed MemoryX result is
still useful evidence if it is recorded honestly.

## 2. Compared Systems

Minimum comparison set:

- `MemoryX`: project-scoped base, fixed snapshot, CLI/MCP query path.
- `Baseline RAG`: standard chunk retrieval over the same source documents.
- `Vector-only RAG`: embeddings + top-k chunks, no explicit claims/conflicts.
- Optional `Hybrid RAG`: BM25 + embeddings + reranker, if available.

Rules:

- Every system receives the same source material.
- Every system receives the same user query.
- No system gets hidden manual hints.
- Prompts, chunk size, embedding model, top-k, reranker, and model version must
  be recorded.
- If an LLM is used for final wording, its output must be judged separately from
  retrieval/provenance.

## 3. Dataset Design

Create several benchmark suites instead of one mixed pile.

### 3.1. Support Cases

Purpose: measure deflection and direct user usefulness.

Examples:

- "How do I connect MemoryX as MCP?"
- "Where is the project base stored?"
- "How do I repair a damaged base?"
- "Why does a query return insufficient evidence?"
- "How do I switch from project base to user-global base?"

Required labels:

- expected answer;
- required sources;
- acceptable answer variants;
- disallowed hallucinations;
- whether the case should be closed without human help.

### 3.2. Engineering Decision Memory

Purpose: test project-memory value.

Examples:

- "Why is production MCP `memoryx serve --stdio` instead of the example server?"
- "Why are native CPU builds not the default release?"
- "What changed in the storage path policy?"
- "Which files define QueryContract and AnswerPack behavior?"

Required labels:

- decision claim;
- rationale;
- source files or commits;
- constraints and non-regression requirements;
- superseded decisions.

### 3.3. Conflict And Branching

Purpose: test a key MemoryX advantage over RAG.

Examples:

- two sources claim different current storage policies;
- one branch says project-local base, another says user-global base;
- one source says a feature is implemented, another says it is only planned.

Required labels:

- conflicting claims;
- expected conflict visibility;
- expected branch/context behavior;
- whether an answer is allowed to choose one side.

### 3.4. Temporal Freshness

Purpose: verify that outdated facts are not treated as current.

Examples:

- old MCP tool count versus current MCP tool count;
- old example server as production entrypoint versus current production entrypoint;
- old storage behavior versus current scoped roots.

Required labels:

- old fact;
- current fact;
- supersedes relation;
- expected current answer;
- expected historical answer if the query asks for history.

### 3.5. Missing Evidence And Refusal

Purpose: measure hallucination resistance.

Examples:

- "Prove MemoryX is faster than all RAG systems."
- "Give benchmark numbers for production users."
- "Which clients rated MemoryX as 95% accurate?"

Expected behavior:

- answer must not invent numbers;
- answer should report insufficient evidence;
- answer should name what evidence would be required.

### 3.6. Multi-Hop Evidence

Purpose: test proof graph construction.

Examples:

- query requires source -> claim -> relation -> answer;
- answer needs two or more facts connected by graph edges;
- evidence is split across source records.

Required labels:

- required hops;
- required supporting claims;
- required source path;
- acceptable minimal proof graph.

## 4. Metrics

### 4.1. Accuracy

Definition: percentage of answers judged correct without human correction.

Suggested scoring:

- `1.0`: correct and directly usable;
- `0.5`: mostly correct but needs minor human correction;
- `0.0`: incorrect, misleading, or unsupported.

Formula:

```text
accuracy = sum(answer_correctness_scores) / total_cases
```

Judge requirements:

- at least two independent reviewers for final published numbers;
- disagreements resolved by written rubric, not by model preference;
- raw answer and source references must be saved.

### 4.2. Recall

Definition: percentage of cases where retrieval found the required relevant
information.

For MemoryX:

- count required source/claim/evidence items present in `AnswerPack`,
  `AnswerGraph`, provenance paths, or retrieval trace.

For RAG:

- count required source chunks present in top-k retrieved chunks.

Formula:

```text
recall = found_required_items / total_required_items
```

Important:

- retrieval recall is not final answer correctness;
- a system can retrieve the right evidence and still answer badly.

### 4.3. Groundedness

Definition: percentage of factual answer statements that are supported by real
stored evidence.

Suggested scoring:

- `grounded_statement`: factual statement has source/provenance support;
- `ungrounded_statement`: factual statement has no evidence;
- `unsupported_refusal`: no answer because evidence is missing, scored as safe
  rather than hallucinated.

Formula:

```text
groundedness = grounded_factual_statements / factual_statements
```

MemoryX-specific checks:

- every factual statement should map to claim/evidence/source path;
- `AnswerPack.unknowns` and `limitations` should be used when evidence is
  missing;
- generated wording must not add extra factual claims outside the proof graph.

### 4.4. Latency

Definition: wall-clock time for the full query cycle.

Measure separately:

- `compile_ms`: natural query -> query contract;
- `retrieve_ms`: lexical/semantic/graph retrieval;
- `solve_ms`: constraints, conflicts, fixed-point assembly;
- `render_ms`: structured answer rendering;
- `total_ms`: full request time from user query to answer.

Rules:

- run warmup iterations;
- report p50, p95, p99;
- record hardware, OS, build profile, feature flags, and base size;
- do not compare debug MemoryX against release RAG or vice versa.

### 4.5. Deflection Rate

Definition: percentage of support cases closed without human intervention.

Formula:

```text
deflection_rate = closed_without_human / total_support_cases
```

A case is closed only if:

- the answer is correct;
- the answer is grounded;
- the answer includes enough steps for the user to act;
- no required warning or limitation is missing;
- the user would not need a human follow-up for the intended task.

### 4.6. Conflict Visibility

Definition: percentage of contradiction cases where the system exposes the
conflict instead of hiding it.

MemoryX expected signal:

- `AnswerPack.conflicts`;
- `ConflictSet`;
- branch/context information;
- alternatives or policy-blocked status.

### 4.7. Temporal Correctness

Definition: percentage of time-sensitive cases where the system uses the right
version of a fact.

Checks:

- current query returns current fact;
- historical query can return old fact when requested;
- superseded facts are not presented as current.

### 4.8. Constraint Compliance

Definition: percentage of cases where hard constraints and negative constraints
are respected.

Examples:

- "Do not use PostgreSQL-related facts."
- "Only answer from project-scoped base policy."
- "Require provenance."
- "Return insufficient evidence if no source exists."

### 4.9. Reproducibility

Definition: same snapshot + same query contract produces the same logical
answer.

Record:

- snapshot id;
- query contract;
- answer status;
- selected claims;
- proof graph hash or stable summary.

## 5. RAG Weaknesses To Test Directly

The benchmark must include cases for each weakness MemoryX claims to address.

| RAG weakness | MemoryX function to test | Metric |
| --- | --- | --- |
| Similar chunk is treated as truth | Claim/evidence validation | Groundedness, accuracy |
| Contradictions are blended | Conflicts and branches | Conflict visibility |
| Outdated facts are retrieved | Supersedes/history/temporal policy | Temporal correctness |
| Missing evidence becomes hallucination | Unknowns, limitations, insufficient evidence | Groundedness, accuracy |
| No exact source path | Provenance path, source records | Groundedness |
| No stable answer state | Snapshot id and query contract | Reproducibility |
| Multi-hop facts are flattened | AnswerGraph and graph traversal | Recall, accuracy |
| Context leaks between projects | Project/user scoped bases and contexts | Accuracy, constraint compliance |
| Retrieval decides the answer | Constraint-first solver | Constraint compliance |
| Support cases still need humans | Structured answer and source-backed steps | Deflection rate |

## 6. Output Files

Recommended benchmark artifact layout:

```text
benchmarks/effectiveness/
  cases/
    support.jsonl
    engineering_decisions.jsonl
    conflicts.jsonl
    temporal.jsonl
    missing_evidence.jsonl
    multi_hop.jsonl
  source_sets/
    memoryx_docs/
    synthetic_conflicts/
  runs/
    memoryx/
    baseline_rag/
    vector_rag/
  reports/
    summary.md
    summary.json
```

Each case should include:

```json
{
  "id": "support_mcp_connect",
  "suite": "support",
  "query": "How do I connect MemoryX as MCP?",
  "required_sources": ["README.md", "src/bin/memoryx.rs"],
  "required_claims": ["production MCP entry point is memoryx serve --stdio"],
  "disallowed_claims": ["examples/mcp_server_full.rs is production"],
  "human_closure_expected": true,
  "metrics": ["accuracy", "recall", "groundedness", "latency", "deflection_rate"]
}
```

## 7. Runner Plan

### Phase 1. Dataset Freeze

- Create the JSONL cases.
- Freeze source files used by all systems.
- Assign each case required claims, sources, and disallowed hallucinations.
- Store dataset version and checksum.

### Phase 2. MemoryX Loader

- Create a clean project-scoped base.
- Register sources.
- Ingest facts as atoms.
- Attach source provenance.
- Create conflicts, branches, supersedes relations where required.
- Save snapshot id.

### Phase 3. Baseline RAG Loader

- Chunk the same source files.
- Store chunk ids and source paths.
- Record chunk size, overlap, embedding model, top-k, and reranker if used.

### Phase 4. Execution

For every case:

- run MemoryX query through CLI or MCP;
- run baseline RAG query;
- save raw retrieval output;
- save final answer;
- save latency breakdown;
- save errors and timeouts.

### Phase 5. Judging

- Automatic scoring for recall, provenance presence, latency, reproducibility.
- Human or expert scoring for accuracy and deflection.
- LLM judging may be used only as an auxiliary label, not as final truth.

### Phase 6. Report

Report:

- per-suite scores;
- aggregate scores;
- raw failure examples;
- where MemoryX loses;
- where MemoryX wins;
- commands, hardware, dataset version, and raw artifacts.

## 8. Minimum Viable Benchmark

Start with 30 cases:

- 8 support cases;
- 6 engineering decision cases;
- 5 conflict cases;
- 5 temporal cases;
- 3 missing evidence cases;
- 3 multi-hop cases.

This is enough to demonstrate direction without pretending to be a universal
benchmark.

## 9. Acceptance Criteria

The benchmark is publishable only when:

- every score links to raw outputs;
- source data is frozen;
- judging rubric is documented;
- latency methodology is documented;
- MemoryX failures are included;
- baseline RAG setup is strong enough to be fair;
- no claim says "MemoryX is better" without the exact benchmark scope.

## 10. Next Implementation Tasks

1. Add `benchmarks/effectiveness/cases/*.jsonl`.
2. Add a MemoryX fixture loader that registers sources and creates atoms.
3. Add a baseline RAG runner interface.
4. Add a scorer for recall, groundedness, conflict visibility, temporal
   correctness, constraint compliance, and latency.
5. Add a human-review CSV/JSON export for accuracy and deflection scoring.
6. Add `docs/BENCHMARK_EFFECTIVENESS_REPORT_TEMPLATE.md`.
7. Run the MVP benchmark and publish only scoped, evidence-backed numbers.
