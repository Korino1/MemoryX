# MemoryX: план оркестрации реализации

Источник: `CONCEPT_EXTENSION_IMPLEMENTATION_PLAN.md`

Цель: довести MemoryX до расширенного концепта с минимальным расходом подписки, не потеряв текущие отличительные возможности проекта.

## 1. Доступные модели и правило выбора

### gpt-5.4-mini

Использовать по умолчанию.

Подходит для:

- локальных механических правок;
- добавления простых типов и enum;
- unit tests;
- README/docs;
- MCP descriptions/examples;
- CLI flags;
- небольших adapters;
- исправления clippy/fmt/test failures;
- подготовки diff summaries.

Рассуждения:

- `low`: простая правка, документация, тест на уже понятную функцию.
- `medium`: несколько файлов, но без архитектурного решения.

Не давать mini:

- изменение CAS binary format;
- изменение solver semantics;
- federation protocol design;
- conflict/context policy design;
- финальные архитектурные выводы “готово/не готово”.

### gpt-5.4

Использовать для основной инженерной интеграции.

Подходит для:

- `QueryContract -> GoalSpec -> solver` integration;
- constraint evaluator;
- AnswerPack extension;
- proof AnswerGraph typing;
- source/evidence/claim model integration;
- MCP tools, которые пишут в базу;
- migration-safe API design;
- сложные regression tests.

Рассуждения:

- `medium`: реализация по уже утверждённому контракту.
- `high`: изменение нескольких подсистем или риск сломать совместимость.

### gpt-5.5

Использовать редко, как архитектора/аудитора.

Подходит для:

- разбиение milestone на безопасные work packets;
- проверка, что новая реализация не ломает концепт;
- выбор между несовместимыми архитектурными вариантами;
- review PR/commit перед публикацией;
- финальная проверка коммерческой готовности.

Рассуждения:

- `high`: архитектурный gate после milestone.
- `max`: только для спорных решений вокруг solver, CAS, federation, CRDT/repair, публичных API.

Запрет:

- не использовать `gpt-5.5` для рутинного кодинга, форматирования, документации и простых тестов.

## 2. Non-regression контракт

Любой агент обязан сохранить эти свойства. Если задача конфликтует с этим списком, задача считается неверно поставленной.

- Атомы знания вместо чанков текста.
- Самосогласованность: contexts, branches, conflicts, `CTX_PROBE`, explicit conflict policy.
- Двунаправленный вывод: backward gaps плюс forward candidate/evidence wave.
- Fixed-point сборка ответа через `FixedPointSolver`.
- Минимальный доказательный `AnswerGraph`.
- Федерация баз на claims/provenance/metadata, а не обмен готовыми текстами.
- Устойчивость к повреждениям: CAS integrity, Merkle/integrity checks, CRDT, WAL/snapshot, repair/rebuild.
- MCP как полноценный управляющий слой базы.
- Local-first storage: project scope или user scope.
- Portable CPU build по умолчанию, native/Zen4 только явный локальный режим.

## 3. Роли агентов

### Оркестратор

Модель: `gpt-5.5`, reasoning `high`; для спорных решений `max`.

Использование:

- 1 раз перед milestone;
- 1 раз после milestone;
- 1 раз перед публикацией/release.

Задачи:

- нарезать milestone на work packets;
- проверять non-regression;
- принимать архитектурные решения;
- запрещать неоправданные breaking changes;
- держать общий tracker.

### Implementer Mini

Модель: `gpt-5.4-mini`, reasoning `medium`.

Использование:

- основной исполнитель;
- один агент держит контекст до 3 последовательных задач, если контекст не переполнен.

Задачи:

- добавление типов;
- простые adapters;
- CLI flags;
- serde derives;
- tests;
- docs;
- MCP examples.

### Integration Engineer

Модель: `gpt-5.4`, reasoning `medium/high`.

Использование:

- когда изменение проходит через 2+ подсистемы;
- когда mini упёрся в неоднозначность.

Задачи:

- связка contract/solver/router/store;
- расширение AnswerPack;
- constraint evaluator;
- source/evidence integration;
- authoring API.

### Safety and Integrity Reviewer

Модель: `gpt-5.5`, reasoning `high`.

Использование:

- после milestone 2, 3, 4, 6;
- перед release.

Задачи:

- проверить, что CAS/integrity/CRDT/federation не сломаны;
- найти скрытые обходы solver;
- проверить, что semantic retrieval не стал source of truth;
- проверить claims/evidence/source trace.

### Test and Gate Agent

Модель: `gpt-5.4-mini`, reasoning `low/medium`.

Задачи:

- запуск `fmt`, `clippy`, `test`, `build`;
- добавление regression tests;
- фиксация failures;
- подготовка короткого evidence report.

## 4. Milestone orchestration

### Milestone 0: стабилизация ветки

Цель: перед началом новых фич зафиксировать чистую точку.

Модель:

