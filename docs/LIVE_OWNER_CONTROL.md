# Live Owner Control

MemoryX preserves one exclusive writer per physical base. The lease prevents
two processes from independently mutating the same files. The live-owner
control protocol adds supported client access without weakening that rule.

## Supported Topology

The first `memoryx serve --base <path> --stdio` process owns the base and starts
the MCP server. It also binds an ephemeral TCP listener on the loopback
interface and writes `<base>/.memoryx.control.json`.

A second `memoryx serve --base <path> --stdio`:

1. fails to acquire the writer lease as expected;
2. reads and validates the live-owner descriptor;
3. authenticates with its random token over loopback;
4. relays MCP requests to the owning process;
5. never opens a second `MemoryX` writer.

JSON-RPC notifications remain response-free. Request and response lines use the
same size bounds as the primary stdio transport. All tool calls are serialized
through the owner's MCP state.

Different physical bases remain independent and can have separate owners.

## Script Access

Use the one-shot client when a script needs one MCP operation against a live
base:

```powershell
memoryx --format json client `
  --base E:\project\.memoryx\bases\project `
  --tool get_stats `
  --arguments '{}'
```

Integrity verification:

```powershell
memoryx --format json client `
  --base E:\project\.memoryx\bases\project `
  --tool verify_integrity `
  --arguments '{}'
```

Audit the projection from current relation journal records into context
`active_claims` without changing the base:

```powershell
memoryx --format json client `
  --base E:\project\.memoryx\bases\project `
  --tool audit_relation_contexts `
  --arguments '{}'
```

If every reported issue is explicitly marked `repairable: true`, reconcile
the durable relation atoms through the same live owner:

```powershell
memoryx --format json client `
  --base E:\project\.memoryx\bases\project `
  --tool repair_relation_contexts `
  --arguments '{}'
```

The repair is idempotent. It replaces an equivalent active claim's atom
identity with the current relation journal atom and records a normal `repair`
history entry. A distinct equivalent single-claim atom is linked as superseded:
it remains readable in CAS/history with all source attachments, but is excluded
from the current view and normal retrieval. The structured result lists it in
`retired_parallel_atom_ids`. The repair does not rewrite relation records or
detach sources. Missing relation atoms, multi-claim parallel atoms, atom/claim
mismatches, unavailable non-default contexts, and conflicting active values
fail closed.

### Relation/Context Non-Regression Contract

The audit and repair restore the existing invariant that every current,
non-deprecated relation journal record has its canonical relation atom in the
declared context's `active_claims`. They do not introduce a new relation model.

- Existing base formats and relation/context semantics remain readable. Audit
  is read-only; no migration or repair is performed implicitly on open.
- Repair is an additive, explicit operator action. It is idempotent,
  provenance/history preserving, and fail-closed on missing, ambiguous,
  conflicting, multi-claim, or mismatched data.
- Solver, QueryContract, AnswerGraph, federation, CAS/Merkle, CRDT,
  replication, scoped storage, and live-owner rules are not redefined by this
  repair.
- N5 operation crash atomicity remains open. This repair does not claim atomic
  pre-state/post-state behavior across all persistence boundaries.
- A future change that requires different relation or context semantics must
  be proposed and accepted as a concept change; it must not be hidden inside
  audit, repair, migration, or orchestration work.

Any base-selectable MCP tool can be called. `--arguments` must be one JSON
object. The command returns the complete MCP JSON-RPC response.

`get_stats` distinguishes:

- `cas_live_*`: non-tombstoned CAS records, including superseded history;
- `current_*`: non-tombstoned atoms not superseded by a newer atom;
- `active_relation_count`: non-deprecated relations not superseded by another
  relation;
- `relation_context_audit`: the independent cross-projection check, including
  the count of current relation atoms actually active in their contexts;
- `physical_graph_*`: stored topology, including historical nodes and edges.

`verify_integrity` verifies every non-tombstoned CAS atom through the process
that already owns the base.

## Diagnostics

Machine-readable CLI errors use:

```json
{
  "schema": "memoryx.cli-error.v1",
  "ok": false,
  "exit_code": 73,
  "error": {
    "code": "BASE_WRITER_LEASE_HELD",
    "message": "...",
    "retryable": false,
    "base_path": "...",
    "control_descriptor": "..."
  }
}
```

Stable cases:

| Condition | Error code | Exit code |
| --- | --- | --- |
| A direct writer command targets an owned base | `BASE_WRITER_LEASE_HELD` | 73 |
| The owner is live but its control endpoint cannot be reached | `LIVE_OWNER_CONTROL_UNAVAILABLE` | 69 |
| The descriptor/authentication/protocol is invalid | `LIVE_OWNER_CONTROL_PROTOCOL_ERROR` | 69 |

For stdio serving, `client`, or global `--format json`, the final diagnostic is
written as one JSON line to standard error. A wrapper should read standard
error as well as the process exit code. A broken stdin write is not the
authoritative lease diagnostic.

Do not kill the owner merely to validate a base. Use the proxy, `client`,
`get_stats`, or `verify_integrity`.

## Trust And Lifecycle

The endpoint accepts loopback connections only. Authentication uses a random
256-bit token stored in the descriptor. Access to the descriptor is equivalent
to access to the live owner's MCP tools, so the base directory must be
protected by normal operating-system permissions. This is a same-user local
control boundary, not a remote network service.

The descriptor is removed on a clean owner shutdown. A stale, malformed,
wrong-base, or unreachable descriptor fails closed. The descriptor is runtime
metadata, not part of durable knowledge and not required to reopen the base.

An owner started by a MemoryX version without this protocol cannot be attached
to retroactively. Restart that owner with the updated executable during a
planned client restart; do not replace or migrate its open base in place.
