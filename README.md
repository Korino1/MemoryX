# MemoryX

MemoryX is a local-first knowledge base for information that must remain
traceable over time. It stores small knowledge atoms instead of treating a
document chunk or a search hit as the final truth. Atoms can contain claims,
evidence, source information, contexts, graph links, history, and explicit
conflicts.

MemoryX is useful when an assistant or a person needs to:

- keep project decisions and research notes locally;
- see which source supports an answer;
- keep contradictory or outdated claims instead of silently blending them;
- distinguish current knowledge from superseded knowledge;
- query several project or user bases;
- repair and verify durable local storage;
- expose the knowledge base to Codex or another MCP client.

MemoryX is probably unnecessary if all you need is ordinary full-text or
semantic search through document chunks. It is not a hosted service and it does
not replace a model: retrieval proposes candidates, while validation and the
`FixedPointSolver` assemble a structured answer from the stored state. The
answer can include an `AnswerPack`, an `AnswerGraph`, evidence paths, gaps, and
conflicts rather than unsupported text.

## What It Includes

- Knowledge atoms with content-addressed identity.
- Claims, evidence, source provenance, contexts, branches, and conflict tracking.
- Lexical, semantic, and graph search.
- Fixed-point answer assembly through `FixedPointSolver`.
- Durable `CAS` storage with history, tombstones, integrity checks, and repair.
- `CRDT` metadata, `WAL`, snapshots, and rebuildable indexes.
- MCP access with 45 tools for reading, writing, provenance, graph work, and
  multiple bases.
- Federation primitives for compatible bases.
- Portable release builds by default, with explicit CPU-specific builds only
  when they are intentionally labelled.

## MemoryX And Ordinary RAG

MemoryX is not a universal replacement for RAG. They solve different parts of
the problem:

| Need | Ordinary RAG | MemoryX |
| --- | --- | --- |
| Find relevant documentation quickly | Good fit | Supported, but may be more than needed |
| Keep a verifiable unit of knowledge | Usually keeps text chunks | Keeps atoms, claims, evidence, and sources |
| Handle two sources that disagree | May blend or rank the passages | Keeps the conflict or separates contexts |
| Distinguish current and old decisions | Depends on document filtering | Keeps versions, superseding links, and history |
| Explain why an answer was accepted | Usually cites retrieved passages | Returns claims, provenance, limitations, and an answer graph |
| Say that evidence is missing | Depends on the prompt and model | Has explicit insufficient-evidence and limitation states |

Use ordinary RAG for straightforward document search. Use MemoryX when
traceability, conflicting facts, history, controlled contexts, or durable
assistant memory matter. See the
[full capability comparison](docs/MEMORYX_VS_RAG.md) and the
[reproducible benchmark](docs/BENCHMARK_RAG_COMPARISON.md).

## Install A Ready EXE

MemoryX is a command-line program; no installer is required. If you already
have a prepared `memoryx.exe`, place it in a stable directory, for example
`C:\Tools\MemoryX\memoryx.exe`, and check it:

```powershell
$exe = "C:\Tools\MemoryX\memoryx.exe"
& $exe --version
& $exe --help
```

For MCP, use an executable that was built with the `mcp` feature. Check that
the command is available with `& $exe serve --help`. The examples below use
PowerShell; the same arguments work from other shells.

## Build From Source

The repository currently uses nightly Rust. Install the toolchain and build a
portable release with MCP support:

```powershell
rustup toolchain install nightly
cargo +nightly build --release --features mcp
```

The resulting executable is:

```text
target/release/memoryx.exe
```

Without `--features mcp`, the command-line database functions can still be
built, but the MCP server is not included. The default release is portable.
For local CPU-specific builds, see [`docs/PORTABLE_CPU_BUILDS.md`](docs/PORTABLE_CPU_BUILDS.md)
and do not publish a machine-specific binary as a generic release.

## First Steps

