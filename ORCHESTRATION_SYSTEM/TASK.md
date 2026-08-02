# Task

Status: `ENGLISH_INTER_AGENT_CONTRACT_VALIDATED`

Implement and publish a durable English-only inter-agent communication
contract across the existing MemoryX modular orchestration. Enforce the rule
for prompts, task packets, handoffs, progress and evidence narratives,
lifecycle recovery instructions, and stable prefixes. New contours must
inherit the rule from `build_scheme.ps1`; supplied prompts and persisted
communication surfaces must fail closed under focused validators.

Acceptance boundary:

- preserve all MemoryX concept invariants and the open N5 roadmap state;
- keep MX-95, every real session binding, immutable execution profile,
  project-local base, and one-owner rule unchanged;
- never synthesize session UUIDs or use user/foreign bases;
- validate structure and physical base layout without claiming unobserved live
  Codex hook, compact, cache, or model-quality behavior;
- exercise negative persisted-surface controls only in isolated ignored
  fixtures below `target/`;
- do not modify runtime Rust, release tags, binaries, or user-owned files.

Final checkpoint evidence also includes direct execution of all twelve
SessionStart scripts and an acquire/release smoke of the shared-host resource
lease. Neither is treated as a real Codex lifecycle event.

User-facing language remains user-selected. The root orchestrator translates a
bounded request into English before invoking or handing work to another agent.
