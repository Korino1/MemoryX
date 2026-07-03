# MemoryX

MemoryX is a local-first knowledge base for cases where simple text search or
classic RAG is not enough. It stores knowledge as small verifiable atoms with
claims, evidence, provenance, contexts, graph links, and conflict handling.

Instead of returning only "similar text chunks", MemoryX tries to assemble an
answer from a consistent proof-like subgraph. This makes it useful for project
memory, engineering decisions, research notes, audit trails, timelines,
contradicting sources, and assistant-accessible knowledge bases.

## What MemoryX Is

MemoryX is not a hosted SaaS product and not just a wrapper around a vector
database. It is a Rust knowledge-store engine with:

- Knowledge atoms with content-addressed identity.
- Claims with evidence, status, confidence components, and provenance.
- Contexts and branches for alternative assumptions or project-specific views.
- Explicit conflict tracking instead of silently merging contradictions.
- Lexical, semantic, and graph retrieval.
- A fixed-point solver that builds a structured `AnswerPack` and `AnswerGraph`.
- Durable local storage with history, tombstones, repair, and rebuild commands.
- MCP support so AI assistants can query and write to the knowledge base.
- Federation primitives for connecting compatible bases.

If you only need semantic search over document chunks, MemoryX is probably more
complex than necessary. If you need traceable answers, conflict visibility,
context control, and durable project memory, MemoryX is the intended tool.

## MemoryX vs Classic RAG

| Aspect | Classic RAG | MemoryX |
| --- | --- | --- |
| Storage unit | Text chunks | Knowledge atoms with claims and evidence |
| Main goal | Retrieve similar passages | Assemble a consistent answer graph |
| Contradictions | Often hidden or blended | Stored as conflicts or branches |
| Context | Usually implicit and global | Explicit contexts and policies |
| Explainability | "Found in document X" | Provenance plus supporting graph |
| Best fit | FAQ, documentation search | Research, engineering, audit, timelines, decision memory |

## Build Requirements

MemoryX currently uses nightly Rust.

```bash
rustup toolchain install nightly
cargo +nightly build --release
```

Portable release builds are the default. For local CPU-specific benchmarking you
may set `RUSTFLAGS="-C target-cpu=native"`, but do not publish that binary as a
generic release. See `docs/PORTABLE_CPU_BUILDS.md`.

## Quick Start

Create a local base:

```bash
cargo +nightly run --release --bin memoryx -- init --base default
```

Ingest data:

```bash
cargo +nightly run --release --bin memoryx -- ingest --base default facts.json
```

Query the base:

```bash
cargo +nightly run --release --bin memoryx -- query --base default "what does the base know about Rust ownership?"
```

Compile an editable query contract without executing it:

```bash
cargo +nightly run --release --bin memoryx -- --format json query --emit-contract "Explain MemoryX MCP"
```

Run a saved query contract and return structured output:

```bash
cargo +nightly run --release --bin memoryx -- --format json query --contract contract.json
```

Show base statistics:

```bash
cargo +nightly run --release --bin memoryx -- stats --base default
```

## CLI

The main binary is `memoryx`.

Common commands:

- `init`
- `ingest`
- `query`
- `import`
- `export`
- `stats`
- `compact`
- `verify-integrity`
- `rebuild-index`
- `repair`
- `history`
- `snapshot`
- `serve`

Help:

```bash
cargo +nightly run --release --bin memoryx -- --help
```

Useful examples:

```bash
# Create a project-scoped base
cargo +nightly run --release --bin memoryx -- --base-scope project init --base default

# Import atoms from JSON
cargo +nightly run --release --bin memoryx -- import --base default --format json atoms.json

# Export atoms to CSV
cargo +nightly run --release --bin memoryx -- export --base default --format csv --output atoms.csv

# Verify and repair a base
cargo +nightly run --release --bin memoryx -- verify-integrity --base default
cargo +nightly run --release --bin memoryx -- rebuild-index --base default
cargo +nightly run --release --bin memoryx -- repair --base default
```

