# MemoryX
                    It is NOT a RAG
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
| Source of truth | Retrieved text plus model interpretation | Stored atoms, claims, evidence, contexts, and graph links |
| Retrieval role | Retrieval often drives the final answer | Retrieval proposes candidates; validation and solver decide |
| Query control | Prompt instructions and top-k settings | Explicit `QueryContract` with constraints and policies |
| Reasoning path | Query -> retrieve chunks -> generate text | Backward gaps + forward candidates + fixed-point answer assembly |
| Output | Usually generated prose | Structured `AnswerPack` and proof-style `AnswerGraph` |
| Contradictions | Often hidden, blended, or resolved by the model | Stored as conflicts, alternatives, or branches |
| Missing evidence | Can become hallucinated text | Reported as unknowns, limitations, gaps, or insufficient evidence |
| Context | Usually implicit and global | Explicit contexts, branches, project/user scopes, and policies |
| Temporal changes | Old chunks can be retrieved as current | History, `SUPERSEDES`, tombstones, snapshots, and temporal policy |
| Explainability | "Found in document X" | Claim/evidence/source provenance plus supporting graph |
| Reproducibility | Depends on model, prompt, and retrieval state | Snapshot + query contract + structured answer state |
| Multi-project work | Usually separate indexes or conventions | Scoped bases plus Multi-Base MCP routing |
| Assistant operations | Often query-only retrieval endpoint | MCP read/write/admin tools for maintaining the knowledge base |
| Federation | Often merges retrieved text | Compatible claims/provenance/metadata between bases |
| Durability | Index rebuild depends on external document pipeline | CAS integrity, repair/rebuild, history, and snapshots |
| Best fit | FAQ and documentation search | Research, engineering, audit, timelines, decision memory, agent memory |

Full comparison: `docs/MEMORYX_VS_RAG.md`.

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
provides 38 tools for querying, ingestion, updates, provenance, entities,
relations, contexts, conflicts, graph traversal, history, and multi-base
routing.

Important distinction:

- `memoryx serve --stdio` is the production MCP transport.
- `memoryx serve` without `--stdio` starts the HTTP federation server, not MCP.
- `examples/mcp_server_full.rs` is a demonstration example, not the production entry point.

### MCP Client Configuration

Most MCP-capable IDEs and agent clients use a JSON object shaped like
`mcpServers`. Adjust the command path if you run a prebuilt `memoryx.exe`
instead of `cargo`.

Project-local base:

```json
{
  "mcpServers": {
    "memoryx-project": {
      "command": "cargo",
      "args": [
        "+nightly",
        "run",
        "--release",
        "--features",
        "mcp",
        "--bin",
        "memoryx",
        "--",
        "--base-scope",
        "project",
        "serve",
        "--base",
        "default",
        "--stdio"
      ]
    }
  }
}
```

Shared user-level base:

```json
{
  "mcpServers": {
    "memoryx-user": {
      "command": "cargo",
      "args": [
        "+nightly",
        "run",
        "--release",
        "--features",
        "mcp",
        "--bin",
        "memoryx",
        "--",
        "--base-scope",
        "user",
        "serve",
        "--base",
        "default",
        "--stdio"
      ]
    }
  }
}
```

Prebuilt executable example:

```json
{
  "mcpServers": {
    "memoryx": {
      "command": "E:\\Memory bank\\memoryx.exe",
      "args": [
        "--base-scope",
        "project",
        "serve",
        "--base",
        "default",
        "--stdio"
      ]
    }
  }
}
```

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

Multi-base MCP workflow:

```json
{"name":"list_bases","arguments":{}}
```

```json
{"name":"connect_base","arguments":{"base_ref":"project:client-a","scope":"project","name":"client-a"}}
```

```json
{"name":"query_base","arguments":{"base_ref":"project:client-a","query_text":"What decisions mention persistence?","ctx_id":0}}
```

```json
{"name":"switch_base","arguments":{"base_ref":"project:client-a"}}
```

Existing store-backed MCP tools use the active base by default. Most of them can
also accept `base_ref` to operate on a connected base without changing the
active base.

Different physical bases can be used in parallel by different MCP clients. One
physical base root has exactly one mutable owner at a time: a second process is
rejected with an explicit writer-lease error before it can open mutable store
components. If several applications need the same live logical base, route them
through one coordinating owner service or use separate replicas synchronized
through CRDT/federation; do not point independent writer processes at the same
directory.

### MCP Tool Map

