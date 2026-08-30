# N5-A First Activation Correction Packet

Status: `MX95_N5_006_FINAL_NAMESPACE_CORRECTION_READY_FOR_REVIEW`

Owner: `MX-20`

Session binding: `019fca57-b841-79a2-88e5-e6b78a52e550`, observed from the
actual 36-byte `session_id.txt`.

Canonical authority:

- `CONCEPT_EXTENSION_IMPLEMENTATION_PLAN.md`, N5
- `AGENT_PROGRESS_LOG.md`, accepted immutable-generation architecture

This packet changes only the private MX-20 format/recovery scaffold and
disposable fixtures. It does not wire `MemoryX::new`, production atom writes,
N5-B, or any real-base migration. It does not close N5.

## Current Review Findings Addressed

The correction retains the accepted bounded fixes for `MX95-N5-007..010` and
closes the implementation-side gap returned as `MX95-N5-006` /
`MX10-N5A-009`. The canonical `commit.bin` remains the sole logical visibility
record. The complete prepare/component/commit tree stays below its durable
pending identity, and one directory rename is only the physical namespace
carrier that atomically exposes that already-bound commit record under its
numeric generation. The carrier is not a second visibility authority and does
not change N5-B or the roadmap.

On Windows, final existing-object rename and disposition retain the verified
source handle through `SetFileInformationByHandle`; the destination parent and
every ancestor remain pinned. Recursive cleanup first moves the exact verified
directory through that handle into an identity-encoded private tombstone, then
removes only the bounded validated tombstone through handles. On Unix, existing-
object rename/removal is explicitly `Unsupported` before mutation because
`renameat` and `unlinkat` are namespace-bound and cannot prove the required
final-object identity. Handle-relative creation remains supported. Recognized
production families still fail closed until N5-B supplies semantic adapters.

The obsolete disabled `prior_checkpoint_tests` block was removed. Required
coverage exists only in the active compiled `store::operation_txn::tests`
module.

## Versioned Durable-Component Registry

Registry ID: `memoryx.n5.disposable-registry.v1`.

The only N5-A durable fixture projection is:

| Kind | Canonical path | Typed invariant |
| --- | --- | --- |
| `fixture_state` | `n5-fixture/state.v1` | exact canonical `counter=<u64>\n` |
| `fixture_history` | `n5-fixture/history.v1` | unique lowercase canonical UUID per line |

Both files are required. A committed transition must stage both, increment the
counter by exactly one, preserve the old history as an exact prefix, and append
its `transaction_id` exactly once.

Excluded runtime/control artifacts are `operation_txn/`,
`.memoryx.control.json`, `.memoryx.control.json.tmp`, `.memoryx.writer.lock`,
and `.memoryx.n5-activation.lock`. Their bytes never enter the logical digest.
An excluded name with the wrong type fails closed. Unclassified files,
directories, links and reparse points fail closed. The recognized production
families `cas`, `index`, `graph`, `meta`, `crdt`, and `federation` return
`Unsupported` because N5-A has no semantic adapter for them.

## Strict Private V1 Wire Contract

Codec ID: `memoryx.n5.private-canonical-json.v1`.

All records are compact serde-JSON structs in declared field order, with no
trailing newline or whitespace. Decoding requires a byte-for-byte canonical
re-encoding match and `deny_unknown_fields`; unknown, reordered through an
unsupported representation, noncanonical, malformed, coexisting or future
artifacts fail closed. CRC32 is domain-separated by codec ID and record label.
CRC32 detects accidental damage; it is not an authenticity mechanism.

- `format.v1`: `magic`, `version`, `codec`, `component_registry`, `limits`,
  `crc32`.
- Baseline body: `magic`, `version`, `codec`, `component_registry`,
  `source_generation`, ordered `components`, `logical_state_digest`,
  `downgrade_guard`; outer `crc32`.
- Component descriptor: typed `kind`, canonical `relative_path`, checked
  `length`, lowercase BLAKE3 `blake3_hash`.
- Prepare body: `magic`, `version`, `codec`, `generation`,
  `parent_commit_hash`, canonical `transaction_id`, `operation`,
  `intent_hash`; outer `crc32`.
- Commit body: prepare fields plus `prepare_hash`, ordered paired components,
  and `logical_snapshot_hash`; outer `crc32`.