Create a project base, add data, query it, and inspect its statistics:

```powershell
$exe = ".\target\release\memoryx.exe"

& $exe --base-scope project init --base default
& $exe --base-scope project ingest --base default facts.json
& $exe --base-scope project query --base default "What decisions are stored?"
& $exe --base-scope project stats --base default
```

For a personal base shared by projects, use `user` instead of `project`:

```powershell
& $exe --base-scope user init --base default
& $exe --base-scope user ingest --base default facts.json
```

Use `--help` for all command options. Useful maintenance commands are
`import`, `export`, `history`, `snapshot`, `compact`, `verify-integrity`,
`rebuild-index`, and `repair`.

## Project And User Bases

The scope controls the physical root of a base:

| Scope | Physical path |
| --- | --- |
| `project` | `<repo>/.memoryx/bases/<name>` |
| `user` | `<home>/.memoryx/bases/<name>`; on Windows this is normally `%USERPROFILE%\\.memoryx\\bases\\<name>` |

For example, a project base named `default` is stored in:

```text
<repo>/.memoryx/bases/default/
  cas/
  index/
  graph/
  meta/
    history.log
    sources.jsonl
    atom_sources.jsonl
    predicates.jsonl
    entities.jsonl
    relations.jsonl
  inverted/
```

The command-line interface and MCP open the same durable store. `update_atom`
creates a new version linked with `SUPERSEDES`; `delete_atom` creates a
tombstone instead of physically erasing the old atom. Successful mutations are
recorded in `meta/history.log`.

One physical base may have only one process holding the write lease at a time.
Do not point two independent writers at the same directory. A running
`memoryx serve --stdio` publishes a loopback-only local control endpoint. A
second `serve --stdio` for the same base transparently becomes an MCP proxy to
the live owner instead of opening another writer. Scripts can call the owner
directly with
`memoryx --format json client --base <path> --tool get_stats --arguments '{}'`
or use `verify_integrity` in the same way. Different physical bases may still
be owned and used in parallel. See
[`docs/LIVE_OWNER_CONTROL.md`](docs/LIVE_OWNER_CONTROL.md).

## Connect MCP To Codex Or An IDE

`memoryx serve --stdio` is the production MCP transport. `memoryx serve` without
`--stdio` starts the HTTP federation server, not MCP. Put an `mcpServers` block
like this into the MCP configuration of Codex or the IDE. The exact location of
that configuration is client-specific.

For a ready executable and a project base:

```json
{
  "mcpServers": {
    "memoryx-project": {
      "command": "C:\\Tools\\MemoryX\\memoryx.exe",
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

For a user base, change `project` to `user`. For a source checkout instead of
an executable, use:

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

After connecting, ask the client to call `active_base` or `list_bases` first.
The MCP server preserves human-readable `content` and also returns
`structuredContent`. Successful mutations report `durability: "committed"`.

## Safe Use Of The 45 MCP Tools

Use the tools in this order:

1. Call `active_base` or `list_bases` before reading or writing.
2. Use `connect_base` for another base and pass its `base_ref` to tools that
   support it; use `switch_base` only when changing the active base is intended.
3. For a complex question, call `compile_query_contract`, then
   `validate_query_contract`, then `query` or `query_base`.
4. Before presenting an important fact, inspect `get_provenance_path`,
   `explain_answer_graph`, or `extract_subgraph`.
5. Use write tools only after the user explicitly asks to change the base.

Exact selectors fail closed: an unresolved selector returns `NoMatch` instead
of broadening the request to the whole base. Normal queries use current atom
versions. Historical queries must explicitly set
`temporal_scope.require_current` to `false`. Read-time conflict branches are
not saved. `register_predicate` IDs must be obtained through
`resolve_predicate`; do not invent numeric IDs. `attach_atom_source` preserves
distinct sources and is idempotent for the same atom/source pair.

The complete tool surface is grouped below. The names are exact MCP tool names.

| Category | Tools | Safe purpose |
| --- | --- | --- |
| Query and answer evidence | `query`, `query_base`, `compile_query_contract`, `validate_query_contract`, `explain_answer_graph`, `get_provenance_path` | Build, validate, answer, and inspect the evidence path. |
| Base selection | `list_bases`, `active_base`, `connect_base`, `switch_base` | Find, connect, and select bases. |
| Search | `search_lex`, `search_graph`, `search_semantic` | Search candidate knowledge without treating search alone as the final answer. |
| Atom writes and history | `ingest`, `batch_ingest`, `update_atom`, `delete_atom`, `history` | Add, revise, remove through tombstones, and review changes. |
| State and claim correction | `supersede_claim`, `correct_claim`, `correct_relation`, `transition_relation` | Replace outdated knowledge while keeping history; use `transition_relation` for one current relation value. |
| Validation and metrics | `get_stats`, `verify_integrity` | Inspect unambiguous logical/physical counts and verify the live base through its owner. |
| Sources | `register_source`, `list_sources`, `attach_atom_source` | Register sources and attach them to atoms. |
| Predicate contracts | `register_predicate`, `list_predicates`, `get_predicate`, `resolve_predicate` | Define and resolve stable relation types. |
| Entities and relations | `create_entity`, `list_entities`, `alias_entity`, `merge_entities`, `split_entity`, `add_claim`, `assert_relation` | Maintain entities, aliases, claims, and relations. |
| Contexts and conflicts | `create_context`, `list_contexts`, `branch_context`, `list_conflicts` | Separate assumptions and inspect unresolved conflicts. |
| Graph traversal | `graph_neighbors`, `graph_walk`, `extract_subgraph` | Inspect connected knowledge and local reasoning subgraphs. |

## Multiple Bases

One MCP process can connect several compatible bases. A typical sequence is:

```json
{"name":"list_bases","arguments":{}}
```

```json
{"name":"connect_base","arguments":{"base_ref":"project:client-a","scope":"project","name":"client-a"}}
```

```json
{"name":"query_base","arguments":{"base_ref":"project:client-a","query_text":"Which decisions mention persistence?","ctx_id":0}}
```

```json
{"name":"switch_base","arguments":{"base_ref":"project:client-a"}}
```

Use `base_ref` to query or update a connected base without changing the active
base whenever the selected tool supports that field.

## Backup, Snapshot, And Repair

`snapshot` reports the logical identity of the current knowledge state. It is
useful for reproducibility, but it is not a copy of the files.

For a filesystem backup, stop every process that writes to the base, then copy
the complete base directory, including `cas`, `index`, `graph`, `meta`, and
`inverted`. Do not copy a live base while another process is writing to it.

Check and repair a project base with:

```powershell
& $exe --base-scope project verify-integrity --base default
& $exe --base-scope project rebuild-index --base default
& $exe --base-scope project repair --base default
```

`verify-integrity` checks the stored data. `rebuild-index` rebuilds derived
indexes. `repair` performs a safe repair sequence with a final integrity check.
`compact --dry-run` can be used to inspect compaction before changing storage.
Maintenance commands need exclusive write access to the physical base.

## Reproducible Benchmark

The repository includes a presentation-ready comparison harness. It calculates
metrics only from real recorded outputs and never fills missing scores with
favorable values:

- [`docs/BENCHMARK_RAG_COMPARISON.md`](docs/BENCHMARK_RAG_COMPARISON.md)
- [`benchmarks/run_rag_comparison.ps1`](benchmarks/run_rag_comparison.ps1)
- [`benchmarks/rag_comparison/cases.jsonl`](benchmarks/rag_comparison/cases.jsonl)
- [`benchmarks/rag_comparison/corpus.jsonl`](benchmarks/rag_comparison/corpus.jsonl)

Validate the frozen inputs and runner:

```powershell
pwsh -NoLogo -File benchmarks/run_rag_comparison.ps1 -Mode validate
pwsh -NoLogo -File benchmarks/run_rag_comparison.ps1 -Mode selftest
```

The harness prepares paired runs, records MemoryX and RAG adapter outputs,
calculates Accuracy, Recall, Groundedness, end-to-end latency, closed-case
rate, and functional scenario rates, then writes JSON and Markdown reports.
The repository does not contain measured superiority results. Real adapters,
raw outputs, reviewer decisions, and runtime configuration are required before
presenting numbers.

## License

MemoryX is available under `AGPL-3.0-or-later`. The full open-source license is
in [`LICENSE.md`](LICENSE.md) and [`COPYING`](COPYING). A separate commercial
license may be available by written agreement; see
[`COMMERCIAL_LICENSE.md`](COMMERCIAL_LICENSE.md).

External contributions are covered by [`CLA.md`](CLA.md) and
[`CONTRIBUTING.md`](CONTRIBUTING.md).

---

# MemoryX: русская версия

MemoryX — локальная база знаний для информации, которую нужно проверять и
сохранять во времени. Она хранит небольшие атомы знаний, а не считает один
фрагмент документа или результат поиска окончательной истиной. В атомах могут
храниться утверждения, подтверждения, сведения об источнике, контексты, связи
графа, история и явные противоречия.

MemoryX подходит, если нужно:

- хранить решения проекта и исследовательские заметки на своём компьютере;
- видеть, какой источник подтверждает ответ;
- сохранять противоречивые или устаревшие утверждения, а не незаметно смешивать их;
- отличать текущие знания от заменённых предыдущих версий;
- запрашивать несколько проектных или пользовательских баз;
- проверять и восстанавливать постоянное локальное хранилище;
- подключать базу к Codex или другому клиенту MCP.

MemoryX, скорее всего, не нужна, если требуется только обычный поиск по тексту
или смысловой поиск по фрагментам документов. Это не облачная служба и не
замена модели: поиск предлагает кандидатов, а проверка и `FixedPointSolver`
собирают структурированный ответ из сохранённых данных. Ответ может содержать
`AnswerPack`, `AnswerGraph`, пути к подтверждениям, пробелы в данных и
противоречия, а не неподтверждённый текст.

## Возможности

- Атомы знаний с идентичностью, вычисляемой по содержимому.
- Утверждения, подтверждения, источники, контексты, ветви и учёт противоречий.
- Поиск по словам, смыслу и связям графа.
- Сборка ответа через `FixedPointSolver`.
- Постоянное хранилище `CAS` с историей, метками удаления, проверкой целостности и восстановлением.
- Служебные данные `CRDT`, журнал `WAL`, снимки состояния и перестраиваемые индексы.
- Доступ через MCP: 45 инструментов для чтения, записи, источников, графа и нескольких баз.
- Средства объединения совместимых баз.
- Переносимая сборка по умолчанию и отдельные сборки под процессор только при явном обозначении.

## MemoryX и обычный RAG

MemoryX не является универсальной заменой RAG. Эти системы решают разные части
задачи:

| Потребность | Обычный RAG | MemoryX |
| --- | --- | --- |
| Быстро найти подходящий фрагмент документа | Подходит | Поддерживается, но может быть избыточно |
| Хранить проверяемую единицу знания | Обычно хранит фрагменты текста | Хранит атомы, утверждения, подтверждения и источники |
| Обработать несогласные источники | Может смешать или ранжировать фрагменты | Сохраняет противоречие или разделяет контексты |
| Отличить текущее решение от старого | Зависит от фильтрации документов | Хранит версии, связи замещения и историю |
| Объяснить, почему ответ принят | Обычно указывает найденные фрагменты | Возвращает утверждения, источники, ограничения и граф ответа |
| Сообщить, что данных недостаточно | Зависит от инструкции и модели | Имеет явные состояния недостатка подтверждений |

Обычный RAG удобен для прямого поиска по документам. MemoryX нужен, когда
важны проверяемость, противоречия, история, управляемые контексты или постоянная
память ассистента. См. [полное сравнение возможностей](docs/MEMORYX_VS_RAG.md)
и [воспроизводимый бенчмарк](docs/BENCHMARK_RAG_COMPARISON.md).

## Установка готового EXE

MemoryX — программа командной строки, отдельная программа установки не нужна.
Если у вас уже есть подготовленный `memoryx.exe`, положите его в постоянную
папку, например `C:\Tools\MemoryX\memoryx.exe`, и проверьте:

```powershell
$exe = "C:\Tools\MemoryX\memoryx.exe"
& $exe --version
& $exe --help
```

Для MCP нужен файл, собранный с признаком `mcp`. Наличие команды можно проверить
через `& $exe serve --help`. Примеры ниже используют PowerShell; сами аргументы
подходят и для других оболочек.

## Сборка из исходников

В репозитории используется ночная версия Rust (`nightly`). Установите её и соберите
переносимый выпуск с поддержкой MCP:

```powershell
rustup toolchain install nightly
cargo +nightly build --release --features mcp
```

Готовый файл появится здесь:

```text
target/release/memoryx.exe
```

Без `--features mcp` собираются функции командной строки для работы с базой,
но сервер MCP не включается. Обычная сборка выпуска не привязана к процессору,
на котором она создана. Для местной сборки под конкретный процессор см.
[`docs/PORTABLE_CPU_BUILDS.md`](docs/PORTABLE_CPU_BUILDS.md); такой файл нельзя
выдавать за общий выпуск.

## Первые действия

Создайте проектную базу, добавьте данные, выполните запрос и посмотрите
статистику:

```powershell
$exe = ".\target\release\memoryx.exe"

