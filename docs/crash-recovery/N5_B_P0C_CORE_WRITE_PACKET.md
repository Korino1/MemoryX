# N5-B P0-C Core Write Integration Packet

Status: `READY_FOR_SHARED_SURFACE_AUTHORIZATION_SOURCE_UNCHANGED`

Owner: `MX-20`

Canonical authority: `CONCEPT_EXTENSION_IMPLEMENTATION_PLAN.md`, section
`N5. Operation crash atomicity`

Bound private checkpoint: `src/store/operation_txn.rs`, 376,500 bytes,
SHA-256
`2DF480E87F78CAD4810EAD342C0F50238FA26A369322540994DC092AA72A5C44`
at HEAD `2312390acd23ac731f03c8ea2acc790386cfc663`.

## 1. Outcome And Ownership Gate

The production write paths do not use immutable operation generations. A
coherent first N5-B implementation cannot be made only in
`src/store/operation_txn.rs`:

1. `MemoryX` already owns the physical-root `BaseLease`, while
   `OperationTransaction::begin` acquires a second lease. N5-B requires an
   explicitly typed handoff from the live owner, not nested acquisition.
2. The private v1 registry admits only `n5-fixture/state.v1` and
   `n5-fixture/history.v1`. Every `cas/`, `index/`, `graph/` and `meta/` path
   deliberately returns `Unsupported` until a production semantic adapter is
   approved.
3. Production open, mutation, in-memory visibility and transport entrypoints
   reside in shared or foreign-owned files. Adding an unused adapter in the
   owned file would not be a functional transaction slice.
4. The existing v1 wire records and six golden vectors name the disposable
   registry. Reinterpreting them for production would be a hidden on-disk
   compatibility change.
5. Public core methods are called from composite entity/relation operations.
   Wrapping them naively would commit the atom before the caller mutates
   contexts and registries, creating a nested or partial composite operation.

Therefore this packet makes no Rust edit. Production source work is gated on
the handoffs in section 9.

## 2. CodeGraph-First Audit Boundary

CodeGraph was queried before manual source exploration for all four methods,
their callers, persistence effects and `operation_txn` usage. It located the
primary `src/store/api.rs` and `src/bin/memoryx.rs` routes but did not return
complete method bodies from the large store file. Targeted current-source
inspection then supplied the missing bodies and exact ordering. CodeGraph is
architecture-routing evidence only, not correctness proof.

## 3. Entrypoint Inventory

### Public/library

| Operation | Primary entrypoint | Transitive composite callers |
| --- | --- | --- |
| ingest | `MemoryX::ingest`, `src/store/api.rs:9357` | `add_entity_claim` and `assert_relation` |
| batch ingest | `MemoryX::batch_ingest`, `src/store/api.rs:10915` | none inside `MemoryX`; external callers may invoke it directly |
| update | `MemoryX::update_atom`, `src/store/api.rs:11106` | `correct_relation` and `transition_relation` |
| delete | `MemoryX::delete_atom`, `src/store/api.rs:11245` | direct library callers; current-relation deletion preflight refuses |

The four methods are public, so downstream library users are also entrypoints.
The composite callers are N5-C operations. N5-B must expose an internal
non-nestable transaction context or fail those composite routes before their
first mutation until N5-C is active.

### CLI

| CLI command | Route |
| --- | --- |
| `ingest` | `Commands::Ingest` -> `cmd_ingest` -> `MemoryX::batch_ingest` |
| `import` | `Commands::Import` -> `cmd_import` -> `MemoryX::batch_ingest` |

There is no separate production CLI command for direct `update_atom` or
`delete_atom`; those operations are exposed through MCP and the library.

### MCP and live owner

`process_mcp_request` dispatches:

- `ingest` -> `mcp_ingest_response` -> `MemoryX::ingest`;
- `batch_ingest` -> `mcp_batch_ingest_response` ->
  `MemoryX::batch_ingest`;
- `update_atom`, `supersede_claim`, and `correct_claim` ->
  `mcp_update_atom_response` -> `MemoryX::update_atom`;
- `delete_atom` -> `mcp_delete_atom_response` -> `MemoryX::delete_atom`.