- Migration report: source/target format IDs, registry and limits IDs, checked
  counts and byte totals, copy-space preflight, baseline digest, explicit
  rollback policy, and `source_files_untouched=true`.

Checked-in exact vectors are under `docs/crash-recovery/golden/v1/`. The active
test reconstructs the records, CRC inputs and BLAKE3 commitments and requires
exact byte equality.

## Durable Layout And Supported Recovery Forms

Published generation-zero layout:

```text
operation_txn/
  baseline.v1/
    manifest.bin
    components/n5-fixture/{state.v1,history.v1}
  migration.v1
  generations/
  format.v1
```

`format.v1` is published last. A formatted top-level tree must equal these four
entries exactly. `commit.bin` is the sole logical visibility record. It becomes
reachable as a committed generation when the fully staged pending directory is
renamed to its canonical 20-digit name; that directory operation publishes the
physical carrier and does not redefine the record-level visibility contract.
The exact tree consists of `components/`, `prepare.bin` and `commit.bin`.
Deterministic pending forms may contain only known prepare/commit temporary or
final names and registered component paths; recovery retains strict support for
the prior private v1 numeric prepare-only form. Extra files,
directories, links, junctions, reparse points, gaps, noncanonical names, later
commits after an incomplete generation, and future artifacts fail closed.

Before format publication, only bounded known activation forms are accepted:
`baseline.tmp`, published `baseline.v1`, `migration.tmp`, published
`migration.v1`, an empty `generations/`, and `format.tmp`. The temporary
baseline tree may contain only its known manifest and a subset of the two
registered component paths. Retry removes only a validated owned temporary
tree. Unknown content is retained and rejected, not recursively treated as
durable state. Temporary/final baseline peers may not coexist and
temporary/final migration peers may not coexist. Immediately before
`format.v1`, the tree must equal canonical `baseline.v1`, canonical
`migration.v1`, empty `generations/` and either no format temporary or one
canonical owned `format.tmp`.

Recovery always verifies the immutable baseline tree, component hashes,
manifest CRC and logical digest. It then validates the complete generation
chain. Live-vs-baseline equality is required only when generation zero is
visible. A later visible generation requires exact equality with the baseline
overlaid by every validated committed typed transition. Prepare-only and direct
live mutations fail closed.

## Exclusive Activation Lease And Path Identity

Baseline activation requires `ExclusiveBaselineLease`; a caller cannot invoke
activation with only a path. Acquisition requires an existing non-reparse root
and absence of `.memoryx.writer.lock`. The owned lease record is strict
canonical v1 and binds the canonical root plus owner PID.

Every source component is opened no-follow. Containment is checked against the
canonical root, and the opened handle is compared with a second no-follow path
handle. Unix uses device/inode identity. Windows uses
`GetFileInformationByHandle` volume serial plus file index and rejects reparse
points. Length, identity and modification metadata are checked before and after
streaming and again before copy publication.

Immutable format, migration, baseline, prepare, commit and activation records,
including temporary forms, must expose exactly one filesystem link and fit the
1 MiB bound before allocation or parse. Create is relative to a pinned parent
(`openat`/`mkdirat` on Unix). Windows existing-object mutation opens the source
with `DELETE` access while denying delete sharing, performs the rename or
disposition through that same handle, keeps the pinned destination parent live,
and verifies the published identity. A final hard link detected after rename
causes handle-bound rollback and removal of only the module-owned temporary
link. Directory cleanup uses identity-bound tombstones and rejects unknown,
linked, over-budget or substituted entries.

Unix creation remains handle-relative, but private v1 existing-object rename
and removal return `Unsupported` before the internal mutation seam or syscall.
The implementation does not describe `renameat` or `unlinkat` as object-bound.
This is portable fail-closed compilation, not functional Unix N5-A activation.

An abort may leave the lease record. Retry reclaims it only when the record is
canonical, names the same canonical root, its PID is proved dead, and the exact
file identity is unchanged. Live, malformed, ambiguous or PID-reused ownership
fails closed. Returned failpoint errors remove only the lock created by that
acquisition.

## Resource Bounds And Measurable Cost

Limits ID: `memoryx.n5.bounds.v1`.

