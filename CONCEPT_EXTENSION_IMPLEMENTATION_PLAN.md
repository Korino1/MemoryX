# MemoryX: план реализации расширения концепта

Источник: `Concept/Расширение.txt`

Дата составления: 2026-07-02

## 0. Цель расширения

Расширение меняет публичный центр MemoryX: проект должен быть не "RAG, но лучше", а исполняемой системой знаний.

Целевая цепочка:

```text
Natural query
  -> QueryContract
  -> required knowledge graph and gaps
  -> federated retrieval candidates
  -> constraint/context/evidence validation
  -> conflict branches
  -> minimal proof AnswerGraph
  -> verifiable AnswerPack
```

Главное правило:

```text
retrieval suggests
MemoryX validates
solver decides
renderer explains
```

## 1. Текущее состояние проекта

Уже есть базовые элементы, на которые надо опираться:

- `src/query/solver.rs`: `GoalSpec`, `GoalSpecCompiler`, `BackwardWave`, `FixedPointSolver`.
- `src/query/gap_generator.rs`: шаблонная генерация gaps по intent.
- `src/query/router.rs`: маршрутизация кандидатов по source strategy.
- `src/query/cost.rs`: cost calculation для `AnswerGraph`.
- `src/store/api.rs`: `AnswerPack`, `AnswerGraph`, `Gap`, `EvidenceRef`, `EntityRef`, context/conflict/store API.
- `src/vm/*`: invariant VM, `CTX_PROBE`, hard/soft validation.
- `src/cas/*`: canonical atoms, claims, evidence, refs, integrity.
- `src/graph/*`, `src/index/*`, `src/query/ann/*`: graph, lexical, semantic retrieval foundations.
- `src/bin/memoryx.rs`: CLI/MCP surface.

Недостающий архитектурный слой:

- `QueryContract` пока не является публичной центральной моделью.
- `GoalSpec` сейчас ближе к внутренней промежуточной структуре, чем к полному контракту запроса.
- `AnswerPack` есть, но не полностью соответствует расширенной схеме из концепта.
- Epistemic statuses, confidence vector, coverage report, rejection summaries и structured provenance нужно сделать явными.
- LLM boundary пока должна быть оформлена как контракт, даже если сама LLM-интеграция останется внешней.

## 2. Принципы реализации

1. Не ломать существующий `GoalSpec` сразу. Сначала добавить `QueryContract` поверх него и адаптер `QueryContract -> GoalSpec + constraints + policies`.
2. Все новые публичные типы должны быть сериализуемы через `serde`, потому что они нужны CLI, MCP, future plugin и внешним агентам.
3. `AnswerPack` должен остаться совместимым на уровне базовых полей, но получить расширенную версию или новые поля с безопасными default values.
4. Embedding/ANN может возвращать кандидатов, но не должен решать выполнение hard constraints.
5. Любой factual output должен быть связан с `ClaimRef/EvidenceRef/SourceRef`, иначе статус должен быть `UNKNOWN`, `HYPOTHESIS` или `INSUFFICIENT_EVIDENCE`.
6. Сначала реализовать deterministic core, потом authoring UX, потом benchmarks.

## 2.1. Non-regression: что нельзя убирать или упрощать

Эти возможности уже являются отличием MemoryX от обычных RAG/vector DB систем. Любая новая реализация должна сохранять их как базовый контракт продукта:

- **Атомы знания вместо чанков текста.** Текстовые фрагменты могут быть source/evidence, но source of truth должен оставаться в атомах, claims, evidence, contexts и graph links.
- **Самосогласованность.** Ветвление контекстов, `CTX_PROBE`, conflicts, conflict sets и explicit resolution policies нельзя заменять выбором “лучшего” чанка или молчаливым сглаживанием противоречий.
- **Двунаправленный вывод.** Heptapod-принцип `backward wave -> gaps` и `forward wave -> candidates/evidence` должен сохраняться; QueryContract расширяет этот механизм, но не заменяет его простым retrieval.
- **Fixed-point сборка ответа.** `FixedPointSolver` и минимальный доказательный `AnswerGraph` остаются ядром query path. Новые planners/retrievers должны подавать candidates в solver, а не обходить solver.
- **Федерация баз на идентичной архитектуре.** Federation должна передавать claims/provenance/metadata/snapshots, а не готовый текстовый ответ. Новые contract/query APIs должны быть совместимы с federated execution.
- **Устойчивость к повреждениям.** Merkle/integrity checks, CAS durability, CRDT/WAL/snapshot, replication and repair paths нельзя выносить в “потом” при изменении форматов. Любые новые индексы должны быть rebuildable from CAS или иметь repair procedure.
- **MCP как полноценный управляющий слой.** MCP должен сохранять возможность работать с базой, контекстами, конфликтами, графом и authoring operations, а не только выполнять текстовый query.
- **Local-first хранение.** База должна оставаться в выбранном scope: project-local или user-global. Новые функции не должны вводить скрытые внешние хранилища.

Правило для всех фаз:

```text
если новая возможность конфликтует с этим списком, менять надо новую возможность, а не ядро MemoryX.
```
## 3. Фаза A: QueryContract и алгебра условий

### A1. Добавить модуль query contract

Файлы:

- добавить `src/query/contract.rs`
- обновить `src/query/mod.rs`
- обновить `src/prelude.rs`

Типы:

```rust
pub struct QueryContract {
    pub intent: Intent,
    pub targets: Vec<EntityPattern>,
    pub must: Vec<Constraint>,
    pub should: Vec<WeightedConstraint>,
    pub must_not: Vec<Constraint>,
    pub quantifiers: Vec<QuantifiedCondition>,
    pub relations: Vec<RelationRequirement>,
    pub temporal_scope: TemporalScope,
    pub spatial_scope: Option<SpatialScope>,
    pub jurisdiction: Option<JurisdictionScope>,
    pub contexts: Vec<ContextSelector>,
    pub source_policy: SourcePolicy,
    pub evidence_policy: EvidencePolicy,
    pub freshness_policy: FreshnessPolicy,
    pub ambiguity_policy: AmbiguityPolicy,
    pub conflict_policy: ConflictPolicy,
    pub completeness_policy: CompletenessPolicy,
    pub output_contract: OutputContract,
    pub budgets: QueryBudgets,
}
```

Подзадачи:

- A1.1. Описать `ConstraintKind`: `Must`, `MustNot`, `Should`, `AtLeast`, `Exactly`, `Any`, `All`, `Unless`, `OnlyIf`, `Before`, `After`, `During`, `Within`, `SupportedBy`.
- A1.2. Описать `ConstraintTarget`: claim predicate, entity type, relation, source, evidence, time, context, numeric value, text term.
- A1.3. Описать `ConstraintResult`: satisfied, violated, unknown, not_applicable, blocked_by_policy.
- A1.4. Описать `ConstraintId`, чтобы constraint можно было трассировать до candidates, gaps и final answer.
- A1.5. Добавить unit tests на сериализацию/десериализацию `QueryContract`.

Критерий готовности:

- `QueryContract` можно создать из JSON, сериализовать обратно без потерь и преобразовать в минимальный `GoalSpec`.

### A2. Компилятор natural query -> QueryContract

Файлы:

- расширить `src/query/solver.rs` или вынести в `src/query/compiler.rs`

Подзадачи:

- A2.1. Переименовать текущий `GoalSpecCompiler` по смыслу или добавить новый `QueryContractCompiler`.
- A2.2. Сохранить deterministic baseline: regex/token rules, без обязательной LLM.
- A2.3. Добавить метод `compile_contract(query: &str) -> QueryContract`.
- A2.4. Старый `compile(query) -> GoalSpec` оставить как compatibility wrapper.
- A2.5. Добавить tests на пример из концепта: Rust, local, conflicts, MCP, not PostgreSQL, Windows/provenance priorities.

Критерий готовности:

- Запрос из `Concept/Расширение.txt` превращается в `QueryContract` с `MUST`, `MUST_NOT`, `SHOULD` и `OutputContract`.

### A3. Adapter QueryContract -> solver input

Файлы:

- `src/query/contract.rs`
- `src/query/solver.rs`
- `src/store/api.rs`

Подзадачи:

- A3.1. Реализовать `impl From<&QueryContract> for GoalSpec`.
- A3.2. Реализовать `QueryConstraintsView`/`QueryConstraints` из hard/soft constraints.
- A3.3. Передавать `source_policy`, `evidence_policy`, `freshness_policy` в router/solver.
- A3.4. Добавить `MemoryX::answer_contract(contract: QueryContract) -> Result<AnswerPack, StoreError>`.
- A3.5. `MemoryX::answer(query_text, ctx_policy)` должен internally вызывать `compile_contract`.

Критерий готовности:

- Появляется публичный API для query через строгий контракт, а старый query path продолжает работать.

## 4. Фаза B: универсальная модель знания

### B1. Source model

Файлы:

- добавить `src/store/source.rs` или расширить `src/cas/evidence.rs`
- обновить `src/store/api.rs`

Подзадачи:

- B1.1. Добавить `SourceId`, `SourceKind`, `SourceRecord`.
- B1.2. Поддержать source kinds: file, page, repository, commit, api, message, table, measurement, human, agent.
- B1.3. Добавить exact location/version fields: path/url, commit hash, byte range, line range, timestamp, source version.
- B1.4. Добавить API `register_source`, `get_source`, `list_sources`.
- B1.5. Сохранение source records должно быть durable и rebuildable из CAS/meta.

Критерий готовности:

- `EvidenceRef` может быть прослежен до `SourceRecord` с конкретной location/version.

### B2. Evidence model upgrade

Файлы:

- `src/cas/evidence.rs`
- `src/store/api.rs`

Подзадачи:

- B2.1. Ввести `EvidenceRecord` как proof-grade объект поверх legacy `EvidenceRef`.
- B2.2. Поля: source_id, source_location, extracted_span, observed_at, extractor, confidence, human_verified.
- B2.3. Добавить conversion `EvidenceRef -> EvidenceRecord` для совместимости.
- B2.4. Обновить provenance chain extraction.
- B2.5. Добавить tests: answer claim -> evidence -> source -> exact location.

Критерий готовности:

- Каждый claim в `AnswerPack` может получить provenance path до source.

### B3. Claim model upgrade

Файлы:

- `src/cas/claims.rs`
- `src/store/api.rs`

Подзадачи:

- B3.1. Добавить `ClaimStatus`: verified, derived, hypothesis, contradicted, superseded, deprecated, unknown.
- B3.2. Добавить `Polarity`, `Modality`, `Qualifier`, `TimeInterval`, `ConfidenceVector`.
- B3.3. Не ломать компактный binary claim format без необходимости: новые поля хранить в meta/evidence/provenance layer, если CAS format менять рискованно.
- B3.4. Добавить `ClaimViewV2` для публичного ответа.
- B3.5. Добавить migration/conversion из текущего `ClaimView`.

Критерий готовности:

- Solver и renderer не могут выдать factual claim без epistemic status.

### B4. Entity and relation authoring model

Файлы:

- добавить `src/store/entity.rs`
- добавить `src/store/relation.rs`
- обновить `src/store/mod.rs`, `src/store/api.rs`

Подзадачи:

- B4.1. Ввести `EntityRecord`: canonical name, aliases, entity_type, claims, merged_from, split_from.
- B4.2. Ввести `RelationRecord`: subject, predicate, object, evidence, valid_time, context, confidence.
- B4.3. Реализовать операции: merge entities, split entity, alias, rename, supersede, deprecate.
- B4.4. Реализовать `correct_claim` и `fork_context`.
- B4.5. Добавить bulk edit и dry-run validation.

Критерий готовности:

- Пользователь/агент может создавать и править знания не вручную через низкоуровневые atoms, а через entity/claim/relation API.

## 5. Фаза C: constraint evaluation engine

### C1. Явный engine проверки условий

Файлы:

- добавить `src/query/constraints.rs`
- обновить `src/query/solver.rs`
- обновить `src/vm/abi.rs` при необходимости

Подзадачи:

- C1.1. Реализовать `ConstraintEvaluator`.
- C1.2. Вход: `QueryContract`, candidate, current context, evidence policy, source policy.
- C1.3. Выход: `Vec<ConstraintResult>`.
- C1.4. Hard constraints должны отбрасывать candidate до ranking.
- C1.5. Soft constraints должны влиять на score, но не подменять hard validation.
- C1.6. Negative constraints должны попадать в `rejected_candidates`.