## MCP For Assistants

Production MCP entry point:

```bash
cargo +nightly run --release --features mcp --bin memoryx -- serve --base default --stdio
```

`memoryx serve --stdio` exposes the store-backed MCP surface. It currently
provides 33 tools for querying, ingestion, updates, provenance, entities,
relations, contexts, conflicts, graph traversal, and history.

Important distinction:

- `memoryx serve --stdio` is the production MCP transport.
- `memoryx serve` without `--stdio` starts the HTTP federation server, not MCP.
- `examples/mcp_server_full.rs` is a demonstration example, not the production entry point.

Example MCP tool calls:

```json
{"name":"compile_query_contract","arguments":{"query_text":"Explain MemoryX MCP"}}
```

```json
{"name":"query","arguments":{"query_text":"What decisions mention persistence?","ctx_id":0}}
```

```json
{"name":"get_provenance_path","arguments":{"atom_id":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}}
```

## Storage Layout

MemoryX keeps bases in explicit scoped roots:

- Project scope: `<repo>/.memoryx/bases/<name>`
- User scope: `<home>/.memoryx/bases/<name>`

`memoryx init` creates a structure like:

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

CLI and MCP open the same durable store layout. `update_atom` writes a new
version and links it with `SUPERSEDES`; `delete_atom` creates a tombstone instead
of physically erasing data. Successful write operations are appended to
`meta/history.log`.

```bash
cargo +nightly run --release --bin memoryx -- history --base default --limit 20
```

## Repository Structure

```text
src/bin/memoryx.rs     CLI and MCP/federation server entry point
src/store/             High-level store API
src/cas/               Content-addressed atom storage
src/query/             Contracts, retrieval, solver, answer assembly
src/graph/             Graph store
src/crdt/              CRDT metadata, WAL, snapshots
src/federation/        Federation protocol
docs/                  User and integration documentation
examples/              CLI, MCP, native API examples
tests/                 Regression tests
benchmarks/            Honest RAG-comparison scaffold
```

## Documentation

- Query contracts: `docs/QUERY_CONTRACT.md`
- Answer packs: `docs/ANSWER_PACK.md`
- Authoring API: `docs/AUTHORING_API.md`
- LLM boundary: `docs/LLM_BOUNDARY.md`
- Portable CPU builds: `docs/PORTABLE_CPU_BUILDS.md`
- Benchmark scaffold: `docs/BENCHMARK_RAG_COMPARISON.md`
- Effectiveness benchmark plan: `docs/BENCHMARK_EFFECTIVENESS_PLAN.md`

## Status

- Current crate version: `0.1.0`.
- Public API and wire formats are pre-1.0 and may change.
- The codebase is store-backed and tested, but the project should still be
  treated as early-stage software.
- MCP is optional and requires the `mcp` feature.
- Administrative base maintenance commands such as `import`, `export`,
  `verify-integrity`, `rebuild-index`, and `repair` are CLI commands.

## License

This project is not published under an open-source license. Default terms are
proprietary / all rights reserved. See `LICENSE.md`.

---

# MemoryX на русском

MemoryX - локальная база знаний для случаев, где обычного поиска по тексту или
классического RAG недостаточно. Она хранит знания не большими текстовыми
чанками, а небольшими проверяемыми атомами: с утверждениями, доказательствами,
источниками, контекстами, связями графа и явной обработкой противоречий.

Вместо ответа из "похожих фрагментов текста" MemoryX пытается собрать ответ из
согласованного доказательного подграфа. Это полезно для памяти проекта,
инженерных решений, исследовательских заметок, аудита, временных линий,
конфликтующих источников и баз знаний, к которым подключается AI-ассистент.

## Что Это Такое

MemoryX - не облачный SaaS и не просто оболочка над vector database. Это
Rust-движок базы знаний с:

- атомами знания с content-addressed identity;
- утверждениями, evidence, статусом, confidence components и provenance;
- контекстами и ветками для разных предположений или проектов;
- явным хранением конфликтов вместо сглаживания противоречий;
- lexical, semantic и graph retrieval;
- solver-ом, который собирает структурированный `AnswerPack` и `AnswerGraph`;
- локальным durable storage, историей операций, tombstones, repair и rebuild;
- MCP-интерфейсом, чтобы ассистенты могли читать и вести базу;
- примитивами federation для совместимых баз.

Если нужен только semantic search по чанкам документов, MemoryX, скорее всего,
избыточен. Если нужны проверяемые ответы, видимые конфликты, контроль контекста
и долговременная память проекта, это целевой сценарий.

## MemoryX И Обычный RAG

| Аспект | Обычный RAG | MemoryX |
| --- | --- | --- |
| Единица хранения | Текстовые чанки | Атомы знания с claims и evidence |
| Цель | Найти похожие фрагменты | Собрать согласованный answer graph |
| Противоречия | Часто скрываются или смешиваются | Хранятся как conflicts или branches |
| Контекст | Обычно неявный и общий | Явные contexts и policies |
| Объяснимость | "Найдено в документе X" | Provenance плюс supporting graph |
| Лучший сценарий | FAQ, поиск по документации | Исследования, инженерия, аудит, timelines, память решений |

## Быстрый Старт

MemoryX сейчас собирается nightly Rust.

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

Получить статистику:

```bash
cargo +nightly run --release --bin memoryx -- stats --base default
```

## CLI

Основной бинарник - `memoryx`.

Справка:

```bash
cargo +nightly run --release --bin memoryx -- --help
```

Примеры:

```bash
# Создать базу в папке проекта
cargo +nightly run --release --bin memoryx -- --base-scope project init --base default

# Импортировать atoms из JSON
cargo +nightly run --release --bin memoryx -- import --base default --format json atoms.json

# Экспортировать atoms в CSV
cargo +nightly run --release --bin memoryx -- export --base default --format csv --output atoms.csv

# Проверить и восстановить базу
cargo +nightly run --release --bin memoryx -- verify-integrity --base default
cargo +nightly run --release --bin memoryx -- rebuild-index --base default
cargo +nightly run --release --bin memoryx -- repair --base default
```

## MCP Для Ассистентов

Production MCP запускается так:

```bash
cargo +nightly run --release --features mcp --bin memoryx -- serve --base default --stdio
```

`memoryx serve --stdio` открывает MCP surface базы. Сейчас доступно 33
инструмента для query, ingestion, updates, provenance, entities, relations,
contexts, conflicts, graph traversal и history.

Важно:

- `memoryx serve --stdio` - production MCP transport.
- `memoryx serve` без `--stdio` запускает HTTP federation server, это не MCP.
- `examples/mcp_server_full.rs` - демонстрационный пример, не production entry point.

## Где Хранится База

MemoryX хранит базы в явных scoped roots:

- Project scope: `<repo>/.memoryx/bases/<name>`
- User scope: `<home>/.memoryx/bases/<name>`

После `memoryx init` структура выглядит так:

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

CLI и MCP открывают одну и ту же durable store layout. `update_atom` создаёт
новую версию и связь `SUPERSEDES`; `delete_atom` создаёт tombstone вместо
физического удаления. Успешные write-операции пишутся в `meta/history.log`.

## Статус

- Текущая версия crate: `0.1.0`.
- Public API и wire formats пока pre-1.0 и могут меняться.
- Кодовая база рабочая и покрыта тестами, но проект всё ещё ранней стадии.
- MCP опционален и требует feature `mcp`.
- Административные операции обслуживания базы остаются CLI-командами.

## Лицензия

Проект не публикуется под open-source лицензией. Условия по умолчанию:
proprietary / all rights reserved. См. `LICENSE.md`.