| Resource | Bound |
| --- | --- |
| stream buffer | 64 KiB |
| component bytes | 64 MiB |
| typed fixture file | 64 KiB |
| checked aggregate bytes | 128 MiB |
| durable components | 32; registry v1 requires exactly 2 |
| canonical path | 240 bytes, ASCII |
| record/allocation | 1 MiB |
| generations | 4096 |
| entries per directory | 8192 |
| cumulative exact-tree entries | 28,688 |
| cumulative exact-tree path bytes | 2,103,296 |
| cumulative depth | 8 |
| copy-space reserve | aggregate source bytes plus 1 MiB |

There is no `read_to_end` or unbounded `read_dir` in the owned implementation.
Record allocation is limited before allocation and while streaming. Byte sums,
counts, generation increments, path lengths and copy-space arithmetic are
checked.

Activation performs one bounded root-registry traversal, at most two bounded
semantic/hash reads of each source component, one streaming copy read of each
source component, and one hash-validation read of each published baseline
copy. It never rescans the raw root tree. `begin` performs one recovery scan and
reuses its transaction ledger and typed live projection; it does not rescan
generations or live state. `commit` reads its bounded prepare record and the two
bounded staged fixture components, performs one strict pending-tree validation
and one exact mapped future-control-tree traversal, and does not call recovery.
Recovery is `O(G*C)` for at most 4096 generations and two active v1 components,
plus one bounded control/root traversal; production-scale costs remain an N5-B
design gate.

## Transaction Identity And Retry

`transaction_id` is a strict lowercase canonical UUID stored in both prepare
and commit. Recovery rejects duplicate IDs, conflicting prepare/commit fields,
and conflicting reuse. Retry of a prepared matching ID removes only its
validated incomplete generation and reuses the same next generation. Retry of
a committed matching ID performs no write and returns that transaction's exact
committed generation/hash, even when later generations exist. The fixture
history appends the ID exactly once.

## Logical Digests And Crash Oracle

Logical digest ID: `memoryx.logical-state-digest.v1`.

The BLAKE3 input is the domain ID plus component count and, in sorted registry
order, typed kind, canonical path, checked length and lowercase content hash,
all with explicit length framing. `logical_snapshot_hash` separately commits
the parent hash, prepare hash and descriptor digest; it is not semantic N5
acceptance.

For an operation with pre-state `S0`, no-fault post-state `S1`, and stable ID:

1. A success acknowledgement permits only `S1`; an injected error or process
   death may recover to `S0` or `S1` according to durable commit publication.
   If a returned publication error occurs after the typed live fixture already
   equals `S1`, the idempotent physical-carrier finalizer detaches injection and
   completes only the already-bound commit, so reopen yields exact `S1`.
2. First reopen must validate all records, trees and typed projections and equal
   exactly `S0` or `S1`; hybrid state or open failure is rejection.
3. Second reopen must equal the first reopen exactly.
4. Retry with the same ID must advance `S0` once or leave `S1` unchanged; the
   history ID occurs exactly once.
5. Another reopen must retain the same digest and projection.

The environment seam addresses any stable boundary as
`MEMORYX_N5_FAILPOINT=<id>#<occurrence>` with action `abort` or `error`. The
active suite enumerates all 99 IDs and repeated component occurrences in
process. It executes owned child aborts at
`n5.baseline.manifest.after_write#0` and
`n5.txn.components_directory.after_create#0`, followed by retry and stable
reopen. These two cases do not execute the complete N5-E matrix or prove
physical power-loss behavior.

## Production Entrypoint And Component Inventory

Audited direct families remain unchanged: `new`, `save`, `flush`, source and
predicate registries, entity mutations, relation/claim mutations and recovery,
`ingest`, `batch_ingest`, `update_atom`, `delete_atom`, contexts, deferred
embeddings, rebuild and repair. Audited CLI writers are `init`, `ingest`,
`import`, entity/relation creation, non-dry-run compact/rebuild and repair.
Audited MCP/live-owner writers include atom CRUD, source/predicate/entity/
relation/context mutations and relation repair.

