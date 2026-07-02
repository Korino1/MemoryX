# MemoryX

MemoryX - локальная knowledge fabric система по концепту `SKF-1.1`, которая хранит знания не в виде чанков текста, а в виде атомов знания с provenance, claims, contexts и graph links. Ответ строится не как "похожие куски текста", а как согласованный `AnswerGraph`, который можно проверить и проследить.

Этот репозиторий предназначен для тех, кому недостаточно обычного RAG: для конфликтующих источников, временных срезов, разных юрисдикций, инженерных и исследовательских баз знаний, где важны объяснимость, provenance и контроль контекста.

## Что Это Такое

MemoryX - не hosted SaaS и не обёртка над vector database. Это Rust-движок базы знаний с:

- content-addressed atoms и provenance
- contexts и conflicts как объектами первого класса
- lexical, semantic и graph retrieval
- fixed-point solver, который собирает `AnswerGraph`
- `ClaimViewV2` с epistemic status, confidence vector, modality, polarity, evidence и provenance
- import/export, stats, compaction и federation
- MCP surface для интеграции с ассистентами

Если нужна только похожесть текстовых фрагментов по embedding, это обычно не тот инструмент. Если нужна работа с противоречиями, объяснимый ответ и трассировка происхождения знания, то это как раз целевой сценарий.

## MemoryX И RAG

| Аспект | Обычный RAG | MemoryX |
| --- | --- | --- |
| Единица хранения | Чанки текста | Атомы знания с claims и evidence |
| Цель retrieval | Найти похожие фрагменты | Собрать согласованный answer graph |
| Противоречия | Часто сглаживаются или теряются | Явно хранятся как conflicts или branches |
| Контекст | Обычно неявный и глобальный | Явные contexts с policy |
| Объяснимость | "Это было в документе X" | Provenance плюс supporting subgraph |
| Подходящий сценарий | FAQ, документация, шаблонные ответы | Исследования, право, инженерия, аудит, timeline knowledge |

## Быстрый Старт

MemoryX использует nightly Rust.

```bash
rustup toolchain install nightly
cargo +nightly build --release
```

Создать базу:

```bash
cargo +nightly run --release --bin memoryx -- init --base default
```

Загрузить данные:

```bash
cargo +nightly run --release --bin memoryx -- ingest --base default facts.json
```

Сделать запрос:

```bash
cargo +nightly run --release --bin memoryx -- query --base default "что известно про Rust ownership?"
```

Посмотреть статистику:

```bash
cargo +nightly run --release --bin memoryx -- stats --base default
```

## Как Запускать

### CLI

Основной бинарник - `src/bin/memoryx.rs`.

Поддерживаемые команды:

- `init`
- `ingest`
- `query`
- `compact`
- `export`
- `import`
- `stats`
- `serve`

Справка:

```bash
cargo +nightly run --release --bin memoryx -- --help
```

Практические команды:

```bash
# Инициализировать project-scoped базу
cargo +nightly run --release --bin memoryx -- --base-scope project init --base default

# Импортировать atoms из JSON
cargo +nightly run --release --bin memoryx -- import --base default --format json atoms.json

# Экспортировать atoms в CSV
cargo +nightly run --release --bin memoryx -- export --base default --format csv --output atoms.csv

# Поднять production MCP по stdio
cargo +nightly run --release --features mcp --bin memoryx -- serve --base default --stdio
```

### MCP через core runtime

`memoryx serve --stdio` запускает store-backed MCP transport. Для этого нужен feature `mcp`.

Доступные инструменты:

- `query`
- `search_lex`
- `search_graph`
- `search_semantic`
- `ingest`
- `batch_ingest`
- `update_atom`
- `delete_atom`
- `history`
- `register_source`
- `list_sources`
- `attach_atom_source`
- `create_entity`
- `list_entities`
- `alias_entity`
- `assert_relation`
- `correct_relation`
- `create_context`
- `list_contexts`
- `branch_context`
- `list_conflicts`
- `graph_neighbors`
- `graph_walk`
- `extract_subgraph`

Пример:

```bash
cargo +nightly run --release --features mcp --bin memoryx -- serve --base default --stdio
```

### Полный MCP example

