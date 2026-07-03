# MemoryX vs Classic RAG

This document explains what MemoryX offers beyond a classic Retrieval-Augmented
Generation pipeline. It is a capability comparison, not a benchmark result.

MemoryX should not be described as "universally better than RAG". A fair claim
is narrower:

```text
Classic RAG retrieves likely relevant text.
MemoryX stores structured knowledge and validates answers against claims,
evidence, contexts, constraints, conflicts, and provenance.
```

## Summary

| Area | Classic RAG | MemoryX |
| --- | --- | --- |
| Primary unit | Text chunk | Knowledge atom with claims, evidence, metadata, and links |
| Retrieval result | Similar passages | Candidate claims/evidence for validation |
| Source of truth | Retrieved text plus model interpretation | Stored atoms, claims, evidence, contexts, and graph links |
| Answer model | Generated text | Structured `AnswerPack` and proof-style `AnswerGraph` |
| Contradictions | Often hidden, blended, or resolved by the model | Explicit conflicts, branches, and context policies |
| Missing evidence | Often becomes a hallucinated answer unless prompted carefully | `unknowns`, `limitations`, gaps, and insufficient-evidence states |
| Temporal changes | Older chunks may be retrieved as current | History, `SUPERSEDES`, tombstones, snapshots, and temporal policy |
| Reasoning direction | Mostly retrieve -> generate | Backward gaps + forward candidates + fixed-point assembly |
| Constraints | Prompt-level instructions | Explicit `QueryContract`, hard/soft constraints, validation |
| Provenance | Document or chunk reference | Claim/evidence/source path plus graph support |
| Reproducibility | Depends on model, retrieval, and prompt state | Snapshot + query contract + structured answer state |
| Multi-project memory | Usually separated by index conventions | Project/user scoped bases, contexts, and multi-base MCP routing |
| Assistant integration | Usually query-only retrieval endpoint | MCP read/write/admin knowledge-operation surface |
| Federation | Often merges retrieved text from multiple stores | Federation of compatible claims/provenance/metadata |
| Repairability | Rebuild index from documents if available | CAS integrity, Merkle/integrity checks, WAL/snapshot, repair/rebuild |

## Capabilities MemoryX Adds

### 1. Knowledge Atoms Instead Of Text Chunks

Classic RAG normally chunks documents and retrieves similar chunks. MemoryX uses
small durable knowledge atoms with structured claims, evidence, source records,
contexts, and graph links.

Why it matters:

- answers can be assembled from facts rather than only nearby text;
- provenance can point to the claim/evidence path, not just a broad document;
- updates can supersede specific facts instead of replacing whole chunks.

### 2. Claim, Evidence, And Provenance Model

MemoryX stores factual claims separately from source material and evidence. An
answer can expose which claims were used and which evidence supports them.

RAG weakness addressed:

- similar text can look relevant but still fail to prove the answer;
- a generated answer can cite a chunk while adding unsupported facts.

MemoryX expected behavior:

- factual answer parts should map to stored evidence;
- unsupported facts should be reported as unknown or insufficiently evidenced.

### 3. Explicit Contexts And Branches

Classic RAG often has one implicit global retrieval context. MemoryX has
contexts, branches, and policies for project-specific or assumption-specific
views.

Use cases:

- separate project memories;
- alternative hypotheses;
- conflicting research branches;
- old versus current operational decisions.

### 4. Conflict Management

RAG may blend contradictory chunks into one smooth answer. MemoryX keeps
conflicts visible through conflict records, branches, and answer status.

MemoryX should not silently choose one side unless the query policy and evidence
allow that choice.

### 5. QueryContract Instead Of Prompt-Only Control

MemoryX can compile or accept an explicit `QueryContract`: required facts,
constraints, forbidden assumptions, context policy, provenance requirements, and
answer limits.

This makes the query auditable:

- what was requested;
- which constraints were enforced;
- which constraints could not be satisfied;
- why an answer is accepted, partial, blocked, or unknown.

### 6. Backward + Forward Reasoning

Classic RAG typically follows:

```text
query -> retrieve chunks -> generate answer
```

MemoryX follows a stricter loop:

```text
query -> QueryContract -> backward gaps -> forward candidates
      -> validation -> conflict/context checks -> fixed-point answer graph
```

This is useful when the system must know what evidence is missing, not just what
text is similar.

### 7. Fixed-Point Answer Assembly

MemoryX uses solver-style answer assembly rather than treating retrieval as the
answer. Retrieval suggests candidates; the solver validates and assembles a
minimal proof-style subgraph.

RAG weakness addressed:

- retrieval rank can dominate truth;
- a high-similarity chunk can override constraints;
- multi-hop answers are often flattened into text.

### 8. AnswerGraph And AnswerPack

MemoryX returns structured output:

- selected context;
- answer status;
- claims used;
- supporting graph;
- evidence references;
- conflicts;
- gaps;
- unknowns;
- limitations;
- query trace.

This is more machine-checkable than a plain generated paragraph.

### 9. Grounded Unknowns Instead Of Forced Answers

MemoryX is expected to say when evidence is missing. This is a product feature,
not a failure.

Examples:

- no source exists;
- sources conflict and no policy resolves them;
- a required claim is missing;
- a requested benchmark number has not been measured.

### 10. Temporal Correctness And History

MemoryX keeps history-oriented structures such as superseding relations,
tombstones, snapshots, and operation history.

This addresses a common RAG problem: outdated chunks can still be retrieved and
presented as current.

### 11. Local-First Scoped Storage

MemoryX keeps bases in explicit roots:

- project scope: `.memoryx/bases/<name>` inside the project;
- user scope: user-level `.memoryx/bases/<name>`.

This makes storage location intentional instead of hidden inside an index
service.

### 12. Multi-Base MCP Layer

MemoryX MCP can work with more than one base in one MCP process:

- `list_bases`;
- `active_base`;
- `connect_base`;
- `switch_base`;
- `query_base`;
- optional `base_ref` routing for store-backed tools.

This matters for agents that need to work with several project memories or a
global user memory without launching separate MCP servers for every operation.

### 13. MCP As A Knowledge Operation Surface

MemoryX MCP is not only a query endpoint. It exposes store-backed operations for
querying, ingestion, updates, provenance, entities, relations, contexts,
conflicts, graph traversal, history, and multi-base routing.

This lets an assistant maintain the database, not just read from it.

### 14. Federation Of Compatible Knowledge Bases

MemoryX federation is intended to exchange compatible knowledge structures:
claims, provenance, metadata, and snapshots. It should not reduce federation to
"ask another RAG endpoint for text".

### 15. Integrity, Repair, And Rebuild

MemoryX includes durability and maintenance concepts that are often outside a
basic RAG stack:

- content-addressed storage;
- integrity verification;
- Merkle/integrity checks;
- WAL/snapshot-oriented durability;
- repair and rebuild commands;
- rebuildable indexes.

### 16. Portable CPU Default With Native Optimization Option

MemoryX is intended to ship portable builds by default, while allowing native
CPU-specific builds for local benchmarking. This avoids publishing binaries
that only work correctly or optimally on one developer CPU.

## What MemoryX Does Not Claim Without Benchmarks

Do not claim these until measured with frozen datasets, raw outputs, and exact
commands:

- MemoryX is faster than all RAG systems.
- MemoryX is more accurate on every domain.
- MemoryX always has better recall.
- MemoryX eliminates hallucinations completely.
- MemoryX closes all support cases without humans.

The correct public benchmark claim should be scoped:

```text
On this dataset, with these baselines, these commands, this hardware, and this
judge rubric, MemoryX scored X on accuracy, Y on groundedness, Z on latency,
and failed these listed cases.
```

## Benchmark Dimensions To Prove The Difference

The comparison should measure:

- accuracy;
- recall;
- groundedness;
- latency;
- support deflection rate;
- conflict visibility;
- temporal correctness;
- constraint compliance;
- reproducibility;
- provenance completeness;
- repair/rebuild behavior;
- multi-base/project isolation.

## Russian Summary

MemoryX нельзя честно рекламировать как "RAG, но всегда лучше". Правильная
формулировка:

```text
RAG ищет похожий текст.
MemoryX хранит проверяемые знания и собирает ответ через claims, evidence,
contexts, constraints, conflicts, provenance и proof graph.
```

Ключевые отличия MemoryX:

- атомы знания вместо чанков текста;
- claims/evidence/provenance как source of truth;
- явные contexts и branches;
- явные conflicts вместо сглаживания противоречий;
- `QueryContract` вместо управления только prompt-ом;
- backward gaps + forward candidates;
- fixed-point solver;
- `AnswerGraph` и `AnswerPack`;
- unknowns/limitations вместо выдумывания ответа;
- история, supersedes, tombstones и snapshots;
- project/user scoped storage;
- Multi-Base MCP layer;
- MCP не только для query, но и для ведения базы;
- federation совместимых баз через claims/provenance/metadata;
- integrity, repair и rebuild;
- portable CPU build по умолчанию.

Для публичных заявлений нужны benchmark results. Без результатов можно
говорить о возможностях архитектуры, но нельзя утверждать измеренное
превосходство.