Persistent production families remain CAS segments and sidecars, location and
idloc, lexical indexes, graph manifests/edges/deltas, metadata/node mappings,
embeddings, contexts with tmp/bak forms, history and JSONL registries,
relation-repair journal/backups, operation transaction control, ownership
control, and separate CRDT WAL/snapshots. None is admitted by the N5-A fixture
registry.

## Downgrade And Migration Boundary

The private current-writer gate refuses any complete or partial activation
artifact with `memoryx.transactional-writer-required.v1`. Unsupported newer or
coexisting formats and corrupted reports refuse without mutation. Migration
records the supported legacy fixture layout, exact byte/copy-space preflight,
untouched source files, baseline digest and the rollback policy
`memoryx.n5.rollback-before-first-commit.v1`.

Rollback metadata is descriptive only: no automatic rollback implementation is
claimed. Historical binaries do not call this new gate and cannot be claimed
to refuse an activated base. Shared production admission, historical-binary
subprocess proof, production semantic adapters, real migration, N5-B, the full
MX-95/MX-90 review and N5 completion remain blocked.

## MX95-N5-006..010 Correction Candidate Identity

Source size: `291692` bytes.

Source SHA-256:
`8282ADC93BC8B7243AA27E4606ED31160CDD7F22A444A1FF1F37CE95CF7C2D85`.

The six byte-exact vectors have no trailing whitespace:

| Record | SHA-256 |
| --- | --- |
| activation lease | `C0CC119FA319EE3861973D5B37BF2D81A7BF3131D79A0EA954A9A5802EDFACC9` |
| baseline | `A152E2D8A9F182E390AC6F0425FA6E76BE7C3722F78B37E003F6C6DDDC454741` |
| commit | `779073325B6F5AD3058B0602F4DBD4F233FBD8C38A3BD94D49A43262B76906D3` |
| format | `4ADA85AA9F7A9E008CC5460C278E46829449AED56D70C35A481474162D82CAE4` |
| migration | `466664222ADC2FC90EF41527BFD5A73AA7CDC900B677C4D0D83F65C22477DD62` |
| prepare | `2026D28181CFF59AB8B76B6460917CA1470E99AA42E7BA32D337F6D1BA050F1F` |

The physical-root writer lease is held throughout activation and each private
transaction. Root-to-parent handles, no-follow final opens and stable object
identities protect guarded operations. Windows source handles exclude delete
sharing and retain `DELETE` access until the handle-bound rename or disposition
finishes. Production N5-B still needs a typed lease handoff from the existing
owner and is not implemented here.

Incomplete generation identity is persisted first in the canonical pending
directory name. Migration preflight is persisted before baseline staging and
reused byte for byte. Directory creation and synchronization are ordered from
new child through modified parent. Exact directory sets and cumulative tree
budgets fail closed. Generation admission rejects `max+1` before staging.

The final focused result is 41 passed, 0 failed and 695 filtered. The BaseLease
focused result is 5 top-level passed plus 1 owned child observation passed,
with 0 failures. Library
all-feature check, warnings-denied clippy and format check passed under released
shared-host coordination. These are bounded implementation facts, not N5 or
production semantic acceptance.

## MX10-N5A-012..018 Correction Checkpoint

Exact corrected source: 371,398 bytes, SHA-256
`21E2447C783C16958C8507BFEFCEE8793CEEDC09E76C72276DF32EDAE0FBE556`.

Windows generation publication is now an explicit two-part protocol:

1. pin the exact source and destination parent, then invoke native
   `FileRenameInformation` with the parent as `RootDirectory` and one relative
   carrier component;
2. reopen/pin the exact immutable child tree, run the final seam and complete
   admission, then use extended-length `MoveFileExW(MOVEFILE_WRITE_THROUGH)` to
   place the same carrier under its numeric generation name.

The protocol preserves `commit.bin` as the sole logical visibility record.
Identity tombstones are monotonic and resume after every child removal.
Unsupported Unix mutation refuses before any private lease or artifact.
`symlink_metadata` `NotFound` is the only absence proof. Windows liveness binds
exit state and process creation identity.

Final results: 50 operation tests, 5 BaseLease tests plus one owned child
observation, 8 new ratchets, fmt, all-feature library check and clippy
`-D warnings` passed. The warnings-denied Linux library check passed and Unix
test code compiled. Functional Unix execution, physical power loss, full fault
coverage and production semantics remain unproved.