& $exe --base-scope project init --base default
& $exe --base-scope project ingest --base default facts.json
& $exe --base-scope project query --base default "Какие решения сохранены?"
& $exe --base-scope project stats --base default
```

Для личной базы, общей для разных проектов, замените `project` на `user`:

```powershell
& $exe --base-scope user init --base default
& $exe --base-scope user ingest --base default facts.json
```

Полный список параметров показывает `--help`. Для обслуживания есть команды
`import`, `export`, `history`, `snapshot`, `compact`, `verify-integrity`,
`rebuild-index` и `repair`.

## Проектные и пользовательские базы

Область определяет физический корень базы:

| Область | Физический путь |
| --- | --- |
| `project` | `<repo>/.memoryx/bases/<name>` |
| `user` | `<home>/.memoryx/bases/<name>`; в Windows обычно `%USERPROFILE%\\.memoryx\\bases\\<name>` |

Например, проектная база `default` хранится так:

```text
<repo>/.memoryx/bases/default/
  cas/
  index/
  graph/
  meta/
    history.log
    sources.jsonl
    atom_sources.jsonl
    predicates.jsonl
    entities.jsonl
    relations.jsonl
  inverted/
```

Командная строка и MCP открывают одно и то же постоянное хранилище.
`update_atom` создаёт новую версию со связью `SUPERSEDES`, а `delete_atom`
создаёт метку удаления вместо физического стирания старого атома. Успешные
изменения записываются в `meta/history.log`.

В одной физической базе одновременно может быть только один процесс,
удерживающий право записи. Не направляйте два независимых процесса записи в
один каталог. Запущенный `memoryx serve --stdio` создаёт локальный управляющий
канал, доступный только через обратную петлю компьютера. Второй
`memoryx serve --stdio` для той же базы не становится писателем, а прозрачно
передаёт запросы уже работающему владельцу. Скрипт может обратиться к владельцу
командой
`memoryx --format json client --base <путь> --tool get_stats --arguments '{}'`;
вместо `get_stats` можно указать `verify_integrity`. Разные физические базы
по-прежнему можно использовать параллельно. Подробный договор описан в
[`docs/LIVE_OWNER_CONTROL.md`](docs/LIVE_OWNER_CONTROL.md).

## Подключение MCP к Codex или среде разработки

`memoryx serve --stdio` — штатный транспорт MCP. `memoryx serve` без `--stdio`
запускает HTTP-сервер объединения баз, а не MCP. Вставьте блок `mcpServers` в
настройки MCP Codex или вашей среды разработки. Точное расположение этого блока зависит от
клиента.

Для готового файла и проектной базы:

```json
{
  "mcpServers": {
    "memoryx-project": {
      "command": "C:\\Tools\\MemoryX\\memoryx.exe",
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

Для пользовательской базы замените `project` на `user`. Если используется
сборка из исходников, вместо готового файла укажите:

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

После подключения сначала попросите клиента вызвать `active_base` или
`list_bases`. MCP сохраняет читаемое человеком поле `content` и также отдаёт
`structuredContent`. Успешные изменения отмечаются как
`durability: "committed"`.

## Безопасная работа с 45 инструментами MCP

Рекомендуемый порядок:

1. Перед чтением или записью вызовите `active_base` или `list_bases`.
2. Для другой базы используйте `connect_base` и передавайте её `base_ref`, если
   инструмент поддерживает это поле; `switch_base` применяйте только при
   намеренной смене активной базы.
3. Для сложного вопроса сначала вызовите `compile_query_contract`, затем
   `validate_query_contract`, а потом `query` или `query_base`.
4. Перед важным фактическим ответом проверьте `get_provenance_path`,
   `explain_answer_graph` или `extract_subgraph`.
5. Инструменты записи используйте только после явной просьбы изменить базу.

Точные указатели работают по принципу отказа при сомнении: если объект не найден,
возвращается `NoMatch`, а запрос не расширяется на всю базу. Обычный запрос
использует текущие версии атомов. Для истории нужно явно установить
`temporal_scope.require_current` в `false`. Ветки, созданные только для чтения,
не сохраняются. Идентификатор для `register_predicate` нужно получать через
`resolve_predicate`, а не придумывать. `attach_atom_source` сохраняет разные
источники и повторное присоединение той же пары атом/источник безопасно.

| Категория | Инструменты | Назначение |
| --- | --- | --- |
| Запрос и проверка ответа | `query`, `query_base`, `compile_query_contract`, `validate_query_contract`, `explain_answer_graph`, `get_provenance_path` | Составить, проверить и разобрать ответ вместе с подтверждениями. |
| Выбор базы | `list_bases`, `active_base`, `connect_base`, `switch_base` | Найти, подключить и выбрать базу. |
| Поиск | `search_lex`, `search_graph`, `search_semantic` | Найти кандидатов, не считая один поиск окончательным ответом. |
| Запись атомов и история | `ingest`, `batch_ingest`, `update_atom`, `delete_atom`, `history` | Добавить, изменить, пометить удалённым и проверить изменения. |
| Изменение состояния и исправление утверждений | `supersede_claim`, `correct_claim`, `correct_relation`, `transition_relation` | Заменить устаревшие знания с сохранением истории; для единственного текущего значения связи использовать `transition_relation`. |
| Проверка и счётчики | `get_stats`, `verify_integrity` | Получить однозначные логические и физические счётчики и проверить живую базу через её владельца. |
| Источники | `register_source`, `list_sources`, `attach_atom_source` | Зарегистрировать источники и связать их с атомами. |
| Описание типов связей | `register_predicate`, `list_predicates`, `get_predicate`, `resolve_predicate` | Задать и получить устойчивые типы связей. |
| Сущности и связи | `create_entity`, `list_entities`, `alias_entity`, `merge_entities`, `split_entity`, `add_claim`, `assert_relation` | Вести сущности, псевдонимы, утверждения и связи. |
| Контексты и противоречия | `create_context`, `list_contexts`, `branch_context`, `list_conflicts` | Разделять предположения и просматривать нерешённые противоречия. |
| Обход графа | `graph_neighbors`, `graph_walk`, `extract_subgraph` | Просматривать связанные знания и локальные цепочки рассуждения. |

## Несколько баз

Один процесс MCP может подключить несколько совместимых баз. Типичная
последовательность:

```json
{"name":"list_bases","arguments":{}}
```

```json
{"name":"connect_base","arguments":{"base_ref":"project:client-a","scope":"project","name":"client-a"}}
```

```json
{"name":"query_base","arguments":{"base_ref":"project:client-a","query_text":"Какие решения связаны с хранением?","ctx_id":0}}
```

```json
{"name":"switch_base","arguments":{"base_ref":"project:client-a"}}
```

Передавайте `base_ref`, чтобы обращаться к подключённой базе без смены активной,
если выбранный инструмент поддерживает это поле.

## Резервная копия, снимок и восстановление

`snapshot` показывает логическую идентичность текущего состояния знаний. Это
полезно для воспроизводимости, но это не копия файлов.

Для резервной копии остановите все процессы, которые записывают в базу, затем
скопируйте весь каталог базы вместе с `cas`, `index`, `graph`, `meta` и
`inverted`. Не копируйте работающую базу во время записи.

Проверка и восстановление проектной базы:

```powershell
& $exe --base-scope project verify-integrity --base default
& $exe --base-scope project rebuild-index --base default
& $exe --base-scope project repair --base default
```

`verify-integrity` проверяет сохранённые данные. `rebuild-index` заново строит
производные индексы. `repair` выполняет безопасную последовательность
восстановления и завершается повторной проверкой целостности. Перед изменением
хранилища можно выполнить `compact --dry-run`. Команды обслуживания требуют
исключительного доступа к физической базе.

## Воспроизводимая проверка

В репозитории есть готовый к проведению и представлению сравнительный
бенчмарк. Он вычисляет показатели только по действительно записанным ответам
и не заменяет отсутствующие оценки выгодными значениями:

- [`docs/BENCHMARK_RAG_COMPARISON.md`](docs/BENCHMARK_RAG_COMPARISON.md)
- [`benchmarks/run_rag_comparison.ps1`](benchmarks/run_rag_comparison.ps1)
- [`benchmarks/rag_comparison/cases.jsonl`](benchmarks/rag_comparison/cases.jsonl)
- [`benchmarks/rag_comparison/corpus.jsonl`](benchmarks/rag_comparison/corpus.jsonl)

Проверка неизменяемых входных данных и сценария:

```powershell
pwsh -NoLogo -File benchmarks/run_rag_comparison.ps1 -Mode validate
pwsh -NoLogo -File benchmarks/run_rag_comparison.ps1 -Mode selftest
```

Сценарий подготавливает парный запуск, записывает ответы MemoryX и обычного
RAG, вычисляет точность, полноту поиска, подтверждённость, полную задержку,
долю закрытых случаев и результаты функциональных проверок, а затем создаёт
отчёты JSON и Markdown. В репозитории нет заранее подготовленных результатов о
превосходстве. Для представления чисел нужны настоящие адаптеры, исходные
ответы, решения проверяющих и описание среды запуска.

## Лицензия

MemoryX распространяется по `AGPL-3.0-or-later`. Полный текст открытой лицензии:
[`LICENSE.md`](LICENSE.md) и [`COPYING`](COPYING). Отдельная коммерческая
лицензия может быть предоставлена по письменному соглашению; см.
[`COMMERCIAL_LICENSE.md`](COMMERCIAL_LICENSE.md).

Внешние вклады регулируются [`CLA.md`](CLA.md) и
[`CONTRIBUTING.md`](CONTRIBUTING.md).