| Category | Tools | Purpose |
| --- | --- | --- |
| Query and proof output | `query`, `query_base`, `compile_query_contract`, `validate_query_contract`, `explain_answer_graph`, `get_provenance_path` | Compile strict queries, execute them, inspect proof graph output, and trace evidence. |
| Multi-base routing | `list_bases`, `active_base`, `connect_base`, `switch_base` | Discover project/user bases, connect additional bases, and choose the active base. |
| Retrieval | `search_lex`, `search_graph`, `search_semantic` | Search lexical, graph, and semantic indexes without treating retrieval as final truth. |
| Atom writes and history | `ingest`, `batch_ingest`, `update_atom`, `delete_atom`, `history` | Add atoms, batch-write atoms, create superseding versions, create tombstones, and inspect recent operations. |
| Claim correction | `supersede_claim`, `correct_claim`, `correct_relation` | Replace outdated or incorrect knowledge while keeping provenance and history. |
| Sources and provenance | `register_source`, `list_sources`, `attach_atom_source` | Register source records and attach atoms to source/provenance paths. |
| Entities and relations | `create_entity`, `list_entities`, `alias_entity`, `merge_entities`, `split_entity`, `add_claim`, `assert_relation` | Maintain structured entities, aliases, entity merges/splits, claims, and relations. |
| Contexts and conflicts | `create_context`, `list_contexts`, `branch_context`, `list_conflicts` | Create contexts, branch assumptions, and inspect unresolved conflicts. |
| Graph traversal | `graph_neighbors`, `graph_walk`, `extract_subgraph` | Traverse graph links and extract a local proof/reasoning subgraph. |

Recommended agent workflow:

1. Call `active_base` or `list_bases` first.
2. Use `connect_base` if the needed project/user base is not connected.
3. Use `compile_query_contract` for non-trivial questions.
4. Use `query` or `query_base` with `base_ref` for answer assembly.
5. Use `get_provenance_path`, `explain_answer_graph`, or `extract_subgraph`
   before presenting factual claims to a user.
6. Use write tools only when the user explicitly asks the agent to update the
   knowledge base.

## Storage Layout

MemoryX keeps bases in explicit scoped roots:

- Project scope: `<repo>/.memoryx/bases/<name>`
- User scope: `<home>/.memoryx/bases/<name>`

Each opened base contains a persistent `.memoryx.writer.lock` file. The file is
not a stale-lock sentinel: the operating system lock is held only for the owner
process lifetime and is released automatically on normal exit or process death.

The user chooses the storage location with `--base-scope`:

```bash
# Store the base inside the current project folder
cargo +nightly run --release --bin memoryx -- --base-scope project init --base default

# Store the base in the shared user-level MemoryX folder
cargo +nightly run --release --bin memoryx -- --base-scope user init --base default
```

MCP uses the same choice:

```bash
# MCP uses the project-local base
cargo +nightly run --release --features mcp --bin memoryx -- --base-scope project serve --base default --stdio

# MCP uses the shared user-level base
cargo +nightly run --release --features mcp --bin memoryx -- --base-scope user serve --base default --stdio
```

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
- MemoryX vs RAG: `docs/MEMORYX_VS_RAG.md`
- Benchmark scaffold: `docs/BENCHMARK_RAG_COMPARISON.md`
- Contributing: `CONTRIBUTING.md`
- Security policy: `SECURITY.md`

## Status

- Current crate version: `1.0.3`.
- Public API and wire formats are stable for the 1.0 release line. Breaking
  changes should use a new major version.
- The codebase is store-backed and tested, but users should still validate
  deployment behavior for their own workloads.
- MCP is optional and requires the `mcp` feature.
- Administrative base maintenance commands such as `import`, `export`,
  `verify-integrity`, `rebuild-index`, and `repair` are CLI commands.

## Maintainer And Development Assistance

- Project creator and maintainer: Korino1.
- Development assistance: OpenAI Codex was used for implementation, review,
  documentation, testing, and release preparation.

## License

MemoryX is licensed under `AGPL-3.0-or-later` for open-source use.

Commercial licensing is available separately for organizations that cannot use
AGPL software. See `LICENSE.md` and `COMMERCIAL_LICENSE.md`.