`examples/mcp_server_full.rs` - демонстрационный stdio MCP server вокруг того же store-backed набора операций. Источник истины для production MCP - `memoryx serve --stdio`.

- `query`
- `search_lex`
- `search_graph`
- `search_semantic`
- `ingest`
- `batch_ingest`
- `update_atom`
- `delete_atom`
- `history`
- `register_source`
- `list_sources`
- `attach_atom_source`
- `create_entity`
- `list_entities`
- `alias_entity`
- `assert_relation`
- `correct_relation`
- `create_context`
- `list_contexts`
- `branch_context`
- `list_conflicts`
- `graph_neighbors`
- `graph_walk`
- `extract_subgraph`

Пример:

```bash
cargo +nightly run --release --features mcp --example mcp_server_full -- --base-scope project --base-name default
```

### HTTP federation server

`memoryx serve` без `--stdio` запускает HTTP federation server. Это не MCP transport. Он обслуживает маршруты вроде `/fetch`, `/negotiate`, `/sync`, `/discover` и `/health`.

Пример:

```bash
cargo +nightly run --release --features mcp --bin memoryx -- serve --base default --host 127.0.0.1 --port 8080
```

## Где Хранится База

MemoryX не пишет базу в произвольный `./data` по умолчанию. Базы разрешаются только в scoped roots:

- project scope: `<repo>/.memoryx/bases/<name>`
- user scope: `<home>/.memoryx/bases/<name>`

Если указан простой base name, корень выбирается через scope. Если указан полный путь, он должен оставаться внутри одного из этих корней.

После `memoryx init` структура базы выглядит так:

```text
.memoryx/bases/default/
  cas/
  index/
  graph/
  meta/
    history.log
    sources.jsonl
    entities.jsonl
    relations.jsonl
  inverted/
```

CLI и MCP открывают одну и ту же durable store layout, поэтому повторные запуски работают с одной и той же базой.
`update_atom` создаёт новую версию атома и связь `SUPERSEDES`, `delete_atom` создаёт tombstone вместо физического удаления, а успешные write-операции дополнительно попадают в append-only `meta/history.log`. Последние записи можно посмотреть через CLI:
Зарегистрированные источники для доказательной provenance-модели хранятся в `meta/sources.jsonl`; через MCP доступны `register_source`, `list_sources` и `attach_atom_source`.
Высокоуровневое authoring API хранит entity/relation registry в `meta/entities.jsonl` и `meta/relations.jsonl`, но relation assertions всё равно создают реальные atoms/claims.

```bash
cargo +nightly run --release --bin memoryx -- history --base default --limit 20
```

## Статус Проекта

- Версия crate сейчас `0.1.0`, то есть это ещё pre-1.0 проект.
- Кодовая база рабочая и store-backed, но публичный API и wire formats проектно-специфичны.
- `mcp` - опциональный feature. Без него `serve` не поднимет MCP surface.
- `memoryx serve --stdio` уже даёт полный production MCP surface для работы с базой.
- `examples/mcp_server_full.rs` остаётся демонстрационной example-обвязкой, а не production entry point.
- Административные операции вроде `init`, `import`, `export`, `stats`, `compact`, `verify-integrity`, `rebuild-index` и `repair` остаются CLI-командами. MCP имеет read-only `history` tool для просмотра последних write-операций базы.

## Для Дальнейшего Чтения

- CLI entry point: `src/bin/memoryx.rs`
- Полный MCP example: `examples/mcp_server_full.rs`
- Нативное API: `examples/native_api.rs` и `examples/basic.rs`
- Общая структура crate: `src/lib.rs`

---

English summary below.

MemoryX is a local-first knowledge fabric built around the SKF-1.1 concept. It stores knowledge as atoms with provenance, claims, contexts, and graph links so answers can be assembled from a consistent subgraph instead of a pile of text chunks.

This repository is for people who need more than "similar text retrieval": conflicting sources, timelines, jurisdictional differences, auditable answers, and a knowledge base that can be queried from Rust or through MCP.

## What It Is

MemoryX is not a hosted app or a generic vector database wrapper. It is a Rust knowledge store with:

- content-addressed atoms and provenance
- context branching for conflicting claims
- lexical, semantic, and graph-backed retrieval
- a fixed-point query solver that builds an AnswerGraph
- `ClaimViewV2` output with epistemic status, confidence vector, modality, polarity, evidence, and provenance
- import/export, stats, compaction, and federation support
- MCP surfaces for assistant integrations