`mcp_with_selected_store` selects a retained `MemoryX` from
`McpServerState::stores`. Each connected store is the live owner and retains
its `BaseLease`; stdio and network serve paths share this dispatch. A
transaction must borrow this authority rather than opening a second owner.

### Non-production adapters that must retain parity

- `examples/mcp_server_full.rs` exposes all four operations.
- `examples/mcp_server.rs` and `examples/native_api.rs` perform ingest/batch
  routes, including a native batch loop that calls `ingest` item by item.

These are not current production transport evidence, but they must not remain
an undocumented semantic bypass after the public library method changes.

## 4. Persistent Component Inventory And Current Ordering

### Component set

| Component | Durable paths | Core operations |
| --- | --- | --- |
| CAS records and segment index | `cas/seg_NNNNN.dat`, `cas/seg_NNNNN.idx` | ingest, each accepted batch item, update, delete tombstone |
| location state | `index/location_state.bin`, `index/idloc.mmap` | all four; delete changes the original and adds the tombstone |
| lexical index | `index/terms.lex`, `index/terms.post` | ingest, accepted batch items, update; delete relies on visibility filtering rather than posting removal |
| graph | `graph/delta_N.edges`, `graph/graph.manifest`, and when compaction triggers `graph/edges_T.offsets`, `graph/edges_T.targets`, `graph/edges_T.attrs` | all four; delete adds `TOMBSTONE_LINK` |
| atom metadata/node map | `meta/meta_state.bin` | all four; delete changes original trust and adds tombstone metadata |
| operation history | `meta/history.log` | all four, currently without a stable transaction ID field |
| context projection | `meta/contexts.json` | not semantically changed by a direct core operation, but rewritten by unconditional `flush()` |
| embedding index | `index/embeddings.bin` | not semantically changed by these methods, but rewritten by unconditional `flush()` |

Evidence references are encoded in the CAS atom and projected as graph
`DERIVED_FROM` edges. Direct core methods do not append
`meta/atom_sources.jsonl` or mutate `meta/sources.jsonl`. The CRDT replication
WAL is not part of this operation path and remains independent.

### Current bypass sequence

For ingest and update, the code validates only part of the request, appends and
flushes the CAS record and segment index, then mutates location, graph, lexical
and metadata state in memory. Delete appends and flushes a tombstone before it
marks the original deleted and changes graph/metadata projections. Batch
repeats the CAS append/flush and in-memory projections per accepted item.

After those mutations, `record_history` appends, flushes and `sync_data`s
`meta/history.log`. Only then does `MemoryX::flush` persist, in order:

1. CAS flush;
2. location state and idloc;
3. lexical index;
4. graph layers and graph manifest;
5. metadata;
6. contexts;
7. embeddings.

No step calls `OperationTransaction`. An error after any earlier step can
leave a durable CAS orphan, prematurely durable history, an in-memory visible
partial state, or a hybrid reopened projection. CAS orphans are permitted only
when no committed location/index/metadata generation makes them live.

## 5. Required Transaction Boundary

One externally invoked core method call is one non-nestable transaction:

- `ingest`: one atom request;
- `batch_ingest`: the entire request, with one deterministic preflight result
  containing the accepted ordered subset and item errors;
- `update_atom`: the new atom plus supersession projection;
- `delete_atom`: tombstone creation plus original visibility change.

All validation that can be performed without persistence must complete before
the first write. The transaction then:

1. receives a stable transaction ID and canonical request intent;
2. borrows the live owner's exclusive/quiescent lease capability;
3. recovers and validates the current committed generation;
4. builds detached post-state projections without mutating live indexes,
   graph, metadata or history;
5. may append CAS bytes as physically orphanable data, but does not expose
   their location before commit;
6. stages typed immutable production components and a history entry bound to
   the transaction ID;
7. publishes canonical `commit.bin`, the sole logical visibility record;
8. installs or replays the exact committed post-state and swaps the live
   in-memory view;
9. acknowledges only the state bound by the commit.

If commit is durable but install fails, the call may return an error only while
recovery deterministically yields the exact committed post-state. If commit is
not durable, recovery must yield exact pre-state; physical CAS orphans remain
non-queryable.

## 6. Exact Logical Oracle

