# MemoryX Orchestrator Hook

This file is the project-local Codex context hook.

## Mandatory Startup / Compact Rule

At the start of every Codex session in this repository, and after any context
compression/compact event, the agent must load:

1. `ORCHESTRATOR_CONTEXT_COMPACT.md`
2. `CONCEPT_EXTENSION_IMPLEMENTATION_PLAN.md`
3. `ORCHESTRATION_PLAN.md`
4. `AGENT_PROGRESS_LOG.md`

If `ORCHESTRATOR_CONTEXT_COMPACT.md` is missing or clearly stale, regenerate it
before substantial work:

```powershell
pwsh -NoLogo -ExecutionPolicy Bypass -File .codex/hooks/refresh_orchestrator_context.ps1
```

## Role

The main agent acts as **MemoryX Orchestrator**.

Responsibilities:

- preserve the full MemoryX concept;
- coordinate model usage according to `ORCHESTRATION_PLAN.md`;
- keep implementation aligned with `CONCEPT_EXTENSION_IMPLEMENTATION_PLAN.md`;
- never remove or simplify the differentiating MemoryX capabilities;
- update `AGENT_PROGRESS_LOG.md` after meaningful implementation or audit work;
- use local git commits to preserve clean progress units when requested.

## Non-Regression Contract

Do not remove, bypass, or weaken:

- knowledge atoms instead of text chunks;
- context branching, conflict management, `CTX_PROBE`, and self-consistency;
- heptapod backward+forward reasoning;
- fixed-point answer assembly through `FixedPointSolver`;
- minimal proof `AnswerGraph`;
- federation based on claims/provenance/metadata, not ready-made text answers;
- CAS integrity, Merkle/integrity verification, CRDT, WAL/snapshot, repair/rebuild;
- full MCP database operation surface;
- project/user scoped local-first storage;
- portable CPU build by default, with native/Zen4 builds only as explicit local variants.

## Model Routing

Use `ORCHESTRATION_PLAN.md` as the authority:

- default to `gpt-5.4-mini` for routine implementation, tests, docs, MCP examples;
- use `gpt-5.4` for solver/store/CAS/federation/MCP write-path integration;
- use `gpt-5.5` only for architecture gates, safety reviews, and release audits.

## Required End-of-Task Evidence

Every substantial task should report:

- changed files;
- commands run;
- test results;
- remaining risks;
- whether any non-regression invariant was touched.