- `gpt-5.4-mini`, reasoning `low` для git/status/docs;
- `gpt-5.4`, reasoning `medium` если есть конфликты в рабочем дереве.

Задачи:

- проверить текущий `git status`;
- отделить пользовательские незакоммиченные изменения от новых работ;
- создать tracker `IMPLEMENTATION_TRACKING.md` или обновить существующий;
- подтвердить baseline:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features --quiet
```

Критерий выхода:

- есть commit/tag baseline;
- ясно, какие файлы dirty и почему.

### Milestone 1: QueryContract core

Фазы плана: A1, A2, A3, J1 minimal.

Модель:

- `gpt-5.5 high`: один раз утвердить публичный shape `QueryContract`;
- `gpt-5.4 medium/high`: реализовать contract types и adapter;
- `gpt-5.4-mini medium`: serde tests, CLI flags, docs.

Пакеты:

1. `QC-01`: `src/query/contract.rs` с типами.
2. `QC-02`: `QueryContractCompiler` deterministic baseline.
3. `QC-03`: adapter `QueryContract -> GoalSpec + QueryConstraints`.
4. `QC-04`: `MemoryX::answer_contract`.
5. `QC-05`: CLI `query --emit-contract` и `query --contract`.

Гейт:

- старый `MemoryX::answer(query_text, ctx_policy)` работает;
- contract JSON roundtrip работает;
- hard/must_not constraints представлены явно, даже если evaluator ещё неполный.

### Milestone 2: Proof-grade AnswerPack

Фазы плана: D1-D5, F1 basic.

Модель:

- `gpt-5.4 high`: AnswerPack integration;
- `gpt-5.4-mini medium`: tests/status cases/docs;
- `gpt-5.5 high`: review после completion.

Пакеты:

1. `AP-01`: `AnswerStatus`.
2. `AP-02`: `CoverageReport`.
3. `AP-03`: `ConfidenceVector`.
4. `AP-04`: `RejectedCandidateSummary`.
5. `AP-05`: `QueryTrace` с budget limits.
6. `AP-06`: `StructuredAnswer` и запрет factual statement без binding.
7. `AP-07`: typed proof nodes/edges в `AnswerGraph`.

Гейт:

- `AnswerPack` сообщает `COMPLETE/PARTIAL/CONFLICTED/INSUFFICIENT_EVIDENCE`;
- uncovered gaps видны;
- evidence/provenance не теряются.

### Milestone 3: Source/Evidence/Claim integrity

Фазы плана: B1, B2, B3, H1.

Модель:

- `gpt-5.5 max`: перед началом решить, менять ли CAS format или хранить расширения в meta/provenance;
- `gpt-5.4 high`: реализация source/evidence/claim models;
- `gpt-5.4-mini medium`: tests and docs.

Рекомендуемое решение по умолчанию:

- не менять CAS binary format на первом проходе;
- новые поля хранить в meta/provenance/entity layer;
- CAS migration отложить до отдельного gated milestone.

Пакеты:

1. `SEC-01`: `SourceRecord`, `SourceId`, `SourceKind`.
2. `SEC-02`: `EvidenceRecord` поверх legacy `EvidenceRef`.
3. `SEC-03`: `ClaimStatus`, `ClaimViewV2`, epistemic statuses.
4. `SEC-04`: provenance path extraction.
5. `SEC-05`: `docs/LLM_BOUNDARY.md` и `Proposal<T>`.

Гейт:

- claim в ответе трассируется до evidence/source/location;
- LLM output не может стать verified fact без validation.

### Milestone 4: Constraint-first solving

Фазы плана: C1-C3, E1, E3 basic.

Модель:

- `gpt-5.5 high`: утвердить semantics hard/soft/negative constraints;
- `gpt-5.4 high`: implementation;
- `gpt-5.4-mini medium`: regression tests.

Пакеты:

1. `CE-01`: `ConstraintEvaluator`.
2. `CE-02`: hard constraints before ranking.
3. `CE-03`: `MUST_NOT` rejection summaries.
4. `CE-04`: temporal/context selectors.
5. `CE-05`: conflict policy integration with `NeedBranch`.
6. `CE-06`: retriever trait and `CandidateV2`.
7. `CE-07`: deterministic retrieval planner.

Гейт:

- ANN/semantic candidate не может попасть в final graph при нарушении hard/must_not;
- conflict не сглаживается;
- context применяется до ranking.

### Milestone 5: Knowledge authoring and MCP

Фазы плана: B4, G1-G3, J2.

Модель:

- `gpt-5.4 high`: authoring API and store integration;
- `gpt-5.4-mini medium`: MCP tool definitions, descriptions, examples, tests;
- `gpt-5.5 high`: review MCP completeness.

Пакеты:

1. `AUTH-01`: entity/relation records.
2. `AUTH-02`: create entity/add claim/create relation APIs.
3. `AUTH-03`: merge/split/alias/rename/supersede/deprecate/correct claim.
4. `AUTH-04`: automatic ingestion dry-run with extracted unverified claims.
5. `AUTH-05`: MCP tools for authoring.
6. `AUTH-06`: MCP examples for each tool.

Гейт:

- агент через MCP может вести базу, а не только читать;
- auto-extracted facts не получают verified status автоматически.

### Milestone 6: Federation, repair, reproducibility

Фазы плана: I1-I3, K1.

Модель:

- `gpt-5.5 max`: architecture gate для snapshot/federation/repair;
- `gpt-5.4 high`: implementation;
- `gpt-5.4-mini medium`: corruption/recovery tests.

Пакеты:

1. `REP-01`: `KnowledgeSnapshotId`.
2. `REP-02`: AnswerPack snapshot binding.
3. `REP-03`: rebuild indexes from CAS.
4. `REP-04`: repair command/API.
5. `REP-05`: federated contract query planning.
6. `REP-06`: remote trust/budget policy.

Гейт:

- indexes rebuildable from CAS;
- federation передаёт claims/provenance, не готовый ответ;
- same snapshot + same contract даёт воспроизводимый logical result.

### Milestone 7: Commercial readiness

Фазы плана: J3, K2, K3.

Модель:

- `gpt-5.4-mini medium`: docs, examples, benchmark harness;
- `gpt-5.4 high`: benchmark design implementation;
- `gpt-5.5 high`: final product/release audit.

Пакеты:

1. `REL-01`: README rewrite under executable knowledge fabric positioning.
2. `REL-02`: docs for QueryContract, AnswerPack, Authoring API, MCP.
3. `REL-03`: RAG comparison benchmark with honest metrics.
4. `REL-04`: portable release build and native performance build docs.
5. `REL-05`: final clean git status, tags, GitHub release draft.

Гейт:

- no false marketing claims;
- all distinguishing features documented and tested;
- release binary portable by default.

## 5. Экономия подписки

Правила:

1. По умолчанию стартует `gpt-5.4-mini`.
2. `gpt-5.4` подключается только если задача затрагивает solver/store/CAS/federation/MCP write path.
3. `gpt-5.5` используется только на архитектурных gates и финальных audits.
4. Один mini-agent должен выполнять до 3 связанных задач подряд, если контекст не переполнен.
5. Не передавать агентам весь проект повторно. Давать им:
   - краткий task packet;
   - список файлов;
   - non-regression контракт;
   - acceptance tests.
6. После каждого work packet агент возвращает:
   - changed files;
   - tests run;
   - risks;
   - next exact task.
7. Не запускать `gpt-5.5` для исправления clippy, formatting, README wording.

## 6. Task packet шаблон

```text
Role:
Model:
Reasoning:
Budget:

Goal:
Files allowed:
Files read-only:

Non-regression:
- atoms not chunks
- solver not bypassed
- conflicts not hidden
- CAS/CRDT/federation not broken
- MCP remains complete

Implementation steps:
1.
2.
3.

Acceptance:
- cargo fmt --check
- cargo clippy --all-targets --all-features -- -D warnings
- cargo test <targeted>
- no public API break unless explicitly approved

Return:
- summary
- changed files
- tests
- risks
- recommended next packet
```

## 7. Минимальный путь до продаваемого продукта

Если подписка ограничена, не делать всё сразу. Минимальный коммерчески сильный путь:

1. Milestone 1: QueryContract core.
2. Milestone 2: Proof-grade AnswerPack.
3. Milestone 4: Constraint-first solving.
4. Milestone 5: MCP authoring tools.
5. Milestone 6 partial: snapshot id + rebuild indexes.
6. Milestone 7: docs, examples, benchmark, release audit.

Отложить до второй версии:

- full adaptive planner;
- advanced sharding;
- complex UI/forms;
- CAS binary migration;
- distributed auto-repair beyond local repair/rebuild;
- large benchmark suite.

## 8. Stop rules

Останавливать реализацию и возвращать задачу Оркестратору, если:

- предлагается заменить atoms на chunks;
- semantic search начинает решать truth/constraint validity;
- conflict исчезает из AnswerPack;
- `NeedBranch` обходится;
- `FixedPointSolver` обходится в query path;
- new source/evidence fields требуют CAS format change без migration plan;
- MCP tool пишет в базу без validation/dry-run/error reporting;
- tests проходят только за счёт удаления проверок.

## 9. Финальный gate перед публикацией

Модель: `gpt-5.5`, reasoning `high`.

Проверить:

- концепт и README говорят одно и то же;
- нет claims “лучше RAG” без benchmark/evidence;
- portable build default;
- native/Zen4 build clearly labelled;
- MCP tools have descriptions/examples;
- database path policy clear;
- all core differentiators preserved;
- clean test suite;
- no secrets, archives, Concept folder in git release.

Команды:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features --quiet
cargo build --release --features mcp
git status --short
```