Критерий готовности:

- ANN/semantic candidate с нарушенным `MUST_NOT` не может попасть в финальный `AnswerGraph`.

### C2. Temporal/context semantics

Файлы:

- `src/store/api.rs`
- `src/query/constraints.rs`
- `src/query/solver.rs`

Подзадачи:

- C2.1. Реализовать `TemporalScope`: before, after, during, valid_at, observed_at, latest.
- C2.2. Реализовать context selectors: active, named, branch, project, user/global, assumption set.
- C2.3. Context должен применяться до ranking.
- C2.4. Добавить tests на устаревшие/superseded claims.
- C2.5. Добавить `AnswerStatus::POLICY_BLOCKED` для случаев запрета policy.

Критерий готовности:

- При одинаковом query, но разных context/time policies, solver выбирает разные допустимые branches явно и воспроизводимо.

### C3. Conflict policy

Файлы:

- `src/query/constraints.rs`
- `src/query/solver.rs`
- `src/store/api.rs`

Подзадачи:

- C3.1. Реализовать `ConflictPolicy`: fail, branch, include_alternatives, prefer_trusted, prefer_recent.
- C3.2. Конфликтующие claims не должны сливаться в нейтральную формулировку.
- C3.3. Добавить `ConflictSet` с branches и resolution policy.
- C3.4. Обновить `NeedBranch` handling, чтобы связать branch с `ConflictSet`.
- C3.5. Добавить MCP-visible conflicts в `query` result.

Критерий готовности:

- Конфликт в данных отражается в `AnswerPack.conflicts` и/или `AnswerPack.alternatives`, а не скрывается.

## 6. Фаза D: расширенный AnswerPack

### D1. AnswerStatus

Файлы:

- `src/store/api.rs`

Подзадачи:

- D1.1. Добавить enum `AnswerStatus`: complete, partial, conflicted, ambiguous, insufficient_evidence, no_match, budget_exhausted, policy_blocked.
- D1.2. Определить deterministic rules для выбора статуса.
- D1.3. `AnswerPack::from_solver` должен выставлять status по coverage/conflicts/policies/budgets.
- D1.4. Добавить tests на каждый status.

Критерий готовности:

- Клиент не должен угадывать состояние ответа по confidence или limitations.

### D2. CoverageReport

Файлы:

- `src/store/api.rs`
- `src/query/solver.rs`

Подзадачи:

- D2.1. Добавить `CoverageReport`: required_total, required_covered, weighted_score, uncovered_required, covered_optional.
- D2.2. Связать gaps с `ConstraintId`.
- D2.3. Расчёт completeness:

```text
weighted_covered_required_gaps / weighted_total_required_gaps
```

- D2.4. Отразить uncovered gaps в `unknowns`.
- D2.5. Добавить tests на weighted completeness.

Критерий готовности:

- Полнота ответа измеряется покрытием обязательных gaps, а не фактом генерации текста.

### D3. ConfidenceVector

Файлы:

- `src/store/api.rs`
- `src/query/cost.rs`

Подзадачи:

- D3.1. Добавить компоненты: evidence_quality, source_reliability, source_independence, constraint_coverage, temporal_relevance, context_consistency, entity_resolution_confidence, inference_depth_penalty.
- D3.2. Старое scalar confidence оставить как derived summary.
- D3.3. Добавить deterministic aggregation.
- D3.4. Добавить tests на независимость источников и temporal relevance.

Критерий готовности:

- `0.87` больше не является единственным объяснением уверенности.

### D4. Rejection summaries and trace

Файлы:

- `src/store/api.rs`
- `src/query/solver.rs`
- `src/query/router.rs`

Подзадачи:

- D4.1. Добавить `RejectedCandidateSummary`: candidate ref, rejected_by, reason, violated_constraints, evidence_status.
- D4.2. Добавить `QueryTrace`: compiled contract, retrieval actions, candidates count, filters, branches, budgets.
- D4.3. Ограничить trace по budget, чтобы MCP output не раздувался.
- D4.4. Добавить CLI/MCP флаг `include_trace`.