If you only need document search over chunks, MemoryX is usually the wrong tool. If you need answerability, provenance, and conflict handling, it is a better fit.

## MemoryX vs RAG

| Aspect | RAG | MemoryX |
| --- | --- | --- |
| Storage unit | Text chunks | Knowledge atoms with claims and evidence |
| Retrieval goal | Similar passages | Consistent answer graph |
| Contradictions | Often flattened or ignored | Kept as explicit conflicts or branches |
| Context | Usually global and fuzzy | Explicit contexts with policies |
| Explainability | "Found in document X" | Provenance plus supporting subgraph |
| Good fit | FAQ, docs, templated answers | Timelines, law, research, engineering, audit trails |

## Key Capabilities

- Knowledge atoms with BLAKE3-based content addressing.
- Contexts and conflicts as first-class objects.
- Heptapod-style query solving: backward wave, forward wave, fixed-point assembly.
- CAS storage with append-only segments and zero-copy reads.
- GraphStore, inverted index, and invariant checks.
- CRDT-backed metadata and federation support.
- MCP server modes for local assistants.

## Build Requirements

MemoryX uses unstable Rust features in `src/lib.rs`, so build it with nightly Rust.

- Rust nightly
- Windows 10/11 or Linux
- On Linux, `io_uring` is available for the storage layer; on Windows, the Windows async I/O stack is used.
- Release binaries are portable by default and are not tied to the build machine's Zen4 CPU.

## Quick Start

```bash
rustup toolchain install nightly
cargo +nightly build --release
```

For local CPU-specific benchmarking you may set `RUSTFLAGS="-C target-cpu=native"`, but do not publish that binary as a generic release. See `docs/PORTABLE_CPU_BUILDS.md`.

Initialize a base:

```bash
cargo +nightly run --release --bin memoryx -- init --base default
```

Ingest a file:

```bash
cargo +nightly run --release --bin memoryx -- ingest --base default facts.json
```

Query the base:

```bash
cargo +nightly run --release --bin memoryx -- query --base default "what does the base know about Rust ownership?"
```

Show stats:

```bash
cargo +nightly run --release --bin memoryx -- stats --base default
```

## Real Launch Modes

### CLI

The CLI binary is `src/bin/memoryx.rs`. It supports:

- `init`
- `ingest`
- `query`
- `compact`
- `export`
- `import`
- `stats`
- `serve`

Example:

```bash
cargo +nightly run --release --bin memoryx -- --help
```

Practical commands:

```bash
# Initialize a project-scoped base
cargo +nightly run --release --bin memoryx -- --base-scope project init --base default

# Import atoms from JSON
cargo +nightly run --release --bin memoryx -- import --base default --format json atoms.json

# Export atoms to CSV
cargo +nightly run --release --bin memoryx -- export --base default --format csv --output atoms.csv

# Start the production MCP server over stdio
cargo +nightly run --release --features mcp --bin memoryx -- serve --base default --stdio
```

### MCP core surface

`memoryx serve --stdio` is the core store-backed MCP transport. Build it with the `mcp` feature.

It exposes the full 24-tool production surface:

- `query`
- `search_lex`
- `search_graph`
- `search_semantic`
- `ingest`
- `batch_ingest`
- `update_atom`
- `delete_atom`
- `history`
- `register_source`
- `list_sources`
- `attach_atom_source`
- `create_entity`
- `list_entities`
- `alias_entity`
- `assert_relation`
- `correct_relation`
- `create_context`
- `list_contexts`
- `branch_context`
- `list_conflicts`
- `graph_neighbors`
- `graph_walk`
- `extract_subgraph`

Example:

```bash
cargo +nightly run --release --features mcp --bin memoryx -- serve --base default --stdio
```

### Full MCP example

`examples/mcp_server_full.rs` is a demonstrational stdio MCP server around the same store-backed operation set. The production MCP source of truth is `memoryx serve --stdio`:

- `query`
- `search_lex`
- `search_graph`
- `search_semantic`
- `ingest`
- `batch_ingest`
- `update_atom`
- `delete_atom`
- `history`
- `register_source`
- `list_sources`
- `attach_atom_source`
- `create_entity`
- `list_entities`
- `alias_entity`
- `assert_relation`
- `correct_relation`
- `create_context`
- `list_contexts`
- `branch_context`
- `list_conflicts`
- `graph_neighbors`
- `graph_walk`
- `extract_subgraph`

Example:

```bash
cargo +nightly run --release --features mcp --example mcp_server_full -- --base-scope project --base-name default
```

### HTTP federation server

`memoryx serve` without `--stdio` starts the HTTP federation server. This is not the MCP transport. It serves federation routes such as `/fetch`, `/negotiate`, `/sync`, `/discover`, and `/health`.

Example:

```bash
cargo +nightly run --release --features mcp --bin memoryx -- serve --base default --host 127.0.0.1 --port 8080
```

## Storage Layout

MemoryX does not use a generic `./data` default. Bases are resolved into scoped roots:

- project scope: `<repo>/.memoryx/bases/<name>`
- user scope: `<home>/.memoryx/bases/<name>`

If you pass a simple base name, the selected scope decides the root. If you pass a full path, it must stay inside one of those roots.

`memoryx init` creates this directory structure:

```text
.memoryx/bases/default/
  cas/
  index/
  graph/
  meta/
    history.log
    sources.jsonl
    entities.jsonl
    relations.jsonl
  inverted/
```

The full MCP example and the CLI both open the same store layout, so repeated runs against the same base load the same knowledge.
`update_atom` creates a new atom version with a `SUPERSEDES` edge, `delete_atom` creates a tombstone instead of physically erasing data, and successful write operations are appended to `meta/history.log`. Inspect recent operations with:
Registered sources for proof-grade provenance are stored in `meta/sources.jsonl`; MCP exposes `register_source`, `list_sources`, and `attach_atom_source`.
The high-level authoring API stores entity/relation registries in `meta/entities.jsonl` and `meta/relations.jsonl`, while relation assertions still create real atoms/claims.

```bash
cargo +nightly run --release --bin memoryx -- history --base default --limit 20
```

## Typical Commands

```bash
# Build the crate
cargo +nightly build --release

# Create a project-scoped base
cargo +nightly run --release --bin memoryx -- --base-scope project init --base default

# Import data from JSON
cargo +nightly run --release --bin memoryx -- import --base default --format json atoms.json

# Export data as CSV
cargo +nightly run --release --bin memoryx -- export --base default --format csv --output atoms.csv

# Verify and repair a base
cargo +nightly run --release --bin memoryx -- verify-integrity --base default
cargo +nightly run --release --bin memoryx -- rebuild-index --base default
cargo +nightly run --release --bin memoryx -- repair --base default
cargo +nightly run --release --bin memoryx -- history --base default --limit 20

# Run the full MCP example
cargo +nightly run --release --features mcp --example mcp_server_full -- --base-scope project --base-name default
```

## Repository Structure

```text
README.md
Cargo.toml
src/
  lib.rs
  prelude.rs
  bin/memoryx.rs
  cas/
  crdt/
  federation/
  graph/
  index/
  query/
  store/
  utils/
  vm/
examples/
  basic.rs
  mcp_server.rs
  mcp_server_full.rs
  native_api.rs
  rag_python.py
```

## Status And Limits

- The crate version is `0.1.0`, so treat this as pre-1.0 software.
- The codebase is functional and store-backed, but the API and wire formats are project-specific.
- `mcp` is an optional feature. Without it, `serve` will fail with a feature error.
- `memoryx serve --stdio` now exposes the full production MCP surface for working with the knowledge base.
- `examples/mcp_server_full.rs` remains a demonstrational example server, not the production entry point.
- Administrative operations such as `init`, `import`, `export`, `stats`, `compact`, `verify-integrity`, `rebuild-index`, and `repair` remain CLI commands rather than standalone MCP tools.

## Native Rust API

The crate exposes a native Rust API under `memoryx::store::api` and related modules. See `examples/native_api.rs` and `examples/basic.rs` for direct usage patterns.

## Further Reading

- CLI entry point: `src/bin/memoryx.rs`
- Full MCP example: `examples/mcp_server_full.rs`
- Full crate layout: `src/lib.rs`
