# MemoryX SKF/README Conformance Audit

Дата: 2026-07-02

Область проверки:

- `Concept/SKF.txt`
- `Concept/SKF-1.1 Implementer-Ready Spec.txt`
- `README.md`
- текущий код проекта в `E:\Rust\AI\MemoryX\MemoryX_2`

Цель: проверить не планы и не намерения, а фактическую реализацию текущего кода относительно базового SKF-концепта и публичных заявлений README.

## Краткий Итог

Проект не является пустой заготовкой: ядро CAS/atoms, контексты, конфликты, VM/CTX_PROBE, fixed-point solver, AnswerGraph, индексы, CRDT/WAL/snapshot и MCP/CLI surface реально присутствуют в коде.

Но текущий публичный статус в README местами завышен. Самые опасные несоответствия:

- MCP stdio transport может загрязнять stdout human-readable сообщениями.
- HTTP federation использует hardcoded `BaseId`, то есть несколько баз могут иметь один и тот же identity.
- Semantic/ANN слой не является полноценно включённой частью live solver path.
- AnswerPack пока не даёт proof-grade provenance/status на каждый claim.
- Federation discovery уже существует, но пока существенно уже концепта.
- Example `mcp_server_full` заявлен как равный production surface, но часть handlers выглядит placeholder.
- Repair/rebuild есть частично внутри, но не оформлен как публичная операция.

## P0: Нужно Исправить До Публичных Заявлений Production

### P0-1. MCP stdio transport может быть невалидным JSON-RPC потоком

Статус: `RISK/OVERCLAIM`

README заявляет `memoryx serve --stdio` как production MCP surface. Но текущий binary печатает human-readable сообщения в stdout перед/во время JSON-RPC loop.

Evidence:

- `src/bin/memoryx.rs:2320` - запуск stdio serve path.
- `src/bin/memoryx.rs:1404` - `print_info()` пишет в stdout.
- `README.md:194` - README говорит, что `memoryx serve --stdio` уже даёт полный production MCP surface.

Почему это проблема:

MCP stdio transport должен отдавать в stdout только protocol frames. Любой баннер, статус или human-readable лог в stdout может сломать клиент.

Как должно быть:

- В режиме `--stdio` весь non-protocol output должен идти в stderr или быть полностью подавлен.
- JSON-RPC/MCP frames должны быть единственным stdout output.
- Добавить regression test: запуск `memoryx serve --stdio` не пишет preamble/banner в stdout до первого JSON-RPC ответа.

### P0-2. Federation server использует hardcoded BaseId

Статус: `RISK`

Концепт требует уникальную identity базы для federation. Кодовые типы это подразумевают, но serve path выставляет один и тот же base id.

Evidence:

- `src/federation/mod.rs:39` - `BaseId` описан как идентификатор базы.
- `src/bin/memoryx.rs:2372` - `cmd_serve()` hardcodes id вида `[1, 0, ...]`.

Почему это проблема:

Если несколько баз стартуют с одинаковым `BaseId`, ломаются:

- visited-base logic;
- trust routing;
- replication/federation identity;
- корректное различение peer bases.

Как должно быть:

- При `init` создавать и сохранять стабильный per-base identity в durable metadata.
- `serve` должен читать этот id из базы.
- Для существующих баз нужна migration/fallback: если id отсутствует, создать один раз и сохранить.
- Тест: две разные базы получают разные `BaseId`; повторное открытие той же базы сохраняет тот же `BaseId`.

## P1: Частичная Реализация Или Завышенное Описание

### P1-1. ANN/semantic retrieval не является полноценной частью live solver path

Статус: `PARTIAL`

README и SKF говорят о lexical, semantic и graph retrieval. Код содержит semantic APIs и HNSW-структуры, но live fixed-point query path использует ANN ограниченно.

Evidence:

- `src/store/api.rs:4577` - `create_router()` регистрирует node/location mappings в `router.ann`.
- `src/query/router.rs:925` - `QueryRouter::route()` вызывает ANN backend как один из backend.
- `src/query/router.rs:600` - `AnnBackend::route()` возвращает candidates только когда goal уже содержит `EntityRef::Node`.
- `src/store/api.rs:5190` - standalone `MemoryX::search_semantic()`.
- `src/query/ann/index.rs:99` - brute-force `EmbeddingIndex::search()`.
- `src/query/ann/hnsw.rs:63` - `HnswGraph` существует, но не является live query route.

Почему это проблема:

SKF допускает semantic retrieval только как источник кандидатов, но README создаёт ожидание, что semantic retrieval является частью основного query solving. Сейчас это скорее отдельный API и частичный backend, чем полноценный semantic candidate stage.

Как должно быть:

- Либо честно сузить README: semantic search доступен отдельной операцией, но solver path пока в основном lexical/graph/CAS.
- Либо подключить embeddings/HNSW к gap resolution:
  - query/goal embedding;
  - ANN top-k candidates;
  - обязательный invariant/constraint filter после ANN;
  - тест, что query через `MemoryX::answer()` реально получает semantic candidate без заранее известного `EntityRef::Node`.

### P1-2. AnswerPack не даёт proof-grade provenance/status на каждый claim

Статус: `PARTIAL`

SKF/README обещают auditable answer/provenance. В текущем публичном `AnswerPack` evidence хранится глобальным списком, а claim не несёт своего provenance/status.

Evidence:

- `src/store/api.rs:2552` - `AnswerPack` содержит `claims: Vec<ClaimView>` и глобальный `evidence: Vec<EvidenceRef>`.
- `src/store/api.rs:1262` - `ClaimView` не содержит явный provenance/status object.
- `src/query/solver.rs:2868` - `extract_claims()` строит `evidence_chains`, но они не возвращаются в `AnswerPack`.

Почему это проблема:

Для SKF утверждение в ответе должно быть проверяемо: claim -> evidence -> source/provenance. Глобального списка evidence недостаточно, если нельзя однозначно понять, какое evidence поддерживает какой claim и с каким epistemic status.

Как должно быть:

- Добавить `ClaimViewV2` или расширить `ClaimView`:
  - `status`;
  - `confidence`;
  - `evidence_refs`;
  - `provenance_path`;
  - `derived_from/proof_step`.
- Или добавить в `AnswerPack` отдельные `ProvenanceChain`/`ClaimEvidenceLink`.
- Тест: каждый factual claim в AnswerPack имеет связанный provenance path или получает статус `unknown/insufficient_evidence`.

### P1-3. Federation discovery уже концепта

Статус: `PARTIAL`

Код содержит federation types и рабочие `fetch`/`sync_crdt`, но discovery path пока в основном lexical.

Evidence:

- `src/federation/mod.rs:1999` - live handler делает `search_lex(&req.term)`.
- `src/federation/mod.rs:2011` - `DiscoveryResult::new(...)` без полноценного `atom_id`/mapping payload.
- `src/federation/mod.rs:252` - data types позволяют более богатый result.
- `src/federation/mod.rs:2118` - `fetch` реализован материально.
- `src/federation/mod.rs:2234` - `sync_crdt` реализован материально.

Почему это проблема:

SKF federation должна передавать claims/provenance/metadata/mappings, а не только находить термин. Discovery должен помогать находить совместимые atoms/mappings/constraints между базами.

Как должно быть:

- Заполнять `atom_id`, mappings и schema/constraint metadata в discovery.
- Discovery должен поддерживать term/id/schema/mapping modes.
- README формулировать аккуратно: federation core есть, discovery пока lexical/partial.

### P1-4. `examples/mcp_server_full.rs` не равен production implementation depth

Статус: `OVERCLAIM`

README говорит, что full MCP example имеет тот же 15-tool surface, что production. По именам tools это похоже, но часть handlers в example не store-backed.

Evidence:

- `README.md:133` - example описан как тот же 15-tool surface.
- `examples/mcp_server_full.rs:595` - `search_graph` выглядит placeholder/ограниченно.
- `examples/mcp_server_full.rs:927` - `graph_neighbors` placeholder/ограниченно.
- `examples/mcp_server_full.rs:1005` - `extract_subgraph` placeholder/ограниченно.
- Production `src/bin/memoryx.rs` содержит реальные handlers.

Почему это проблема:

Пользователь README может решить, что example является полноценной reference implementation, а не демонстрационной обвязкой.

Как должно быть:

- Либо сделать example реально store-backed на тех же APIs.
- Либо изменить README:
  - "same tool names/interface";
  - "example implementation, not full production parity".

### P1-5. Новый QueryContract слой пока не является основным query path

Статус: `PARTIAL`

В проект уже добавлены `QueryContract`, `QueryContractCompiler` и `ConstraintEvaluator`, но основной `MemoryX::answer()` пока вызывает старый `GoalSpecCompiler`.

Evidence:

- `src/store/api.rs:4516` - `MemoryX::answer()` вызывает `GoalSpecCompiler::compile(query_text)`.
- `src/query/contract.rs` - новый `QueryContract`.
- `src/query/compiler.rs` - новый deterministic `QueryContractCompiler`.
- `src/query/constraints.rs` - новый deterministic `ConstraintEvaluator`.

Почему это проблема:

Для базового SKF это не P0, потому что SKF говорит о `GoalSpec`. Но для заявленного направления расширения и будущего MCP contract query это означает, что новый строгий contract layer пока не влияет на production `answer()`.