Критерий готовности:

- Пользователь может понять, почему кандидат не попал в ответ.

### D5. StructuredAnswer and renderer boundary

Файлы:

- добавить `src/query/render.rs`
- обновить `src/bin/memoryx.rs`

Подзадачи:

- D5.1. Добавить `StructuredAnswer`: sections, statements, statement_kind, claim_refs, evidence_refs.
- D5.2. Запретить factual statement без claim/evidence binding.
- D5.3. Отделить renderer от solver.
- D5.4. CLI по умолчанию может печатать краткий human text, но JSON должен отдавать полный `AnswerPack`.
- D5.5. MCP `query` должен возвращать structured pack, а не только строку.

Критерий готовности:

- LLM/renderer может объяснять, но не может незаметно создавать неподдержанный factual claim.

## 7. Фаза E: federation of retrieval channels

### E1. Общий trait retriever

Файлы:

- добавить `src/query/retrieval.rs`
- обновить `src/query/router.rs`

Подзадачи:

- E1.1. Ввести trait `Retriever`.
- E1.2. Ввести `CandidateV2`:

```rust
pub struct CandidateV2 {
    pub object: KnowledgeObjectRef,
    pub matched_constraints: BitSet,
    pub retrieval_reason: RetrievalReason,
    pub estimated_gain: f32,
    pub estimated_cost: f32,
}
```

- E1.3. Сделать adapters для текущих lexical, semantic, graph paths.
- E1.4. `Router` должен возвращать candidates с reason и matched constraints.
- E1.5. Добавить tests, что semantic retrieval не обходит hard constraints.

Критерий готовности:

- Все retrieval каналы имеют общий контракт и возвращают не ответ, а проверяемых кандидатов.

### E2. Добавить специализированные retrievers

Файлы:

- `src/query/retrieval.rs`
- новые файлы в `src/query/retrievers/`

Подзадачи:

- E2.1. `LexicalRetriever` поверх inverted index.
- E2.2. `SemanticRetriever` поверх ANN.
- E2.3. `EntityRetriever` поверх entity index.
- E2.4. `GraphRetriever` поверх GraphStore.
- E2.5. `TemporalRetriever` поверх valid/observed time metadata.
- E2.6. `NumericRetriever` для числовых constraints.
- E2.7. `CodeRetriever` для repo/code-specific facts, если база используется с проектами.
- E2.8. `CitationRetriever` для provenance/evidence.
- E2.9. `RuleRetriever` для процедур/логических правил.

Критерий готовности:

- Query planner может выбирать канал по типу gap/constraint, а не гонять всё через один поиск.

### E3. Adaptive retrieval planner

Файлы:

- добавить `src/query/planner.rs`
- обновить `src/query/solver.rs`

Подзадачи:

- E3.1. Реализовать utility:

```text
expected_gap_coverage * evidence_quality * constraint_selectivity / execution_cost
```

- E3.2. Учитывать budgets: atoms, I/O, iterations, time, remote calls.
- E3.3. Добавить query trace для выбранных/пропущенных actions.
- E3.4. Сохранить deterministic tie-breaking.
- E3.5. Добавить tests на порядок retrieval actions.

Критерий готовности:

- Solver выбирает следующий retrieval action по ожидаемой полезности, а не по фиксированному порядку.

## 8. Фаза F: доказательный AnswerGraph

### F1. Типизация узлов и рёбер AnswerGraph

Файлы:

- `src/store/api.rs`
- `src/query/solver.rs`

Подзадачи:

- F1.1. Расширить `AgNodeType`: goal, applied_constraint, conclusion, supporting_claim, evidence, source, inference_step, alternative, conflict, missing_requirement, limitation.
- F1.2. Расширить `AgEdgeType`: supports, derived_from, satisfies_constraint, violates_constraint, cites, conflicts_with, supersedes, depends_on.
- F1.3. Добавить validation: every conclusion must have support path.
- F1.4. Добавить transitive proof path extraction.

Критерий готовности:

- `AnswerGraph` становится доказательным объектом, а не просто набором найденных nodes.

