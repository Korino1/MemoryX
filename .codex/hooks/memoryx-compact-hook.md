# MemoryX Compact Hook Protocol

This is a local project hook protocol for Codex sessions.

Codex currently guarantees project instruction loading through `AGENTS.md`.
Therefore the practical hook is:

1. `AGENTS.md` is loaded on session start and after compact/context rebuild.
2. `AGENTS.md` forces the agent to load `ORCHESTRATOR_CONTEXT_COMPACT.md`.
3. `ORCHESTRATOR_CONTEXT_COMPACT.md` points back to the full implementation and orchestration plans.
4. `refresh_orchestrator_context.ps1` regenerates the compact file from durable sources.

## Manual Refresh

Run:

```powershell
pwsh -NoLogo -ExecutionPolicy Bypass -File .codex/hooks/refresh_orchestrator_context.ps1
```

## Required Durable Sources

- `CONCEPT_EXTENSION_IMPLEMENTATION_PLAN.md`
- `ORCHESTRATION_PLAN.md`
- `AGENT_PROGRESS_LOG.md`
- `git log --oneline -10`
- `git status --short`

## After Compact

The agent must:

1. Read `ORCHESTRATOR_CONTEXT_COMPACT.md`.
2. Reconfirm non-regression contract.
3. Continue as MemoryX Orchestrator.
4. Avoid re-reading the whole repository unless the next task requires it.