Future external contributions require a CLA or equivalent agreement so the
project can preserve dual licensing. See `CLA.md` and `CONTRIBUTING.md`.

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
| Источник истины | Найденный текст плюс интерпретация модели | Атомы, утверждения, evidence, contexts и graph links |
| Роль retrieval | Retrieval часто фактически ведёт к ответу | Retrieval предлагает candidates; validation и solver принимают решение |
| Управление запросом | Prompt-инструкции и top-k настройки | Явный `QueryContract` с constraints и policies |
| Ход рассуждения | Query -> retrieve chunks -> generate text | Backward gaps + forward candidates + fixed-point сборка ответа |
| Выход | Обычно сгенерированный текст | Structured `AnswerPack` и proof-style `AnswerGraph` |
| Противоречия | Часто скрываются, смешиваются или решаются моделью | Хранятся как conflicts, alternatives или branches |
| Недостающие факты | Могут превратиться в галлюцинацию | Возвращаются как unknowns, limitations, gaps или insufficient evidence |
| Контекст | Обычно неявный и общий | Явные contexts, branches, project/user scopes и policies |
| Временные изменения | Старые chunks могут выдаваться как текущие | History, `SUPERSEDES`, tombstones, snapshots и temporal policy |
| Объяснимость | "Найдено в документе X" | Claim/evidence/source provenance плюс supporting graph |
| Воспроизводимость | Зависит от модели, prompt и retrieval state | Snapshot + query contract + structured answer state |
| Несколько проектов | Обычно отдельные индексы или соглашения | Scoped bases плюс Multi-Base MCP routing |
| Работа ассистента | Часто только query endpoint | MCP read/write/admin tools для ведения базы |
| Федерация | Часто объединяет найденный текст | Совместимые claims/provenance/metadata между базами |
| Надёжность хранения | Rebuild index зависит от внешнего document pipeline | CAS integrity, repair/rebuild, history и snapshots |
| Лучший сценарий | FAQ и поиск по документации | Исследования, инженерия, аудит, timelines, память решений, agent memory |

Полное сравнение: `docs/MEMORYX_VS_RAG.md`.

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

`memoryx serve --stdio` открывает MCP surface базы. Сейчас доступно 38
инструментов для query, ingestion, updates, provenance, entities, relations,
contexts, conflicts, graph traversal, history и multi-base routing.

Важно:

- `memoryx serve --stdio` - production MCP transport.
- `memoryx serve` без `--stdio` запускает HTTP federation server, это не MCP.
- `examples/mcp_server_full.rs` - демонстрационный пример, не production entry point.

### MCP Конфигурация Клиента

Большинство IDE и AI-agent клиентов с MCP используют JSON вида `mcpServers`.
Если запускается готовый `memoryx.exe`, замените `command` и `args` на путь к
исполняемому файлу.

Project-local base:

```json
{
  "mcpServers": {
    "memoryx-project": {
      "command": "cargo",
      "args": [
        "+nightly",
        "run",
        "--release",
        "--features",
        "mcp",
        "--bin",
        "memoryx",
        "--",
        "--base-scope",
        "project",
        "serve",
        "--base",
        "default",
        "--stdio"
      ]
    }
  }
}
```

User-level base:

```json
{
  "mcpServers": {
    "memoryx-user": {
      "command": "cargo",
      "args": [
        "+nightly",
        "run",
        "--release",
        "--features",
        "mcp",
        "--bin",
        "memoryx",
        "--",
        "--base-scope",
        "user",
        "serve",
        "--base",
        "default",
        "--stdio"
      ]
    }
  }
}
```

Пример с готовым exe:

```json
{
  "mcpServers": {
    "memoryx": {
      "command": "E:\\Memory bank\\memoryx.exe",
      "args": [
        "--base-scope",
        "project",
        "serve",
        "--base",
        "default",
        "--stdio"
      ]
    }
  }
}
```

Multi-base MCP workflow:

```json
{"name":"list_bases","arguments":{}}
```

```json
{"name":"connect_base","arguments":{"base_ref":"project:client-a","scope":"project","name":"client-a"}}
```

```json
{"name":"query_base","arguments":{"base_ref":"project:client-a","query_text":"What decisions mention persistence?","ctx_id":0}}
```

```json
{"name":"switch_base","arguments":{"base_ref":"project:client-a"}}
```

Старые MCP tools используют active base по умолчанию. Большинство
store-backed tools также могут принять `base_ref`, чтобы работать с
подключённой базой без смены active base.

Разные физические базы могут параллельно использоваться разными MCP-клиентами.
У одного физического корня базы одновременно может быть только один процесс,
изменяющий данные: второй процесс получит явную ошибку writer lease до открытия
изменяемых компонентов хранилища. Если нескольким приложениям нужна одна живая
логическая база, они должны работать через один координирующий owner-сервис или
через отдельные реплики с синхронизацией CRDT/federation. Нельзя направлять два
независимых writer-процесса в одну папку.