### F2. Многокритериальная минимизация

Файлы:

- `src/query/cost.rs`
- `src/query/set_cover.rs`

Подзадачи:

- F2.1. Расширить cost breakdown: coverage, source independence, evidence quality, conflict risk, freshness, inference depth, entity resolution uncertainty, primary source availability.
- F2.2. Не сводить всё только к одному непрозрачному числу: сохранять `CostBreakdown`.
- F2.3. Обновить set cover, чтобы учитывать hard coverage сначала, multi-objective score потом.
- F2.4. Добавить tests на выбор меньшего proof graph при одинаковом coverage.

Критерий готовности:

- Выбранный `AnswerGraph` можно объяснить через cost breakdown.

## 9. Фаза G: authoring API и MCP операции

### G1. Автоматический ingestion pipeline

Файлы:

- добавить `src/ingest/`
- обновить `src/lib.rs`, `src/bin/memoryx.rs`

Подзадачи:

- G1.1. Pipeline: document -> segments -> candidate claims -> entity mentions -> evidence links -> suggested relations.
- G1.2. Сохранять extractor identity, model/tool, confidence, source span.
- G1.3. По умолчанию auto-extracted claims должны иметь статус не выше `HYPOTHESIS` или `EXTRACTED_UNVERIFIED`.
- G1.4. Добавить human/agent confirmation path.
- G1.5. CLI command `ingest --extract-claims --dry-run`.

Критерий готовности:

- Автоматическое извлечение не создаёт молча verified facts.

### G2. Полуструктурированное создание сущностей

Файлы:

- `src/store/entity.rs`
- `src/bin/memoryx.rs`

Подзадачи:

- G2.1. API `create_entity`.
- G2.2. API `add_entity_claim`.
- G2.3. API `create_relation`.
- G2.4. CLI JSON/YAML forms for entity creation.
- G2.5. Tests на создание GPU example из концепта.

Критерий готовности:

- Пользователь может создать entity и claims без ручного binary atom authoring.

### G3. MCP tools для knowledge authoring

Файлы:

- `src/bin/memoryx.rs`

Подзадачи:

- G3.1. Добавить MCP tool `create_entity`.
- G3.2. Добавить MCP tool `add_claim`.
- G3.3. Добавить MCP tool `create_relation`.
- G3.4. Добавить MCP tool `merge_entities`.
- G3.5. Добавить MCP tool `split_entity`.
- G3.6. Добавить MCP tool `supersede_claim`.
- G3.7. Добавить MCP tool `correct_claim`.
- G3.8. Для каждого tool обязательны description и examples.

Критерий готовности:

- Агент через MCP может не только искать, но и нормально вести базу знаний.

## 10. Фаза H: LLM boundary

### H1. Контракт внешней LLM

Файлы:

- добавить `docs/LLM_BOUNDARY.md`
- возможно добавить `src/query/llm_boundary.rs` только с типами, без провайдера

Подзадачи:

- H1.1. Описать allowed operations: propose QueryContract, propose candidate claims, propose entity links, render explanation.
- H1.2. Описать forbidden operations: verify facts, hide conflicts, change hard constraints, invent source, mark complete.
- H1.3. Добавить `Proposal<T>` wrapper: proposed_by, model, timestamp, confidence, validation_status.
- H1.4. В `AnswerPack` отделить proposed text от validated claims.

Критерий готовности:

- Любая LLM-интеграция подключается только как proposer/renderer, не как source of truth.

## 11. Фаза I: snapshot, reproducibility и масштабирование

### I1. Snapshot identity в AnswerPack

Файлы:

- `src/store/api.rs`
- `src/cas/io.rs`
- `src/crdt/snapshot.rs`

Подзадачи:

- I1.1. Добавить `KnowledgeSnapshotId`.
- I1.2. Включать в него CAS manifest/version, graph manifest, index generation, context id, solver version.
- I1.3. `AnswerPack.snapshot` должен быть обязательным.
- I1.4. Добавить command/API `snapshot`.

Критерий готовности:

- Ответ можно привязать к конкретной версии базы.

### I2. Rebuildable indexes guarantee

Файлы:

- `src/index/mod.rs`
- `src/graph/store.rs`
- `src/store/api.rs`