Let `S0` be the validated committed semantic projection and let `Plan(O, S0)`
be the deterministic preflight plan for request `O`. Let `S1` be the no-fault
application of that plan. Every returned error or injected crash must reopen
twice to exactly `S0` or `S1`; acknowledgment requires exactly `S1`.

The production projection under the existing
`memoryx.logical-state-digest.v1` identifier must be versioned and ratified
before code. It must deterministically include:

- committed generation and parent/commit identity;
- live/tombstoned atom identities and validated CAS body hashes;
- AtomId-to-location/node mapping and deletion state;
- canonical term-to-node postings;
- canonical graph node/edge projection, including type and confidence;
- atom metadata and node reverse mapping;
- transaction-ID-bound history sequence exactly once;
- source/evidence projection relevant to each atom;
- hashes of unchanged contexts and embeddings when they remain part of the
  admitted base.

Uncommitted physical CAS bytes are recorded by a separate orphan/integrity
projection and are excluded from logical visibility. A matching digest without
query/search/history/provenance/integrity agreement does not pass.

### Operation postconditions

- Ingest: the canonical atom is reachable once through CAS/location, its node,
  terms, claim/evidence graph edges, metadata and one history record agree.
- Batch: every preflight-accepted item appears in input order as one committed
  batch post-state and every rejected item remains absent; the batch history
  record appears once. An empty accepted set is a no-op `S1 = S0`.
- Update: the old atom remains durable, the new atom is reachable, the exact
  `SUPERSEDES` and evidence/claim edges exist, metadata agrees and history
  appears once.
- Delete: the original becomes non-queryable but remains auditable, the
  tombstone CAS/location/metadata and `TOMBSTONE_LINK` agree, and history
  appears once.

Different-transaction duplicate AtomIds, same-ID update, timestamp binding and
direct update of relation-backed atoms lack a frozen compatibility decision.
Admission must fail closed for those ambiguous cases until MX-10/MX-50/MX-80
approve exact semantics; this packet does not silently change them.

## 7. Failpoint And Permanent MX-95 Ratchet Handoff

The existing 99 stable N5-A IDs remain unchanged. N5-B must either map every
new repeated production component occurrence to those generic boundaries or
add a separately versioned extension without renaming an accepted ID. Required
new functional boundaries include CAS record/index durability, detached-plan
serialization, post-commit install, recovery replay and history-once install.

Permanent functional scenarios for MX-95:

| Scenario ID | Failure model and oracle |
| --- | --- |
| `SC-MX95-N5B-INGEST-CAS-BEFORE-COMMIT` | abort after CAS record/index durability; atom remains absent from all live projections on two reopens; same-ID retry commits once |
| `SC-MX95-N5B-INGEST-HISTORY-BEFORE-DERIVED` | prevent the legacy early-history hybrid; history and atom are both absent pre-commit or both present post-commit |
| `SC-MX95-N5B-BATCH-MID-CAS` | abort between accepted batch-item CAS appends; no subset becomes visible; retry commits the full planned subset once |
| `SC-MX95-N5B-UPDATE-BEFORE-SUPERSEDES` | abort after new CAS bytes but before supersession projection; reopen is exact old state or exact full update |
| `SC-MX95-N5B-DELETE-BEFORE-TOMBSTONE-LINK` | abort after tombstone bytes or deletion staging; original is either fully live or fully tombstoned with audit link/history |
| `SC-MX95-N5B-COMMIT-BEFORE-INSTALL` | abort after `commit.bin` publication but before live install; first recovery rolls forward and second reopen is byte/logically identical |
| `SC-MX95-N5B-RETURNED-ERROR-RETRY` | each transport returns only an exact pre/post state; same transaction ID produces history once and conflicting reuse fails closed |
| `SC-MX95-N5B-COMPOSITE-NONNESTING` | entity/relation callers cannot commit a core atom and then fail outside the transaction; they fail before mutation until N5-C or use one outer transaction |

Each scenario requires a direct case, meaningful cross sequence, explicit
oracle, first/second reopen, same-ID retry, evidence path and source boundary
row. No count-only test is authorized. This packet adds no test because no
functional source slice was authorized.

## 8. Migration And Compatibility Gate

- Private v1 records and six golden vectors must not be reinterpreted as a
  production registry.
- MX-80 must choose and freeze either a new production format version or an
  explicitly separate compatible registry/adapter marker with strict codecs
  and golden bytes.