### Карта MCP Инструментов

| Категория | Tools | Назначение |
| --- | --- | --- |
| Query и proof output | `query`, `query_base`, `compile_query_contract`, `validate_query_contract`, `explain_answer_graph`, `get_provenance_path` | Компилировать строгие запросы, выполнять их, смотреть answer graph и evidence. |
| Multi-base routing | `list_bases`, `active_base`, `connect_base`, `switch_base` | Найти project/user базы, подключить дополнительные базы и выбрать active base. |
| Retrieval | `search_lex`, `search_graph`, `search_semantic` | Искать по lexical, graph и semantic индексам без превращения retrieval в источник истины. |
| Atom writes и history | `ingest`, `batch_ingest`, `update_atom`, `delete_atom`, `history` | Добавлять atoms, batch-write, создавать superseding versions, tombstones и смотреть историю. |
| Claim correction | `supersede_claim`, `correct_claim`, `correct_relation` | Исправлять устаревшие или неверные знания с сохранением provenance/history. |
| Sources и provenance | `register_source`, `list_sources`, `attach_atom_source` | Регистрировать источники и связывать atoms с source/provenance paths. |
| Entities и relations | `create_entity`, `list_entities`, `alias_entity`, `merge_entities`, `split_entity`, `add_claim`, `assert_relation` | Вести entities, aliases, merges/splits, claims и relations. |
| Contexts и conflicts | `create_context`, `list_contexts`, `branch_context`, `list_conflicts` | Создавать contexts, ветвить assumptions и смотреть unresolved conflicts. |
| Graph traversal | `graph_neighbors`, `graph_walk`, `extract_subgraph` | Обходить graph links и извлекать локальный proof/reasoning subgraph. |

Рекомендуемый workflow для AI-агента:

1. Сначала вызвать `active_base` или `list_bases`.
2. Если нужная база не подключена, использовать `connect_base`.
3. Для сложных вопросов использовать `compile_query_contract`.
4. Для ответа использовать `query` или `query_base` с `base_ref`.
5. Перед утверждениями пользователю проверять `get_provenance_path`,
   `explain_answer_graph` или `extract_subgraph`.
6. Write tools использовать только если пользователь явно попросил обновить
   базу знаний.

## Где Хранится База

MemoryX хранит базы в явных scoped roots:

- Project scope: `<repo>/.memoryx/bases/<name>`
- User scope: `<home>/.memoryx/bases/<name>`

В каждой открытой базе сохраняется файл `.memoryx.writer.lock`. Это не маркер
устаревшей блокировки: операционная система удерживает lock только пока жив
процесс-владелец и автоматически освобождает его при штатном завершении или
аварийной остановке процесса.

Пользователь выбирает место хранения через `--base-scope`:

```bash
# Хранить базу внутри текущей папки проекта
cargo +nightly run --release --bin memoryx -- --base-scope project init --base default

# Хранить базу в общей пользовательской папке MemoryX
cargo +nightly run --release --bin memoryx -- --base-scope user init --base default
```

MCP использует тот же выбор:

```bash
# MCP работает с базой внутри папки проекта
cargo +nightly run --release --features mcp --bin memoryx -- --base-scope project serve --base default --stdio

# MCP работает с общей пользовательской базой
cargo +nightly run --release --features mcp --bin memoryx -- --base-scope user serve --base default --stdio
```

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

- Текущая версия crate: `1.0.3`.
- Public API и wire formats стабильны для release line 1.0. Breaking changes
  должны идти через новую major version.
- Кодовая база рабочая и покрыта тестами, но пользователям всё равно нужно
  проверять поведение на своих workload-ах.
- MCP опционален и требует feature `mcp`.
- Административные операции обслуживания базы остаются CLI-командами.

## Мейнтейнер И Участие В Разработке

- Автор и мейнтейнер проекта: Korino1.
- Помощь в разработке: OpenAI Codex использовался для реализации, review,
  документации, тестирования и подготовки release.

## Лицензия

MemoryX лицензируется как open source под `AGPL-3.0-or-later`.

Для компаний и продуктов, которым не подходит AGPL, возможна отдельная
коммерческая лицензия по письменному соглашению. См. `LICENSE.md` и
`COMMERCIAL_LICENSE.md`.

Для будущих внешних вкладов потребуется CLA или аналогичное соглашение, чтобы
сохранить возможность двойного лицензирования.