Подзадачи:

- I2.1. Добавить API `rebuild_indexes_from_cas`.
- I2.2. Проверить восстановление idloc, inverted, graph, entity index.
- I2.3. Добавить corruption/recovery tests.
- I2.4. Документировать, что semantic index не source of truth.

Критерий готовности:

- Индексы можно удалить и восстановить из CAS без потери знания.

### I3. Sharding/federated planning

Файлы:

- `src/federation/mod.rs`
- `src/query/planner.rs`

Подзадачи:

- I3.1. Описать `ShardDescriptor`.
- I3.2. Query planner должен решать, какие gaps отправлять на какой shard.
- I3.3. Federation должна передавать claims/provenance, не готовый текст.
- I3.4. Добавить remote budget и trust policy.

Критерий готовности:

- Federation работает как расширение knowledge fabric, а не как обмен ответами.

## 12. Фаза J: CLI, MCP, docs

### J1. CLI для QueryContract

Файлы:

- `src/bin/memoryx.rs`
- README.md

Подзадачи:

- J1.1. `memoryx query --contract contract.json`.
- J1.2. `memoryx query --emit-contract "natural query"`.
- J1.3. `memoryx query --json` должен отдавать полный `AnswerPack`.
- J1.4. `memoryx query --include-trace`.
- J1.5. `memoryx query --explain-rejections`.

Критерий готовности:

- Пользователь может увидеть и отредактировать contract до выполнения.

### J2. MCP для QueryContract и AnswerPack

Файлы:

- `src/bin/memoryx.rs`

Подзадачи:

- J2.1. Обновить `query`: вход может быть `query_text` или `contract`.
- J2.2. Добавить `compile_query_contract`.
- J2.3. Добавить `validate_query_contract`.
- J2.4. Добавить `explain_answer_graph`.
- J2.5. Добавить `get_provenance_path`.
- J2.6. Обновить descriptions/examples для всех новых tools.

Критерий готовности:

- Codex/IDE agent может использовать MemoryX как строгую базу знаний, не угадывая внутренние структуры.

### J3. README и concept docs

Файлы:

- README.md
- добавить `docs/QUERY_CONTRACT.md`
- добавить `docs/ANSWER_PACK.md`
- добавить `docs/AUTHORING_API.md`

Подзадачи:

- J3.1. Переписать центральное описание: MemoryX как исполняемая knowledge fabric.
- J3.2. Добавить отличие от RAG по новой модели.
- J3.3. Добавить examples: simple query, contract query, conflicted answer, partial answer, unsupported fact.
- J3.4. Добавить MCP examples для Codex.
- J3.5. Добавить статус реализации по фазам.

Критерий готовности:

- Новый пользователь понимает, что MemoryX не vector DB и не обычный RAG.

## 13. Фаза K: tests, benchmarks, acceptance

### K1. Golden scenarios

Файлы:

- добавить `tests/query_contract.rs`
- добавить `tests/answer_pack.rs`
- добавить `tests/conflict_branching.rs`
- добавить `tests/provenance_paths.rs`

Сценарии:

- K1.1. Query with MUST/MUST_NOT/SHOULD.
- K1.2. Conflicting claims produce branches.
- K1.3. Missing evidence produces `INSUFFICIENT_EVIDENCE`.
- K1.4. Partial gap coverage produces `PARTIAL`.
- K1.5. Unsupported factual statement is rejected.
- K1.6. Same snapshot + same contract returns same logical result.

Критерий готовности:

- Основные инварианты концепта покрыты regression tests.

### K2. RAG comparison benchmark

Файлы:

- добавить `benchmarks/`
- добавить `docs/BENCHMARK_RAG_COMPARISON.md`

Подзадачи:

- K2.1. Создать небольшой dataset с конфликтами, temporal changes, multi-hop facts, missing evidence.
- K2.2. Метрики: hard constraint accuracy, conflict visibility, provenance completeness, gap reporting, reproducibility.
- K2.3. Не использовать неподтверждённые marketing claims.
- K2.4. Результаты хранить как reproducible scripts.

Критерий готовности:

- Можно честно показать, где MemoryX решает задачи, которые обычный RAG часто смешивает.