Как должно быть:

- Добавить `MemoryX::answer_contract(contract, ctx_policy)`.
- `MemoryX::answer(query_text, ctx_policy)` должен компилировать natural query в `QueryContract`, затем в solver input.
- Constraint results должны попадать в AnswerPack/trace.

## P2: Есть Основа, Но Нет Полного Публичного Operational Surface

### P2-1. Repair/rebuild не оформлены как first-class public operation

Статус: `PARTIAL`

Durability primitives существуют, но публичный CLI/MCP repair/rebuild surface отсутствует.

Evidence:

- `src/crdt/wal.rs:1079` - WAL code реален.
- `src/crdt/snapshot.rs:101` - snapshot code реален.
- `src/graph/store.rs:1725` - reverse-index rebuild exists internally.
- `src/bin/memoryx.rs:108` - CLI exposes `Compact`, но нет `repair`/`rebuild`.

Почему это проблема:

SKF заявляет устойчивость к повреждениям, Merkle integrity, CRDT, replication, repair. Если repair не является явной пользовательской операцией, claim о repair нужно считать частичным.

Как должно быть:

- CLI:
  - `memoryx check`;
  - `memoryx repair`;
  - `memoryx rebuild-index`;
  - `memoryx verify-integrity`.
- MCP:
  - как минимум read-only `check_integrity`;
  - repair operations можно оставить CLI-only, если они опасны, но это должно быть описано в README.

## Verified / Implemented Areas

### Atoms/CAS binary layout

Статус: `VERIFIED/IMPLEMENTED`

Evidence:

- `src/cas/canonical.rs:371`
- `src/cas/mod.rs:328`
- `src/cas/mod.rs:503`
- `src/index/mod.rs:304`
- `src/index/mod.rs:1171`
- `src/index/mod.rs:1566`

Вывод:

Content-addressed atoms, sectioned atom body, id/location/index foundations реализованы материально.

### Backward + forward fixed-point solver skeleton

Статус: `VERIFIED/IMPLEMENTED`

Evidence:

- `src/query/solver.rs:1328` - старый `GoalSpecCompiler`.
- `src/query/solver.rs:1655` - solver setup/path.
- `src/query/solver.rs:1774` - backward/forward solving area.
- `src/query/solver.rs:1935` - fixed-point / graph assembly area.

Вывод:

SKF heptapod skeleton существует: compile goal -> backward gaps -> route candidates -> invariant gate -> set cover/minimal AnswerGraph. Но semantic/ANN и proof-grade claim provenance пока не полные.

### Contexts, conflicts, CTX_PROBE

Статус: `VERIFIED/IMPLEMENTED`

Evidence:

- `src/store/api.rs:1981` - context creation.
- `src/store/api.rs:2202` - conflict handling path.
- `src/store/api.rs:2250` - branch/conflict operations.
- `src/vm/interpreter.rs:1023` - `CTX_PROBE` VM support.

Вывод:

Контексты и конфликты не являются только документацией: они есть в store/VM path. Требуется отдельная проверка глубины TMS semantics на сложных сценариях, но базовая реализация присутствует.

### Local-first storage and CPU portability

Статус: `VERIFIED/IMPLEMENTED`

Evidence:

- `src/bin/memoryx.rs:1423`
- `src/bin/memoryx.rs:1447`
- `src/bin/memoryx.rs:1464`
- `.cargo/config.toml`
- `src/utils/cpu.rs`

Вывод:

Project/user scoped base path logic присутствует. Portable CPU defaults присутствуют; Zen4-only привязка не является текущим default.

## Проверки

Аудитор запускал:

```powershell
cargo +nightly test --all-targets --all-features --quiet
cargo +nightly clippy --all-targets --all-features -- -D warnings
```

Результат аудитора:

```text
609 + 14 + 5 + 6 + 3 tests passed
0 failed
clippy -D warnings completed without diagnostics
```

Примечание: audit findings не означают, что проект не компилируется. Они означают, что часть публичных/concept claims пока реализована частично или требует корректировки README.

## Приоритет Исправлений

1. Исправить stdio MCP stdout pollution.
2. Сделать persistent unique `BaseId` для federation.
3. Добавить per-claim provenance/status в AnswerPack.
4. Подключить `QueryContractCompiler`/`ConstraintEvaluator` к `MemoryX::answer_contract` и затем к MCP query.
5. Определиться с ANN claim: либо подключить HNSW/semantic retrieval в solver path, либо сузить README.
6. Расширить federation discovery до atom/mapping-aware discovery.
7. Исправить README по `examples/mcp_server_full.rs` или сделать example реально production-parity.
8. Добавить public check/repair/rebuild surface.