- Production generation-zero activation must verify the complete supported
  legacy layout, not the disposable two-file fixture.
- `MemoryX::new` must recover/validate the format before any component opens or
  reconciles mutable state. The current startup reconciliation can mutate
  metadata before transaction admission and is therefore part of the gate.
- Current-writer format refusal must be wired before write admission. No claim
  may be made that historical binaries refuse the new format without executed
  historical-binary evidence.
- Functional Unix support or an explicit product-level unsupported-platform
  decision is required before portable production activation.

## 9. Recorded Cross-Module Handoffs

- `MX-10`: define typed CAS orphan descriptors, stable CAS record identity,
  detached location/idloc and lexical serialization, copy/space bounds and
  durability boundaries. Decide duplicate AtomId semantics. No MX-10 file was
  edited.
- `MX-50`: freeze transaction-ID-bearing history-once encoding, timestamp
  binding, supersession/tombstone postconditions, evidence/source projection
  and ambiguous direct-update policy. No MX-50 file was edited.
- `MX-60`: make graph delta/manifest and automatic compaction expressible as a
  detached staged projection; CRDT/federation WAL remains independent. No
  MX-60 file was edited.
- `MX-70`: define typed live-owner lease borrowing, library/CLI/MCP transaction
  ID and retry envelope, transport acknowledgment semantics, base selection and
  example parity. No MX-70 file was edited.
- `MX-80`: version the production component registry/digest/wire records,
  strict codecs/goldens, generation-zero legacy layout, downgrade gate and
  migration compatibility. No MX-80 file was edited.
- `MX-95`: ratify the new boundary inventory and add the eight functional
  ratchet scenarios across library, CLI, MCP and live-owner execution. No
  MX-95 file was edited.

`src/store/api.rs` is a declared MX-20 shared surface. The root orchestrator
must authorize its edit set only after the above owners return compatible
contracts.

## 10. Executable Next Packet

After explicit shared-surface authorization:

1. Freeze the production registry/digest/history/transaction-ID codecs and
   golden vectors with MX-80/MX-50.
2. Add a typed borrowed-owner capability; keep `BaseLease` ownership in
   `MemoryX` and prohibit nested acquisition.
3. Add detached adapters for CAS visibility, location/idloc, lexical, graph,
   metadata and history with checked resource bounds.
4. Add one internal non-nestable coordinator and split public direct methods
   from N5-C outer-transaction helpers.
5. Wire `MemoryX::new` recovery before mutable component open/reconciliation.
6. Activate direct library `ingest` first on disposable production-layout
   fixtures; keep batch/update/delete and every transport fail closed until
   their own ratchet passes.
7. Execute the direct plus subprocess ingest matrix, first/second reopen,
   same-ID retry and conflicting reuse before authorizing the next operation.

Required focused gates include fmt, all-feature library check, warnings-denied
clippy, operation-specific unit tests, BaseLease tests and MX-95 subprocess
oracles under shared-host coordination. Full repository/release gates remain
later N5-E/MX-90 work.

## 11. 2026-08-28 Implementation Gate Result

The narrow shared source set in section 10 was later authorized, and the
amended MX-80 registry-v2 graph reconciliation was consumed. Implementation
still cannot begin coherently under the complete accepted contract set:

- MX-70 section 5 requires MX-80 ratification of
  `memoryx.base-binding.v1`, platform canonical-root/stable-root-identity
  encoding and the direct-ingest intent/receipt/failure codecs before
  implementation.
- Amended MX-80 section 13 explicitly states those codecs and platform bytes
  were not ratified; section 14 retains them as implementation prerequisites.
- The later graph reconciliation fixes only registry/DELT/GRM1 semantics and
  explicitly leaves the base/envelope registration open.

These bytes participate in intent hashing, parent binding, same-ID retry and
conflict classification. Selecting them inside MX-20 would be a hidden API and
on-disk compatibility decision. In accordance with the no-speculative-
scaffold rule, no Rust, test or production golden changed. A coordinated
`cargo check --lib --all-features` passed on the unchanged candidate. The next
executable gate is an exact MX-80 registration/golden amendment accepted by
MX-70; all other scope and non-regression boundaries in this packet remain.