### K3. Full verification gate

Команды:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features --quiet
cargo build --release --features mcp
```

Критерий готовности:

- Все команды проходят без warnings/errors.

## 14. Рекомендуемый порядок выполнения

### Milestone 1: Public contract core

1. A1 QueryContract types.
2. A2 compiler baseline.
3. A3 adapter into existing solver.
4. J1 CLI `--emit-contract` and `--contract`.
5. Tests for contract serialization and basic solving.

Результат:

- MemoryX получает центральную публичную модель запроса без ломки текущего solver.

### Milestone 2: Proof-grade answer

1. D1 AnswerStatus.
2. D2 CoverageReport.
3. D3 ConfidenceVector.
4. D4 Rejection summaries and trace.
5. F1 typed proof AnswerGraph.

Результат:

- Ответ становится проверяемым объектом, а не просто набором claims/graph/confidence.

### Milestone 3: Source/Evidence/Claim integrity

1. B1 Source model.
2. B2 Evidence model upgrade.
3. B3 Claim status and epistemic typing.
4. H1 LLM boundary.

Результат:

- Нельзя выдать factual claim без статуса и provenance path.

### Milestone 4: Constraint-first solving

1. C1 constraint evaluator.
2. C2 temporal/context semantics.
3. C3 conflict policy.
4. E1 common retriever trait.
5. E3 adaptive planner basic version.

Результат:

- Retrieval перестаёт быть решающим механизмом; он только поставляет candidates.

### Milestone 5: Knowledge authoring

1. B4 entity/relation model.
2. G1 automatic ingestion pipeline.
3. G2 semi-structured entity creation.
4. G3 MCP authoring tools.

Результат:

- MemoryX становится базой, которую агент может полноценно вести через MCP.

### Milestone 6: Federation, scale, docs, benchmark

1. I1 snapshot identity.
2. I2 rebuildable indexes.
3. I3 shard/federated planning.
4. J2/J3 docs and MCP updates.
5. K2 benchmark.

Результат:

- Проект готов как публичная local-first knowledge fabric, а не только как исследовательский движок.

## 15. Риски и решения

### Риск 1. Слишком большой breaking change

Решение:

- Ввести `QueryContract` как верхний слой.
- Оставить `GoalSpec` внутренним и совместимым.
- Обновлять CLI/MCP постепенно.

### Риск 2. Раздувание AnswerPack

Решение:

- По умолчанию отдавать compact pack.
- Полный trace/provenance/rejections включать флагами.
- MCP tools должны иметь параметры `include_trace`, `include_rejections`, `max_trace_items`.

### Риск 3. Смешивание LLM и verified core

Решение:

- Ввести `Proposal<T>` и `EpistemicStatus`.
- LLM output никогда не становится verified без MemoryX validation.

### Риск 4. Изменение CAS binary format

Решение:

- Новые поля сначала хранить в meta/provenance/entity layers.
- CAS atom body менять только после migration plan.

### Риск 5. Слишком сложный planner

Решение:

- Сначала deterministic utility formula.
- Потом adaptive tuning.
- Все решения planner логировать в `QueryTrace`.

## 16. Definition of Done для расширения

Расширение можно считать реализованным, когда выполнены условия:

- Natural query компилируется в публичный `QueryContract`.
- Query можно выполнить напрямую по JSON contract.
- Hard/negative constraints применяются до ranking.
- Semantic retrieval не может самостоятельно установить выполнение условия.
- `AnswerPack` содержит `AnswerStatus`, `CoverageReport`, `ConfidenceVector`, conflicts, alternatives, unknowns, limitations, provenance paths и snapshot id.
- Каждый factual statement имеет claim/evidence/source path.
- Конфликты образуют `ConflictSet`/branches, а не сглаживаются.
- Gaps дают измеримую completeness.
- LLM boundary документирован и технически выражен в типах.
- MCP поддерживает contract query, proof answer и authoring operations.
- Есть regression tests на constraints, conflicts, provenance, partial answers, unsupported facts и reproducibility.
- `cargo fmt`, `cargo clippy -D warnings`, `cargo test`, release build проходят без warnings/errors.

