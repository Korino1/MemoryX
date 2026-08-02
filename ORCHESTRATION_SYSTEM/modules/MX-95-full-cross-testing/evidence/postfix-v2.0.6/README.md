# MX-95 MemoryX 2.0.6 Evidence Bundle

Run ID: `20260802T085223672Z`

This bounded bundle joins the authoritative 47-tool MCP surface to 47 direct
tool cases, 66 observed calls, 14 cross-tool sequences, 18 resilience cases,
32 same-process deterministic queries, one reopen query, and nine fail-closed
validator controls.

The resilience evidence includes an executable legacy tombstone fixture. It
proves that unresolved current-relation tombstones fail closed, dry-run does
not write, an explicit reviewed restore creates a verified backup and replay
journal, repeat is idempotent, and reopen remains semantically consistent.

The bundle does not prove that restoring is the correct operator decision for
any KPA relation. It does not close N5, prove real hook/compact execution,
cache reuse, model quality, or total MemoryX semantic acceptance.
