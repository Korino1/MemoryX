//! Immutable operation-transaction generations.
//!
//! This module is the N5-A durable format and recovery scanner.  It deliberately
//! does not make existing store writes transactional yet: N5-B will stage CAS,
//! indexes, graph state, metadata, and history through this coordinator.  Until
//! then, the only published data owned by this module is its own generation log.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::base_lease::{
    BaseLease, BaseLeaseError, BorrowedOwnerQuiescence, PhysicalRootIdentity, QuiescentWrite,
    StartupAdmission,
};
use crate::cas::canonical::compute_atom_id_from_payload;
use crate::cas::io::{BloomFilter, IndexEntry, IndexFileHeader};
use crate::cas::{AtomBodyHeader, RecordHeader};
use crate::graph::{DeltaHeader, EdgeListEntry, GraphManifest};
use crate::index::{IdLocBuilder, LexHeader, PostHeader};
use crate::store::{AtomId, AtomType, EdgeType};
use crate::utils::crc32;

const FORMAT_MAGIC: &str = "MEMORYX_OPERATION_TXN";
const FORMAT_VERSION: u32 = 1;
const TXN_DIR_NAME: &str = "operation_txn";
const FORMAT_FILE_NAME: &str = "format.v1";
const FORMAT_TEMP_FILE_NAME: &str = "format.tmp";
const MIGRATION_FILE_NAME: &str = "migration.v1";
const MIGRATION_TEMP_FILE_NAME: &str = "migration.tmp";
const GENERATIONS_DIR_NAME: &str = "generations";
const PUBLICATION_DIRECTORY_NAME: &str = ".publication";
const BASELINE_DIR_NAME: &str = "baseline.v1";
const BASELINE_TEMP_DIR_NAME: &str = "baseline.tmp";
const BASELINE_MANIFEST_FILE_NAME: &str = "manifest.bin";
const PREPARE_FILE_NAME: &str = "prepare.bin";
const PREPARE_TEMP_FILE_NAME: &str = "prepare.tmp";
const COMMIT_FILE_NAME: &str = "commit.bin";
const COMMIT_TEMP_FILE_NAME: &str = "commit.tmp";
const COMPONENTS_DIR_NAME: &str = "components";
const PENDING_GENERATION_PREFIX: &str = "pending-";
const PUBLICATION_GENERATION_PREFIX: &str = ".publication-generation-";
const CLEANUP_GENERATION_PREFIX: &str = ".cleanup-generation-";
const CLEANUP_BASELINE_PREFIX: &str = ".cleanup-baseline-";
const PRIVATE_CODEC_ID: &str = "memoryx.n5.private-canonical-json.v1";
const COMPONENT_REGISTRY_ID: &str = "memoryx.n5.disposable-registry.v1";
const LIMITS_ID: &str = "memoryx.n5.bounds.v1";
const SOURCE_LAYOUT_ID: &str = "memoryx.n5-disposable-fixture.v1";
const ROLLBACK_POLICY_ID: &str = "memoryx.n5.rollback-before-first-commit.v1";
const FIXTURE_STATE_PATH: &str = "n5-fixture/state.v1";
const FIXTURE_HISTORY_PATH: &str = "n5-fixture/history.v1";
const CONTROL_FILE_NAME: &str = ".memoryx.control.json";
const CONTROL_TEMP_FILE_NAME: &str = ".memoryx.control.json.tmp";
const ACTIVATION_LOCK_FILE_NAME: &str = ".memoryx.n5-activation.lock";
const ACTIVATION_LEASE_SCHEMA: &str = "memoryx.n5.activation-lease.v1";
const STREAM_BUFFER_BYTES: usize = 64 * 1024;
const MAX_COMPONENT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_AGGREGATE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_COMPONENT_COUNT: usize = 32;
const MAX_PATH_BYTES: usize = 240;
const MAX_CONTROL_RECORD_BYTES: u64 = 1024 * 1024;
const MAX_GENERATIONS: usize = 4096;
const MAX_DIRECTORY_ENTRIES: usize = 8192;
const MAX_TREE_DEPTH: usize = 8;
// The v1 disposable registry has exactly seven durable entries per committed
// generation. A 20-digit generation contributes fewer than 512 cumulative
// relative-path bytes. The extra pending allowance covers the longer identity-
// first directory name while generation 4096 is being staged. These constants
// are intentionally derived from the supported registry rather than from an
// unrelated small-tree test fixture.
const MAX_GENERATION_TREE_ENTRIES: usize = 7;
const MAX_GENERATION_TREE_PATH_BYTES: usize = 512;
const MAX_FIXED_TREE_ENTRIES: usize = 16;
const MAX_FIXED_TREE_PATH_BYTES: usize = 4096;
const MAX_PENDING_TREE_PATH_BYTES: usize = 2048;
const MAX_TREE_ENTRIES: usize =
    MAX_FIXED_TREE_ENTRIES + MAX_GENERATIONS * MAX_GENERATION_TREE_ENTRIES;
const MAX_TREE_PATH_BYTES: usize = MAX_FIXED_TREE_PATH_BYTES
    + MAX_GENERATIONS * MAX_GENERATION_TREE_PATH_BYTES
    + MAX_PENDING_TREE_PATH_BYTES;
const COPY_SPACE_OVERHEAD_BYTES: u64 = 1024 * 1024;
const FIXTURE_COMPONENT_MAX_BYTES: u64 = 64 * 1024;
const BASELINE_MAGIC: &str = "MEMORYX_LEGACY_BASELINE";
const CANONICAL_LOGICAL_STATE_DIGEST_ID: &str = "memoryx.logical-state-digest.v1";
const DOWNGRADE_GUARD_ID: &str = "memoryx.transactional-writer-required.v1";
const WRITER_LOCK_FILE_NAME: &str = ".memoryx.writer.lock";

#[derive(Debug, Clone, Copy)]
struct ResourceLimits {
    max_component_bytes: u64,
    max_aggregate_bytes: u64,
    max_component_count: usize,
    max_path_bytes: usize,
    max_record_bytes: u64,
    max_generations: usize,
    max_directory_entries: usize,
    max_tree_entries: usize,
    max_tree_depth: usize,
    max_tree_path_bytes: usize,
}

const DEFAULT_LIMITS: ResourceLimits = ResourceLimits {
    max_component_bytes: MAX_COMPONENT_BYTES,
    max_aggregate_bytes: MAX_AGGREGATE_BYTES,
    max_component_count: MAX_COMPONENT_COUNT,
    max_path_bytes: MAX_PATH_BYTES,
    max_record_bytes: MAX_CONTROL_RECORD_BYTES,
    max_generations: MAX_GENERATIONS,
    max_directory_entries: MAX_DIRECTORY_ENTRIES,
    max_tree_entries: MAX_TREE_ENTRIES,
    max_tree_depth: MAX_TREE_DEPTH,
    max_tree_path_bytes: MAX_TREE_PATH_BYTES,
};

#[derive(Debug, Default)]
struct TreeBudget {
    entries: usize,
    path_bytes: usize,
}

impl TreeBudget {
    fn observe(&mut self, relative: &str, limits: &ResourceLimits) -> io::Result<()> {
        let depth = relative.split('/').count();
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or_else(|| invalid_data("tree entry count overflow"))?;
        self.path_bytes = self
            .path_bytes
            .checked_add(relative.len())
            .ok_or_else(|| invalid_data("tree path-byte total overflow"))?;
        if self.entries > limits.max_tree_entries
            || depth > limits.max_tree_depth
            || self.path_bytes > limits.max_tree_path_bytes
        {
            return Err(invalid_data(
                "tree exceeds the cumulative entry, depth, or path-byte bounds",
            ));
        }
        Ok(())
    }
}

/// A logical write operation that will later be mapped to staged store deltas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperationKind {
    Ingest,
    BatchIngest,
    UpdateAtom,
    DeleteAtom,
    Authoring,
    Registry,
    Context,
    Maintenance,
}

impl OperationKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ingest => "ingest",
            Self::BatchIngest => "batch_ingest",
            Self::UpdateAtom => "update_atom",
            Self::DeleteAtom => "delete_atom",
            Self::Authoring => "authoring",
            Self::Registry => "registry",
            Self::Context => "context",
            Self::Maintenance => "maintenance",
        }
    }

    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "ingest" => Ok(Self::Ingest),
            "batch_ingest" => Ok(Self::BatchIngest),
            "update_atom" => Ok(Self::UpdateAtom),
            "delete_atom" => Ok(Self::DeleteAtom),
            "authoring" => Ok(Self::Authoring),
            "registry" => Ok(Self::Registry),
            "context" => Ok(Self::Context),
            "maintenance" => Ok(Self::Maintenance),
            _ => Err(invalid_data(
                "pending generation operation is not canonical",
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct TransactionId(String);

impl TransactionId {
    pub(crate) fn parse(value: &str) -> io::Result<Self> {
        let bytes = value.as_bytes();
        if bytes.len() != 36
            || !bytes.iter().enumerate().all(|(index, byte)| match index {
                8 | 13 | 18 | 23 => *byte == b'-',
                _ => byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase(),
            })
        {
            return Err(invalid_data(
                "transaction_id must be a canonical lowercase UUID",
            ));
        }
        Ok(Self(value.to_owned()))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DurableComponentKind {
    FixtureState,
    FixtureHistory,
}

impl DurableComponentKind {
    const fn path(self) -> &'static str {
        match self {
            Self::FixtureState => FIXTURE_STATE_PATH,
            Self::FixtureHistory => FIXTURE_HISTORY_PATH,
        }
    }

    const fn tag(self) -> &'static str {
        match self {
            Self::FixtureState => "fixture_state",
            Self::FixtureHistory => "fixture_history",
        }
    }
}

/// Stable failpoint positions for subprocess crash injection in N5-E.
///
/// The coordinator calls each stage in this exact order. Repeated component
/// stages are selected by their zero-based occurrence. Once the commit rename
/// has run, the response alone cannot classify the result; reopen and the
/// pre-state/post-state oracle decide whether the commit is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum OperationStage {
    LeaseBeforeWrite,
    LeaseAfterWrite,
    LeaseAfterFlush,
    LeaseAfterSync,
    LeaseAfterParentSync,
    RecoveryBeforeControlScan,
    RecoveryAfterControlScan,
    RecoveryAfterBaselineValidation,
    RecoveryBeforeGeneration,
    RecoveryAfterGeneration,
    RecoveryBeforeLiveValidation,
    RecoveryAfterLiveValidation,
    RecoveryComplete,
    BaselineBeforeScan,
    BaselineAfterScan,
    BaselineBeforeControlCreate,
    BaselineAfterControlCreate,
    BaselineAfterControlSync,
    BaselineAfterRootSync,
    BaselineBeforeStagingCreate,
    BaselineAfterStagingCreate,
    BaselineBeforeComponentsCreate,
    BaselineAfterComponentsCreate,
    BaselineAfterComponentsSync,
    BaselineAfterStagingSync,
    BaselineBeforeComponentParentCreate,
    BaselineAfterComponentParentCreate,
    BaselineAfterComponentParentSync,
    BaselineAfterComponentsParentSync,
    BaselineBeforeComponentOpen,
    BaselineBeforeComponentWrite,
    BaselineAfterComponentWrite,
    BaselineAfterComponentFlush,
    BaselineAfterComponentFileSync,
    BaselineAfterComponentSync,
    BaselineBeforeManifestWrite,
    BaselineAfterManifestWrite,
    BaselineAfterManifestFlush,
    BaselineAfterManifestSync,
    BaselineAfterManifestDirectorySync,
    BaselineBeforePublish,
    BaselineAfterRename,
    BaselineAfterParentSync,
    BeforeMigrationWrite,
    AfterMigrationWrite,
    AfterMigrationFlush,
    AfterMigrationSync,
    MigrationPreflightAfterParentSync,
    BeforeMigrationPublish,
    AfterMigrationRename,
    AfterMigrationParentSync,
    BeforeGenerationsCreate,
    AfterGenerationsCreate,
    AfterGenerationsSync,
    AfterGenerationsControlSync,
    BeforeFormatWrite,
    AfterFormatWrite,
    AfterFormatFlush,
    AfterFormatSync,
    BeforeFormatPublish,
    AfterFormatRename,
    AfterFormatParentSync,
    BeforeGenerationCreate,
    AfterGenerationCreate,
    AfterGenerationSync,
    AfterGenerationsParentSync,
    BeforeGenerationComponentsCreate,
    AfterGenerationComponentsCreate,
    AfterGenerationComponentsSync,
    AfterGenerationComponentsParentSync,
    BeforePrepareWrite,
    AfterPrepareWrite,
    AfterPrepareFlush,
    AfterPrepareSync,
    BeforePreparePublish,
    AfterPrepareRename,
    AfterPrepareParentSync,
    BeforeGenerationPublish,
    AfterGenerationPublish,
    AfterGenerationPublishParentSync,
    BeforeComponentParentCreate,
    AfterComponentParentCreate,
    AfterComponentParentSync,
    AfterComponentComponentsSync,
    BeforeComponentWrite,
    AfterComponentWrite,
    AfterComponentFlush,
    AfterComponentFileSync,
    AfterComponentSync,
    BeforeCommitWrite,
    AfterCommitWrite,
    AfterCommitFlush,
    AfterCommitSync,
    BeforeCommitPublish,
    AfterCommitRename,
    AfterCommitParentSync,
    CleanupBeforeRemove,
    CleanupAfterRemove,
    CleanupAfterParentSync,
}

const ALL_OPERATION_STAGES: &[OperationStage] = &[
    OperationStage::LeaseBeforeWrite,
    OperationStage::LeaseAfterWrite,
    OperationStage::LeaseAfterFlush,
    OperationStage::LeaseAfterSync,
    OperationStage::LeaseAfterParentSync,
    OperationStage::RecoveryBeforeControlScan,
    OperationStage::RecoveryAfterControlScan,
    OperationStage::RecoveryAfterBaselineValidation,
    OperationStage::RecoveryBeforeGeneration,
    OperationStage::RecoveryAfterGeneration,
    OperationStage::RecoveryBeforeLiveValidation,
    OperationStage::RecoveryAfterLiveValidation,
    OperationStage::RecoveryComplete,
    OperationStage::BaselineBeforeScan,
    OperationStage::BaselineAfterScan,
    OperationStage::BaselineBeforeControlCreate,
    OperationStage::BaselineAfterControlCreate,
    OperationStage::BaselineAfterControlSync,
    OperationStage::BaselineAfterRootSync,
    OperationStage::BaselineBeforeStagingCreate,
    OperationStage::BaselineAfterStagingCreate,
    OperationStage::BaselineBeforeComponentsCreate,
    OperationStage::BaselineAfterComponentsCreate,
    OperationStage::BaselineAfterComponentsSync,
    OperationStage::BaselineAfterStagingSync,
    OperationStage::BaselineBeforeComponentParentCreate,
    OperationStage::BaselineAfterComponentParentCreate,
    OperationStage::BaselineAfterComponentParentSync,
    OperationStage::BaselineAfterComponentsParentSync,
    OperationStage::BaselineBeforeComponentOpen,
    OperationStage::BaselineBeforeComponentWrite,
    OperationStage::BaselineAfterComponentWrite,
    OperationStage::BaselineAfterComponentFlush,
    OperationStage::BaselineAfterComponentFileSync,
    OperationStage::BaselineAfterComponentSync,
    OperationStage::BaselineBeforeManifestWrite,
    OperationStage::BaselineAfterManifestWrite,
    OperationStage::BaselineAfterManifestFlush,
    OperationStage::BaselineAfterManifestSync,
    OperationStage::BaselineAfterManifestDirectorySync,
    OperationStage::BaselineBeforePublish,
    OperationStage::BaselineAfterRename,
    OperationStage::BaselineAfterParentSync,
    OperationStage::BeforeMigrationWrite,
    OperationStage::AfterMigrationWrite,
    OperationStage::AfterMigrationFlush,
    OperationStage::AfterMigrationSync,
    OperationStage::MigrationPreflightAfterParentSync,
    OperationStage::BeforeMigrationPublish,
    OperationStage::AfterMigrationRename,
    OperationStage::AfterMigrationParentSync,
    OperationStage::BeforeGenerationsCreate,
    OperationStage::AfterGenerationsCreate,
    OperationStage::AfterGenerationsSync,
    OperationStage::AfterGenerationsControlSync,
    OperationStage::BeforeFormatWrite,
    OperationStage::AfterFormatWrite,
    OperationStage::AfterFormatFlush,
    OperationStage::AfterFormatSync,
    OperationStage::BeforeFormatPublish,
    OperationStage::AfterFormatRename,
    OperationStage::AfterFormatParentSync,
    OperationStage::BeforeGenerationCreate,
    OperationStage::AfterGenerationCreate,
    OperationStage::AfterGenerationSync,
    OperationStage::AfterGenerationsParentSync,
    OperationStage::BeforeGenerationComponentsCreate,
    OperationStage::AfterGenerationComponentsCreate,
    OperationStage::AfterGenerationComponentsSync,
    OperationStage::AfterGenerationComponentsParentSync,
    OperationStage::BeforePrepareWrite,
    OperationStage::AfterPrepareWrite,
    OperationStage::AfterPrepareFlush,
    OperationStage::AfterPrepareSync,
    OperationStage::BeforePreparePublish,
    OperationStage::AfterPrepareRename,
    OperationStage::AfterPrepareParentSync,
    OperationStage::BeforeGenerationPublish,
    OperationStage::AfterGenerationPublish,
    OperationStage::AfterGenerationPublishParentSync,
    OperationStage::BeforeComponentParentCreate,
    OperationStage::AfterComponentParentCreate,
    OperationStage::AfterComponentParentSync,
    OperationStage::AfterComponentComponentsSync,
    OperationStage::BeforeComponentWrite,
    OperationStage::AfterComponentWrite,
    OperationStage::AfterComponentFlush,
    OperationStage::AfterComponentFileSync,
    OperationStage::AfterComponentSync,
    OperationStage::BeforeCommitWrite,
    OperationStage::AfterCommitWrite,
    OperationStage::AfterCommitFlush,
    OperationStage::AfterCommitSync,
    OperationStage::BeforeCommitPublish,
    OperationStage::AfterCommitRename,
    OperationStage::AfterCommitParentSync,
    OperationStage::CleanupBeforeRemove,
    OperationStage::CleanupAfterRemove,
    OperationStage::CleanupAfterParentSync,
];

impl OperationStage {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::LeaseBeforeWrite => "n5.lease.before_write",
            Self::LeaseAfterWrite => "n5.lease.after_write",
            Self::LeaseAfterFlush => "n5.lease.after_flush",
            Self::LeaseAfterSync => "n5.lease.after_sync",
            Self::LeaseAfterParentSync => "n5.lease.after_parent_sync",
            Self::RecoveryBeforeControlScan => "n5.recovery.control.before_scan",
            Self::RecoveryAfterControlScan => "n5.recovery.control.after_scan",
            Self::RecoveryAfterBaselineValidation => "n5.recovery.baseline.after_validate",
            Self::RecoveryBeforeGeneration => "n5.recovery.generation.before_validate",
            Self::RecoveryAfterGeneration => "n5.recovery.generation.after_validate",
            Self::RecoveryBeforeLiveValidation => "n5.recovery.live.before_validate",
            Self::RecoveryAfterLiveValidation => "n5.recovery.live.after_validate",
            Self::RecoveryComplete => "n5.recovery.complete",
            Self::BaselineBeforeScan => "n5.baseline.before_scan",
            Self::BaselineAfterScan => "n5.baseline.after_scan",
            Self::BaselineBeforeControlCreate => "n5.baseline.control.before_create",
            Self::BaselineAfterControlCreate => "n5.baseline.control.after_create",
            Self::BaselineAfterControlSync => "n5.baseline.control.after_sync",
            Self::BaselineAfterRootSync => "n5.baseline.root.after_sync",
            Self::BaselineBeforeStagingCreate => "n5.baseline.staging.before_create",
            Self::BaselineAfterStagingCreate => "n5.baseline.staging.after_create",
            Self::BaselineBeforeComponentsCreate => "n5.baseline.components.before_create",
            Self::BaselineAfterComponentsCreate => "n5.baseline.components.after_create",
            Self::BaselineAfterComponentsSync => "n5.baseline.components.after_sync",
            Self::BaselineAfterStagingSync => "n5.baseline.staging.after_components_sync",
            Self::BaselineBeforeComponentParentCreate => {
                "n5.baseline.component_parent.before_create"
            }
            Self::BaselineAfterComponentParentCreate => "n5.baseline.component_parent.after_create",
            Self::BaselineAfterComponentParentSync => "n5.baseline.component_parent.after_sync",
            Self::BaselineAfterComponentsParentSync => {
                "n5.baseline.components.after_component_parent_sync"
            }
            Self::BaselineBeforeComponentOpen => "n5.baseline.component.before_open",
            Self::BaselineBeforeComponentWrite => "n5.baseline.component.before_write",
            Self::BaselineAfterComponentWrite => "n5.baseline.component.after_write",
            Self::BaselineAfterComponentFlush => "n5.baseline.component.after_flush",
            Self::BaselineAfterComponentFileSync => "n5.baseline.component.after_file_sync",
            Self::BaselineAfterComponentSync => "n5.baseline.component.after_sync",
            Self::BaselineBeforeManifestWrite => "n5.baseline.manifest.before_write",
            Self::BaselineAfterManifestWrite => "n5.baseline.manifest.after_write",
            Self::BaselineAfterManifestFlush => "n5.baseline.manifest.after_flush",
            Self::BaselineAfterManifestSync => "n5.baseline.manifest.after_sync",
            Self::BaselineAfterManifestDirectorySync => "n5.baseline.manifest.after_directory_sync",
            Self::BaselineBeforePublish => "n5.baseline.before_publish",
            Self::BaselineAfterRename => "n5.baseline.after_rename",
            Self::BaselineAfterParentSync => "n5.baseline.after_parent_sync",
            Self::BeforeMigrationWrite => "n5.migration.report.before_write",
            Self::AfterMigrationWrite => "n5.migration.report.after_write",
            Self::AfterMigrationFlush => "n5.migration.report.after_flush",
            Self::AfterMigrationSync => "n5.migration.report.after_sync",
            Self::MigrationPreflightAfterParentSync => "n5.migration.preflight.after_parent_sync",
            Self::BeforeMigrationPublish => "n5.migration.report.before_publish",
            Self::AfterMigrationRename => "n5.migration.report.after_rename",
            Self::AfterMigrationParentSync => "n5.migration.report.after_parent_sync",
            Self::BeforeGenerationsCreate => "n5.generations.before_create",
            Self::AfterGenerationsCreate => "n5.generations.after_create",
            Self::AfterGenerationsSync => "n5.generations.after_sync",
            Self::AfterGenerationsControlSync => "n5.generations.control.after_sync",
            Self::BeforeFormatWrite => "n5.format.before_write",
            Self::AfterFormatWrite => "n5.format.after_write",
            Self::AfterFormatFlush => "n5.format.after_flush",
            Self::AfterFormatSync => "n5.format.after_sync",
            Self::BeforeFormatPublish => "n5.format.before_publish",
            Self::AfterFormatRename => "n5.format.after_rename",
            Self::AfterFormatParentSync => "n5.format.after_parent_sync",
            Self::BeforeGenerationCreate => "n5.txn.generation.before_create",
            Self::AfterGenerationCreate => "n5.txn.generation.after_create",
            Self::AfterGenerationSync => "n5.txn.generation.after_sync",
            Self::AfterGenerationsParentSync => "n5.txn.generations.after_parent_sync",
            Self::BeforeGenerationComponentsCreate => "n5.txn.components_directory.before_create",
            Self::AfterGenerationComponentsCreate => "n5.txn.components_directory.after_create",
            Self::AfterGenerationComponentsSync => "n5.txn.components_directory.after_sync",
            Self::AfterGenerationComponentsParentSync => "n5.txn.generation.after_components_sync",
            Self::BeforePrepareWrite => "n5.txn.prepare.before_write",
            Self::AfterPrepareWrite => "n5.txn.prepare.after_write",
            Self::AfterPrepareFlush => "n5.txn.prepare.after_flush",
            Self::AfterPrepareSync => "n5.txn.prepare.after_sync",
            Self::BeforePreparePublish => "n5.txn.prepare.before_publish",
            Self::AfterPrepareRename => "n5.txn.prepare.after_rename",
            Self::AfterPrepareParentSync => "n5.txn.prepare.after_parent_sync",
            Self::BeforeGenerationPublish => "n5.txn.generation.before_publish",
            Self::AfterGenerationPublish => "n5.txn.generation.after_publish",
            Self::AfterGenerationPublishParentSync => "n5.txn.generation.publish.after_parent_sync",
            Self::BeforeComponentParentCreate => "n5.txn.component_parent.before_create",
            Self::AfterComponentParentCreate => "n5.txn.component_parent.after_create",
            Self::AfterComponentParentSync => "n5.txn.component_parent.after_sync",
            Self::AfterComponentComponentsSync => "n5.txn.components.after_component_parent_sync",
            Self::BeforeComponentWrite => "n5.txn.component.before_write",
            Self::AfterComponentWrite => "n5.txn.component.after_write",
            Self::AfterComponentFlush => "n5.txn.component.after_flush",
            Self::AfterComponentFileSync => "n5.txn.component.after_file_sync",
            Self::AfterComponentSync => "n5.txn.component.after_sync",
            Self::BeforeCommitWrite => "n5.txn.commit.before_write",
            Self::AfterCommitWrite => "n5.txn.commit.after_write",
            Self::AfterCommitFlush => "n5.txn.commit.after_flush",
            Self::AfterCommitSync => "n5.txn.commit.after_sync",
            Self::BeforeCommitPublish => "n5.txn.commit.before_publish",
            Self::AfterCommitRename => "n5.txn.commit.after_rename",
            Self::AfterCommitParentSync => "n5.txn.commit.after_parent_sync",
            Self::CleanupBeforeRemove => "n5.cleanup.incomplete.before_remove",
            Self::CleanupAfterRemove => "n5.cleanup.incomplete.after_remove",
            Self::CleanupAfterParentSync => "n5.cleanup.incomplete.after_parent_sync",
        }
    }
}

/// Test and fault-injection hook. Production code normally uses no hook.
pub(crate) trait OperationFailpoint: Send {
    fn hit(&mut self, stage: OperationStage, occurrence: usize) -> io::Result<()>;

    /// Internal namespace-race seam. These positions are deliberately not
    /// durable failpoint IDs: the accepted 99-ID crash inventory remains
    /// unchanged, while tests can act after the final object/link check and
    /// immediately before the handle-bound namespace syscall.
    fn before_namespace_mutation(
        &mut self,
        _kind: NamespaceMutationKind,
        _source: &Path,
        _target: Option<&Path>,
    ) -> io::Result<()> {
        Ok(())
    }

    /// Internal post-mutation seam used to prove monotonic cleanup recovery.
    /// It is deliberately outside the durable 99-ID inventory.
    fn after_namespace_mutation(
        &mut self,
        _kind: NamespaceMutationKind,
        _path: &Path,
    ) -> io::Result<()> {
        Ok(())
    }

    /// Internal seam after a mutation handle closes but before pathname
    /// absence is accepted. Tests use it to inject a dangling link or
    /// replacement in the only interval where `NotFound` is authoritative.
    fn after_namespace_handle_close(
        &mut self,
        _kind: NamespaceMutationKind,
        _path: &Path,
    ) -> io::Result<()> {
        Ok(())
    }

    /// Internal seam immediately before the Windows write-through visibility
    /// move. It is not a durable failpoint ID.
    fn before_write_through_visibility(
        &mut self,
        _source: &Path,
        _target: &Path,
    ) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NamespaceMutationKind {
    FileRename,
    DirectoryRename,
    FileRemove,
    DirectoryRemove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanupNamespaceKind {
    Generation,
    Baseline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CleanupBinding {
    Generation(PendingGenerationIdentity),
    Baseline,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FormatRecord {
    magic: String,
    version: u32,
    codec: String,
    component_registry: String,
    limits: String,
    crc32: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineBody {
    magic: String,
    version: u32,
    codec: String,
    component_registry: String,
    source_generation: u64,
    components: Vec<ComponentDescriptor>,
    logical_state_digest: String,
    downgrade_guard: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineRecord {
    body: BaselineBody,
    crc32: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrepareBody {
    magic: String,
    version: u32,
    codec: String,
    generation: u64,
    parent_commit_hash: String,
    transaction_id: TransactionId,
    operation: OperationKind,
    intent_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrepareRecord {
    body: PrepareBody,
    crc32: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComponentDescriptor {
    kind: DurableComponentKind,
    relative_path: String,
    length: u64,
    blake3_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitBody {
    magic: String,
    version: u32,
    codec: String,
    generation: u64,
    parent_commit_hash: String,
    prepare_hash: String,
    transaction_id: TransactionId,
    operation: OperationKind,
    intent_hash: String,
    components: Vec<ComponentDescriptor>,
    logical_snapshot_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitRecord {
    body: CommitBody,
    crc32: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrationBody {
    magic: String,
    version: u32,
    codec: String,
    source_layout: String,
    target_format: String,
    component_registry: String,
    limits: String,
    component_count: u64,
    total_bytes: u64,
    required_copy_bytes: u64,
    available_copy_bytes: u64,
    baseline_digest: String,
    rollback_policy: String,
    source_files_untouched: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrationRecord {
    body: MigrationBody,
    crc32: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FixtureProjection {
    counter: u64,
    history: Vec<TransactionId>,
}

#[derive(Debug, Clone)]
struct ComponentSource {
    descriptor: ComponentDescriptor,
    storage_path: PathBuf,
}

#[derive(Debug, Clone)]
struct InventoryComponent {
    descriptor: ComponentDescriptor,
    source_path: PathBuf,
    identity: StableFileIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StableFileIdentity {
    length: u64,
    modified: Option<std::time::SystemTime>,
    platform_a: u64,
    platform_b: u64,
    links: u64,
}

#[cfg(windows)]
struct PinnedGenerationTreeEntry {
    relative_path: String,
    identity: StableFileIdentity,
    is_directory: bool,
    content_hash: Option<String>,
    handle: File,
}

#[cfg(windows)]
struct PinnedGenerationTree {
    entries: Vec<PinnedGenerationTreeEntry>,
}

#[cfg(windows)]
impl PinnedGenerationTree {
    fn capture(root: &Path) -> io::Result<Self> {
        let mut entries = Vec::new();
        let mut budget = TreeBudget::default();
        let mut stack = vec![(root.to_path_buf(), String::new())];
        while let Some((directory, prefix)) = stack.pop() {
            let mut children = read_directory_bounded(&directory, &DEFAULT_LIMITS)?;
            children.sort_by_key(|entry| entry.file_name());
            for child in children {
                let name = child
                    .file_name()
                    .into_string()
                    .map_err(|_| invalid_data("pinned generation path is not canonical UTF-8"))?;
                let relative_path = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                budget.observe(&relative_path, &DEFAULT_LIMITS)?;
                let metadata = fs::symlink_metadata(child.path())?;
                if is_link_or_reparse(&child.path(), &metadata) {
                    return Err(invalid_data(
                        "pinned generation tree contains a link or reparse point",
                    ));
                }
                let is_directory = metadata.is_dir();
                let handle = if is_directory {
                    stack.push((child.path(), relative_path.clone()));
                    open_verification_handle_while_mutating(&child.path(), true)?
                } else if metadata.is_file() {
                    open_generation_child_pin(&child.path())?
                } else {
                    return Err(invalid_data(
                        "pinned generation tree contains a non-file entry",
                    ));
                };
                let identity = stable_identity(&handle)?;
                let content_hash = if is_directory {
                    None
                } else {
                    require_single_link(&identity, "generation child at final admission")?;
                    Some(hash_open_file_handle(
                        &handle,
                        DEFAULT_LIMITS.max_component_bytes,
                    )?)
                };
                entries.push(PinnedGenerationTreeEntry {
                    relative_path,
                    identity,
                    is_directory,
                    content_hash,
                    handle,
                });
            }
        }
        entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        Ok(Self { entries })
    }

    fn revalidate(&self, root: &Path) -> io::Result<()> {
        let expected = self
            .entries
            .iter()
            .map(|entry| entry.relative_path.clone())
            .collect::<BTreeSet<_>>();
        let mut actual = BTreeSet::new();
        let mut budget = TreeBudget::default();
        let mut stack = vec![(root.to_path_buf(), String::new())];
        while let Some((directory, prefix)) = stack.pop() {
            let mut children = read_directory_bounded(&directory, &DEFAULT_LIMITS)?;
            children.sort_by_key(|entry| entry.file_name());
            for child in children {
                let name = child
                    .file_name()
                    .into_string()
                    .map_err(|_| invalid_data("generation revalidation path is not UTF-8"))?;
                let relative = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                budget.observe(&relative, &DEFAULT_LIMITS)?;
                let metadata = fs::symlink_metadata(child.path())?;
                if is_link_or_reparse(&child.path(), &metadata) {
                    return Err(invalid_data(
                        "generation child became a link or reparse point at visibility",
                    ));
                }
                if metadata.is_dir() {
                    stack.push((child.path(), relative.clone()));
                } else if !metadata.is_file() {
                    return Err(invalid_data(
                        "generation child changed to an unsupported entry type",
                    ));
                }
                actual.insert(relative);
            }
        }
        if actual != expected {
            return Err(invalid_data(
                "generation child tree changed at the visibility boundary",
            ));
        }

        for expected in &self.entries {
            let path = root.join(Path::new(&expected.relative_path));
            let current = open_verification_handle_while_mutating(&path, expected.is_directory)?;
            let current_identity = stable_identity(&current)?;
            let pinned_identity = stable_identity(&expected.handle)?;
            if !same_file_object(&current_identity, &expected.identity)
                || !same_file_object(&pinned_identity, &expected.identity)
                || current_identity.links != expected.identity.links
                || pinned_identity.links != expected.identity.links
                || current_identity.length != expected.identity.length
                || pinned_identity.length != expected.identity.length
            {
                return Err(invalid_data(
                    "generation child identity or link count changed at visibility",
                ));
            }
            if let Some(expected_hash) = &expected.content_hash {
                require_single_link(&current_identity, "generation child at visibility")?;
                if hash_open_file_handle(&current, DEFAULT_LIMITS.max_component_bytes)?
                    != *expected_hash
                {
                    return Err(invalid_data(
                        "generation child bytes changed at the visibility boundary",
                    ));
                }
            }
        }
        Ok(())
    }

    fn quarantine_new_links(&self) -> io::Result<usize> {
        let mut removed = 0usize;
        for entry in &self.entries {
            if entry.is_directory {
                continue;
            }
            let current = stable_identity(&entry.handle)?;
            if !same_file_object(&current, &entry.identity) || current.links < entry.identity.links
            {
                return Err(invalid_data(
                    "generation child identity changed before late-link quarantine",
                ));
            }
            if entry.identity.links == 1 && current.links > 1 {
                mark_opened_object_for_removal(&entry.handle)?;
                removed = removed
                    .checked_add(1)
                    .ok_or_else(|| invalid_data("late-link quarantine count overflow"))?;
            }
        }
        Ok(removed)
    }
}

/// Pinned handles for every existing directory between a selected physical
/// root and one operation path. Path-based opens are accepted only while these
/// identities remain unchanged before and after the final open.
///
/// This is deliberately bounded by the v1 depth/path limits. It is not a claim
/// that arbitrary hostile filesystem mutation can be made safe on every OS;
/// any observed substitution fails closed and the private N5-A gate remains.
struct AncestorGuard {
    paths: Vec<PathBuf>,
    identities: Vec<StableFileIdentity>,
    _handles: Vec<File>,
}

struct PinnedDirectory {
    path: PathBuf,
    identity: StableFileIdentity,
    handle: File,
}

impl PinnedDirectory {
    fn verify(&self) -> io::Result<()> {
        reject_link_or_reparse(&self.path)?;
        if !same_file_object(&stable_identity(&self.handle)?, &self.identity) {
            return Err(invalid_data("pinned directory handle identity changed"));
        }
        let current = open_directory_no_follow(&self.path)?;
        if !same_file_object(&stable_identity(&current)?, &self.identity) {
            return Err(invalid_data("pinned directory path identity changed"));
        }
        Ok(())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl AncestorGuard {
    fn acquire(root: &Path, target_directory: &Path) -> io::Result<Self> {
        let relative = target_directory
            .strip_prefix(root)
            .map_err(|_| invalid_data("guarded path is outside the selected physical root"))?;
        let mut paths = Vec::new();
        let mut identities = Vec::new();
        let mut handles = Vec::new();
        let mut current = root.to_path_buf();
        Self::push_directory(&current, &mut paths, &mut identities, &mut handles)?;
        let mut depth = 0usize;
        let mut path_bytes = 0usize;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(invalid_data(
                    "guarded path is not a canonical relative path",
                ));
            };
            let name = name
                .to_str()
                .ok_or_else(|| invalid_data("guarded path is not canonical UTF-8"))?;
            depth = depth
                .checked_add(1)
                .ok_or_else(|| invalid_data("guarded path depth overflow"))?;
            path_bytes = path_bytes
                .checked_add(name.len())
                .and_then(|value| value.checked_add(1))
                .ok_or_else(|| invalid_data("guarded path-byte total overflow"))?;
            if depth > MAX_TREE_DEPTH || path_bytes > MAX_TREE_PATH_BYTES {
                return Err(invalid_data("guarded path exceeds the v1 tree bounds"));
            }
            current.push(name);
            Self::push_directory(&current, &mut paths, &mut identities, &mut handles)?;
        }
        let guard = Self {
            paths,
            identities,
            _handles: handles,
        };
        guard.verify()?;
        Ok(guard)
    }

    fn push_directory(
        path: &Path,
        paths: &mut Vec<PathBuf>,
        identities: &mut Vec<StableFileIdentity>,
        handles: &mut Vec<File>,
    ) -> io::Result<()> {
        reject_link_or_reparse(path)?;
        let handle = open_directory_no_follow(path)?;
        if !handle.metadata()?.is_dir() {
            return Err(invalid_data("guarded path ancestor is not a directory"));
        }
        paths.push(path.to_path_buf());
        identities.push(stable_identity(&handle)?);
        handles.push(handle);
        Ok(())
    }

    fn verify(&self) -> io::Result<()> {
        for (path, identity) in self.paths.iter().zip(&self.identities) {
            reject_link_or_reparse(path)?;
            let current = open_directory_no_follow(path)?;
            if !same_file_object(&stable_identity(&current)?, identity) {
                return Err(invalid_data(
                    "guarded path ancestor identity changed during the operation",
                ));
            }
        }
        Ok(())
    }

    fn directory_path(&self) -> io::Result<&Path> {
        self.paths
            .last()
            .map(PathBuf::as_path)
            .ok_or_else(|| invalid_data("guard has no pinned directory"))
    }

    fn directory_handle(&self) -> io::Result<&File> {
        self._handles
            .last()
            .ok_or_else(|| invalid_data("guard has no pinned directory handle"))
    }

    fn into_final_pin(mut self) -> io::Result<PinnedDirectory> {
        self.verify()?;
        let path = self
            .paths
            .pop()
            .ok_or_else(|| invalid_data("guard has no final directory path"))?;
        let identity = self
            .identities
            .pop()
            .ok_or_else(|| invalid_data("guard has no final directory identity"))?;
        let handle = self
            ._handles
            .pop()
            .ok_or_else(|| invalid_data("guard has no final directory handle"))?;
        if !same_file_object(&stable_identity(&handle)?, &identity) {
            return Err(invalid_data(
                "guard final-directory handoff changed object identity",
            ));
        }
        Ok(PinnedDirectory {
            path,
            identity,
            handle,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivationLeaseRecord {
    schema: String,
    owner_pid: u32,
    canonical_root: String,
}

pub(crate) struct ExclusiveBaselineLease {
    root: PathBuf,
    canonical_root: PathBuf,
    lock_path: PathBuf,
    owner_pid: u32,
    lock_identity: StableFileIdentity,
    lock_file: Option<File>,
    // The activation capability and every production writer contend on the
    // same physical-root OS lease. The persistent writer-lock pathname is
    // merely BaseLease's stable lock target and is never treated as ownership.
    _writer_lease: Box<BaseLease>,
}

impl std::fmt::Debug for ExclusiveBaselineLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExclusiveBaselineLease")
            .field("root", &self.root)
            .field("canonical_root", &self.canonical_root)
            .field("lock_path", &self.lock_path)
            .field("owner_pid", &self.owner_pid)
            .field("lock_identity", &self.lock_identity)
            .finish_non_exhaustive()
    }
}

#[cfg(windows)]
fn ensure_private_mutation_supported() -> io::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn ensure_private_mutation_supported() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private N5-A mutation is disabled on this platform before any lease or filesystem artifact is created",
    ))
}

impl ExclusiveBaselineLease {
    pub(crate) fn acquire_disposable(root: &Path) -> io::Result<Self> {
        let mut failpoint = None;
        Self::acquire_disposable_inner(root, &mut failpoint)
    }

    #[cfg(test)]
    fn acquire_disposable_with_failpoint(
        root: &Path,
        mut failpoint: Option<Box<dyn OperationFailpoint>>,
    ) -> io::Result<Self> {
        Self::acquire_disposable_inner(root, &mut failpoint)
    }

    fn acquire_disposable_inner(
        root: &Path,
        failpoint: &mut Option<Box<dyn OperationFailpoint>>,
    ) -> io::Result<Self> {
        ensure_private_mutation_supported()?;
        if !root.is_dir() {
            return Err(invalid_data(
                "baseline activation lease requires an existing base directory",
            ));
        }
        reject_link_or_reparse(root)?;
        let writer_lease = acquire_physical_root_lease(root, "baseline activation")?;
        let canonical_root = writer_lease.canonical_root().to_path_buf();
        let lock_path = root.join(ACTIVATION_LOCK_FILE_NAME);
        let mut occurrences = BTreeMap::new();
        hit_failpoint(
            failpoint,
            &mut occurrences,
            OperationStage::LeaseBeforeWrite,
        )?;
        let mut lock_file = match create_activation_lock(root, &lock_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if !reclaim_dead_activation_lock(
                    root,
                    &canonical_root,
                    &lock_path,
                    failpoint,
                    &mut occurrences,
                )? {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "exclusive baseline activation lease is held by a live or unverified owner",
                    ));
                }
                create_activation_lock(root, &lock_path)?
            }
            Err(error) => return Err(error),
        };
        bind_activation_lock_to_current_process(&lock_file)?;
        let created_identity = stable_identity(&lock_file)?;
        require_single_link(&created_identity, "new activation lease record")?;
        let owner_pid = std::process::id();
        let canonical_root_text = canonical_root
            .to_str()
            .ok_or_else(|| invalid_data("activation root is not canonical UTF-8"))?;
        let record = ActivationLeaseRecord {
            schema: ACTIVATION_LEASE_SCHEMA.to_owned(),
            owner_pid,
            canonical_root: canonical_root_text.to_owned(),
        };
        let bytes = encode_record(&record)?;
        let acquisition = (|| -> io::Result<StableFileIdentity> {
            lock_file.write_all(&bytes)?;
            hit_failpoint(failpoint, &mut occurrences, OperationStage::LeaseAfterWrite)?;
            lock_file.flush()?;
            hit_failpoint(failpoint, &mut occurrences, OperationStage::LeaseAfterFlush)?;
            lock_file.sync_all()?;
            hit_failpoint(failpoint, &mut occurrences, OperationStage::LeaseAfterSync)?;
            sync_directory(root)?;
            hit_failpoint(
                failpoint,
                &mut occurrences,
                OperationStage::LeaseAfterParentSync,
            )?;
            stable_identity(&lock_file)
        })();
        let lock_identity = match acquisition {
            Ok(identity) => identity,
            Err(error) => {
                let cleanup = mark_verified_file_for_removal_guarded(
                    root,
                    root,
                    &lock_path,
                    &created_identity,
                    &lock_file,
                    failpoint,
                );
                if cleanup.is_err() {
                    let mut no_failpoint = None;
                    let _ = mark_verified_file_for_removal_guarded(
                        root,
                        root,
                        &lock_path,
                        &created_identity,
                        &lock_file,
                        &mut no_failpoint,
                    );
                }
                drop(lock_file);
                let _ = sync_directory(root);
                return Err(error);
            }
        };
        Ok(Self {
            root: root.to_path_buf(),
            canonical_root,
            lock_path,
            owner_pid,
            lock_identity,
            lock_file: Some(lock_file),
            _writer_lease: Box::new(writer_lease),
        })
    }

    fn verify(&self) -> io::Result<()> {
        let lock_metadata = fs::symlink_metadata(&self.lock_path)?;
        #[cfg(windows)]
        let current_lock = open_verification_handle_while_mutating(&self.lock_path, false)?;
        #[cfg(unix)]
        let current_lock = open_no_follow(&self.lock_path)?;
        let held_lock = self
            .lock_file
            .as_ref()
            .ok_or_else(|| invalid_data("activation lease handle is unavailable"))?;
        let owner_instance_live = process_is_alive(self.owner_pid, held_lock)?;
        if fs::canonicalize(&self.root)? != self.canonical_root
            || is_link_or_reparse(&self.lock_path, &lock_metadata)
            || !lock_metadata.is_file()
            || stable_identity(&current_lock)? != self.lock_identity
            || stable_identity(held_lock)? != self.lock_identity
            || self.owner_pid != std::process::id()
            || !owner_instance_live
            || self._writer_lease.canonical_root() != self.canonical_root
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "exclusive baseline activation lease or quiescence was lost",
            ));
        }
        Ok(())
    }

    fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for ExclusiveBaselineLease {
    fn drop(&mut self) {
        if self.verify().is_ok() {
            if let Some(lock_file) = self.lock_file.take() {
                let mut no_failpoint = None;
                let _ = mark_verified_file_for_removal_guarded(
                    &self.root,
                    &self.root,
                    &self.lock_path,
                    &self.lock_identity,
                    &lock_file,
                    &mut no_failpoint,
                );
                drop(lock_file);
            }
            let _ = sync_directory(&self.root);
        }
    }
}

fn acquire_physical_root_lease(root: &Path, purpose: &str) -> io::Result<BaseLease> {
    BaseLease::acquire(root).map_err(|error| match error {
        BaseLeaseError::Busy { .. } => io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("{purpose} requires the exclusive physical-root writer lease"),
        ),
        other => io::Error::other(other.to_string()),
    })
}

fn create_activation_lock(root: &Path, path: &Path) -> io::Result<File> {
    let guard = AncestorGuard::acquire(root, root)?;
    create_new_file_guarded(&guard, path)
}

fn reclaim_dead_activation_lock(
    root: &Path,
    canonical_root: &Path,
    lock_path: &Path,
    failpoint: &mut Option<Box<dyn OperationFailpoint>>,
    occurrences: &mut BTreeMap<OperationStage, usize>,
) -> io::Result<bool> {
    let (file, identity) =
        open_verified_control_record(root, lock_path, "activation lease record")?;
    if identity.length > DEFAULT_LIMITS.max_record_bytes {
        return Err(invalid_data(
            "activation lease record exceeds its byte limit",
        ));
    }
    let mut reader = BufReader::with_capacity(STREAM_BUFFER_BYTES, file);
    let mut bytes = Vec::with_capacity(
        usize::try_from(identity.length)
            .map_err(|_| invalid_data("activation lease allocation does not fit usize"))?,
    );
    let mut buffer = [0u8; STREAM_BUFFER_BYTES];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        if (bytes.len() as u64)
            .checked_add(read as u64)
            .is_none_or(|total| total > DEFAULT_LIMITS.max_record_bytes)
        {
            return Err(invalid_data("activation lease grew beyond its byte limit"));
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    if bytes.len() as u64 != identity.length {
        return Err(invalid_data("activation lease changed while being read"));
    }
    let after = stable_identity(reader.get_ref())?;
    if after != identity {
        return Err(invalid_data(
            "activation lease identity changed while being read",
        ));
    }
    let record = decode_record::<ActivationLeaseRecord>(&bytes, "activation lease")?;
    let expected_root = canonical_root
        .to_str()
        .ok_or_else(|| invalid_data("activation root is not canonical UTF-8"))?;
    if record.schema != ACTIVATION_LEASE_SCHEMA || record.canonical_root != expected_root {
        return Err(invalid_data(
            "activation lease record does not match this base",
        ));
    }
    if process_is_alive(record.owner_pid, reader.get_ref())? {
        return Ok(false);
    }
    let path_metadata = fs::symlink_metadata(lock_path)?;
    let current_lock = open_no_follow(lock_path)?;
    if is_link_or_reparse(lock_path, &path_metadata) || stable_identity(&current_lock)? != identity
    {
        return Err(invalid_data(
            "activation lease changed before stale-owner reclamation",
        ));
    }
    drop(current_lock);
    drop(reader);
    hit_failpoint(failpoint, occurrences, OperationStage::CleanupBeforeRemove)?;
    remove_verified_file_guarded(root, root, lock_path, &identity, failpoint)?;
    hit_failpoint(failpoint, occurrences, OperationStage::CleanupAfterRemove)?;
    sync_directory(root)?;
    hit_failpoint(
        failpoint,
        occurrences,
        OperationStage::CleanupAfterParentSync,
    )?;
    Ok(true)
}

#[cfg(windows)]
fn windows_filetime_value(value: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64
}

#[cfg(windows)]
fn windows_process_creation_time(
    handle: windows_sys::Win32::Foundation::HANDLE,
) -> io::Result<windows_sys::Win32::Foundation::FILETIME> {
    use std::mem::MaybeUninit;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;

    let mut creation = MaybeUninit::<FILETIME>::zeroed();
    let mut exit = MaybeUninit::<FILETIME>::zeroed();
    let mut kernel = MaybeUninit::<FILETIME>::zeroed();
    let mut user = MaybeUninit::<FILETIME>::zeroed();
    // Safety: `handle` is a live process handle with query access. Every output
    // pointer refers to distinct, aligned writable FILETIME storage.
    let result = unsafe {
        GetProcessTimes(
            handle,
            creation.as_mut_ptr(),
            exit.as_mut_ptr(),
            kernel.as_mut_ptr(),
            user.as_mut_ptr(),
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    // Safety: a nonzero GetProcessTimes result initializes every output,
    // including `creation`.
    Ok(unsafe { creation.assume_init() })
}

#[cfg(windows)]
fn activation_lock_creation_time(
    lock_file: &File,
) -> io::Result<windows_sys::Win32::Foundation::FILETIME> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Storage::FileSystem::GetFileTime;

    let mut creation = MaybeUninit::<FILETIME>::zeroed();
    // Safety: the raw handle is borrowed from a live activation-lock File and
    // `creation` is aligned writable FILETIME storage. Null optional outputs
    // request only the creation timestamp used as the process-instance token.
    let result = unsafe {
        GetFileTime(
            lock_file.as_raw_handle() as _,
            creation.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    // Safety: a nonzero GetFileTime result initialized `creation`.
    Ok(unsafe { creation.assume_init() })
}

#[cfg(windows)]
fn bind_activation_lock_to_current_process(lock_file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::SetFileTime;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    // Safety: GetCurrentProcess returns the documented process pseudo-handle;
    // it is valid for the current process and must not be closed.
    let process = unsafe { GetCurrentProcess() };
    let creation = windows_process_creation_time(process)?;
    // Safety: the file handle is borrowed and live with write-attributes access.
    // `creation` remains live for the call; null optional pointers preserve the
    // access and write timestamps. The pseudo process handle is not consumed.
    let result = unsafe {
        SetFileTime(
            lock_file.as_raw_handle() as _,
            &creation,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
fn bind_activation_lock_to_current_process(_lock_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(windows)]
fn process_is_alive(owner_pid: u32, lock_file: &File) -> io::Result<bool> {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, GetLastError, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
    };

    // Safety: OpenProcess receives a numeric PID and no borrowed pointer; a
    // non-null returned handle is owned here and closed exactly once below.
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
            0,
            owner_pid,
        )
    };
    if handle.is_null() {
        // Safety: GetLastError has no pointer or lifetime preconditions and is
        // read immediately after the failed OpenProcess call.
        return match unsafe { GetLastError() } {
            ERROR_INVALID_PARAMETER => Ok(false),
            ERROR_ACCESS_DENIED => Ok(true),
            error => Err(io::Error::from_raw_os_error(error as i32)),
        };
    }

    let result = (|| -> io::Result<bool> {
        // Safety: `handle` is a live process handle opened with SYNCHRONIZE.
        // A zero timeout observes signaled state without blocking.
        match unsafe { WaitForSingleObject(handle, 0) } {
            WAIT_OBJECT_0 => return Ok(false),
            WAIT_TIMEOUT => {}
            _ => return Err(io::Error::last_os_error()),
        }
        let process_creation = windows_process_creation_time(handle)?;
        let recorded_creation = activation_lock_creation_time(lock_file)?;
        Ok(windows_filetime_value(process_creation) == windows_filetime_value(recorded_creation))
    })();
    // Safety: `handle` is the non-null owned handle returned by OpenProcess and
    // has not previously been closed.
    unsafe {
        CloseHandle(handle);
    }
    result
}

#[cfg(unix)]
fn process_is_alive(owner_pid: u32, _lock_file: &File) -> io::Result<bool> {
    let owner_pid = i32::try_from(owner_pid)
        .map_err(|_| invalid_data("activation lease PID is not supported on this platform"))?;
    // Safety: signal zero performs only the documented liveness/permission
    // probe; `owner_pid` was checked to fit the platform pid_t representation.
    if unsafe { libc::kill(owner_pid, 0) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error),
    }
}

#[cfg(not(any(unix, windows)))]
fn process_is_alive(_owner_pid: u32, _lock_file: &File) -> io::Result<bool> {
    Ok(true)
}

/// Recovery result. A base with no N5 format is legacy generation zero and is
/// left unchanged by recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecoveryState {
    pub(crate) generation: u64,
    pub(crate) commit_hash: String,
    pub(crate) legacy: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BaselineMigrationStatus {
    Created,
    Resumed,
    AlreadyMigrated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BaselineMigrationResult {
    pub(crate) status: BaselineMigrationStatus,
    pub(crate) state: RecoveryState,
    pub(crate) logical_state_digest: String,
    pub(crate) component_count: usize,
    pub(crate) total_bytes: u64,
    pub(crate) available_copy_bytes: u64,
    pub(crate) rollback_policy: String,
}

pub(crate) enum TransactionAdmission {
    New(OperationTransaction),
    AlreadyCommitted(RecoveryState),
}

struct RecoveryScan {
    state: RecoveryState,
    live_projection: Option<FixtureProjection>,
    transactions: BTreeMap<TransactionId, TransactionIdStatus>,
    saw_incomplete: bool,
}

impl RecoveryState {
    fn legacy() -> Self {
        Self {
            generation: 0,
            commit_hash: empty_hash(),
            legacy: true,
        }
    }
}

/// A prepared immutable generation. It is not visible to recovery until
/// `commit()` atomically publishes the fully staged generation directory.
pub(crate) struct OperationTransaction {
    root: PathBuf,
    generation: u64,
    parent_commit_hash: String,
    transaction_id: TransactionId,
    operation: OperationKind,
    intent_hash: String,
    components: BTreeMap<String, ComponentDescriptor>,
    pre_projection: FixtureProjection,
    failpoint: Option<Box<dyn OperationFailpoint>>,
    occurrences: BTreeMap<OperationStage, usize>,
    limits: Box<ResourceLimits>,
    committed: bool,
    // Private N5-A transactions acquire the physical-root writer lease
    // directly. N5-B must replace this with an explicit handoff from the
    // production owner rather than attempting a nested acquisition.
    _writer_lease: Box<BaseLease>,
}

impl OperationTransaction {
    /// Scan committed generations without modifying an old base.
    ///
    /// A valid prepare without `commit.bin` is intentionally ignored. A corrupt
    /// committed generation, a chain gap, or a committed generation after an
    /// incomplete one fails closed.
    pub(crate) fn recover(root: &Path) -> io::Result<RecoveryState> {
        Self::recover_with_failpoint(root, None)
    }

    fn recover_with_failpoint(
        root: &Path,
        failpoint: Option<Box<dyn OperationFailpoint>>,
    ) -> io::Result<RecoveryState> {
        Ok(Self::recover_scan_with_failpoint(root, failpoint)?.state)
    }

    fn recover_scan_with_failpoint(
        root: &Path,
        mut failpoint: Option<Box<dyn OperationFailpoint>>,
    ) -> io::Result<RecoveryScan> {
        let mut occurrences = BTreeMap::new();
        hit_failpoint(
            &mut failpoint,
            &mut occurrences,
            OperationStage::RecoveryBeforeControlScan,
        )?;
        let transaction_dir = transaction_dir(root);
        let format_path = transaction_dir.join(FORMAT_FILE_NAME);
        let generations = generations_dir(root);

        if !path_entry_exists(&format_path)? {
            validate_interrupted_activation_layout(root)?;
            hit_failpoint(
                &mut failpoint,
                &mut occurrences,
                OperationStage::RecoveryAfterControlScan,
            )?;
            hit_failpoint(
                &mut failpoint,
                &mut occurrences,
                OperationStage::RecoveryComplete,
            )?;
            return Ok(RecoveryScan {
                state: RecoveryState::legacy(),
                live_projection: None,
                transactions: BTreeMap::new(),
                saw_incomplete: false,
            });
        }

        validate_formatted_control_layout(root)?;
        hit_failpoint(
            &mut failpoint,
            &mut occurrences,
            OperationStage::RecoveryAfterControlScan,
        )?;
        validate_format(&read_record_under::<FormatRecord>(
            root,
            &format_path,
            "format",
        )?)?;
        let baseline = validate_baseline(root, false)?.ok_or_else(|| {
            invalid_data("operation transaction format has no immutable baseline")
        })?;
        validate_migration(root, &baseline)?;
        hit_failpoint(
            &mut failpoint,
            &mut occurrences,
            OperationStage::RecoveryAfterBaselineValidation,
        )?;
        if !generations.is_dir() {
            return Err(invalid_data(
                "operation transaction format exists without generations directory",
            ));
        }

        let mut directories = Vec::new();
        for entry in read_directory_bounded(&generations, &DEFAULT_LIMITS)? {
            if entry.file_name() == PUBLICATION_DIRECTORY_NAME {
                let metadata = fs::symlink_metadata(entry.path())?;
                if !metadata.is_dir() || is_link_or_reparse(&entry.path(), &metadata) {
                    return Err(invalid_data(
                        "private publication namespace is not an ordinary directory",
                    ));
                }
                let carriers = read_directory_bounded(&entry.path(), &DEFAULT_LIMITS)?;
                if carriers.len() > 1 {
                    return Err(invalid_data(
                        "private publication namespace contains more than one carrier",
                    ));
                }
                for carrier in carriers {
                    let name = carrier.file_name();
                    let name = name.to_str().ok_or_else(|| {
                        invalid_data("private publication carrier is not canonical UTF-8")
                    })?;
                    if !name.starts_with(PUBLICATION_GENERATION_PREFIX)
                        && !name.starts_with(CLEANUP_GENERATION_PREFIX)
                    {
                        return Err(invalid_data(
                            "private publication namespace contains an unknown artifact",
                        ));
                    }
                    directories.push(carrier);
                }
            } else {
                directories.push(entry);
            }
        }
        if directories.len() > DEFAULT_LIMITS.max_generations {
            return Err(invalid_data(
                "operation transaction generation count exceeds the N5-A limit",
            ));
        }
        directories.sort_by_key(|entry| {
            let name = entry.file_name();
            let numeric = name.len() == 20
                && name
                    .to_str()
                    .is_some_and(|value| value.bytes().all(|byte| byte.is_ascii_digit()));
            (!numeric, name)
        });

        let mut state = RecoveryState {
            generation: 0,
            commit_hash: empty_hash(),
            legacy: false,
        };
        let mut saw_incomplete = false;
        let mut expected_live_components = baseline_component_sources(root, &baseline);
        let mut transactions = BTreeMap::<TransactionId, TransactionIdStatus>::new();

        for (occurrence, entry) in directories.into_iter().enumerate() {
            hit_failpoint_at(
                &mut failpoint,
                OperationStage::RecoveryBeforeGeneration,
                occurrence,
            )?;
            let entry_type = entry.file_type()?;
            if !entry_type.is_dir() || is_link_or_reparse(&entry.path(), &entry.metadata()?) {
                return Err(invalid_data(
                    "operation transaction generations contains a non-directory entry",
                ));
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| invalid_data("generation name is not canonical UTF-8"))?;
            if name.starts_with(CLEANUP_GENERATION_PREFIX) {
                if saw_incomplete {
                    return Err(invalid_data(
                        "more than one incomplete transaction generation exists",
                    ));
                }
                let path = entry.path();
                let (_, binding) =
                    validate_cleanup_tombstone_identity(&path, CleanupNamespaceKind::Generation)?;
                let CleanupBinding::Generation(identity) = binding else {
                    return Err(invalid_data(
                        "generation cleanup tombstone has a baseline binding",
                    ));
                };
                if identity.generation != state.generation.saturating_add(1) {
                    return Err(invalid_data(
                        "generation cleanup tombstone is not the exact next generation",
                    ));
                }
                validate_cleanup_tree_subset(root, &path, CleanupNamespaceKind::Generation)?;
                let prepare_path = path.join(PREPARE_FILE_NAME);
                if path_entry_exists(&prepare_path)? {
                    let prepare = read_prepare(root, &prepare_path)?;
                    validate_prepare(&prepare, identity.generation, &state)?;
                    if prepare.body.transaction_id != identity.transaction_id
                        || prepare.body.operation != identity.operation
                        || prepare.body.intent_hash != identity.intent_hash
                    {
                        return Err(invalid_data(
                            "generation cleanup tombstone name conflicts with remaining prepare",
                        ));
                    }
                }
                if transactions.contains_key(&identity.transaction_id) {
                    return Err(invalid_data(
                        "transaction_id appears in more than one generation",
                    ));
                }
                transactions.insert(
                    identity.transaction_id.clone(),
                    TransactionIdStatus::Prepared {
                        operation: identity.operation,
                        intent_hash: identity.intent_hash,
                        generation_dir: path,
                    },
                );
                saw_incomplete = true;
                hit_failpoint_at(
                    &mut failpoint,
                    OperationStage::RecoveryAfterGeneration,
                    occurrence,
                )?;
                continue;
            }
            if name.starts_with(PUBLICATION_GENERATION_PREFIX) {
                if saw_incomplete {
                    return Err(invalid_data(
                        "more than one incomplete transaction generation exists",
                    ));
                }
                let path = entry.path();
                let (_, identity) = validate_publication_carrier_identity(&path)?;
                if identity.generation != state.generation.saturating_add(1) {
                    return Err(invalid_data(
                        "publication carrier is not the exact next generation",
                    ));
                }
                validate_pending_generation_layout(root, &path, &identity, &state)?;
                if transactions.contains_key(&identity.transaction_id) {
                    return Err(invalid_data(
                        "transaction_id appears in more than one generation",
                    ));
                }
                transactions.insert(
                    identity.transaction_id,
                    TransactionIdStatus::Prepared {
                        operation: identity.operation,
                        intent_hash: identity.intent_hash,
                        generation_dir: path,
                    },
                );
                saw_incomplete = true;
                hit_failpoint_at(
                    &mut failpoint,
                    OperationStage::RecoveryAfterGeneration,
                    occurrence,
                )?;
                continue;
            }
            if name.starts_with(PENDING_GENERATION_PREFIX) {
                if saw_incomplete {
                    return Err(invalid_data(
                        "more than one incomplete transaction generation exists",
                    ));
                }
                let identity = parse_pending_generation_name(&name)?;
                if identity.generation != state.generation.saturating_add(1) {
                    return Err(invalid_data(
                        "pending transaction generation is not the exact next generation",
                    ));
                }
                let path = entry.path();
                validate_pending_generation_layout(root, &path, &identity, &state)?;
                if transactions.contains_key(&identity.transaction_id) {
                    return Err(invalid_data(
                        "transaction_id appears in more than one generation",
                    ));
                }
                transactions.insert(
                    identity.transaction_id,
                    TransactionIdStatus::Prepared {
                        operation: identity.operation,
                        intent_hash: identity.intent_hash,
                        generation_dir: path,
                    },
                );
                saw_incomplete = true;
                hit_failpoint_at(
                    &mut failpoint,
                    OperationStage::RecoveryAfterGeneration,
                    occurrence,
                )?;
                continue;
            }
            let generation = parse_generation_name(&name)?;
            if generation != state.generation.saturating_add(1) {
                return Err(invalid_data(
                    "operation transaction generations are not exactly contiguous",
                ));
            }
            let path = entry.path();
            validate_generation_layout(root, &path)?;
            let prepare_path = path.join(PREPARE_FILE_NAME);
            let commit_path = path.join(COMMIT_FILE_NAME);

            if !path_entry_exists(&prepare_path)? {
                if path_entry_exists(&commit_path)? {
                    return Err(invalid_data("committed generation has no prepare record"));
                }
                saw_incomplete = true;
                hit_failpoint_at(
                    &mut failpoint,
                    OperationStage::RecoveryAfterGeneration,
                    occurrence,
                )?;
                continue;
            }

            let prepare = read_prepare(root, &prepare_path)?;
            validate_prepare(&prepare, generation, &state)?;
            if transactions.contains_key(&prepare.body.transaction_id) {
                return Err(invalid_data(
                    "transaction_id appears in more than one generation",
                ));
            }

            if !path_entry_exists(&commit_path)? {
                saw_incomplete = true;
                transactions.insert(
                    prepare.body.transaction_id.clone(),
                    TransactionIdStatus::Prepared {
                        operation: prepare.body.operation,
                        intent_hash: prepare.body.intent_hash,
                        generation_dir: path.clone(),
                    },
                );
                hit_failpoint_at(
                    &mut failpoint,
                    OperationStage::RecoveryAfterGeneration,
                    occurrence,
                )?;
                continue;
            }
            if saw_incomplete {
                return Err(invalid_data(
                    "committed generation follows an incomplete generation",
                ));
            }

            let commit_bytes =
                read_bytes_bounded_under(root, &commit_path, DEFAULT_LIMITS.max_record_bytes)?;
            let commit = decode_record::<CommitRecord>(&commit_bytes, "commit")?;
            validate_commit(root, &commit, &prepare, generation, &state, &path)?;
            let next_sources = overlay_component_sources(
                &expected_live_components,
                &commit.body.components,
                &path.join(COMPONENTS_DIR_NAME),
            );
            validate_fixture_transition(
                &expected_live_components,
                &next_sources,
                &commit.body.transaction_id,
            )?;
            expected_live_components = next_sources;
            if commit.body.transaction_id != prepare.body.transaction_id {
                return Err(invalid_data(
                    "commit transaction_id does not match prepare transaction_id",
                ));
            }
            state.generation = generation;
            state.commit_hash = hash_hex(&commit_bytes);
            transactions.insert(
                commit.body.transaction_id.clone(),
                TransactionIdStatus::Committed {
                    operation: commit.body.operation(),
                    intent_hash: commit.body.intent_hash.clone(),
                    state: state.clone(),
                },
            );
            hit_failpoint_at(
                &mut failpoint,
                OperationStage::RecoveryAfterGeneration,
                occurrence,
            )?;
        }

        hit_failpoint(
            &mut failpoint,
            &mut occurrences,
            OperationStage::RecoveryBeforeLiveValidation,
        )?;
        let live_projection =
            validate_baseline_backed_live_state(root, state.generation, &expected_live_components)?;
        hit_failpoint(
            &mut failpoint,
            &mut occurrences,
            OperationStage::RecoveryAfterLiveValidation,
        )?;
        hit_failpoint(
            &mut failpoint,
            &mut occurrences,
            OperationStage::RecoveryComplete,
        )?;

        Ok(RecoveryScan {
            state,
            live_projection: Some(live_projection),
            transactions,
            saw_incomplete,
        })
    }

    /// Prepare a transaction using a canonical operation-intent byte sequence.
    pub(crate) fn begin(
        root: &Path,
        transaction_id: TransactionId,
        operation: OperationKind,
        canonical_intent: &[u8],
    ) -> io::Result<TransactionAdmission> {
        Self::begin_with_failpoint(root, transaction_id, operation, canonical_intent, None)
    }

    /// Prepare a transaction with an optional deterministic failpoint hook.
    pub(crate) fn begin_with_failpoint(
        root: &Path,
        transaction_id: TransactionId,
        operation: OperationKind,
        canonical_intent: &[u8],
        failpoint: Option<Box<dyn OperationFailpoint>>,
    ) -> io::Result<TransactionAdmission> {
        Self::begin_with_options(
            root,
            transaction_id,
            operation,
            canonical_intent,
            failpoint,
            DEFAULT_LIMITS,
        )
    }

    fn begin_with_options(
        root: &Path,
        transaction_id: TransactionId,
        operation: OperationKind,
        canonical_intent: &[u8],
        failpoint: Option<Box<dyn OperationFailpoint>>,
        limits: ResourceLimits,
    ) -> io::Result<TransactionAdmission> {
        ensure_private_mutation_supported()?;
        let intent_hash = hash_hex(canonical_intent);
        let writer_lease = acquire_physical_root_lease(root, "operation transaction")?;
        let mut transaction = Self {
            root: root.to_path_buf(),
            generation: 0,
            parent_commit_hash: String::new(),
            transaction_id: transaction_id.clone(),
            operation,
            intent_hash: intent_hash.clone(),
            components: BTreeMap::new(),
            pre_projection: FixtureProjection {
                counter: 0,
                history: Vec::new(),
            },
            failpoint,
            occurrences: BTreeMap::new(),
            limits: Box::new(limits),
            committed: false,
            _writer_lease: Box::new(writer_lease),
        };
        transaction.ensure_format()?;
        let mut scan = Self::recover_scan_with_failpoint(root, None)?;
        transaction.pre_projection = scan.live_projection.clone().ok_or_else(|| {
            invalid_data("formatted transaction recovery has no typed live projection")
        })?;
        transaction.parent_commit_hash = scan.state.commit_hash.clone();
        match scan
            .transactions
            .remove(&transaction_id)
            .unwrap_or(TransactionIdStatus::Absent)
        {
            TransactionIdStatus::Committed {
                operation: found_operation,
                intent_hash: found_intent,
                state,
            } => {
                if found_operation == operation && found_intent == intent_hash {
                    return Ok(TransactionAdmission::AlreadyCommitted(state));
                }
                return Err(invalid_data(
                    "transaction_id was reused with conflicting operation intent",
                ));
            }
            TransactionIdStatus::Prepared {
                operation: found_operation,
                intent_hash: found_intent,
                generation_dir,
            } => {
                if found_operation != operation || found_intent != intent_hash {
                    return Err(invalid_data(
                        "transaction_id was reused with conflicting prepared intent",
                    ));
                }
                transaction.generation = admitted_next_generation(scan.state.generation, &limits)?;
                transaction.cleanup_incomplete_generation(&generation_dir)?;
            }
            TransactionIdStatus::Absent => {
                if scan.saw_incomplete {
                    return Err(invalid_data(
                        "another transaction has an incomplete generation",
                    ));
                }
            }
        }
        if transaction.generation == 0 {
            transaction.generation = admitted_next_generation(scan.state.generation, &limits)?;
        }
        let record = PrepareRecord::new(PrepareBody {
            magic: FORMAT_MAGIC.to_owned(),
            version: FORMAT_VERSION,
            codec: PRIVATE_CODEC_ID.to_owned(),
            generation: transaction.generation,
            parent_commit_hash: transaction.parent_commit_hash.clone(),
            transaction_id,
            operation,
            intent_hash: transaction.intent_hash.clone(),
        })?;
        let pending_dir = transaction.pending_generation_dir();
        transaction.hit(OperationStage::BeforeGenerationCreate)?;
        create_directory_guarded(root, &generations_dir(root), &pending_dir)?;
        transaction.hit(OperationStage::AfterGenerationCreate)?;
        sync_directory(&pending_dir)?;
        transaction.hit(OperationStage::AfterGenerationSync)?;
        sync_directory(&generations_dir(root))?;
        transaction.hit(OperationStage::AfterGenerationsParentSync)?;

        let components_dir = pending_dir.join(COMPONENTS_DIR_NAME);
        transaction.hit(OperationStage::BeforeGenerationComponentsCreate)?;
        create_directory_guarded(root, &pending_dir, &components_dir)?;
        transaction.hit(OperationStage::AfterGenerationComponentsCreate)?;
        sync_directory(&components_dir)?;
        transaction.hit(OperationStage::AfterGenerationComponentsSync)?;
        sync_directory(&pending_dir)?;
        transaction.hit(OperationStage::AfterGenerationComponentsParentSync)?;

        transaction.hit(OperationStage::BeforePrepareWrite)?;
        let prepare_temp = pending_dir.join(PREPARE_TEMP_FILE_NAME);
        let prepare_target = pending_dir.join(PREPARE_FILE_NAME);
        write_new_record_with_failpoints(
            root,
            &prepare_temp,
            &record,
            &mut transaction.failpoint,
            &mut transaction.occurrences,
            [
                OperationStage::AfterPrepareWrite,
                OperationStage::AfterPrepareFlush,
                OperationStage::AfterPrepareSync,
            ],
        )?;
        transaction.hit(OperationStage::BeforePreparePublish)?;
        atomic_rename_under(
            root,
            &prepare_temp,
            &prepare_target,
            &mut transaction.failpoint,
        )?;
        transaction.hit(OperationStage::AfterPrepareRename)?;
        sync_directory(&pending_dir)?;
        transaction.hit(OperationStage::AfterPrepareParentSync)?;

        Ok(TransactionAdmission::New(transaction))
    }

    /// Write one immutable component owned by this generation.
    ///
    /// Component paths are relative to `components/`; absolute paths, parent
    /// traversal, and duplicate paths are rejected before any file is written.
    pub(crate) fn stage_component(&mut self, relative_path: &Path, bytes: &[u8]) -> io::Result<()> {
        if self.committed {
            return Err(io::Error::other("cannot stage a committed transaction"));
        }
        if bytes.len() as u64 > MAX_COMPONENT_BYTES {
            return Err(invalid_data(
                "transaction component exceeds the N5-A size limit",
            ));
        }
        let normalized = normalize_component_path(relative_path)?;
        if self.components.contains_key(&normalized) {
            return Err(invalid_data("transaction component path is already staged"));
        }

        let kind = registry_kind(&normalized)?;
        validate_component_bytes(kind, bytes)?;
        let occurrence = self.components.len();
        self.hit_at(OperationStage::BeforeComponentWrite, occurrence)?;
        let target = self
            .generation_dir()
            .join(COMPONENTS_DIR_NAME)
            .join(&normalized);
        let parent = target
            .parent()
            .ok_or_else(|| io::Error::other("component path has no parent"))?;
        let components_root = self.generation_dir().join(COMPONENTS_DIR_NAME);
        self.hit_at(OperationStage::BeforeComponentParentCreate, occurrence)?;
        if !path_entry_exists(parent)? {
            create_directory_guarded(&self.root, &components_root, parent)?;
        }
        self.hit_at(OperationStage::AfterComponentParentCreate, occurrence)?;
        sync_directory(parent)?;
        self.hit_at(OperationStage::AfterComponentParentSync, occurrence)?;
        sync_directory(&components_root)?;
        self.hit_at(OperationStage::AfterComponentComponentsSync, occurrence)?;
        let _guard = AncestorGuard::acquire(&self.root, parent)?;
        if path_entry_exists(&target)? {
            return Err(invalid_data("transaction component target already exists"));
        }
        write_new_file_with_failpoints(
            &self.root,
            &target,
            bytes,
            &mut self.failpoint,
            [
                OperationStage::AfterComponentWrite,
                OperationStage::AfterComponentFlush,
                OperationStage::AfterComponentFileSync,
            ],
            occurrence,
        )?;
        sync_directory(parent)?;
        self.hit_at(OperationStage::AfterComponentSync, occurrence)?;

        self.components.insert(
            normalized.clone(),
            ComponentDescriptor {
                kind,
                relative_path: normalized,
                length: bytes.len() as u64,
                blake3_hash: hash_hex(bytes),
            },
        );
        Ok(())
    }

    /// Atomically publish the durable commit point for this generation.
    pub(crate) fn commit(mut self) -> io::Result<RecoveryState> {
        if self.committed {
            return Err(io::Error::other(
                "operation transaction is already committed",
            ));
        }
        let components = self.components.values().cloned().collect::<Vec<_>>();
        if components.is_empty() {
            return Err(invalid_data(
                "operation transaction commit requires at least one component",
            ));
        }
        let prepare_path = self.generation_dir().join(PREPARE_FILE_NAME);
        let prepare_hash = hash_hex(&read_bytes_bounded_under(
            &self.root,
            &prepare_path,
            DEFAULT_LIMITS.max_record_bytes,
        )?);
        validate_staged_fixture_transition(
            &self.generation_dir().join(COMPONENTS_DIR_NAME),
            &components,
            &self.pre_projection,
            &self.transaction_id,
        )?;
        let logical_snapshot_hash =
            logical_snapshot_hash(&self.parent_commit_hash, &prepare_hash, &components)?;
        let record = CommitRecord::new(CommitBody {
            magic: FORMAT_MAGIC.to_owned(),
            version: FORMAT_VERSION,
            codec: PRIVATE_CODEC_ID.to_owned(),
            generation: self.generation,
            parent_commit_hash: self.parent_commit_hash.clone(),
            prepare_hash,
            transaction_id: self.transaction_id.clone(),
            operation: self.operation,
            intent_hash: self.intent_hash.clone(),
            components,
            logical_snapshot_hash,
        })?;

        self.hit(OperationStage::BeforeCommitWrite)?;
        let generation_dir = self.generation_dir();
        write_new_record_with_failpoints(
            &self.root,
            &generation_dir.join(COMMIT_TEMP_FILE_NAME),
            &record,
            &mut self.failpoint,
            &mut self.occurrences,
            [
                OperationStage::AfterCommitWrite,
                OperationStage::AfterCommitFlush,
                OperationStage::AfterCommitSync,
            ],
        )?;
        match self.finish_commit_publication(&record, true) {
            Ok(state) => {
                self.committed = true;
                Ok(state)
            }
            Err(original) => {
                // A returned error after the live fixture has transitioned may
                // not leave an uncommitted post-state. Re-run only the
                // idempotent, already-bound physical-carrier finalizer with
                // fault injection detached. Success establishes the committed
                // post-state even though the original call reports its error.
                let saved_failpoint = self.failpoint.take();
                let resolved = self.finish_commit_publication(&record, false);
                self.failpoint = saved_failpoint;
                match resolved {
                    Ok(_) => {
                        self.committed = true;
                        Err(io::Error::new(
                            original.kind(),
                            format!(
                                "{original}; commit resolved to the exact committed post-state"
                            ),
                        ))
                    }
                    Err(resolution) => match cleanup_failed_publication_carrier(
                        &self.root,
                        &PendingGenerationIdentity {
                            generation: self.generation,
                            transaction_id: self.transaction_id.clone(),
                            operation: self.operation,
                            intent_hash: self.intent_hash.clone(),
                        },
                    ) {
                        Err(cleanup) => Err(io::Error::new(
                            original.kind(),
                            format!(
                                "{original}; exact commit-state resolution failed: {resolution}; malformed owned-carrier cleanup also failed: {cleanup}"
                            ),
                        )),
                        Ok(()) => match restore_fixture_projection(
                            &self.root,
                            &self.pre_projection,
                            &self.limits,
                        ) {
                            Ok(()) => {
                                self.committed = true;
                                Err(io::Error::new(
                                    original.kind(),
                                    format!(
                                        "{original}; exact commit-state resolution failed: {resolution}; typed fixture restored to the exact pre-state"
                                    ),
                                ))
                            }
                            Err(rollback) => Err(io::Error::new(
                                original.kind(),
                                format!(
                                    "{original}; exact commit-state resolution failed: {resolution}; typed pre-state restore also failed: {rollback}"
                                ),
                            )),
                        },
                    },
                }
            }
        }
    }

    fn finish_commit_publication(
        &mut self,
        record: &CommitRecord,
        inject_public_failpoints: bool,
    ) -> io::Result<RecoveryState> {
        let generation_dir = self.generation_dir();
        let published_dir = self.published_generation_dir();
        let binding = PendingGenerationIdentity {
            generation: self.generation,
            transaction_id: self.transaction_id.clone(),
            operation: self.operation,
            intent_hash: self.intent_hash.clone(),
        };
        let publication_carrier = find_publication_carrier(&self.root, &binding)?;
        let encoded_commit = encode_record(record)?;
        let state = RecoveryState {
            generation: self.generation,
            commit_hash: hash_hex(&encoded_commit),
            legacy: false,
        };

        if path_entry_exists(&published_dir)? {
            if path_entry_exists(&generation_dir)? || publication_carrier.is_some() {
                return Err(invalid_data(
                    "incomplete and published carriers coexist during commit resolution",
                ));
            }
            validate_generation_layout(&self.root, &published_dir)?;
            if read_bytes_bounded_under(
                &self.root,
                &published_dir.join(COMMIT_FILE_NAME),
                DEFAULT_LIMITS.max_record_bytes,
            )? != encoded_commit
            {
                return Err(invalid_data(
                    "published commit carrier does not contain the bound commit record",
                ));
            }
            sync_directory(&generations_dir(&self.root))?;
            return Ok(state);
        }
        let source_dir = match (path_entry_exists(&generation_dir)?, publication_carrier) {
            (true, None) => generation_dir,
            (false, Some(carrier)) => carrier,
            (true, Some(_)) => {
                return Err(invalid_data(
                    "pending and private publication carriers coexist",
                ));
            }
            (false, None) => {
                return Err(invalid_data(
                    "commit resolution found neither pending nor private publication carrier",
                ));
            }
        };
        if !path_entry_exists(&source_dir)? {
            return Err(invalid_data(
                "commit resolution found neither pending nor published carrier",
            ));
        }

        #[cfg(windows)]
        let generations_guard = AncestorGuard::acquire(&self.root, &generations_dir(&self.root))
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("commit resolution could not pin generations: {error}"),
                )
            })?;
        let pending_guard = AncestorGuard::acquire(&self.root, &source_dir).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("commit resolution could not pin pending carrier: {error}"),
            )
        })?;
        #[cfg(windows)]
        let pending_identity =
            stable_identity(pending_guard.directory_handle()?).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("commit resolution could not identify pending carrier: {error}"),
                )
            })?;
        let commit_temp = source_dir.join(COMMIT_TEMP_FILE_NAME);
        let commit_target = source_dir.join(COMMIT_FILE_NAME);
        match (
            path_entry_exists(&commit_temp)?,
            path_entry_exists(&commit_target)?,
        ) {
            (true, false) => {
                atomic_rename_under(
                    &self.root,
                    &commit_temp,
                    &commit_target,
                    &mut self.failpoint,
                )
                .map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("commit-record handle publication failed: {error}"),
                    )
                })?;
                if inject_public_failpoints {
                    self.hit(OperationStage::AfterCommitRename)?;
                }
                sync_directory(&source_dir)?;
            }
            (false, true) => {
                let found = read_bytes_bounded_under(
                    &self.root,
                    &commit_target,
                    DEFAULT_LIMITS.max_record_bytes,
                )
                .map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("commit resolution could not reopen commit.bin: {error}"),
                    )
                })?;
                if found != encoded_commit {
                    return Err(invalid_data(
                        "pending carrier contains a conflicting commit record",
                    ));
                }
            }
            _ => {
                return Err(invalid_data(
                    "pending carrier has an ambiguous commit-record publication form",
                ));
            }
        }
        pending_guard.verify().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("pending carrier pin verification failed: {error}"),
            )
        })?;
        if inject_public_failpoints {
            self.hit(OperationStage::BeforeCommitPublish)?;
            self.hit(OperationStage::BeforeGenerationPublish)?;
        }
        validate_pending_generation_layout(
            &self.root,
            &source_dir,
            &binding,
            &RecoveryState {
                generation: self.generation.saturating_sub(1),
                commit_hash: self.parent_commit_hash.clone(),
                legacy: false,
            },
        )
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("pending carrier validation failed: {error}"),
            )
        })?;
        validate_future_generation_tree_budget(
            &self.root,
            &source_dir,
            &published_dir,
            &self.limits,
        )?;
        // The validation guard pins the pending object through all public
        // pre-publication hooks. It must then close before the same directory
        // can be reopened with DELETE access. The handle-bound helper performs
        // a fresh final identity/link check and retains that mutation handle
        // continuously through its internal seam and rename syscall.
        drop(pending_guard);
        #[cfg(windows)]
        {
            publish_generation_directory_guarded(
                &self.root,
                generations_guard,
                &source_dir,
                &published_dir,
                &pending_identity,
                &binding,
                &mut self.failpoint,
            )
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("generation carrier publication failed: {error}"),
                )
            })?;
            validate_pending_generation_layout(
                &self.root,
                &published_dir,
                &binding,
                &RecoveryState {
                    generation: self.generation.saturating_sub(1),
                    commit_hash: self.parent_commit_hash.clone(),
                    legacy: false,
                },
            )
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("published generation validation failed: {error}"),
                )
            })?;
            if inject_public_failpoints {
                self.hit(OperationStage::AfterGenerationPublish)?;
            }
            sync_directory(&generations_dir(&self.root))?;
            if inject_public_failpoints {
                self.hit(OperationStage::AfterGenerationPublishParentSync)?;
                self.hit(OperationStage::AfterCommitParentSync)?;
            }
            Ok(state)
        }
        #[cfg(not(windows))]
        {
            let _ = state;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private generation publication is unsupported on this platform",
            ))
        }
    }

    /// Leave a prepared generation invisible. Recovery will ignore it.
    pub(crate) fn abandon(mut self) {
        self.committed = true;
    }

    #[cfg(test)]
    fn generation(&self) -> u64 {
        self.generation
    }

    fn ensure_format(&mut self) -> io::Result<()> {
        let format_path = transaction_dir(&self.root).join(FORMAT_FILE_NAME);
        if path_entry_exists(&format_path)? {
            return validate_format(&read_record_under::<FormatRecord>(
                &self.root,
                &format_path,
                "format",
            )?);
        }
        Err(invalid_data(
            "transaction writes require explicit typed baseline activation",
        ))
    }

    fn generation_dir(&self) -> PathBuf {
        self.pending_generation_dir()
    }

    fn published_generation_dir(&self) -> PathBuf {
        generations_dir(&self.root).join(generation_name(self.generation))
    }

    fn pending_generation_dir(&self) -> PathBuf {
        generations_dir(&self.root).join(pending_generation_name(
            self.generation,
            &self.transaction_id,
            self.operation,
            &self.intent_hash,
        ))
    }

    fn hit(&mut self, stage: OperationStage) -> io::Result<()> {
        hit_failpoint(&mut self.failpoint, &mut self.occurrences, stage)
    }

    fn hit_at(&mut self, stage: OperationStage, occurrence: usize) -> io::Result<()> {
        hit_failpoint_at(&mut self.failpoint, stage, occurrence)
    }

    fn cleanup_incomplete_generation(&mut self, path: &Path) -> io::Result<()> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid_data("incomplete generation name is not canonical UTF-8"))?;
        let expected_binding = PendingGenerationIdentity {
            generation: self.generation,
            transaction_id: self.transaction_id.clone(),
            operation: self.operation,
            intent_hash: self.intent_hash.clone(),
        };
        if name.starts_with(PENDING_GENERATION_PREFIX) {
            let identity = parse_pending_generation_name(name)?;
            if identity != expected_binding {
                return Err(invalid_data(
                    "pending generation cleanup identity does not match retry",
                ));
            }
            let parent_state = RecoveryState {
                generation: self.generation.saturating_sub(1),
                commit_hash: self.parent_commit_hash.clone(),
                legacy: false,
            };
            validate_pending_generation_layout(&self.root, path, &identity, &parent_state)?;
        } else if name.starts_with(PUBLICATION_GENERATION_PREFIX) {
            let (_, identity) = validate_publication_carrier_identity(path)?;
            if identity != expected_binding {
                return Err(invalid_data(
                    "publication carrier cleanup identity does not match retry",
                ));
            }
            let parent_state = RecoveryState {
                generation: self.generation.saturating_sub(1),
                commit_hash: self.parent_commit_hash.clone(),
                legacy: false,
            };
            validate_pending_generation_layout(&self.root, path, &identity, &parent_state)?;
        } else if name.starts_with(CLEANUP_GENERATION_PREFIX) {
            let (_, binding) =
                validate_cleanup_tombstone_identity(path, CleanupNamespaceKind::Generation)?;
            if binding != CleanupBinding::Generation(expected_binding.clone()) {
                return Err(invalid_data(
                    "generation cleanup tombstone identity does not match retry",
                ));
            }
            validate_cleanup_tree_subset(&self.root, path, CleanupNamespaceKind::Generation)?;
        } else {
            validate_generation_layout(&self.root, path)?;
        }
        let cleanup_identity = stable_identity(&open_directory_no_follow(path)?)?;
        let cleanup_parent = path
            .parent()
            .ok_or_else(|| invalid_data("incomplete generation has no cleanup parent"))?;
        if cleanup_parent != generations_dir(&self.root)
            && cleanup_parent != publication_dir(&self.root)
        {
            return Err(invalid_data(
                "incomplete generation escaped its bounded cleanup namespace",
            ));
        }
        self.hit(OperationStage::CleanupBeforeRemove)?;
        remove_verified_directory_tree_guarded(
            &self.root,
            cleanup_parent,
            path,
            &cleanup_identity,
            CleanupNamespaceKind::Generation,
            &CleanupBinding::Generation(expected_binding),
            &mut self.failpoint,
        )?;
        self.hit(OperationStage::CleanupAfterRemove)?;
        sync_directory(cleanup_parent)?;
        if cleanup_parent != generations_dir(&self.root) {
            sync_directory(&generations_dir(&self.root))?;
        }
        self.hit(OperationStage::CleanupAfterParentSync)
    }
}

impl Drop for OperationTransaction {
    fn drop(&mut self) {
        // A dropped prepared generation is deliberately retained for recovery to
        // classify as invisible; deleting it would make crash testing ambiguous.
    }
}

impl FormatRecord {
    fn new() -> io::Result<Self> {
        let mut record = Self {
            magic: FORMAT_MAGIC.to_owned(),
            version: FORMAT_VERSION,
            codec: PRIVATE_CODEC_ID.to_owned(),
            component_registry: COMPONENT_REGISTRY_ID.to_owned(),
            limits: LIMITS_ID.to_owned(),
            crc32: 0,
        };
        record.crc32 = format_crc(&record)?;
        Ok(record)
    }
}

impl PrepareRecord {
    fn new(body: PrepareBody) -> io::Result<Self> {
        let crc32 = record_crc("prepare", &body)?;
        Ok(Self { body, crc32 })
    }
}

impl CommitRecord {
    fn new(body: CommitBody) -> io::Result<Self> {
        let crc32 = record_crc("commit", &body)?;
        Ok(Self { body, crc32 })
    }
}

impl BaselineRecord {
    fn new(body: BaselineBody) -> io::Result<Self> {
        let crc32 = record_crc("baseline", &body)?;
        Ok(Self { body, crc32 })
    }
}

impl MigrationRecord {
    fn new(body: MigrationBody) -> io::Result<Self> {
        let crc32 = record_crc("migration", &body)?;
        Ok(Self { body, crc32 })
    }
}

impl CommitBody {
    const fn operation(&self) -> OperationKind {
        self.operation
    }
}

/// Create and publish an immutable checkpoint for a verified legacy
/// generation-zero base.
///
/// The caller must hold the base's exclusive writer lease. This function is
/// intentionally not wired into `MemoryX::new`; the first activation packet
/// exercises it only on disposable fixtures.
pub(crate) fn create_legacy_baseline(
    lease: &ExclusiveBaselineLease,
) -> io::Result<BaselineMigrationResult> {
    create_legacy_baseline_with_failpoint(lease, None)
}

pub(crate) fn create_legacy_baseline_with_failpoint(
    lease: &ExclusiveBaselineLease,
    failpoint: Option<Box<dyn OperationFailpoint>>,
) -> io::Result<BaselineMigrationResult> {
    create_legacy_baseline_with_options(lease, failpoint, DEFAULT_LIMITS, None)
}

fn create_legacy_baseline_with_options(
    lease: &ExclusiveBaselineLease,
    mut failpoint: Option<Box<dyn OperationFailpoint>>,
    limits: ResourceLimits,
    available_space_override: Option<u64>,
) -> io::Result<BaselineMigrationResult> {
    ensure_private_mutation_supported()?;
    lease.verify()?;
    let root = lease.root();
    if !root.is_dir() {
        return Err(invalid_data(
            "legacy baseline requires an existing base directory",
        ));
    }

    let format_path = transaction_dir(root).join(FORMAT_FILE_NAME);
    if path_entry_exists(&format_path)? {
        let state = OperationTransaction::recover(root)?;
        let baseline = validate_baseline(root, false)?.ok_or_else(|| {
            invalid_data("operation transaction format has no legacy baseline checkpoint")
        })?;
        let migration = validate_migration(root, &baseline)?;
        return Ok(baseline_result(
            BaselineMigrationStatus::AlreadyMigrated,
            state,
            &baseline,
            &migration,
        ));
    }

    validate_interrupted_activation_layout(root)?;
    if let Some(tombstone) = find_baseline_cleanup_tombstone(root)? {
        let (cleanup_identity, binding) =
            validate_cleanup_tombstone_identity(&tombstone, CleanupNamespaceKind::Baseline)?;
        if binding != CleanupBinding::Baseline {
            return Err(invalid_data(
                "baseline cleanup tombstone has a generation binding",
            ));
        }
        validate_cleanup_tree_subset(root, &tombstone, CleanupNamespaceKind::Baseline)?;
        remove_verified_directory_tree_guarded(
            root,
            &transaction_dir(root),
            &tombstone,
            &cleanup_identity,
            CleanupNamespaceKind::Baseline,
            &CleanupBinding::Baseline,
            &mut failpoint,
        )?;
        sync_directory(&transaction_dir(root))?;
        validate_interrupted_activation_layout(root)?;
    }
    if path_entry_exists(&baseline_dir(root))? {
        let baseline = validate_baseline(root, true)?
            .ok_or_else(|| invalid_data("published legacy baseline checkpoint is unavailable"))?;
        let available = available_space_override.unwrap_or(fs2::available_space(root)?);
        let migration = if path_entry_exists(&transaction_dir(root).join(MIGRATION_FILE_NAME))? {
            validate_migration(root, &baseline)?
        } else {
            validate_and_reuse_migration_preflight(root, &baseline, available)?;
            publish_migration_record(root, &baseline, &mut failpoint)?
        };
        ensure_generation_directory(root, &mut failpoint)?;
        publish_format_record(root, &mut failpoint)?;
        let state = RecoveryState {
            generation: 0,
            commit_hash: empty_hash(),
            legacy: false,
        };
        return Ok(baseline_result(
            BaselineMigrationStatus::Resumed,
            state,
            &baseline,
            &migration,
        ));
    }

    let mut occurrences = BTreeMap::new();
    hit_failpoint(
        &mut failpoint,
        &mut occurrences,
        OperationStage::BaselineBeforeScan,
    )?;
    let source_inventory = collect_registered_inventory(root, &limits)?;
    let source_components = source_inventory
        .iter()
        .map(|component| component.descriptor.clone())
        .collect::<Vec<_>>();
    validate_fixture_projection_from_inventory(&source_inventory)?;
    let total_bytes = checked_component_total(&source_components, &limits)?;
    let required_copy_bytes = total_bytes
        .checked_add(COPY_SPACE_OVERHEAD_BYTES)
        .ok_or_else(|| invalid_data("baseline copy-space calculation overflow"))?;
    let available_copy_bytes = available_space_override.unwrap_or(fs2::available_space(root)?);
    if available_copy_bytes < required_copy_bytes {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "insufficient free space for immutable baseline and metadata",
        ));
    }
    let logical_state_digest = canonical_logical_state_digest(&source_components)?;
    hit_failpoint(
        &mut failpoint,
        &mut occurrences,
        OperationStage::BaselineAfterScan,
    )?;
    lease.verify()?;

    let record = BaselineRecord::new(BaselineBody {
        magic: BASELINE_MAGIC.to_owned(),
        version: FORMAT_VERSION,
        codec: PRIVATE_CODEC_ID.to_owned(),
        component_registry: COMPONENT_REGISTRY_ID.to_owned(),
        source_generation: 0,
        components: source_components,
        logical_state_digest,
        downgrade_guard: DOWNGRADE_GUARD_ID.to_owned(),
    })?;

    let transaction_directory = transaction_dir(root);
    hit_failpoint(
        &mut failpoint,
        &mut occurrences,
        OperationStage::BaselineBeforeControlCreate,
    )?;
    if !path_entry_exists(&transaction_directory)? {
        create_directory_guarded(root, root, &transaction_directory)?;
    }
    hit_failpoint(
        &mut failpoint,
        &mut occurrences,
        OperationStage::BaselineAfterControlCreate,
    )?;
    sync_directory(&transaction_directory)?;
    hit_failpoint(
        &mut failpoint,
        &mut occurrences,
        OperationStage::BaselineAfterControlSync,
    )?;
    sync_directory(root)?;
    hit_failpoint(
        &mut failpoint,
        &mut occurrences,
        OperationStage::BaselineAfterRootSync,
    )?;
    ensure_migration_preflight(root, &record, available_copy_bytes, &mut failpoint)?;
    let staging = baseline_temp_dir(root);
    hit_failpoint(
        &mut failpoint,
        &mut occurrences,
        OperationStage::BaselineBeforeStagingCreate,
    )?;
    if path_entry_exists(&staging)? {
        validate_baseline_temp_layout(root, &staging)?;
        let cleanup_identity = stable_identity(&open_directory_no_follow(&staging)?)?;
        hit_failpoint(
            &mut failpoint,
            &mut occurrences,
            OperationStage::CleanupBeforeRemove,
        )?;
        remove_verified_directory_tree_guarded(
            root,
            &transaction_directory,
            &staging,
            &cleanup_identity,
            CleanupNamespaceKind::Baseline,
            &CleanupBinding::Baseline,
            &mut failpoint,
        )?;
        hit_failpoint(
            &mut failpoint,
            &mut occurrences,
            OperationStage::CleanupAfterRemove,
        )?;
        sync_directory(&transaction_directory)?;
        hit_failpoint(
            &mut failpoint,
            &mut occurrences,
            OperationStage::CleanupAfterParentSync,
        )?;
    }
    let staging_components = staging.join(COMPONENTS_DIR_NAME);
    create_directory_guarded(root, &transaction_directory, &staging)?;
    sync_directory(&staging)?;
    sync_directory(&transaction_directory)?;
    hit_failpoint(
        &mut failpoint,
        &mut occurrences,
        OperationStage::BaselineAfterStagingCreate,
    )?;
    hit_failpoint(
        &mut failpoint,
        &mut occurrences,
        OperationStage::BaselineBeforeComponentsCreate,
    )?;
    create_directory_guarded(root, &staging, &staging_components)?;
    hit_failpoint(
        &mut failpoint,
        &mut occurrences,
        OperationStage::BaselineAfterComponentsCreate,
    )?;
    sync_directory(&staging_components)?;
    hit_failpoint(
        &mut failpoint,
        &mut occurrences,
        OperationStage::BaselineAfterComponentsSync,
    )?;
    sync_directory(&staging)?;
    hit_failpoint(
        &mut failpoint,
        &mut occurrences,
        OperationStage::BaselineAfterStagingSync,
    )?;

    for (occurrence, component) in source_inventory.iter().enumerate() {
        hit_failpoint_at(
            &mut failpoint,
            OperationStage::BaselineBeforeComponentOpen,
            occurrence,
        )?;
        let target = staging_components.join(Path::new(&component.descriptor.relative_path));
        let target_parent = target
            .parent()
            .ok_or_else(|| io::Error::other("baseline component has no parent"))?;
        hit_failpoint_at(
            &mut failpoint,
            OperationStage::BaselineBeforeComponentParentCreate,
            occurrence,
        )?;
        if !path_entry_exists(target_parent)? {
            create_directory_guarded(root, &staging_components, target_parent)?;
        }
        hit_failpoint_at(
            &mut failpoint,
            OperationStage::BaselineAfterComponentParentCreate,
            occurrence,
        )?;
        sync_directory(target_parent)?;
        hit_failpoint_at(
            &mut failpoint,
            OperationStage::BaselineAfterComponentParentSync,
            occurrence,
        )?;
        sync_directory(&staging_components)?;
        hit_failpoint_at(
            &mut failpoint,
            OperationStage::BaselineAfterComponentsParentSync,
            occurrence,
        )?;
        copy_verified_component(
            root,
            component,
            &target,
            &mut failpoint,
            occurrence,
            &limits,
        )?;
        sync_directory(target_parent)?;
        hit_failpoint_at(
            &mut failpoint,
            OperationStage::BaselineAfterComponentSync,
            occurrence,
        )?;
    }
    lease.verify()?;

    hit_failpoint(
        &mut failpoint,
        &mut occurrences,
        OperationStage::BaselineBeforeManifestWrite,
    )?;
    write_new_record_with_failpoints(
        root,
        &staging.join(BASELINE_MANIFEST_FILE_NAME),
        &record,
        &mut failpoint,
        &mut occurrences,
        [
            OperationStage::BaselineAfterManifestWrite,
            OperationStage::BaselineAfterManifestFlush,
            OperationStage::BaselineAfterManifestSync,
        ],
    )?;
    sync_directory(&staging)?;
    hit_failpoint(
        &mut failpoint,
        &mut occurrences,
        OperationStage::BaselineAfterManifestDirectorySync,
    )?;
    hit_failpoint(
        &mut failpoint,
        &mut occurrences,
        OperationStage::BaselineBeforePublish,
    )?;
    atomic_rename_under(root, &staging, &baseline_dir(root), &mut failpoint)?;
    hit_failpoint(
        &mut failpoint,
        &mut occurrences,
        OperationStage::BaselineAfterRename,
    )?;
    sync_directory(&transaction_directory)?;
    hit_failpoint(
        &mut failpoint,
        &mut occurrences,
        OperationStage::BaselineAfterParentSync,
    )?;

    let baseline = validate_baseline(root, false)?
        .ok_or_else(|| invalid_data("published legacy baseline checkpoint is unavailable"))?;
    let migration = publish_migration_record(root, &baseline, &mut failpoint)?;
    ensure_generation_directory(root, &mut failpoint)?;
    publish_format_record(root, &mut failpoint)?;
    let state = RecoveryState {
        generation: 0,
        commit_hash: empty_hash(),
        legacy: false,
    };
    Ok(baseline_result(
        BaselineMigrationStatus::Created,
        state,
        &baseline,
        &migration,
    ))
}

/// Gate for a legacy persistence writer. Once activation artifacts exist, a
/// writer that cannot validate operation generations must refuse the base.
///
/// Historical binaries do not call this function; production enforcement is a
/// shared-surface follow-up rather than evidence supplied by this owned module.
pub(crate) fn require_legacy_writer_compatible(root: &Path) -> io::Result<()> {
    let directory = transaction_dir(root);
    if path_entry_exists(&directory)? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "{DOWNGRADE_GUARD_ID}: legacy writer refuses a transaction-activated or partially activated base"
            ),
        ));
    }
    Ok(())
}

fn baseline_result(
    status: BaselineMigrationStatus,
    state: RecoveryState,
    baseline: &BaselineRecord,
    migration: &MigrationRecord,
) -> BaselineMigrationResult {
    BaselineMigrationResult {
        status,
        state,
        logical_state_digest: baseline.body.logical_state_digest.clone(),
        component_count: baseline.body.components.len(),
        total_bytes: migration.body.total_bytes,
        available_copy_bytes: migration.body.available_copy_bytes,
        rollback_policy: migration.body.rollback_policy.clone(),
    }
}

fn transaction_dir(root: &Path) -> PathBuf {
    root.join(TXN_DIR_NAME)
}

fn generations_dir(root: &Path) -> PathBuf {
    transaction_dir(root).join(GENERATIONS_DIR_NAME)
}

fn publication_dir(root: &Path) -> PathBuf {
    generations_dir(root).join(PUBLICATION_DIRECTORY_NAME)
}

fn baseline_dir(root: &Path) -> PathBuf {
    transaction_dir(root).join(BASELINE_DIR_NAME)
}

fn baseline_temp_dir(root: &Path) -> PathBuf {
    transaction_dir(root).join(BASELINE_TEMP_DIR_NAME)
}

fn find_baseline_cleanup_tombstone(root: &Path) -> io::Result<Option<PathBuf>> {
    let directory = transaction_dir(root);
    if !path_entry_exists(&directory)? {
        return Ok(None);
    }
    let tombstones = read_directory_bounded(&directory, &DEFAULT_LIMITS)?
        .into_iter()
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(CLEANUP_BASELINE_PREFIX))
                .then(|| entry.path())
        })
        .collect::<Vec<_>>();
    match tombstones.as_slice() {
        [] => Ok(None),
        [path] => Ok(Some(path.clone())),
        _ => Err(invalid_data(
            "more than one baseline cleanup tombstone exists",
        )),
    }
}

fn find_publication_carrier(
    root: &Path,
    binding: &PendingGenerationIdentity,
) -> io::Result<Option<PathBuf>> {
    let directory = publication_dir(root);
    if !path_entry_exists(&directory)? {
        return Ok(None);
    }
    let mut found = None;
    for entry in read_directory_bounded(&directory, &DEFAULT_LIMITS)? {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid_data("publication carrier name is not canonical UTF-8"))?;
        if !name.starts_with(PUBLICATION_GENERATION_PREFIX) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_dir() || is_link_or_reparse(&entry.path(), &metadata) {
            return Err(invalid_data(
                "publication carrier is not an ordinary directory",
            ));
        }
        let (_, found_binding) = validate_publication_carrier_identity(&entry.path())?;
        if &found_binding != binding || found.replace(entry.path()).is_some() {
            return Err(invalid_data(
                "publication carrier set conflicts with the active transaction",
            ));
        }
    }
    Ok(found)
}

fn ensure_generation_directory(
    root: &Path,
    failpoint: &mut Option<Box<dyn OperationFailpoint>>,
) -> io::Result<()> {
    let directory = transaction_dir(root);
    let mut occurrences = BTreeMap::new();
    hit_failpoint(
        failpoint,
        &mut occurrences,
        OperationStage::BeforeGenerationsCreate,
    )?;
    if !path_entry_exists(&generations_dir(root))? {
        create_directory_guarded(root, &directory, &generations_dir(root))?;
    } else {
        reject_link_or_reparse(&generations_dir(root))?;
        if !generations_dir(root).is_dir() {
            return Err(invalid_data("generations control path is not a directory"));
        }
    }
    hit_failpoint(
        failpoint,
        &mut occurrences,
        OperationStage::AfterGenerationsCreate,
    )?;
    sync_directory(&generations_dir(root))?;
    hit_failpoint(
        failpoint,
        &mut occurrences,
        OperationStage::AfterGenerationsSync,
    )?;
    sync_directory(&directory)?;
    hit_failpoint(
        failpoint,
        &mut occurrences,
        OperationStage::AfterGenerationsControlSync,
    )
}

fn publish_format_record(
    root: &Path,
    failpoint: &mut Option<Box<dyn OperationFailpoint>>,
) -> io::Result<()> {
    let mut occurrences = BTreeMap::new();
    let directory = transaction_dir(root);
    if !directory.is_dir() {
        return Err(invalid_data(
            "format publication requires the existing transaction-control directory",
        ));
    }
    if !generations_dir(root).is_dir() {
        return Err(invalid_data(
            "format publication requires a durable generations directory",
        ));
    }
    let target = directory.join(FORMAT_FILE_NAME);
    if path_entry_exists(&target)? {
        return validate_format(&read_record_under::<FormatRecord>(root, &target, "format")?);
    }
    let temporary = directory.join(FORMAT_TEMP_FILE_NAME);
    validate_ready_for_format_publication(root, path_entry_exists(&temporary)?)?;
    if !path_entry_exists(&temporary)? {
        hit_failpoint(
            failpoint,
            &mut occurrences,
            OperationStage::BeforeFormatWrite,
        )?;
        write_new_record_with_failpoints(
            root,
            &temporary,
            &FormatRecord::new()?,
            failpoint,
            &mut occurrences,
            [
                OperationStage::AfterFormatWrite,
                OperationStage::AfterFormatFlush,
                OperationStage::AfterFormatSync,
            ],
        )?;
    }
    validate_ready_for_format_publication(root, true)?;
    hit_failpoint(
        failpoint,
        &mut occurrences,
        OperationStage::BeforeFormatPublish,
    )?;
    atomic_rename_under(root, &temporary, &target, failpoint)?;
    hit_failpoint(
        failpoint,
        &mut occurrences,
        OperationStage::AfterFormatRename,
    )?;
    sync_directory(&directory)?;
    hit_failpoint(
        failpoint,
        &mut occurrences,
        OperationStage::AfterFormatParentSync,
    )
}

fn migration_record(
    baseline: &BaselineRecord,
    available_copy_bytes: u64,
) -> io::Result<MigrationRecord> {
    let component_count = u64::try_from(baseline.body.components.len())
        .map_err(|_| invalid_data("migration component count does not fit u64"))?;
    let total_bytes = checked_component_total(&baseline.body.components, &DEFAULT_LIMITS)?;
    let required_copy_bytes = total_bytes
        .checked_add(COPY_SPACE_OVERHEAD_BYTES)
        .ok_or_else(|| invalid_data("baseline copy-space calculation overflow"))?;
    MigrationRecord::new(MigrationBody {
        magic: "MEMORYX_N5_MIGRATION_REPORT".to_owned(),
        version: FORMAT_VERSION,
        codec: PRIVATE_CODEC_ID.to_owned(),
        source_layout: SOURCE_LAYOUT_ID.to_owned(),
        target_format: FORMAT_MAGIC.to_owned(),
        component_registry: COMPONENT_REGISTRY_ID.to_owned(),
        limits: LIMITS_ID.to_owned(),
        component_count,
        total_bytes,
        required_copy_bytes,
        available_copy_bytes,
        baseline_digest: baseline.body.logical_state_digest.clone(),
        rollback_policy: ROLLBACK_POLICY_ID.to_owned(),
        source_files_untouched: true,
    })
}

fn ensure_migration_preflight(
    root: &Path,
    baseline: &BaselineRecord,
    available_copy_bytes: u64,
    failpoint: &mut Option<Box<dyn OperationFailpoint>>,
) -> io::Result<MigrationRecord> {
    let directory = transaction_dir(root);
    let target = directory.join(MIGRATION_FILE_NAME);
    if path_entry_exists(&target)? {
        return validate_migration(root, baseline);
    }
    let temporary = directory.join(MIGRATION_TEMP_FILE_NAME);
    if path_entry_exists(&temporary)? {
        return validate_and_reuse_migration_preflight(root, baseline, available_copy_bytes);
    }
    let record = migration_record(baseline, available_copy_bytes)?;
    let mut occurrences = BTreeMap::new();
    hit_failpoint(
        failpoint,
        &mut occurrences,
        OperationStage::BeforeMigrationWrite,
    )?;
    write_new_record_with_failpoints(
        root,
        &temporary,
        &record,
        failpoint,
        &mut occurrences,
        [
            OperationStage::AfterMigrationWrite,
            OperationStage::AfterMigrationFlush,
            OperationStage::AfterMigrationSync,
        ],
    )?;
    sync_directory(&directory)?;
    hit_failpoint(
        failpoint,
        &mut occurrences,
        OperationStage::MigrationPreflightAfterParentSync,
    )?;
    Ok(record)
}

fn validate_and_reuse_migration_preflight(
    root: &Path,
    baseline: &BaselineRecord,
    currently_available: u64,
) -> io::Result<MigrationRecord> {
    let temporary = transaction_dir(root).join(MIGRATION_TEMP_FILE_NAME);
    let record = read_record_under::<MigrationRecord>(root, &temporary, "migration")?;
    validate_migration_record(&record, baseline)?;
    if currently_available < record.body.required_copy_bytes {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "current free space is below the resume-stable migration preflight requirement",
        ));
    }
    Ok(record)
}

fn publish_migration_record(
    root: &Path,
    baseline: &BaselineRecord,
    failpoint: &mut Option<Box<dyn OperationFailpoint>>,
) -> io::Result<MigrationRecord> {
    let directory = transaction_dir(root);
    let target = directory.join(MIGRATION_FILE_NAME);
    if path_entry_exists(&target)? {
        return validate_migration(root, baseline);
    }
    let temporary = directory.join(MIGRATION_TEMP_FILE_NAME);
    if !path_entry_exists(&temporary)? {
        return Err(invalid_data(
            "published baseline has no resume-stable migration preflight record",
        ));
    }
    let record = read_record_under::<MigrationRecord>(root, &temporary, "migration")?;
    validate_migration_record(&record, baseline)?;
    let mut occurrences = BTreeMap::new();
    hit_failpoint(
        failpoint,
        &mut occurrences,
        OperationStage::BeforeMigrationPublish,
    )?;
    atomic_rename_under(root, &temporary, &target, failpoint)?;
    hit_failpoint(
        failpoint,
        &mut occurrences,
        OperationStage::AfterMigrationRename,
    )?;
    sync_directory(&directory)?;
    hit_failpoint(
        failpoint,
        &mut occurrences,
        OperationStage::AfterMigrationParentSync,
    )?;
    Ok(record)
}

fn hit_failpoint(
    failpoint: &mut Option<Box<dyn OperationFailpoint>>,
    occurrences: &mut BTreeMap<OperationStage, usize>,
    stage: OperationStage,
) -> io::Result<()> {
    let occurrence = occurrences.entry(stage).or_insert(0);
    let current = *occurrence;
    *occurrence = occurrence
        .checked_add(1)
        .ok_or_else(|| invalid_data("failpoint occurrence overflow"))?;
    hit_failpoint_at(failpoint, stage, current)
}

fn hit_failpoint_at(
    failpoint: &mut Option<Box<dyn OperationFailpoint>>,
    stage: OperationStage,
    occurrence: usize,
) -> io::Result<()> {
    if let Some(failpoint) = failpoint {
        failpoint.hit(stage, occurrence)?;
    }
    Ok(())
}

fn generation_name(generation: u64) -> String {
    format!("{generation:020}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingGenerationIdentity {
    generation: u64,
    transaction_id: TransactionId,
    operation: OperationKind,
    intent_hash: String,
}

fn pending_generation_name(
    generation: u64,
    transaction_id: &TransactionId,
    operation: OperationKind,
    intent_hash: &str,
) -> String {
    format!(
        "{PENDING_GENERATION_PREFIX}{}--{}--{}--{intent_hash}",
        generation_name(generation),
        transaction_id.as_str(),
        operation.as_str()
    )
}

fn parse_pending_generation_name(name: &str) -> io::Result<PendingGenerationIdentity> {
    let suffix = name
        .strip_prefix(PENDING_GENERATION_PREFIX)
        .ok_or_else(|| invalid_data("pending generation name has no canonical prefix"))?;
    let fields = suffix.split("--").collect::<Vec<_>>();
    if fields.len() != 4 {
        return Err(invalid_data("pending generation name has invalid fields"));
    }
    let generation = parse_generation_name(fields[0])?;
    let transaction_id = TransactionId::parse(fields[1])?;
    let operation = OperationKind::parse(fields[2])?;
    if !is_hash(fields[3]) {
        return Err(invalid_data("pending generation intent hash is invalid"));
    }
    let identity = PendingGenerationIdentity {
        generation,
        transaction_id,
        operation,
        intent_hash: fields[3].to_owned(),
    };
    if pending_generation_name(
        identity.generation,
        &identity.transaction_id,
        identity.operation,
        &identity.intent_hash,
    ) != name
    {
        return Err(invalid_data("pending generation name is noncanonical"));
    }
    Ok(identity)
}

fn admitted_next_generation(visible_generation: u64, limits: &ResourceLimits) -> io::Result<u64> {
    let next = visible_generation
        .checked_add(1)
        .ok_or_else(|| invalid_data("operation transaction generation overflow"))?;
    let next_index = usize::try_from(next)
        .map_err(|_| invalid_data("operation transaction generation does not fit usize"))?;
    if next_index > limits.max_generations {
        return Err(invalid_data(
            "operation transaction generation exceeds the admitted v1 maximum",
        ));
    }
    Ok(next)
}

fn parse_generation_name(name: &str) -> io::Result<u64> {
    if name.len() != 20 || !name.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_data(
            "invalid operation transaction generation name",
        ));
    }
    let generation = name
        .parse::<u64>()
        .map_err(|_| invalid_data("invalid operation transaction generation number"))?;
    if generation == 0 || generation_name(generation) != name {
        return Err(invalid_data(
            "non-canonical operation transaction generation name",
        ));
    }
    Ok(generation)
}

enum TransactionIdStatus {
    Absent,
    Prepared {
        operation: OperationKind,
        intent_hash: String,
        generation_dir: PathBuf,
    },
    Committed {
        operation: OperationKind,
        intent_hash: String,
        state: RecoveryState,
    },
}

fn validate_pending_generation_layout(
    root: &Path,
    path: &Path,
    identity: &PendingGenerationIdentity,
    state: &RecoveryState,
) -> io::Result<()> {
    let _guard = AncestorGuard::acquire(root, path)?;
    let entries = read_directory_bounded(path, &DEFAULT_LIMITS)?;
    let mut names = BTreeSet::new();
    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid_data("pending generation artifact is not canonical UTF-8"))?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if is_link_or_reparse(&entry.path(), &metadata) {
            return Err(invalid_data(
                "pending generation contains a link or reparse point",
            ));
        }
        match name.as_str() {
            COMPONENTS_DIR_NAME if metadata.is_dir() => {
                validate_uncommitted_component_tree(&entry.path())?;
            }
            PREPARE_TEMP_FILE_NAME
            | PREPARE_FILE_NAME
            | COMMIT_TEMP_FILE_NAME
            | COMMIT_FILE_NAME
                if metadata.is_file() =>
            {
                validate_control_record_artifact(root, &entry.path(), "generation record")?;
            }
            _ => {
                return Err(invalid_data(
                    "pending generation contains an unknown artifact",
                ));
            }
        }
        names.insert(name);
    }
    if names.contains(PREPARE_FILE_NAME) && !names.contains(COMPONENTS_DIR_NAME) {
        return Err(invalid_data(
            "published pending prepare has no components directory",
        ));
    }
    if names.contains(PREPARE_FILE_NAME) && names.contains(PREPARE_TEMP_FILE_NAME) {
        return Err(invalid_data(
            "pending generation contains coexisting prepare publications",
        ));
    }
    if (names.contains(COMMIT_FILE_NAME) || names.contains(COMMIT_TEMP_FILE_NAME))
        && !names.contains(PREPARE_FILE_NAME)
    {
        return Err(invalid_data(
            "pending generation commit artifact has no published prepare",
        ));
    }
    if names.contains(COMMIT_FILE_NAME) && names.contains(COMMIT_TEMP_FILE_NAME) {
        return Err(invalid_data(
            "pending generation contains coexisting commit publications",
        ));
    }
    if names.contains(PREPARE_FILE_NAME) {
        let prepare = read_prepare(root, &path.join(PREPARE_FILE_NAME))?;
        validate_prepare(&prepare, identity.generation, state)?;
        if prepare.body.transaction_id != identity.transaction_id
            || prepare.body.operation != identity.operation
            || prepare.body.intent_hash != identity.intent_hash
        {
            return Err(invalid_data(
                "pending generation directory identity conflicts with prepare",
            ));
        }
        if names.contains(COMMIT_FILE_NAME) {
            let commit = read_record_under::<CommitRecord>(
                root,
                &path.join(COMMIT_FILE_NAME),
                "pending commit",
            )?;
            validate_commit(root, &commit, &prepare, identity.generation, state, path)?;
        }
    }
    Ok(())
}

fn validate_generation_layout(root: &Path, path: &Path) -> io::Result<()> {
    let _guard = AncestorGuard::acquire(root, path)?;
    reject_link_or_reparse(path)?;
    let entries = read_directory_bounded(path, &DEFAULT_LIMITS)?;
    let mut names = BTreeSet::new();
    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid_data("generation artifact name is not canonical UTF-8"))?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if is_link_or_reparse(&entry.path(), &metadata) {
            return Err(invalid_data(
                "generation tree contains a link or reparse point",
            ));
        }
        match name.as_str() {
            COMPONENTS_DIR_NAME if metadata.is_dir() => {}
            PREPARE_FILE_NAME
            | PREPARE_TEMP_FILE_NAME
            | COMMIT_FILE_NAME
            | COMMIT_TEMP_FILE_NAME
                if metadata.is_file() =>
            {
                validate_control_record_artifact(root, &entry.path(), "generation record")?;
            }
            _ => return Err(invalid_data("generation tree contains an unknown artifact")),
        }
        names.insert(name);
    }
    if !names.contains(COMPONENTS_DIR_NAME)
        || (names.contains(PREPARE_FILE_NAME) && names.contains(PREPARE_TEMP_FILE_NAME))
        || (names.contains(COMMIT_FILE_NAME) && names.contains(COMMIT_TEMP_FILE_NAME))
        || (names.contains(COMMIT_FILE_NAME) && !names.contains(PREPARE_FILE_NAME))
    {
        return Err(invalid_data("generation tree has an invalid recovery form"));
    }
    validate_uncommitted_component_tree(&path.join(COMPONENTS_DIR_NAME))
}

fn validate_uncommitted_component_tree(root: &Path) -> io::Result<()> {
    reject_link_or_reparse(root)?;
    let mut files = BTreeSet::new();
    let mut directories = BTreeSet::new();
    let mut budget = TreeBudget::default();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((directory, prefix)) = stack.pop() {
        for entry in read_directory_bounded(&directory, &DEFAULT_LIMITS)? {
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| invalid_data("component path is not canonical UTF-8"))?;
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            budget.observe(&relative, &DEFAULT_LIMITS)?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if is_link_or_reparse(&entry.path(), &metadata) {
                return Err(invalid_data(
                    "component tree contains a link or reparse point",
                ));
            }
            if metadata.is_dir() {
                if relative != "n5-fixture" || !directories.insert(relative.clone()) {
                    return Err(invalid_data(
                        "component tree contains an unregistered directory",
                    ));
                }
                stack.push((entry.path(), relative));
            } else if metadata.is_file() {
                registry_kind(&relative)?;
                if !files.insert(relative) || files.len() > DEFAULT_LIMITS.max_component_count {
                    return Err(invalid_data("component tree is duplicate or over limit"));
                }
            } else {
                return Err(invalid_data("component tree contains a non-file entry"));
            }
        }
    }
    if !files.is_empty() && directories != BTreeSet::from(["n5-fixture".to_owned()]) {
        return Err(invalid_data(
            "component tree file set is missing its registered directory",
        ));
    }
    Ok(())
}

fn validate_interrupted_activation_layout(root: &Path) -> io::Result<()> {
    let directory = transaction_dir(root);
    if !path_entry_exists(&directory)? {
        return Ok(());
    }
    if !directory.is_dir() {
        return Err(invalid_data(
            "operation transaction control path is not a directory",
        ));
    }
    let _guard = AncestorGuard::acquire(root, &directory)?;
    validate_cumulative_tree_budget(root, &directory, &DEFAULT_LIMITS)?;

    let mut names = BTreeSet::new();
    for entry in read_directory_bounded(&directory, &DEFAULT_LIMITS)? {
        let metadata = fs::symlink_metadata(entry.path())?;
        if is_link_or_reparse(&entry.path(), &metadata) {
            return Err(invalid_data(
                "operation transaction control contains a link or reparse point",
            ));
        }
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| invalid_data("transaction-control name is not canonical UTF-8"))?;
        names.insert(name.to_owned());
        match name {
            FORMAT_TEMP_FILE_NAME => {
                if !entry.file_type()?.is_file() {
                    return Err(invalid_data(
                        "temporary operation transaction format is not a file",
                    ));
                }
                validate_control_record_artifact(root, &entry.path(), "temporary format record")?;
            }
            BASELINE_TEMP_DIR_NAME => {
                if !entry.file_type()?.is_dir() {
                    return Err(invalid_data("temporary legacy baseline is not a directory"));
                }
                validate_baseline_temp_layout(root, &entry.path())?;
            }
            name if name.starts_with(CLEANUP_BASELINE_PREFIX) => {
                if !entry.file_type()?.is_dir() {
                    return Err(invalid_data(
                        "legacy baseline cleanup tombstone is not a directory",
                    ));
                }
                validate_cleanup_tombstone_identity(&entry.path(), CleanupNamespaceKind::Baseline)?;
                validate_cleanup_tree_subset(root, &entry.path(), CleanupNamespaceKind::Baseline)?;
            }
            BASELINE_DIR_NAME => {
                if !entry.file_type()?.is_dir() {
                    return Err(invalid_data("legacy baseline is not a directory"));
                }
                validate_baseline(root, true)?;
            }
            MIGRATION_TEMP_FILE_NAME | MIGRATION_FILE_NAME => {
                if !entry.file_type()?.is_file() {
                    return Err(invalid_data("migration report artifact is not a file"));
                }
                validate_control_record_artifact(root, &entry.path(), "migration report")?;
                if name == MIGRATION_FILE_NAME && !path_entry_exists(&baseline_dir(root))? {
                    return Err(invalid_data(
                        "migration report exists without a published baseline",
                    ));
                }
            }
            GENERATIONS_DIR_NAME => {
                if !entry.file_type()?.is_dir()
                    || !read_directory_bounded(&entry.path(), &DEFAULT_LIMITS)?.is_empty()
                {
                    return Err(invalid_data(
                        "operation transaction generations exist without format.v1",
                    ));
                }
            }
            _ => {
                return Err(invalid_data(
                    "unknown operation transaction artifact exists without format.v1",
                ));
            }
        }
    }
    if names.contains(BASELINE_TEMP_DIR_NAME) && names.contains(BASELINE_DIR_NAME) {
        return Err(invalid_data(
            "temporary and published baseline checkpoints coexist",
        ));
    }
    let cleanup_count = names
        .iter()
        .filter(|name| name.starts_with(CLEANUP_BASELINE_PREFIX))
        .count();
    if cleanup_count > 1
        || (cleanup_count == 1
            && (names.contains(BASELINE_TEMP_DIR_NAME) || names.contains(BASELINE_DIR_NAME)))
    {
        return Err(invalid_data(
            "baseline cleanup tombstone coexists with another baseline carrier",
        ));
    }
    if names.contains(MIGRATION_TEMP_FILE_NAME) && names.contains(MIGRATION_FILE_NAME) {
        return Err(invalid_data(
            "temporary and published migration reports coexist",
        ));
    }
    Ok(())
}

fn validate_ready_for_format_publication(root: &Path, allow_format_temp: bool) -> io::Result<()> {
    let directory = transaction_dir(root);
    let _guard = AncestorGuard::acquire(root, &directory)?;
    let mut expected = BTreeMap::from([
        (BASELINE_DIR_NAME, true),
        (GENERATIONS_DIR_NAME, true),
        (MIGRATION_FILE_NAME, false),
    ]);
    if allow_format_temp {
        expected.insert(FORMAT_TEMP_FILE_NAME, false);
    }
    let entries = read_directory_bounded(&directory, &DEFAULT_LIMITS)?;
    if entries.len() != expected.len() {
        return Err(invalid_data(
            "format publication requires the exact ready transaction-control tree",
        ));
    }
    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid_data("ready-tree name is not canonical UTF-8"))?;
        let should_be_directory = expected
            .get(name.as_str())
            .ok_or_else(|| invalid_data("format ready tree contains an unknown artifact"))?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if is_link_or_reparse(&entry.path(), &metadata)
            || (*should_be_directory && !metadata.is_dir())
            || (!*should_be_directory && !metadata.is_file())
        {
            return Err(invalid_data(
                "format ready-tree artifact has an invalid type",
            ));
        }
    }
    if !read_directory_bounded(&generations_dir(root), &DEFAULT_LIMITS)?.is_empty() {
        return Err(invalid_data(
            "format publication requires an empty generations directory",
        ));
    }
    let baseline = validate_baseline(root, true)?
        .ok_or_else(|| invalid_data("format publication requires a valid baseline"))?;
    validate_migration(root, &baseline)?;
    if allow_format_temp {
        let record = read_record_under::<FormatRecord>(
            root,
            &directory.join(FORMAT_TEMP_FILE_NAME),
            "temporary format",
        )?;
        validate_format(&record)?;
    }
    Ok(())
}

fn validate_baseline_temp_layout(root: &Path, staging: &Path) -> io::Result<()> {
    let _guard = AncestorGuard::acquire(root, staging)?;
    reject_link_or_reparse(staging)?;
    let mut stack = vec![(staging.to_path_buf(), String::new())];
    let mut budget = TreeBudget::default();
    let mut files = BTreeSet::new();
    let mut directories = BTreeSet::new();
    while let Some((directory, prefix)) = stack.pop() {
        for entry in read_directory_bounded(&directory, &DEFAULT_LIMITS)? {
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| invalid_data("temporary baseline name is not canonical UTF-8"))?;
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            budget.observe(&relative, &DEFAULT_LIMITS)?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if is_link_or_reparse(&entry.path(), &metadata) {
                return Err(invalid_data(
                    "temporary baseline contains a link or reparse point",
                ));
            }
            if metadata.is_dir() {
                if relative != COMPONENTS_DIR_NAME
                    && relative != format!("{COMPONENTS_DIR_NAME}/n5-fixture")
                {
                    return Err(invalid_data(
                        "temporary baseline contains an unregistered directory",
                    ));
                }
                if !directories.insert(relative.clone()) {
                    return Err(invalid_data("temporary baseline directory is duplicate"));
                }
                stack.push((entry.path(), relative));
                continue;
            }
            if !metadata.is_file() {
                return Err(invalid_data(
                    "temporary baseline contains a non-file artifact",
                ));
            }
            let allowed_length = match relative.as_str() {
                BASELINE_MANIFEST_FILE_NAME => {
                    validate_control_record_artifact(
                        root,
                        &entry.path(),
                        "temporary baseline manifest",
                    )?;
                    DEFAULT_LIMITS.max_record_bytes
                }
                path if path == format!("{COMPONENTS_DIR_NAME}/{FIXTURE_STATE_PATH}") => {
                    FIXTURE_COMPONENT_MAX_BYTES
                }
                path if path == format!("{COMPONENTS_DIR_NAME}/{FIXTURE_HISTORY_PATH}") => {
                    FIXTURE_COMPONENT_MAX_BYTES
                }
                _ => {
                    return Err(invalid_data(
                        "temporary baseline contains an unregistered file",
                    ));
                }
            };
            if metadata.len() > allowed_length || !files.insert(relative) {
                return Err(invalid_data(
                    "temporary baseline file is duplicate or exceeds its limit",
                ));
            }
        }
    }
    for file in &files {
        let mut parent = Path::new(file).parent();
        while let Some(path) = parent.filter(|path| !path.as_os_str().is_empty()) {
            let normalized = normalize_component_path(path)?;
            if !directories.contains(&normalized) {
                return Err(invalid_data(
                    "temporary baseline file is missing a registered ancestor",
                ));
            }
            parent = path.parent();
        }
    }
    Ok(())
}

fn validate_formatted_control_layout(root: &Path) -> io::Result<()> {
    let directory = transaction_dir(root);
    let _guard = AncestorGuard::acquire(root, &directory)?;
    validate_cumulative_tree_budget(root, &directory, &DEFAULT_LIMITS)?;
    reject_link_or_reparse(&directory)?;
    let expected = BTreeMap::from([
        (BASELINE_DIR_NAME, true),
        (FORMAT_FILE_NAME, false),
        (GENERATIONS_DIR_NAME, true),
        (MIGRATION_FILE_NAME, false),
    ]);
    let entries = read_directory_bounded(&directory, &DEFAULT_LIMITS)?;
    if entries.len() != expected.len() {
        return Err(invalid_data(
            "formatted transaction-control tree has extra or missing artifacts",
        ));
    }
    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid_data("transaction-control name is not canonical UTF-8"))?;
        let should_be_directory = expected.get(name.as_str()).ok_or_else(|| {
            invalid_data("formatted transaction-control tree contains an unknown artifact")
        })?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if is_link_or_reparse(&entry.path(), &metadata)
            || (*should_be_directory && !metadata.is_dir())
            || (!*should_be_directory && !metadata.is_file())
        {
            return Err(invalid_data(
                "formatted transaction-control artifact has an invalid type",
            ));
        }
    }
    Ok(())
}

fn validate_baseline(root: &Path, compare_live: bool) -> io::Result<Option<BaselineRecord>> {
    let directory = baseline_dir(root);
    if !path_entry_exists(&directory)? {
        return Ok(None);
    }
    if !directory.is_dir() {
        return Err(invalid_data("legacy baseline path is not a directory"));
    }
    let _guard = AncestorGuard::acquire(root, &directory)?;

    let mut names = read_directory_bounded(&directory, &DEFAULT_LIMITS)?
        .into_iter()
        .map(|entry| entry.file_name())
        .collect::<Vec<_>>();
    names.sort();
    let expected_names = [
        std::ffi::OsString::from(COMPONENTS_DIR_NAME),
        std::ffi::OsString::from(BASELINE_MANIFEST_FILE_NAME),
    ];
    if names != expected_names {
        return Err(invalid_data(
            "legacy baseline contains an unknown or missing artifact",
        ));
    }

    let record = read_record_under::<BaselineRecord>(
        root,
        &directory.join(BASELINE_MANIFEST_FILE_NAME),
        "baseline",
    )?;
    if record.body.magic != BASELINE_MAGIC
        || record.body.version != FORMAT_VERSION
        || record.body.codec != PRIVATE_CODEC_ID
        || record.body.component_registry != COMPONENT_REGISTRY_ID
        || record.body.source_generation != 0
        || record.body.downgrade_guard != DOWNGRADE_GUARD_ID
    {
        return Err(invalid_data("unsupported legacy baseline record"));
    }
    if record.crc32 != record_crc("baseline", &record.body)? {
        return Err(invalid_data("legacy baseline CRC mismatch"));
    }
    if record.body.components.is_empty() {
        return Err(invalid_data("legacy baseline has no persistent components"));
    }

    validate_component_descriptors(&record.body.components)?;
    let stored_components = collect_exact_component_tree(
        root,
        &directory.join(COMPONENTS_DIR_NAME),
        &record.body.components,
        &DEFAULT_LIMITS,
    )?;
    if stored_components != record.body.components {
        return Err(invalid_data(
            "legacy baseline component snapshot does not match its manifest",
        ));
    }
    let digest = canonical_logical_state_digest(&record.body.components)?;
    if digest != record.body.logical_state_digest {
        return Err(invalid_data(
            "legacy baseline logical-state digest mismatch",
        ));
    }
    if compare_live
        && collect_registered_descriptors(root, &DEFAULT_LIMITS)? != record.body.components
    {
        return Err(invalid_data(
            "legacy state changed after its immutable baseline was published",
        ));
    }
    Ok(Some(record))
}

fn validate_baseline_backed_live_state(
    root: &Path,
    visible_generation: u64,
    expected: &BTreeMap<String, ComponentSource>,
) -> io::Result<FixtureProjection> {
    let expected_descriptors = expected
        .values()
        .map(|source| source.descriptor.clone())
        .collect::<Vec<_>>();
    let live_inventory = collect_registered_inventory(root, &DEFAULT_LIMITS)?;
    let live = live_inventory
        .iter()
        .map(|component| component.descriptor.clone())
        .collect::<Vec<_>>();
    if live == expected_descriptors {
        let live_projection = fixture_projection_from_inventory(&live_inventory)?;
        let expected_projection = read_fixture_projection(expected)?;
        if live_projection != expected_projection {
            return Err(invalid_data(
                "live typed component projection differs from committed state",
            ));
        }
        return Ok(live_projection);
    }

    if visible_generation == 0 {
        return Err(invalid_data(
            "legacy state changed after its immutable baseline was published",
        ));
    }
    Err(invalid_data(
        "live state does not match the validated committed generation",
    ))
}

fn validate_component_descriptors(components: &[ComponentDescriptor]) -> io::Result<()> {
    validate_component_descriptors_with_limits(components, &DEFAULT_LIMITS)
}

fn validate_component_descriptors_with_limits(
    components: &[ComponentDescriptor],
    limits: &ResourceLimits,
) -> io::Result<()> {
    if components.is_empty() || components.len() > limits.max_component_count {
        return Err(invalid_data(
            "persistent component count is outside the N5-A bounds",
        ));
    }
    let mut previous: Option<&str> = None;
    for descriptor in components {
        if descriptor.relative_path
            != normalize_component_path(Path::new(&descriptor.relative_path))?
            || descriptor.relative_path.len() > limits.max_path_bytes
            || descriptor.length > limits.max_component_bytes
            || descriptor.kind.path() != descriptor.relative_path
            || !is_hash(&descriptor.blake3_hash)
            || previous >= Some(descriptor.relative_path.as_str())
        {
            return Err(invalid_data(
                "invalid or unsorted persistent component descriptor",
            ));
        }
        previous = Some(&descriptor.relative_path);
    }
    checked_component_total(components, limits).map(|_| ())
}

fn checked_component_total(
    components: &[ComponentDescriptor],
    limits: &ResourceLimits,
) -> io::Result<u64> {
    let total = components.iter().try_fold(0u64, |total, descriptor| {
        total
            .checked_add(descriptor.length)
            .ok_or_else(|| invalid_data("persistent component byte total overflow"))
    })?;
    if total > limits.max_aggregate_bytes {
        return Err(invalid_data(
            "persistent component aggregate bytes exceed the N5-A limit",
        ));
    }
    Ok(total)
}

fn collect_registered_descriptors(
    root: &Path,
    limits: &ResourceLimits,
) -> io::Result<Vec<ComponentDescriptor>> {
    Ok(collect_registered_inventory(root, limits)?
        .into_iter()
        .map(|component| component.descriptor)
        .collect())
}

fn collect_registered_inventory(
    root: &Path,
    limits: &ResourceLimits,
) -> io::Result<Vec<InventoryComponent>> {
    let _root_guard = AncestorGuard::acquire(root, root)?;
    reject_link_or_reparse(root)?;
    let mut saw_fixture_directory = false;
    for entry in read_directory_bounded(root, limits)? {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid_data("base entry name is not canonical UTF-8"))?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if is_link_or_reparse(&entry.path(), &metadata) {
            return Err(invalid_data(
                "base component registry rejects links and reparse points",
            ));
        }
        match name.as_str() {
            TXN_DIR_NAME => {
                if !metadata.is_dir() {
                    return Err(invalid_data("transaction control is not a directory"));
                }
            }
            CONTROL_FILE_NAME
            | CONTROL_TEMP_FILE_NAME
            | WRITER_LOCK_FILE_NAME
            | ACTIVATION_LOCK_FILE_NAME => {
                if !metadata.is_file() {
                    return Err(invalid_data("runtime control artifact is not a file"));
                }
            }
            "n5-fixture" => {
                if !metadata.is_dir() {
                    return Err(invalid_data("fixture component root is not a directory"));
                }
                saw_fixture_directory = true;
            }
            "cas" | "index" | "graph" | "meta" | "crdt" | "federation" => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "recognized production component family requires an N5-B semantic adapter",
                ));
            }
            _ => {
                return Err(invalid_data(
                    "unclassified base artifact is not in the N5-A durable-component registry",
                ));
            }
        }
    }
    if !saw_fixture_directory {
        return Err(invalid_data(
            "unsupported legacy layout: disposable fixture component root is missing",
        ));
    }

    let fixture_root = root.join("n5-fixture");
    let _fixture_guard = AncestorGuard::acquire(root, &fixture_root)?;
    let entries = read_directory_bounded(&fixture_root, limits)?;
    let expected_names = BTreeSet::from(["history.v1".to_owned(), "state.v1".to_owned()]);
    let actual_names = entries
        .iter()
        .map(|entry| {
            entry
                .file_name()
                .into_string()
                .map_err(|_| invalid_data("fixture path is not canonical UTF-8"))
        })
        .collect::<io::Result<BTreeSet<_>>>()?;
    if actual_names != expected_names {
        return Err(invalid_data(
            "fixture registry requires exactly state.v1 and history.v1",
        ));
    }

    let mut inventory = Vec::with_capacity(2);
    for kind in [
        DurableComponentKind::FixtureHistory,
        DurableComponentKind::FixtureState,
    ] {
        let relative_path = kind.path().to_owned();
        let path = root.join(Path::new(&relative_path));
        let (length, blake3_hash, identity) = hash_verified_file(
            root,
            &path,
            FIXTURE_COMPONENT_MAX_BYTES.min(limits.max_component_bytes),
        )?;
        inventory.push(InventoryComponent {
            descriptor: ComponentDescriptor {
                kind,
                relative_path,
                length,
                blake3_hash,
            },
            source_path: path,
            identity,
        });
    }
    inventory.sort_by(|left, right| {
        left.descriptor
            .relative_path
            .cmp(&right.descriptor.relative_path)
    });
    let descriptors = inventory
        .iter()
        .map(|component| component.descriptor.clone())
        .collect::<Vec<_>>();
    validate_component_descriptors_with_limits(&descriptors, limits)?;
    checked_component_total(&descriptors, limits)?;
    Ok(inventory)
}

fn collect_exact_component_tree(
    physical_root: &Path,
    root: &Path,
    expected: &[ComponentDescriptor],
    limits: &ResourceLimits,
) -> io::Result<Vec<ComponentDescriptor>> {
    let _guard = AncestorGuard::acquire(physical_root, root)?;
    reject_link_or_reparse(root)?;
    let expected_paths = expected
        .iter()
        .map(|descriptor| descriptor.relative_path.clone())
        .collect::<BTreeSet<_>>();
    let expected_directories = expected_paths
        .iter()
        .flat_map(|path| {
            let mut directories = Vec::new();
            let mut parent = Path::new(path).parent();
            while let Some(value) = parent.filter(|value| !value.as_os_str().is_empty()) {
                directories.push(value.to_string_lossy().replace('\\', "/"));
                parent = value.parent();
            }
            directories
        })
        .collect::<BTreeSet<_>>();
    let mut actual_paths = BTreeSet::new();
    let mut actual_directories = BTreeSet::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    let mut budget = TreeBudget::default();
    while let Some((directory, prefix)) = stack.pop() {
        for entry in read_directory_bounded(&directory, limits)? {
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| invalid_data("component tree path is not canonical UTF-8"))?;
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            budget.observe(&relative, limits)?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if is_link_or_reparse(&entry.path(), &metadata) {
                return Err(invalid_data(
                    "component tree contains a link or reparse point",
                ));
            }
            if metadata.is_dir() {
                actual_directories.insert(relative.clone());
                stack.push((entry.path(), relative));
            } else if metadata.is_file() {
                actual_paths.insert(relative);
            } else {
                return Err(invalid_data("component tree contains a non-file entry"));
            }
        }
    }
    if actual_paths != expected_paths || actual_directories != expected_directories {
        return Err(invalid_data(
            "component file and directory tree is not exactly equal to its descriptor set",
        ));
    }

    let mut actual = Vec::with_capacity(expected.len());
    for descriptor in expected {
        let path = root.join(Path::new(&descriptor.relative_path));
        let (length, blake3_hash, identity) =
            hash_verified_file(root, &path, limits.max_component_bytes)?;
        require_single_link(&identity, "generation component")?;
        actual.push(ComponentDescriptor {
            kind: descriptor.kind,
            relative_path: descriptor.relative_path.clone(),
            length,
            blake3_hash,
        });
    }
    Ok(actual)
}

fn registry_kind(path: &str) -> io::Result<DurableComponentKind> {
    match path {
        FIXTURE_STATE_PATH => Ok(DurableComponentKind::FixtureState),
        FIXTURE_HISTORY_PATH => Ok(DurableComponentKind::FixtureHistory),
        _ if path.starts_with("cas/")
            || path.starts_with("index/")
            || path.starts_with("graph/")
            || path.starts_with("meta/") =>
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "recognized production component requires an N5-B semantic adapter",
            ))
        }
        _ => Err(invalid_data(
            "component path is not classified by the N5-A registry",
        )),
    }
}

fn validate_component_bytes(kind: DurableComponentKind, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() as u64 > FIXTURE_COMPONENT_MAX_BYTES {
        return Err(invalid_data(
            "typed fixture component exceeds its adapter limit",
        ));
    }
    match kind {
        DurableComponentKind::FixtureState => {
            parse_fixture_counter(bytes)?;
        }
        DurableComponentKind::FixtureHistory => {
            parse_fixture_history(bytes)?;
        }
    }
    Ok(())
}

fn parse_fixture_counter(bytes: &[u8]) -> io::Result<u64> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| invalid_data("fixture state is not canonical UTF-8"))?;
    let value = text
        .strip_prefix("counter=")
        .and_then(|value| value.strip_suffix('\n'))
        .ok_or_else(|| invalid_data("fixture state has a noncanonical encoding"))?;
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid_data("fixture counter is noncanonical"));
    }
    value
        .parse::<u64>()
        .map_err(|_| invalid_data("fixture counter overflow"))
}

fn parse_fixture_history(bytes: &[u8]) -> io::Result<Vec<TransactionId>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| invalid_data("fixture history is not canonical UTF-8"))?;
    if !text.is_empty() && !text.ends_with('\n') {
        return Err(invalid_data("fixture history must end with a newline"));
    }
    let mut history = Vec::new();
    let mut unique = BTreeSet::new();
    for line in text.lines() {
        let transaction_id = TransactionId::parse(line)?;
        if !unique.insert(transaction_id.clone()) {
            return Err(invalid_data(
                "fixture history contains a duplicate transaction_id",
            ));
        }
        history.push(transaction_id);
    }
    Ok(history)
}

fn rewrite_fixture_component(root: &Path, relative: &str, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() as u64 > FIXTURE_COMPONENT_MAX_BYTES {
        return Err(invalid_data(
            "typed fixture pre-state restore exceeds its component limit",
        ));
    }
    registry_kind(relative)?;
    let path = root.join(relative);
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data("typed fixture restore target has no parent"))?;
    let guard = AncestorGuard::acquire(root, parent)?;
    reject_link_or_reparse(&path)?;
    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = options.open(&path)?;
    let metadata = file.metadata()?;
    let identity = stable_identity(&file)?;
    if !metadata.is_file() {
        return Err(invalid_data(
            "typed fixture restore target is not a regular file",
        ));
    }
    require_single_link(&identity, "typed fixture restore target")?;
    file.set_len(0)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    guard.verify()?;
    Ok(())
}

fn restore_fixture_projection(
    root: &Path,
    projection: &FixtureProjection,
    limits: &ResourceLimits,
) -> io::Result<()> {
    let state = format!("counter={}\n", projection.counter);
    let mut history = String::new();
    for transaction_id in &projection.history {
        history.push_str(transaction_id.as_str());
        history.push('\n');
    }
    validate_component_bytes(DurableComponentKind::FixtureState, state.as_bytes())?;
    validate_component_bytes(DurableComponentKind::FixtureHistory, history.as_bytes())?;
    rewrite_fixture_component(root, FIXTURE_STATE_PATH, state.as_bytes())?;
    rewrite_fixture_component(root, FIXTURE_HISTORY_PATH, history.as_bytes())?;
    let restored = fixture_projection_from_inventory(&collect_registered_inventory(root, limits)?)?;
    if restored != *projection {
        return Err(invalid_data(
            "typed fixture pre-state restore did not reproduce the bound projection",
        ));
    }
    Ok(())
}

fn read_fixture_projection(
    sources: &BTreeMap<String, ComponentSource>,
) -> io::Result<FixtureProjection> {
    if sources.len() != 2 {
        return Err(invalid_data(
            "typed fixture projection requires exactly state and history",
        ));
    }
    let state = sources
        .get(FIXTURE_STATE_PATH)
        .ok_or_else(|| invalid_data("typed fixture state component is missing"))?;
    let history = sources
        .get(FIXTURE_HISTORY_PATH)
        .ok_or_else(|| invalid_data("typed fixture history component is missing"))?;
    let state_bytes = read_bytes_bounded(&state.storage_path, FIXTURE_COMPONENT_MAX_BYTES)?;
    let history_bytes = read_bytes_bounded(&history.storage_path, FIXTURE_COMPONENT_MAX_BYTES)?;
    validate_component_bytes(DurableComponentKind::FixtureState, &state_bytes)?;
    validate_component_bytes(DurableComponentKind::FixtureHistory, &history_bytes)?;
    Ok(FixtureProjection {
        counter: parse_fixture_counter(&state_bytes)?,
        history: parse_fixture_history(&history_bytes)?,
    })
}

fn fixture_projection_from_inventory(
    inventory: &[InventoryComponent],
) -> io::Result<FixtureProjection> {
    let sources = inventory
        .iter()
        .map(|component| {
            (
                component.descriptor.relative_path.clone(),
                ComponentSource {
                    descriptor: component.descriptor.clone(),
                    storage_path: component.source_path.clone(),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    read_fixture_projection(&sources)
}

fn validate_fixture_projection_from_inventory(inventory: &[InventoryComponent]) -> io::Result<()> {
    fixture_projection_from_inventory(inventory).map(|_| ())
}

fn validate_fixture_transition(
    before: &BTreeMap<String, ComponentSource>,
    after: &BTreeMap<String, ComponentSource>,
    transaction_id: &TransactionId,
) -> io::Result<()> {
    let before = read_fixture_projection(before)?;
    let after = read_fixture_projection(after)?;
    if after.counter != before.counter.saturating_add(1)
        || after.history.len() != before.history.len().saturating_add(1)
        || !after.history.starts_with(&before.history)
        || after.history.last() != Some(transaction_id)
        || before.history.contains(transaction_id)
    {
        return Err(invalid_data(
            "typed fixture transition is not counter-plus-one with history exactly once",
        ));
    }
    Ok(())
}

fn validate_staged_fixture_transition(
    component_root: &Path,
    components: &[ComponentDescriptor],
    before: &FixtureProjection,
    transaction_id: &TransactionId,
) -> io::Result<()> {
    if components.len() != 2
        || !components
            .iter()
            .any(|component| component.kind == DurableComponentKind::FixtureState)
        || !components
            .iter()
            .any(|component| component.kind == DurableComponentKind::FixtureHistory)
    {
        return Err(invalid_data(
            "typed fixture transaction must stage the paired state and history components",
        ));
    }
    let sources = components
        .iter()
        .map(|descriptor| {
            (
                descriptor.relative_path.clone(),
                ComponentSource {
                    descriptor: descriptor.clone(),
                    storage_path: component_root.join(Path::new(&descriptor.relative_path)),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let after = read_fixture_projection(&sources)?;
    if after.counter != before.counter.saturating_add(1)
        || after.history.len() != before.history.len().saturating_add(1)
        || !after.history.starts_with(&before.history)
        || after.history.last() != Some(transaction_id)
        || before.history.contains(transaction_id)
    {
        return Err(invalid_data(
            "staged typed fixture transition violates idempotency/history-once",
        ));
    }
    Ok(())
}

fn baseline_component_sources(
    root: &Path,
    baseline: &BaselineRecord,
) -> BTreeMap<String, ComponentSource> {
    baseline
        .body
        .components
        .iter()
        .cloned()
        .map(|descriptor| {
            (
                descriptor.relative_path.clone(),
                ComponentSource {
                    storage_path: baseline_dir(root)
                        .join(COMPONENTS_DIR_NAME)
                        .join(Path::new(&descriptor.relative_path)),
                    descriptor,
                },
            )
        })
        .collect()
}

fn overlay_component_sources(
    previous: &BTreeMap<String, ComponentSource>,
    components: &[ComponentDescriptor],
    component_root: &Path,
) -> BTreeMap<String, ComponentSource> {
    let mut next = previous.clone();
    for descriptor in components {
        next.insert(
            descriptor.relative_path.clone(),
            ComponentSource {
                descriptor: descriptor.clone(),
                storage_path: component_root.join(Path::new(&descriptor.relative_path)),
            },
        );
    }
    next
}

fn canonical_logical_state_digest(components: &[ComponentDescriptor]) -> io::Result<String> {
    validate_component_descriptors(components)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(CANONICAL_LOGICAL_STATE_DIGEST_ID.as_bytes());
    hasher.update(&[0]);
    let component_count = u64::try_from(components.len())
        .map_err(|_| invalid_data("component count does not fit the digest schema"))?;
    hasher.update(&component_count.to_le_bytes());
    for descriptor in components {
        let kind = descriptor.kind.tag().as_bytes();
        let kind_length = u64::try_from(kind.len())
            .map_err(|_| invalid_data("component kind length overflow"))?;
        hasher.update(&kind_length.to_le_bytes());
        hasher.update(kind);
        let path = descriptor.relative_path.as_bytes();
        let path_length = u64::try_from(path.len())
            .map_err(|_| invalid_data("component path length overflow"))?;
        hasher.update(&path_length.to_le_bytes());
        hasher.update(path);
        hasher.update(&descriptor.length.to_le_bytes());
        hasher.update(descriptor.blake3_hash.as_bytes());
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn validate_format(record: &FormatRecord) -> io::Result<()> {
    if record.magic != FORMAT_MAGIC
        || record.version != FORMAT_VERSION
        || record.codec != PRIVATE_CODEC_ID
        || record.component_registry != COMPONENT_REGISTRY_ID
        || record.limits != LIMITS_ID
    {
        return Err(invalid_data("unsupported operation transaction format"));
    }
    let expected = format_crc(record)?;
    if record.crc32 != expected {
        return Err(invalid_data("operation transaction format CRC mismatch"));
    }
    Ok(())
}

fn validate_migration(root: &Path, baseline: &BaselineRecord) -> io::Result<MigrationRecord> {
    let record = read_record_under::<MigrationRecord>(
        root,
        &transaction_dir(root).join(MIGRATION_FILE_NAME),
        "migration",
    )?;
    validate_migration_record(&record, baseline)?;
    Ok(record)
}

fn validate_migration_record(
    record: &MigrationRecord,
    baseline: &BaselineRecord,
) -> io::Result<()> {
    if record.body.magic != "MEMORYX_N5_MIGRATION_REPORT"
        || record.body.version != FORMAT_VERSION
        || record.body.codec != PRIVATE_CODEC_ID
        || record.body.source_layout != SOURCE_LAYOUT_ID
        || record.body.target_format != FORMAT_MAGIC
        || record.body.component_registry != COMPONENT_REGISTRY_ID
        || record.body.limits != LIMITS_ID
        || record.body.rollback_policy != ROLLBACK_POLICY_ID
        || !record.body.source_files_untouched
        || record.body.baseline_digest != baseline.body.logical_state_digest
        || record.crc32 != record_crc("migration", &record.body)?
    {
        return Err(invalid_data("invalid migration preflight/report record"));
    }
    let total = checked_component_total(&baseline.body.components, &DEFAULT_LIMITS)?;
    let component_count = u64::try_from(baseline.body.components.len())
        .map_err(|_| invalid_data("baseline component count does not fit u64"))?;
    if record.body.component_count != component_count
        || record.body.total_bytes != total
        || record.body.required_copy_bytes
            != total
                .checked_add(COPY_SPACE_OVERHEAD_BYTES)
                .ok_or_else(|| invalid_data("migration copy-space total overflow"))?
        || record.body.available_copy_bytes < record.body.required_copy_bytes
    {
        return Err(invalid_data(
            "migration report bounds do not match baseline",
        ));
    }
    Ok(())
}

fn read_prepare(root: &Path, path: &Path) -> io::Result<PrepareRecord> {
    let record = read_record_under::<PrepareRecord>(root, path, "prepare")?;
    if record.body.magic != FORMAT_MAGIC || record.body.version != FORMAT_VERSION {
        return Err(invalid_data(
            "unsupported operation transaction prepare record",
        ));
    }
    if record.body.codec != PRIVATE_CODEC_ID || record.crc32 != record_crc("prepare", &record.body)?
    {
        return Err(invalid_data("operation transaction prepare CRC mismatch"));
    }
    if !is_hash(&record.body.parent_commit_hash)
        || !is_hash(&record.body.intent_hash)
        || TransactionId::parse(record.body.transaction_id.as_str())? != record.body.transaction_id
    {
        return Err(invalid_data(
            "operation transaction prepare has invalid hash fields",
        ));
    }
    Ok(record)
}

fn validate_prepare(
    prepare: &PrepareRecord,
    generation: u64,
    state: &RecoveryState,
) -> io::Result<()> {
    if prepare.body.generation != generation
        || generation != state.generation.saturating_add(1)
        || prepare.body.parent_commit_hash != state.commit_hash
    {
        return Err(invalid_data(
            "operation transaction prepare does not extend the committed chain",
        ));
    }
    Ok(())
}

fn validate_commit(
    root: &Path,
    commit: &CommitRecord,
    prepare: &PrepareRecord,
    generation: u64,
    state: &RecoveryState,
    generation_dir: &Path,
) -> io::Result<()> {
    if commit.body.magic != FORMAT_MAGIC || commit.body.version != FORMAT_VERSION {
        return Err(invalid_data(
            "unsupported operation transaction commit record",
        ));
    }
    if commit.body.codec != PRIVATE_CODEC_ID || commit.crc32 != record_crc("commit", &commit.body)?
    {
        return Err(invalid_data("operation transaction commit CRC mismatch"));
    }
    let expected_prepare_hash = hash_hex(&encode_record(prepare)?);
    if commit.body.generation != generation
        || generation != state.generation.saturating_add(1)
        || commit.body.parent_commit_hash != state.commit_hash
        || commit.body.prepare_hash != expected_prepare_hash
        || commit.body.transaction_id != prepare.body.transaction_id
        || commit.body.operation != prepare.body.operation
        || commit.body.intent_hash != prepare.body.intent_hash
        || !is_hash(&commit.body.parent_commit_hash)
        || !is_hash(&commit.body.prepare_hash)
        || !is_hash(&commit.body.intent_hash)
        || !is_hash(&commit.body.logical_snapshot_hash)
        || TransactionId::parse(commit.body.transaction_id.as_str())? != commit.body.transaction_id
    {
        return Err(invalid_data(
            "operation transaction commit does not match its prepare or parent chain",
        ));
    }

    validate_component_descriptors(&commit.body.components)?;
    let stored = collect_exact_component_tree(
        root,
        &generation_dir.join(COMPONENTS_DIR_NAME),
        &commit.body.components,
        &DEFAULT_LIMITS,
    )?;
    if stored != commit.body.components {
        return Err(invalid_data(
            "committed operation transaction component tree does not match descriptors",
        ));
    }
    let expected_snapshot_hash = logical_snapshot_hash(
        &commit.body.parent_commit_hash,
        &commit.body.prepare_hash,
        &commit.body.components,
    )?;
    if commit.body.components.is_empty()
        || commit.body.logical_snapshot_hash != expected_snapshot_hash
    {
        return Err(invalid_data(
            "operation transaction logical snapshot hash mismatch",
        ));
    }
    Ok(())
}

fn logical_snapshot_hash(
    parent_commit_hash: &str,
    prepare_hash: &str,
    components: &[ComponentDescriptor],
) -> io::Result<String> {
    validate_component_descriptors(components)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"memoryx.n5.logical-snapshot.v1\0");
    hasher.update(parent_commit_hash.as_bytes());
    hasher.update(prepare_hash.as_bytes());
    hasher.update(canonical_logical_state_digest(components)?.as_bytes());
    Ok(hasher.finalize().to_hex().to_string())
}

fn normalize_component_path(path: &Path) -> io::Result<String> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(invalid_data(
            "transaction component path must be non-empty and relative",
        ));
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) if !part.is_empty() => {
                let value = part
                    .to_str()
                    .ok_or_else(|| invalid_data("transaction component path is not UTF-8"))?;
                if value.contains('\\') || value.contains('/') {
                    return Err(invalid_data("transaction component path is not normalized"));
                }
                if !value.is_ascii() {
                    return Err(invalid_data(
                        "N5-A private codec supports ASCII component paths only",
                    ));
                }
                parts.push(value.to_owned());
            }
            _ => {
                return Err(invalid_data(
                    "transaction component path contains traversal",
                ));
            }
        }
    }
    if parts.is_empty() {
        return Err(invalid_data("transaction component path is empty"));
    }
    let normalized = parts.join("/");
    if normalized.len() > MAX_PATH_BYTES {
        return Err(invalid_data(
            "transaction component path exceeds N5-A limit",
        ));
    }
    Ok(normalized)
}

fn read_directory_bounded(path: &Path, limits: &ResourceLimits) -> io::Result<Vec<fs::DirEntry>> {
    reject_link_or_reparse(path)?;
    let mut entries = Vec::new();
    for entry in fs::read_dir(path)? {
        if entries.len() >= limits.max_directory_entries {
            return Err(invalid_data("directory entry count exceeds the N5-A limit"));
        }
        entries.push(entry?);
    }
    Ok(entries)
}

fn validate_cumulative_tree_budget(
    physical_root: &Path,
    tree_root: &Path,
    limits: &ResourceLimits,
) -> io::Result<()> {
    let _guard = AncestorGuard::acquire(physical_root, tree_root)?;
    let mut budget = TreeBudget::default();
    let mut stack = vec![(tree_root.to_path_buf(), String::new())];
    while let Some((directory, prefix)) = stack.pop() {
        for entry in read_directory_bounded(&directory, limits)? {
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| invalid_data("control-tree path is not canonical UTF-8"))?;
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            budget.observe(&relative, limits)?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if is_link_or_reparse(&entry.path(), &metadata) {
                return Err(invalid_data(
                    "control tree contains a link or reparse point",
                ));
            }
            if metadata.is_dir() {
                stack.push((entry.path(), relative));
            } else if !metadata.is_file() {
                return Err(invalid_data("control tree contains a non-file entry"));
            }
        }
    }
    Ok(())
}

fn future_generation_tree_budget(
    physical_root: &Path,
    pending: &Path,
    published: &Path,
    limits: &ResourceLimits,
) -> io::Result<TreeBudget> {
    let tree_root = transaction_dir(physical_root);
    if path_entry_exists(published)? {
        return Err(invalid_data(
            "future generation publication target already exists",
        ));
    }
    let pending_relative = canonical_relative_path(&tree_root, pending)?;
    let published_relative = canonical_relative_path(&tree_root, published)?;
    let pending_prefix = format!("{pending_relative}/");
    let mut budget = TreeBudget::default();
    let mut stack = vec![(tree_root.clone(), String::new())];
    let _guard = AncestorGuard::acquire(physical_root, &tree_root)?;
    while let Some((directory, prefix)) = stack.pop() {
        for entry in read_directory_bounded(&directory, limits)? {
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| invalid_data("future-tree path is not canonical UTF-8"))?;
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let future_relative = if relative == pending_relative {
                published_relative.clone()
            } else if let Some(suffix) = relative.strip_prefix(&pending_prefix) {
                format!("{published_relative}/{suffix}")
            } else {
                relative.clone()
            };
            budget.observe(&future_relative, limits)?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if is_link_or_reparse(&entry.path(), &metadata) {
                return Err(invalid_data(
                    "future transaction-control tree contains a link or reparse point",
                ));
            }
            if metadata.is_dir() {
                stack.push((entry.path(), relative));
            } else if !metadata.is_file() {
                return Err(invalid_data(
                    "future transaction-control tree contains a non-file entry",
                ));
            }
        }
    }
    Ok(budget)
}

fn validate_future_generation_tree_budget(
    physical_root: &Path,
    pending: &Path,
    published: &Path,
    limits: &ResourceLimits,
) -> io::Result<()> {
    future_generation_tree_budget(physical_root, pending, published, limits)?;
    Ok(())
}

fn canonical_relative_path(root: &Path, path: &Path) -> io::Result<String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| invalid_data("future-tree path is outside its control root"))?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(invalid_data("future-tree path is not canonical"));
        };
        parts.push(
            part.to_str()
                .ok_or_else(|| invalid_data("future-tree path is not canonical UTF-8"))?,
        );
    }
    if parts.is_empty() {
        return Err(invalid_data("future-tree path is empty"));
    }
    Ok(parts.join("/"))
}

fn reject_link_or_reparse(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if is_link_or_reparse(path, &metadata) {
        return Err(invalid_data(
            "path is a symbolic link, junction, or reparse point",
        ));
    }
    Ok(())
}

fn is_link_or_reparse(_path: &Path, metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    false
}

fn open_verified_regular(root: &Path, path: &Path) -> io::Result<(File, StableFileIdentity)> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data("component path has no parent"))?;
    let guard = AncestorGuard::acquire(root, parent)?;
    reject_link_or_reparse(path)?;

    let file = open_no_follow(path)?;
    let handle_metadata = file.metadata()?;
    let path_metadata = fs::symlink_metadata(path)?;
    if is_link_or_reparse(path, &path_metadata)
        || !handle_metadata.is_file()
        || !path_metadata.is_file()
    {
        return Err(invalid_data("component is not a verified regular file"));
    }
    let handle_identity = stable_identity(&file)?;
    let path_handle = open_no_follow(path)?;
    let path_identity = stable_identity(&path_handle)?;
    if handle_identity != path_identity {
        return Err(invalid_data(
            "component identity changed between path validation and open",
        ));
    }
    guard.verify()?;
    Ok((file, handle_identity))
}

fn same_file_object(left: &StableFileIdentity, right: &StableFileIdentity) -> bool {
    left.platform_a == right.platform_a && left.platform_b == right.platform_b
}

fn open_no_follow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        // Excluding FILE_SHARE_DELETE pins the final directory entry against
        // rename/delete substitution for the lifetime of the returned handle.
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

fn open_append_no_follow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

fn open_read_write_no_follow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

fn open_directory_no_follow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_READ_ATTRIBUTES, FILE_TRAVERSE, SYNCHRONIZE,
        };
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        // Directory handles intentionally deny delete sharing. Holding the
        // root-to-parent chain therefore prevents junction/ancestor rename or
        // delete substitution while a guarded path operation is in flight.
        options
            .access_mode(FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

#[cfg(windows)]
fn open_directory_publication_parent(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_READ_ATTRIBUTES, FILE_TRAVERSE, SYNCHRONIZE,
    };

    const FILE_ADD_SUBDIRECTORY: u32 = 0x0000_0004;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    let mut options = OpenOptions::new();
    options
        .access_mode(FILE_ADD_SUBDIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    options.open(path)
}

#[cfg(unix)]
fn stable_identity(file: &File) -> io::Result<StableFileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok(StableFileIdentity {
        length: metadata.len(),
        modified: metadata.modified().ok(),
        platform_a: metadata.dev(),
        platform_b: metadata.ino(),
        links: metadata.nlink(),
    })
}

#[cfg(windows)]
fn stable_identity(file: &File) -> io::Result<StableFileIdentity> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // Safety: the raw handle is borrowed from `file` for this call and remains
    // valid; `information` points to writable, correctly aligned storage.
    let result =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, information.as_mut_ptr()) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    // Safety: a nonzero GetFileInformationByHandle result guarantees the
    // entire output structure was initialized.
    let information = unsafe { information.assume_init() };
    let metadata = file.metadata()?;
    Ok(StableFileIdentity {
        length: metadata.len(),
        modified: metadata.modified().ok(),
        platform_a: information.dwVolumeSerialNumber as u64,
        platform_b: ((information.nFileIndexHigh as u64) << 32) | information.nFileIndexLow as u64,
        links: information.nNumberOfLinks as u64,
    })
}

fn require_single_link(identity: &StableFileIdentity, label: &str) -> io::Result<()> {
    if identity.links != 1 {
        return Err(invalid_data(&format!(
            "immutable {label} must have exactly one filesystem link"
        )));
    }
    Ok(())
}

fn open_verified_control_record(
    root: &Path,
    path: &Path,
    label: &str,
) -> io::Result<(File, StableFileIdentity)> {
    let (file, identity) = open_verified_regular(root, path)?;
    require_single_link(&identity, label)?;
    if identity.length > DEFAULT_LIMITS.max_record_bytes {
        return Err(invalid_data(&format!(
            "immutable {label} exceeds the control-record byte limit"
        )));
    }
    Ok((file, identity))
}

fn validate_control_record_artifact(root: &Path, path: &Path, label: &str) -> io::Result<()> {
    let (_file, _identity) = open_verified_control_record(root, path, label)?;
    Ok(())
}

fn hash_open_file_handle(file: &File, max_bytes: u64) -> io::Result<String> {
    let before = stable_identity(file)?;
    if before.length > max_bytes {
        return Err(invalid_data("open file exceeds its streaming hash bound"));
    }
    let mut reader = BufReader::with_capacity(STREAM_BUFFER_BYTES, file.try_clone()?);
    reader.seek(SeekFrom::Start(0))?;
    let mut buffer = [0u8; STREAM_BUFFER_BYTES];
    let mut total = 0u64;
    let mut hasher = blake3::Hasher::new();
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| invalid_data("open file streaming hash byte count overflow"))?;
        if total > max_bytes {
            return Err(invalid_data(
                "open file grew beyond its streaming hash bound",
            ));
        }
        hasher.update(&buffer[..read]);
    }
    if total != before.length || stable_identity(file)? != before {
        return Err(invalid_data("open file changed during streaming hash"));
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn hash_verified_file(
    root: &Path,
    path: &Path,
    max_bytes: u64,
) -> io::Result<(u64, String, StableFileIdentity)> {
    let (file, identity) = open_verified_regular(root, path)?;
    if identity.length > max_bytes {
        return Err(invalid_data("component exceeds its streaming byte limit"));
    }
    let mut reader = BufReader::with_capacity(STREAM_BUFFER_BYTES, file);
    let mut buffer = [0u8; STREAM_BUFFER_BYTES];
    let mut total = 0u64;
    let mut hasher = blake3::Hasher::new();
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| invalid_data("component byte count overflow"))?;
        if total > max_bytes {
            return Err(invalid_data(
                "component grew beyond its streaming byte limit",
            ));
        }
        hasher.update(&buffer[..read]);
    }
    let after = stable_identity(reader.get_ref())?;
    if after != identity || total != identity.length {
        return Err(invalid_data(
            "component changed during bounded streaming read",
        ));
    }
    Ok((total, hasher.finalize().to_hex().to_string(), identity))
}

fn copy_verified_component(
    root: &Path,
    component: &InventoryComponent,
    target: &Path,
    failpoint: &mut Option<Box<dyn OperationFailpoint>>,
    occurrence: usize,
    limits: &ResourceLimits,
) -> io::Result<()> {
    let (source, identity) = open_verified_regular(root, &component.source_path)?;
    if identity != component.identity || identity.length != component.descriptor.length {
        return Err(invalid_data("component changed after migration preflight"));
    }
    let parent = target
        .parent()
        .ok_or_else(|| invalid_data("baseline target has no parent"))?;
    let guard = AncestorGuard::acquire(root, parent)?;
    let target_file = create_new_file_guarded(&guard, target)?;
    let mut reader = BufReader::with_capacity(STREAM_BUFFER_BYTES, source);
    let mut writer = BufWriter::with_capacity(STREAM_BUFFER_BYTES, target_file);
    let mut buffer = [0u8; STREAM_BUFFER_BYTES];
    let mut total = 0u64;
    let mut hasher = blake3::Hasher::new();
    hit_failpoint_at(
        failpoint,
        OperationStage::BaselineBeforeComponentWrite,
        occurrence,
    )?;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| invalid_data("baseline copy byte count overflow"))?;
        if total > component.descriptor.length || total > limits.max_component_bytes {
            return Err(invalid_data("component grew during bounded baseline copy"));
        }
        writer.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
    }
    hit_failpoint_at(
        failpoint,
        OperationStage::BaselineAfterComponentWrite,
        occurrence,
    )?;
    writer.flush()?;
    hit_failpoint_at(
        failpoint,
        OperationStage::BaselineAfterComponentFlush,
        occurrence,
    )?;
    writer.get_ref().sync_all()?;
    hit_failpoint_at(
        failpoint,
        OperationStage::BaselineAfterComponentFileSync,
        occurrence,
    )?;
    let after = stable_identity(reader.get_ref())?;
    if after != component.identity
        || total != component.descriptor.length
        || hasher.finalize().to_hex().as_str() != component.descriptor.blake3_hash
    {
        return Err(invalid_data(
            "component changed during bounded baseline copy",
        ));
    }
    Ok(())
}

fn write_new_record_with_failpoints<T: Serialize>(
    root: &Path,
    path: &Path,
    record: &T,
    failpoint: &mut Option<Box<dyn OperationFailpoint>>,
    occurrences: &mut BTreeMap<OperationStage, usize>,
    stages: [OperationStage; 3],
) -> io::Result<()> {
    let bytes = encode_record(record)?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("transaction file has no parent"))?;
    let guard = AncestorGuard::acquire(root, parent)?;
    let mut file = create_new_file_guarded(&guard, path)?;
    file.write_all(&bytes)?;
    hit_failpoint(failpoint, occurrences, stages[0])?;
    file.flush()?;
    hit_failpoint(failpoint, occurrences, stages[1])?;
    file.sync_all()?;
    require_single_link(&stable_identity(&file)?, "new control record")?;
    hit_failpoint(failpoint, occurrences, stages[2])
}

fn write_new_file_with_failpoints(
    root: &Path,
    path: &Path,
    bytes: &[u8],
    failpoint: &mut Option<Box<dyn OperationFailpoint>>,
    stages: [OperationStage; 3],
    occurrence: usize,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("transaction file has no parent"))?;
    let guard = AncestorGuard::acquire(root, parent)?;
    let mut file = create_new_file_guarded(&guard, path)?;
    file.write_all(bytes)?;
    hit_failpoint_at(failpoint, stages[0], occurrence)?;
    file.flush()?;
    hit_failpoint_at(failpoint, stages[1], occurrence)?;
    file.sync_all()?;
    hit_failpoint_at(failpoint, stages[2], occurrence)
}

fn create_new_file_guarded(parent_guard: &AncestorGuard, path: &Path) -> io::Result<File> {
    if path.parent() != Some(parent_guard.directory_path()?) {
        return Err(invalid_data(
            "new file path is not directly beneath its pinned parent",
        ));
    }
    parent_guard.verify()?;
    #[cfg(unix)]
    let file = {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::io::{AsRawFd, FromRawFd};

        let name = path
            .file_name()
            .ok_or_else(|| invalid_data("new file path has no final component"))?;
        let name = CString::new(name.as_bytes())
            .map_err(|_| invalid_data("new file name contains NUL"))?;
        // Safety: the parent descriptor is a live pinned directory handle and
        // `name` is a live NUL-terminated single component. O_EXCL and
        // O_NOFOLLOW prevent replacement and final-link traversal. A
        // nonnegative descriptor is uniquely owned and converted once.
        let descriptor = unsafe {
            libc::openat(
                parent_guard.directory_handle()?.as_raw_fd(),
                name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        // Safety: `descriptor` was returned as a new owned descriptor above
        // and has not been wrapped or closed.
        unsafe { File::from_raw_fd(descriptor) }
    };
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
        use windows_sys::Win32::Storage::FileSystem::{DELETE, SYNCHRONIZE};

        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        let mut options = OpenOptions::new();
        options
            .create_new(true)
            .read(true)
            .write(true)
            .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE | SYNCHRONIZE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
        options.open(path)?
    };
    parent_guard.verify()?;
    Ok(file)
}

fn create_directory_guarded(root: &Path, parent: &Path, path: &Path) -> io::Result<()> {
    if path.parent() != Some(parent) {
        return Err(invalid_data(
            "new directory path is not directly beneath its parent",
        ));
    }
    let guard = AncestorGuard::acquire(root, parent)?;
    guard.verify()?;
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::io::AsRawFd;

        let name = path
            .file_name()
            .ok_or_else(|| invalid_data("new directory path has no final component"))?;
        let name = CString::new(name.as_bytes())
            .map_err(|_| invalid_data("new directory name contains NUL"))?;
        // Safety: the directory descriptor is a live pinned parent and `name`
        // is one live NUL-terminated component. mkdirat performs the creation
        // relative to that handle and does not follow a final link.
        let result =
            unsafe { libc::mkdirat(guard.directory_handle()?.as_raw_fd(), name.as_ptr(), 0o700) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(windows)]
    fs::create_dir(path)?;
    guard.verify()?;
    let created = open_directory_no_follow(path)?;
    if !created.metadata()?.is_dir() {
        return Err(invalid_data("created control path is not a directory"));
    }
    Ok(())
}

fn hit_namespace_mutation(
    failpoint: &mut Option<Box<dyn OperationFailpoint>>,
    kind: NamespaceMutationKind,
    source: &Path,
    target: Option<&Path>,
) -> io::Result<()> {
    if let Some(failpoint) = failpoint.as_deref_mut() {
        failpoint.before_namespace_mutation(kind, source, target)?;
    }
    Ok(())
}

fn hit_after_namespace_mutation(
    failpoint: &mut Option<Box<dyn OperationFailpoint>>,
    kind: NamespaceMutationKind,
    path: &Path,
) -> io::Result<()> {
    if let Some(failpoint) = failpoint.as_deref_mut() {
        failpoint.after_namespace_mutation(kind, path)?;
    }
    Ok(())
}

fn hit_after_namespace_handle_close(
    failpoint: &mut Option<Box<dyn OperationFailpoint>>,
    kind: NamespaceMutationKind,
    path: &Path,
) -> io::Result<()> {
    if let Some(failpoint) = failpoint.as_mut() {
        failpoint.after_namespace_handle_close(kind, path)?;
    }
    Ok(())
}

fn hit_before_write_through_visibility(
    failpoint: &mut Option<Box<dyn OperationFailpoint>>,
    source: &Path,
    target: &Path,
) -> io::Result<()> {
    if let Some(failpoint) = failpoint.as_deref_mut() {
        failpoint.before_write_through_visibility(source, target)?;
    }
    Ok(())
}

#[cfg(windows)]
fn open_mutation_handle(path: &Path, directory: bool) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::Storage::FileSystem::{DELETE, SYNCHRONIZE};

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    let flags = FILE_FLAG_OPEN_REPARSE_POINT
        | if directory {
            FILE_FLAG_BACKUP_SEMANTICS
        } else {
            0
        };
    let mut options = OpenOptions::new();
    options
        .access_mode(GENERIC_READ | DELETE | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(flags);
    options.open(path)
}

#[cfg(windows)]
fn open_generation_publication_handle(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const DELETE: u32 = 0x0001_0000;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    let mut options = OpenOptions::new();
    options
        .access_mode(DELETE | SYNCHRONIZE | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    options.open(path)
}

#[cfg(windows)]
fn open_generation_child_pin(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const GENERIC_READ: u32 = 0x8000_0000;
    const DELETE: u32 = 0x0001_0000;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    let mut options = OpenOptions::new();
    options
        .access_mode(GENERIC_READ | DELETE | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    options.open(path)
}

#[cfg(windows)]
fn open_verification_handle_while_mutating(path: &Path, directory: bool) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    let flags = FILE_FLAG_OPEN_REPARSE_POINT
        | if directory {
            FILE_FLAG_BACKUP_SEMANTICS
        } else {
            0
        };
    let mut options = OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(flags);
    options.open(path)
}

#[cfg(windows)]
fn rename_opened_object_by_handle(
    source: &File,
    destination_parent: &File,
    target: &Path,
) -> io::Result<()> {
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::ptr;
    use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS, RtlNtStatusToDosError};
    use windows_sys::Win32::Storage::FileSystem::{FILE_RENAME_INFO, FILE_RENAME_INFO_0};
    use windows_sys::Win32::System::IO::{IO_STATUS_BLOCK, IO_STATUS_BLOCK_0};

    const FILE_RENAME_INFORMATION: i32 = 10;
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtSetInformationFile(
            file_handle: HANDLE,
            io_status_block: *mut IO_STATUS_BLOCK,
            file_information: *const core::ffi::c_void,
            length: u32,
            file_information_class: i32,
        ) -> NTSTATUS;
    }

    let target_component = target
        .file_name()
        .ok_or_else(|| invalid_data("handle-bound rename target has no final component"))?;
    let mut components = Path::new(target_component).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(invalid_data(
            "handle-bound rename target is not one relative final component",
        ));
    }
    let target_name = target_component.encode_wide().collect::<Vec<_>>();
    if target_name.is_empty() {
        return Err(invalid_data("handle-bound rename target is empty"));
    }
    let name_bytes = target_name
        .len()
        .checked_mul(size_of::<u16>())
        .ok_or_else(|| invalid_data("handle-bound rename name length overflow"))?;
    let buffer_bytes = size_of::<FILE_RENAME_INFO>()
        .checked_add(name_bytes)
        .ok_or_else(|| invalid_data("handle-bound rename buffer length overflow"))?;
    let buffer_size = u32::try_from(buffer_bytes)
        .map_err(|_| invalid_data("handle-bound rename buffer exceeds the Windows API limit"))?;
    let words = buffer_bytes.div_ceil(size_of::<usize>());
    let mut storage = vec![0usize; words];
    let information = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();

    // Safety: `storage` is aligned to `usize`, which is sufficient for
    // FILE_RENAME_INFO; its checked capacity includes the flexible UTF-16
    // tail. The source raw handle is borrowed from a live File for the entire
    // native API call, the pinned destination-parent File also stays live, and
    // the copied name has exactly `FileNameLength` initialized bytes. The
    // information class is the documented FILE_RENAME_INFORMATION value.
    unsafe {
        ptr::write(
            information,
            FILE_RENAME_INFO {
                Anonymous: FILE_RENAME_INFO_0 { ReplaceIfExists: 0 },
                // The name is one validated relative component resolved by the
                // kernel beneath the pinned destination-parent handle.
                RootDirectory: destination_parent.as_raw_handle() as _,
                FileNameLength: u32::try_from(name_bytes).map_err(|_| {
                    invalid_data("handle-bound rename name exceeds the Windows API limit")
                })?,
                FileName: [0],
            },
        );
        ptr::copy_nonoverlapping(
            target_name.as_ptr(),
            ptr::addr_of_mut!((*information).FileName).cast::<u16>(),
            target_name.len(),
        );
        let mut io_status = IO_STATUS_BLOCK {
            Anonymous: IO_STATUS_BLOCK_0 { Status: 0 },
            Information: 0,
        };
        let status = NtSetInformationFile(
            source.as_raw_handle() as _,
            &mut io_status,
            information.cast(),
            buffer_size,
            FILE_RENAME_INFORMATION,
        );
        if status < 0 {
            return Err(io::Error::from_raw_os_error(
                i32::try_from(RtlNtStatusToDosError(status)).unwrap_or(87),
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn rename_opened_object_same_parent_by_handle(source: &File, target: &Path) -> io::Result<()> {
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::ptr;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_RENAME_INFO, FILE_RENAME_INFO_0, FileRenameInfo, SetFileInformationByHandle,
    };

    if !target.is_absolute() {
        return Err(invalid_data(
            "same-parent handle rename target is not an absolute guarded path",
        ));
    }
    let target_name = target.as_os_str().encode_wide().collect::<Vec<_>>();
    if target_name.is_empty() {
        return Err(invalid_data("same-parent handle rename target is empty"));
    }
    let name_bytes = target_name
        .len()
        .checked_mul(size_of::<u16>())
        .ok_or_else(|| invalid_data("same-parent rename name length overflow"))?;
    let buffer_bytes = size_of::<FILE_RENAME_INFO>()
        .checked_add(name_bytes)
        .ok_or_else(|| invalid_data("same-parent rename buffer length overflow"))?;
    let buffer_size = u32::try_from(buffer_bytes)
        .map_err(|_| invalid_data("same-parent rename buffer exceeds the Windows API limit"))?;
    let words = buffer_bytes.div_ceil(size_of::<usize>());
    let mut storage = vec![0usize; words];
    let information = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();

    // Safety: `storage` is aligned and sized for FILE_RENAME_INFO plus the
    // checked flexible UTF-16 tail. `source` owns a live DELETE-capable handle;
    // the absolute target remains under the separately pinned same parent.
    unsafe {
        ptr::write(
            information,
            FILE_RENAME_INFO {
                Anonymous: FILE_RENAME_INFO_0 { ReplaceIfExists: 0 },
                RootDirectory: std::ptr::null_mut(),
                FileNameLength: u32::try_from(name_bytes).map_err(|_| {
                    invalid_data("same-parent rename name exceeds the Windows API limit")
                })?,
                FileName: [0],
            },
        );
        ptr::copy_nonoverlapping(
            target_name.as_ptr(),
            ptr::addr_of_mut!((*information).FileName).cast::<u16>(),
            target_name.len(),
        );
        if SetFileInformationByHandle(
            source.as_raw_handle() as _,
            FileRenameInfo,
            information.cast(),
            buffer_size,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn move_directory_write_through(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    fn extended_path(path: &Path) -> io::Result<Vec<u16>> {
        const SEPARATOR: u16 = b'\\' as u16;
        const ALT_SEPARATOR: u16 = b'/' as u16;
        const COLON: u16 = b':' as u16;
        const QUESTION: u16 = b'?' as u16;
        let mut path = path.as_os_str().encode_wide().collect::<Vec<_>>();
        for unit in &mut path {
            if *unit == ALT_SEPARATOR {
                *unit = SEPARATOR;
            }
        }
        let mut extended = Vec::with_capacity(path.len().saturating_add(8));
        if path.starts_with(&[SEPARATOR, SEPARATOR, QUESTION, SEPARATOR]) {
            extended.extend_from_slice(&path);
        } else if path.starts_with(&[SEPARATOR, SEPARATOR]) {
            extended.extend("\\\\?\\UNC\\".encode_utf16());
            extended.extend_from_slice(&path[2..]);
        } else if path.len() >= 3
            && path[1] == COLON
            && path[2] == SEPARATOR
            && ((b'A' as u16..=b'Z' as u16).contains(&path[0])
                || (b'a' as u16..=b'z' as u16).contains(&path[0]))
        {
            extended.extend("\\\\?\\".encode_utf16());
            extended.extend_from_slice(&path);
        } else {
            return Err(invalid_data(
                "write-through visibility path is not an absolute DOS or UNC path",
            ));
        }
        extended.push(0);
        Ok(extended)
    }

    if !source.is_absolute()
        || !target.is_absolute()
        || source.parent().is_none()
        || target.parent().is_none()
        || source.components().next() != target.components().next()
    {
        return Err(invalid_data(
            "write-through visibility move is not between absolute same-volume paths",
        ));
    }
    let source_wide = extended_path(source)?;
    let target_wide = extended_path(target)?;
    // Safety: both UTF-16 buffers are live, NUL-terminated absolute paths for
    // the duration of the call. No replacement/copy flags are supplied; the
    // source and destination are preflighted beneath the two pinned bounded
    // parents on one physical root.
    let result = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
fn move_directory_write_through(source: &Path, target: &Path) -> io::Result<()> {
    if !source.is_absolute() || !target.is_absolute() || source.parent() != target.parent() {
        return Err(invalid_data(
            "write-through visibility move is not between absolute same-parent paths",
        ));
    }
    let parent = target
        .parent()
        .ok_or_else(|| invalid_data("write-through visibility target has no parent"))?;
    reject_link_or_reparse(source)?;
    if !fs::symlink_metadata(source)?.is_dir() {
        return Err(invalid_data(
            "write-through visibility source is not a directory",
        ));
    }
    require_path_entry_absent(target, "write-through visibility target")?;
    fs::rename(source, target)?;
    sync_directory(parent)
}

#[cfg(windows)]
fn mark_opened_object_for_removal(source: &File) -> io::Result<()> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
    };

    let information = FILE_DISPOSITION_INFO { DeleteFile: 1 };
    // Safety: `source` owns a live handle opened with DELETE access, and
    // `information` is a fully initialized fixed-size input borrowed only for
    // the duration of SetFileInformationByHandle.
    let result = unsafe {
        SetFileInformationByHandle(
            source.as_raw_handle() as _,
            FileDispositionInfo,
            (&information as *const FILE_DISPOSITION_INFO).cast(),
            u32::try_from(size_of::<FILE_DISPOSITION_INFO>())
                .map_err(|_| invalid_data("Windows disposition structure size overflow"))?,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn mark_verified_file_for_removal_guarded(
    root: &Path,
    parent: &Path,
    path: &Path,
    expected: &StableFileIdentity,
    current: &File,
    failpoint: &mut Option<Box<dyn OperationFailpoint>>,
) -> io::Result<()> {
    let guard = AncestorGuard::acquire(root, parent)?;
    let metadata = current.metadata()?;
    let identity = stable_identity(current)?;
    require_single_link(&identity, "control record cleanup target")?;
    if !metadata.is_file() || !same_file_object(&identity, expected) {
        return Err(invalid_data(
            "control record cleanup target identity changed",
        ));
    }
    guard.verify()?;
    #[cfg(unix)]
    {
        let _ = (path, failpoint);
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private N5-A existing-object cleanup is disabled on Unix because unlinkat is namespace-bound rather than object-bound",
        ));
    }
    #[cfg(windows)]
    {
        hit_namespace_mutation(failpoint, NamespaceMutationKind::FileRemove, path, None)?;
        mark_opened_object_for_removal(current)?;
        guard.verify()?;
        Ok(())
    }
}

fn remove_verified_file_guarded(
    root: &Path,
    parent: &Path,
    path: &Path,
    expected: &StableFileIdentity,
    failpoint: &mut Option<Box<dyn OperationFailpoint>>,
) -> io::Result<()> {
    #[cfg(windows)]
    let current = open_mutation_handle(path, false)?;
    #[cfg(unix)]
    let current = open_no_follow(path)?;
    mark_verified_file_for_removal_guarded(root, parent, path, expected, &current, failpoint)?;
    drop(current);
    hit_after_namespace_handle_close(failpoint, NamespaceMutationKind::FileRemove, path)?;
    require_path_entry_absent(path, "verified control record after cleanup")?;
    hit_after_namespace_mutation(failpoint, NamespaceMutationKind::FileRemove, path)?;
    Ok(())
}

fn identity_bound_generation_name(
    prefix: &str,
    binding: &PendingGenerationIdentity,
    identity: &StableFileIdentity,
) -> String {
    format!(
        "{prefix}{}-{:016x}-{:016x}",
        pending_generation_name(
            binding.generation,
            &binding.transaction_id,
            binding.operation,
            &binding.intent_hash,
        ),
        identity.platform_a,
        identity.platform_b
    )
}

fn parse_identity_bound_generation_name(
    name: &str,
    prefix: &str,
    label: &str,
) -> io::Result<(PendingGenerationIdentity, u64, u64)> {
    let encoded = name
        .strip_prefix(prefix)
        .ok_or_else(|| invalid_data(&format!("{label} has the wrong namespace prefix")))?;
    let (pending_and_volume, object) = encoded
        .rsplit_once('-')
        .ok_or_else(|| invalid_data(&format!("{label} object identity is missing")))?;
    let (pending, volume) = pending_and_volume
        .rsplit_once('-')
        .ok_or_else(|| invalid_data(&format!("{label} volume identity is missing")))?;
    if volume.len() != 16 || object.len() != 16 {
        return Err(invalid_data(&format!(
            "{label} filesystem identity width is malformed"
        )));
    }
    let binding = parse_pending_generation_name(pending)?;
    let platform_a = u64::from_str_radix(volume, 16)
        .map_err(|_| invalid_data(&format!("{label} volume identity is malformed")))?;
    let platform_b = u64::from_str_radix(object, 16)
        .map_err(|_| invalid_data(&format!("{label} object identity is malformed")))?;
    let expected = identity_bound_generation_name(
        prefix,
        &binding,
        &StableFileIdentity {
            length: 0,
            modified: None,
            platform_a,
            platform_b,
            links: 0,
        },
    );
    if expected != name {
        return Err(invalid_data(&format!("{label} name is noncanonical")));
    }
    Ok((binding, platform_a, platform_b))
}

fn publication_generation_name(
    binding: &PendingGenerationIdentity,
    identity: &StableFileIdentity,
) -> String {
    identity_bound_generation_name(PUBLICATION_GENERATION_PREFIX, binding, identity)
}

fn cleanup_tombstone_name(binding: &CleanupBinding, identity: &StableFileIdentity) -> String {
    match binding {
        CleanupBinding::Generation(generation) => {
            identity_bound_generation_name(CLEANUP_GENERATION_PREFIX, generation, identity)
        }
        CleanupBinding::Baseline => format!(
            "{CLEANUP_BASELINE_PREFIX}{:016x}-{:016x}",
            identity.platform_a, identity.platform_b
        ),
    }
}

fn validate_identity_bound_directory(
    path: &Path,
    expected_a: u64,
    expected_b: u64,
    label: &str,
) -> io::Result<StableFileIdentity> {
    reject_link_or_reparse(path)?;
    let handle = open_directory_no_follow(path)?;
    let identity = stable_identity(&handle)?;
    if identity.platform_a != expected_a || identity.platform_b != expected_b {
        return Err(invalid_data(&format!(
            "{label} name does not bind its directory identity"
        )));
    }
    Ok(identity)
}

fn validate_publication_carrier_identity(
    path: &Path,
) -> io::Result<(StableFileIdentity, PendingGenerationIdentity)> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_data("publication carrier name is not canonical UTF-8"))?;
    let (binding, expected_a, expected_b) = parse_identity_bound_generation_name(
        name,
        PUBLICATION_GENERATION_PREFIX,
        "publication carrier",
    )?;
    let identity =
        validate_identity_bound_directory(path, expected_a, expected_b, "publication carrier")?;
    Ok((identity, binding))
}

fn validate_cleanup_tombstone_identity(
    path: &Path,
    kind: CleanupNamespaceKind,
) -> io::Result<(StableFileIdentity, CleanupBinding)> {
    let (binding, expected_a, expected_b) = parse_cleanup_tombstone_identity(path, kind)?;
    let identity =
        validate_identity_bound_directory(path, expected_a, expected_b, "cleanup tombstone")?;
    Ok((identity, binding))
}

fn parse_cleanup_tombstone_identity(
    path: &Path,
    kind: CleanupNamespaceKind,
) -> io::Result<(CleanupBinding, u64, u64)> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_data("cleanup tombstone name is not canonical UTF-8"))?;
    match kind {
        CleanupNamespaceKind::Generation => {
            let (binding, expected_a, expected_b) = parse_identity_bound_generation_name(
                name,
                CLEANUP_GENERATION_PREFIX,
                "generation cleanup tombstone",
            )?;
            Ok((CleanupBinding::Generation(binding), expected_a, expected_b))
        }
        CleanupNamespaceKind::Baseline => {
            let encoded = name.strip_prefix(CLEANUP_BASELINE_PREFIX).ok_or_else(|| {
                invalid_data("baseline cleanup tombstone has the wrong namespace prefix")
            })?;
            let (platform_a, platform_b) = encoded
                .split_once('-')
                .ok_or_else(|| invalid_data("baseline cleanup tombstone identity is malformed"))?;
            if platform_a.len() != 16 || platform_b.len() != 16 {
                return Err(invalid_data(
                    "baseline cleanup tombstone identity width is malformed",
                ));
            }
            let expected_a = u64::from_str_radix(platform_a, 16).map_err(|_| {
                invalid_data("baseline cleanup tombstone volume identity is malformed")
            })?;
            let expected_b = u64::from_str_radix(platform_b, 16).map_err(|_| {
                invalid_data("baseline cleanup tombstone object identity is malformed")
            })?;
            let synthetic = StableFileIdentity {
                length: 0,
                modified: None,
                platform_a: expected_a,
                platform_b: expected_b,
                links: 0,
            };
            if cleanup_tombstone_name(&CleanupBinding::Baseline, &synthetic) != name {
                return Err(invalid_data(
                    "baseline cleanup tombstone name is noncanonical",
                ));
            }
            Ok((CleanupBinding::Baseline, expected_a, expected_b))
        }
    }
}

fn validate_cleanup_tree_subset(
    root: &Path,
    path: &Path,
    kind: CleanupNamespaceKind,
) -> io::Result<BTreeMap<String, StableFileIdentity>> {
    let allowed_directories = BTreeSet::from([
        COMPONENTS_DIR_NAME.to_owned(),
        format!("{COMPONENTS_DIR_NAME}/n5-fixture"),
    ]);
    let mut allowed_files = BTreeSet::from([
        format!("{COMPONENTS_DIR_NAME}/{FIXTURE_STATE_PATH}"),
        format!("{COMPONENTS_DIR_NAME}/{FIXTURE_HISTORY_PATH}"),
    ]);
    match kind {
        CleanupNamespaceKind::Generation => {
            allowed_files.extend([
                PREPARE_FILE_NAME.to_owned(),
                PREPARE_TEMP_FILE_NAME.to_owned(),
                COMMIT_FILE_NAME.to_owned(),
                COMMIT_TEMP_FILE_NAME.to_owned(),
            ]);
        }
        CleanupNamespaceKind::Baseline => {
            allowed_files.insert(BASELINE_MANIFEST_FILE_NAME.to_owned());
        }
    }

    let mut identities = BTreeMap::new();
    let mut budget = TreeBudget::default();
    let mut stack = vec![(path.to_path_buf(), String::new())];
    while let Some((directory, prefix)) = stack.pop() {
        for entry in read_directory_bounded(&directory, &DEFAULT_LIMITS)? {
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| invalid_data("cleanup tree name is not canonical UTF-8"))?;
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            budget.observe(&relative, &DEFAULT_LIMITS)?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if is_link_or_reparse(&entry.path(), &metadata) {
                return Err(invalid_data(
                    "cleanup tree contains a link or reparse point",
                ));
            }
            let handle = if metadata.is_dir() {
                if !allowed_directories.contains(&relative) {
                    return Err(invalid_data(
                        "cleanup tree contains an unregistered directory",
                    ));
                }
                stack.push((entry.path(), relative.clone()));
                open_directory_no_follow(&entry.path())?
            } else if metadata.is_file() {
                if !allowed_files.contains(&relative) {
                    return Err(invalid_data("cleanup tree contains an unregistered file"));
                }
                let file = open_no_follow(&entry.path())?;
                let identity = stable_identity(&file)?;
                require_single_link(&identity, "cleanup-tree file")?;
                if identity.length > DEFAULT_LIMITS.max_component_bytes {
                    return Err(invalid_data("cleanup-tree file exceeds its byte bound"));
                }
                file
            } else {
                return Err(invalid_data("cleanup tree contains a non-file entry"));
            };
            if identities
                .insert(relative, stable_identity(&handle)?)
                .is_some()
            {
                return Err(invalid_data("cleanup tree contains a duplicate entry"));
            }
        }
    }
    let _ = root;
    Ok(identities)
}

#[cfg(windows)]
fn remove_tombstone_contents_by_handle(
    directory: &Path,
    prefix: &str,
    identities: &BTreeMap<String, StableFileIdentity>,
    failpoint: &mut Option<Box<dyn OperationFailpoint>>,
) -> io::Result<()> {
    let mut entries = read_directory_bounded(directory, &DEFAULT_LIMITS)?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid_data("cleanup entry is not canonical UTF-8"))?;
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let expected = identities
            .get(&relative)
            .ok_or_else(|| invalid_data("cleanup entry appeared after tombstone validation"))?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if is_link_or_reparse(&entry.path(), &metadata) {
            return Err(invalid_data("cleanup entry became a link or reparse point"));
        }
        let current = open_mutation_handle(&entry.path(), metadata.is_dir())?;
        if !same_file_object(&stable_identity(&current)?, expected) {
            return Err(invalid_data(
                "cleanup entry identity changed after tombstone validation",
            ));
        }
        if metadata.is_dir() {
            remove_tombstone_contents_by_handle(&entry.path(), &relative, identities, failpoint)?;
            hit_namespace_mutation(
                failpoint,
                NamespaceMutationKind::DirectoryRemove,
                &entry.path(),
                None,
            )?;
        } else {
            require_single_link(&stable_identity(&current)?, "cleanup-tree file")?;
            hit_namespace_mutation(
                failpoint,
                NamespaceMutationKind::FileRemove,
                &entry.path(),
                None,
            )?;
        }
        mark_opened_object_for_removal(&current)?;
        drop(current);
        hit_after_namespace_handle_close(
            failpoint,
            if metadata.is_dir() {
                NamespaceMutationKind::DirectoryRemove
            } else {
                NamespaceMutationKind::FileRemove
            },
            &entry.path(),
        )?;
        require_path_entry_absent(&entry.path(), "cleanup entry after handle disposition")?;
        hit_after_namespace_mutation(
            failpoint,
            if metadata.is_dir() {
                NamespaceMutationKind::DirectoryRemove
            } else {
                NamespaceMutationKind::FileRemove
            },
            &entry.path(),
        )?;
    }
    Ok(())
}

fn remove_verified_directory_tree_guarded(
    root: &Path,
    parent: &Path,
    path: &Path,
    expected: &StableFileIdentity,
    kind: CleanupNamespaceKind,
    binding: &CleanupBinding,
    failpoint: &mut Option<Box<dyn OperationFailpoint>>,
) -> io::Result<()> {
    let guard = AncestorGuard::acquire(root, parent)?;
    #[cfg(unix)]
    {
        let current = open_directory_no_follow(path)?;
        if !same_file_object(&stable_identity(&current)?, expected) {
            return Err(invalid_data(
                "control directory cleanup target identity changed",
            ));
        }
        let _ = (guard, kind, binding, failpoint);
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private N5-A directory cleanup is disabled on Unix because renameat/unlinkat cannot bind the final object identity",
        ));
    }
    #[cfg(windows)]
    {
        let current = open_mutation_handle(path, true)?;
        if !same_file_object(&stable_identity(&current)?, expected) {
            return Err(invalid_data(
                "control directory cleanup target identity changed",
            ));
        }
        guard.verify()?;
        let tombstone = parent.join(cleanup_tombstone_name(binding, expected));
        let already_tombstone = path == tombstone;
        if !already_tombstone {
            if path_entry_exists(&tombstone)? {
                return Err(invalid_data(
                    "identity-bound cleanup tombstone already exists",
                ));
            }
            hit_namespace_mutation(
                failpoint,
                NamespaceMutationKind::DirectoryRename,
                path,
                Some(&tombstone),
            )?;
            let mutation_parent = open_directory_publication_parent(parent)?;
            if !same_file_object(
                &stable_identity(&mutation_parent)?,
                &stable_identity(guard.directory_handle()?)?,
            ) {
                return Err(invalid_data(
                    "cleanup parent mutation handle changed directory identity",
                ));
            }
            rename_opened_object_by_handle(&current, &mutation_parent, &tombstone)?;
        } else {
            let (found_binding, expected_a, expected_b) =
                parse_cleanup_tombstone_identity(&tombstone, kind)?;
            let found_identity = stable_identity(&current)?;
            if found_identity.platform_a != expected_a
                || found_identity.platform_b != expected_b
                || !same_file_object(&found_identity, expected)
                || &found_binding != binding
            {
                return Err(invalid_data(
                    "cleanup tombstone binding changed before resumed deletion",
                ));
            }
        }
        let identities = validate_cleanup_tree_subset(root, &tombstone, kind)?;
        hit_namespace_mutation(
            failpoint,
            NamespaceMutationKind::DirectoryRemove,
            &tombstone,
            None,
        )?;
        remove_tombstone_contents_by_handle(&tombstone, "", &identities, failpoint)?;
        mark_opened_object_for_removal(&current)?;
        drop(current);
        hit_after_namespace_handle_close(
            failpoint,
            NamespaceMutationKind::DirectoryRemove,
            &tombstone,
        )?;
        require_path_entry_absent(&tombstone, "identity-bound cleanup tombstone")?;
        hit_after_namespace_mutation(
            failpoint,
            NamespaceMutationKind::DirectoryRemove,
            &tombstone,
        )?;
        guard.verify()?;
        Ok(())
    }
}

#[cfg(windows)]
fn cleanup_failed_publication_carrier(
    root: &Path,
    binding: &PendingGenerationIdentity,
) -> io::Result<()> {
    let Some(carrier) = find_publication_carrier(root, binding)? else {
        return Ok(());
    };
    let (identity, found_binding) = validate_publication_carrier_identity(&carrier)?;
    if found_binding != *binding {
        return Err(invalid_data(
            "failed-publication cleanup carrier binding changed",
        ));
    }
    validate_cleanup_tree_subset(root, &carrier, CleanupNamespaceKind::Generation)?;
    let parent = publication_dir(root);
    let mut no_failpoint = None;
    remove_verified_directory_tree_guarded(
        root,
        &parent,
        &carrier,
        &identity,
        CleanupNamespaceKind::Generation,
        &CleanupBinding::Generation(binding.clone()),
        &mut no_failpoint,
    )?;
    sync_directory(&parent)?;
    sync_directory(&generations_dir(root))
}

#[cfg(not(windows))]
fn cleanup_failed_publication_carrier(
    _root: &Path,
    _binding: &PendingGenerationIdentity,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "failed-publication carrier cleanup is unavailable on this platform",
    ))
}

#[cfg(windows)]
fn publish_generation_directory_guarded(
    root: &Path,
    parent_guard: AncestorGuard,
    source: &Path,
    target: &Path,
    expected_identity: &StableFileIdentity,
    binding: &PendingGenerationIdentity,
    failpoint: &mut Option<Box<dyn OperationFailpoint>>,
) -> io::Result<()> {
    let parent = parent_guard.directory_path()?.to_path_buf();
    let publication_directory = publication_dir(root);
    if (source.parent() != Some(parent.as_path())
        && source.parent() != Some(&publication_directory))
        || target.parent() != Some(parent.as_path())
    {
        return Err(invalid_data(
            "generation publication paths are outside the bounded carrier namespace",
        ));
    }
    require_path_entry_absent(target, "generation visibility target")?;
    parent_guard.verify()?;

    if !path_entry_exists(&publication_directory)? {
        create_directory_guarded(root, &parent, &publication_directory)?;
        sync_directory(&publication_directory)?;
        sync_directory(&parent)?;
    } else {
        reject_link_or_reparse(&publication_directory)?;
        if !fs::symlink_metadata(&publication_directory)?.is_dir() {
            return Err(invalid_data(
                "private publication namespace is not a directory",
            ));
        }
    }
    let publication_guard = AncestorGuard::acquire(root, &publication_directory)?;
    let publication_parent = open_directory_publication_parent(&publication_directory)?;
    if !same_file_object(
        &stable_identity(&publication_parent)?,
        &stable_identity(publication_guard.directory_handle()?)?,
    ) {
        return Err(invalid_data(
            "publication parent mutation handle changed directory identity",
        ));
    }

    let expected_carrier =
        publication_directory.join(publication_generation_name(binding, expected_identity));
    let source_is_carrier = source == expected_carrier;
    if source_is_carrier {
        let (carrier_identity, carrier_binding) = validate_publication_carrier_identity(source)?;
        if !same_file_object(&carrier_identity, expected_identity) || &carrier_binding != binding {
            return Err(invalid_data(
                "publication carrier does not match the bound generation",
            ));
        }
    } else {
        let source_name = source
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid_data("generation publication source is not canonical UTF-8"))?;
        if parse_pending_generation_name(source_name)? != *binding {
            return Err(invalid_data(
                "generation publication source name conflicts with its binding",
            ));
        }
        require_path_entry_absent(&expected_carrier, "private publication carrier")?;
    }

    let pre_bind_tree = PinnedGenerationTree::capture(source)?;
    pre_bind_tree.revalidate(source)?;
    let source_handle = open_generation_publication_handle(source)?;
    let source_identity = stable_identity(&source_handle)?;
    if !same_file_object(&source_identity, expected_identity) {
        return Err(invalid_data(
            "generation publication source identity changed before carrier binding",
        ));
    }
    // Classic FileRenameInformation refuses a directory rename while a
    // descendant handle remains open. The exact child snapshot was verified
    // above; close only those descendants while retaining the source-directory
    // mutation handle and both destination-parent pins continuously.
    drop(pre_bind_tree);

    let carrier = if source_is_carrier {
        source.to_path_buf()
    } else {
        rename_opened_object_by_handle(&source_handle, &publication_parent, &expected_carrier)
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("handle-bound private-carrier rename failed: {error}"),
                )
            })?;
        require_path_entry_absent(source, "pending generation after carrier binding")?;
        expected_carrier
    };
    let carrier_identity_pin = open_verification_handle_while_mutating(&carrier, true)?;
    if !same_file_object(&stable_identity(&carrier_identity_pin)?, expected_identity)
        || !same_file_object(&stable_identity(&source_handle)?, expected_identity)
    {
        return Err(invalid_data(
            "publication carrier identity-pin transfer changed the source object",
        ));
    }

    // Reopen and pin the complete immutable child tree after the identity-bound
    // carrier bind. These handles span the deterministic late-substitution
    // seam and the final validation immediately before visibility.
    let pinned_tree = PinnedGenerationTree::capture(&carrier)?;

    hit_namespace_mutation(
        failpoint,
        NamespaceMutationKind::DirectoryRename,
        &carrier,
        Some(target),
    )?;
    hit_before_write_through_visibility(failpoint, &carrier, target)?;
    if let Err(validation_error) = pinned_tree.revalidate(&carrier) {
        let quarantined = pinned_tree.quarantine_new_links()?;
        drop(pinned_tree);
        drop(source_handle);
        if quarantined > 0 {
            let cleanup_result = remove_verified_directory_tree_guarded(
                root,
                &publication_directory,
                &carrier,
                expected_identity,
                CleanupNamespaceKind::Generation,
                &CleanupBinding::Generation(binding.clone()),
                failpoint,
            );
            if let Err(cleanup_error) = cleanup_result {
                return Err(io::Error::new(
                    validation_error.kind(),
                    format!(
                        "{validation_error}; late-link quarantine remained resumable: {cleanup_error}"
                    ),
                ));
            }
        }
        return Err(validation_error);
    }

    // MoveFileExW requires pathname-based source admission. The handle-bound
    // first rename has already isolated the exact object under an identity-
    // encoded private carrier, and the post-seam tree admission above ran while
    // every child and the carrier handle were pinned. The cooperative BaseLease
    // keeps other MemoryX writers out; handles are closed only for the bounded
    // write-through move itself. The source-directory identity handle remains
    // live and shares delete because Windows requires that sharing mode for
    // MoveFileExW to move the open directory. Hostile non-cooperative namespace mutation is
    // outside this private fixture contract and remains an explicit residual.
    drop(pinned_tree);
    // Transfer from the DELETE-capable native-rename handle to the verified
    // read/share-delete identity pin before MoveFileExW performs its own open.
    // Both handles overlapped and were proven to name the same object.
    drop(source_handle);
    // These source-parent handles deny delete sharing and would prevent the
    // carrier entry from leaving `.publication`. The exact target parent
    // remains pinned by `parent_guard` through the write-through mutation.
    drop(publication_parent);
    drop(publication_guard);
    let parent_pin = parent_guard.into_final_pin()?;
    if parent_pin.path() != parent {
        return Err(invalid_data(
            "generation destination-parent handoff changed its path",
        ));
    }
    let move_result = move_directory_write_through(&carrier, target).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("write-through carrier visibility move failed: {error}"),
        )
    });
    match move_result {
        Ok(()) => {}
        Err(error) => {
            if path_entry_exists(target)? {
                require_path_entry_absent(&carrier, "publication carrier after visibility")?;
                let published = open_verification_handle_while_mutating(target, true)?;
                if same_file_object(&stable_identity(&published)?, expected_identity)
                    && same_file_object(&stable_identity(&carrier_identity_pin)?, expected_identity)
                {
                    parent_pin.verify()?;
                    return Ok(());
                }
                return Err(invalid_data(
                    "write-through visibility error exposed an unexpected target object",
                ));
            }
            if path_entry_exists(&carrier)? {
                let (carrier_identity, carrier_binding) =
                    validate_publication_carrier_identity(&carrier)?;
                if !same_file_object(&carrier_identity, expected_identity)
                    || carrier_binding != *binding
                {
                    return Err(invalid_data(
                        "write-through visibility error changed the private carrier",
                    ));
                }
                require_path_entry_absent(target, "generation target after failed visibility")?;
                return Err(error);
            }
            return Err(invalid_data(
                "write-through visibility error left neither exact carrier nor target",
            ));
        }
    }

    require_path_entry_absent(&carrier, "publication carrier after visibility")?;
    let published = open_verification_handle_while_mutating(target, true)?;
    if !same_file_object(&stable_identity(&published)?, expected_identity)
        || !same_file_object(&stable_identity(&carrier_identity_pin)?, expected_identity)
    {
        return Err(invalid_data(
            "write-through visibility did not expose the bound generation object",
        ));
    }
    parent_pin.verify()?;
    drop(published);
    drop(carrier_identity_pin);
    Ok(())
}

fn atomic_rename_guarded(
    parent_guard: &AncestorGuard,
    source: &Path,
    target: &Path,
    expected_identity: &StableFileIdentity,
    failpoint: &mut Option<Box<dyn OperationFailpoint>>,
) -> io::Result<()> {
    let parent = parent_guard.directory_path()?;
    if source.parent() != Some(parent) || target.parent() != Some(parent) {
        return Err(invalid_data(
            "guarded publication paths do not share the pinned parent",
        ));
    }
    if path_entry_exists(target)? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "guarded publication target already exists",
        ));
    }
    #[cfg(unix)]
    {
        parent_guard.verify()?;
        reject_link_or_reparse(source)?;
        let source_is_directory = fs::symlink_metadata(source)?.is_dir();
        let source_handle = if source_is_directory {
            open_directory_no_follow(source)?
        } else {
            open_no_follow(source)?
        };
        if !same_file_object(&stable_identity(&source_handle)?, expected_identity) {
            return Err(invalid_data(
                "guarded publication source identity changed before rename",
            ));
        }
        let _ = failpoint;
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private N5-A existing-object publication is disabled on Unix because renameat is namespace-bound rather than object-bound",
        ));
    }
    #[cfg(windows)]
    {
        parent_guard.verify()?;
        reject_link_or_reparse(source)?;
        let source_is_directory = fs::symlink_metadata(source)?.is_dir();
        let source_handle = open_mutation_handle(source, source_is_directory)?;
        let before = stable_identity(&source_handle)?;
        if !same_file_object(&before, expected_identity) {
            return Err(invalid_data(
                "guarded publication source identity changed before rename",
            ));
        }
        if !source_is_directory {
            require_single_link(&before, "control record before publication")?;
        }
        let mutation_kind = if source_is_directory {
            NamespaceMutationKind::DirectoryRename
        } else {
            NamespaceMutationKind::FileRename
        };
        hit_namespace_mutation(failpoint, mutation_kind, source, Some(target))?;
        rename_opened_object_same_parent_by_handle(&source_handle, target)?;
        let after = stable_identity(&source_handle)?;
        if !same_file_object(&after, expected_identity) {
            if !path_entry_exists(source)? {
                let _ = rename_opened_object_same_parent_by_handle(&source_handle, source);
            }
            return Err(invalid_data(
                "guarded publication source identity changed at rename",
            ));
        }
        if !source_is_directory && after.links != expected_identity.links {
            if !path_entry_exists(source)? {
                rename_opened_object_same_parent_by_handle(&source_handle, source)?;
                // The verified temporary link is module-owned. Removing only
                // that link through the still-live object handle prevents a
                // new external hard link from becoming a canonical immutable
                // record; the external link itself is not modified.
                mark_opened_object_for_removal(&source_handle)?;
            }
            return Err(invalid_data(
                "guarded publication source link count changed at rename",
            ));
        }
        let published_handle =
            open_verification_handle_while_mutating(target, source_is_directory)?;
        if !same_file_object(&stable_identity(&published_handle)?, expected_identity) {
            return Err(invalid_data(
                "guarded publication did not expose the verified source object",
            ));
        }
        parent_guard.verify()?;
        Ok(())
    }
}

fn atomic_rename_under(
    root: &Path,
    source: &Path,
    target: &Path,
    failpoint: &mut Option<Box<dyn OperationFailpoint>>,
) -> io::Result<()> {
    let parent = source
        .parent()
        .ok_or_else(|| invalid_data("publication source has no parent"))?;
    if target.parent() != Some(parent) {
        return Err(invalid_data(
            "private publication must remain within one pinned directory",
        ));
    }
    let guard = AncestorGuard::acquire(root, parent)?;
    reject_link_or_reparse(source)?;
    let source_is_directory = fs::symlink_metadata(source)?.is_dir();
    let source_handle = if source_is_directory {
        open_directory_no_follow(source)?
    } else {
        open_no_follow(source)?
    };
    let identity = stable_identity(&source_handle)?;
    if !source_is_directory {
        require_single_link(&identity, "control record before publication")?;
    }
    drop(source_handle);
    atomic_rename_guarded(&guard, source, target, &identity, failpoint).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "handle-bound publication {} -> {} failed: {error}",
                source.display(),
                target.display()
            ),
        )
    })
}

fn read_record<T: for<'de> Deserialize<'de> + Serialize>(
    path: &Path,
    label: &str,
) -> io::Result<T> {
    decode_record(
        &read_bytes_bounded(path, DEFAULT_LIMITS.max_record_bytes)?,
        label,
    )
}

fn read_record_under<T: for<'de> Deserialize<'de> + Serialize>(
    root: &Path,
    path: &Path,
    label: &str,
) -> io::Result<T> {
    decode_record(
        &read_bytes_bounded_under(root, path, DEFAULT_LIMITS.max_record_bytes)?,
        label,
    )
}

fn read_bytes_bounded_under(root: &Path, path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    let (file, identity) = open_verified_control_record(root, path, "control record")?;
    read_bounded_from_verified_file(file, identity, max_bytes)
}

fn read_bytes_bounded(path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    let file = open_no_follow(path)?;
    let identity = stable_identity(&file)?;
    read_bounded_from_verified_file(file, identity, max_bytes)
}

fn read_bounded_from_verified_file(
    mut file: File,
    identity: StableFileIdentity,
    max_bytes: u64,
) -> io::Result<Vec<u8>> {
    if identity.length > max_bytes {
        return Err(invalid_data("bounded record/file read exceeds its limit"));
    }
    let capacity = usize::try_from(identity.length)
        .map_err(|_| invalid_data("bounded read allocation does not fit usize"))?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut buffer = [0u8; STREAM_BUFFER_BYTES];
    let mut total = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| invalid_data("bounded read byte count overflow"))?;
        if total > max_bytes {
            return Err(invalid_data("file grew beyond the bounded read limit"));
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    if total != identity.length || stable_identity(&file)? != identity {
        return Err(invalid_data("bounded record/file changed while being read"));
    }
    Ok(bytes)
}

fn encode_record<T: Serialize>(record: &T) -> io::Result<Vec<u8>> {
    let bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
    if bytes.len() as u64 > DEFAULT_LIMITS.max_record_bytes {
        return Err(invalid_data(
            "canonical record exceeds the N5-A record limit",
        ));
    }
    Ok(bytes)
}

fn decode_record<T: for<'de> Deserialize<'de> + Serialize>(
    bytes: &[u8],
    label: &str,
) -> io::Result<T> {
    if bytes.len() as u64 > DEFAULT_LIMITS.max_record_bytes {
        return Err(invalid_data(
            "canonical record exceeds the N5-A record limit",
        ));
    }
    let record: T = serde_json::from_slice(bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid operation transaction {label} record: {error}"),
        )
    })?;
    if encode_record(&record)? != bytes {
        return Err(invalid_data(
            "operation transaction record is not in canonical v1 encoding",
        ));
    }
    Ok(record)
}

fn record_crc<T: Serialize>(label: &str, body: &T) -> io::Result<u32> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(PRIVATE_CODEC_ID.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(label.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&serde_json::to_vec(body).map_err(io::Error::other)?);
    Ok(crc32(&bytes))
}

fn format_crc(record: &FormatRecord) -> io::Result<u32> {
    #[derive(Serialize)]
    struct FormatCrcBody<'a> {
        magic: &'a str,
        version: u32,
        codec: &'a str,
        component_registry: &'a str,
        limits: &'a str,
    }
    record_crc(
        "format",
        &FormatCrcBody {
            magic: &record.magic,
            version: record.version,
            codec: &record.codec,
            component_registry: &record.component_registry,
            limits: &record.limits,
        },
    )
}

fn hash_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn empty_hash() -> String {
    hash_hex(&[])
}

fn is_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn path_entry_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn require_path_entry_absent(path: &Path, label: &str) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(invalid_data(&format!(
            "{label} remained or was replaced by a namespace entry"
        ))),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("{label} absence could not be established: {error}"),
        )),
    }
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> io::Result<()> {
    // Windows does not provide a portable directory fsync equivalent that is
    // accepted for ordinary directory handles. Transaction commit stages the
    // complete immutable tree first and publishes the carrier through the
    // verified directory handle. SetFileInformationByHandle provides object
    // identity continuity, not a physical metadata-flush guarantee; physical
    // sudden-power-loss validation therefore remains open.
    Ok(())
}

#[cfg(not(windows))]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(test)]
struct EnvironmentFailpoint;

#[cfg(test)]
fn environment_fail_action(label: &str) -> io::Result<()> {
    match std::env::var("MEMORYX_N5_FAIL_ACTION").as_deref() {
        Ok("abort") => std::process::abort(),
        Ok("error") | Err(_) => Err(io::Error::other(format!(
            "injected deterministic environment failure at {label}"
        ))),
        Ok(_) => Err(invalid_data("unknown environment failpoint action")),
    }
}

#[cfg(test)]
impl OperationFailpoint for EnvironmentFailpoint {
    fn hit(&mut self, stage: OperationStage, occurrence: usize) -> io::Result<()> {
        let specification = std::env::var("MEMORYX_N5_FAILPOINT").unwrap_or_default();
        let selected = format!("{}#{occurrence}", stage.as_str());
        if specification != selected {
            return Ok(());
        }
        environment_fail_action(&selected)
    }

    fn after_namespace_mutation(
        &mut self,
        kind: NamespaceMutationKind,
        path: &Path,
    ) -> io::Result<()> {
        let specification = std::env::var("MEMORYX_N5_NAMESPACE_AFTER").unwrap_or_default();
        if specification.is_empty() {
            return Ok(());
        }
        let normalized = path.to_string_lossy().replace('\\', "/");
        if path.file_name().and_then(|name| name.to_str()) != Some(specification.as_str())
            && !normalized.ends_with(&format!("/{specification}"))
        {
            return Ok(());
        }
        environment_fail_action(&format!("{kind:?}:{specification}"))
    }

    fn before_write_through_visibility(
        &mut self,
        _source: &Path,
        _target: &Path,
    ) -> io::Result<()> {
        if std::env::var("MEMORYX_N5_VISIBILITY_SEAM").as_deref() != Ok("before_move") {
            return Ok(());
        }
        environment_fail_action("before_write_through_visibility")
    }
}

// ============================================================================
// N5-B P0-C production-v2 direct-ingest codecs and owner-bound coordinator
// ============================================================================

const PRODUCTION_FORMAT_SCHEMA: &str = "memoryx.operation-txn.production-format.v2";
const PRODUCTION_CODEC_ID: &str = "memoryx.canonical-json-ascii.v1";
const PRODUCTION_REGISTRY_ID: &str = "memoryx.production-core-registry.v2";
const PRODUCTION_DIGEST_ID: &str = "memoryx.logical-state-digest.v2";
const PRODUCTION_COMPONENT_ROOT_ID: &str = "memoryx.physical-component-root.v1";
const PRODUCTION_ORPHAN_DIGEST_ID: &str = "memoryx.cas-orphan-inventory.v1";
const PRODUCTION_LIMITS_ID: &str = "memoryx.production-direct-ingest-limits.v1";
const PRODUCTION_LEGACY_LAYOUT_ID: &str = "memoryx.legacy-production-layout.v1";
const PRODUCTION_DOWNGRADE_POLICY_ID: &str = "memoryx.one-way-until-historical-refusal.v1";
const PRODUCTION_MINIMUM_WRITER: &str = "memoryx-production-operation-txn-v2";
const PRODUCTION_BASE_BINDING_ID: &str = "memoryx.base-binding.v1";
const PRODUCTION_INTENT_ID: &str = "memoryx.direct-ingest-intent.v1";
const PRODUCTION_ENVELOPE_ID: &str = "memoryx.direct-ingest-envelope.v1";
const PRODUCTION_RECEIPT_SCHEMA: &str = "memoryx.direct-ingest-receipt.v1";
const PRODUCTION_FAILURE_SCHEMA: &str = "memoryx.direct-ingest-failure.v1";
const PRODUCTION_STARTUP_SCHEMA: &str = "memoryx.startup-no-repair-admission.v1";
const PRODUCTION_HISTORY_EVENT_ID: &str = "memoryx.history-event-id.v1";
const PRODUCTION_HISTORY_SEMANTIC_ID: &str = "memoryx.history-event-semantic.v1";
const PRODUCTION_HISTORY_SCHEMA: &str = "memoryx.history.transaction-once.v1";
const PRODUCTION_HISTORY_LEAF_ID: &str = "memoryx.history-digest-leaf.v1";
const PRODUCTION_PROVENANCE_LEAF_ID: &str = "memoryx.provenance-semantic-leaf.v1";
const PRODUCTION_METADATA_LEAF_ID: &str = "memoryx.metadata-semantic-leaf.v1";
const PRODUCTION_FORMAT_FILE_NAME: &str = "format.v2";
const PRODUCTION_BASELINE_FILE_NAME: &str = "baseline.v2";
const PRODUCTION_MIGRATION_FILE_NAME: &str = "migration.v2";
const PRODUCTION_FORMAT_TEMP_FILE_NAME: &str = "format.tmp";
const PRODUCTION_BASELINE_TEMP_FILE_NAME: &str = "baseline.tmp";
const PRODUCTION_MIGRATION_TEMP_FILE_NAME: &str = "migration.tmp";
const PRODUCTION_MAX_BODY_BYTES: u64 = 67_108_784;
const PRODUCTION_MAX_INTENT_BYTES: u64 = 67_108_864;
const PRODUCTION_MAX_BASE_BINDING_BYTES: u64 = 131_072;
const PRODUCTION_MAX_GENERATIONS: u64 = 4096;
const PRODUCTION_ROLLBACK_POLICY: &str = "rollback-before-first-production-commit-only";
const PRODUCTION_ABSENT_HASH: &str =
    "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
const PRODUCTION_ZERO_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";
const PRODUCTION_BATCH_OPERATION_REGISTRY_ID: &str =
    "memoryx.production-batch-edgefree-registry.v1";
const PRODUCTION_BATCH_LIMITS_ID: &str = "memoryx.production-batch-edgefree-limits.v1";
const PRODUCTION_BATCH_ITEM_ID: &str = "memoryx.batch-ingest-item.v1";
const PRODUCTION_BATCH_INTENT_ID: &str = "memoryx.batch-ingest-intent.v1";
const PRODUCTION_BATCH_ENVELOPE_ID: &str = "memoryx.batch-ingest-envelope.v1";
const PRODUCTION_BATCH_PROBE_ID: &str = "memoryx.batch-parent-probe.v1";
const PRODUCTION_BATCH_DECISION_ID: &str = "memoryx.batch-ingest-item-decision.v1";
const PRODUCTION_BATCH_PREFLIGHT_ID: &str = "memoryx.batch-ingest-preflight.v1";
const PRODUCTION_BATCH_RECEIPT_SCHEMA: &str = "memoryx.batch-ingest-receipt.v1";
const PRODUCTION_BATCH_HISTORY_EVENT_ID: &str = "memoryx.batch-history-event-id.v1";
const PRODUCTION_BATCH_HISTORY_SEMANTIC_ID: &str = "memoryx.batch-history-event-semantic.v1";
const PRODUCTION_BATCH_HISTORY_LEAF_ID: &str = "memoryx.batch-history-digest-leaf.v1";
const PRODUCTION_BATCH_MAX_ITEMS: usize = 16;
const PRODUCTION_BATCH_MAX_TOTAL_BODY_BYTES: u64 = 268_435_456;
const PRODUCTION_BATCH_MAX_TOTAL_PROJECTION_BYTES: u64 = 33_554_432;
const PRODUCTION_BATCH_MAX_APPEND_EXTENT_BYTES: u64 = 268_436_976;
const PRODUCTION_BATCH_REQUIRED_FREE_BYTES: u64 = 1_476_398_048;
const PRODUCTION_BATCH_MAX_DETACHED_BYTES: u64 = 536_870_912;
const PRODUCTION_BATCH_MAX_CONTROL_BYTES: u64 = 67_108_864;
const PRODUCTION_BATCH_MAX_STAGED_BYTES: u64 = 872_416_752;
const PRODUCTION_BATCH_MAX_INSTALL_SCRATCH_BYTES: u64 = 268_435_456;
const PRODUCTION_BATCH_MINIMUM_FREE_RESERVE_BYTES: u64 = 67_108_864;
const PRODUCTION_BATCH_MAX_PATH_BYTES: usize = 240;
const PRODUCTION_BATCH_MAX_TOTAL_PATH_BYTES: usize = 13_680;
const PRODUCTION_UPDATE_OPERATION_REGISTRY_ID: &str = "memoryx.production-update-atom-registry.v1";
const PRODUCTION_UPDATE_LIMITS_ID: &str = "memoryx.production-update-atom-limits.v1";
const PRODUCTION_UPDATE_INTENT_ID: &str = "memoryx.update-atom-intent.v1";
const PRODUCTION_UPDATE_ENVELOPE_ID: &str = "memoryx.update-atom-envelope.v1";
const PRODUCTION_UPDATE_PREPARE_SCHEMA: &str = "memoryx.update-atom-prepare.v1";
const PRODUCTION_UPDATE_MANIFEST_SCHEMA: &str = "memoryx.update-atom-generation-manifest.v1";
const PRODUCTION_UPDATE_RECEIPT_ID: &str = "memoryx.update-atom-receipt.v1";
const PRODUCTION_UPDATE_FAILURE_SCHEMA: &str = "memoryx.update-atom-failure.v1";
const PRODUCTION_UPDATE_DESCRIPTOR_SCHEMA: &str = "memoryx.update-atom-component-descriptor.v1";
const PRODUCTION_UPDATE_DESCRIPTOR_HASH_ID: &str = "memoryx.update-component-descriptor-hash.v1";
const PRODUCTION_UPDATE_COMPONENT_ROOT_ID: &str = "memoryx.update-atom-component-root.v1";
const PRODUCTION_UPDATE_RELATION_ID: &str = "memoryx.supersedes-relation-id.v1";
const PRODUCTION_UPDATE_HISTORY_EVENT_ID: &str = "memoryx.update-history-event-id.v1";
const PRODUCTION_UPDATE_HISTORY_SEMANTIC_ID: &str = "memoryx.update-history-event-semantic.v1";
const PRODUCTION_UPDATE_HISTORY_LEAF_ID: &str = "memoryx.update-history-provenance-leaf.v1";
const PRODUCTION_UPDATE_SPV1_SEMANTIC_ID: &str =
    "memoryx.successor-provenance-source-attachment-semantic.v1";
const PRODUCTION_UPDATE_SOURCE_ATTACHMENT_PREFIX: &str =
    "memoryx.atom-source-attachment-projection.v1|atom_id=";
const PRODUCTION_UPDATE_SOURCE_ATTACHMENT_PAYLOAD: &str = "|projection=";
const PRODUCTION_GRAPH_ATTRIBUTE_ID: &str = "memoryx.graph-edge-attributes.v1";
const PRODUCTION_GRAPH_LEAF_ID: &str = "memoryx.graph-semantic-leaf.v1";
const PRODUCTION_GRAPH_DELTA_SEMANTIC_ID: &str = "memoryx.graph-delta-semantic.v1";
const PRODUCTION_GRAPH_MANIFEST_SEMANTIC_ID: &str = "memoryx.graph-manifest-semantic.v1";
const PRODUCTION_UPDATE_MAX_PROJECTION_BYTES: u64 = 54_000_000;
const PRODUCTION_UPDATE_MAX_CONTROL_BYTES: u64 = 1_048_576;
const PRODUCTION_UPDATE_MAX_AGGREGATE_CONTROL_BYTES: u64 = 67_108_864;
const PRODUCTION_UPDATE_MAX_DESCRIPTOR_BYTES: u64 = 8_388_608;
const PRODUCTION_UPDATE_MAX_COMPONENT_BYTES: u64 = 268_435_456;
const PRODUCTION_UPDATE_MAX_TOTAL_BYTES: u64 = 536_870_912;
const PRODUCTION_UPDATE_MAX_PATH_COUNT: usize = 10;
const PRODUCTION_UPDATE_MAX_TOTAL_PATH_BYTES: usize = 2_400;
const PRODUCTION_UPDATE_COMPONENT_COUNT: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProductionRegistryEntry {
    order: u16,
    key: &'static str,
    mode: &'static str,
    target: Option<&'static str>,
    codec: &'static str,
    pair_id: Option<&'static str>,
}

const PRODUCTION_DIRECT_REGISTRY: &[ProductionRegistryEntry] = &[
    ProductionRegistryEntry {
        order: 5,
        key: "cas.segment-data.skf1.v1",
        mode: "anchor_present",
        target: Some("cas/seg_00000.dat"),
        codec: "memoryx.skf1.v0101",
        pair_id: None,
    },
    ProductionRegistryEntry {
        order: 10,
        key: "cas.staged-record.skf1.v1",
        mode: "orphan",
        target: None,
        codec: "memoryx.skf1.v0101",
        pair_id: None,
    },
    ProductionRegistryEntry {
        order: 20,
        key: "cas.orphan-descriptor.v1",
        mode: "orphan",
        target: None,
        codec: "memoryx.cas-orphan-descriptor.v1",
        pair_id: None,
    },
    ProductionRegistryEntry {
        order: 30,
        key: "cas.segment-index.idx1.v1",
        mode: "replace",
        target: Some("cas/seg_00000.idx"),
        codec: "memoryx.idx1.v0101",
        pair_id: None,
    },
    ProductionRegistryEntry {
        order: 40,
        key: "index.location-state.loc1.v1",
        mode: "replace",
        target: Some("index/location_state.bin"),
        codec: "memoryx.loc1.v0001",
        pair_id: Some("memoryx.location-idloc-pair.v1"),
    },
    ProductionRegistryEntry {
        order: 50,
        key: "index.idloc.idl1.v1",
        mode: "replace",
        target: Some("index/idloc.mmap"),
        codec: "memoryx.idl1.v0001",
        pair_id: Some("memoryx.location-idloc-pair.v1"),
    },
    ProductionRegistryEntry {
        order: 60,
        key: "index.lexicon.lex1.v1",
        mode: "replace",
        target: Some("index/terms.lex"),
        codec: "memoryx.lex1.implemented-v0001",
        pair_id: Some("memoryx.lexical-postings-pair.v1"),
    },
    ProductionRegistryEntry {
        order: 70,
        key: "index.postings.pst1.v1",
        mode: "replace",
        target: Some("index/terms.post"),
        codec: "memoryx.pst1.implemented-v0001",
        pair_id: Some("memoryx.lexical-postings-pair.v1"),
    },
    ProductionRegistryEntry {
        order: 90,
        key: "graph.manifest.v1",
        mode: "replace",
        target: Some("graph/graph.manifest"),
        codec: "memoryx.graph-manifest.grm1-v0101",
        pair_id: None,
    },
    ProductionRegistryEntry {
        order: 130,
        key: "meta.atom-state.met1.v1",
        mode: "replace",
        target: Some("meta/meta_state.bin"),
        codec: "memoryx.met1.v0001",
        pair_id: None,
    },
    ProductionRegistryEntry {
        order: 140,
        key: "meta.history-once.v1",
        mode: "replace",
        target: Some("meta/history.log"),
        codec: "memoryx.history.transaction-once.v1",
        pair_id: None,
    },
    ProductionRegistryEntry {
        order: 150,
        key: "meta.contexts.anchor.v1",
        mode: "anchor_absent",
        target: Some("meta/contexts.json"),
        codec: "memoryx.anchor.v1",
        pair_id: None,
    },
    ProductionRegistryEntry {
        order: 160,
        key: "index.embeddings.anchor.v1",
        mode: "anchor_absent",
        target: Some("index/embeddings.bin"),
        codec: "memoryx.anchor.v1",
        pair_id: None,
    },
    ProductionRegistryEntry {
        order: 170,
        key: "meta.sources.anchor.v1",
        mode: "anchor_absent",
        target: Some("meta/sources.jsonl"),
        codec: "memoryx.anchor.v1",
        pair_id: None,
    },
    ProductionRegistryEntry {
        order: 180,
        key: "meta.atom-sources.anchor.v1",
        mode: "anchor_absent",
        target: Some("meta/atom_sources.jsonl"),
        codec: "memoryx.anchor.v1",
        pair_id: None,
    },
    ProductionRegistryEntry {
        order: 190,
        key: "meta.predicates.anchor.v1",
        mode: "anchor_absent",
        target: Some("meta/predicates.jsonl"),
        codec: "memoryx.anchor.v1",
        pair_id: None,
    },
    ProductionRegistryEntry {
        order: 200,
        key: "meta.entities.anchor.v1",
        mode: "anchor_absent",
        target: Some("meta/entities.jsonl"),
        codec: "memoryx.anchor.v1",
        pair_id: None,
    },
    ProductionRegistryEntry {
        order: 210,
        key: "meta.relations.anchor.v1",
        mode: "anchor_absent",
        target: Some("meta/relations.jsonl"),
        codec: "memoryx.anchor.v1",
        pair_id: None,
    },
    ProductionRegistryEntry {
        order: 220,
        key: "meta.relation-resolutions.anchor.v1",
        mode: "anchor_absent",
        target: Some("meta/relation_tombstone_resolutions.jsonl"),
        codec: "memoryx.anchor.v1",
        pair_id: None,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectIngestResultKindV1 {
    Created,
    ReusedCommitted,
}

impl DirectIngestResultKindV1 {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::ReusedCommitted => "reused_committed",
        }
    }

    fn parse(value: &str) -> io::Result<Self> {
        match value {
            "created" => Ok(Self::Created),
            "reused_committed" => Ok(Self::ReusedCommitted),
            _ => Err(invalid_data("direct-ingest receipt result is unsupported")),
        }
    }
}

/// Canonical acknowledged result for the explicit direct-library P0-C carrier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectIngestReceiptV1 {
    pub result: DirectIngestResultKindV1,
    pub transaction_id: String,
    pub semantic_time_unix_ns: u64,
    pub intent_hash: [u8; 32],
    pub base_binding_hash: [u8; 32],
    pub committed_generation: u64,
    pub commit_hash: [u8; 32],
    pub logical_digest: [u8; 32],
    pub atom_id: [u8; 32],
    pub node_num: u64,
    pub history_event_id: Option<[u8; 32]>,
    canonical_bytes: Vec<u8>,
}

impl DirectIngestReceiptV1 {
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        result: DirectIngestResultKindV1,
        transaction_id: String,
        semantic_time_unix_ns: u64,
        intent_hash: [u8; 32],
        base_binding_hash: [u8; 32],
        committed_generation: u64,
        commit_hash: [u8; 32],
        logical_digest: [u8; 32],
        atom_id: [u8; 32],
        node_num: u64,
        history_event_id: Option<[u8; 32]>,
    ) -> io::Result<Self> {
        validate_production_uuid(&transaction_id)?;
        if semantic_time_unix_ns == 0 || committed_generation > PRODUCTION_MAX_GENERATIONS {
            return Err(invalid_data(
                "direct-ingest receipt numeric field is out of range",
            ));
        }
        match result {
            DirectIngestResultKindV1::Created => {
                if committed_generation == 0
                    || commit_hash == [0; 32]
                    || history_event_id.is_none_or(|value| value == [0; 32])
                {
                    return Err(invalid_data(
                        "created receipt requires a committed generation, commit, and history event",
                    ));
                }
            }
            DirectIngestResultKindV1::ReusedCommitted => {
                if history_event_id.is_some()
                    || (committed_generation == 0) != (commit_hash == [0; 32])
                {
                    return Err(invalid_data(
                        "reused receipt has an invalid history or generation binding",
                    ));
                }
            }
        }
        if intent_hash == [0; 32]
            || base_binding_hash == [0; 32]
            || logical_digest == [0; 32]
            || atom_id == [0; 32]
        {
            return Err(invalid_data(
                "direct-ingest receipt contains a zero identity",
            ));
        }

        let body = DirectIngestReceiptBodyWire {
            schema: PRODUCTION_RECEIPT_SCHEMA.to_owned(),
            version: 1,
            operation: "ingest".to_owned(),
            result: result.as_str().to_owned(),
            transaction_id: transaction_id.clone(),
            semantic_time_unix_ns,
            intent_hash: hex_lower(&intent_hash),
            base_binding_hash: hex_lower(&base_binding_hash),
            committed_generation,
            commit_hash: hex_lower(&commit_hash),
            logical_digest: hex_lower(&logical_digest),
            atom_id: hex_lower(&atom_id),
            node_num,
            history_event_id: history_event_id.as_ref().map(|value| hex_lower(value)),
            acknowledged: true,
            durability: "committed".to_owned(),
        };
        let canonical_bytes = encode_receipt_wire(&body)?;
        Ok(Self {
            result,
            transaction_id,
            semantic_time_unix_ns,
            intent_hash,
            base_binding_hash,
            committed_generation,
            commit_hash,
            logical_digest,
            atom_id,
            node_num,
            history_event_id,
            canonical_bytes,
        })
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let wire: DirectIngestReceiptWire = decode_production_json(bytes, "receipt")?;
        let expected_crc32 = wire.crc32.clone();
        let body = wire.body();
        if body.schema != PRODUCTION_RECEIPT_SCHEMA
            || body.version != 1
            || body.operation != "ingest"
            || !body.acknowledged
            || body.durability != "committed"
        {
            return Err(invalid_data(
                "direct-ingest receipt fixed fields are invalid",
            ));
        }
        verify_production_crc(&body.schema, &body, &expected_crc32)?;
        let receipt = Self::create(
            DirectIngestResultKindV1::parse(&body.result)?,
            body.transaction_id,
            body.semantic_time_unix_ns,
            parse_hash_hex(&body.intent_hash, "receipt intent_hash")?,
            parse_hash_hex(&body.base_binding_hash, "receipt base_binding_hash")?,
            body.committed_generation,
            parse_hash_hex(&body.commit_hash, "receipt commit_hash")?,
            parse_hash_hex(&body.logical_digest, "receipt logical_digest")?,
            parse_hash_hex(&body.atom_id, "receipt atom_id")?,
            body.node_num,
            body.history_event_id
                .as_deref()
                .map(|value| parse_hash_hex(value, "receipt history_event_id"))
                .transpose()?,
        )?;
        if receipt.canonical_bytes != bytes {
            return Err(invalid_data("direct-ingest receipt is not canonical"));
        }
        Ok(receipt)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectIngestFailureCodeV1 {
    ExplicitTransactionEnvelopeRequired,
    BaseNotAdmitted,
    BaseBindingMismatch,
    ParentStateChanged,
    InvalidTransactionId,
    InvalidSemanticTime,
    InvalidIntent,
    InvalidBatchItem,
    BoundsExceeded,
    ConflictingTransactionReuse,
    TombstonedIdentity,
    CanonicalRepresentationConflict,
    InvalidRequest,
    EvidenceSourceNotLive,
    GraphCompactionRequired,
    NestedTransactionForbidden,
    CompositeOperationNotAdmitted,
    TransportWriteNotRatified,
    UnsupportedPlatform,
    MigrationRequired,
    RecoveryRequired,
    UnsupportedOrCorrupt,
}

impl DirectIngestFailureCodeV1 {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitTransactionEnvelopeRequired => "explicit_transaction_envelope_required",
            Self::BaseNotAdmitted => "base_not_admitted",
            Self::BaseBindingMismatch => "base_binding_mismatch",
            Self::ParentStateChanged => "parent_state_changed",
            Self::InvalidTransactionId => "invalid_transaction_id",
            Self::InvalidSemanticTime => "invalid_semantic_time",
            Self::InvalidIntent => "invalid_intent",
            Self::InvalidBatchItem => "invalid_batch_item",
            Self::BoundsExceeded => "bounds_exceeded",
            Self::ConflictingTransactionReuse => "conflicting_transaction_reuse",
            Self::TombstonedIdentity => "tombstoned_identity",
            Self::CanonicalRepresentationConflict => "canonical_representation_conflict",
            Self::InvalidRequest => "invalid_request",
            Self::EvidenceSourceNotLive => "evidence_source_not_live",
            Self::GraphCompactionRequired => "graph_compaction_required",
            Self::NestedTransactionForbidden => "nested_transaction_forbidden",
            Self::CompositeOperationNotAdmitted => "composite_operation_not_admitted",
            Self::TransportWriteNotRatified => "transport_write_not_ratified",
            Self::UnsupportedPlatform => "unsupported_platform",
            Self::MigrationRequired => "migration_required",
            Self::RecoveryRequired => "recovery_required",
            Self::UnsupportedOrCorrupt => "unsupported_or_corrupt",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectIngestCommitDispositionV1 {
    NotStarted,
    NotCommitted,
    CommittedInstallPending,
    IndeterminateFailClosed,
}

impl DirectIngestCommitDispositionV1 {
    const fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::NotCommitted => "not_committed",
            Self::CommittedInstallPending => "committed_install_pending",
            Self::IndeterminateFailClosed => "indeterminate_fail_closed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectIngestRetryV1 {
    Never,
    SameTransaction,
    AfterRecoverySameTransaction,
}

impl DirectIngestRetryV1 {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::SameTransaction => "same_transaction",
            Self::AfterRecoverySameTransaction => "after_recovery_same_transaction",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectIngestFailureV1 {
    pub code: DirectIngestFailureCodeV1,
    pub message: String,
    pub transaction_id: Option<String>,
    pub intent_hash: Option<[u8; 32]>,
    pub commit_disposition: DirectIngestCommitDispositionV1,
    pub retry: DirectIngestRetryV1,
    canonical_bytes: Vec<u8>,
}

impl DirectIngestFailureV1 {
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }

    pub(crate) fn not_started(
        code: DirectIngestFailureCodeV1,
        message: impl Into<String>,
        transaction_id: Option<String>,
        intent_hash: Option<[u8; 32]>,
    ) -> Self {
        let retry = if matches!(
            code,
            DirectIngestFailureCodeV1::BaseNotAdmitted
                | DirectIngestFailureCodeV1::RecoveryRequired
        ) {
            DirectIngestRetryV1::SameTransaction
        } else {
            DirectIngestRetryV1::Never
        };
        Self::new(
            code,
            message,
            transaction_id,
            intent_hash,
            DirectIngestCommitDispositionV1::NotStarted,
            retry,
        )
        .unwrap_or_else(|_| Self {
            code: DirectIngestFailureCodeV1::UnsupportedOrCorrupt,
            message: "failure record encoding failed".to_owned(),
            transaction_id: None,
            intent_hash: None,
            commit_disposition: DirectIngestCommitDispositionV1::NotStarted,
            retry: DirectIngestRetryV1::Never,
            canonical_bytes: Vec::new(),
        })
    }

    pub(crate) fn recovery_required(
        message: impl Into<String>,
        transaction_id: String,
        intent_hash: [u8; 32],
        committed: bool,
    ) -> Self {
        Self::new(
            DirectIngestFailureCodeV1::RecoveryRequired,
            message,
            Some(transaction_id),
            Some(intent_hash),
            if committed {
                DirectIngestCommitDispositionV1::CommittedInstallPending
            } else {
                DirectIngestCommitDispositionV1::NotCommitted
            },
            DirectIngestRetryV1::SameTransaction,
        )
        .unwrap_or_else(|_| {
            Self::not_started(
                DirectIngestFailureCodeV1::UnsupportedOrCorrupt,
                "failure record encoding failed",
                None,
                None,
            )
        })
    }

    fn new(
        code: DirectIngestFailureCodeV1,
        message: impl Into<String>,
        transaction_id: Option<String>,
        intent_hash: Option<[u8; 32]>,
        commit_disposition: DirectIngestCommitDispositionV1,
        retry: DirectIngestRetryV1,
    ) -> io::Result<Self> {
        let message = message.into();
        if message.is_empty()
            || message.len() > 512
            || !message
                .bytes()
                .all(|byte| (0x20..=0x7e).contains(&byte) && byte != b'"' && byte != b'\\')
        {
            return Err(invalid_data(
                "direct-ingest failure message is not canonical ASCII",
            ));
        }
        if let Some(id) = &transaction_id {
            validate_production_uuid(id)?;
        }
        validate_failure_combination(
            code,
            commit_disposition,
            retry,
            transaction_id.is_some(),
            intent_hash.is_some(),
        )?;
        let body = DirectIngestFailureBodyWire {
            schema: PRODUCTION_FAILURE_SCHEMA.to_owned(),
            version: 1,
            operation: "ingest".to_owned(),
            code: code.as_str().to_owned(),
            message: message.clone(),
            transaction_id: transaction_id.clone(),
            intent_hash: intent_hash.as_ref().map(|value| hex_lower(value)),
            commit_disposition: commit_disposition.as_str().to_owned(),
            acknowledged: false,
            retry: retry.as_str().to_owned(),
        };
        let body_bytes = serde_json::to_vec(&body).map_err(io::Error::other)?;
        let crc32 = production_crc(&body.schema, &body_bytes);
        let wire = DirectIngestFailureWire::from_body(body, format!("{crc32:08x}"));
        let canonical_bytes = serde_json::to_vec(&wire).map_err(io::Error::other)?;
        Ok(Self {
            code,
            message,
            transaction_id,
            intent_hash,
            commit_disposition,
            retry,
            canonical_bytes,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProductionCommittedHead {
    pub(crate) generation: u64,
    pub(crate) commit_hash: [u8; 32],
    pub(crate) logical_digest: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProductionBaseBindingV1 {
    bytes: Vec<u8>,
    hash: [u8; 32],
    pub(crate) parent: ProductionCommittedHead,
}

impl ProductionBaseBindingV1 {
    pub(crate) fn from_identity(
        identity: &PhysicalRootIdentity,
        parent: ProductionCommittedHead,
    ) -> io::Result<Self> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(PRODUCTION_BASE_BINDING_ID.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.push(identity.platform_code);
        append_u64_frame(&mut bytes, &identity.canonical_root_key)?;
        append_u64_frame(&mut bytes, &identity.stable_root_identity)?;
        append_u64_frame(&mut bytes, PRODUCTION_FORMAT_SCHEMA.as_bytes())?;
        append_u64_frame(&mut bytes, PRODUCTION_REGISTRY_ID.as_bytes())?;
        bytes.extend_from_slice(&parent.generation.to_le_bytes());
        bytes.extend_from_slice(&parent.commit_hash);
        bytes.extend_from_slice(&parent.logical_digest);
        let decoded = Self::decode(&bytes)?;
        if decoded.parent != parent {
            return Err(invalid_data("base binding parent changed during encoding"));
        }
        Ok(decoded)
    }

    pub(crate) fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() as u64 > PRODUCTION_MAX_BASE_BINDING_BYTES {
            return Err(invalid_data("base binding exceeds its byte limit"));
        }
        let mut cursor = ProductionBinaryCursor::new(bytes);
        cursor.expect_domain(PRODUCTION_BASE_BINDING_ID)?;
        if cursor.read_u16()? != 1 {
            return Err(invalid_data("base binding has an unsupported version"));
        }
        let platform_code = cursor.read_u8()?;
        let canonical_root_key = cursor.read_u64_frame(PRODUCTION_MAX_BASE_BINDING_BYTES)?;
        let stable_root_identity = cursor.read_u64_frame(24)?;
        let format_id = cursor.read_u64_frame(128)?;
        let registry_id = cursor.read_u64_frame(128)?;
        if format_id != PRODUCTION_FORMAT_SCHEMA.as_bytes()
            || registry_id != PRODUCTION_REGISTRY_ID.as_bytes()
        {
            return Err(invalid_data(
                "base binding names an unsupported format or registry",
            ));
        }
        validate_platform_root_payload(platform_code, &canonical_root_key, &stable_root_identity)?;
        let generation = cursor.read_u64()?;
        let commit_hash = cursor.read_hash()?;
        let logical_digest = cursor.read_hash()?;
        cursor.finish()?;
        if generation > PRODUCTION_MAX_GENERATIONS
            || (generation == 0) != (commit_hash == [0; 32])
            || logical_digest == [0; 32]
        {
            return Err(invalid_data("base binding parent fields are invalid"));
        }
        Ok(Self {
            bytes: bytes.to_vec(),
            hash: *blake3::hash(bytes).as_bytes(),
            parent: ProductionCommittedHead {
                generation,
                commit_hash,
                logical_digest,
            },
        })
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) const fn hash(&self) -> [u8; 32] {
        self.hash
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProductionDirectIntentV1 {
    bytes: Vec<u8>,
    hash: [u8; 32],
    pub(crate) base_binding_hash: [u8; 32],
    pub(crate) atom_id: [u8; 32],
    pub(crate) atom_type: u8,
    pub(crate) body_len: u64,
    pub(crate) body_crc32c: u32,
    pub(crate) body_hash: [u8; 32],
    pub(crate) claim_count: u64,
    pub(crate) evidence_ref_count: u64,
}

impl ProductionDirectIntentV1 {
    pub(crate) fn create(
        base_binding_hash: [u8; 32],
        atom_id: [u8; 32],
        atom_type: u8,
        body: &[u8],
        claim_projection: &[u8],
        evidence_projection: &[u8],
    ) -> io::Result<Self> {
        if base_binding_hash == [0; 32]
            || atom_id == [0; 32]
            || !(1..=13).contains(&atom_type)
            || !(48..=PRODUCTION_MAX_BODY_BYTES).contains(&(body.len() as u64))
            || !claim_projection.len().is_multiple_of(25)
            || !evidence_projection.len().is_multiple_of(54)
            || claim_projection.len() > 25_000_000
            || evidence_projection.len() > 54_000_000
            || claim_projection
                .len()
                .checked_add(evidence_projection.len())
                .is_none_or(|length| length > PRODUCTION_MAX_BODY_BYTES as usize)
        {
            return Err(invalid_data(
                "direct-ingest intent input is outside the v1 limits",
            ));
        }
        validate_claim_projection(claim_projection)?;
        validate_evidence_projection(evidence_projection)?;
        let claim_count = (claim_projection.len() / 25) as u64;
        let evidence_ref_count = (evidence_projection.len() / 54) as u64;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(PRODUCTION_INTENT_ID.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&base_binding_hash);
        bytes.extend_from_slice(&atom_id);
        bytes.push(atom_type);
        bytes.extend_from_slice(&(body.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&crc32c(body).to_le_bytes());
        bytes.extend_from_slice(blake3::hash(body).as_bytes());
        bytes.extend_from_slice(&claim_count.to_le_bytes());
        bytes.extend_from_slice(&(claim_projection.len() as u64).to_le_bytes());
        bytes.extend_from_slice(claim_projection);
        bytes.extend_from_slice(&evidence_ref_count.to_le_bytes());
        bytes.extend_from_slice(&(evidence_projection.len() as u64).to_le_bytes());
        bytes.extend_from_slice(evidence_projection);
        append_u64_frame(&mut bytes, PRODUCTION_LIMITS_ID.as_bytes())?;
        if bytes.len() as u64 > PRODUCTION_MAX_INTENT_BYTES {
            return Err(invalid_data("direct-ingest intent exceeds its byte limit"));
        }
        Self::decode(&bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() as u64 > PRODUCTION_MAX_INTENT_BYTES {
            return Err(invalid_data("direct-ingest intent exceeds its byte limit"));
        }
        let mut cursor = ProductionBinaryCursor::new(bytes);
        cursor.expect_domain(PRODUCTION_INTENT_ID)?;
        if cursor.read_u16()? != 1 || cursor.read_u16()? != 1 {
            return Err(invalid_data(
                "direct-ingest intent version or operation is unsupported",
            ));
        }
        let base_binding_hash = cursor.read_hash()?;
        let atom_id = cursor.read_hash()?;
        let atom_type = cursor.read_u8()?;
        let body_len = cursor.read_u64()?;
        let body_crc32c = cursor.read_u32()?;
        let body_hash = cursor.read_hash()?;
        let claim_count = cursor.read_u64()?;
        let claim_projection_len = cursor.read_u64()?;
        let claim_projection = cursor.read_exact_vec(claim_projection_len, 25_000_000)?;
        let evidence_ref_count = cursor.read_u64()?;
        let evidence_projection_len = cursor.read_u64()?;
        let evidence_projection = cursor.read_exact_vec(evidence_projection_len, 54_000_000)?;
        let limits = cursor.read_u64_frame(128)?;
        cursor.finish()?;
        if base_binding_hash == [0; 32]
            || atom_id == [0; 32]
            || !(1..=13).contains(&atom_type)
            || !(48..=PRODUCTION_MAX_BODY_BYTES).contains(&body_len)
            || body_hash == [0; 32]
            || claim_count > 1_000_000
            || evidence_ref_count > 1_000_000
            || claim_projection_len != claim_count.saturating_mul(25)
            || evidence_projection_len != evidence_ref_count.saturating_mul(54)
            || claim_projection_len
                .checked_add(evidence_projection_len)
                .is_none_or(|length| length > PRODUCTION_MAX_BODY_BYTES)
            || limits != PRODUCTION_LIMITS_ID.as_bytes()
        {
            return Err(invalid_data("direct-ingest intent fields are invalid"));
        }
        validate_claim_projection(&claim_projection)?;
        validate_evidence_projection(&evidence_projection)?;
        Ok(Self {
            bytes: bytes.to_vec(),
            hash: *blake3::hash(bytes).as_bytes(),
            base_binding_hash,
            atom_id,
            atom_type,
            body_len,
            body_crc32c,
            body_hash,
            claim_count,
            evidence_ref_count,
        })
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) const fn hash(&self) -> [u8; 32] {
        self.hash
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProductionDirectEnvelopeV1 {
    bytes: Vec<u8>,
    hash: [u8; 32],
    pub(crate) transaction_id: String,
    pub(crate) transaction_uuid: [u8; 16],
    pub(crate) semantic_time_unix_ns: u64,
    pub(crate) base_binding_hash: [u8; 32],
    pub(crate) intent_hash: [u8; 32],
}

impl ProductionDirectEnvelopeV1 {
    pub(crate) fn create(
        transaction_id: &str,
        semantic_time_unix_ns: u64,
        base_binding_hash: [u8; 32],
        intent_hash: [u8; 32],
    ) -> io::Result<Self> {
        let transaction_uuid = validate_production_uuid(transaction_id)?;
        if semantic_time_unix_ns == 0 || base_binding_hash == [0; 32] || intent_hash == [0; 32] {
            return Err(invalid_data(
                "direct-ingest envelope contains a zero identity",
            ));
        }
        let mut bytes = Vec::with_capacity(124);
        bytes.extend_from_slice(PRODUCTION_ENVELOPE_ID.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&transaction_uuid);
        bytes.extend_from_slice(&semantic_time_unix_ns.to_le_bytes());
        bytes.extend_from_slice(&base_binding_hash);
        bytes.extend_from_slice(&intent_hash);
        Self::decode(&bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> io::Result<Self> {
        let mut cursor = ProductionBinaryCursor::new(bytes);
        cursor.expect_domain(PRODUCTION_ENVELOPE_ID)?;
        if cursor.read_u16()? != 1 {
            return Err(invalid_data(
                "direct-ingest envelope version is unsupported",
            ));
        }
        let transaction_uuid = cursor.read_uuid()?;
        validate_uuid_bytes(transaction_uuid)?;
        let semantic_time_unix_ns = cursor.read_u64()?;
        let base_binding_hash = cursor.read_hash()?;
        let intent_hash = cursor.read_hash()?;
        cursor.finish()?;
        if semantic_time_unix_ns == 0 || base_binding_hash == [0; 32] || intent_hash == [0; 32] {
            return Err(invalid_data(
                "direct-ingest envelope contains a zero identity",
            ));
        }
        Ok(Self {
            bytes: bytes.to_vec(),
            hash: *blake3::hash(bytes).as_bytes(),
            transaction_id: uuid_to_string(transaction_uuid),
            transaction_uuid,
            semantic_time_unix_ns,
            base_binding_hash,
            intent_hash,
        })
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) const fn hash(&self) -> [u8; 32] {
        self.hash
    }
}

/// One canonical edge-free item accepted by the direct-library batch carrier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchIngestItemV1 {
    pub atom_id: AtomId,
    pub body: Vec<u8>,
    pub atom_type: AtomType,
    pub claim_projection: Vec<u8>,
    pub evidence_projection: Vec<u8>,
}

impl BatchIngestItemV1 {
    pub fn new(
        atom_id: AtomId,
        body: impl Into<Vec<u8>>,
        atom_type: AtomType,
        claim_projection: impl Into<Vec<u8>>,
        evidence_projection: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            atom_id,
            body: body.into(),
            atom_type,
            claim_projection: claim_projection.into(),
            evidence_projection: evidence_projection.into(),
        }
    }

    pub fn from_body(
        body: impl Into<Vec<u8>>,
        atom_type: AtomType,
        claim_projection: impl Into<Vec<u8>>,
        evidence_projection: impl Into<Vec<u8>>,
    ) -> io::Result<Self> {
        let body = body.into();
        let atom_id = compute_atom_id_from_payload(&body)
            .map_err(|error| invalid_data(&format!("batch item atom body is invalid: {error}")))?;
        Ok(Self::new(
            atom_id,
            body,
            atom_type,
            claim_projection,
            evidence_projection,
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchIngestItemResultV1 {
    Created,
    Reused,
    Refused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchIngestItemReasonV1 {
    Created,
    AlreadyCommitted,
    DuplicateInput,
    CanonicalConflict,
    TombstonedIdentity,
    InvalidItem,
    EvidenceSourceNotLive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchIngestItemOutcomeV1 {
    pub ordinal: u32,
    pub atom_id: AtomId,
    pub result: BatchIngestItemResultV1,
    pub reason: BatchIngestItemReasonV1,
    pub node_num: Option<u64>,
    pub committed_generation: Option<u64>,
    pub first_input_ordinal: Option<u32>,
    pub decision_hash: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchIngestResultKindV1 {
    Committed,
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchIngestReceiptV1 {
    pub result: BatchIngestResultKindV1,
    pub transaction_id: String,
    pub semantic_time_unix_ns: u64,
    pub intent_hash: [u8; 32],
    pub base_binding_hash: [u8; 32],
    pub committed_generation: u64,
    pub commit_hash: [u8; 32],
    pub logical_digest: [u8; 32],
    pub outcomes: Vec<BatchIngestItemOutcomeV1>,
    pub history_event_id: Option<[u8; 32]>,
    canonical_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchIngestFailureV1 {
    pub code: DirectIngestFailureCodeV1,
    pub message: String,
    pub transaction_id: Option<String>,
    pub intent_hash: Option<[u8; 32]>,
    pub item_ordinal: Option<u32>,
    pub commit_disposition: DirectIngestCommitDispositionV1,
    pub retry: DirectIngestRetryV1,
    canonical_bytes: Vec<u8>,
}

impl BatchIngestFailureV1 {
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }

    fn new(
        code: DirectIngestFailureCodeV1,
        message: impl Into<String>,
        transaction_id: Option<String>,
        intent_hash: Option<[u8; 32]>,
        item_ordinal: Option<u32>,
        committed: bool,
    ) -> Self {
        let message = message.into();
        let disposition = if committed {
            DirectIngestCommitDispositionV1::CommittedInstallPending
        } else if intent_hash.is_some() {
            DirectIngestCommitDispositionV1::NotCommitted
        } else {
            DirectIngestCommitDispositionV1::NotStarted
        };
        let retry = if matches!(
            code,
            DirectIngestFailureCodeV1::RecoveryRequired
                | DirectIngestFailureCodeV1::BaseNotAdmitted
        ) {
            DirectIngestRetryV1::SameTransaction
        } else {
            DirectIngestRetryV1::Never
        };
        let body = BatchIngestFailureBodyWire {
            schema: "memoryx.batch-ingest-failure.v1".to_owned(),
            version: 1,
            operation: "batch_ingest".to_owned(),
            code: code.as_str().to_owned(),
            message: canonical_failure_message(&message),
            transaction_id: transaction_id.clone(),
            intent_hash: intent_hash.as_ref().map(|value| hex_lower(value)),
            item_ordinal,
            commit_disposition: disposition.as_str().to_owned(),
            acknowledged: false,
            retry: retry.as_str().to_owned(),
        };
        let canonical_bytes = BatchIngestFailureWire::from_body(body)
            .and_then(|wire| wire.canonical_bytes())
            .unwrap_or_default();
        debug_assert!(validate_batch_failure_wire(&canonical_bytes).is_ok());
        Self {
            code,
            message,
            transaction_id,
            intent_hash,
            item_ordinal,
            commit_disposition: disposition,
            retry,
            canonical_bytes,
        }
    }
}

fn canonical_failure_message(message: &str) -> String {
    let mut output = String::new();
    for byte in message.bytes().take(512) {
        if (0x20..=0x7e).contains(&byte) && byte != b'"' && byte != b'\\' {
            output.push(byte as char);
        }
    }
    if output.is_empty() {
        "batch operation failed closed".to_owned()
    } else {
        output
    }
}

fn classify_batch_intent_error(error: &io::Error) -> DirectIngestFailureCodeV1 {
    let message = error.to_string();
    if [
        "bound", "count", "limit", "overflow", "exceed", "length", "resource",
    ]
    .iter()
    .any(|needle| message.contains(needle))
    {
        DirectIngestFailureCodeV1::BoundsExceeded
    } else {
        DirectIngestFailureCodeV1::InvalidBatchItem
    }
}

impl BatchIngestReceiptV1 {
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        result: BatchIngestResultKindV1,
        transaction_id: String,
        semantic_time_unix_ns: u64,
        intent_hash: [u8; 32],
        base_binding_hash: [u8; 32],
        committed_generation: u64,
        commit_hash: [u8; 32],
        logical_digest: [u8; 32],
        outcomes: Vec<BatchIngestItemOutcomeV1>,
        history_event_id: Option<[u8; 32]>,
    ) -> io::Result<Self> {
        validate_production_uuid(&transaction_id)?;
        if semantic_time_unix_ns == 0
            || outcomes.is_empty()
            || outcomes.len() > PRODUCTION_BATCH_MAX_ITEMS
            || intent_hash == [0; 32]
            || base_binding_hash == [0; 32]
            || logical_digest == [0; 32]
        {
            return Err(invalid_data("batch receipt identity or count is invalid"));
        }
        let created = outcomes
            .iter()
            .filter(|outcome| outcome.result == BatchIngestItemResultV1::Created)
            .count();
        let reused = outcomes
            .iter()
            .filter(|outcome| outcome.result == BatchIngestItemResultV1::Reused)
            .count();
        let refused = outcomes.len() - created - reused;
        match result {
            BatchIngestResultKindV1::Committed
                if created == 0
                    || committed_generation == 0
                    || commit_hash == [0; 32]
                    || history_event_id.is_none_or(|value| value == [0; 32]) =>
            {
                return Err(invalid_data("committed batch receipt is incomplete"));
            }
            BatchIngestResultKindV1::Unchanged if created != 0 || history_event_id.is_some() => {
                return Err(invalid_data("unchanged batch receipt contains a mutation"));
            }
            _ => {}
        }
        let body = BatchIngestReceiptBodyWire {
            schema: PRODUCTION_BATCH_RECEIPT_SCHEMA.to_owned(),
            version: 1,
            operation: "batch_ingest".to_owned(),
            result: match result {
                BatchIngestResultKindV1::Committed => "committed",
                BatchIngestResultKindV1::Unchanged => "unchanged",
            }
            .to_owned(),
            transaction_id: transaction_id.clone(),
            semantic_time_unix_ns,
            intent_hash: hex_lower(&intent_hash),
            base_binding_hash: hex_lower(&base_binding_hash),
            item_count: outcomes.len() as u32,
            created_count: created as u32,
            reused_count: reused as u32,
            refused_count: refused as u32,
            committed_generation,
            commit_hash: hex_lower(&commit_hash),
            logical_digest: hex_lower(&logical_digest),
            decision_hashes: outcomes
                .iter()
                .map(|outcome| hex_lower(&outcome.decision_hash))
                .collect(),
            history_event_id: history_event_id.as_ref().map(|value| hex_lower(value)),
            acknowledged: true,
            durability: match result {
                BatchIngestResultKindV1::Committed => "committed",
                BatchIngestResultKindV1::Unchanged => "unchanged",
            }
            .to_owned(),
        };
        let canonical_bytes = BatchIngestReceiptWire::from_body(body)?.canonical_bytes()?;
        validate_batch_receipt_wire(&canonical_bytes)?;
        Ok(Self {
            result,
            transaction_id,
            semantic_time_unix_ns,
            intent_hash,
            base_binding_hash,
            committed_generation,
            commit_hash,
            logical_digest,
            outcomes,
            history_event_id,
            canonical_bytes,
        })
    }
}

/// Explicit immutable request for the sealed direct-library P1 update carrier.
/// Every byte slice is successor-local; the predecessor's provenance is read
/// from the committed parent and can never be supplied or inherited by the
/// caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateAtomRequestV1 {
    pub transaction_id: String,
    pub semantic_time_unix_ns: u64,
    pub old_atom_id: AtomId,
    pub successor_atom_id: AtomId,
    pub successor_body: Vec<u8>,
    pub successor_atom_type: AtomType,
    pub claim_projection: Vec<u8>,
    pub api_evidence_projection: Vec<u8>,
    pub successor_source_attachment_projection: Vec<u8>,
    pub successor_provenance: Vec<u8>,
}

impl UpdateAtomRequestV1 {
    #[allow(clippy::too_many_arguments)]
    pub fn from_successor_body(
        transaction_id: impl Into<String>,
        semantic_time_unix_ns: u64,
        old_atom_id: AtomId,
        successor_body: impl Into<Vec<u8>>,
        successor_atom_type: AtomType,
        claim_projection: impl Into<Vec<u8>>,
        api_evidence_projection: impl Into<Vec<u8>>,
        successor_source_attachment_projection: impl Into<Vec<u8>>,
        successor_provenance: impl Into<Vec<u8>>,
    ) -> io::Result<Self> {
        let successor_body = successor_body.into();
        let successor_atom_id = compute_atom_id_from_payload(&successor_body).map_err(|error| {
            invalid_data(&format!("update successor body is noncanonical: {error}"))
        })?;
        Ok(Self {
            transaction_id: transaction_id.into(),
            semantic_time_unix_ns,
            old_atom_id,
            successor_atom_id,
            successor_body,
            successor_atom_type,
            claim_projection: claim_projection.into(),
            api_evidence_projection: api_evidence_projection.into(),
            successor_source_attachment_projection: successor_source_attachment_projection.into(),
            successor_provenance: successor_provenance.into(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAtomFailureCodeV1 {
    InvalidTransactionId,
    InvalidSemanticTime,
    InvalidSuccessor,
    OldAtomMissing,
    SameAtomIdUpdate,
    AlreadySuperseded,
    AmbiguousSupersessionState,
    RelationBackedAtomRequiresCompositeOperation,
    SuccessorCollision,
    ProvenanceProjectionConflict,
    ResourceLimitExceeded,
    GraphCompactionRequired,
    ConflictingTransactionReuse,
    RecoveryRequired,
    UnsupportedOrCorrupt,
}

impl UpdateAtomFailureCodeV1 {
    const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidTransactionId => "invalid_transaction_id",
            Self::InvalidSemanticTime => "invalid_semantic_time",
            Self::InvalidSuccessor => "noncanonical_successor",
            Self::OldAtomMissing => "old_atom_missing",
            Self::SameAtomIdUpdate => "same_atom_id_update",
            Self::AlreadySuperseded => "already_superseded",
            Self::AmbiguousSupersessionState => "ambiguous_supersession_state",
            Self::RelationBackedAtomRequiresCompositeOperation => {
                "relation_backed_atom_requires_composite_operation"
            }
            Self::SuccessorCollision => "successor_collision",
            Self::ProvenanceProjectionConflict => "provenance_projection_conflict",
            Self::ResourceLimitExceeded => "resource_limit_exceeded",
            Self::GraphCompactionRequired => "graph_compaction_required",
            Self::ConflictingTransactionReuse => "conflicting_transaction_reuse",
            Self::RecoveryRequired => "recovery_required",
            Self::UnsupportedOrCorrupt => "unsupported_or_corrupt",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateAtomFailureV1 {
    pub code: UpdateAtomFailureCodeV1,
    pub message: String,
    pub transaction_id: Option<String>,
    pub intent_hash: Option<[u8; 32]>,
    pub commit_disposition: DirectIngestCommitDispositionV1,
    pub retry: DirectIngestRetryV1,
    canonical_bytes: Vec<u8>,
}

impl UpdateAtomFailureV1 {
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }

    fn new(
        code: UpdateAtomFailureCodeV1,
        message: impl Into<String>,
        transaction_id: Option<String>,
        intent_hash: Option<[u8; 32]>,
        committed: bool,
    ) -> Self {
        let message = canonical_failure_message(&message.into());
        let commit_disposition = if committed {
            DirectIngestCommitDispositionV1::CommittedInstallPending
        } else if intent_hash.is_some() {
            DirectIngestCommitDispositionV1::NotCommitted
        } else {
            DirectIngestCommitDispositionV1::NotStarted
        };
        let retry = if code == UpdateAtomFailureCodeV1::RecoveryRequired {
            DirectIngestRetryV1::SameTransaction
        } else {
            DirectIngestRetryV1::Never
        };
        let body = UpdateAtomFailureBodyWire {
            schema: PRODUCTION_UPDATE_FAILURE_SCHEMA.to_owned(),
            version: 1,
            operation: "update_atom".to_owned(),
            code: code.as_str().to_owned(),
            message: message.clone(),
            transaction_id: transaction_id.clone(),
            intent_hash: intent_hash.as_ref().map(|value| hex_lower(value)),
            commit_disposition: commit_disposition.as_str().to_owned(),
            acknowledged: false,
            retry: retry.as_str().to_owned(),
        };
        let canonical_bytes = UpdateAtomFailureWire::from_body(body)
            .and_then(|wire| wire.canonical_bytes())
            .unwrap_or_default();
        Self {
            code,
            message,
            transaction_id,
            intent_hash,
            commit_disposition,
            retry,
            canonical_bytes,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateAtomReceiptV1 {
    pub transaction_id: String,
    pub semantic_time_unix_ns: u64,
    pub parent_generation: u64,
    pub parent_commit_hash: [u8; 32],
    pub committed_generation: u64,
    pub commit_hash: [u8; 32],
    pub logical_digest: [u8; 32],
    pub base_binding_hash: [u8; 32],
    pub envelope_hash: [u8; 32],
    pub intent_hash: [u8; 32],
    pub old_atom_id: AtomId,
    pub successor_atom_id: AtomId,
    pub successor_node: u64,
    pub supersedes_relation_id: [u8; 32],
    pub history_event_id: [u8; 32],
    pub history_semantic_hash: [u8; 32],
    pub old_provenance_hash: [u8; 32],
    pub successor_provenance_hash: [u8; 32],
    pub component_root_hash: [u8; 32],
    canonical_bytes: Vec<u8>,
}

impl UpdateAtomReceiptV1 {
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        transaction_id: String,
        semantic_time_unix_ns: u64,
        parent_generation: u64,
        parent_commit_hash: [u8; 32],
        committed_generation: u64,
        commit_hash: [u8; 32],
        logical_digest: [u8; 32],
        base_binding_hash: [u8; 32],
        envelope_hash: [u8; 32],
        intent_hash: [u8; 32],
        old_atom_id: AtomId,
        successor_atom_id: AtomId,
        successor_node: u64,
        supersedes_relation_id: [u8; 32],
        history_event_id: [u8; 32],
        history_semantic_hash: [u8; 32],
        old_provenance_hash: [u8; 32],
        successor_provenance_hash: [u8; 32],
        component_root_hash: [u8; 32],
    ) -> io::Result<Self> {
        let transaction_uuid = validate_production_uuid(&transaction_id)?;
        if semantic_time_unix_ns == 0
            || committed_generation != parent_generation.saturating_add(1)
            || committed_generation == 0
            || committed_generation > PRODUCTION_MAX_GENERATIONS
            || old_atom_id == successor_atom_id
            || successor_node == u64::MAX
            || [
                commit_hash,
                logical_digest,
                base_binding_hash,
                envelope_hash,
                intent_hash,
                supersedes_relation_id,
                history_event_id,
                history_semantic_hash,
                old_provenance_hash,
                successor_provenance_hash,
                component_root_hash,
            ]
            .contains(&[0; 32])
        {
            return Err(invalid_data("update receipt identity is incomplete"));
        }
        let mut canonical_bytes = Vec::new();
        canonical_bytes.extend_from_slice(PRODUCTION_UPDATE_RECEIPT_ID.as_bytes());
        canonical_bytes.push(0);
        canonical_bytes.extend_from_slice(&1u16.to_le_bytes());
        canonical_bytes.extend_from_slice(&transaction_uuid);
        canonical_bytes.extend_from_slice(&committed_generation.to_le_bytes());
        canonical_bytes.extend_from_slice(&successor_atom_id);
        canonical_bytes.extend_from_slice(&old_atom_id);
        canonical_bytes.extend_from_slice(&supersedes_relation_id);
        canonical_bytes.extend_from_slice(&history_event_id);
        canonical_bytes.extend_from_slice(&history_semantic_hash);
        canonical_bytes.extend_from_slice(&old_provenance_hash);
        canonical_bytes.extend_from_slice(&successor_provenance_hash);
        canonical_bytes.extend_from_slice(&intent_hash);
        Ok(Self {
            transaction_id,
            semantic_time_unix_ns,
            parent_generation,
            parent_commit_hash,
            committed_generation,
            commit_hash,
            logical_digest,
            base_binding_hash,
            envelope_hash,
            intent_hash,
            old_atom_id,
            successor_atom_id,
            successor_node,
            supersedes_relation_id,
            history_event_id,
            history_semantic_hash,
            old_provenance_hash,
            successor_provenance_hash,
            component_root_hash,
            canonical_bytes,
        })
    }
}

fn production_sha256(bytes: &[u8]) -> [u8; 32] {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let mut padded = bytes.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    let mut state = INITIAL;
    for block in padded.as_chunks::<64>().0 {
        let mut words = [0u32; 64];
        for (index, word) in words.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes(block[index * 4..index * 4 + 4].try_into().unwrap());
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut output = [0u8; 32];
    for (index, word) in state.iter().enumerate() {
        output[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    output
}

fn production_update_source_attachment_hash(
    bytes: &[u8],
    successor_atom_id: AtomId,
    old_atom_id: AtomId,
) -> io::Result<[u8; 32]> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        invalid_data("provenance_projection_conflict: source attachment is not ASCII")
    })?;
    if !text.is_ascii() || bytes.len() as u64 > PRODUCTION_UPDATE_MAX_PROJECTION_BYTES {
        return Err(invalid_data(
            "provenance_projection_conflict: source attachment length or encoding is invalid",
        ));
    }
    let declared = text
        .strip_prefix(PRODUCTION_UPDATE_SOURCE_ATTACHMENT_PREFIX)
        .ok_or_else(|| {
            invalid_data("provenance_projection_conflict: source attachment schema is not declared")
        })?;
    let (atom_hex, projection) = declared
        .split_once(PRODUCTION_UPDATE_SOURCE_ATTACHMENT_PAYLOAD)
        .ok_or_else(|| {
            invalid_data("provenance_projection_conflict: source attachment fields are malformed")
        })?;
    if atom_hex.len() != 64
        || !atom_hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || projection.is_empty()
        || projection.bytes().any(|byte| {
            !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b':' | b'/'))
        })
    {
        return Err(invalid_data(
            "provenance_projection_conflict: source attachment encoding is noncanonical",
        ));
    }
    let declared_atom = parse_hash_hex(atom_hex, "source attachment AtomId")?;
    if declared_atom == [0; 32]
        || declared_atom == old_atom_id
        || declared_atom != successor_atom_id
    {
        return Err(invalid_data(
            "provenance_projection_conflict: source attachment does not bind the successor AtomId",
        ));
    }
    Ok(production_sha256(bytes))
}

#[derive(Debug, Clone)]
struct ProductionUpdateIntentV1 {
    bytes: Vec<u8>,
    hash: [u8; 32],
    old_atom_id: AtomId,
    successor_atom_id: AtomId,
    successor_body_hash: [u8; 32],
    claim_projection_hash: [u8; 32],
    api_evidence_projection_hash: [u8; 32],
    successor_source_attachment_hash: [u8; 32],
    supersedes_relation_id: [u8; 32],
}

impl ProductionUpdateIntentV1 {
    fn create(request: &UpdateAtomRequestV1) -> io::Result<Self> {
        if request.old_atom_id == request.successor_atom_id {
            return Err(invalid_data("same_atom_id_update"));
        }
        if request.successor_body.is_empty()
            || request.successor_body.len() as u64 > PRODUCTION_MAX_BODY_BYTES
            || request
                .claim_projection
                .len()
                .checked_add(request.api_evidence_projection.len())
                .and_then(|value| {
                    value.checked_add(request.successor_source_attachment_projection.len())
                })
                .is_none_or(|value| value as u64 > PRODUCTION_UPDATE_MAX_PROJECTION_BYTES)
        {
            return Err(invalid_data("resource_limit_exceeded"));
        }
        if compute_atom_id_from_payload(&request.successor_body)
            .map_err(|error| invalid_data(&format!("noncanonical_successor: {error}")))?
            != request.successor_atom_id
            || AtomBodyHeader::from_bytes(&request.successor_body)
                .map_err(|error| invalid_data(&format!("noncanonical_successor: {error}")))?
                .atom_type()
                != Some(request.successor_atom_type)
        {
            return Err(invalid_data("noncanonical_successor"));
        }
        validate_claim_projection(&request.claim_projection)?;
        validate_evidence_projection(&request.api_evidence_projection)?;
        let successor_body_hash = production_sha256(&request.successor_body);
        let claim_projection_hash = production_sha256(&request.claim_projection);
        let api_evidence_projection_hash = production_sha256(&request.api_evidence_projection);
        let successor_source_attachment_hash = production_update_source_attachment_hash(
            &request.successor_source_attachment_projection,
            request.successor_atom_id,
            request.old_atom_id,
        )?;
        let supersedes_relation_id =
            production_update_relation_id(request.successor_atom_id, request.old_atom_id);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(PRODUCTION_UPDATE_INTENT_ID.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&3u16.to_le_bytes());
        bytes.extend_from_slice(&request.old_atom_id);
        bytes.extend_from_slice(&request.successor_atom_id);
        bytes.extend_from_slice(&successor_body_hash);
        bytes.extend_from_slice(&claim_projection_hash);
        bytes.extend_from_slice(&api_evidence_projection_hash);
        bytes.extend_from_slice(&successor_source_attachment_hash);
        bytes.extend_from_slice(&supersedes_relation_id);
        append_u64_frame(&mut bytes, PRODUCTION_UPDATE_LIMITS_ID.as_bytes())?;
        Ok(Self {
            hash: production_hash_bytes(&bytes),
            bytes,
            old_atom_id: request.old_atom_id,
            successor_atom_id: request.successor_atom_id,
            successor_body_hash,
            claim_projection_hash,
            api_evidence_projection_hash,
            successor_source_attachment_hash,
            supersedes_relation_id,
        })
    }
}

#[derive(Debug, Clone)]
struct ProductionUpdateEnvelopeV1 {
    bytes: Vec<u8>,
    hash: [u8; 32],
    transaction_id: String,
    transaction_uuid: [u8; 16],
    semantic_time_unix_ns: u64,
    base_binding_hash: [u8; 32],
    intent_hash: [u8; 32],
}

impl ProductionUpdateEnvelopeV1 {
    fn create(
        transaction_id: &str,
        semantic_time_unix_ns: u64,
        base_binding_hash: [u8; 32],
        intent_hash: [u8; 32],
    ) -> io::Result<Self> {
        let transaction_uuid = validate_production_uuid(transaction_id)?;
        if semantic_time_unix_ns == 0 || base_binding_hash == [0; 32] || intent_hash == [0; 32] {
            return Err(invalid_data("update envelope contains a zero identity"));
        }
        let mut bytes = Vec::new();
        bytes.extend_from_slice(PRODUCTION_UPDATE_ENVELOPE_ID.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&transaction_uuid);
        bytes.extend_from_slice(&semantic_time_unix_ns.to_le_bytes());
        bytes.extend_from_slice(&base_binding_hash);
        bytes.extend_from_slice(&intent_hash);
        Ok(Self {
            hash: production_hash_bytes(&bytes),
            bytes,
            transaction_id: transaction_id.to_owned(),
            transaction_uuid,
            semantic_time_unix_ns,
            base_binding_hash,
            intent_hash,
        })
    }
}

fn production_update_relation_id(successor: AtomId, old: AtomId) -> [u8; 32] {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(PRODUCTION_UPDATE_RELATION_ID.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&EdgeType::SUPERSEDES.to_u32().to_le_bytes());
    bytes.extend_from_slice(&successor);
    bytes.extend_from_slice(&old);
    production_hash_bytes(&bytes)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProductionBatchItemCodecV1 {
    bytes: Vec<u8>,
    hash: [u8; 32],
    ordinal: u32,
    atom_id: AtomId,
    atom_type: AtomType,
    body_len: u64,
    body_crc32c: u32,
    body_hash: [u8; 32],
    body: Vec<u8>,
    claim_projection: Vec<u8>,
    evidence_projection: Vec<u8>,
}

impl ProductionBatchItemCodecV1 {
    fn create(ordinal: u32, item: &BatchIngestItemV1) -> io::Result<Self> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(PRODUCTION_BATCH_ITEM_ID.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&ordinal.to_le_bytes());
        bytes.extend_from_slice(&item.atom_id);
        bytes.push(item.atom_type.to_u32() as u8);
        bytes.extend_from_slice(&(item.body.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&crc32c(&item.body).to_le_bytes());
        bytes.extend_from_slice(blake3::hash(&item.body).as_bytes());
        bytes.extend_from_slice(&((item.claim_projection.len() / 25) as u64).to_le_bytes());
        bytes.extend_from_slice(&(item.claim_projection.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&item.claim_projection);
        bytes.extend_from_slice(&((item.evidence_projection.len() / 54) as u64).to_le_bytes());
        bytes.extend_from_slice(&(item.evidence_projection.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&item.evidence_projection);
        Self::decode(&bytes, item)
    }

    fn decode(bytes: &[u8], source: &BatchIngestItemV1) -> io::Result<Self> {
        let mut cursor = ProductionBinaryCursor::new(bytes);
        cursor.expect_domain(PRODUCTION_BATCH_ITEM_ID)?;
        if cursor.read_u16()? != 1 {
            return Err(invalid_data("batch item version is unsupported"));
        }
        let ordinal = cursor.read_u32()?;
        let atom_id = cursor.read_hash()?;
        let atom_type = AtomType::from_u32(u32::from(cursor.read_u8()?))
            .ok_or_else(|| invalid_data("batch item atom type is invalid"))?;
        let body_len = cursor.read_u64()?;
        let body_crc32c = cursor.read_u32()?;
        let body_hash = cursor.read_hash()?;
        let claim_count = cursor.read_u64()?;
        let claim_len = cursor.read_u64()?;
        let claim_projection = cursor.read_exact_vec(claim_len, 25_000_000)?;
        let evidence_count = cursor.read_u64()?;
        let evidence_len = cursor.read_u64()?;
        let evidence_projection = cursor.read_exact_vec(evidence_len, 33_554_432)?;
        cursor.finish()?;
        if !(48..=67_108_864).contains(&body_len)
            || body_len != source.body.len() as u64
            || body_crc32c != crc32c(&source.body)
            || body_hash != production_hash_bytes(&source.body)
            || atom_id != source.atom_id
            || atom_type != source.atom_type
            || claim_len != claim_count.saturating_mul(25)
            || evidence_len != evidence_count.saturating_mul(54)
            || claim_count > 1_000_000
            || evidence_count > 1_000_000
            || claim_projection != source.claim_projection
            || evidence_projection != source.evidence_projection
        {
            return Err(invalid_data("batch item fields are inconsistent"));
        }
        validate_claim_projection(&claim_projection)?;
        validate_evidence_projection(&evidence_projection)?;
        Ok(Self {
            bytes: bytes.to_vec(),
            hash: production_hash_bytes(bytes),
            ordinal,
            atom_id,
            atom_type,
            body_len,
            body_crc32c,
            body_hash,
            body: source.body.clone(),
            claim_projection,
            evidence_projection,
        })
    }
}

#[derive(Debug, Clone)]
struct ProductionBatchIntentV1 {
    bytes: Vec<u8>,
    hash: [u8; 32],
    base_binding_hash: [u8; 32],
    items: Vec<ProductionBatchItemCodecV1>,
}

impl ProductionBatchIntentV1 {
    fn create(base_binding_hash: [u8; 32], items: &[BatchIngestItemV1]) -> io::Result<Self> {
        if base_binding_hash == [0; 32]
            || items.is_empty()
            || items.len() > PRODUCTION_BATCH_MAX_ITEMS
        {
            return Err(invalid_data(
                "batch item count is outside the frozen limits",
            ));
        }
        let mut encoded = Vec::with_capacity(items.len());
        let mut body_total = 0u64;
        let mut projection_total = 0u64;
        for (ordinal, item) in items.iter().enumerate() {
            body_total = body_total
                .checked_add(item.body.len() as u64)
                .ok_or_else(|| invalid_data("batch body total overflow"))?;
            projection_total = projection_total
                .checked_add(item.claim_projection.len() as u64)
                .and_then(|value| value.checked_add(item.evidence_projection.len() as u64))
                .ok_or_else(|| invalid_data("batch projection total overflow"))?;
            encoded.push(ProductionBatchItemCodecV1::create(ordinal as u32, item)?);
        }
        if body_total > PRODUCTION_BATCH_MAX_TOTAL_BODY_BYTES
            || projection_total > PRODUCTION_BATCH_MAX_TOTAL_PROJECTION_BYTES
        {
            return Err(invalid_data("batch aggregate resource bound exceeded"));
        }
        let mut bytes = Vec::new();
        bytes.extend_from_slice(PRODUCTION_BATCH_INTENT_ID.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&base_binding_hash);
        bytes.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
        for item in &encoded {
            append_u64_frame(&mut bytes, &item.bytes)?;
        }
        append_u64_frame(&mut bytes, PRODUCTION_BATCH_LIMITS_ID.as_bytes())?;
        if bytes.len() as u64 > PRODUCTION_BATCH_MAX_TOTAL_BODY_BYTES + 1_048_576 {
            return Err(invalid_data("batch intent exceeds its frozen byte limit"));
        }
        Ok(Self {
            hash: production_hash_bytes(&bytes),
            bytes,
            base_binding_hash,
            items: encoded,
        })
    }

    fn decode(bytes: &[u8], source_items: &[BatchIngestItemV1]) -> io::Result<Self> {
        let mut cursor = ProductionBinaryCursor::new(bytes);
        cursor.expect_domain(PRODUCTION_BATCH_INTENT_ID)?;
        if cursor.read_u16()? != 1 || cursor.read_u16()? != 2 {
            return Err(invalid_data(
                "batch intent version or operation is unsupported",
            ));
        }
        let base_binding_hash = cursor.read_hash()?;
        let item_count = cursor.read_u32()? as usize;
        if item_count == 0
            || item_count > PRODUCTION_BATCH_MAX_ITEMS
            || item_count != source_items.len()
        {
            return Err(invalid_data("batch intent item count is invalid"));
        }
        let mut items = Vec::with_capacity(item_count);
        for (ordinal, source) in source_items.iter().enumerate() {
            let frame = cursor.read_u64_frame(PRODUCTION_BATCH_MAX_TOTAL_BODY_BYTES + 1_048_576)?;
            let item = ProductionBatchItemCodecV1::decode(&frame, source)?;
            if item.ordinal != ordinal as u32 {
                return Err(invalid_data("batch intent item ordinal is noncanonical"));
            }
            items.push(item);
        }
        if cursor.read_u64_frame(128)?.as_slice() != PRODUCTION_BATCH_LIMITS_ID.as_bytes() {
            return Err(invalid_data("batch intent limits identity is unsupported"));
        }
        cursor.finish()?;
        let rebuilt = Self::create(base_binding_hash, source_items)?;
        if rebuilt.bytes != bytes {
            return Err(invalid_data("batch intent encoding is noncanonical"));
        }
        Ok(Self {
            bytes: bytes.to_vec(),
            hash: production_hash_bytes(bytes),
            base_binding_hash,
            items,
        })
    }
}

#[derive(Debug, Clone)]
struct ProductionBatchEnvelopeV1 {
    bytes: Vec<u8>,
    hash: [u8; 32],
    transaction_id: String,
    transaction_uuid: [u8; 16],
    semantic_time_unix_ns: u64,
    base_binding_hash: [u8; 32],
    intent_hash: [u8; 32],
}

impl ProductionBatchEnvelopeV1 {
    fn create(
        transaction_id: &str,
        semantic_time_unix_ns: u64,
        base_binding_hash: [u8; 32],
        intent_hash: [u8; 32],
    ) -> io::Result<Self> {
        let transaction_uuid = validate_production_uuid(transaction_id)?;
        if semantic_time_unix_ns == 0 || base_binding_hash == [0; 32] || intent_hash == [0; 32] {
            return Err(invalid_data("batch envelope contains a zero identity"));
        }
        let mut bytes = Vec::new();
        bytes.extend_from_slice(PRODUCTION_BATCH_ENVELOPE_ID.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&transaction_uuid);
        bytes.extend_from_slice(&semantic_time_unix_ns.to_le_bytes());
        bytes.extend_from_slice(&base_binding_hash);
        bytes.extend_from_slice(&intent_hash);
        Ok(Self {
            hash: production_hash_bytes(&bytes),
            bytes,
            transaction_id: transaction_id.to_owned(),
            transaction_uuid,
            semantic_time_unix_ns,
            base_binding_hash,
            intent_hash,
        })
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let mut cursor = ProductionBinaryCursor::new(bytes);
        cursor.expect_domain(PRODUCTION_BATCH_ENVELOPE_ID)?;
        if cursor.read_u16()? != 1 {
            return Err(invalid_data("batch envelope version is unsupported"));
        }
        let transaction_uuid = cursor.read_uuid()?;
        let semantic_time_unix_ns = cursor.read_u64()?;
        let base_binding_hash = cursor.read_hash()?;
        let intent_hash = cursor.read_hash()?;
        cursor.finish()?;
        let transaction_id = uuid_to_string(transaction_uuid);
        let rebuilt = Self::create(
            &transaction_id,
            semantic_time_unix_ns,
            base_binding_hash,
            intent_hash,
        )?;
        if rebuilt.bytes != bytes {
            return Err(invalid_data("batch envelope encoding is noncanonical"));
        }
        Ok(Self {
            bytes: bytes.to_vec(),
            hash: production_hash_bytes(bytes),
            transaction_id,
            transaction_uuid,
            semantic_time_unix_ns,
            base_binding_hash,
            intent_hash,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProductionHistoryEventV1 {
    pub(crate) event_id: [u8; 32],
    pub(crate) event_semantic_hash: [u8; 32],
    pub(crate) object_bytes: Vec<u8>,
    pub(crate) line_bytes: Vec<u8>,
    pub(crate) leaf_bytes: Vec<u8>,
}

impl ProductionHistoryEventV1 {
    pub(crate) fn create(
        envelope: &ProductionDirectEnvelopeV1,
        generation: u64,
        atom_id: [u8; 32],
        atom_type: u32,
        claim_count: u64,
        evidence_ref_count: u64,
    ) -> io::Result<Self> {
        if generation == 0
            || generation > PRODUCTION_MAX_GENERATIONS
            || !(1..=13).contains(&atom_type)
        {
            return Err(invalid_data(
                "history event fields are outside the direct-ingest limits",
            ));
        }
        let mut event_id_preimage = Vec::new();
        event_id_preimage.extend_from_slice(PRODUCTION_HISTORY_EVENT_ID.as_bytes());
        event_id_preimage.push(0);
        event_id_preimage.extend_from_slice(&envelope.transaction_uuid);
        event_id_preimage.extend_from_slice(&0u32.to_le_bytes());
        let event_id = *blake3::hash(&event_id_preimage).as_bytes();

        let mut semantic = Vec::new();
        semantic.extend_from_slice(PRODUCTION_HISTORY_SEMANTIC_ID.as_bytes());
        semantic.push(0);
        semantic.extend_from_slice(&1u16.to_le_bytes());
        semantic.extend_from_slice(&envelope.transaction_uuid);
        semantic.extend_from_slice(&0u32.to_le_bytes());
        semantic.extend_from_slice(&generation.to_le_bytes());
        semantic.extend_from_slice(&envelope.semantic_time_unix_ns.to_le_bytes());
        semantic.extend_from_slice(&1u16.to_le_bytes());
        semantic.push(1);
        semantic.push(1);
        semantic.extend_from_slice(&1u64.to_le_bytes());
        semantic.extend_from_slice(&atom_id);
        semantic.extend_from_slice(&envelope.intent_hash);
        semantic.extend_from_slice(&atom_type.to_le_bytes());
        semantic.extend_from_slice(&claim_count.to_le_bytes());
        semantic.extend_from_slice(&evidence_ref_count.to_le_bytes());
        semantic.push(1);
        let event_semantic_hash = *blake3::hash(&semantic).as_bytes();

        let wire = ProductionHistoryEventWire {
            schema_version: PRODUCTION_HISTORY_SCHEMA.to_owned(),
            event_id: hex_lower(&event_id),
            transaction_id: envelope.transaction_id.clone(),
            event_ordinal: 0,
            generation,
            timestamp_unix_ns: envelope.semantic_time_unix_ns,
            operation: "ingest".to_owned(),
            event_kind: "mutation".to_owned(),
            outcome: "committed".to_owned(),
            atom_ids: vec![hex_lower(&atom_id)],
            details: ProductionHistoryDetailsWire {
                atom_type_u32: atom_type.to_string(),
                claim_count: claim_count.to_string(),
                evidence_ref_count: evidence_ref_count.to_string(),
                result_kind: "created".to_owned(),
            },
            intent_hash: hex_lower(&envelope.intent_hash),
            event_semantic_hash: hex_lower(&event_semantic_hash),
        };
        let object_bytes = serde_json::to_vec(&wire).map_err(io::Error::other)?;
        let mut line_bytes = object_bytes.clone();
        line_bytes.push(b'\n');

        let mut leaf_bytes = Vec::new();
        leaf_bytes.extend_from_slice(PRODUCTION_HISTORY_LEAF_ID.as_bytes());
        leaf_bytes.push(0);
        leaf_bytes.extend_from_slice(&1u16.to_le_bytes());
        leaf_bytes.extend_from_slice(&generation.to_le_bytes());
        leaf_bytes.extend_from_slice(&0u32.to_le_bytes());
        leaf_bytes.extend_from_slice(&envelope.transaction_uuid);
        leaf_bytes.extend_from_slice(&event_id);
        leaf_bytes.extend_from_slice(&envelope.semantic_time_unix_ns.to_le_bytes());
        leaf_bytes.extend_from_slice(&1u16.to_le_bytes());
        leaf_bytes.extend_from_slice(&event_semantic_hash);
        Ok(Self {
            event_id,
            event_semantic_hash,
            object_bytes,
            line_bytes,
            leaf_bytes,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectIngestReceiptBodyWire {
    schema: String,
    version: u64,
    operation: String,
    result: String,
    transaction_id: String,
    semantic_time_unix_ns: u64,
    intent_hash: String,
    base_binding_hash: String,
    committed_generation: u64,
    commit_hash: String,
    logical_digest: String,
    atom_id: String,
    node_num: u64,
    history_event_id: Option<String>,
    acknowledged: bool,
    durability: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectIngestReceiptWire {
    schema: String,
    version: u64,
    operation: String,
    result: String,
    transaction_id: String,
    semantic_time_unix_ns: u64,
    intent_hash: String,
    base_binding_hash: String,
    committed_generation: u64,
    commit_hash: String,
    logical_digest: String,
    atom_id: String,
    node_num: u64,
    history_event_id: Option<String>,
    acknowledged: bool,
    durability: String,
    crc32: String,
}

impl DirectIngestReceiptWire {
    fn body(self) -> DirectIngestReceiptBodyWire {
        DirectIngestReceiptBodyWire {
            schema: self.schema,
            version: self.version,
            operation: self.operation,
            result: self.result,
            transaction_id: self.transaction_id,
            semantic_time_unix_ns: self.semantic_time_unix_ns,
            intent_hash: self.intent_hash,
            base_binding_hash: self.base_binding_hash,
            committed_generation: self.committed_generation,
            commit_hash: self.commit_hash,
            logical_digest: self.logical_digest,
            atom_id: self.atom_id,
            node_num: self.node_num,
            history_event_id: self.history_event_id,
            acknowledged: self.acknowledged,
            durability: self.durability,
        }
    }

    fn from_body(body: DirectIngestReceiptBodyWire, crc32: String) -> Self {
        Self {
            schema: body.schema,
            version: body.version,
            operation: body.operation,
            result: body.result,
            transaction_id: body.transaction_id,
            semantic_time_unix_ns: body.semantic_time_unix_ns,
            intent_hash: body.intent_hash,
            base_binding_hash: body.base_binding_hash,
            committed_generation: body.committed_generation,
            commit_hash: body.commit_hash,
            logical_digest: body.logical_digest,
            atom_id: body.atom_id,
            node_num: body.node_num,
            history_event_id: body.history_event_id,
            acknowledged: body.acknowledged,
            durability: body.durability,
            crc32,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectIngestFailureBodyWire {
    schema: String,
    version: u64,
    operation: String,
    code: String,
    message: String,
    transaction_id: Option<String>,
    intent_hash: Option<String>,
    commit_disposition: String,
    acknowledged: bool,
    retry: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectIngestFailureWire {
    schema: String,
    version: u64,
    operation: String,
    code: String,
    message: String,
    transaction_id: Option<String>,
    intent_hash: Option<String>,
    commit_disposition: String,
    acknowledged: bool,
    retry: String,
    crc32: String,
}

impl DirectIngestFailureWire {
    fn from_body(body: DirectIngestFailureBodyWire, crc32: String) -> Self {
        Self {
            schema: body.schema,
            version: body.version,
            operation: body.operation,
            code: body.code,
            message: body.message,
            transaction_id: body.transaction_id,
            intent_hash: body.intent_hash,
            commit_disposition: body.commit_disposition,
            acknowledged: body.acknowledged,
            retry: body.retry,
            crc32,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProductionHistoryDetailsWire {
    atom_type_u32: String,
    claim_count: String,
    evidence_ref_count: String,
    result_kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProductionHistoryEventWire {
    schema_version: String,
    event_id: String,
    transaction_id: String,
    event_ordinal: u32,
    generation: u64,
    timestamp_unix_ns: u64,
    operation: String,
    event_kind: String,
    outcome: String,
    atom_ids: Vec<String>,
    details: ProductionHistoryDetailsWire,
    intent_hash: String,
    event_semantic_hash: String,
}

macro_rules! production_crc_record {
    ($record:ident, $body:ident { $($field:ident : $ty:ty),+ $(,)? }) => {
        #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
        #[serde(deny_unknown_fields)]
        struct $body {
            $(pub(crate) $field: $ty),+
        }

        #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
        #[serde(deny_unknown_fields)]
        struct $record {
            $(pub(crate) $field: $ty),+,
            pub(crate) crc32: String,
        }

        impl $record {
            fn from_body(body: $body) -> io::Result<Self> {
                let body_bytes = serde_json::to_vec(&body).map_err(io::Error::other)?;
                let crc32 = format!("{:08x}", production_crc(&body.schema, &body_bytes));
                Ok(Self {
                    $($field: body.$field),+,
                    crc32,
                })
            }

            fn body(&self) -> $body {
                $body {
                    $($field: self.$field.clone()),+
                }
            }

            fn canonical_bytes(&self) -> io::Result<Vec<u8>> {
                serde_json::to_vec(self).map_err(io::Error::other)
            }

            fn decode(bytes: &[u8], label: &str) -> io::Result<Self> {
                let record: Self = decode_production_json(bytes, label)?;
                let body = record.body();
                verify_production_crc(&body.schema, &body, &record.crc32)?;
                Ok(record)
            }
        }
    };
}

macro_rules! production_update_crc_record {
    ($record:ident, $body:ident { $($field:ident : $ty:ty),+ $(,)? }) => {
        #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
        #[serde(deny_unknown_fields)]
        struct $body {
            $(pub(crate) $field: $ty),+
        }

        #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
        #[serde(deny_unknown_fields)]
        struct $record {
            $(pub(crate) $field: $ty),+,
            pub(crate) crc32: String,
        }

        impl $record {
            fn from_body(body: $body) -> io::Result<Self> {
                let body_bytes = serde_json::to_vec(&body).map_err(io::Error::other)?;
                let crc32 = format!("{:08x}", production_update_control_crc(&body_bytes)?);
                Ok(Self {
                    $($field: body.$field),+,
                    crc32,
                })
            }

            fn body(&self) -> $body {
                $body {
                    $($field: self.$field.clone()),+
                }
            }

            fn canonical_bytes(&self) -> io::Result<Vec<u8>> {
                serde_json::to_vec(self).map_err(io::Error::other)
            }

            fn decode(bytes: &[u8], label: &str) -> io::Result<Self> {
                let record: Self = decode_production_json(bytes, label)?;
                let body = record.body();
                verify_production_update_crc(&body, &record.crc32)?;
                Ok(record)
            }
        }
    };
}

production_crc_record!(BatchIngestReceiptWire, BatchIngestReceiptBodyWire {
    schema: String,
    version: u64,
    operation: String,
    result: String,
    transaction_id: String,
    semantic_time_unix_ns: u64,
    intent_hash: String,
    base_binding_hash: String,
    item_count: u32,
    created_count: u32,
    reused_count: u32,
    refused_count: u32,
    committed_generation: u64,
    commit_hash: String,
    logical_digest: String,
    decision_hashes: Vec<String>,
    history_event_id: Option<String>,
    acknowledged: bool,
    durability: String,
});

production_crc_record!(BatchIngestFailureWire, BatchIngestFailureBodyWire {
    schema: String,
    version: u64,
    operation: String,
    code: String,
    message: String,
    transaction_id: Option<String>,
    intent_hash: Option<String>,
    item_ordinal: Option<u32>,
    commit_disposition: String,
    acknowledged: bool,
    retry: String,
});

production_update_crc_record!(UpdateAtomFailureWire, UpdateAtomFailureBodyWire {
    schema: String,
    version: u64,
    operation: String,
    code: String,
    message: String,
    transaction_id: Option<String>,
    intent_hash: Option<String>,
    commit_disposition: String,
    acknowledged: bool,
    retry: String,
});

fn validate_batch_receipt_wire(bytes: &[u8]) -> io::Result<BatchIngestReceiptWire> {
    let wire = BatchIngestReceiptWire::decode(bytes, "batch receipt")?;
    validate_production_uuid(&wire.transaction_id)?;
    let counts = wire
        .created_count
        .checked_add(wire.reused_count)
        .and_then(|value| value.checked_add(wire.refused_count))
        .ok_or_else(|| invalid_data("batch receipt count overflow"))?;
    if wire.schema != PRODUCTION_BATCH_RECEIPT_SCHEMA
        || wire.version != 1
        || wire.operation != "batch_ingest"
        || wire.semantic_time_unix_ns == 0
        || wire.item_count == 0
        || wire.item_count as usize > PRODUCTION_BATCH_MAX_ITEMS
        || counts != wire.item_count
        || wire.decision_hashes.len() != wire.item_count as usize
        || !wire.acknowledged
        || !is_hash(&wire.intent_hash)
        || !is_hash(&wire.base_binding_hash)
        || !is_hash(&wire.commit_hash)
        || !is_hash(&wire.logical_digest)
        || wire.decision_hashes.iter().any(|value| !is_hash(value))
        || wire
            .history_event_id
            .as_ref()
            .is_some_and(|value| !is_hash(value))
    {
        return Err(invalid_data("batch receipt fixed fields are invalid"));
    }
    match wire.result.as_str() {
        "committed"
            if wire.durability == "committed"
                && wire.created_count > 0
                && wire.committed_generation > 0
                && wire.commit_hash != PRODUCTION_ZERO_HASH
                && wire.history_event_id.is_some() => {}
        "unchanged"
            if wire.durability == "unchanged"
                && wire.created_count == 0
                && wire.history_event_id.is_none()
                && ((wire.committed_generation == 0)
                    == (wire.commit_hash == PRODUCTION_ZERO_HASH)) => {}
        _ => return Err(invalid_data("batch receipt result state is invalid")),
    }
    Ok(wire)
}

fn validate_batch_failure_wire(bytes: &[u8]) -> io::Result<BatchIngestFailureWire> {
    let wire = BatchIngestFailureWire::decode(bytes, "batch failure")?;
    if wire.schema != "memoryx.batch-ingest-failure.v1"
        || wire.version != 1
        || wire.operation != "batch_ingest"
        || wire.acknowledged
        || !matches!(
            wire.code.as_str(),
            "explicit_transaction_envelope_required"
                | "base_not_admitted"
                | "base_binding_mismatch"
                | "parent_state_changed"
                | "invalid_transaction_id"
                | "invalid_semantic_time"
                | "invalid_intent"
                | "invalid_batch_item"
                | "bounds_exceeded"
                | "conflicting_transaction_reuse"
                | "tombstoned_identity"
                | "canonical_representation_conflict"
                | "invalid_request"
                | "evidence_source_not_live"
                | "graph_compaction_required"
                | "nested_transaction_forbidden"
                | "composite_operation_not_admitted"
                | "transport_write_not_ratified"
                | "unsupported_platform"
                | "migration_required"
                | "recovery_required"
                | "unsupported_or_corrupt"
        )
        || !matches!(
            wire.commit_disposition.as_str(),
            "not_started"
                | "not_committed"
                | "committed_install_pending"
                | "indeterminate_fail_closed"
        )
        || !matches!(
            wire.retry.as_str(),
            "never" | "same_transaction" | "after_recovery_same_transaction"
        )
        || wire
            .transaction_id
            .as_ref()
            .is_some_and(|value| validate_production_uuid(value).is_err())
        || wire
            .intent_hash
            .as_ref()
            .is_some_and(|value| !is_hash(value))
        || wire.message.is_empty()
        || wire.message.len() > 512
    {
        return Err(invalid_data("batch failure fixed fields are invalid"));
    }
    Ok(wire)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ProductionStorageLimitsV1 {
    max_request_atoms: u32,
    max_unique_atom_ids: u32,
    max_atom_body_bytes: u64,
    max_staged_record_bytes: u64,
    max_new_cas_extent_bytes: u64,
    max_affected_segments: u32,
    max_component_count: u32,
    max_pair_count: u32,
    max_component_bytes: u64,
    max_total_staged_generation_bytes: u64,
    max_control_record_bytes: u64,
    max_base_binding_bytes: u64,
    max_canonical_root_key_bytes: u64,
    max_posix_root_key_bytes: u64,
    max_direct_intent_bytes: u64,
    max_claim_count: u64,
    max_claim_projection_bytes: u64,
    max_evidence_ref_count: u64,
    max_evidence_projection_bytes: u64,
    max_combined_projection_bytes: u64,
    max_failure_message_bytes: u64,
    max_path_count: u32,
    max_path_depth: u16,
    max_path_bytes: u16,
    max_total_path_bytes: u64,
    max_transaction_directory_entries: u32,
    max_committed_generations: u32,
    max_location_records: u64,
    idloc_default_shard_bits: u8,
    idloc_max_shard_bits: u8,
    max_lexical_terms: u64,
    max_lexical_utf8_bytes: u64,
    max_posting_values: u64,
    max_posting_payload_bytes: u64,
    max_combined_lexical_pair_bytes: u64,
    max_post_commit_install_scratch_bytes: u64,
    minimum_free_space_reserve_bytes: u64,
}

impl ProductionStorageLimitsV1 {
    const fn frozen() -> Self {
        Self {
            max_request_atoms: 1,
            max_unique_atom_ids: 1,
            max_atom_body_bytes: 67_108_784,
            max_staged_record_bytes: 67_108_864,
            max_new_cas_extent_bytes: 67_108_864,
            max_affected_segments: 1,
            max_component_count: 64,
            max_pair_count: 8,
            max_component_bytes: 17_179_869_184,
            max_total_staged_generation_bytes: 68_719_476_736,
            max_control_record_bytes: 1_048_576,
            max_base_binding_bytes: 131_072,
            max_canonical_root_key_bytes: 65_538,
            max_posix_root_key_bytes: 4_096,
            max_direct_intent_bytes: 67_108_864,
            max_claim_count: 1_000_000,
            max_claim_projection_bytes: 25_000_000,
            max_evidence_ref_count: 1_000_000,
            max_evidence_projection_bytes: 54_000_000,
            max_combined_projection_bytes: 67_108_784,
            max_failure_message_bytes: 512,
            max_path_count: 256,
            max_path_depth: 8,
            max_path_bytes: 240,
            max_total_path_bytes: 32_768,
            max_transaction_directory_entries: 4_096,
            max_committed_generations: 4_096,
            max_location_records: 100_000_000,
            idloc_default_shard_bits: 8,
            idloc_max_shard_bits: 20,
            max_lexical_terms: 100_000_000,
            max_lexical_utf8_bytes: 8_589_934_592,
            max_posting_values: 1_000_000_000,
            max_posting_payload_bytes: 17_179_869_184,
            max_combined_lexical_pair_bytes: 34_359_738_368,
            max_post_commit_install_scratch_bytes: 17_179_869_184,
            minimum_free_space_reserve_bytes: 268_435_456,
        }
    }
}

production_crc_record!(
    ProductionFormatRecordV2,
    ProductionFormatBodyV2 {
        schema: String,
        version: u64,
        format_version: u64,
        codec_id: String,
        registry_id: String,
        digest_id: String,
        component_root_id: String,
        orphan_digest_id: String,
        limits_id: String,
        legacy_layout_id: String,
        downgrade_policy_id: String,
        baseline_hash: String,
        migration_hash: String,
        minimum_writer_capability: String,
    }
);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ProductionPlannedComponentV1 {
    registry_key: String,
    registry_order: u16,
    ordinal: u32,
    mode: String,
    target_path: Option<String>,
    stage_path: Option<String>,
    content_codec_id: String,
    pair_id: Option<String>,
}

production_crc_record!(ProductionPrepareRecordV1, ProductionPrepareBodyV1 {
    schema: String,
    version: u64,
    format_version: u64,
    generation: u64,
    parent_commit_hash: String,
    transaction_id: String,
    semantic_time_unix_ns: u64,
    base_binding_hash: String,
    envelope_hash: String,
    operation: String,
    intent_hash: String,
    codec_id: String,
    registry_id: String,
    digest_id: String,
    limits_id: String,
    limits: ProductionStorageLimitsV1,
    planned_components: Vec<ProductionPlannedComponentV1>,
});

production_crc_record!(ProductionComponentDescriptorV1, ProductionComponentDescriptorBodyV1 {
    schema: String,
    version: u64,
    registry_key: String,
    registry_order: u16,
    ordinal: u32,
    mode: String,
    target_path: Option<String>,
    stage_path: Option<String>,
    content_codec_id: String,
    byte_length: u64,
    byte_hash: String,
    semantic_hash: String,
    record_count: u64,
    pair_id: Option<String>,
});

production_crc_record!(
    ProductionPairDescriptorV1,
    ProductionPairDescriptorBodyV1 {
        schema: String,
        version: u64,
        pair_id: String,
        left_registry_key: String,
        left_ordinal: u32,
        right_registry_key: String,
        right_ordinal: u32,
        left_record_count: u64,
        right_record_count: u64,
        logical_item_count: u64,
        auxiliary_count: u64,
        shared_semantic_root: String,
    }
);

production_crc_record!(
    CasOrphanDescriptorV1,
    CasOrphanDescriptorBodyV1 {
        schema: String,
        version: u64,
        transaction_id: String,
        record_ordinal: u32,
        atom_id: String,
        body_len: u64,
        body_crc32: String,
        body_hash: String,
        record_len: u64,
        record_hash: String,
        staged_component_key: String,
        staged_component_hash: String,
        segment_id: u32,
        segment_existed: bool,
        segment_pre_len: u64,
        record_offset: u64,
        record_extent_len: u64,
        segment_post_len: u64,
        record_flags: u16,
        idx_fp64_bits: String,
        idx_seg_offset: u64,
        idx_body_len: u32,
        idx_flags: u32,
        post_index_component_key: String,
        post_index_component_hash: String,
        parent_generation: u64,
        parent_commit_hash: String,
        pinned_physical_root: String,
    }
);

production_crc_record!(ProductionGenerationManifestV1, ProductionGenerationManifestBodyV1 {
    schema: String,
    version: u64,
    format_version: u64,
    generation: u64,
    parent_commit_hash: String,
    prepare_hash: String,
    transaction_id: String,
    semantic_time_unix_ns: u64,
    base_binding_hash: String,
    envelope_hash: String,
    operation: String,
    intent_hash: String,
    codec_id: String,
    registry_id: String,
    digest_id: String,
    limits_id: String,
    components: Vec<ProductionComponentDescriptorV1>,
    pairs: Vec<ProductionPairDescriptorV1>,
    component_root_hash: String,
    logical_state_digest: String,
    orphan_inventory_digest: String,
    history_event_hash: String,
    history_event_count: u64,
});

production_crc_record!(
    BatchCasAppendDescriptorV1,
    BatchCasAppendDescriptorBodyV1 {
        schema: String,
        version: u64,
        segment_id: u32,
        ordinal: u32,
        item_ordinal: u32,
        atom_id: String,
        body_length: u64,
        body_crc32: String,
        body_hash: String,
        record_offset: u64,
        record_extent_length: u64,
        pre_segment_length: u64,
        post_segment_length: u64,
        staged_record_hash: String,
        idx_entry_hash: String,
    }
);

production_crc_record!(BatchPrepareV1, BatchPrepareBodyV1 {
    schema: String,
    version: u64,
    format_version: u64,
    generation: u64,
    parent_commit_hash: String,
    transaction_id: String,
    semantic_time_unix_ns: u64,
    base_binding_hash: String,
    envelope_hash: String,
    operation: String,
    intent_hash: String,
    codec_id: String,
    registry_id: String,
    operation_registry_id: String,
    digest_id: String,
    limits_id: String,
    preflight_hash: String,
    decision_hashes: Vec<String>,
    components: Vec<ProductionComponentDescriptorV1>,
    pairs: Vec<ProductionPairDescriptorV1>,
    component_root_hash: String,
    logical_state_digest: String,
    orphan_inventory_digest: String,
    history_event_hash: String,
    history_event_count: u64,
    post_atom_count: u64,
});

production_crc_record!(BatchGenerationManifestV1, BatchGenerationManifestBodyV1 {
    schema: String,
    version: u64,
    format_version: u64,
    generation: u64,
    parent_commit_hash: String,
    prepare_hash: String,
    transaction_id: String,
    semantic_time_unix_ns: u64,
    base_binding_hash: String,
    envelope_hash: String,
    operation: String,
    intent_hash: String,
    codec_id: String,
    registry_id: String,
    operation_registry_id: String,
    digest_id: String,
    limits_id: String,
    decision_hashes: Vec<String>,
    components: Vec<ProductionComponentDescriptorV1>,
    pairs: Vec<ProductionPairDescriptorV1>,
    component_root_hash: String,
    logical_state_digest: String,
    orphan_inventory_digest: String,
    history_event_hash: String,
    history_event_count: u64,
    post_atom_count: u64,
});

production_update_crc_record!(
    UpdateComponentDescriptorV1,
    UpdateComponentDescriptorBodyV1 {
        schema: String,
        version: u64,
        registry_order: u16,
        registry_key: String,
        ordinal: u32,
        mode: String,
        target_path: String,
        stage_path: String,
        content_codec_id: String,
        byte_length: u64,
        byte_hash: String,
        semantic_hash: String,
        descriptor_hash: String,
    }
);

production_update_crc_record!(UpdateHistoryEventV1, UpdateHistoryEventBodyV1 {
    schema: String,
    version: u64,
    event_id: String,
    transaction_id: String,
    event_ordinal: u32,
    generation: u64,
    semantic_time_unix_ns: u64,
    operation: String,
    outcome: String,
    atom_ids: Vec<String>,
    supersedes_relation_id: String,
    intent_hash: String,
    successor_provenance_hash: String,
    old_provenance_hash: String,
    history_semantic_hash: String,
});

production_update_crc_record!(
    UpdateRelationJournalV1,
    UpdateRelationJournalBodyV1 {
        schema: String,
        version: u64,
        journal_kind: String,
        ordinal: u32,
        relation_atom_id: String,
        subject_atom_id: String,
        predicate_id: u32,
        object_atom_id: String,
        current: bool,
        historical: bool,
    }
);

production_update_crc_record!(UpdatePrepareV1, UpdatePrepareBodyV1 {
    schema: String,
    version: u64,
    format_version: u64,
    generation: u64,
    parent_commit_hash: String,
    transaction_id: String,
    semantic_time_unix_ns: u64,
    base_binding_hash: String,
    envelope_hash: String,
    operation: String,
    intent_hash: String,
    operation_registry_id: String,
    limits_id: String,
    old_atom_id: String,
    successor_atom_id: String,
    successor_body_hash: String,
    claim_projection_hash: String,
    api_evidence_projection_hash: String,
    successor_atom_type: u32,
    old_node: u64,
    successor_node: u64,
    supersedes_relation_id: String,
    old_provenance_hash: String,
    successor_provenance_hash: String,
    successor_source_attachment_hash: String,
    history_event_id: String,
    history_semantic_hash: String,
    component_root_hash: String,
    logical_state_digest: String,
    components: Vec<UpdateComponentDescriptorV1>,
});

production_update_crc_record!(UpdateGenerationManifestV1, UpdateGenerationManifestBodyV1 {
    schema: String,
    version: u64,
    format_version: u64,
    generation: u64,
    parent_commit_hash: String,
    prepare_hash: String,
    transaction_id: String,
    semantic_time_unix_ns: u64,
    base_binding_hash: String,
    envelope_hash: String,
    operation: String,
    intent_hash: String,
    operation_registry_id: String,
    limits_id: String,
    old_atom_id: String,
    successor_atom_id: String,
    successor_body_hash: String,
    claim_projection_hash: String,
    api_evidence_projection_hash: String,
    successor_atom_type: u32,
    old_node: u64,
    successor_node: u64,
    supersedes_relation_id: String,
    old_provenance_hash: String,
    successor_provenance_hash: String,
    successor_source_attachment_hash: String,
    history_event_id: String,
    history_semantic_hash: String,
    history_event_count: u64,
    relation_count: u64,
    post_atom_count: u64,
    graph_leaf_count: u64,
    component_root_hash: String,
    logical_state_digest: String,
    components: Vec<UpdateComponentDescriptorV1>,
});

production_crc_record!(ProductionBaselineManifestV1, ProductionBaselineManifestBodyV1 {
    schema: String,
    version: u64,
    format_version: u64,
    source_layout_id: String,
    registry_id: String,
    limits_id: String,
    components: Vec<ProductionComponentDescriptorV1>,
    pairs: Vec<ProductionPairDescriptorV1>,
    component_root_hash: String,
    legacy_semantic_digest: String,
    root_tree_digest: String,
    component_count: u64,
    total_bytes: u64,
});

production_crc_record!(
    ProductionMigrationRecordV1,
    ProductionMigrationBodyV1 {
        schema: String,
        version: u64,
        source_layout_id: String,
        target_format_version: u64,
        registry_id: String,
        limits_id: String,
        baseline_manifest_hash: String,
        backup_manifest_hash: String,
        component_count: u64,
        total_bytes: u64,
        required_free_bytes: u64,
        source_untouched: bool,
        rollback_policy: String,
        first_commit_generation: u64,
    }
);

production_crc_record!(
    ProductionStartupAdmissionV1,
    ProductionStartupAdmissionBodyV1 {
        schema: String,
        version: u64,
        format_version: u64,
        classification: String,
        codec_id: String,
        registry_id: String,
        limits_id: String,
        base_binding_hash: String,
        head_generation: u64,
        head_commit_hash: String,
        head_logical_digest: String,
        install_state: String,
        component_open_mode: String,
        live_view_state: String,
    }
);

struct ProductionBinaryCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> ProductionBinaryCursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn expect_domain(&mut self, domain: &str) -> io::Result<()> {
        let expected = domain.as_bytes();
        if self.take(expected.len())? != expected || self.read_u8()? != 0 {
            return Err(invalid_data("production binary codec domain is invalid"));
        }
        Ok(())
    }

    fn take(&mut self, length: usize) -> io::Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| invalid_data("production binary cursor overflow"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| invalid_data("production binary record is truncated"))?;
        self.offset = end;
        Ok(value)
    }

    fn read_u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn read_u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn read_u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn read_u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn read_hash(&mut self) -> io::Result<[u8; 32]> {
        Ok(self.take(32)?.try_into().unwrap())
    }

    fn read_uuid(&mut self) -> io::Result<[u8; 16]> {
        Ok(self.take(16)?.try_into().unwrap())
    }

    fn read_exact_vec(&mut self, length: u64, maximum: u64) -> io::Result<Vec<u8>> {
        if length > maximum {
            return Err(invalid_data("production binary frame exceeds its limit"));
        }
        let length = usize::try_from(length)
            .map_err(|_| invalid_data("production binary frame does not fit usize"))?;
        Ok(self.take(length)?.to_vec())
    }

    fn read_u64_frame(&mut self, maximum: u64) -> io::Result<Vec<u8>> {
        let length = self.read_u64()?;
        self.read_exact_vec(length, maximum)
    }

    fn finish(self) -> io::Result<()> {
        if self.offset != self.bytes.len() {
            return Err(invalid_data("production binary record has trailing bytes"));
        }
        Ok(())
    }
}

fn append_u64_frame(target: &mut Vec<u8>, value: &[u8]) -> io::Result<()> {
    let length = u64::try_from(value.len())
        .map_err(|_| invalid_data("production binary frame length overflow"))?;
    target.extend_from_slice(&length.to_le_bytes());
    target.extend_from_slice(value);
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn parse_hash_hex(value: &str, label: &str) -> io::Result<[u8; 32]> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid_data(&format!(
            "{label} is not canonical lowercase hexadecimal"
        )));
    }
    let mut output = [0u8; 32];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        output[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Ok(output)
}

fn hex_nibble(value: u8) -> io::Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(invalid_data("invalid lowercase hexadecimal digit")),
    }
}

fn validate_production_uuid(value: &str) -> io::Result<[u8; 16]> {
    if value.len() != 36
        || !value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
    {
        return Err(invalid_data(
            "transaction UUID is not canonical lowercase ASCII",
        ));
    }
    let compact = value
        .bytes()
        .filter(|byte| *byte != b'-')
        .collect::<Vec<_>>();
    let mut output = [0u8; 16];
    for (index, pair) in compact.as_chunks::<2>().0.iter().enumerate() {
        output[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    validate_uuid_bytes(output)?;
    Ok(output)
}

fn validate_uuid_bytes(value: [u8; 16]) -> io::Result<()> {
    let version = value[6] >> 4;
    if value == [0; 16] || !(1..=8).contains(&version) || value[8] & 0xc0 != 0x80 {
        return Err(invalid_data(
            "transaction UUID version or RFC variant is invalid",
        ));
    }
    Ok(())
}

fn uuid_to_string(value: [u8; 16]) -> String {
    let compact = hex_lower(&value);
    format!(
        "{}-{}-{}-{}-{}",
        &compact[0..8],
        &compact[8..12],
        &compact[12..16],
        &compact[16..20],
        &compact[20..32]
    )
}

fn validate_platform_root_payload(
    platform_code: u8,
    canonical_root_key: &[u8],
    stable_root_identity: &[u8],
) -> io::Result<()> {
    match platform_code {
        1 => {
            if stable_root_identity.len() != 24 || canonical_root_key.len() < 6 {
                return Err(invalid_data(
                    "Windows base binding identity width is invalid",
                ));
            }
            let count = u32::from_le_bytes(canonical_root_key[0..4].try_into().unwrap()) as usize;
            if count == 0
                || count > 32767
                || canonical_root_key.len() != 4usize.saturating_add(count.saturating_mul(2))
            {
                return Err(invalid_data(
                    "Windows canonical root payload length is invalid",
                ));
            }
        }
        2 => {
            if stable_root_identity.len() != 16
                || canonical_root_key.is_empty()
                || canonical_root_key.len() > 4096
                || canonical_root_key[0] != b'/'
                || canonical_root_key.contains(&0)
            {
                return Err(invalid_data("POSIX base binding identity is invalid"));
            }
        }
        _ => return Err(invalid_data("base binding platform is unsupported")),
    }
    if stable_root_identity.iter().all(|byte| *byte == 0) {
        return Err(invalid_data("base binding stable identity is zero"));
    }
    Ok(())
}

fn validate_claim_projection(bytes: &[u8]) -> io::Result<()> {
    let mut previous: Option<(u64, u32, u8, u64, u32)> = None;
    for record in bytes.as_chunks::<25>().0 {
        let tuple = (
            u64::from_le_bytes(record[0..8].try_into().unwrap()),
            u32::from_le_bytes(record[8..12].try_into().unwrap()),
            record[12],
            u64::from_le_bytes(record[13..21].try_into().unwrap()),
            u32::from_le_bytes(record[21..25].try_into().unwrap()),
        );
        if !matches!(tuple.2, 0 | 1 | 2 | 3 | 4 | 6 | 8)
            || (tuple.2 == 0 && tuple.3 != 0)
            || (tuple.2 == 1 && tuple.3 > 1)
            || (tuple.2 == 6 && tuple.3 > u32::MAX as u64)
            || previous.is_some_and(|value| value >= tuple)
        {
            return Err(invalid_data("claim projection is noncanonical"));
        }
        previous = Some(tuple);
    }
    Ok(())
}

fn validate_evidence_projection(bytes: &[u8]) -> io::Result<()> {
    let mut previous: Option<([u8; 32], u32, u64, u64, u16)> = None;
    for record in bytes.as_chunks::<54>().0 {
        let atom_id: [u8; 32] = record[0..32].try_into().unwrap();
        let section = u32::from_le_bytes(record[32..36].try_into().unwrap());
        let offset = u64::from_le_bytes(record[36..44].try_into().unwrap());
        let length = u64::from_le_bytes(record[44..52].try_into().unwrap());
        let trust = u16::from_le_bytes(record[52..54].try_into().unwrap());
        let tuple = (atom_id, section, offset, length, trust);
        if atom_id == [0; 32]
            || !(1..=7).contains(&section)
            || trust > 10_000
            || offset.checked_add(length).is_none()
            || previous.as_ref().is_some_and(|value| value >= &tuple)
        {
            return Err(invalid_data("evidence projection is noncanonical"));
        }
        previous = Some(tuple);
    }
    Ok(())
}

fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f6_3b78 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn production_crc(schema: &str, body_bytes: &[u8]) -> u32 {
    let mut bytes = Vec::with_capacity(schema.len() + 1 + body_bytes.len());
    bytes.extend_from_slice(schema.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(body_bytes);
    crc32(&bytes)
}

fn production_update_control_crc(body_bytes: &[u8]) -> io::Result<u32> {
    if !body_bytes.is_ascii() || body_bytes.last() != Some(&b'}') {
        return Err(invalid_data(
            "update control record body is not canonical ASCII JSON",
        ));
    }
    Ok(crc32(&body_bytes[..body_bytes.len() - 1]))
}

fn encode_receipt_wire(body: &DirectIngestReceiptBodyWire) -> io::Result<Vec<u8>> {
    let body_bytes = serde_json::to_vec(body).map_err(io::Error::other)?;
    let crc32 = production_crc(&body.schema, &body_bytes);
    serde_json::to_vec(&DirectIngestReceiptWire::from_body(
        body.clone(),
        format!("{crc32:08x}"),
    ))
    .map_err(io::Error::other)
}

fn decode_production_json<T: for<'de> Deserialize<'de> + Serialize>(
    bytes: &[u8],
    label: &str,
) -> io::Result<T> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_CONTROL_RECORD_BYTES {
        return Err(invalid_data(&format!(
            "production {label} record length is invalid"
        )));
    }
    if bytes.iter().any(|byte| !byte.is_ascii()) {
        return Err(invalid_data(&format!(
            "production {label} record contains noncanonical bytes"
        )));
    }
    let value: T = serde_json::from_slice(bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid production {label} record: {error}"),
        )
    })?;
    if serde_json::to_vec(&value).map_err(io::Error::other)? != bytes {
        return Err(invalid_data(&format!(
            "production {label} record is not canonical"
        )));
    }
    Ok(value)
}

fn verify_production_crc<T: Serialize>(schema: &str, body: &T, found: &str) -> io::Result<()> {
    if found.len() != 8
        || !found
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid_data("production record crc32 is not canonical"));
    }
    let expected = production_crc(schema, &serde_json::to_vec(body).map_err(io::Error::other)?);
    if found != format!("{expected:08x}") {
        return Err(invalid_data("production record crc32 does not match"));
    }
    Ok(())
}

fn verify_production_update_crc<T: Serialize>(body: &T, found: &str) -> io::Result<()> {
    if found.len() != 8
        || !found
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid_data("update record crc32 is not canonical"));
    }
    let body_bytes = serde_json::to_vec(body).map_err(io::Error::other)?;
    let expected = production_update_control_crc(&body_bytes)?;
    if found != format!("{expected:08x}") {
        return Err(invalid_data("update record crc32 does not match"));
    }
    Ok(())
}

fn validate_failure_combination(
    code: DirectIngestFailureCodeV1,
    disposition: DirectIngestCommitDispositionV1,
    retry: DirectIngestRetryV1,
    transaction_present: bool,
    intent_present: bool,
) -> io::Result<()> {
    let valid = match disposition {
        DirectIngestCommitDispositionV1::NotStarted => {
            retry
                == if matches!(
                    code,
                    DirectIngestFailureCodeV1::BaseNotAdmitted
                        | DirectIngestFailureCodeV1::RecoveryRequired
                ) {
                    DirectIngestRetryV1::SameTransaction
                } else {
                    DirectIngestRetryV1::Never
                }
        }
        DirectIngestCommitDispositionV1::NotCommitted
        | DirectIngestCommitDispositionV1::CommittedInstallPending => {
            code == DirectIngestFailureCodeV1::RecoveryRequired
                && retry == DirectIngestRetryV1::SameTransaction
                && transaction_present
                && intent_present
        }
        DirectIngestCommitDispositionV1::IndeterminateFailClosed => {
            matches!(
                code,
                DirectIngestFailureCodeV1::RecoveryRequired
                    | DirectIngestFailureCodeV1::UnsupportedOrCorrupt
            ) && retry == DirectIngestRetryV1::AfterRecoverySameTransaction
                && transaction_present
                && intent_present
        }
    };
    if valid {
        Ok(())
    } else {
        Err(invalid_data(
            "direct-ingest failure state combination is invalid",
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProductionAtomStateV1 {
    pub(crate) atom_id: AtomId,
    pub(crate) atom_type: AtomType,
    pub(crate) node_num: u64,
    pub(crate) committed_generation: u64,
    pub(crate) body_len: u64,
    pub(crate) body_crc32: u32,
    pub(crate) body_hash: [u8; 32],
    pub(crate) segment_id: u32,
    pub(crate) record_offset: u64,
    pub(crate) record_extent_len: u64,
    pub(crate) domain_mask: u64,
    pub(crate) created_at_ns: u64,
    pub(crate) trust_level: u16,
    pub(crate) source_id: u32,
    pub(crate) provenance_hash: [u8; 32],
    pub(crate) history_event_id: [u8; 32],
    pub(crate) history_leaf: Vec<u8>,
}

#[derive(Debug, Clone)]
enum ProductionOwnerLifetimeTransactionV1 {
    Direct(DirectIngestReceiptV1),
    Batch(BatchIngestReceiptV1),
    Update(Box<UpdateAtomReceiptV1>),
}

#[derive(Debug, Clone)]
pub(crate) struct ProductionRuntimeStateV1 {
    pub(crate) head: ProductionCommittedHead,
    pub(crate) base_binding: ProductionBaseBindingV1,
    pub(crate) admission_bytes: Vec<u8>,
    pub(crate) atom: Option<ProductionAtomStateV1>,
    pub(crate) atoms: Vec<ProductionAtomStateV1>,
    pub(crate) history_leaves: Vec<Vec<u8>>,
    pub(crate) graph_leaves: Vec<Vec<u8>>,
    pub(crate) superseded_by: BTreeMap<AtomId, AtomId>,
    committed_receipts: BTreeMap<String, DirectIngestReceiptV1>,
    committed_transactions: BTreeMap<String, ProductionCommittedTransactionV1>,
    batch_transactions: BTreeMap<String, ProductionCommittedBatchV1>,
    update_transactions: BTreeMap<String, ProductionCommittedUpdateV1>,
    owner_lifetime_transactions: BTreeMap<String, ProductionOwnerLifetimeTransactionV1>,
}

impl ProductionRuntimeStateV1 {
    pub(crate) fn receipt_for_transaction(
        &self,
        transaction_id: &str,
    ) -> Option<&DirectIngestReceiptV1> {
        self.committed_receipts.get(transaction_id)
    }
}

#[derive(Debug, Clone)]
struct ProductionCommittedTransactionV1 {
    parent: ProductionCommittedHead,
    semantic_time_unix_ns: u64,
    base_binding_hash: [u8; 32],
    intent_hash: [u8; 32],
    envelope_hash: [u8; 32],
}

#[derive(Debug, Clone)]
struct ProductionCommittedBatchV1 {
    parent: ProductionCommittedHead,
    parent_atoms: Vec<ProductionAtomStateV1>,
    parent_history_leaves: Vec<Vec<u8>>,
    semantic_time_unix_ns: u64,
    base_binding_hash: [u8; 32],
    intent_hash: [u8; 32],
    envelope_hash: [u8; 32],
    decision_hashes: Vec<[u8; 32]>,
    created_atom_ids: Vec<AtomId>,
    history_event_id: [u8; 32],
    commit_hash: [u8; 32],
    logical_digest: [u8; 32],
}

#[derive(Debug, Clone)]
struct ProductionCommittedUpdateV1 {
    receipt: UpdateAtomReceiptV1,
    successor_body_hash: [u8; 32],
    claim_projection_hash: [u8; 32],
    api_evidence_projection_hash: [u8; 32],
    successor_source_attachment_hash: [u8; 32],
}

#[derive(Debug, Clone)]
pub(crate) struct ProductionDirectRequestV1 {
    pub(crate) transaction_id: String,
    pub(crate) semantic_time_unix_ns: u64,
    pub(crate) base_binding_bytes: Vec<u8>,
    pub(crate) body: Vec<u8>,
    pub(crate) atom_type: AtomType,
    pub(crate) claim_projection: Vec<u8>,
    pub(crate) evidence_projection: Vec<u8>,
}

#[derive(Debug, Clone)]
struct ProductionDetachedComponentsV1 {
    staged_record: Vec<u8>,
    segment_index: Vec<u8>,
    location_state: Vec<u8>,
    idloc: Vec<u8>,
    lexicon: Vec<u8>,
    postings: Vec<u8>,
    graph_manifest: Vec<u8>,
    metadata: Vec<u8>,
    history: Vec<u8>,
    atom: ProductionAtomStateV1,
}

fn production_record_bytes(atom_id: AtomId, body: &[u8], segment_id: u32) -> io::Result<Vec<u8>> {
    let padded_body_len = body
        .len()
        .checked_add(15)
        .map(|length| length & !15)
        .ok_or_else(|| invalid_data("production CAS body extent overflow"))?;
    let extent = RecordHeader::SIZE
        .checked_add(padded_body_len)
        .and_then(|length| length.checked_add(16))
        .ok_or_else(|| invalid_data("production CAS record extent overflow"))?;
    if extent as u64 > ProductionStorageLimitsV1::frozen().max_new_cas_extent_bytes {
        return Err(invalid_data(
            "production CAS record exceeds the frozen extent limit",
        ));
    }

    let header = RecordHeader::new(atom_id, body.len() as u64, segment_id, 0);
    let mut bytes = vec![0; extent];
    header
        .write_to_bytes(&mut bytes[..RecordHeader::SIZE])
        .map_err(|error| {
            invalid_data(&format!("production SKF1 header encoding failed: {error}"))
        })?;
    bytes[RecordHeader::SIZE..RecordHeader::SIZE + body.len()].copy_from_slice(body);
    let crc_offset = RecordHeader::SIZE + padded_body_len;
    bytes[crc_offset..crc_offset + 4].copy_from_slice(&crc32(body).to_le_bytes());
    Ok(bytes)
}

fn production_idx1_bytes(
    atom_id: AtomId,
    record_offset: u64,
    body_len: u64,
) -> io::Result<Vec<u8>> {
    let header = IndexFileHeader::new(1);
    let entry = IndexEntry::new(atom_id, record_offset, body_len, 0);
    // SegmentIndex starts each segment with the current 1024-element Bloom
    // capacity; detached generation bytes must match that writer exactly.
    let mut bloom = BloomFilter::new(1024);
    bloom.insert(&atom_id);
    let bloom_bytes = bloom.to_bytes();
    let mut bytes = vec![0; IndexFileHeader::SIZE + IndexEntry::SIZE];
    // Both structs define their byte representation as the current native
    // little-endian compatibility layout; production-v2 rejects big-endian.
    let header_bytes = unsafe {
        std::slice::from_raw_parts(
            &header as *const IndexFileHeader as *const u8,
            IndexFileHeader::SIZE,
        )
    };
    bytes[..IndexFileHeader::SIZE].copy_from_slice(header_bytes);
    entry
        .write_to_bytes(&mut bytes[IndexFileHeader::SIZE..])
        .map_err(|error| {
            invalid_data(&format!("production IDX1 entry encoding failed: {error}"))
        })?;
    bytes.extend_from_slice(&(bloom_bytes.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&bloom_bytes);
    Ok(bytes)
}

fn production_loc1_bytes(atom: &ProductionAtomStateV1, shard_bits: u8) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(24 + 65);
    bytes.extend_from_slice(&0x4c4f4331u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.push(shard_bits);
    bytes.push(0);
    bytes.extend_from_slice(&atom.node_num.saturating_add(1).to_le_bytes());
    bytes.extend_from_slice(&1u64.to_le_bytes());
    bytes.extend_from_slice(&atom.atom_id);
    bytes.extend_from_slice(&atom.node_num.to_le_bytes());
    bytes.extend_from_slice(&atom.segment_id.to_le_bytes());
    bytes.extend_from_slice(&atom.record_offset.to_le_bytes());
    bytes.extend_from_slice(&(atom.body_len as u32).to_le_bytes());
    bytes.extend_from_slice(&atom.domain_mask.to_le_bytes());
    bytes.push(0);
    bytes
}

fn production_idloc_bytes(atom: &ProductionAtomStateV1, shard_bits: u8) -> Vec<u8> {
    let mut builder = IdLocBuilder::new(shard_bits);
    builder.add(
        &atom.atom_id,
        atom.segment_id,
        atom.body_len as u32,
        atom.record_offset,
        atom.node_num,
    );
    builder.build_to_vec()
}

fn production_empty_lexical_pair() -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut lexicon = vec![0; LexHeader::SIZE];
    let lex_header = LexHeader::new(128);
    if !lex_header.write_to_bytes(&mut lexicon) {
        return Err(invalid_data("production LEX1 header encoding failed"));
    }
    let mut postings = vec![0; PostHeader::SIZE];
    let post_header = PostHeader::new();
    if !post_header.write_to_bytes(&mut postings) {
        return Err(invalid_data("production PST1 header encoding failed"));
    }
    Ok((lexicon, postings))
}

fn production_graph_manifest_bytes(node_count: u64) -> io::Result<Vec<u8>> {
    let manifest = GraphManifest::new(node_count);
    let mut bytes = vec![0; GraphManifest::SIZE];
    if !manifest.write_to_bytes(&mut bytes) {
        return Err(invalid_data("production GRM1 encoding failed"));
    }
    Ok(bytes)
}

fn production_metadata_bytes(atom: &ProductionAtomStateV1) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(16 + 66);
    bytes.extend_from_slice(&0x4d455431u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&1u64.to_le_bytes());
    bytes.extend_from_slice(&atom.atom_id);
    bytes.extend_from_slice(&atom.node_num.to_le_bytes());
    bytes.extend_from_slice(&atom.atom_type.to_u32().to_le_bytes());
    bytes.extend_from_slice(&atom.created_at_ns.to_le_bytes());
    bytes.extend_from_slice(&atom.trust_level.to_le_bytes());
    bytes.extend_from_slice(&atom.domain_mask.to_le_bytes());
    bytes.extend_from_slice(&atom.source_id.to_le_bytes());
    bytes
}

fn production_idx1_bytes_many(atoms: &[ProductionAtomStateV1]) -> io::Result<Vec<u8>> {
    let count = u16::try_from(atoms.len())
        .map_err(|_| invalid_data("batch IDX1 record count exceeds u16"))?;
    let header = IndexFileHeader::new(count);
    let mut ordered = atoms.to_vec();
    ordered.sort_by(|left, right| {
        let left = f64::from_bits(u64::from_le_bytes(left.atom_id[..8].try_into().unwrap()));
        let right = f64::from_bits(u64::from_le_bytes(right.atom_id[..8].try_into().unwrap()));
        left.total_cmp(&right)
    });
    let mut bloom = BloomFilter::new(1024);
    let mut bytes = Vec::with_capacity(IndexFileHeader::SIZE + atoms.len() * IndexEntry::SIZE);
    let header_bytes = unsafe {
        std::slice::from_raw_parts(
            &header as *const IndexFileHeader as *const u8,
            IndexFileHeader::SIZE,
        )
    };
    bytes.extend_from_slice(header_bytes);
    for atom in &ordered {
        let entry = IndexEntry::new(atom.atom_id, atom.record_offset, atom.body_len, 0);
        let offset = bytes.len();
        bytes.resize(offset + IndexEntry::SIZE, 0);
        entry
            .write_to_bytes(&mut bytes[offset..])
            .map_err(|error| invalid_data(&format!("batch IDX1 encoding failed: {error}")))?;
        bloom.insert(&atom.atom_id);
    }
    let bloom_bytes = bloom.to_bytes();
    bytes.extend_from_slice(&(bloom_bytes.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&bloom_bytes);
    Ok(bytes)
}

fn production_loc1_bytes_many(atoms: &[ProductionAtomStateV1]) -> io::Result<Vec<u8>> {
    let mut ordered = atoms.to_vec();
    ordered.sort_by_key(|atom| atom.atom_id);
    let next_node = ordered
        .iter()
        .map(|atom| atom.node_num)
        .max()
        .map_or(0, |value| value.saturating_add(1));
    let mut bytes = Vec::with_capacity(24 + ordered.len() * 65);
    bytes.extend_from_slice(&0x4c4f4331u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&[8, 0]);
    bytes.extend_from_slice(&next_node.to_le_bytes());
    bytes.extend_from_slice(&(ordered.len() as u64).to_le_bytes());
    for atom in ordered {
        bytes.extend_from_slice(&atom.atom_id);
        bytes.extend_from_slice(&atom.node_num.to_le_bytes());
        bytes.extend_from_slice(&atom.segment_id.to_le_bytes());
        bytes.extend_from_slice(&atom.record_offset.to_le_bytes());
        bytes.extend_from_slice(&(atom.body_len as u32).to_le_bytes());
        bytes.extend_from_slice(&atom.domain_mask.to_le_bytes());
        bytes.push(0);
    }
    Ok(bytes)
}

fn production_idloc_bytes_many(atoms: &[ProductionAtomStateV1]) -> Vec<u8> {
    let mut builder = IdLocBuilder::new(8);
    for atom in atoms {
        builder.add(
            &atom.atom_id,
            atom.segment_id,
            atom.body_len as u32,
            atom.record_offset,
            atom.node_num,
        );
    }
    builder.build_to_vec()
}

fn production_metadata_bytes_many(atoms: &[ProductionAtomStateV1]) -> Vec<u8> {
    let mut ordered = atoms.to_vec();
    ordered.sort_by_key(|atom| atom.atom_id);
    let mut bytes = Vec::with_capacity(16 + ordered.len() * 66);
    bytes.extend_from_slice(&0x4d455431u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&(ordered.len() as u64).to_le_bytes());
    for atom in ordered {
        bytes.extend_from_slice(&atom.atom_id);
        bytes.extend_from_slice(&atom.node_num.to_le_bytes());
        bytes.extend_from_slice(&atom.atom_type.to_u32().to_le_bytes());
        bytes.extend_from_slice(&atom.created_at_ns.to_le_bytes());
        bytes.extend_from_slice(&atom.trust_level.to_le_bytes());
        bytes.extend_from_slice(&atom.domain_mask.to_le_bytes());
        bytes.extend_from_slice(&atom.source_id.to_le_bytes());
    }
    bytes
}

fn production_detached_components(
    request: &ProductionDirectRequestV1,
    envelope: &ProductionDirectEnvelopeV1,
    generation: u64,
    node_num: u64,
) -> io::Result<ProductionDetachedComponentsV1> {
    let atom_id = compute_atom_id_from_payload(&request.body).map_err(|error| {
        invalid_data(&format!(
            "production atom identity validation failed: {error}"
        ))
    })?;
    let body_header = AtomBodyHeader::from_bytes(&request.body).map_err(|error| {
        invalid_data(&format!("production atom body header is invalid: {error}"))
    })?;
    if body_header.atom_type() != Some(request.atom_type) {
        return Err(invalid_data(
            "production request atom type does not match its body",
        ));
    }
    let staged_record = production_record_bytes(atom_id, &request.body, 0)?;
    let history = ProductionHistoryEventV1::create(
        envelope,
        generation,
        atom_id,
        request.atom_type.to_u32(),
        (request.claim_projection.len() / 25) as u64,
        (request.evidence_projection.len() / 54) as u64,
    )?;
    let atom = ProductionAtomStateV1 {
        atom_id,
        atom_type: request.atom_type,
        node_num,
        committed_generation: generation,
        body_len: request.body.len() as u64,
        body_crc32: crc32(&request.body),
        body_hash: production_hash_bytes(&request.body),
        segment_id: 0,
        record_offset: 0,
        record_extent_len: staged_record.len() as u64,
        domain_mask: 0xffff,
        created_at_ns: body_header.created_at_unix_ns,
        trust_level: 5000,
        source_id: 0,
        provenance_hash: production_hash_bytes(&production_zero_provenance_leaf(&atom_id)),
        history_event_id: history.event_id,
        history_leaf: history.leaf_bytes.clone(),
    };
    let segment_index = production_idx1_bytes(atom_id, 0, request.body.len() as u64)?;
    let location_state = production_loc1_bytes(&atom, 8);
    let idloc = production_idloc_bytes(&atom, 8);
    let (lexicon, postings) = production_empty_lexical_pair()?;
    let graph_manifest = production_graph_manifest_bytes(node_num.saturating_add(1))?;
    let metadata = production_metadata_bytes(&atom);
    Ok(ProductionDetachedComponentsV1 {
        staged_record,
        segment_index,
        location_state,
        idloc,
        lexicon,
        postings,
        graph_manifest,
        metadata,
        history: history.line_bytes,
        atom,
    })
}

fn production_txn_root(root: &Path) -> PathBuf {
    root.join(TXN_DIR_NAME)
}

fn production_generation_path(root: &Path, generation: u64) -> PathBuf {
    production_txn_root(root)
        .join(GENERATIONS_DIR_NAME)
        .join(format!("{generation:020}"))
}

fn production_component_descriptor(
    entry: &ProductionRegistryEntry,
    ordinal: u32,
    mode: &str,
    target_path: Option<String>,
    stage_path: Option<String>,
    bytes: &[u8],
    record_count: u64,
) -> io::Result<ProductionComponentDescriptorV1> {
    ProductionComponentDescriptorV1::from_body(ProductionComponentDescriptorBodyV1 {
        schema: "memoryx.production-component-descriptor.v1".to_owned(),
        version: 1,
        registry_key: entry.key.to_owned(),
        registry_order: entry.order,
        ordinal,
        mode: mode.to_owned(),
        target_path,
        stage_path,
        content_codec_id: entry.codec.to_owned(),
        byte_length: bytes.len() as u64,
        byte_hash: production_hash_hex(bytes),
        semantic_hash: production_hash_hex(bytes),
        record_count,
        pair_id: entry.pair_id.map(ToOwned::to_owned),
    })
}

fn production_pair_descriptor(
    pair_id: &str,
    left: &ProductionComponentDescriptorV1,
    right: &ProductionComponentDescriptorV1,
    logical_item_count: u64,
    auxiliary_count: u64,
) -> io::Result<ProductionPairDescriptorV1> {
    let mut semantic = Vec::new();
    semantic.extend_from_slice(pair_id.as_bytes());
    semantic.push(0);
    semantic.extend_from_slice(&parse_hash_hex(&left.semantic_hash, "left semantic hash")?);
    semantic.extend_from_slice(&parse_hash_hex(
        &right.semantic_hash,
        "right semantic hash",
    )?);
    semantic.extend_from_slice(&logical_item_count.to_le_bytes());
    semantic.extend_from_slice(&auxiliary_count.to_le_bytes());
    ProductionPairDescriptorV1::from_body(ProductionPairDescriptorBodyV1 {
        schema: "memoryx.production-pair-descriptor.v1".to_owned(),
        version: 1,
        pair_id: pair_id.to_owned(),
        left_registry_key: left.registry_key.clone(),
        left_ordinal: left.ordinal,
        right_registry_key: right.registry_key.clone(),
        right_ordinal: right.ordinal,
        left_record_count: left.record_count,
        right_record_count: right.record_count,
        logical_item_count,
        auxiliary_count,
        shared_semantic_root: production_hash_hex(&semantic),
    })
}

fn production_write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data("production file has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(windows)]
fn production_replace_file(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = fs::canonicalize(source)?;
    let target_parent = fs::canonicalize(
        target
            .parent()
            .ok_or_else(|| invalid_data("production target has no parent"))?,
    )?;
    let target = target_parent.join(
        target
            .file_name()
            .ok_or_else(|| invalid_data("production target has no file name"))?,
    );
    let mut source_wide = source.as_os_str().encode_wide().collect::<Vec<_>>();
    let mut target_wide = target.as_os_str().encode_wide().collect::<Vec<_>>();
    source_wide.push(0);
    target_wide.push(0);
    // Safety: both vectors are live, NUL-terminated UTF-16 paths. The source
    // is a synced private temporary file and the target parent was resolved
    // beneath the verified physical base before this helper is called.
    let result = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn production_replace_file(source: &Path, target: &Path) -> io::Result<()> {
    fs::rename(source, target)?;
    sync_directory(
        target
            .parent()
            .ok_or_else(|| invalid_data("production target has no parent"))?,
    )
}

fn production_install_replacement(
    root: &Path,
    target_relative: &str,
    bytes: &[u8],
) -> io::Result<()> {
    let target = root.join(target_relative.replace('/', std::path::MAIN_SEPARATOR_STR));
    canonical_relative_path(root, &target)?;
    let parent = target
        .parent()
        .ok_or_else(|| invalid_data("production replacement target has no parent"))?;
    let guard = AncestorGuard::acquire(root, parent)?;
    match fs::symlink_metadata(&target) {
        Ok(metadata) => {
            if is_link_or_reparse(&target, &metadata) || !metadata.is_file() {
                return Err(invalid_data(
                    "production replacement target is not a regular non-reparse file",
                ));
            }
            let current = read_bytes_bounded_under(
                root,
                &target,
                ProductionStorageLimitsV1::frozen().max_component_bytes,
            )?;
            guard.verify()?;
            if current == bytes {
                return Ok(());
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let temporary = parent.join(format!(
        ".{}.production-install.tmp",
        target
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid_data("production target name is not UTF-8"))?
    ));
    if path_entry_exists(&temporary)? {
        let metadata = fs::symlink_metadata(&temporary)?;
        if is_link_or_reparse(&temporary, &metadata) || !metadata.is_file() {
            return Err(invalid_data(
                "production replacement temporary path is not a regular non-reparse file",
            ));
        }
        let staged = read_bytes_bounded_under(
            root,
            &temporary,
            ProductionStorageLimitsV1::frozen().max_component_bytes,
        )?;
        if staged != bytes {
            return Err(invalid_data(
                "production replacement temporary file conflicts with committed bytes",
            ));
        }
    } else {
        production_write_new(&temporary, bytes)?;
    }
    guard.verify()?;
    production_replace_file(&temporary, &target)?;
    guard.verify()
}

fn production_read_component(
    root: &Path,
    generation_dir: &Path,
    descriptor: &ProductionComponentDescriptorV1,
) -> io::Result<Vec<u8>> {
    if !matches!(
        descriptor.schema.as_str(),
        "memoryx.production-component-descriptor.v1" | "memoryx.batch-component-descriptor.v1"
    ) || descriptor.version != 1
        || !matches!(
            descriptor.mode.as_str(),
            "orphan" | "replace" | "anchor_present" | "anchor_absent"
        )
        || descriptor.byte_hash.len() != 64
        || descriptor.semantic_hash.len() != 64
    {
        return Err(invalid_data(
            "production component descriptor is unsupported or corrupt",
        ));
    }
    if descriptor.mode == "anchor_present" && descriptor.registry_key == "cas.segment-data.skf1.v1"
    {
        let relative = descriptor
            .target_path
            .as_deref()
            .ok_or_else(|| invalid_data("production CAS anchor has no target"))?;
        if descriptor.byte_length > ProductionStorageLimitsV1::frozen().max_component_bytes
            || descriptor.byte_hash != descriptor.semantic_hash
        {
            return Err(invalid_data(
                "production CAS anchor descriptor is out of bounds",
            ));
        }
        let (file, identity) = open_verified_regular(
            root,
            &root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR)),
        )?;
        require_single_link(&identity, "production CAS anchor")?;
        if identity.length < descriptor.byte_length {
            return Err(invalid_data("production CAS anchor prefix is truncated"));
        }
        let mut reader = BufReader::with_capacity(STREAM_BUFFER_BYTES, file);
        let mut remaining = descriptor.byte_length;
        let mut buffer = [0u8; STREAM_BUFFER_BYTES];
        let mut hasher = blake3::Hasher::new();
        while remaining > 0 {
            let requested = usize::try_from(remaining.min(STREAM_BUFFER_BYTES as u64)).unwrap();
            reader.read_exact(&mut buffer[..requested])?;
            hasher.update(&buffer[..requested]);
            remaining -= requested as u64;
        }
        if stable_identity(reader.get_ref())? != identity
            || hasher.finalize().to_hex().as_str() != descriptor.byte_hash
        {
            return Err(invalid_data(
                "production CAS anchor prefix changed or disagrees with its descriptor",
            ));
        }
        // CAS anchors are validated as a bounded stream. Callers never need
        // to materialize the historical segment prefix.
        return Ok(Vec::new());
    }
    let bytes = match descriptor.mode.as_str() {
        "orphan" | "replace" => {
            let relative = descriptor
                .stage_path
                .as_deref()
                .ok_or_else(|| invalid_data("production staged component has no stage path"))?;
            read_bytes_bounded_under(
                root,
                &generation_dir.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR)),
                ProductionStorageLimitsV1::frozen().max_component_bytes,
            )?
        }
        "anchor_present" => {
            let relative = descriptor
                .target_path
                .as_deref()
                .ok_or_else(|| invalid_data("production present anchor has no target"))?;
            let bytes = read_bytes_bounded_under(
                root,
                &root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR)),
                ProductionStorageLimitsV1::frozen().max_component_bytes,
            )?;
            if descriptor.registry_key == "cas.segment-data.skf1.v1" {
                let prefix_len = usize::try_from(descriptor.byte_length)
                    .map_err(|_| invalid_data("production CAS anchor length does not fit usize"))?;
                bytes
                    .get(..prefix_len)
                    .ok_or_else(|| invalid_data("production CAS anchor prefix is truncated"))?
                    .to_vec()
            } else {
                bytes
            }
        }
        "anchor_absent" => Vec::new(),
        _ => unreachable!(),
    };
    if bytes.len() as u64 != descriptor.byte_length
        || production_hash_hex(&bytes) != descriptor.byte_hash
    {
        return Err(invalid_data(
            "production component bytes do not match their descriptor",
        ));
    }
    Ok(bytes)
}

fn production_install_generation<Phase>(
    token: &BorrowedOwnerQuiescence<'_, Phase>,
    generation_dir: &Path,
    manifest: &ProductionGenerationManifestV1,
) -> io::Result<()> {
    token.verify()?;
    let root = token.canonical_root();
    for descriptor in &manifest.components {
        let bytes = production_read_component(root, generation_dir, descriptor)?;
        match descriptor.mode.as_str() {
            "replace" => production_install_replacement(
                root,
                descriptor
                    .target_path
                    .as_deref()
                    .ok_or_else(|| invalid_data("production replacement has no target"))?,
                &bytes,
            )?,
            "orphan" if descriptor.registry_key == "cas.staged-record.skf1.v1" => {
                let target = root.join("cas/seg_00000.dat");
                let orphan = manifest
                    .components
                    .iter()
                    .find(|candidate| candidate.registry_key == "cas.orphan-descriptor.v1")
                    .and_then(|candidate| {
                        production_read_component(root, generation_dir, candidate)
                            .ok()
                            .and_then(|bytes| {
                                CasOrphanDescriptorV1::decode(&bytes, "CAS orphan descriptor").ok()
                            })
                    })
                    .ok_or_else(|| {
                        invalid_data("production CAS orphan descriptor is unavailable")
                    })?;
                if stable_identity(&open_no_follow(&target)?)?.length != orphan.segment_post_len {
                    return Err(invalid_data(
                        "production head CAS segment has an unknown suffix",
                    ));
                }
                production_verify_committed_cas_extent(root, &target, &bytes, &orphan)?;
            }
            "orphan" | "anchor_present" | "anchor_absent" => {}
            _ => {
                return Err(invalid_data(
                    "production install encountered an invalid mode",
                ));
            }
        }
    }
    token.verify()
}

fn production_validate_orphan_for_record(
    orphan: &CasOrphanDescriptorV1,
    record: &[u8],
) -> io::Result<()> {
    let expected_post = orphan
        .segment_pre_len
        .checked_add(orphan.record_extent_len)
        .ok_or_else(|| invalid_data("production CAS orphan extent overflows"))?;
    if orphan.schema != "memoryx.cas-orphan-descriptor.v1"
        || orphan.version != 1
        || orphan.record_ordinal != 0
        || orphan.segment_id != 0
        || !orphan.segment_existed
        || orphan.record_offset != orphan.segment_pre_len
        || orphan.record_len != record.len() as u64
        || orphan.record_extent_len != record.len() as u64
        || orphan.segment_post_len != expected_post
        || orphan.record_hash != production_hash_hex(record)
        || orphan.staged_component_key != "cas.staged-record.skf1.v1"
        || orphan.staged_component_hash != production_hash_hex(record)
        || orphan.post_index_component_key != "cas.segment-index.idx1.v1"
    {
        return Err(invalid_data(
            "production CAS orphan descriptor does not bind the staged record",
        ));
    }
    Ok(())
}

fn production_append_precommit_cas(
    root: &Path,
    record: &[u8],
    orphan: &CasOrphanDescriptorV1,
) -> io::Result<()> {
    production_validate_orphan_for_record(orphan, record)?;
    let target = root.join("cas/seg_00000.dat");
    let parent = target
        .parent()
        .ok_or_else(|| invalid_data("production CAS segment has no parent"))?;
    let guard = AncestorGuard::acquire(root, parent)?;
    reject_link_or_reparse(&target)?;
    let mut file = open_append_no_follow(&target)?;
    let identity = stable_identity(&file)?;
    require_single_link(&identity, "production CAS segment")?;
    if !file.metadata()?.is_file() || identity.length != orphan.segment_pre_len {
        return Err(invalid_data(
            "production CAS segment changed before descriptor-bound append",
        ));
    }
    guard.verify()?;
    file.write_all(record)?;
    file.sync_all()?;
    guard.verify()?;
    if file.metadata()?.len() != orphan.segment_post_len {
        return Err(invalid_data(
            "production CAS append did not end at the descriptor boundary",
        ));
    }
    drop(file);
    production_verify_committed_cas_extent(root, &target, record, orphan)
}

fn production_verify_committed_cas_extent(
    root: &Path,
    target: &Path,
    record: &[u8],
    orphan: &CasOrphanDescriptorV1,
) -> io::Result<()> {
    production_validate_orphan_for_record(orphan, record)?;
    let (mut file, identity) = open_verified_regular(root, target)?;
    require_single_link(&identity, "production CAS segment")?;
    if identity.length < orphan.segment_post_len {
        return Err(invalid_data(
            "committed production CAS extent is missing, torn, or has an unknown suffix",
        ));
    }
    file.seek(SeekFrom::Start(orphan.record_offset))?;
    let mut installed = vec![0; record.len()];
    file.read_exact(&mut installed)?;
    if installed != record {
        return Err(invalid_data(
            "installed production CAS extent conflicts with its descriptor",
        ));
    }
    Ok(())
}

fn production_read_exact_committed_body(
    root: &Path,
    atom: &ProductionAtomStateV1,
) -> io::Result<Vec<u8>> {
    let segment = if atom.segment_id == 0 {
        root.join("cas/seg_00000.dat")
    } else {
        root.join(format!("cas/segments/seg_{:08}.skf1", atom.segment_id))
    };
    let (mut file, identity) = open_verified_regular(root, &segment)?;
    require_single_link(&identity, "production CAS segment")?;
    let extent_end = atom
        .record_offset
        .checked_add(atom.record_extent_len)
        .ok_or_else(|| invalid_data("committed production CAS extent overflows"))?;
    if identity.length < extent_end
        || atom.record_extent_len > ProductionStorageLimitsV1::frozen().max_new_cas_extent_bytes
    {
        return Err(invalid_data(
            "committed production CAS extent is not the exact P0-C record",
        ));
    }
    let extent_len = usize::try_from(atom.record_extent_len)
        .map_err(|_| invalid_data("committed production CAS extent does not fit usize"))?;
    file.seek(SeekFrom::Start(atom.record_offset))?;
    let mut record = vec![0; extent_len];
    file.read_exact(&mut record)?;
    if record.len() < RecordHeader::SIZE {
        return Err(invalid_data("committed production CAS record is truncated"));
    }
    let header = RecordHeader::from_bytes(&record[..RecordHeader::SIZE])
        .map_err(|error| invalid_data(&format!("committed CAS header is invalid: {error}")))?;
    let body_len = usize::try_from(header.body_len())
        .map_err(|_| invalid_data("committed production body length does not fit usize"))?;
    let body_end = RecordHeader::SIZE
        .checked_add(body_len)
        .ok_or_else(|| invalid_data("committed production body extent overflows"))?;
    let body = record
        .get(RecordHeader::SIZE..body_end)
        .ok_or_else(|| invalid_data("committed production body is truncated"))?
        .to_vec();
    if !header.is_valid()
        || header.atom_id != atom.atom_id
        || header.seg_id != atom.segment_id
        || header.body_len() != atom.body_len
        || production_hash_bytes(&body) != atom.body_hash
        || crc32(&body) != atom.body_crc32
        || compute_atom_id_from_payload(&body)
            .map_err(|error| invalid_data(&format!("committed atom body is invalid: {error}")))?
            != atom.atom_id
        || production_record_bytes(atom.atom_id, &body, atom.segment_id)? != record
    {
        return Err(invalid_data(
            "committed production CAS record disagrees with durable atom state",
        ));
    }
    Ok(body)
}

fn production_history_from_line(line: &[u8]) -> io::Result<(ProductionHistoryEventWire, Vec<u8>)> {
    if line.last() != Some(&b'\n') || line[..line.len() - 1].contains(&b'\n') {
        return Err(invalid_data(
            "production history component is not one canonical line",
        ));
    }
    let object = &line[..line.len() - 1];
    let wire: ProductionHistoryEventWire = decode_production_json(object, "history event")?;
    let transaction_uuid = validate_production_uuid(&wire.transaction_id)?;
    let event_id = parse_hash_hex(&wire.event_id, "history event_id")?;
    let semantic_hash = parse_hash_hex(&wire.event_semantic_hash, "history semantic hash")?;
    if wire.schema_version != PRODUCTION_HISTORY_SCHEMA
        || wire.event_ordinal != 0
        || wire.generation == 0
        || wire.timestamp_unix_ns == 0
        || wire.operation != "ingest"
        || wire.event_kind != "mutation"
        || wire.outcome != "committed"
        || wire.atom_ids.len() != 1
        || wire.details.result_kind != "created"
    {
        return Err(invalid_data("production history event fields are invalid"));
    }
    let mut leaf = Vec::new();
    leaf.extend_from_slice(PRODUCTION_HISTORY_LEAF_ID.as_bytes());
    leaf.push(0);
    leaf.extend_from_slice(&1u16.to_le_bytes());
    leaf.extend_from_slice(&wire.generation.to_le_bytes());
    leaf.extend_from_slice(&0u32.to_le_bytes());
    leaf.extend_from_slice(&transaction_uuid);
    leaf.extend_from_slice(&event_id);
    leaf.extend_from_slice(&wire.timestamp_unix_ns.to_le_bytes());
    leaf.extend_from_slice(&1u16.to_le_bytes());
    leaf.extend_from_slice(&semantic_hash);
    Ok((wire, leaf))
}

fn production_atom_from_generation(
    root: &Path,
    generation_dir: &Path,
    manifest: &ProductionGenerationManifestV1,
) -> io::Result<(ProductionAtomStateV1, [u8; 32])> {
    let find = |key: &str| {
        manifest
            .components
            .iter()
            .find(|descriptor| descriptor.registry_key == key)
            .ok_or_else(|| invalid_data("production generation is missing a required component"))
    };
    let staged =
        production_read_component(root, generation_dir, find("cas.staged-record.skf1.v1")?)?;
    if staged.len() < RecordHeader::SIZE + 16 {
        return Err(invalid_data("production staged SKF1 record is truncated"));
    }
    let record_header = RecordHeader::from_bytes(&staged[..RecordHeader::SIZE])
        .map_err(|error| invalid_data(&format!("production SKF1 record is invalid: {error}")))?;
    let body_end = RecordHeader::SIZE
        .checked_add(record_header.body_len as usize)
        .ok_or_else(|| invalid_data("production SKF1 body extent overflow"))?;
    let body = staged
        .get(RecordHeader::SIZE..body_end)
        .ok_or_else(|| invalid_data("production SKF1 body is truncated"))?;
    if compute_atom_id_from_payload(body)
        .map_err(|error| invalid_data(&format!("production atom identity failed: {error}")))?
        != record_header.atom_id
    {
        return Err(invalid_data(
            "production SKF1 AtomId does not match its body",
        ));
    }
    let body_header = AtomBodyHeader::from_bytes(body)
        .map_err(|error| invalid_data(&format!("production atom body is invalid: {error}")))?;
    let orphan_bytes =
        production_read_component(root, generation_dir, find("cas.orphan-descriptor.v1")?)?;
    let orphan = CasOrphanDescriptorV1::decode(&orphan_bytes, "CAS orphan descriptor")?;
    let metadata =
        production_read_component(root, generation_dir, find("meta.atom-state.met1.v1")?)?;
    if metadata.len() != 82
        || u32::from_le_bytes(metadata[0..4].try_into().unwrap()) != 0x4d455431
        || u64::from_le_bytes(metadata[8..16].try_into().unwrap()) != 1
    {
        return Err(invalid_data("production MET1 component is invalid"));
    }
    let node_num = u64::from_le_bytes(metadata[48..56].try_into().unwrap());
    let atom_type = AtomType::from_u32(u32::from_le_bytes(metadata[56..60].try_into().unwrap()))
        .ok_or_else(|| invalid_data("production MET1 atom type is invalid"))?;
    let history = production_read_component(root, generation_dir, find("meta.history-once.v1")?)?;
    let (history_wire, history_leaf) = production_history_from_line(&history)?;
    let event_id = parse_hash_hex(&history_wire.event_id, "history event_id")?;
    if metadata[16..48] != record_header.atom_id
        || history_wire.atom_ids[0] != hex_lower(&record_header.atom_id)
        || history_wire.generation != manifest.generation
    {
        return Err(invalid_data(
            "production atom, metadata, and history identities disagree",
        ));
    }
    Ok((
        ProductionAtomStateV1 {
            atom_id: record_header.atom_id,
            atom_type,
            node_num,
            committed_generation: manifest.generation,
            body_len: record_header.body_len,
            body_crc32: crc32(body),
            body_hash: production_hash_bytes(body),
            segment_id: orphan.segment_id,
            record_offset: orphan.record_offset,
            record_extent_len: orphan.record_extent_len,
            domain_mask: u64::from_le_bytes(metadata[70..78].try_into().unwrap()),
            created_at_ns: body_header.created_at_unix_ns,
            trust_level: u16::from_le_bytes(metadata[68..70].try_into().unwrap()),
            source_id: u32::from_le_bytes(metadata[78..82].try_into().unwrap()),
            provenance_hash: production_hash_bytes(&production_zero_provenance_leaf(
                &record_header.atom_id,
            )),
            history_event_id: event_id,
            history_leaf,
        },
        parse_hash_hex(&history_wire.event_semantic_hash, "history semantic hash")?,
    ))
}

fn production_atoms_from_batch_generation(
    root: &Path,
    generation_dir: &Path,
    manifest: &BatchGenerationManifestV1,
) -> io::Result<(Vec<ProductionAtomStateV1>, ProductionBatchHistoryV1)> {
    let history_descriptor = manifest
        .components
        .iter()
        .find(|descriptor| descriptor.registry_key == "meta.history-once.v1")
        .ok_or_else(|| invalid_data("batch history component is missing"))?;
    let history_bytes = production_read_component(root, generation_dir, history_descriptor)?;
    let last_line_start = history_bytes[..history_bytes.len().saturating_sub(1)]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |offset| offset + 1);
    let line = history_bytes
        .get(last_line_start..)
        .ok_or_else(|| invalid_data("batch history line is missing"))?;
    if line.last() != Some(&b'\n') {
        return Err(invalid_data("batch history file has no terminal LF"));
    }
    let wire: ProductionBatchHistoryWire =
        decode_production_json(&line[..line.len() - 1], "batch history event")?;
    if wire.schema_version != "memoryx.history.batch-transaction-once.v1"
        || wire.transaction_id != manifest.transaction_id
        || wire.generation != manifest.generation
        || wire.operation != "batch_ingest"
        || wire.event_ordinal != 0
        || wire.atom_ids.is_empty()
    {
        return Err(invalid_data("batch history event fields are invalid"));
    }
    let transaction_uuid = validate_production_uuid(&wire.transaction_id)?;
    let event_id = parse_hash_hex(&wire.event_id, "batch history event_id")?;
    let semantic_hash = parse_hash_hex(&wire.event_semantic_hash, "batch history semantic hash")?;
    let mut leaf_bytes = Vec::new();
    leaf_bytes.extend_from_slice(PRODUCTION_BATCH_HISTORY_LEAF_ID.as_bytes());
    leaf_bytes.push(0);
    leaf_bytes.extend_from_slice(&1u16.to_le_bytes());
    leaf_bytes.extend_from_slice(&wire.generation.to_le_bytes());
    leaf_bytes.extend_from_slice(&0u32.to_le_bytes());
    leaf_bytes.extend_from_slice(&transaction_uuid);
    leaf_bytes.extend_from_slice(&event_id);
    leaf_bytes.extend_from_slice(&wire.timestamp_unix_ns.to_le_bytes());
    leaf_bytes.extend_from_slice(&2u16.to_le_bytes());
    leaf_bytes.extend_from_slice(&semantic_hash);

    let metadata_descriptor = manifest
        .components
        .iter()
        .find(|descriptor| descriptor.registry_key == "meta.atom-state.met1.v1")
        .ok_or_else(|| invalid_data("batch metadata component is missing"))?;
    let metadata = production_read_component(root, generation_dir, metadata_descriptor)?;
    if metadata.len() < 16
        || u32::from_le_bytes(metadata[..4].try_into().unwrap()) != 0x4d455431
        || (metadata.len() - 16) % 66 != 0
    {
        return Err(invalid_data("batch MET1 component is invalid"));
    }
    let mut metadata_rows = BTreeMap::new();
    for record in metadata[16..].as_chunks::<66>().0 {
        metadata_rows.insert(
            <[u8; 32]>::try_from(&record[..32]).unwrap(),
            (
                u64::from_le_bytes(record[32..40].try_into().unwrap()),
                AtomType::from_u32(u32::from_le_bytes(record[40..44].try_into().unwrap()))
                    .ok_or_else(|| invalid_data("batch MET1 atom type is invalid"))?,
                u64::from_le_bytes(record[44..52].try_into().unwrap()),
                u16::from_le_bytes(record[52..54].try_into().unwrap()),
                u64::from_le_bytes(record[54..62].try_into().unwrap()),
                u32::from_le_bytes(record[62..66].try_into().unwrap()),
            ),
        );
    }
    let mut atoms = Vec::new();
    for descriptor in manifest
        .components
        .iter()
        .filter(|descriptor| descriptor.registry_key == "cas.staged-record.skf1.v1")
    {
        let record = production_read_component(root, generation_dir, descriptor)?;
        let header = RecordHeader::from_bytes(
            record
                .get(..RecordHeader::SIZE)
                .ok_or_else(|| invalid_data("batch staged record is truncated"))?,
        )
        .map_err(|error| invalid_data(&format!("batch staged record is invalid: {error}")))?;
        let body_end = RecordHeader::SIZE
            .checked_add(header.body_len as usize)
            .ok_or_else(|| invalid_data("batch staged body extent overflow"))?;
        let body = record
            .get(RecordHeader::SIZE..body_end)
            .ok_or_else(|| invalid_data("batch staged body is truncated"))?;
        let append_component = manifest
            .components
            .iter()
            .find(|candidate| {
                candidate.registry_key == "cas.orphan-descriptor.v1"
                    && candidate.ordinal == descriptor.ordinal
            })
            .ok_or_else(|| invalid_data("batch append descriptor component is missing"))?;
        let append_bytes = production_read_component(root, generation_dir, append_component)?;
        let append = BatchCasAppendDescriptorV1::decode(&append_bytes, "batch append descriptor")?;
        let (node, atom_type, created_at_ns, trust, domain, source) = metadata_rows
            .get(&header.atom_id)
            .copied()
            .ok_or_else(|| invalid_data("batch staged atom has no MET1 row"))?;
        if compute_atom_id_from_payload(body)
            .map_err(|error| invalid_data(&format!("batch staged body is invalid: {error}")))?
            != header.atom_id
            || append.atom_id != hex_lower(&header.atom_id)
            || append.record_offset != append.pre_segment_length
            || append.record_extent_length != record.len() as u64
            || append.staged_record_hash != production_hash_hex(&record)
        {
            return Err(invalid_data("batch staged atom identities disagree"));
        }
        atoms.push(ProductionAtomStateV1 {
            atom_id: header.atom_id,
            atom_type,
            node_num: node,
            committed_generation: manifest.generation,
            body_len: header.body_len,
            body_crc32: crc32(body),
            body_hash: production_hash_bytes(body),
            segment_id: 0,
            record_offset: append.record_offset,
            record_extent_len: append.record_extent_length,
            domain_mask: domain,
            created_at_ns,
            trust_level: trust,
            source_id: source,
            provenance_hash: production_hash_bytes(&production_zero_provenance_leaf(
                &header.atom_id,
            )),
            history_event_id: event_id,
            history_leaf: Vec::new(),
        });
    }
    atoms.sort_by_key(|atom| atom.node_num);
    if atoms.len() != wire.atom_ids.len()
        || atoms
            .iter()
            .map(|atom| hex_lower(&atom.atom_id))
            .collect::<Vec<_>>()
            != wire.atom_ids
        || hex_lower(&semantic_hash) != manifest.history_event_hash
    {
        return Err(invalid_data("batch history membership is invalid"));
    }
    Ok((
        atoms,
        ProductionBatchHistoryV1 {
            event_id,
            semantic_hash,
            line_bytes: line.to_vec(),
            leaf_bytes,
        },
    ))
}

fn production_generation_directories(root: &Path) -> io::Result<Vec<(u64, PathBuf)>> {
    let generations = production_txn_root(root).join(GENERATIONS_DIR_NAME);
    let mut result = Vec::new();
    for entry in fs::read_dir(&generations)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| invalid_data("production generation name is not UTF-8"))?
            .to_owned();
        if name.starts_with(".pending-") {
            continue;
        }
        if name.len() != 20 || !name.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid_data(
                "production generations directory contains an unknown entry",
            ));
        }
        let generation = name
            .parse::<u64>()
            .map_err(|_| invalid_data("production generation number is invalid"))?;
        if generation == 0
            || generation > PRODUCTION_MAX_GENERATIONS
            || !entry.file_type()?.is_dir()
        {
            return Err(invalid_data(
                "production generation entry is outside its limits",
            ));
        }
        result.push((generation, entry.path()));
    }
    result.sort_by_key(|(generation, _)| *generation);
    for (index, (generation, _)) in result.iter().enumerate() {
        if *generation != index as u64 + 1 {
            return Err(invalid_data(
                "production generation chain is not contiguous",
            ));
        }
    }
    Ok(result)
}

fn production_rollback_pending_update_cas(root: &Path, pending: &Path) -> io::Result<bool> {
    let staged_path = pending.join(production_stage_path(20, 0));
    if !path_entry_exists(&staged_path)? {
        return Ok(false);
    }
    let record = read_bytes_bounded_under(
        root,
        &staged_path,
        ProductionStorageLimitsV1::frozen().max_staged_record_bytes,
    )?;
    if record.len() < RecordHeader::SIZE + 16 {
        return Ok(false);
    }
    let header = match RecordHeader::from_bytes(&record[..RecordHeader::SIZE]) {
        Ok(header) if header.is_valid() && header.seg_id != 0 => header,
        _ => return Ok(false),
    };
    let target = root.join(format!("cas/segments/seg_{:08}.skf1", header.seg_id));
    match fs::symlink_metadata(&target) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Ok(metadata) => {
            if is_link_or_reparse(&target, &metadata) || !metadata.is_file() {
                return Err(invalid_data(
                    "pending update CAS target is not a regular file",
                ));
            }
        }
        Err(error) => return Err(error),
    }
    let installed = read_bytes_bounded_under(
        root,
        &target,
        ProductionStorageLimitsV1::frozen().max_staged_record_bytes,
    )?;
    if installed != record {
        return Err(invalid_data(
            "pending update CAS target conflicts with its staged record",
        ));
    }
    let parent = target
        .parent()
        .ok_or_else(|| invalid_data("pending update CAS target has no parent"))?;
    let guard = AncestorGuard::acquire(root, parent)?;
    guard.verify()?;
    fs::remove_file(&target)?;
    sync_directory(parent)?;
    guard.verify()?;
    Ok(true)
}

fn production_rollback_pending_cas(
    root: &Path,
    transaction_id: &str,
    pending: &Path,
) -> io::Result<()> {
    if production_rollback_pending_update_cas(root, pending)? {
        return Ok(());
    }
    let batch_cas = pending.join("components").join("cas");
    if path_entry_exists(&batch_cas)? {
        let mut descriptors = Vec::new();
        let mut records = BTreeMap::new();
        for entry in fs::read_dir(&batch_cas)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .to_str()
                .ok_or_else(|| invalid_data("batch pending CAS name is not UTF-8"))?
                .to_owned();
            if entry.file_type()?.is_symlink() || !entry.file_type()?.is_file() {
                return Err(invalid_data("batch pending CAS tree contains a non-file"));
            }
            if let Some(ordinal) = name
                .strip_prefix("staged_")
                .and_then(|value| value.strip_suffix(".skf1"))
                .and_then(|value| value.parse::<u32>().ok())
            {
                records.insert(
                    ordinal,
                    read_bytes_bounded_under(root, &entry.path(), 67_108_944)?,
                );
            } else if name.starts_with("orphan_") && name.ends_with(".json") {
                let bytes =
                    read_bytes_bounded_under(root, &entry.path(), MAX_CONTROL_RECORD_BYTES)?;
                descriptors.push(BatchCasAppendDescriptorV1::decode(
                    &bytes,
                    "pending batch append descriptor",
                )?);
            } else if name != "seg_00000.idx" {
                return Err(invalid_data("batch pending CAS tree has an unknown file"));
            }
        }
        if !descriptors.is_empty() || !records.is_empty() {
            descriptors.sort_by_key(|descriptor| descriptor.ordinal);
            if descriptors.len() != records.len()
                || descriptors.iter().enumerate().any(|(ordinal, descriptor)| {
                    descriptor.ordinal != ordinal as u32
                        || records.get(&(ordinal as u32)).is_none_or(|record| {
                            descriptor.staged_record_hash != production_hash_hex(record)
                        })
                })
            {
                return Err(invalid_data("pending batch CAS inventory is incomplete"));
            }
            let segment = root.join("cas/seg_00000.dat");
            let parent = segment
                .parent()
                .ok_or_else(|| invalid_data("batch CAS segment has no parent"))?;
            let guard = AncestorGuard::acquire(root, parent)?;
            let mut file = open_read_write_no_follow(&segment)?;
            require_single_link(&stable_identity(&file)?, "pending batch CAS segment")?;
            let pre = descriptors[0].pre_segment_length;
            let post = descriptors
                .last()
                .map(|descriptor| descriptor.post_segment_length)
                .ok_or_else(|| invalid_data("pending batch CAS descriptor is missing"))?;
            let current = file.metadata()?.len();
            if current < pre || current > post {
                return Err(invalid_data(
                    "pending batch CAS suffix has an unknown length",
                ));
            }
            let mut expected = Vec::new();
            for ordinal in 0..records.len() as u32 {
                expected.extend_from_slice(
                    records
                        .get(&ordinal)
                        .ok_or_else(|| invalid_data("pending batch CAS ordinal is missing"))?,
                );
            }
            let installed_len = (current - pre) as usize;
            file.seek(SeekFrom::Start(pre))?;
            let mut installed = vec![0; installed_len];
            file.read_exact(&mut installed)?;
            if installed != expected[..installed_len] {
                return Err(invalid_data(
                    "pending batch CAS suffix conflicts with staging",
                ));
            }
            guard.verify()?;
            file.set_len(pre)?;
            file.sync_all()?;
            guard.verify()?;
            return Ok(());
        }
    }
    let record_entry = production_registry_entry("cas.staged-record.skf1.v1")?;
    let orphan_entry = production_registry_entry("cas.orphan-descriptor.v1")?;
    let record_path = pending.join(production_stage_path(record_entry.order, 0));
    let orphan_path = pending.join(production_stage_path(orphan_entry.order, 0));
    let record_exists = path_entry_exists(&record_path)?;
    let orphan_exists = path_entry_exists(&orphan_path)?;
    if !record_exists && !orphan_exists {
        return Ok(());
    }

    let segment = root.join("cas/seg_00000.dat");
    let segment_len = fs::metadata(&segment)?.len();
    if !record_exists || !orphan_exists {
        if segment_len == 0 {
            return Ok(());
        }
        return Err(invalid_data(
            "incomplete pending CAS descriptor cannot justify rollback of a nonempty segment",
        ));
    }

    let record = read_bytes_bounded_under(
        root,
        &record_path,
        ProductionStorageLimitsV1::frozen().max_staged_record_bytes,
    )?;
    let orphan_bytes = read_bytes_bounded_under(root, &orphan_path, MAX_CONTROL_RECORD_BYTES)?;
    let orphan = CasOrphanDescriptorV1::decode(&orphan_bytes, "pending CAS orphan descriptor")?;
    production_validate_orphan_for_record(&orphan, &record)?;
    if orphan.transaction_id != transaction_id {
        return Err(invalid_data(
            "pending CAS orphan transaction does not match its namespace",
        ));
    }

    let parent = segment
        .parent()
        .ok_or_else(|| invalid_data("production CAS segment has no parent"))?;
    let guard = AncestorGuard::acquire(root, parent)?;
    reject_link_or_reparse(&segment)?;
    let mut file = open_read_write_no_follow(&segment)?;
    let identity = stable_identity(&file)?;
    require_single_link(&identity, "production CAS segment")?;
    if !file.metadata()?.is_file()
        || identity.length < orphan.segment_pre_len
        || identity.length > orphan.segment_post_len
    {
        return Err(invalid_data(
            "pending CAS extent cannot be rolled back without discarding unknown bytes",
        ));
    }
    let installed_len = identity.length - orphan.segment_pre_len;
    if installed_len > 0 {
        let installed_len = usize::try_from(installed_len)
            .map_err(|_| invalid_data("pending CAS extent length does not fit usize"))?;
        file.seek(SeekFrom::Start(orphan.segment_pre_len))?;
        let mut installed = vec![0; installed_len];
        file.read_exact(&mut installed)?;
        if installed != record[..installed_len] {
            return Err(invalid_data(
                "pending CAS extent conflicts with its staged descriptor",
            ));
        }
        guard.verify()?;
        file.set_len(orphan.segment_pre_len)?;
        file.sync_all()?;
        guard.verify()?;
    }
    Ok(())
}

fn production_cleanup_pending<Phase>(token: &BorrowedOwnerQuiescence<'_, Phase>) -> io::Result<()> {
    token.verify()?;
    let generations = production_txn_root(token.canonical_root()).join(GENERATIONS_DIR_NAME);
    let mut pending = Vec::new();
    for entry in fs::read_dir(&generations)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| invalid_data("production pending name is not UTF-8"))?
            .to_owned();
        if !name.starts_with(".pending-") {
            continue;
        }
        validate_production_uuid(
            name.strip_prefix(".pending-")
                .ok_or_else(|| invalid_data("production pending prefix is invalid"))?,
        )?;
        if !entry.file_type()?.is_dir() || entry.file_type()?.is_symlink() {
            return Err(invalid_data(
                "production pending entry is not a real directory",
            ));
        }
        let path = entry.path();
        let mut files = Vec::new();
        let mut components = None;
        for child in fs::read_dir(&path)? {
            let child = child?;
            let child_name = child.file_name();
            let child_name = child_name
                .to_str()
                .ok_or_else(|| invalid_data("production pending child is not UTF-8"))?;
            let kind = child.file_type()?;
            if kind.is_symlink() {
                return Err(invalid_data("production pending tree contains a symlink"));
            }
            match child_name {
                PREPARE_FILE_NAME | COMMIT_FILE_NAME if kind.is_file() => files.push(child.path()),
                COMPONENTS_DIR_NAME if kind.is_dir() => components = Some(child.path()),
                _ => {
                    return Err(invalid_data(
                        "production pending tree contains an unknown entry",
                    ));
                }
            }
        }
        let mut component_files = Vec::new();
        let mut component_dirs = Vec::new();
        if components.is_none() && !files.is_empty() {
            return Err(invalid_data(
                "production pending control record has no components directory",
            ));
        }
        let mut stack = components.iter().cloned().collect::<Vec<_>>();
        while let Some(directory) = stack.pop() {
            for component in fs::read_dir(&directory)? {
                let component = component?;
                let kind = component.file_type()?;
                if kind.is_symlink() {
                    return Err(invalid_data(
                        "production pending component tree contains a link",
                    ));
                }
                if kind.is_dir() {
                    let relative = component
                        .path()
                        .strip_prefix(components.as_ref().expect("component walk has a root"))
                        .map_err(|_| invalid_data("pending component directory escaped"))?
                        .to_string_lossy()
                        .replace('\\', "/");
                    if !matches!(relative.as_str(), "cas" | "index" | "graph" | "meta") {
                        return Err(invalid_data(
                            "production pending component directory is unknown",
                        ));
                    }
                    component_dirs.push(component.path());
                    stack.push(component.path());
                } else if kind.is_file() {
                    let relative = component
                        .path()
                        .strip_prefix(components.as_ref().expect("component walk has a root"))
                        .map_err(|_| invalid_data("pending component file escaped"))?
                        .to_string_lossy()
                        .replace('\\', "/");
                    let direct = relative.len() == 16
                        && relative.as_bytes()[3] == b'_'
                        && relative.ends_with(".bin")
                        && relative[..3].bytes().all(|byte| byte.is_ascii_digit())
                        && relative[4..12].bytes().all(|byte| byte.is_ascii_digit());
                    let batch = matches!(
                        relative.as_str(),
                        "cas/seg_00000.idx"
                            | "index/location_state.bin"
                            | "index/idloc.mmap"
                            | "index/terms.lex"
                            | "index/terms.post"
                            | "graph/graph.manifest"
                            | "meta/meta_state.bin"
                            | "meta/history.log"
                    ) || relative
                        .strip_prefix("cas/staged_")
                        .and_then(|value| value.strip_suffix(".skf1"))
                        .is_some_and(|value| {
                            value.len() == 5 && value.bytes().all(|byte| byte.is_ascii_digit())
                        })
                        || relative
                            .strip_prefix("cas/orphan_")
                            .and_then(|value| value.strip_suffix(".json"))
                            .is_some_and(|value| {
                                value.len() == 5 && value.bytes().all(|byte| byte.is_ascii_digit())
                            });
                    if !direct && !batch {
                        return Err(invalid_data("production pending component file is unknown"));
                    }
                    component_files.push(component.path());
                } else {
                    return Err(invalid_data("production pending component is not a file"));
                }
            }
        }
        if component_files.len() > ProductionStorageLimitsV1::frozen().max_component_count as usize
        {
            return Err(invalid_data(
                "production pending component count exceeds its limit",
            ));
        }
        for file_path in files.iter().chain(component_files.iter()) {
            let file = File::open(file_path)?;
            require_single_link(&stable_identity(&file)?, "production pending file")?;
        }
        production_rollback_pending_cas(
            token.canonical_root(),
            name.strip_prefix(".pending-")
                .ok_or_else(|| invalid_data("production pending prefix is invalid"))?,
            &path,
        )?;
        pending.push((path, components, component_dirs, files, component_files));
    }
    for (path, components, mut component_dirs, files, component_files) in pending {
        token.verify()?;
        for file in component_files {
            fs::remove_file(file)?;
        }
        component_dirs.sort_by_key(|directory| std::cmp::Reverse(directory.components().count()));
        for directory in component_dirs {
            fs::remove_dir(directory)?;
        }
        if let Some(components) = components {
            fs::remove_dir(components)?;
        }
        for file in files {
            fs::remove_file(file)?;
        }
        fs::remove_dir(path)?;
    }
    token.verify()
}

fn production_validate_manifest(
    root: &Path,
    generation_dir: &Path,
    manifest: &ProductionGenerationManifestV1,
    expected_generation: u64,
    parent: &ProductionCommittedHead,
) -> io::Result<()> {
    if manifest.schema != "memoryx.production-generation-manifest.v1"
        || manifest.version != 1
        || manifest.format_version != 2
        || manifest.generation != expected_generation
        || manifest.parent_commit_hash != hex_lower(&parent.commit_hash)
        || manifest.operation != "direct_ingest"
        || manifest.codec_id != PRODUCTION_CODEC_ID
        || manifest.registry_id != PRODUCTION_REGISTRY_ID
        || manifest.digest_id != PRODUCTION_DIGEST_ID
        || manifest.limits_id != PRODUCTION_LIMITS_ID
        || manifest.history_event_count != 1
        || manifest.components.len() != PRODUCTION_DIRECT_REGISTRY.len()
        || manifest.components.len()
            > ProductionStorageLimitsV1::frozen().max_component_count as usize
        || manifest.pairs.len() > ProductionStorageLimitsV1::frozen().max_pair_count as usize
    {
        return Err(invalid_data(
            "production generation manifest fixed fields are invalid",
        ));
    }
    validate_production_uuid(&manifest.transaction_id)?;
    let mut previous = None;
    for (descriptor, registry) in manifest
        .components
        .iter()
        .zip(PRODUCTION_DIRECT_REGISTRY.iter())
    {
        let key = (
            descriptor.registry_order,
            descriptor.target_path.clone(),
            descriptor.ordinal,
        );
        if previous.as_ref().is_some_and(|value| value >= &key)
            || descriptor.registry_key != registry.key
            || descriptor.registry_order != registry.order
            || descriptor.ordinal != 0
            || descriptor.content_codec_id != registry.codec
            || descriptor.target_path.as_deref() != registry.target
            || descriptor.pair_id.as_deref() != registry.pair_id
            || ((150..=220).contains(&registry.order)
                && !matches!(descriptor.mode.as_str(), "anchor_present" | "anchor_absent"))
            || (!(150..=220).contains(&registry.order) && descriptor.mode != registry.mode)
        {
            return Err(invalid_data(
                "production component descriptors do not match the compiled registry",
            ));
        }
        previous = Some(key);
        production_read_component(root, generation_dir, descriptor)?;
    }
    if production_component_root(manifest.generation, &manifest.components, &manifest.pairs)?
        != parse_hash_hex(&manifest.component_root_hash, "component root")?
    {
        return Err(invalid_data(
            "production component root does not match manifest",
        ));
    }
    let prepare_bytes = read_bytes_bounded_under(
        root,
        &generation_dir.join(PREPARE_FILE_NAME),
        MAX_CONTROL_RECORD_BYTES,
    )?;
    let prepare = ProductionPrepareRecordV1::decode(&prepare_bytes, "prepare.bin")?;
    if production_hash_hex(&prepare_bytes) != manifest.prepare_hash
        || prepare.generation != manifest.generation
        || prepare.parent_commit_hash != manifest.parent_commit_hash
        || prepare.transaction_id != manifest.transaction_id
        || prepare.semantic_time_unix_ns != manifest.semantic_time_unix_ns
        || prepare.base_binding_hash != manifest.base_binding_hash
        || prepare.envelope_hash != manifest.envelope_hash
        || prepare.intent_hash != manifest.intent_hash
        || prepare.operation != manifest.operation
    {
        return Err(invalid_data(
            "production prepare does not bind the committed manifest",
        ));
    }
    let orphan_descriptor = manifest
        .components
        .iter()
        .find(|descriptor| descriptor.registry_key == "cas.orphan-descriptor.v1")
        .ok_or_else(|| invalid_data("production orphan descriptor component is missing"))?;
    let orphan_bytes = production_read_component(root, generation_dir, orphan_descriptor)?;
    let orphan = CasOrphanDescriptorV1::decode(&orphan_bytes, "CAS orphan descriptor")?;
    if production_orphan_inventory_digest(&[orphan])?
        != parse_hash_hex(&manifest.orphan_inventory_digest, "orphan inventory digest")?
    {
        return Err(invalid_data(
            "production orphan inventory digest does not match",
        ));
    }
    let component = |key: &str| {
        manifest
            .components
            .iter()
            .find(|descriptor| descriptor.registry_key == key)
            .ok_or_else(|| invalid_data("production pair component is missing"))
    };
    let expected_pairs = vec![
        production_pair_descriptor(
            "memoryx.location-idloc-pair.v1",
            component("index.location-state.loc1.v1")?,
            component("index.idloc.idl1.v1")?,
            1,
            0,
        )?,
        production_pair_descriptor(
            "memoryx.lexical-postings-pair.v1",
            component("index.lexicon.lex1.v1")?,
            component("index.postings.pst1.v1")?,
            0,
            0,
        )?,
    ];
    if manifest.pairs != expected_pairs {
        return Err(invalid_data(
            "production pair descriptors are not canonical",
        ));
    }
    Ok(())
}

fn production_install_batch_generation<Phase>(
    token: &BorrowedOwnerQuiescence<'_, Phase>,
    generation_dir: &Path,
    manifest: &BatchGenerationManifestV1,
) -> io::Result<()> {
    token.verify()?;
    let root = token.canonical_root();
    for descriptor in &manifest.components {
        if descriptor.mode == "replace"
            && (descriptor.byte_length > PRODUCTION_BATCH_MAX_INSTALL_SCRATCH_BYTES
                || fs2::available_space(root)?
                    < descriptor
                        .byte_length
                        .checked_add(PRODUCTION_BATCH_MINIMUM_FREE_RESERVE_BYTES)
                        .ok_or_else(|| invalid_data("batch install space requirement overflow"))?)
        {
            return Err(invalid_data("batch install scratch preflight failed"));
        }
        let bytes = production_read_component(root, generation_dir, descriptor)?;
        if descriptor.mode == "replace" {
            production_install_replacement(
                root,
                descriptor
                    .target_path
                    .as_deref()
                    .ok_or_else(|| invalid_data("batch replacement has no target"))?,
                &bytes,
            )?;
        }
    }
    let segment = root.join("cas/seg_00000.dat");
    let identity = stable_identity(&open_no_follow(&segment)?)?;
    require_single_link(&identity, "batch committed CAS segment")?;
    let mut append_descriptors = manifest
        .components
        .iter()
        .filter(|descriptor| descriptor.registry_key == "cas.orphan-descriptor.v1")
        .map(|descriptor| {
            production_read_component(root, generation_dir, descriptor).and_then(|bytes| {
                BatchCasAppendDescriptorV1::decode(&bytes, "batch CAS append descriptor")
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    append_descriptors.sort_by_key(|descriptor| descriptor.ordinal);
    if append_descriptors.is_empty()
        || append_descriptors
            .last()
            .is_none_or(|descriptor| descriptor.post_segment_length != identity.length)
    {
        return Err(invalid_data(
            "batch committed CAS segment length is invalid",
        ));
    }
    let (mut file, _) = open_verified_regular(root, &segment)?;
    for descriptor in &append_descriptors {
        let staged_descriptor = manifest
            .components
            .iter()
            .find(|component| {
                component.registry_key == "cas.staged-record.skf1.v1"
                    && component.ordinal == descriptor.ordinal
            })
            .ok_or_else(|| invalid_data("batch staged record descriptor is missing"))?;
        let staged = production_read_component(root, generation_dir, staged_descriptor)?;
        file.seek(SeekFrom::Start(descriptor.record_offset))?;
        let mut installed = vec![0; staged.len()];
        file.read_exact(&mut installed)?;
        if installed != staged || production_hash_hex(&staged) != descriptor.staged_record_hash {
            return Err(invalid_data("batch committed CAS extent is not exact"));
        }
    }
    token.verify()
}

fn production_validate_batch_manifest(
    root: &Path,
    generation_dir: &Path,
    manifest: &BatchGenerationManifestV1,
    expected_generation: u64,
    parent: &ProductionCommittedHead,
) -> io::Result<()> {
    if manifest.schema != "memoryx.batch-generation-manifest.v1"
        || manifest.version != 1
        || manifest.format_version != 2
        || manifest.generation != expected_generation
        || manifest.parent_commit_hash != hex_lower(&parent.commit_hash)
        || manifest.operation != "batch_ingest"
        || manifest.codec_id != PRODUCTION_CODEC_ID
        || manifest.registry_id != PRODUCTION_REGISTRY_ID
        || manifest.operation_registry_id != PRODUCTION_BATCH_OPERATION_REGISTRY_ID
        || manifest.digest_id != PRODUCTION_DIGEST_ID
        || manifest.limits_id != PRODUCTION_BATCH_LIMITS_ID
        || manifest.decision_hashes.is_empty()
        || manifest.decision_hashes.len() > PRODUCTION_BATCH_MAX_ITEMS
        || manifest.components.len() > 49
        || manifest.pairs.len() > 18
    {
        return Err(invalid_data(
            "batch generation manifest fixed fields are invalid",
        ));
    }
    validate_production_uuid(&manifest.transaction_id)?;
    let mut expected_files =
        BTreeSet::from([PREPARE_FILE_NAME.to_owned(), COMMIT_FILE_NAME.to_owned()]);
    let mut expected_dirs = BTreeSet::from([COMPONENTS_DIR_NAME.to_owned()]);
    for descriptor in &manifest.components {
        if let Some(stage) = &descriptor.stage_path {
            expected_files.insert(stage.clone());
            let mut path = Path::new(stage).parent();
            while let Some(parent_path) = path {
                let normalized = parent_path.to_string_lossy().replace('\\', "/");
                if normalized.is_empty() {
                    break;
                }
                expected_dirs.insert(normalized);
                path = parent_path.parent();
            }
        }
    }
    let mut actual_files = BTreeSet::new();
    let mut actual_dirs = BTreeSet::new();
    let mut stack = vec![generation_dir.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err(invalid_data("batch generation tree contains a link"));
            }
            let relative = entry
                .path()
                .strip_prefix(generation_dir)
                .map_err(|_| invalid_data("batch generation entry escaped"))?
                .to_string_lossy()
                .replace('\\', "/");
            if kind.is_dir() {
                actual_dirs.insert(relative);
                stack.push(entry.path());
            } else if kind.is_file() {
                require_single_link(
                    &stable_identity(&File::open(entry.path())?)?,
                    "batch generation file",
                )?;
                actual_files.insert(relative);
            } else {
                return Err(invalid_data(
                    "batch generation contains an unsupported entry",
                ));
            }
        }
    }
    if actual_files != expected_files || actual_dirs != expected_dirs {
        return Err(invalid_data("batch generation tree is not exact"));
    }
    let mut previous = None;
    let mut staged_count = 0usize;
    let mut orphan_count = 0usize;
    for descriptor in &manifest.components {
        let key = (descriptor.registry_order, descriptor.ordinal);
        if previous.is_some_and(|value| value >= key)
            || descriptor.schema != "memoryx.batch-component-descriptor.v1"
        {
            return Err(invalid_data("batch component order or schema is invalid"));
        }
        previous = Some(key);
        staged_count += usize::from(descriptor.registry_key == "cas.staged-record.skf1.v1");
        orphan_count += usize::from(descriptor.registry_key == "cas.orphan-descriptor.v1");
        production_read_component(root, generation_dir, descriptor)?;
    }
    if staged_count == 0
        || staged_count != orphan_count
        || manifest.components.len() != 17 + 2 * staged_count
        || manifest.pairs.len() != staged_count + 2
        || production_component_root(manifest.generation, &manifest.components, &manifest.pairs)?
            != parse_hash_hex(&manifest.component_root_hash, "batch component root")?
    {
        return Err(invalid_data(
            "batch component cardinality or root is invalid",
        ));
    }
    let prepare_bytes = read_bytes_bounded_under(
        root,
        &generation_dir.join(PREPARE_FILE_NAME),
        MAX_CONTROL_RECORD_BYTES,
    )?;
    let prepare = BatchPrepareV1::decode(&prepare_bytes, "batch prepare.bin")?;
    if manifest.prepare_hash != production_hash_hex(&prepare_bytes)
        || prepare.generation != manifest.generation
        || prepare.parent_commit_hash != manifest.parent_commit_hash
        || prepare.transaction_id != manifest.transaction_id
        || prepare.semantic_time_unix_ns != manifest.semantic_time_unix_ns
        || prepare.base_binding_hash != manifest.base_binding_hash
        || prepare.envelope_hash != manifest.envelope_hash
        || prepare.intent_hash != manifest.intent_hash
        || prepare.preflight_hash.len() != 64
        || prepare.decision_hashes != manifest.decision_hashes
        || prepare.components != manifest.components
        || prepare.pairs != manifest.pairs
        || prepare.post_atom_count != manifest.post_atom_count
    {
        return Err(invalid_data("batch prepare does not bind its manifest"));
    }
    let mut orphans = Vec::new();
    for descriptor in manifest
        .components
        .iter()
        .filter(|descriptor| descriptor.registry_key == "cas.orphan-descriptor.v1")
    {
        let bytes = production_read_component(root, generation_dir, descriptor)?;
        orphans.push(BatchCasAppendDescriptorV1::decode(
            &bytes,
            "batch CAS append descriptor",
        )?);
    }
    if production_batch_orphan_digest(&orphans)?
        != parse_hash_hex(&manifest.orphan_inventory_digest, "batch orphan digest")?
    {
        return Err(invalid_data("batch orphan inventory digest is invalid"));
    }
    Ok(())
}

fn production_validate_update_manifest(
    root: &Path,
    generation_dir: &Path,
    manifest: &UpdateGenerationManifestV1,
    expected_generation: u64,
    parent: &ProductionCommittedHead,
) -> io::Result<()> {
    let fixed_hashes = [
        &manifest.parent_commit_hash,
        &manifest.prepare_hash,
        &manifest.base_binding_hash,
        &manifest.envelope_hash,
        &manifest.intent_hash,
        &manifest.old_atom_id,
        &manifest.successor_atom_id,
        &manifest.successor_body_hash,
        &manifest.claim_projection_hash,
        &manifest.api_evidence_projection_hash,
        &manifest.supersedes_relation_id,
        &manifest.old_provenance_hash,
        &manifest.successor_provenance_hash,
        &manifest.successor_source_attachment_hash,
        &manifest.history_event_id,
        &manifest.history_semantic_hash,
        &manifest.component_root_hash,
        &manifest.logical_state_digest,
    ];
    if manifest.schema != PRODUCTION_UPDATE_MANIFEST_SCHEMA
        || manifest.version != 1
        || manifest.format_version != 2
        || manifest.generation != expected_generation
        || manifest.parent_commit_hash != hex_lower(&parent.commit_hash)
        || manifest.operation != "update_atom"
        || manifest.operation_registry_id != PRODUCTION_UPDATE_OPERATION_REGISTRY_ID
        || manifest.limits_id != PRODUCTION_UPDATE_LIMITS_ID
        || manifest.old_atom_id == manifest.successor_atom_id
        || manifest.old_node == manifest.successor_node
        || manifest.successor_atom_type == 0
        || manifest.history_event_count == 0
        || manifest.relation_count == 0
        || manifest.post_atom_count < 2
        || manifest.graph_leaf_count != manifest.relation_count
        || manifest.components.len() != PRODUCTION_UPDATE_COMPONENT_COUNT
        || fixed_hashes.iter().any(|value| !is_hash(value))
    {
        return Err(invalid_data(
            "update generation manifest fields are invalid",
        ));
    }
    validate_production_uuid(&manifest.transaction_id)?;
    let expected = [
        (20, "cas.successor-append.v1", "append"),
        (30, "index.idloc-replace.v1", "replace"),
        (40, "index.locate-replace.v1", "replace"),
        (50, "meta.successor-provenance.v1", "replace"),
        (60, "meta.update-history-once.v1", "replace"),
        (70, "meta.current-view.v1", "replace"),
        (80, "graph.delta-supersedes.v1", "create"),
        (90, "graph.manifest-grm1.v0101", "replace"),
    ];
    for (descriptor, (order, key, mode)) in manifest.components.iter().zip(expected) {
        if descriptor.registry_order != order
            || descriptor.registry_key != key
            || descriptor.mode != mode
            || descriptor.byte_length > PRODUCTION_UPDATE_MAX_COMPONENT_BYTES
        {
            return Err(invalid_data(
                "update component registry projection is invalid",
            ));
        }
        production_read_update_component(root, generation_dir, descriptor)?;
    }
    if production_update_component_root(&manifest.components)?
        != parse_hash_hex(&manifest.component_root_hash, "update component root")?
    {
        return Err(invalid_data("update component root does not match"));
    }
    let prepare_bytes = read_bytes_bounded_under(
        root,
        &generation_dir.join(PREPARE_FILE_NAME),
        MAX_CONTROL_RECORD_BYTES,
    )?;
    let prepare = UpdatePrepareV1::decode(&prepare_bytes, "update prepare.bin")?;
    if manifest.prepare_hash != production_hash_hex(&prepare_bytes)
        || prepare.schema != PRODUCTION_UPDATE_PREPARE_SCHEMA
        || prepare.version != 1
        || prepare.format_version != 2
        || prepare.generation != manifest.generation
        || prepare.parent_commit_hash != manifest.parent_commit_hash
        || prepare.transaction_id != manifest.transaction_id
        || prepare.semantic_time_unix_ns != manifest.semantic_time_unix_ns
        || prepare.base_binding_hash != manifest.base_binding_hash
        || prepare.envelope_hash != manifest.envelope_hash
        || prepare.operation != manifest.operation
        || prepare.intent_hash != manifest.intent_hash
        || prepare.operation_registry_id != manifest.operation_registry_id
        || prepare.limits_id != manifest.limits_id
        || prepare.old_atom_id != manifest.old_atom_id
        || prepare.successor_atom_id != manifest.successor_atom_id
        || prepare.successor_body_hash != manifest.successor_body_hash
        || prepare.claim_projection_hash != manifest.claim_projection_hash
        || prepare.api_evidence_projection_hash != manifest.api_evidence_projection_hash
        || prepare.successor_atom_type != manifest.successor_atom_type
        || prepare.old_node != manifest.old_node
        || prepare.successor_node != manifest.successor_node
        || prepare.supersedes_relation_id != manifest.supersedes_relation_id
        || prepare.old_provenance_hash != manifest.old_provenance_hash
        || prepare.successor_provenance_hash != manifest.successor_provenance_hash
        || prepare.successor_source_attachment_hash != manifest.successor_source_attachment_hash
        || prepare.history_event_id != manifest.history_event_id
        || prepare.history_semantic_hash != manifest.history_semantic_hash
        || prepare.component_root_hash != manifest.component_root_hash
        || prepare.logical_state_digest != manifest.logical_state_digest
        || prepare.components != manifest.components
    {
        return Err(invalid_data("update prepare does not bind its manifest"));
    }
    Ok(())
}

fn production_update_from_generation(
    root: &Path,
    generation_dir: &Path,
    manifest: &UpdateGenerationManifestV1,
    prior_graph_leaves: &[Vec<u8>],
) -> io::Result<(ProductionAtomStateV1, ProductionUpdateHistoryV1, Vec<u8>)> {
    let component = |order| {
        manifest
            .components
            .iter()
            .find(|descriptor| descriptor.registry_order == order)
            .ok_or_else(|| invalid_data("update generation component is missing"))
    };
    let record = production_read_update_component(root, generation_dir, component(20)?)?;
    if record.len() < RecordHeader::SIZE + 16 {
        return Err(invalid_data("update successor SKF1 record is truncated"));
    }
    let header = RecordHeader::from_bytes(&record[..RecordHeader::SIZE])
        .map_err(|error| invalid_data(&format!("update successor SKF1 is invalid: {error}")))?;
    let body_end = RecordHeader::SIZE
        .checked_add(header.body_len as usize)
        .ok_or_else(|| invalid_data("update successor body extent overflow"))?;
    let body = record
        .get(RecordHeader::SIZE..body_end)
        .ok_or_else(|| invalid_data("update successor body is truncated"))?;
    let successor = parse_hash_hex(&manifest.successor_atom_id, "update successor AtomId")?;
    let old = parse_hash_hex(&manifest.old_atom_id, "update old AtomId")?;
    if header.atom_id != successor
        || header.seg_id != manifest.generation as u32
        || compute_atom_id_from_payload(body)
            .map_err(|error| invalid_data(&format!("update successor is invalid: {error}")))?
            != successor
        || production_sha256(body)
            != parse_hash_hex(&manifest.successor_body_hash, "update successor body hash")?
    {
        return Err(invalid_data("update successor record identities disagree"));
    }
    let body_header = AtomBodyHeader::from_bytes(body)
        .map_err(|error| invalid_data(&format!("update successor body is invalid: {error}")))?;
    let atom_type = AtomType::from_u32(manifest.successor_atom_type)
        .ok_or_else(|| invalid_data("update successor atom type is invalid"))?;
    if body_header.atom_type() != Some(atom_type) {
        return Err(invalid_data(
            "update successor atom type disagrees with body",
        ));
    }
    let spv = production_read_update_component(root, generation_dir, component(50)?)?;
    let (spv_successor, spv_provenance, spv_source) = production_update_decode_spv1(&spv)?;
    if spv_successor != successor
        || spv_provenance
            != parse_hash_hex(
                &manifest.successor_provenance_hash,
                "update successor provenance hash",
            )?
        || spv_source
            != parse_hash_hex(
                &manifest.successor_source_attachment_hash,
                "update successor source attachment hash",
            )?
    {
        return Err(invalid_data("update SPV1 projection is invalid"));
    }
    let history_component = production_read_update_component(root, generation_dir, component(60)?)?;
    let (history_wire, history_identity) = production_update_decode_history(&history_component)?;
    let manifest_transaction_uuid = validate_production_uuid(&manifest.transaction_id)?;
    let event_id = parse_hash_hex(&manifest.history_event_id, "update history event ID")?;
    let semantic_hash = parse_hash_hex(
        &manifest.history_semantic_hash,
        "update history semantic hash",
    )?;
    let intent_hash = parse_hash_hex(&manifest.intent_hash, "update intent hash")?;
    let successor_provenance_hash = parse_hash_hex(
        &manifest.successor_provenance_hash,
        "update successor provenance hash",
    )?;
    let old_provenance_hash =
        parse_hash_hex(&manifest.old_provenance_hash, "update old provenance hash")?;
    let manifest_relation_id =
        parse_hash_hex(&manifest.supersedes_relation_id, "update relation ID")?;
    if history_wire.transaction_id != manifest.transaction_id
        || history_wire.generation != manifest.generation
        || history_wire.semantic_time_unix_ns != manifest.semantic_time_unix_ns
        || history_wire.atom_ids
            != vec![
                manifest.successor_atom_id.clone(),
                manifest.old_atom_id.clone(),
            ]
        || history_wire.supersedes_relation_id != manifest.supersedes_relation_id
        || history_wire.intent_hash != manifest.intent_hash
        || history_wire.successor_provenance_hash != manifest.successor_provenance_hash
        || history_wire.old_provenance_hash != manifest.old_provenance_hash
        || history_wire.history_semantic_hash != manifest.history_semantic_hash
        || history_wire.event_id != manifest.history_event_id
        || history_identity.transaction_uuid != manifest_transaction_uuid
        || history_identity.event_id != event_id
        || history_identity.semantic_hash != semantic_hash
        || history_identity.successor_atom_id != successor
        || history_identity.old_atom_id != old
        || history_identity.relation_id != manifest_relation_id
        || history_identity.intent_hash != intent_hash
        || history_identity.successor_provenance_hash != successor_provenance_hash
        || history_identity.old_provenance_hash != old_provenance_hash
    {
        return Err(invalid_data("update history event fields are invalid"));
    }
    let mut leaf_bytes = Vec::new();
    leaf_bytes.extend_from_slice(PRODUCTION_UPDATE_HISTORY_LEAF_ID.as_bytes());
    leaf_bytes.push(0);
    leaf_bytes.extend_from_slice(&event_id);
    leaf_bytes.extend_from_slice(&semantic_hash);
    leaf_bytes.extend_from_slice(&successor_provenance_hash);
    leaf_bytes.extend_from_slice(&old_provenance_hash);
    let graph_leaf = production_update_graph_leaf(manifest.successor_node, manifest.old_node);
    let relation_id = production_update_relation_id(successor, old);
    if relation_id != manifest_relation_id {
        return Err(invalid_data("update supersedes relation ID is invalid"));
    }
    let delta_descriptor = component(80)?;
    let delta_bytes = production_read_update_component(root, generation_dir, delta_descriptor)?;
    let (delta_header, delta_edge, delta_semantic_hash) =
        production_update_decode_delta(&delta_bytes)?;
    let mut post_graph = prior_graph_leaves.to_vec();
    post_graph.push(graph_leaf.clone());
    post_graph.sort();
    let delta_id = u32::try_from(post_graph.len())
        .map_err(|_| invalid_data("update graph delta count overflow"))?;
    let (expected_delta, expected_delta_semantic) =
        production_update_delta_bytes(delta_id, 0, manifest.successor_node, manifest.old_node)?;
    if delta_header.delta_id != delta_id
        || delta_header.base_gen != 0
        || delta_edge.src_node != manifest.successor_node
        || delta_edge.dst_node != manifest.old_node
        || delta_edge.edge_type != EdgeType::SUPERSEDES.to_u32()
        || delta_edge.confidence_q != 5000
        || delta_edge.flags != 0
        || delta_edge.valid_from_bucket != 0
        || delta_edge.valid_to_bucket != 0
        || delta_bytes != expected_delta
        || delta_semantic_hash != expected_delta_semantic
        || delta_semantic_hash
            != parse_hash_hex(
                &delta_descriptor.semantic_hash,
                "update DELT descriptor semantic hash",
            )?
    {
        return Err(invalid_data("update DELT semantics disagree with lineage"));
    }
    let grm1_descriptor = component(90)?;
    let grm1_bytes = production_read_update_component(root, generation_dir, grm1_descriptor)?;
    let grm1 = production_update_decode_graph_manifest(&grm1_bytes)?;
    let node_count = manifest
        .successor_node
        .checked_add(1)
        .ok_or_else(|| invalid_data("update GRM1 node count overflow"))?;
    let (expected_grm1, expected_grm1_semantic) =
        production_update_graph_manifest(delta_id, 0, node_count, &post_graph)?;
    if grm1.base_gen != 0
        || grm1.node_count != node_count
        || !grm1.has_edge_type(EdgeType::SUPERSEDES)
        || grm1.edge_type_mask != 1u64 << (EdgeType::SUPERSEDES.to_u32() - 1)
        || grm1.delta_count != delta_id
        || manifest.graph_leaf_count != post_graph.len() as u64
        || manifest.relation_count != post_graph.len() as u64
        || grm1_bytes != expected_grm1
        || expected_grm1_semantic
            != parse_hash_hex(
                &grm1_descriptor.semantic_hash,
                "update GRM1 descriptor semantic hash",
            )?
    {
        return Err(invalid_data(
            "update GRM1 semantics disagree with cumulative graph lineage",
        ));
    }
    Ok((
        ProductionAtomStateV1 {
            atom_id: successor,
            atom_type,
            node_num: manifest.successor_node,
            committed_generation: manifest.generation,
            body_len: header.body_len,
            body_crc32: crc32(body),
            body_hash: production_hash_bytes(body),
            segment_id: manifest.generation as u32,
            record_offset: 0,
            record_extent_len: record.len() as u64,
            domain_mask: 0xffff,
            created_at_ns: body_header.created_at_unix_ns,
            trust_level: 5000,
            source_id: 0,
            provenance_hash: successor_provenance_hash,
            history_event_id: event_id,
            history_leaf: leaf_bytes.clone(),
        },
        ProductionUpdateHistoryV1 {
            event_id,
            semantic_hash,
            leaf_bytes,
            record_bytes: history_component,
        },
        graph_leaf,
    ))
}

fn production_validate_baseline(
    root: &Path,
    baseline: &ProductionBaselineManifestV1,
    validate_live_targets: bool,
) -> io::Result<()> {
    let total_bytes = baseline
        .components
        .iter()
        .try_fold(0u64, |total, component| {
            total.checked_add(component.byte_length)
        })
        .ok_or_else(|| invalid_data("production baseline byte count overflow"))?;
    if baseline.component_count != baseline.components.len() as u64
        || baseline.components.is_empty()
        || baseline.components.len()
            > ProductionStorageLimitsV1::frozen().max_component_count as usize
        || baseline.total_bytes != total_bytes
    {
        return Err(invalid_data("production baseline counts are invalid"));
    }
    let mut previous = None;
    for descriptor in &baseline.components {
        let key = (
            descriptor.registry_order,
            descriptor.target_path.clone(),
            descriptor.ordinal,
        );
        if previous.as_ref().is_some_and(|value| value >= &key)
            || descriptor.schema != "memoryx.production-component-descriptor.v1"
            || descriptor.version != 1
            || descriptor.stage_path.is_some()
            || !matches!(
                descriptor.mode.as_str(),
                "baseline_present" | "baseline_absent"
            )
        {
            return Err(invalid_data(
                "production baseline descriptor is noncanonical",
            ));
        }
        previous = Some(key);
        let relative = descriptor
            .target_path
            .as_deref()
            .ok_or_else(|| invalid_data("production baseline descriptor has no target"))?;
        let target = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        if validate_live_targets {
            let bytes = match descriptor.mode.as_str() {
                "baseline_present" => read_bytes_bounded_under(
                    root,
                    &target,
                    ProductionStorageLimitsV1::frozen().max_component_bytes,
                )?,
                "baseline_absent" => match fs::symlink_metadata(&target) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
                    Ok(_) => {
                        return Err(invalid_data("production baseline absent target now exists"));
                    }
                    Err(error) => return Err(error),
                },
                _ => unreachable!(),
            };
            if bytes.len() as u64 != descriptor.byte_length
                || production_hash_hex(&bytes) != descriptor.byte_hash
            {
                return Err(invalid_data(&format!(
                    "production baseline component changed: {relative}"
                )));
            }
        }
    }
    if production_component_root(0, &baseline.components, &baseline.pairs)?
        != parse_hash_hex(&baseline.component_root_hash, "baseline component root")?
        || !is_hash(&baseline.root_tree_digest)
    {
        return Err(invalid_data("production baseline root digest is invalid"));
    }
    Ok(())
}

fn production_baseline_logical_digest(
    root: &Path,
    baseline: &ProductionBaselineManifestV1,
) -> io::Result<[u8; 32]> {
    let mut anchors = Vec::new();
    for entry in PRODUCTION_DIRECT_REGISTRY
        .iter()
        .filter(|entry| (150..=220).contains(&entry.order))
    {
        let descriptor = baseline
            .components
            .iter()
            .find(|descriptor| descriptor.registry_order == entry.order)
            .ok_or_else(|| invalid_data("production baseline anchor descriptor is absent"))?;
        match descriptor.mode.as_str() {
            "baseline_absent" if descriptor.byte_length == 0 => {
                anchors.push((entry.order, false, Vec::new()));
            }
            "baseline_present" => {
                let relative = descriptor
                    .target_path
                    .as_deref()
                    .ok_or_else(|| invalid_data("production baseline anchor target is absent"))?;
                let bytes = read_bytes_bounded_under(
                    root,
                    &root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR)),
                    PRODUCTION_UPDATE_MAX_COMPONENT_BYTES,
                )?;
                if bytes.len() as u64 != descriptor.byte_length
                    || production_hash_hex(&bytes) != descriptor.byte_hash
                {
                    return Err(invalid_data(
                        "production baseline anchor bytes are unavailable",
                    ));
                }
                anchors.push((entry.order, true, bytes));
            }
            _ => return Err(invalid_data("production baseline anchor is invalid")),
        }
    }
    production_logical_digest(0, [0; 32], None, &anchors)
}

fn production_open_runtime(
    token: &BorrowedOwnerQuiescence<'_, StartupAdmission>,
) -> io::Result<ProductionRuntimeStateV1> {
    token.verify()?;
    let root = token.canonical_root();
    let txn_root = production_txn_root(root);
    if txn_root.join(FORMAT_FILE_NAME).exists() {
        return Err(invalid_data(
            "production format.v1 and format.v2 cannot coexist",
        ));
    }
    let format_path = txn_root.join(PRODUCTION_FORMAT_FILE_NAME);
    if !format_path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "base_not_admitted: production format.v2 is absent",
        ));
    }
    let (format, _) = production_read_control(root, &format_path, "format.v2", |bytes, label| {
        ProductionFormatRecordV2::decode(bytes, label)
    })?;
    production_validate_format(&format)?;
    let baseline_path = txn_root.join(PRODUCTION_BASELINE_FILE_NAME);
    let (baseline, baseline_bytes) = production_read_control(
        root,
        &baseline_path,
        "baseline.v2",
        ProductionBaselineManifestV1::decode,
    )?;
    if format.baseline_hash != production_hash_hex(&baseline_bytes)
        || baseline.schema != "memoryx.production-baseline-manifest.v1"
        || baseline.version != 1
        || baseline.format_version != 2
        || baseline.source_layout_id != PRODUCTION_LEGACY_LAYOUT_ID
        || baseline.registry_id != PRODUCTION_REGISTRY_ID
        || baseline.limits_id != PRODUCTION_LIMITS_ID
    {
        return Err(invalid_data(
            "production baseline is unsupported or corrupt",
        ));
    }
    let generations = production_generation_directories(root)?;
    // A pre-commit crash may leave a descriptor-bound CAS suffix. Reconcile
    // that private transaction state before validating the exact live baseline.
    production_cleanup_pending(token)?;
    production_validate_baseline(root, &baseline, generations.is_empty())?;
    let migration_path = txn_root.join(PRODUCTION_MIGRATION_FILE_NAME);
    let (migration, migration_bytes) = production_read_control(
        root,
        &migration_path,
        "migration.v2",
        ProductionMigrationRecordV1::decode,
    )?;
    if format.migration_hash != production_hash_hex(&migration_bytes) {
        return Err(invalid_data(
            "production migration hash does not match format.v2",
        ));
    }
    production_validate_migration(&migration, &format.baseline_hash)?;
    if migration.component_count != baseline.component_count
        || migration.total_bytes != baseline.total_bytes
        || migration.required_free_bytes
            < migration.total_bytes.saturating_add(
                ProductionStorageLimitsV1::frozen().minimum_free_space_reserve_bytes,
            )
    {
        return Err(invalid_data(
            "production migration counts do not match the baseline",
        ));
    }

    let baseline_digest =
        parse_hash_hex(&baseline.legacy_semantic_digest, "baseline logical digest")?;
    if baseline_digest != production_baseline_logical_digest(root, &baseline)? {
        return Err(invalid_data(
            "production generation-zero logical digest changed",
        ));
    }
    let mut head = ProductionCommittedHead {
        generation: 0,
        commit_hash: [0; 32],
        logical_digest: baseline_digest,
    };
    let mut atom = None;
    let mut atoms = Vec::new();
    let mut history_leaves = Vec::new();
    let mut graph_leaves = Vec::new();
    let mut superseded_by = BTreeMap::new();
    let mut committed_receipts = BTreeMap::new();
    let mut committed_transactions = BTreeMap::new();
    let mut batch_transactions = BTreeMap::new();
    let mut update_transactions = BTreeMap::new();
    let final_generation = generations.last().map(|(generation, _)| *generation);
    for (generation, generation_dir) in generations {
        let is_head = Some(generation) == final_generation;
        let parent = head.clone();
        let commit_path = generation_dir.join(COMMIT_FILE_NAME);
        let commit_bytes = read_bytes_bounded_under(root, &commit_path, MAX_CONTROL_RECORD_BYTES)?;
        let peek: serde_json::Value = serde_json::from_slice(&commit_bytes).map_err(|error| {
            invalid_data(&format!(
                "production commit schema cannot be decoded: {error}"
            ))
        })?;
        let schema = peek
            .get("schema")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| invalid_data("production commit has no schema"))?;
        let (
            logical_digest,
            transaction_id,
            semantic_time_unix_ns,
            base_hash,
            intent_hash,
            envelope_hash,
        ) = if schema == "memoryx.production-generation-manifest.v1" {
            if !atoms.is_empty() {
                return Err(invalid_data(
                    "direct-v1 generation cannot follow a committed atom",
                ));
            }
            let manifest = ProductionGenerationManifestV1::decode(&commit_bytes, "commit.bin")?;
            production_validate_manifest(root, &generation_dir, &manifest, generation, &parent)?;
            if is_head {
                production_install_generation(token, &generation_dir, &manifest)?;
            }
            let (generation_atom, history_semantic_hash) =
                production_atom_from_generation(root, &generation_dir, &manifest)?;
            if hex_lower(&history_semantic_hash) != manifest.history_event_hash {
                return Err(invalid_data(
                    "production history semantic hash does not match manifest",
                ));
            }
            atoms.push(generation_atom.clone());
            history_leaves.push(generation_atom.history_leaf.clone());
            let atom_refs = atoms.iter().collect::<Vec<_>>();
            let history_refs = history_leaves.iter().map(Vec::as_slice).collect::<Vec<_>>();
            let logical_digest = production_logical_digest_multi(
                generation,
                parent.commit_hash,
                &atom_refs,
                &history_refs,
                &production_anchor_leaves_from_descriptors(root, &manifest.components)?,
            )?;
            if hex_lower(&logical_digest) != manifest.logical_state_digest {
                return Err(invalid_data(
                    "production logical state digest does not match manifest",
                ));
            }
            let commit_hash = production_hash_bytes(&commit_bytes);
            let receipt = DirectIngestReceiptV1::create(
                DirectIngestResultKindV1::Created,
                manifest.transaction_id.clone(),
                manifest.semantic_time_unix_ns,
                parse_hash_hex(&manifest.intent_hash, "manifest intent_hash")?,
                parse_hash_hex(&manifest.base_binding_hash, "manifest base_binding_hash")?,
                generation,
                commit_hash,
                logical_digest,
                generation_atom.atom_id,
                generation_atom.node_num,
                Some(generation_atom.history_event_id),
            )?;
            committed_receipts.insert(manifest.transaction_id.clone(), receipt);
            committed_transactions.insert(
                manifest.transaction_id.clone(),
                ProductionCommittedTransactionV1 {
                    parent: parent.clone(),
                    semantic_time_unix_ns: manifest.semantic_time_unix_ns,
                    base_binding_hash: parse_hash_hex(
                        &manifest.base_binding_hash,
                        "manifest base_binding_hash",
                    )?,
                    intent_hash: parse_hash_hex(&manifest.intent_hash, "manifest intent_hash")?,
                    envelope_hash: parse_hash_hex(
                        &manifest.envelope_hash,
                        "manifest envelope_hash",
                    )?,
                },
            );
            atom = Some(generation_atom);
            (
                logical_digest,
                manifest.transaction_id,
                manifest.semantic_time_unix_ns,
                parse_hash_hex(&manifest.base_binding_hash, "manifest base_binding_hash")?,
                parse_hash_hex(&manifest.intent_hash, "manifest intent_hash")?,
                parse_hash_hex(&manifest.envelope_hash, "manifest envelope_hash")?,
            )
        } else if schema == "memoryx.batch-generation-manifest.v1" {
            let manifest = BatchGenerationManifestV1::decode(&commit_bytes, "batch commit.bin")?;
            production_validate_batch_manifest(
                root,
                &generation_dir,
                &manifest,
                generation,
                &parent,
            )?;
            if is_head {
                production_install_batch_generation(token, &generation_dir, &manifest)?;
            }
            let (created_atoms, history) =
                production_atoms_from_batch_generation(root, &generation_dir, &manifest)?;
            let created_atom_ids = created_atoms
                .iter()
                .map(|atom| atom.atom_id)
                .collect::<Vec<_>>();
            let parent_atoms = atoms.clone();
            let parent_history_leaves = history_leaves.clone();
            atoms.extend(created_atoms);
            history_leaves.push(history.leaf_bytes.clone());
            let atom_refs = atoms.iter().collect::<Vec<_>>();
            let history_refs = history_leaves.iter().map(Vec::as_slice).collect::<Vec<_>>();
            let logical_digest = production_logical_digest_multi(
                generation,
                parent.commit_hash,
                &atom_refs,
                &history_refs,
                &production_anchor_leaves_from_descriptors(root, &manifest.components)?,
            )?;
            if hex_lower(&logical_digest) != manifest.logical_state_digest
                || manifest.post_atom_count != atoms.len() as u64
                || manifest.history_event_count != history_leaves.len() as u64
            {
                return Err(invalid_data(
                    "batch logical state does not match its manifest",
                ));
            }
            let commit_hash = production_hash_bytes(&commit_bytes);
            batch_transactions.insert(
                manifest.transaction_id.clone(),
                ProductionCommittedBatchV1 {
                    parent: parent.clone(),
                    parent_atoms,
                    parent_history_leaves,
                    semantic_time_unix_ns: manifest.semantic_time_unix_ns,
                    base_binding_hash: parse_hash_hex(
                        &manifest.base_binding_hash,
                        "batch base binding",
                    )?,
                    intent_hash: parse_hash_hex(&manifest.intent_hash, "batch intent hash")?,
                    envelope_hash: parse_hash_hex(&manifest.envelope_hash, "batch envelope hash")?,
                    decision_hashes: manifest
                        .decision_hashes
                        .iter()
                        .map(|value| parse_hash_hex(value, "batch decision hash"))
                        .collect::<io::Result<Vec<_>>>()?,
                    created_atom_ids,
                    history_event_id: history.event_id,
                    commit_hash,
                    logical_digest,
                },
            );
            (
                logical_digest,
                manifest.transaction_id,
                manifest.semantic_time_unix_ns,
                parse_hash_hex(&manifest.base_binding_hash, "batch base binding")?,
                parse_hash_hex(&manifest.intent_hash, "batch intent hash")?,
                parse_hash_hex(&manifest.envelope_hash, "batch envelope hash")?,
            )
        } else if schema == PRODUCTION_UPDATE_MANIFEST_SCHEMA {
            let manifest = UpdateGenerationManifestV1::decode(&commit_bytes, "update commit.bin")?;
            production_validate_update_manifest(
                root,
                &generation_dir,
                &manifest,
                generation,
                &parent,
            )?;
            if committed_transactions.contains_key(&manifest.transaction_id)
                || batch_transactions.contains_key(&manifest.transaction_id)
                || update_transactions.contains_key(&manifest.transaction_id)
            {
                return Err(invalid_data(
                    "production transaction UUID is duplicated across operation kinds",
                ));
            }
            let old_id = parse_hash_hex(&manifest.old_atom_id, "update old AtomId")?;
            let successor_id =
                parse_hash_hex(&manifest.successor_atom_id, "update successor AtomId")?;
            let old_atom = atoms
                .iter()
                .find(|candidate| candidate.atom_id == old_id)
                .cloned()
                .ok_or_else(|| invalid_data("update old atom is absent from parent state"))?;
            if old_atom.node_num != manifest.old_node
                || old_atom.provenance_hash
                    != parse_hash_hex(&manifest.old_provenance_hash, "update old provenance")?
                || superseded_by.contains_key(&old_id)
                || atoms
                    .iter()
                    .any(|candidate| candidate.atom_id == successor_id)
            {
                return Err(invalid_data(
                    "update parent lineage or provenance state is invalid",
                ));
            }
            let (successor, history, graph_leaf) =
                production_update_from_generation(root, &generation_dir, &manifest, &graph_leaves)?;
            if successor.node_num
                != atoms
                    .iter()
                    .map(|candidate| candidate.node_num)
                    .max()
                    .map_or(0, |node| node.saturating_add(1))
            {
                return Err(invalid_data("update successor NodeNum is not canonical"));
            }
            atoms.push(successor.clone());
            history_leaves.push(history.leaf_bytes.clone());
            graph_leaves.push(graph_leaf.clone());
            superseded_by.insert(old_id, successor_id);
            let idloc = production_read_update_component(
                root,
                &generation_dir,
                manifest
                    .components
                    .iter()
                    .find(|descriptor| descriptor.registry_order == 30)
                    .ok_or_else(|| invalid_data("update IDL1 component is absent"))?,
            )?;
            let locate = production_read_update_component(
                root,
                &generation_dir,
                manifest
                    .components
                    .iter()
                    .find(|descriptor| descriptor.registry_order == 40)
                    .ok_or_else(|| invalid_data("update LOC1 component is absent"))?,
            )?;
            let current_view = production_read_update_component(
                root,
                &generation_dir,
                manifest
                    .components
                    .iter()
                    .find(|descriptor| descriptor.registry_order == 70)
                    .ok_or_else(|| invalid_data("update current-view component is absent"))?,
            )?;
            if idloc != production_idloc_bytes_many(&atoms)
                || locate != production_update_locate_bytes(&atoms)
                || current_view != production_update_current_view_bytes(&superseded_by)?
            {
                return Err(invalid_data(
                    "update membership or current-view projection is invalid",
                ));
            }
            let provenance = production_read_update_component(
                root,
                &generation_dir,
                manifest
                    .components
                    .iter()
                    .find(|descriptor| descriptor.registry_order == 50)
                    .ok_or_else(|| invalid_data("update provenance component is absent"))?,
            )?;
            let atom_refs = atoms.iter().collect::<Vec<_>>();
            let graph_refs = graph_leaves.iter().map(Vec::as_slice).collect::<Vec<_>>();
            let history_refs = history_leaves.iter().map(Vec::as_slice).collect::<Vec<_>>();
            let logical_digest = production_logical_digest_multi_with_graph(
                generation,
                parent.commit_hash,
                &atom_refs,
                &graph_refs,
                &history_refs,
                &production_update_post_anchors(root, provenance)?,
            )?;
            if hex_lower(&logical_digest) != manifest.logical_state_digest
                || manifest.post_atom_count != atoms.len() as u64
                || manifest.history_event_count != history_leaves.len() as u64
                || manifest.graph_leaf_count != graph_leaves.len() as u64
                || manifest.relation_count != superseded_by.len() as u64
            {
                return Err(invalid_data("update logical state does not match manifest"));
            }
            if is_head {
                production_install_update_generation(token, &generation_dir, &manifest)?;
            }
            let commit_hash = production_hash_bytes(&commit_bytes);
            let receipt = UpdateAtomReceiptV1::create(
                manifest.transaction_id.clone(),
                manifest.semantic_time_unix_ns,
                parent.generation,
                parent.commit_hash,
                generation,
                commit_hash,
                logical_digest,
                parse_hash_hex(&manifest.base_binding_hash, "update base binding")?,
                parse_hash_hex(&manifest.envelope_hash, "update envelope hash")?,
                parse_hash_hex(&manifest.intent_hash, "update intent hash")?,
                old_id,
                successor_id,
                successor.node_num,
                parse_hash_hex(&manifest.supersedes_relation_id, "update relation ID")?,
                history.event_id,
                history.semantic_hash,
                old_atom.provenance_hash,
                successor.provenance_hash,
                parse_hash_hex(&manifest.component_root_hash, "update component root")?,
            )?;
            update_transactions.insert(
                manifest.transaction_id.clone(),
                ProductionCommittedUpdateV1 {
                    receipt,
                    successor_body_hash: parse_hash_hex(
                        &manifest.successor_body_hash,
                        "update successor body hash",
                    )?,
                    claim_projection_hash: parse_hash_hex(
                        &manifest.claim_projection_hash,
                        "update claim projection hash",
                    )?,
                    api_evidence_projection_hash: parse_hash_hex(
                        &manifest.api_evidence_projection_hash,
                        "update evidence projection hash",
                    )?,
                    successor_source_attachment_hash: parse_hash_hex(
                        &manifest.successor_source_attachment_hash,
                        "update source attachment hash",
                    )?,
                },
            );
            atom = Some(successor);
            (
                logical_digest,
                manifest.transaction_id,
                manifest.semantic_time_unix_ns,
                parse_hash_hex(&manifest.base_binding_hash, "update base binding")?,
                parse_hash_hex(&manifest.intent_hash, "update intent hash")?,
                parse_hash_hex(&manifest.envelope_hash, "update envelope hash")?,
            )
        } else {
            return Err(invalid_data("production commit schema is unsupported"));
        };
        let commit_hash = production_hash_bytes(&commit_bytes);
        head = ProductionCommittedHead {
            generation,
            commit_hash,
            logical_digest,
        };
        let _ = (
            transaction_id,
            semantic_time_unix_ns,
            base_hash,
            intent_hash,
            envelope_hash,
        );
    }
    let base_binding =
        ProductionBaseBindingV1::from_identity(token.physical_identity(), head.clone())?;
    let admission = ProductionStartupAdmissionV1::from_body(ProductionStartupAdmissionBodyV1 {
        schema: PRODUCTION_STARTUP_SCHEMA.to_owned(),
        version: 1,
        format_version: 2,
        classification: "production_v2".to_owned(),
        codec_id: PRODUCTION_CODEC_ID.to_owned(),
        registry_id: PRODUCTION_REGISTRY_ID.to_owned(),
        limits_id: PRODUCTION_LIMITS_ID.to_owned(),
        base_binding_hash: hex_lower(&base_binding.hash()),
        head_generation: head.generation,
        head_commit_hash: hex_lower(&head.commit_hash),
        head_logical_digest: hex_lower(&head.logical_digest),
        install_state: "installed_state_verified".to_owned(),
        component_open_mode: "open_existing_no_repair".to_owned(),
        live_view_state: "not_published".to_owned(),
    })?;
    production_validate_startup_admission(&admission, &base_binding, &head)?;
    token.verify()?;
    Ok(ProductionRuntimeStateV1 {
        head,
        base_binding,
        admission_bytes: admission.canonical_bytes()?,
        atom,
        atoms,
        history_leaves,
        graph_leaves,
        superseded_by,
        committed_receipts,
        committed_transactions,
        batch_transactions,
        update_transactions,
        owner_lifetime_transactions: BTreeMap::new(),
    })
}

fn production_registry_entry(key: &str) -> io::Result<&'static ProductionRegistryEntry> {
    PRODUCTION_DIRECT_REGISTRY
        .iter()
        .find(|entry| entry.key == key)
        .ok_or_else(|| invalid_data("production registry key is not compiled"))
}

fn production_preflight_direct_ingest(root: &Path) -> io::Result<Vec<u8>> {
    let segment_path = root.join("cas/seg_00000.dat");
    let segment_bytes = read_bytes_bounded_under(
        root,
        &segment_path,
        ProductionStorageLimitsV1::frozen().max_component_bytes,
    )?;
    if !segment_bytes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "migration_required: P0-C requires an empty admitted CAS segment",
        ));
    }
    Ok(segment_bytes)
}

#[derive(Debug, Clone)]
struct ProductionBatchPlanV1 {
    outcomes: Vec<BatchIngestItemOutcomeV1>,
    decision_bytes: Vec<Vec<u8>>,
    created_ordinals: Vec<usize>,
    preflight_bytes: Vec<u8>,
}

fn production_projection_hash(claims: &[u8], evidence: &[u8]) -> [u8; 32] {
    let mut bytes = Vec::with_capacity(16 + claims.len() + evidence.len());
    bytes.extend_from_slice(&((claims.len() / 25) as u64).to_le_bytes());
    bytes.extend_from_slice(claims);
    bytes.extend_from_slice(&((evidence.len() / 54) as u64).to_le_bytes());
    bytes.extend_from_slice(evidence);
    production_hash_bytes(&bytes)
}

#[allow(clippy::too_many_arguments)]
fn production_batch_probe_bytes(
    base_binding_hash: [u8; 32],
    item: &ProductionBatchItemCodecV1,
    class: u8,
    existing_body_hash: [u8; 32],
    existing_projection_hash: [u8; 32],
    node_num: u64,
    generation: u64,
    evidence_statuses: &[u8],
) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(PRODUCTION_BATCH_PROBE_ID.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&base_binding_hash);
    bytes.extend_from_slice(&item.ordinal.to_le_bytes());
    bytes.extend_from_slice(&item.atom_id);
    bytes.push(class);
    bytes.extend_from_slice(&existing_body_hash);
    bytes.extend_from_slice(&existing_projection_hash);
    bytes.extend_from_slice(&node_num.to_le_bytes());
    bytes.extend_from_slice(&generation.to_le_bytes());
    bytes.extend_from_slice(&(evidence_statuses.len() as u32).to_le_bytes());
    bytes.extend_from_slice(evidence_statuses);
    bytes
}

#[allow(clippy::too_many_arguments)]
fn production_batch_decision_bytes(
    transaction_uuid: [u8; 16],
    item: &ProductionBatchItemCodecV1,
    probe_hash: [u8; 32],
    result: BatchIngestItemResultV1,
    reason: BatchIngestItemReasonV1,
    node_num: Option<u64>,
    generation: Option<u64>,
    first_ordinal: Option<u32>,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(PRODUCTION_BATCH_DECISION_ID.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&transaction_uuid);
    bytes.extend_from_slice(&item.ordinal.to_le_bytes());
    bytes.extend_from_slice(&item.hash);
    bytes.extend_from_slice(&probe_hash);
    bytes.push(match result {
        BatchIngestItemResultV1::Created => 1,
        BatchIngestItemResultV1::Reused => 2,
        BatchIngestItemResultV1::Refused => 3,
    });
    bytes.extend_from_slice(
        &(match reason {
            BatchIngestItemReasonV1::Created => 1u16,
            BatchIngestItemReasonV1::AlreadyCommitted => 2,
            BatchIngestItemReasonV1::DuplicateInput => 3,
            BatchIngestItemReasonV1::CanonicalConflict => 4,
            BatchIngestItemReasonV1::TombstonedIdentity => 5,
            BatchIngestItemReasonV1::InvalidItem => 6,
            BatchIngestItemReasonV1::EvidenceSourceNotLive => 7,
        })
        .to_le_bytes(),
    );
    bytes.extend_from_slice(&item.atom_id);
    bytes.extend_from_slice(&node_num.unwrap_or(u64::MAX).to_le_bytes());
    bytes.extend_from_slice(&generation.unwrap_or(0).to_le_bytes());
    bytes.extend_from_slice(&first_ordinal.unwrap_or(u32::MAX).to_le_bytes());
    bytes.push(u8::from(result == BatchIngestItemResultV1::Created));
    bytes
}

fn production_plan_batch(
    root: &Path,
    state: &ProductionRuntimeStateV1,
    intent: &ProductionBatchIntentV1,
    envelope: &ProductionBatchEnvelopeV1,
) -> io::Result<ProductionBatchPlanV1> {
    if intent.base_binding_hash != state.base_binding.hash()
        || envelope.base_binding_hash != intent.base_binding_hash
        || envelope.intent_hash != intent.hash
    {
        return Err(invalid_data(
            "batch envelope, intent, and parent binding disagree",
        ));
    }
    let generation = state
        .head
        .generation
        .checked_add(1)
        .ok_or_else(|| invalid_data("batch generation overflow"))?;
    if generation > PRODUCTION_MAX_GENERATIONS {
        return Err(invalid_data("batch generation limit reached"));
    }
    let segment = root.join("cas/seg_00000.dat");
    let segment_identity = stable_identity(&open_no_follow(&segment)?)?;
    require_single_link(&segment_identity, "production batch CAS segment")?;
    for entry in PRODUCTION_DIRECT_REGISTRY
        .iter()
        .filter(|entry| (150..=220).contains(&entry.order))
    {
        let target = root.join(entry.target.expect("anchor target"));
        match fs::symlink_metadata(target) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "edge-free batch does not admit pre-existing source, provenance, context, embedding, predicate, entity, relation, or tombstone anchors",
                ));
            }
            Err(error) => return Err(error),
        }
    }

    let next_node = state
        .atoms
        .iter()
        .map(|atom| atom.node_num)
        .max()
        .map_or(0, |node| node.saturating_add(1));
    let mut next_created_node = next_node;
    let mut outcomes = Vec::with_capacity(intent.items.len());
    let mut decision_bytes = Vec::with_capacity(intent.items.len());
    let mut created_ordinals = Vec::new();
    let mut first_by_atom = BTreeMap::<AtomId, usize>::new();
    for (index, item) in intent.items.iter().enumerate() {
        let existing = state.atoms.iter().find(|atom| atom.atom_id == item.atom_id);
        let evidence_statuses = item
            .evidence_projection
            .as_chunks::<54>()
            .0
            .iter()
            .map(|record| {
                let source: AtomId = record[..32].try_into().unwrap();
                if state.atoms.iter().any(|atom| atom.atom_id == source) {
                    1
                } else {
                    2
                }
            })
            .collect::<Vec<_>>();
        let header_valid = AtomBodyHeader::from_bytes(&item.body)
            .ok()
            .is_some_and(|header| header.atom_type() == Some(item.atom_type))
            && compute_atom_id_from_payload(&item.body)
                .ok()
                .is_some_and(|atom_id| atom_id == item.atom_id);
        let (class, existing_body_hash, existing_projection_hash, node, parent_generation) =
            if !header_valid {
                (4, [0; 32], [0; 32], u64::MAX, 0)
            } else if let Some(atom) = existing {
                let body = production_read_exact_committed_body(root, atom)?;
                let exact = production_hash_bytes(&body) == item.body_hash
                    && item.claim_projection.is_empty()
                    && item.evidence_projection.is_empty();
                (
                    if exact { 1 } else { 2 },
                    atom.body_hash,
                    production_projection_hash(&[], &[]),
                    atom.node_num,
                    atom.committed_generation,
                )
            } else {
                (0, [0; 32], [0; 32], u64::MAX, 0)
            };
        let probe = production_batch_probe_bytes(
            state.base_binding.hash(),
            item,
            class,
            existing_body_hash,
            existing_projection_hash,
            node,
            parent_generation,
            &evidence_statuses,
        );
        let probe_hash = production_hash_bytes(&probe);
        let duplicate = first_by_atom.get(&item.atom_id).copied();
        let (result, reason, outcome_node, outcome_generation, first_ordinal) = if class == 4 {
            (
                BatchIngestItemResultV1::Refused,
                BatchIngestItemReasonV1::InvalidItem,
                None,
                None,
                None,
            )
        } else if let Some(first) = duplicate {
            let previous = &intent.items[first];
            if previous.body_hash == item.body_hash
                && previous.atom_type == item.atom_type
                && previous.claim_projection == item.claim_projection
                && previous.evidence_projection == item.evidence_projection
            {
                (
                    BatchIngestItemResultV1::Refused,
                    BatchIngestItemReasonV1::DuplicateInput,
                    None,
                    None,
                    Some(first as u32),
                )
            } else {
                (
                    BatchIngestItemResultV1::Refused,
                    BatchIngestItemReasonV1::CanonicalConflict,
                    None,
                    None,
                    None,
                )
            }
        } else if class == 1 {
            (
                BatchIngestItemResultV1::Reused,
                BatchIngestItemReasonV1::AlreadyCommitted,
                Some(node),
                Some(parent_generation),
                None,
            )
        } else if class == 2 {
            (
                BatchIngestItemResultV1::Refused,
                BatchIngestItemReasonV1::CanonicalConflict,
                None,
                None,
                None,
            )
        } else if evidence_statuses.iter().any(|status| *status != 1) {
            (
                BatchIngestItemResultV1::Refused,
                BatchIngestItemReasonV1::EvidenceSourceNotLive,
                None,
                None,
                None,
            )
        } else if !item.claim_projection.is_empty() || !item.evidence_projection.is_empty() {
            (
                BatchIngestItemResultV1::Refused,
                BatchIngestItemReasonV1::InvalidItem,
                None,
                None,
                None,
            )
        } else {
            let node = next_created_node;
            next_created_node = next_created_node
                .checked_add(1)
                .ok_or_else(|| invalid_data("batch NodeNum allocation overflow"))?;
            created_ordinals.push(index);
            (
                BatchIngestItemResultV1::Created,
                BatchIngestItemReasonV1::Created,
                Some(node),
                Some(generation),
                None,
            )
        };
        if class != 4 {
            first_by_atom.entry(item.atom_id).or_insert(index);
        }
        let decision = production_batch_decision_bytes(
            envelope.transaction_uuid,
            item,
            probe_hash,
            result,
            reason,
            outcome_node,
            outcome_generation,
            first_ordinal,
        );
        let decision_hash = production_hash_bytes(&decision);
        decision_bytes.push(decision);
        outcomes.push(BatchIngestItemOutcomeV1 {
            ordinal: item.ordinal,
            atom_id: item.atom_id,
            result,
            reason,
            node_num: outcome_node,
            committed_generation: outcome_generation,
            first_input_ordinal: first_ordinal,
            decision_hash,
        });
    }
    let created = created_ordinals.len() as u32;
    let reused = outcomes
        .iter()
        .filter(|outcome| outcome.result == BatchIngestItemResultV1::Reused)
        .count() as u32;
    let refused = outcomes.len() as u32 - created - reused;
    let mut preflight = Vec::new();
    preflight.extend_from_slice(PRODUCTION_BATCH_PREFLIGHT_ID.as_bytes());
    preflight.push(0);
    preflight.extend_from_slice(&1u16.to_le_bytes());
    preflight.extend_from_slice(&envelope.transaction_uuid);
    preflight.extend_from_slice(&state.base_binding.hash());
    preflight.extend_from_slice(&intent.hash);
    preflight.extend_from_slice(&state.head.generation.to_le_bytes());
    preflight.extend_from_slice(&state.head.commit_hash);
    preflight.extend_from_slice(&state.head.logical_digest);
    preflight.extend_from_slice(&(state.atoms.len() as u64).to_le_bytes());
    preflight.extend_from_slice(&(state.history_leaves.len() as u64).to_le_bytes());
    preflight.extend_from_slice(&next_node.to_le_bytes());
    preflight.extend_from_slice(&(outcomes.len() as u32).to_le_bytes());
    for outcome in &outcomes {
        preflight.extend_from_slice(&outcome.decision_hash);
    }
    preflight.extend_from_slice(&created.to_le_bytes());
    preflight.extend_from_slice(&reused.to_le_bytes());
    preflight.extend_from_slice(&refused.to_le_bytes());

    if created > 0 {
        let extent = created_ordinals.iter().try_fold(0u64, |total, ordinal| {
            let body_len = intent.items[*ordinal].body_len;
            let aligned = body_len
                .checked_add(15)
                .map(|value| value / 16 * 16)
                .ok_or_else(|| invalid_data("batch append alignment overflow"))?;
            total
                .checked_add(64)
                .and_then(|value| value.checked_add(aligned))
                .and_then(|value| value.checked_add(16))
                .ok_or_else(|| invalid_data("batch append extent overflow"))
        })?;
        if extent > PRODUCTION_BATCH_MAX_APPEND_EXTENT_BYTES {
            return Err(invalid_data("batch resource preflight failed"));
        }
    }
    Ok(ProductionBatchPlanV1 {
        outcomes,
        decision_bytes,
        created_ordinals,
        preflight_bytes: preflight,
    })
}

#[derive(Debug, Clone)]
struct ProductionBatchHistoryV1 {
    event_id: [u8; 32],
    semantic_hash: [u8; 32],
    line_bytes: Vec<u8>,
    leaf_bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProductionBatchHistoryDetailsWire {
    item_count: String,
    created_count: String,
    reused_count: String,
    refused_count: String,
    result_kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProductionBatchHistoryWire {
    schema_version: String,
    event_id: String,
    transaction_id: String,
    event_ordinal: u32,
    generation: u64,
    timestamp_unix_ns: u64,
    operation: String,
    event_kind: String,
    outcome: String,
    atom_ids: Vec<String>,
    details: ProductionBatchHistoryDetailsWire,
    intent_hash: String,
    event_semantic_hash: String,
}

fn production_batch_history(
    generation: u64,
    envelope: &ProductionBatchEnvelopeV1,
    intent: &ProductionBatchIntentV1,
    plan: &ProductionBatchPlanV1,
) -> io::Result<ProductionBatchHistoryV1> {
    let mut id_preimage = Vec::new();
    id_preimage.extend_from_slice(PRODUCTION_BATCH_HISTORY_EVENT_ID.as_bytes());
    id_preimage.push(0);
    id_preimage.extend_from_slice(&envelope.transaction_uuid);
    id_preimage.extend_from_slice(&0u32.to_le_bytes());
    let event_id = production_hash_bytes(&id_preimage);
    let created = plan.created_ordinals.len() as u32;
    let reused = plan
        .outcomes
        .iter()
        .filter(|outcome| outcome.result == BatchIngestItemResultV1::Reused)
        .count() as u32;
    let refused = plan.outcomes.len() as u32 - created - reused;
    let mut semantic = Vec::new();
    semantic.extend_from_slice(PRODUCTION_BATCH_HISTORY_SEMANTIC_ID.as_bytes());
    semantic.push(0);
    semantic.extend_from_slice(&1u16.to_le_bytes());
    semantic.extend_from_slice(&envelope.transaction_uuid);
    semantic.extend_from_slice(&0u32.to_le_bytes());
    semantic.extend_from_slice(&generation.to_le_bytes());
    semantic.extend_from_slice(&envelope.semantic_time_unix_ns.to_le_bytes());
    semantic.extend_from_slice(&2u16.to_le_bytes());
    semantic.push(1);
    semantic.extend_from_slice(&(plan.outcomes.len() as u32).to_le_bytes());
    semantic.extend_from_slice(&created.to_le_bytes());
    semantic.extend_from_slice(&reused.to_le_bytes());
    semantic.extend_from_slice(&refused.to_le_bytes());
    for ordinal in &plan.created_ordinals {
        let item = &intent.items[*ordinal];
        semantic.extend_from_slice(&item.ordinal.to_le_bytes());
        semantic.extend_from_slice(&item.atom_id);
        semantic.push(item.atom_type.to_u32() as u8);
        semantic.extend_from_slice(&((item.claim_projection.len() / 25) as u64).to_le_bytes());
        semantic.extend_from_slice(&((item.evidence_projection.len() / 54) as u64).to_le_bytes());
        semantic.extend_from_slice(&plan.outcomes[*ordinal].decision_hash);
    }
    semantic.extend_from_slice(&intent.hash);
    let semantic_hash = production_hash_bytes(&semantic);
    let wire = ProductionBatchHistoryWire {
        schema_version: "memoryx.history.batch-transaction-once.v1".to_owned(),
        event_id: hex_lower(&event_id),
        transaction_id: envelope.transaction_id.clone(),
        event_ordinal: 0,
        generation,
        timestamp_unix_ns: envelope.semantic_time_unix_ns,
        operation: "batch_ingest".to_owned(),
        event_kind: "mutation".to_owned(),
        outcome: "committed".to_owned(),
        atom_ids: plan
            .created_ordinals
            .iter()
            .map(|ordinal| hex_lower(&intent.items[*ordinal].atom_id))
            .collect(),
        details: ProductionBatchHistoryDetailsWire {
            item_count: plan.outcomes.len().to_string(),
            created_count: created.to_string(),
            reused_count: reused.to_string(),
            refused_count: refused.to_string(),
            result_kind: "batch_created".to_owned(),
        },
        intent_hash: hex_lower(&intent.hash),
        event_semantic_hash: hex_lower(&semantic_hash),
    };
    let mut line_bytes = serde_json::to_vec(&wire).map_err(io::Error::other)?;
    line_bytes.push(b'\n');
    let mut leaf_bytes = Vec::new();
    leaf_bytes.extend_from_slice(PRODUCTION_BATCH_HISTORY_LEAF_ID.as_bytes());
    leaf_bytes.push(0);
    leaf_bytes.extend_from_slice(&1u16.to_le_bytes());
    leaf_bytes.extend_from_slice(&generation.to_le_bytes());
    leaf_bytes.extend_from_slice(&0u32.to_le_bytes());
    leaf_bytes.extend_from_slice(&envelope.transaction_uuid);
    leaf_bytes.extend_from_slice(&event_id);
    leaf_bytes.extend_from_slice(&envelope.semantic_time_unix_ns.to_le_bytes());
    leaf_bytes.extend_from_slice(&2u16.to_le_bytes());
    leaf_bytes.extend_from_slice(&semantic_hash);
    Ok(ProductionBatchHistoryV1 {
        event_id,
        semantic_hash,
        line_bytes,
        leaf_bytes,
    })
}

#[allow(clippy::too_many_arguments)]
fn production_batch_component(
    key: &str,
    order: u16,
    ordinal: u32,
    mode: &str,
    target: Option<&str>,
    stage: Option<String>,
    codec: &str,
    bytes: &[u8],
    record_count: u64,
    pair_id: Option<String>,
) -> io::Result<ProductionComponentDescriptorV1> {
    production_batch_component_from_digest(
        key,
        order,
        ordinal,
        mode,
        target,
        stage,
        codec,
        bytes.len() as u64,
        production_hash_hex(bytes),
        record_count,
        pair_id,
    )
}

#[allow(clippy::too_many_arguments)]
fn production_batch_component_from_digest(
    key: &str,
    order: u16,
    ordinal: u32,
    mode: &str,
    target: Option<&str>,
    stage: Option<String>,
    codec: &str,
    byte_length: u64,
    byte_hash: String,
    record_count: u64,
    pair_id: Option<String>,
) -> io::Result<ProductionComponentDescriptorV1> {
    ProductionComponentDescriptorV1::from_body(ProductionComponentDescriptorBodyV1 {
        schema: "memoryx.batch-component-descriptor.v1".to_owned(),
        version: 1,
        registry_key: key.to_owned(),
        registry_order: order,
        ordinal,
        mode: mode.to_owned(),
        target_path: target.map(ToOwned::to_owned),
        stage_path: stage,
        content_codec_id: codec.to_owned(),
        byte_length,
        byte_hash: byte_hash.clone(),
        semantic_hash: byte_hash,
        record_count,
        pair_id,
    })
}

#[allow(clippy::too_many_arguments)]
fn production_batch_pair(
    pair_id: String,
    left_key: &str,
    left_ordinal: u32,
    right_key: &str,
    right_ordinal: u32,
    left_count: u64,
    right_count: u64,
    logical_count: u64,
    auxiliary_count: u64,
    root_bytes: &[u8],
) -> io::Result<ProductionPairDescriptorV1> {
    ProductionPairDescriptorV1::from_body(ProductionPairDescriptorBodyV1 {
        schema: "memoryx.batch-pair-descriptor.v1".to_owned(),
        version: 1,
        pair_id,
        left_registry_key: left_key.to_owned(),
        left_ordinal,
        right_registry_key: right_key.to_owned(),
        right_ordinal,
        left_record_count: left_count,
        right_record_count: right_count,
        logical_item_count: logical_count,
        auxiliary_count,
        shared_semantic_root: production_hash_hex(root_bytes),
    })
}

fn production_write_batch_stage(pending: &Path, relative: &str, bytes: &[u8]) -> io::Result<()> {
    let path = pending.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
    production_write_new(&path, bytes)
}

fn production_append_batch_cas(
    root: &Path,
    records: &[Vec<u8>],
    descriptors: &[BatchCasAppendDescriptorV1],
) -> io::Result<()> {
    let target = root.join("cas/seg_00000.dat");
    let parent = target
        .parent()
        .ok_or_else(|| invalid_data("batch CAS segment has no parent"))?;
    let guard = AncestorGuard::acquire(root, parent)?;
    reject_link_or_reparse(&target)?;
    let mut file = open_append_no_follow(&target)?;
    require_single_link(&stable_identity(&file)?, "batch CAS segment")?;
    for (ordinal, (record, descriptor)) in records.iter().zip(descriptors).enumerate() {
        let expected_pre = file.metadata()?.len();
        if descriptor.schema != "memoryx.batch-cas-append-descriptor.v1"
            || descriptor.version != 1
            || descriptor.segment_id != 0
            || descriptor.ordinal != ordinal as u32
            || descriptor.pre_segment_length != expected_pre
            || descriptor.record_offset != expected_pre
            || descriptor.record_extent_length != record.len() as u64
            || descriptor.post_segment_length
                != expected_pre
                    .checked_add(record.len() as u64)
                    .ok_or_else(|| invalid_data("batch CAS post length overflow"))?
            || descriptor.staged_record_hash != production_hash_hex(record)
        {
            return Err(invalid_data(
                "batch append descriptor does not bind its record",
            ));
        }
        guard.verify()?;
        file.write_all(record)?;
        file.sync_all()?;
        guard.verify()?;
    }
    let final_len = file.metadata()?.len();
    if descriptors
        .last()
        .is_none_or(|descriptor| descriptor.post_segment_length != final_len)
    {
        return Err(invalid_data("batch CAS append final length is invalid"));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct ProductionBatchResourcePlanV1 {
    required_additional_free_space: u64,
    after_record_staging_required: u64,
    before_commit_required: u64,
}

fn production_checked_sum<'a>(values: impl IntoIterator<Item = &'a u64>) -> io::Result<u64> {
    values.into_iter().try_fold(0u64, |total, value| {
        total
            .checked_add(*value)
            .ok_or_else(|| invalid_data("batch resource sum overflow"))
    })
}

#[allow(clippy::too_many_arguments)]
fn production_validate_batch_resource_plan(
    root: &Path,
    transaction_id: &str,
    generation: u64,
    records: &[Vec<u8>],
    append_descriptors: &[BatchCasAppendDescriptorV1],
    replacements: &[(&str, &[u8])],
    prepare_bytes: &[u8],
    manifest_bytes: &[u8],
    component_count: usize,
    pair_count: usize,
) -> io::Result<ProductionBatchResourcePlanV1> {
    let created = records.len();
    if created == 0
        || created > PRODUCTION_BATCH_MAX_ITEMS
        || component_count != 17 + 2 * created
        || pair_count != created + 2
    {
        return Err(invalid_data("batch resource topology is inconsistent"));
    }
    let record_lengths = records
        .iter()
        .map(|record| record.len() as u64)
        .collect::<Vec<_>>();
    let total_append_extent = production_checked_sum(&record_lengths)?;
    if total_append_extent > PRODUCTION_BATCH_MAX_APPEND_EXTENT_BYTES {
        return Err(invalid_data("batch append extent exceeds its limit"));
    }
    if replacements.len() != 8 {
        return Err(invalid_data("batch detached component count is invalid"));
    }
    let detached_lengths = replacements
        .iter()
        .map(|(_, bytes)| bytes.len() as u64)
        .collect::<Vec<_>>();
    let detached_total = production_checked_sum(&detached_lengths)?;
    let max_install_scratch = detached_lengths.iter().copied().max().unwrap_or(0);
    if detached_total > PRODUCTION_BATCH_MAX_DETACHED_BYTES
        || max_install_scratch > PRODUCTION_BATCH_MAX_INSTALL_SCRATCH_BYTES
    {
        return Err(invalid_data("batch detached component bound exceeded"));
    }
    let mut control_lengths = append_descriptors
        .iter()
        .map(|descriptor| descriptor.canonical_bytes().map(|bytes| bytes.len() as u64))
        .collect::<io::Result<Vec<_>>>()?;
    control_lengths.push(prepare_bytes.len() as u64);
    control_lengths.push(manifest_bytes.len() as u64);
    if control_lengths.len() > 64
        || control_lengths
            .iter()
            .any(|length| *length > MAX_CONTROL_RECORD_BYTES)
    {
        return Err(invalid_data("batch control record bound exceeded"));
    }
    let control_total = production_checked_sum(&control_lengths)?;
    if control_total > PRODUCTION_BATCH_MAX_CONTROL_BYTES {
        return Err(invalid_data("batch control aggregate bound exceeded"));
    }
    let staged_generation = total_append_extent
        .checked_add(detached_total)
        .and_then(|value| value.checked_add(control_total))
        .ok_or_else(|| invalid_data("batch staged generation size overflow"))?;
    if staged_generation > PRODUCTION_BATCH_MAX_STAGED_BYTES {
        return Err(invalid_data("batch staged generation exceeds its limit"));
    }
    let required_additional_free_space = total_append_extent
        .checked_add(total_append_extent)
        .and_then(|value| value.checked_add(detached_total))
        .and_then(|value| value.checked_add(control_total))
        .and_then(|value| value.checked_add(max_install_scratch))
        .and_then(|value| value.checked_add(PRODUCTION_BATCH_MINIMUM_FREE_RESERVE_BYTES))
        .ok_or_else(|| invalid_data("batch free-space requirement overflow"))?;
    if required_additional_free_space > PRODUCTION_BATCH_REQUIRED_FREE_BYTES
        || fs2::available_space(root)? < required_additional_free_space
    {
        return Err(invalid_data("batch free-space preflight failed"));
    }

    let mut paths = vec![
        "components".to_owned(),
        "components/cas".to_owned(),
        "components/index".to_owned(),
        "components/graph".to_owned(),
        "components/meta".to_owned(),
        PREPARE_FILE_NAME.to_owned(),
        COMMIT_FILE_NAME.to_owned(),
        format!(".pending-{transaction_id}"),
        format!("{generation:020}"),
    ];
    for ordinal in 0..created {
        paths.push(format!("components/cas/staged_{ordinal:05}.skf1"));
        paths.push(format!("components/cas/orphan_{ordinal:05}.json"));
    }
    for (stage, _) in replacements {
        paths.push((*stage).to_owned());
    }
    for target in [
        "cas/seg_00000.idx",
        "index/location_state.bin",
        "index/idloc.mmap",
        "index/terms.lex",
        "index/terms.post",
        "graph/graph.manifest",
        "meta/meta_state.bin",
        "meta/history.log",
    ] {
        paths.push(target.to_owned());
    }
    let total_path_bytes = paths.iter().try_fold(0usize, |total, path| {
        if !path.is_ascii() || path.len() > PRODUCTION_BATCH_MAX_PATH_BYTES {
            return Err(invalid_data("batch path is outside its bound"));
        }
        total
            .checked_add(path.len())
            .ok_or_else(|| invalid_data("batch path byte sum overflow"))
    })?;
    if paths.len() != 25 + 2 * created
        || paths.len() > 57
        || total_path_bytes > PRODUCTION_BATCH_MAX_TOTAL_PATH_BYTES
    {
        return Err(invalid_data("batch path inventory is outside its bound"));
    }
    let prepare_and_manifest = (prepare_bytes.len() as u64)
        .checked_add(manifest_bytes.len() as u64)
        .ok_or_else(|| invalid_data("batch terminal control size overflow"))?;
    let after_record_staging_required = total_append_extent
        .checked_add(detached_total)
        .and_then(|value| value.checked_add(prepare_and_manifest))
        .and_then(|value| value.checked_add(max_install_scratch))
        .and_then(|value| value.checked_add(PRODUCTION_BATCH_MINIMUM_FREE_RESERVE_BYTES))
        .ok_or_else(|| invalid_data("batch post-staging requirement overflow"))?;
    let before_commit_required = (manifest_bytes.len() as u64)
        .checked_add(max_install_scratch)
        .and_then(|value| value.checked_add(PRODUCTION_BATCH_MINIMUM_FREE_RESERVE_BYTES))
        .ok_or_else(|| invalid_data("batch pre-commit requirement overflow"))?;
    Ok(ProductionBatchResourcePlanV1 {
        required_additional_free_space,
        after_record_staging_required,
        before_commit_required,
    })
}

fn production_stage_batch_ingest(
    token: &BorrowedOwnerQuiescence<'_, QuiescentWrite>,
    state: &ProductionRuntimeStateV1,
    intent: &ProductionBatchIntentV1,
    envelope: &ProductionBatchEnvelopeV1,
    plan: &ProductionBatchPlanV1,
) -> io::Result<PathBuf> {
    token.verify()?;
    if plan.created_ordinals.is_empty() {
        return Err(invalid_data("unchanged batch must not enter staging"));
    }
    let root = token.canonical_root();
    let generation = state
        .head
        .generation
        .checked_add(1)
        .ok_or_else(|| invalid_data("batch generation overflow"))?;
    if generation > PRODUCTION_MAX_GENERATIONS {
        return Err(invalid_data("batch generation limit reached"));
    }
    let generations = production_txn_root(root).join(GENERATIONS_DIR_NAME);
    let pending = generations.join(format!(".pending-{}", envelope.transaction_id));
    let published = production_generation_path(root, generation);
    require_path_entry_absent(&pending, "batch pending generation")?;
    require_path_entry_absent(&published, "batch published generation")?;

    let segment_path = root.join("cas/seg_00000.dat");
    let (parent_segment_length, parent_segment_hash, parent_segment_identity) = hash_verified_file(
        root,
        &segment_path,
        ProductionStorageLimitsV1::frozen().max_component_bytes,
    )?;
    require_single_link(
        &parent_segment_identity,
        "production batch parent CAS segment",
    )?;
    let mut records = Vec::with_capacity(plan.created_ordinals.len());
    let mut created_atoms = Vec::with_capacity(plan.created_ordinals.len());
    let mut cursor = parent_segment_length;
    for ordinal in &plan.created_ordinals {
        let item = &intent.items[*ordinal];
        let record = production_record_bytes(item.atom_id, &item.body, 0)?;
        let outcome = &plan.outcomes[*ordinal];
        let _header = AtomBodyHeader::from_bytes(&item.body)
            .map_err(|error| invalid_data(&format!("batch atom body is invalid: {error}")))?;
        let atom = ProductionAtomStateV1 {
            atom_id: item.atom_id,
            atom_type: item.atom_type,
            node_num: outcome
                .node_num
                .ok_or_else(|| invalid_data("created batch item has no NodeNum"))?,
            committed_generation: generation,
            body_len: item.body_len,
            body_crc32: crc32(&item.body),
            body_hash: item.body_hash,
            segment_id: 0,
            record_offset: cursor,
            record_extent_len: record.len() as u64,
            domain_mask: 0xffff,
            created_at_ns: envelope.semantic_time_unix_ns,
            trust_level: 5000,
            source_id: 0,
            provenance_hash: production_hash_bytes(&production_zero_provenance_leaf(&item.atom_id)),
            history_event_id: [0; 32],
            history_leaf: Vec::new(),
        };
        cursor = cursor
            .checked_add(record.len() as u64)
            .ok_or_else(|| invalid_data("batch CAS cursor overflow"))?;
        records.push(record);
        created_atoms.push(atom);
    }
    let mut post_atoms = state.atoms.clone();
    post_atoms.extend(created_atoms.clone());
    let index_bytes = production_idx1_bytes_many(&post_atoms)?;
    let location_bytes = production_loc1_bytes_many(&post_atoms)?;
    let idloc_bytes = production_idloc_bytes_many(&post_atoms);
    let (lexicon_bytes, postings_bytes) = production_empty_lexical_pair()?;
    let next_node = post_atoms
        .iter()
        .map(|atom| atom.node_num)
        .max()
        .map_or(0, |node| node.saturating_add(1));
    let graph_bytes = production_graph_manifest_bytes(next_node)?;
    let metadata_bytes = production_metadata_bytes_many(&post_atoms);
    let history = production_batch_history(generation, envelope, intent, plan)?;
    for atom in &mut created_atoms {
        atom.history_event_id = history.event_id;
    }
    let mut history_bytes = match fs::symlink_metadata(root.join("meta/history.log")) {
        Ok(_) => read_bytes_bounded_under(root, &root.join("meta/history.log"), 67_108_864)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    history_bytes.extend_from_slice(&history.line_bytes);

    let mut append_descriptors = Vec::new();
    let mut pre = parent_segment_length;
    for (created_ordinal, (item_ordinal, record)) in
        plan.created_ordinals.iter().zip(&records).enumerate()
    {
        let atom = &created_atoms[created_ordinal];
        let mut idx_entry = [0u8; IndexEntry::SIZE];
        IndexEntry::new(atom.atom_id, atom.record_offset, atom.body_len, 0)
            .write_to_bytes(&mut idx_entry)
            .map_err(|error| invalid_data(&format!("batch index entry failed: {error}")))?;
        let post = pre
            .checked_add(record.len() as u64)
            .ok_or_else(|| invalid_data("batch append descriptor overflow"))?;
        append_descriptors.push(BatchCasAppendDescriptorV1::from_body(
            BatchCasAppendDescriptorBodyV1 {
                schema: "memoryx.batch-cas-append-descriptor.v1".to_owned(),
                version: 1,
                segment_id: 0,
                ordinal: created_ordinal as u32,
                item_ordinal: *item_ordinal as u32,
                atom_id: hex_lower(&atom.atom_id),
                body_length: atom.body_len,
                body_crc32: format!("{:08x}", atom.body_crc32),
                body_hash: hex_lower(&atom.body_hash),
                record_offset: pre,
                record_extent_length: record.len() as u64,
                pre_segment_length: pre,
                post_segment_length: post,
                staged_record_hash: production_hash_hex(record),
                idx_entry_hash: production_hash_hex(&idx_entry),
            },
        )?);
        pre = post;
    }

    let replacements = [
        ("components/cas/seg_00000.idx", index_bytes.as_slice()),
        (
            "components/index/location_state.bin",
            location_bytes.as_slice(),
        ),
        ("components/index/idloc.mmap", idloc_bytes.as_slice()),
        ("components/index/terms.lex", lexicon_bytes.as_slice()),
        ("components/index/terms.post", postings_bytes.as_slice()),
        ("components/graph/graph.manifest", graph_bytes.as_slice()),
        ("components/meta/meta_state.bin", metadata_bytes.as_slice()),
        ("components/meta/history.log", history_bytes.as_slice()),
    ];
    let mut components = Vec::new();
    components.push(production_batch_component_from_digest(
        "cas.segment-data.skf1.v1",
        5,
        0,
        "anchor_present",
        Some("cas/seg_00000.dat"),
        None,
        "memoryx.skf1.v0101",
        parent_segment_length,
        parent_segment_hash,
        state.atoms.len() as u64,
        None,
    )?);
    for (ordinal, record) in records.iter().enumerate() {
        let pair = format!("memoryx.batch-cas-append-index.v1.{ordinal}");
        components.push(production_batch_component(
            "cas.staged-record.skf1.v1",
            10,
            ordinal as u32,
            "orphan",
            None,
            Some(format!("components/cas/staged_{ordinal:05}.skf1")),
            "memoryx.skf1.v0101",
            record,
            1,
            Some(pair.clone()),
        )?);
        let descriptor_bytes = append_descriptors[ordinal].canonical_bytes()?;
        components.push(production_batch_component(
            "cas.orphan-descriptor.v1",
            20,
            ordinal as u32,
            "orphan",
            None,
            Some(format!("components/cas/orphan_{ordinal:05}.json")),
            "memoryx.batch-cas-append-descriptor.v1",
            &descriptor_bytes,
            1,
            Some(pair),
        )?);
    }
    let post_count = post_atoms.len() as u64;
    #[allow(clippy::type_complexity)]
    let replacement_specs: [(&str, u16, &str, &str, &[u8], u64, Option<&str>); 8] = [
        (
            "cas.segment-index.idx1.v1",
            30,
            "cas/seg_00000.idx",
            "memoryx.idx1.v0101",
            &index_bytes,
            post_count,
            None,
        ),
        (
            "index.location-state.loc1.v1",
            40,
            "index/location_state.bin",
            "memoryx.loc1.v0001",
            &location_bytes,
            post_count,
            Some("memoryx.location-idloc.v1"),
        ),
        (
            "index.idloc.idl1.v1",
            50,
            "index/idloc.mmap",
            "memoryx.idl1.v0001",
            &idloc_bytes,
            post_count,
            Some("memoryx.location-idloc.v1"),
        ),
        (
            "index.lexicon.lex1.v1",
            60,
            "index/terms.lex",
            "memoryx.lex1.implemented-v0001",
            &lexicon_bytes,
            0,
            Some("memoryx.lexical.v1"),
        ),
        (
            "index.postings.pst1.v1",
            70,
            "index/terms.post",
            "memoryx.pst1.implemented-v0001",
            &postings_bytes,
            0,
            Some("memoryx.lexical.v1"),
        ),
        (
            "graph.manifest.v1",
            90,
            "graph/graph.manifest",
            "memoryx.graph-manifest.grm1-v0101",
            &graph_bytes,
            1,
            None,
        ),
        (
            "meta.atom-state.met1.v1",
            130,
            "meta/meta_state.bin",
            "memoryx.met1.v1",
            &metadata_bytes,
            post_count,
            None,
        ),
        (
            "meta.history-once.v1",
            140,
            "meta/history.log",
            "memoryx.history.batch-transaction-once.v1",
            &history_bytes,
            state.history_leaves.len() as u64 + 1,
            None,
        ),
    ];
    for (key, order, target, codec, bytes, count, pair) in replacement_specs {
        components.push(production_batch_component(
            key,
            order,
            0,
            "replace",
            Some(target),
            Some(format!("components/{target}")),
            codec,
            bytes,
            count,
            pair.map(ToOwned::to_owned),
        )?);
    }
    for entry in PRODUCTION_DIRECT_REGISTRY
        .iter()
        .filter(|entry| (150..=220).contains(&entry.order))
    {
        let target = entry.target.expect("anchor target");
        components.push(production_batch_component(
            entry.key,
            entry.order,
            0,
            "anchor_absent",
            Some(target),
            None,
            "memoryx.raw-anchor.v1",
            &[],
            0,
            None,
        )?);
    }
    components.sort_by_key(|component| (component.registry_order, component.ordinal));

    let mut pairs = Vec::new();
    for (ordinal, append) in append_descriptors.iter().enumerate() {
        let mut root_bytes = append.canonical_bytes()?;
        root_bytes.extend_from_slice(&index_bytes);
        pairs.push(production_batch_pair(
            format!("memoryx.batch-cas-append-index.v1.{ordinal}"),
            "cas.orphan-descriptor.v1",
            ordinal as u32,
            "cas.segment-index.idx1.v1",
            0,
            1,
            post_count,
            1,
            plan.created_ordinals.len() as u64,
            &root_bytes,
        )?);
    }
    let mut lexical_root = lexicon_bytes.clone();
    lexical_root.extend_from_slice(&postings_bytes);
    pairs.push(production_batch_pair(
        "memoryx.lexical.v1".to_owned(),
        "index.lexicon.lex1.v1",
        0,
        "index.postings.pst1.v1",
        0,
        0,
        0,
        0,
        0,
        &lexical_root,
    )?);
    let mut location_root = location_bytes.clone();
    location_root.extend_from_slice(&idloc_bytes);
    pairs.push(production_batch_pair(
        "memoryx.location-idloc.v1".to_owned(),
        "index.location-state.loc1.v1",
        0,
        "index.idloc.idl1.v1",
        0,
        post_count,
        post_count,
        post_count,
        0,
        &location_root,
    )?);

    let mut post_history = state.history_leaves.clone();
    post_history.push(history.leaf_bytes.clone());
    let atom_refs = post_atoms.iter().collect::<Vec<_>>();
    let history_refs = post_history.iter().map(Vec::as_slice).collect::<Vec<_>>();
    let logical_digest = production_logical_digest_multi(
        generation,
        state.head.commit_hash,
        &atom_refs,
        &history_refs,
        &production_anchor_leaves(root)?,
    )?;
    let component_root = production_component_root(generation, &components, &pairs)?;
    let orphan_digest = production_batch_orphan_digest(&append_descriptors)?;
    let decision_hashes = plan
        .outcomes
        .iter()
        .map(|outcome| hex_lower(&outcome.decision_hash))
        .collect::<Vec<_>>();
    let prepare = BatchPrepareV1::from_body(BatchPrepareBodyV1 {
        schema: "memoryx.batch-prepare.v1".to_owned(),
        version: 1,
        format_version: 2,
        generation,
        parent_commit_hash: hex_lower(&state.head.commit_hash),
        transaction_id: envelope.transaction_id.clone(),
        semantic_time_unix_ns: envelope.semantic_time_unix_ns,
        base_binding_hash: hex_lower(&intent.base_binding_hash),
        envelope_hash: hex_lower(&envelope.hash),
        operation: "batch_ingest".to_owned(),
        intent_hash: hex_lower(&intent.hash),
        codec_id: PRODUCTION_CODEC_ID.to_owned(),
        registry_id: PRODUCTION_REGISTRY_ID.to_owned(),
        operation_registry_id: PRODUCTION_BATCH_OPERATION_REGISTRY_ID.to_owned(),
        digest_id: PRODUCTION_DIGEST_ID.to_owned(),
        limits_id: PRODUCTION_BATCH_LIMITS_ID.to_owned(),
        preflight_hash: production_hash_hex(&plan.preflight_bytes),
        decision_hashes: decision_hashes.clone(),
        components: components.clone(),
        pairs: pairs.clone(),
        component_root_hash: hex_lower(&component_root),
        logical_state_digest: hex_lower(&logical_digest),
        orphan_inventory_digest: hex_lower(&orphan_digest),
        history_event_hash: hex_lower(&history.semantic_hash),
        history_event_count: state.history_leaves.len() as u64 + 1,
        post_atom_count: post_count,
    })?;
    let prepare_bytes = prepare.canonical_bytes()?;
    let manifest = BatchGenerationManifestV1::from_body(BatchGenerationManifestBodyV1 {
        schema: "memoryx.batch-generation-manifest.v1".to_owned(),
        version: 1,
        format_version: 2,
        generation,
        parent_commit_hash: hex_lower(&state.head.commit_hash),
        prepare_hash: production_hash_hex(&prepare_bytes),
        transaction_id: envelope.transaction_id.clone(),
        semantic_time_unix_ns: envelope.semantic_time_unix_ns,
        base_binding_hash: hex_lower(&intent.base_binding_hash),
        envelope_hash: hex_lower(&envelope.hash),
        operation: "batch_ingest".to_owned(),
        intent_hash: hex_lower(&intent.hash),
        codec_id: PRODUCTION_CODEC_ID.to_owned(),
        registry_id: PRODUCTION_REGISTRY_ID.to_owned(),
        operation_registry_id: PRODUCTION_BATCH_OPERATION_REGISTRY_ID.to_owned(),
        digest_id: PRODUCTION_DIGEST_ID.to_owned(),
        limits_id: PRODUCTION_BATCH_LIMITS_ID.to_owned(),
        decision_hashes,
        components,
        pairs,
        component_root_hash: hex_lower(&component_root),
        logical_state_digest: hex_lower(&logical_digest),
        orphan_inventory_digest: hex_lower(&orphan_digest),
        history_event_hash: hex_lower(&history.semantic_hash),
        history_event_count: state.history_leaves.len() as u64 + 1,
        post_atom_count: post_count,
    })?;
    let manifest_bytes = manifest.canonical_bytes()?;
    let resources = production_validate_batch_resource_plan(
        root,
        &envelope.transaction_id,
        generation,
        &records,
        &append_descriptors,
        &replacements,
        &prepare_bytes,
        &manifest_bytes,
        manifest.components.len(),
        manifest.pairs.len(),
    )?;
    debug_assert!(resources.required_additional_free_space > 0);

    // The complete immutable generation, resource arithmetic, and every
    // input/decision identity have now been built and checked in memory. This
    // directory creation is the first persistent side effect.
    fs::create_dir(&pending)?;
    for relative in [
        "components",
        "components/cas",
        "components/index",
        "components/graph",
        "components/meta",
    ] {
        fs::create_dir(pending.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR)))?;
    }
    for (ordinal, record) in records.iter().enumerate() {
        production_write_batch_stage(
            &pending,
            &format!("components/cas/staged_{ordinal:05}.skf1"),
            record,
        )?;
        production_write_batch_stage(
            &pending,
            &format!("components/cas/orphan_{ordinal:05}.json"),
            &append_descriptors[ordinal].canonical_bytes()?,
        )?;
    }
    sync_directory(&pending.join("components/cas"))?;
    if fs2::available_space(root)? < resources.after_record_staging_required {
        return Err(invalid_data(
            "batch free space changed after record staging",
        ));
    }
    production_append_batch_cas(root, &records, &append_descriptors)?;
    for (path, bytes) in replacements {
        production_write_batch_stage(&pending, path, bytes)?;
    }
    for relative in [
        "components/cas",
        "components/index",
        "components/graph",
        "components/meta",
        "components",
    ] {
        sync_directory(&pending.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR)))?;
    }
    production_write_new(&pending.join(PREPARE_FILE_NAME), &prepare_bytes)?;
    sync_directory(&pending)?;
    if fs2::available_space(root)? < resources.before_commit_required {
        return Err(invalid_data("batch free space changed before commit"));
    }
    production_write_new(&pending.join(COMMIT_FILE_NAME), &manifest_bytes)?;
    sync_directory(&pending)?;
    token.verify()?;
    move_directory_write_through(&pending, &published)?;
    Ok(published)
}

fn production_batch_orphan_digest(
    descriptors: &[BatchCasAppendDescriptorV1],
) -> io::Result<[u8; 32]> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(PRODUCTION_ORPHAN_DIGEST_ID.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&(descriptors.len() as u64).to_le_bytes());
    for descriptor in descriptors {
        production_append_u32_frame(&mut bytes, &descriptor.canonical_bytes()?)?;
    }
    Ok(production_hash_bytes(&bytes))
}

fn production_stage_direct_ingest(
    token: &BorrowedOwnerQuiescence<'_, QuiescentWrite>,
    state: &ProductionRuntimeStateV1,
    request: &ProductionDirectRequestV1,
    intent: &ProductionDirectIntentV1,
    envelope: &ProductionDirectEnvelopeV1,
) -> io::Result<PathBuf> {
    token.verify()?;
    let root = token.canonical_root();
    // All persistent-state validation must finish before `.pending-*` exists.
    let segment_bytes = production_preflight_direct_ingest(root)?;
    let generation = state
        .head
        .generation
        .checked_add(1)
        .ok_or_else(|| invalid_data("production generation overflow"))?;
    if generation != 1 || state.atom.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "composite_operation_not_admitted: P0-C permits one new atom only",
        ));
    }
    let generations = production_txn_root(root).join(GENERATIONS_DIR_NAME);
    let pending = generations.join(format!(".pending-{}", envelope.transaction_id));
    let published = production_generation_path(root, generation);
    if pending.exists() || published.exists() {
        return Err(invalid_data(
            "production transaction namespace already exists",
        ));
    }
    fs::create_dir(&pending)?;
    fs::create_dir(pending.join(COMPONENTS_DIR_NAME))?;

    let planned = production_planned_components(root)?;
    let prepare = ProductionPrepareRecordV1::from_body(ProductionPrepareBodyV1 {
        schema: "memoryx.production-prepare.v1".to_owned(),
        version: 1,
        format_version: 2,
        generation,
        parent_commit_hash: hex_lower(&state.head.commit_hash),
        transaction_id: envelope.transaction_id.clone(),
        semantic_time_unix_ns: envelope.semantic_time_unix_ns,
        base_binding_hash: hex_lower(&intent.base_binding_hash),
        envelope_hash: hex_lower(&envelope.hash()),
        operation: "direct_ingest".to_owned(),
        intent_hash: hex_lower(&intent.hash()),
        codec_id: PRODUCTION_CODEC_ID.to_owned(),
        registry_id: PRODUCTION_REGISTRY_ID.to_owned(),
        digest_id: PRODUCTION_DIGEST_ID.to_owned(),
        limits_id: PRODUCTION_LIMITS_ID.to_owned(),
        limits: ProductionStorageLimitsV1::frozen(),
        planned_components: planned,
    })?;
    let prepare_bytes = prepare.canonical_bytes()?;
    production_write_new(&pending.join(PREPARE_FILE_NAME), &prepare_bytes)?;

    let detached = production_detached_components(request, envelope, generation, 0)?;

    let mut staged: Vec<(&'static str, Vec<u8>, u64)> = vec![
        (
            "cas.staged-record.skf1.v1",
            detached.staged_record.clone(),
            1,
        ),
        (
            "cas.segment-index.idx1.v1",
            detached.segment_index.clone(),
            1,
        ),
        (
            "index.location-state.loc1.v1",
            detached.location_state.clone(),
            1,
        ),
        ("index.idloc.idl1.v1", detached.idloc.clone(), 1),
        ("index.lexicon.lex1.v1", detached.lexicon.clone(), 0),
        ("index.postings.pst1.v1", detached.postings.clone(), 0),
        ("graph.manifest.v1", detached.graph_manifest.clone(), 1),
        ("meta.atom-state.met1.v1", detached.metadata.clone(), 1),
        ("meta.history-once.v1", detached.history.clone(), 1),
    ];
    let staged_record_hash = production_hash_hex(&detached.staged_record);
    let index_hash = production_hash_hex(&detached.segment_index);
    let root_identity_hash = {
        let mut bytes = token.physical_identity().canonical_root_key.clone();
        bytes.extend_from_slice(&token.physical_identity().stable_root_identity);
        production_hash_hex(&bytes)
    };
    let fp_bits = u64::from_le_bytes(detached.atom.atom_id[0..8].try_into().unwrap());
    let orphan = CasOrphanDescriptorV1::from_body(CasOrphanDescriptorBodyV1 {
        schema: "memoryx.cas-orphan-descriptor.v1".to_owned(),
        version: 1,
        transaction_id: envelope.transaction_id.clone(),
        record_ordinal: 0,
        atom_id: hex_lower(&detached.atom.atom_id),
        body_len: detached.atom.body_len,
        body_crc32: format!("{:08x}", detached.atom.body_crc32),
        body_hash: hex_lower(&detached.atom.body_hash),
        record_len: detached.staged_record.len() as u64,
        record_hash: staged_record_hash.clone(),
        staged_component_key: "cas.staged-record.skf1.v1".to_owned(),
        staged_component_hash: staged_record_hash,
        segment_id: 0,
        segment_existed: true,
        segment_pre_len: 0,
        record_offset: 0,
        record_extent_len: detached.staged_record.len() as u64,
        segment_post_len: detached.staged_record.len() as u64,
        record_flags: 0,
        idx_fp64_bits: format!("{fp_bits:016x}"),
        idx_seg_offset: 0,
        idx_body_len: detached.atom.body_len as u32,
        idx_flags: 0,
        post_index_component_key: "cas.segment-index.idx1.v1".to_owned(),
        post_index_component_hash: index_hash,
        parent_generation: state.head.generation,
        parent_commit_hash: hex_lower(&state.head.commit_hash),
        pinned_physical_root: root_identity_hash,
    })?;
    staged.push(("cas.orphan-descriptor.v1", orphan.canonical_bytes()?, 1));
    staged.sort_by_key(|(key, _, _)| {
        production_registry_entry(key)
            .map(|entry| entry.order)
            .unwrap_or(u16::MAX)
    });

    for required_key in ["cas.staged-record.skf1.v1", "cas.orphan-descriptor.v1"] {
        let (key, bytes, _) = staged
            .iter()
            .find(|(key, _, _)| *key == required_key)
            .ok_or_else(|| invalid_data("pre-commit CAS artifact is missing"))?;
        let entry = production_registry_entry(key)?;
        production_write_new(&pending.join(production_stage_path(entry.order, 0)), bytes)?;
    }
    sync_directory(&pending.join(COMPONENTS_DIR_NAME))?;
    production_append_precommit_cas(root, &detached.staged_record, &orphan)?;

    let mut components = Vec::new();
    components.push(production_component_descriptor(
        production_registry_entry("cas.segment-data.skf1.v1")?,
        0,
        "anchor_present",
        Some("cas/seg_00000.dat".to_owned()),
        None,
        &segment_bytes,
        0,
    )?);
    for (key, bytes, record_count) in &staged {
        let entry = production_registry_entry(key)?;
        let stage_path = production_stage_path(entry.order, 0);
        if !matches!(
            *key,
            "cas.staged-record.skf1.v1" | "cas.orphan-descriptor.v1"
        ) {
            production_write_new(&pending.join(&stage_path), bytes)?;
        }
        components.push(production_component_descriptor(
            entry,
            0,
            entry.mode,
            entry.target.map(ToOwned::to_owned),
            Some(stage_path),
            bytes,
            *record_count,
        )?);
    }
    sync_directory(&pending.join(COMPONENTS_DIR_NAME))?;
    for entry in PRODUCTION_DIRECT_REGISTRY
        .iter()
        .filter(|entry| (150..=220).contains(&entry.order))
    {
        let relative = entry.target.expect("anchor target");
        let target = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        let (mode, bytes) = match fs::read(&target) {
            Ok(bytes) => ("anchor_present", bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => ("anchor_absent", Vec::new()),
            Err(error) => return Err(error),
        };
        components.push(production_component_descriptor(
            entry,
            0,
            mode,
            Some(relative.to_owned()),
            None,
            &bytes,
            u64::from(mode == "anchor_present"),
        )?);
    }
    components.sort_by(|left, right| {
        (left.registry_order, &left.target_path, left.ordinal).cmp(&(
            right.registry_order,
            &right.target_path,
            right.ordinal,
        ))
    });
    let by_key = |key: &str| {
        components
            .iter()
            .find(|component| component.registry_key == key)
            .ok_or_else(|| invalid_data("production pair member is missing"))
    };
    let pairs = vec![
        production_pair_descriptor(
            "memoryx.location-idloc-pair.v1",
            by_key("index.location-state.loc1.v1")?,
            by_key("index.idloc.idl1.v1")?,
            1,
            0,
        )?,
        production_pair_descriptor(
            "memoryx.lexical-postings-pair.v1",
            by_key("index.lexicon.lex1.v1")?,
            by_key("index.postings.pst1.v1")?,
            0,
            0,
        )?,
    ];
    let logical_digest = production_logical_digest(
        generation,
        state.head.commit_hash,
        Some(&detached.atom),
        &production_anchor_leaves(root)?,
    )?;
    let manifest = ProductionGenerationManifestV1::from_body(ProductionGenerationManifestBodyV1 {
        schema: "memoryx.production-generation-manifest.v1".to_owned(),
        version: 1,
        format_version: 2,
        generation,
        parent_commit_hash: hex_lower(&state.head.commit_hash),
        prepare_hash: production_hash_hex(&prepare_bytes),
        transaction_id: envelope.transaction_id.clone(),
        semantic_time_unix_ns: envelope.semantic_time_unix_ns,
        base_binding_hash: hex_lower(&intent.base_binding_hash),
        envelope_hash: hex_lower(&envelope.hash()),
        operation: "direct_ingest".to_owned(),
        intent_hash: hex_lower(&intent.hash()),
        codec_id: PRODUCTION_CODEC_ID.to_owned(),
        registry_id: PRODUCTION_REGISTRY_ID.to_owned(),
        digest_id: PRODUCTION_DIGEST_ID.to_owned(),
        limits_id: PRODUCTION_LIMITS_ID.to_owned(),
        component_root_hash: hex_lower(&production_component_root(
            generation,
            &components,
            &pairs,
        )?),
        logical_state_digest: hex_lower(&logical_digest),
        orphan_inventory_digest: hex_lower(&production_orphan_inventory_digest(&[orphan])?),
        history_event_hash: hex_lower(
            &ProductionHistoryEventV1::create(
                envelope,
                generation,
                detached.atom.atom_id,
                detached.atom.atom_type.to_u32(),
                intent.claim_count,
                intent.evidence_ref_count,
            )?
            .event_semantic_hash,
        ),
        history_event_count: 1,
        components,
        pairs,
    })?;
    production_write_new(
        &pending.join(COMMIT_FILE_NAME),
        &manifest.canonical_bytes()?,
    )?;
    token.verify()?;
    move_directory_write_through(&pending, &published)?;
    Ok(published)
}

#[derive(Debug, Clone)]
struct ProductionUpdateHistoryV1 {
    event_id: [u8; 32],
    semantic_hash: [u8; 32],
    leaf_bytes: Vec<u8>,
    record_bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProductionUpdateHistoryIdentityV1 {
    transaction_uuid: [u8; 16],
    event_id: [u8; 32],
    semantic_hash: [u8; 32],
    successor_atom_id: AtomId,
    old_atom_id: AtomId,
    relation_id: [u8; 32],
    intent_hash: [u8; 32],
    successor_provenance_hash: [u8; 32],
    old_provenance_hash: [u8; 32],
}

fn production_update_history_event_id(transaction_uuid: [u8; 16], event_ordinal: u32) -> [u8; 32] {
    let mut preimage = Vec::new();
    preimage.extend_from_slice(PRODUCTION_UPDATE_HISTORY_EVENT_ID.as_bytes());
    preimage.push(0);
    preimage.extend_from_slice(&transaction_uuid);
    preimage.extend_from_slice(&event_ordinal.to_le_bytes());
    production_hash_bytes(&preimage)
}

#[allow(clippy::too_many_arguments)]
fn production_update_history_semantic_hash(
    transaction_uuid: [u8; 16],
    event_ordinal: u32,
    generation: u64,
    semantic_time_unix_ns: u64,
    successor: AtomId,
    old: AtomId,
    relation_id: [u8; 32],
    intent_hash: [u8; 32],
    successor_provenance_hash: [u8; 32],
    old_provenance_hash: [u8; 32],
) -> [u8; 32] {
    let mut preimage = Vec::new();
    preimage.extend_from_slice(PRODUCTION_UPDATE_HISTORY_SEMANTIC_ID.as_bytes());
    preimage.push(0);
    preimage.extend_from_slice(&1u16.to_le_bytes());
    preimage.extend_from_slice(&transaction_uuid);
    preimage.extend_from_slice(&event_ordinal.to_le_bytes());
    preimage.extend_from_slice(&generation.to_le_bytes());
    preimage.extend_from_slice(&semantic_time_unix_ns.to_le_bytes());
    preimage.extend_from_slice(&3u16.to_le_bytes());
    preimage.push(1);
    preimage.extend_from_slice(&successor);
    preimage.extend_from_slice(&old);
    preimage.extend_from_slice(&relation_id);
    preimage.extend_from_slice(&intent_hash);
    preimage.extend_from_slice(&successor_provenance_hash);
    preimage.extend_from_slice(&old_provenance_hash);
    production_hash_bytes(&preimage)
}

#[derive(Debug, Clone)]
struct ProductionUpdatePlanV1 {
    generation: u64,
    old_atom: ProductionAtomStateV1,
    successor_atom: ProductionAtomStateV1,
    relation_id: [u8; 32],
    history: ProductionUpdateHistoryV1,
    graph_leaf: Vec<u8>,
    staged: Vec<(UpdateComponentDescriptorV1, Vec<u8>)>,
    component_root: [u8; 32],
    logical_digest: [u8; 32],
    prepare: UpdatePrepareV1,
    manifest: UpdateGenerationManifestV1,
}

fn production_update_descriptor_binary(
    descriptor: &UpdateComponentDescriptorV1,
) -> io::Result<Vec<u8>> {
    if descriptor.schema != PRODUCTION_UPDATE_DESCRIPTOR_SCHEMA
        || descriptor.version != 1
        || descriptor.ordinal != 0
        || !matches!(descriptor.mode.as_str(), "append" | "replace" | "create")
        || descriptor.byte_hash.len() != 64
        || descriptor.semantic_hash.len() != 64
        || !descriptor.target_path.is_ascii()
        || !descriptor.stage_path.is_ascii()
        || descriptor.target_path.len() > MAX_PATH_BYTES
        || descriptor.stage_path.len() > MAX_PATH_BYTES
        || descriptor.target_path.contains('\\')
        || descriptor.stage_path.contains('\\')
    {
        return Err(invalid_data("update component descriptor is noncanonical"));
    }
    for path in [&descriptor.target_path, &descriptor.stage_path] {
        let candidate = Path::new(path);
        if candidate.is_absolute()
            || candidate
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(invalid_data("update component path is noncanonical"));
        }
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(PRODUCTION_UPDATE_DESCRIPTOR_SCHEMA.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&descriptor.registry_order.to_le_bytes());
    append_u64_frame(&mut bytes, descriptor.registry_key.as_bytes())?;
    bytes.extend_from_slice(&descriptor.ordinal.to_le_bytes());
    bytes.push(match descriptor.mode.as_str() {
        "append" => 1,
        "replace" => 2,
        "create" => 3,
        _ => unreachable!(),
    });
    append_u64_frame(&mut bytes, descriptor.target_path.as_bytes())?;
    append_u64_frame(&mut bytes, descriptor.stage_path.as_bytes())?;
    append_u64_frame(&mut bytes, descriptor.content_codec_id.as_bytes())?;
    bytes.extend_from_slice(&descriptor.byte_length.to_le_bytes());
    bytes.extend_from_slice(&parse_hash_hex(
        &descriptor.byte_hash,
        "update component byte hash",
    )?);
    bytes.extend_from_slice(&parse_hash_hex(
        &descriptor.semantic_hash,
        "update component semantic hash",
    )?);
    let mut hash_preimage = Vec::new();
    hash_preimage.extend_from_slice(PRODUCTION_UPDATE_DESCRIPTOR_HASH_ID.as_bytes());
    hash_preimage.push(0);
    hash_preimage.extend_from_slice(&bytes);
    if descriptor.descriptor_hash != production_hash_hex(&hash_preimage) {
        return Err(invalid_data("update component descriptor hash is invalid"));
    }
    Ok(bytes)
}

#[allow(clippy::too_many_arguments)]
fn production_update_descriptor(
    order: u16,
    key: &str,
    mode: &str,
    target_path: String,
    codec: &str,
    bytes: &[u8],
    semantic_hash: [u8; 32],
) -> io::Result<UpdateComponentDescriptorV1> {
    let stage_path = production_stage_path(order, 0);
    let mut binary = Vec::new();
    binary.extend_from_slice(PRODUCTION_UPDATE_DESCRIPTOR_SCHEMA.as_bytes());
    binary.push(0);
    binary.extend_from_slice(&1u16.to_le_bytes());
    binary.extend_from_slice(&order.to_le_bytes());
    append_u64_frame(&mut binary, key.as_bytes())?;
    binary.extend_from_slice(&0u32.to_le_bytes());
    binary.push(match mode {
        "append" => 1,
        "replace" => 2,
        "create" => 3,
        _ => return Err(invalid_data("update descriptor mode is unsupported")),
    });
    append_u64_frame(&mut binary, target_path.as_bytes())?;
    append_u64_frame(&mut binary, stage_path.as_bytes())?;
    append_u64_frame(&mut binary, codec.as_bytes())?;
    binary.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    binary.extend_from_slice(production_hash_bytes(bytes).as_slice());
    binary.extend_from_slice(&semantic_hash);
    let mut descriptor_preimage = Vec::new();
    descriptor_preimage.extend_from_slice(PRODUCTION_UPDATE_DESCRIPTOR_HASH_ID.as_bytes());
    descriptor_preimage.push(0);
    descriptor_preimage.extend_from_slice(&binary);
    let descriptor = UpdateComponentDescriptorV1::from_body(UpdateComponentDescriptorBodyV1 {
        schema: PRODUCTION_UPDATE_DESCRIPTOR_SCHEMA.to_owned(),
        version: 1,
        registry_order: order,
        registry_key: key.to_owned(),
        ordinal: 0,
        mode: mode.to_owned(),
        target_path,
        stage_path,
        content_codec_id: codec.to_owned(),
        byte_length: bytes.len() as u64,
        byte_hash: production_hash_hex(bytes),
        semantic_hash: hex_lower(&semantic_hash),
        descriptor_hash: production_hash_hex(&descriptor_preimage),
    })?;
    if production_update_descriptor_binary(&descriptor)? != binary {
        return Err(invalid_data("update descriptor binary is not stable"));
    }
    Ok(descriptor)
}

fn production_update_component_root(
    descriptors: &[UpdateComponentDescriptorV1],
) -> io::Result<[u8; 32]> {
    if descriptors.len() != PRODUCTION_UPDATE_COMPONENT_COUNT {
        return Err(invalid_data("update component count is invalid"));
    }
    let mut preimage = Vec::new();
    preimage.extend_from_slice(PRODUCTION_UPDATE_COMPONENT_ROOT_ID.as_bytes());
    preimage.push(0);
    preimage.extend_from_slice(&1u16.to_le_bytes());
    preimage.extend_from_slice(&(descriptors.len() as u32).to_le_bytes());
    let mut previous = None;
    for descriptor in descriptors {
        if previous.is_some_and(|order| order >= descriptor.registry_order) {
            return Err(invalid_data("update component order is noncanonical"));
        }
        previous = Some(descriptor.registry_order);
        production_append_u32_frame(
            &mut preimage,
            &production_update_descriptor_binary(descriptor)?,
        )?;
    }
    Ok(production_hash_bytes(&preimage))
}

fn production_update_history(
    envelope: &ProductionUpdateEnvelopeV1,
    generation: u64,
    successor: AtomId,
    old: AtomId,
    relation_id: [u8; 32],
    successor_provenance_hash: [u8; 32],
    old_provenance_hash: [u8; 32],
) -> io::Result<ProductionUpdateHistoryV1> {
    let event_id = production_update_history_event_id(envelope.transaction_uuid, 0);
    let semantic_hash = production_update_history_semantic_hash(
        envelope.transaction_uuid,
        0,
        generation,
        envelope.semantic_time_unix_ns,
        successor,
        old,
        relation_id,
        envelope.intent_hash,
        successor_provenance_hash,
        old_provenance_hash,
    );
    let event = UpdateHistoryEventV1::from_body(UpdateHistoryEventBodyV1 {
        schema: "memoryx.update-history-event.v1".to_owned(),
        version: 1,
        event_id: hex_lower(&event_id),
        transaction_id: envelope.transaction_id.clone(),
        event_ordinal: 0,
        generation,
        semantic_time_unix_ns: envelope.semantic_time_unix_ns,
        operation: "update".to_owned(),
        outcome: "committed".to_owned(),
        atom_ids: vec![hex_lower(&successor), hex_lower(&old)],
        supersedes_relation_id: hex_lower(&relation_id),
        intent_hash: hex_lower(&envelope.intent_hash),
        successor_provenance_hash: hex_lower(&successor_provenance_hash),
        old_provenance_hash: hex_lower(&old_provenance_hash),
        history_semantic_hash: hex_lower(&semantic_hash),
    })?;
    let record_bytes = event.canonical_bytes()?;
    let mut leaf_bytes = Vec::new();
    leaf_bytes.extend_from_slice(PRODUCTION_UPDATE_HISTORY_LEAF_ID.as_bytes());
    leaf_bytes.push(0);
    leaf_bytes.extend_from_slice(&event_id);
    leaf_bytes.extend_from_slice(&semantic_hash);
    leaf_bytes.extend_from_slice(&successor_provenance_hash);
    leaf_bytes.extend_from_slice(&old_provenance_hash);
    Ok(ProductionUpdateHistoryV1 {
        event_id,
        semantic_hash,
        leaf_bytes,
        record_bytes,
    })
}

fn production_update_graph_leaf(successor_node: u64, old_node: u64) -> Vec<u8> {
    let mut attribute = Vec::new();
    attribute.extend_from_slice(PRODUCTION_GRAPH_ATTRIBUTE_ID.as_bytes());
    attribute.push(0);
    attribute.extend_from_slice(&1u16.to_le_bytes());
    attribute.extend_from_slice(&0u16.to_le_bytes());
    attribute.extend_from_slice(&0u32.to_le_bytes());
    attribute.extend_from_slice(&0u32.to_le_bytes());
    let attribute_hash = production_hash_bytes(&attribute);
    let mut leaf = Vec::new();
    leaf.extend_from_slice(PRODUCTION_GRAPH_LEAF_ID.as_bytes());
    leaf.push(0);
    leaf.extend_from_slice(&1u16.to_le_bytes());
    leaf.extend_from_slice(&successor_node.to_le_bytes());
    leaf.extend_from_slice(&EdgeType::SUPERSEDES.to_u32().to_le_bytes());
    leaf.extend_from_slice(&old_node.to_le_bytes());
    leaf.extend_from_slice(&5000u16.to_le_bytes());
    leaf.extend_from_slice(&attribute_hash);
    leaf
}

fn production_update_delta_bytes(
    delta_id: u32,
    base_generation: u32,
    successor_node: u64,
    old_node: u64,
) -> io::Result<(Vec<u8>, [u8; 32])> {
    let mut header = DeltaHeader::new(delta_id, base_generation, 1);
    let edge = EdgeListEntry::new(successor_node, old_node, EdgeType::SUPERSEDES, 5000, 0, 0);
    let mut bytes = vec![0; DeltaHeader::SIZE + EdgeListEntry::SIZE];
    if !header.write_to_bytes(&mut bytes[..DeltaHeader::SIZE])
        || !edge.write_to_bytes(&mut bytes[DeltaHeader::SIZE..])
    {
        return Err(invalid_data("update DELT encoding failed"));
    }
    let mut semantic = Vec::new();
    semantic.extend_from_slice(PRODUCTION_GRAPH_DELTA_SEMANTIC_ID.as_bytes());
    semantic.push(0);
    semantic.extend_from_slice(&1u16.to_le_bytes());
    semantic.extend_from_slice(&delta_id.to_le_bytes());
    semantic.extend_from_slice(&base_generation.to_le_bytes());
    semantic.extend_from_slice(&1u64.to_le_bytes());
    semantic.extend_from_slice(&successor_node.to_le_bytes());
    semantic.extend_from_slice(&EdgeType::SUPERSEDES.to_u32().to_le_bytes());
    semantic.extend_from_slice(&old_node.to_le_bytes());
    semantic.extend_from_slice(&5000u16.to_le_bytes());
    semantic.extend_from_slice(&0u16.to_le_bytes());
    semantic.extend_from_slice(&0u32.to_le_bytes());
    semantic.extend_from_slice(&0u32.to_le_bytes());
    Ok((bytes, production_hash_bytes(&semantic)))
}

fn production_update_graph_manifest(
    delta_count: u32,
    base_generation: u32,
    node_count: u64,
    graph_leaves: &[Vec<u8>],
) -> io::Result<(Vec<u8>, [u8; 32])> {
    if delta_count == 0 || delta_count > 8 || graph_leaves.len() != delta_count as usize {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "graph_compaction_required",
        ));
    }
    let mut manifest = GraphManifest::new(node_count);
    manifest.base_gen = base_generation;
    manifest.mark_edge_type(EdgeType::SUPERSEDES);
    for delta_id in 1..=delta_count {
        if !manifest.add_delta(delta_id) {
            return Err(invalid_data("update GRM1 delta list overflow"));
        }
    }
    let mut bytes = vec![0; GraphManifest::SIZE];
    if !manifest.write_to_bytes(&mut bytes) {
        return Err(invalid_data("update GRM1 encoding failed"));
    }
    let mut graph_root = Vec::new();
    graph_root.extend_from_slice(b"memoryx.graph-semantic-root.v1\0");
    graph_root.extend_from_slice(&1u16.to_le_bytes());
    graph_root.extend_from_slice(&node_count.to_le_bytes());
    graph_root.extend_from_slice(&(graph_leaves.len() as u64).to_le_bytes());
    for leaf in graph_leaves {
        production_append_u32_frame(&mut graph_root, leaf)?;
    }
    let graph_root = production_hash_bytes(&graph_root);
    let mut semantic = Vec::new();
    semantic.extend_from_slice(PRODUCTION_GRAPH_MANIFEST_SEMANTIC_ID.as_bytes());
    semantic.push(0);
    semantic.extend_from_slice(&1u16.to_le_bytes());
    semantic.extend_from_slice(&base_generation.to_le_bytes());
    semantic.extend_from_slice(&node_count.to_le_bytes());
    semantic.extend_from_slice(&manifest.edge_type_mask.to_le_bytes());
    semantic.extend_from_slice(&delta_count.to_le_bytes());
    for delta_id in 1..=delta_count {
        semantic.extend_from_slice(&delta_id.to_le_bytes());
    }
    semantic.extend_from_slice(&graph_root);
    Ok((bytes, production_hash_bytes(&semantic)))
}

fn production_update_locate_bytes(atoms: &[ProductionAtomStateV1]) -> Vec<u8> {
    let mut ordered = atoms.to_vec();
    ordered.sort_by_key(|atom| atom.node_num);
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"LOC1");
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&(ordered.len() as u32).to_le_bytes());
    for atom in ordered {
        bytes.extend_from_slice(&atom.node_num.to_le_bytes());
        bytes.extend_from_slice(&atom.atom_id);
    }
    bytes
}

fn production_update_current_view_bytes(
    superseded_by: &BTreeMap<AtomId, AtomId>,
) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"CVW1");
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(
        &u32::try_from(superseded_by.len())
            .map_err(|_| invalid_data("update current-view count overflow"))?
            .to_le_bytes(),
    );
    for (old, successor) in superseded_by {
        bytes.extend_from_slice(successor);
        bytes.extend_from_slice(old);
    }
    Ok(bytes)
}

fn production_update_spv1(
    successor: AtomId,
    successor_provenance_hash: [u8; 32],
    source_projection_hash: [u8; 32],
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(102);
    bytes.extend_from_slice(b"SPV1");
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&successor);
    bytes.extend_from_slice(&successor_provenance_hash);
    bytes.extend_from_slice(&source_projection_hash);
    bytes
}

fn production_update_semantic(domain: &str, bytes: &[u8]) -> [u8; 32] {
    let mut preimage = Vec::new();
    preimage.extend_from_slice(domain.as_bytes());
    preimage.push(0);
    preimage.extend_from_slice(bytes);
    production_hash_bytes(&preimage)
}

fn production_update_decode_spv1(bytes: &[u8]) -> io::Result<(AtomId, [u8; 32], [u8; 32])> {
    if bytes.len() != 102 || bytes[..4] != *b"SPV1" || bytes[4..6] != 1u16.to_le_bytes() {
        return Err(invalid_data("update SPV1 component is noncanonical"));
    }
    Ok((
        bytes[6..38]
            .try_into()
            .map_err(|_| invalid_data("update SPV1 successor identity is invalid"))?,
        bytes[38..70]
            .try_into()
            .map_err(|_| invalid_data("update SPV1 provenance identity is invalid"))?,
        bytes[70..102]
            .try_into()
            .map_err(|_| invalid_data("update SPV1 source identity is invalid"))?,
    ))
}

fn production_update_decode_history(
    bytes: &[u8],
) -> io::Result<(UpdateHistoryEventV1, ProductionUpdateHistoryIdentityV1)> {
    let record = UpdateHistoryEventV1::decode(bytes, "update history event")?;
    if record.schema != "memoryx.update-history-event.v1"
        || record.version != 1
        || record.event_ordinal != 0
        || record.operation != "update"
        || record.outcome != "committed"
        || record.atom_ids.len() != 2
    {
        return Err(invalid_data("update history event is noncanonical"));
    }
    let transaction_uuid = validate_production_uuid(&record.transaction_id)?;
    let event_id = parse_hash_hex(&record.event_id, "update history event ID")?;
    let semantic_hash = parse_hash_hex(
        &record.history_semantic_hash,
        "update history semantic hash",
    )?;
    let successor_atom_id = parse_hash_hex(&record.atom_ids[0], "update history successor AtomId")?;
    let old_atom_id = parse_hash_hex(&record.atom_ids[1], "update history old AtomId")?;
    let relation_id = parse_hash_hex(
        &record.supersedes_relation_id,
        "update history supersedes relation ID",
    )?;
    let intent_hash = parse_hash_hex(&record.intent_hash, "update history intent hash")?;
    let successor_provenance_hash = parse_hash_hex(
        &record.successor_provenance_hash,
        "update history successor provenance hash",
    )?;
    let old_provenance_hash = parse_hash_hex(
        &record.old_provenance_hash,
        "update history old provenance hash",
    )?;
    let expected_event_id =
        production_update_history_event_id(transaction_uuid, record.event_ordinal);
    let expected_semantic_hash = production_update_history_semantic_hash(
        transaction_uuid,
        record.event_ordinal,
        record.generation,
        record.semantic_time_unix_ns,
        successor_atom_id,
        old_atom_id,
        relation_id,
        intent_hash,
        successor_provenance_hash,
        old_provenance_hash,
    );
    if event_id != expected_event_id || semantic_hash != expected_semantic_hash {
        return Err(invalid_data(
            "update history event identity or semantic hash is invalid",
        ));
    }
    Ok((
        record,
        ProductionUpdateHistoryIdentityV1 {
            transaction_uuid,
            event_id,
            semantic_hash,
            successor_atom_id,
            old_atom_id,
            relation_id,
            intent_hash,
            successor_provenance_hash,
            old_provenance_hash,
        },
    ))
}

fn production_update_decode_delta(
    bytes: &[u8],
) -> io::Result<(DeltaHeader, EdgeListEntry, [u8; 32])> {
    if bytes.len() != DeltaHeader::SIZE + EdgeListEntry::SIZE {
        return Err(invalid_data("update DELT length is noncanonical"));
    }
    let header = DeltaHeader::from_bytes(&bytes[..DeltaHeader::SIZE])
        .ok_or_else(|| invalid_data("update DELT header or CRC is invalid"))?;
    let edge = EdgeListEntry::from_bytes(&bytes[DeltaHeader::SIZE..])
        .ok_or_else(|| invalid_data("update DELT edge is invalid"))?;
    if header.flags != 0 || header.edge_count != 1 || header.reserved1 != 0 || header.reserved2 != 0
    {
        return Err(invalid_data("update DELT header fields are noncanonical"));
    }
    let mut semantic = Vec::new();
    semantic.extend_from_slice(PRODUCTION_GRAPH_DELTA_SEMANTIC_ID.as_bytes());
    semantic.push(0);
    semantic.extend_from_slice(&1u16.to_le_bytes());
    semantic.extend_from_slice(&header.delta_id.to_le_bytes());
    semantic.extend_from_slice(&header.base_gen.to_le_bytes());
    semantic.extend_from_slice(&1u64.to_le_bytes());
    semantic.extend_from_slice(&edge.src_node.to_le_bytes());
    semantic.extend_from_slice(&edge.edge_type.to_le_bytes());
    semantic.extend_from_slice(&edge.dst_node.to_le_bytes());
    semantic.extend_from_slice(&edge.confidence_q.to_le_bytes());
    semantic.extend_from_slice(&edge.flags.to_le_bytes());
    semantic.extend_from_slice(&edge.valid_from_bucket.to_le_bytes());
    semantic.extend_from_slice(&edge.valid_to_bucket.to_le_bytes());
    Ok((header, edge, production_hash_bytes(&semantic)))
}

fn production_update_decode_graph_manifest(bytes: &[u8]) -> io::Result<GraphManifest> {
    if bytes.len() != GraphManifest::SIZE {
        return Err(invalid_data("update GRM1 length is noncanonical"));
    }
    let manifest = GraphManifest::from_bytes(bytes)
        .ok_or_else(|| invalid_data("update GRM1 header is invalid"))?;
    let delta_count = manifest.delta_count as usize;
    if manifest.flags != 0
        || manifest.reserved1 != 0
        || delta_count == 0
        || delta_count > 8
        || manifest.delta_ids[..delta_count]
            .iter()
            .enumerate()
            .any(|(index, delta_id)| *delta_id != index as u32 + 1)
        || manifest.delta_ids[delta_count..]
            .iter()
            .any(|delta_id| *delta_id != 0)
    {
        return Err(invalid_data("update GRM1 fields are noncanonical"));
    }
    Ok(manifest)
}

fn production_update_component_semantic_hash(
    descriptor: &UpdateComponentDescriptorV1,
    bytes: &[u8],
) -> io::Result<Option<[u8; 32]>> {
    let semantic = match descriptor.registry_order {
        20 => production_hash_bytes(bytes),
        30 => production_update_semantic("memoryx.idl1-membership-semantic.v1", bytes),
        40 => production_update_semantic("memoryx.loc1-membership-semantic.v1", bytes),
        50 => {
            production_update_decode_spv1(bytes)?;
            production_update_semantic(PRODUCTION_UPDATE_SPV1_SEMANTIC_ID, bytes)
        }
        60 => {
            let (_, history_identity) = production_update_decode_history(bytes)?;
            history_identity.semantic_hash
        }
        70 => production_update_semantic("memoryx.current-view-semantic.v1", bytes),
        80 => production_update_decode_delta(bytes)?.2,
        90 => {
            production_update_decode_graph_manifest(bytes)?;
            return Ok(None);
        }
        _ => {
            return Err(invalid_data(
                "update component semantic class is unsupported",
            ));
        }
    };
    Ok(Some(semantic))
}

fn production_update_relation_journal_preflight(root: &Path, old: AtomId) -> io::Result<()> {
    let old = hex_lower(&old);
    for (relative, expected_kind) in [
        ("meta/relations.jsonl", "current"),
        ("meta/relation_tombstone_resolutions.jsonl", "historical"),
    ] {
        let path = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        let bytes = match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Ok(metadata) if metadata.is_file() && !is_link_or_reparse(&path, &metadata) => {
                read_bytes_bounded_under(root, &path, PRODUCTION_UPDATE_MAX_COMPONENT_BYTES)?
            }
            Ok(_) => return Err(invalid_data("relation journal is not a regular file")),
            Err(error) => return Err(error),
        };
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let record = UpdateRelationJournalV1::decode(line, "relation journal")?;
            if record.schema != "memoryx.update-relation-journal.v1"
                || record.version != 1
                || record.journal_kind != expected_kind
                || record.predicate_id != EdgeType::SUPERSEDES.to_u32()
                || (record.current != (expected_kind == "current"))
                || (record.historical != (expected_kind == "historical"))
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "relation journal version is not admitted by P1",
                ));
            }
            for (value, label) in [
                (&record.relation_atom_id, "relation_atom_id"),
                (&record.subject_atom_id, "subject_atom_id"),
                (&record.object_atom_id, "object_atom_id"),
            ] {
                parse_hash_hex(value, label)?;
            }
            if record.relation_atom_id == old
                || record.subject_atom_id == old
                || record.object_atom_id == old
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "relation_backed_atom_requires_composite_operation",
                ));
            }
        }
    }
    Ok(())
}

fn production_update_post_anchors(
    root: &Path,
    provenance_bytes: Vec<u8>,
) -> io::Result<Vec<(u16, bool, Vec<u8>)>> {
    let mut anchors = production_anchor_leaves(root)?;
    let atom_sources = anchors
        .iter_mut()
        .find(|(order, _, _)| *order == 180)
        .ok_or_else(|| invalid_data("atom-source anchor is absent from registry"))?;
    atom_sources.1 = true;
    atom_sources.2 = provenance_bytes;
    Ok(anchors)
}

fn production_update_resource_preflight(
    request: &UpdateAtomRequestV1,
    staged: &[(UpdateComponentDescriptorV1, Vec<u8>)],
    prepare_bytes: &[u8],
    manifest_bytes: &[u8],
) -> io::Result<()> {
    if staged.len() != PRODUCTION_UPDATE_COMPONENT_COUNT {
        return Err(invalid_data("resource_limit_exceeded"));
    }
    let mut control_lengths = vec![prepare_bytes.len() as u64, manifest_bytes.len() as u64];
    for (descriptor, _) in staged {
        control_lengths.push(descriptor.canonical_bytes()?.len() as u64);
    }
    if control_lengths
        .iter()
        .any(|length| *length > PRODUCTION_UPDATE_MAX_CONTROL_BYTES)
    {
        return Err(invalid_data("resource_limit_exceeded"));
    }
    let controls = control_lengths
        .iter()
        .try_fold(0u64, |total, length| total.checked_add(*length));
    let controls = controls.ok_or_else(|| invalid_data("resource_limit_exceeded"))?;
    let projection_bytes = (request.claim_projection.len() as u64)
        .checked_add(request.api_evidence_projection.len() as u64)
        .and_then(|value| {
            value.checked_add(request.successor_source_attachment_projection.len() as u64)
        })
        .ok_or_else(|| invalid_data("resource_limit_exceeded"))?;
    let descriptor_bytes = staged.iter().try_fold(0u64, |total, (descriptor, _)| {
        total.checked_add(
            production_update_descriptor_binary(descriptor)
                .map(|bytes| bytes.len() as u64)
                .unwrap_or(u64::MAX),
        )
    });
    let descriptor_bytes =
        descriptor_bytes.ok_or_else(|| invalid_data("resource_limit_exceeded"))?;
    let component_bytes = staged.iter().try_fold(0u64, |total, (_, bytes)| {
        total.checked_add(bytes.len() as u64)
    });
    let component_bytes = component_bytes.ok_or_else(|| invalid_data("resource_limit_exceeded"))?;
    let path_bytes = staged.iter().try_fold(0usize, |total, (descriptor, _)| {
        total
            .checked_add(descriptor.target_path.len())
            .and_then(|value| value.checked_add(descriptor.stage_path.len()))
    });
    let path_bytes = path_bytes.ok_or_else(|| invalid_data("resource_limit_exceeded"))?;
    let total = controls
        .checked_add(request.successor_body.len() as u64)
        .and_then(|value| value.checked_add(projection_bytes))
        .and_then(|value| value.checked_add(descriptor_bytes))
        .and_then(|value| value.checked_add(path_bytes as u64))
        .and_then(|value| value.checked_add(0))
        .and_then(|value| value.checked_add(component_bytes))
        .and_then(|value| value.checked_add(PRODUCTION_UPDATE_MAX_COMPONENT_BYTES))
        .ok_or_else(|| invalid_data("resource_limit_exceeded"))?;
    if controls > PRODUCTION_UPDATE_MAX_AGGREGATE_CONTROL_BYTES
        || request.successor_body.len() as u64 > PRODUCTION_MAX_BODY_BYTES
        || projection_bytes > PRODUCTION_UPDATE_MAX_PROJECTION_BYTES
        || descriptor_bytes > PRODUCTION_UPDATE_MAX_DESCRIPTOR_BYTES
        || staged.len() != PRODUCTION_UPDATE_COMPONENT_COUNT
        || PRODUCTION_UPDATE_MAX_PATH_COUNT != 10
        || path_bytes > PRODUCTION_UPDATE_MAX_TOTAL_PATH_BYTES
        || component_bytes > PRODUCTION_UPDATE_MAX_COMPONENT_BYTES
        || total > PRODUCTION_UPDATE_MAX_TOTAL_BYTES
    {
        return Err(invalid_data("resource_limit_exceeded"));
    }
    Ok(())
}

fn production_plan_update(
    root: &Path,
    state: &ProductionRuntimeStateV1,
    request: &UpdateAtomRequestV1,
    intent: &ProductionUpdateIntentV1,
    envelope: &ProductionUpdateEnvelopeV1,
) -> io::Result<ProductionUpdatePlanV1> {
    let generation = state
        .head
        .generation
        .checked_add(1)
        .ok_or_else(|| invalid_data("production generation overflow"))?;
    if generation > PRODUCTION_MAX_GENERATIONS {
        return Err(invalid_data("resource_limit_exceeded"));
    }
    let old_atom = state
        .atoms
        .iter()
        .find(|atom| atom.atom_id == request.old_atom_id)
        .cloned()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "old_atom_missing"))?;
    if state.superseded_by.contains_key(&request.old_atom_id) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "already_superseded",
        ));
    }
    if state
        .atoms
        .iter()
        .any(|atom| atom.atom_id == request.successor_atom_id)
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "successor_collision",
        ));
    }
    production_update_relation_journal_preflight(root, request.old_atom_id)?;
    let successor_node = state
        .atoms
        .iter()
        .map(|atom| atom.node_num)
        .max()
        .map_or(0, |node| node.saturating_add(1));
    if successor_node == u64::MAX {
        return Err(invalid_data("resource_limit_exceeded"));
    }
    let successor_provenance_hash = production_sha256(&request.successor_provenance);
    if request.successor_provenance.is_empty()
        || successor_provenance_hash == old_atom.provenance_hash
    {
        return Err(invalid_data("provenance_projection_conflict"));
    }
    let record = production_record_bytes(
        request.successor_atom_id,
        &request.successor_body,
        generation as u32,
    )?;
    let body_header = AtomBodyHeader::from_bytes(&request.successor_body)
        .map_err(|error| invalid_data(&format!("noncanonical_successor: {error}")))?;
    let history = production_update_history(
        envelope,
        generation,
        request.successor_atom_id,
        request.old_atom_id,
        intent.supersedes_relation_id,
        successor_provenance_hash,
        old_atom.provenance_hash,
    )?;
    let successor_atom = ProductionAtomStateV1 {
        atom_id: request.successor_atom_id,
        atom_type: request.successor_atom_type,
        node_num: successor_node,
        committed_generation: generation,
        body_len: request.successor_body.len() as u64,
        body_crc32: crc32(&request.successor_body),
        body_hash: production_hash_bytes(&request.successor_body),
        segment_id: generation as u32,
        record_offset: 0,
        record_extent_len: record.len() as u64,
        domain_mask: 0xffff,
        created_at_ns: body_header.created_at_unix_ns,
        trust_level: 5000,
        source_id: 0,
        provenance_hash: successor_provenance_hash,
        history_event_id: history.event_id,
        history_leaf: history.leaf_bytes.clone(),
    };
    let mut post_atoms = state.atoms.clone();
    post_atoms.push(successor_atom.clone());
    let idloc = production_idloc_bytes_many(&post_atoms);
    let locate = production_update_locate_bytes(&post_atoms);
    let spv1 = production_update_spv1(
        request.successor_atom_id,
        successor_provenance_hash,
        intent.successor_source_attachment_hash,
    );
    if spv1.len() != 102 {
        return Err(invalid_data("provenance_projection_conflict"));
    }
    let prior_provenance = match fs::symlink_metadata(root.join("meta/atom_sources.jsonl")) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Ok(metadata) if metadata.is_file() => read_bytes_bounded_under(
            root,
            &root.join("meta/atom_sources.jsonl"),
            PRODUCTION_UPDATE_MAX_COMPONENT_BYTES,
        )?,
        Ok(_) => return Err(invalid_data("successor provenance target is not a file")),
        Err(error) => return Err(error),
    };
    if prior_provenance.len() % 102 != 0 {
        return Err(invalid_data(
            "existing successor provenance is noncanonical",
        ));
    }
    for prior in prior_provenance.as_chunks::<102>().0 {
        production_update_decode_spv1(prior)?;
    }
    let mut post_provenance = prior_provenance;
    post_provenance.extend_from_slice(&spv1);
    let mut post_superseded = state.superseded_by.clone();
    if post_superseded
        .insert(request.old_atom_id, request.successor_atom_id)
        .is_some()
    {
        return Err(invalid_data("ambiguous_supersession_state"));
    }
    let current_view = production_update_current_view_bytes(&post_superseded)?;
    let graph_leaf = production_update_graph_leaf(successor_node, old_atom.node_num);
    let mut post_graph = state.graph_leaves.clone();
    post_graph.push(graph_leaf.clone());
    post_graph.sort();
    let delta_id =
        u32::try_from(post_graph.len()).map_err(|_| invalid_data("graph_compaction_required"))?;
    let (delta, delta_semantic) =
        production_update_delta_bytes(delta_id, 0, successor_node, old_atom.node_num)?;
    let (grm1, grm1_semantic) =
        production_update_graph_manifest(delta_id, 0, successor_node + 1, &post_graph)?;
    let mut staged = vec![
        (
            production_update_descriptor(
                20,
                "cas.successor-append.v1",
                "append",
                format!("cas/segments/seg_{generation:08}.skf1"),
                "memoryx.skf1.current",
                &record,
                production_hash_bytes(&record),
            )?,
            record,
        ),
        (
            production_update_descriptor(
                30,
                "index.idloc-replace.v1",
                "replace",
                "index/idloc.mmap".to_owned(),
                "memoryx.idl1.current",
                &idloc,
                production_update_semantic("memoryx.idl1-membership-semantic.v1", &idloc),
            )?,
            idloc,
        ),
        (
            production_update_descriptor(
                40,
                "index.locate-replace.v1",
                "replace",
                "index/locate.bin".to_owned(),
                "memoryx.loc1.current",
                &locate,
                production_update_semantic("memoryx.loc1-membership-semantic.v1", &locate),
            )?,
            locate,
        ),
        (
            production_update_descriptor(
                50,
                "meta.successor-provenance.v1",
                "replace",
                "meta/atom_sources.jsonl".to_owned(),
                "memoryx.atom-source-links.v1",
                &spv1,
                production_update_semantic(PRODUCTION_UPDATE_SPV1_SEMANTIC_ID, &spv1),
            )?,
            spv1,
        ),
        (
            production_update_descriptor(
                60,
                "meta.update-history-once.v1",
                "replace",
                "meta/history.log".to_owned(),
                "memoryx.history.transaction-once.v1",
                &history.record_bytes,
                history.semantic_hash,
            )?,
            history.record_bytes.clone(),
        ),
        (
            production_update_descriptor(
                70,
                "meta.current-view.v1",
                "replace",
                "meta/current_versions.jsonl".to_owned(),
                "memoryx.current-view.v1",
                &current_view,
                production_update_semantic("memoryx.current-view-semantic.v1", &current_view),
            )?,
            current_view,
        ),
        (
            production_update_descriptor(
                80,
                "graph.delta-supersedes.v1",
                "create",
                format!("index/graph/deltas/d_{delta_id:08}.edges"),
                "memoryx.graph.delta.v0101",
                &delta,
                delta_semantic,
            )?,
            delta,
        ),
        (
            production_update_descriptor(
                90,
                "graph.manifest-grm1.v0101",
                "replace",
                "index/graph/manifest.dat".to_owned(),
                "memoryx.graph-manifest.grm1-v0101",
                &grm1,
                grm1_semantic,
            )?,
            grm1,
        ),
    ];
    staged.sort_by_key(|(descriptor, _)| descriptor.registry_order);
    let descriptors = staged
        .iter()
        .map(|(descriptor, _)| descriptor.clone())
        .collect::<Vec<_>>();
    let component_root = production_update_component_root(&descriptors)?;
    let mut post_history = state.history_leaves.clone();
    post_history.push(history.leaf_bytes.clone());
    let atom_refs = post_atoms.iter().collect::<Vec<_>>();
    let graph_refs = post_graph.iter().map(Vec::as_slice).collect::<Vec<_>>();
    let history_refs = post_history.iter().map(Vec::as_slice).collect::<Vec<_>>();
    let logical_digest = production_logical_digest_multi_with_graph(
        generation,
        state.head.commit_hash,
        &atom_refs,
        &graph_refs,
        &history_refs,
        &production_update_post_anchors(root, post_provenance)?,
    )?;
    let prepare = UpdatePrepareV1::from_body(UpdatePrepareBodyV1 {
        schema: PRODUCTION_UPDATE_PREPARE_SCHEMA.to_owned(),
        version: 1,
        format_version: 2,
        generation,
        parent_commit_hash: hex_lower(&state.head.commit_hash),
        transaction_id: envelope.transaction_id.clone(),
        semantic_time_unix_ns: envelope.semantic_time_unix_ns,
        base_binding_hash: hex_lower(&envelope.base_binding_hash),
        envelope_hash: hex_lower(&envelope.hash),
        operation: "update_atom".to_owned(),
        intent_hash: hex_lower(&intent.hash),
        operation_registry_id: PRODUCTION_UPDATE_OPERATION_REGISTRY_ID.to_owned(),
        limits_id: PRODUCTION_UPDATE_LIMITS_ID.to_owned(),
        old_atom_id: hex_lower(&intent.old_atom_id),
        successor_atom_id: hex_lower(&intent.successor_atom_id),
        successor_body_hash: hex_lower(&intent.successor_body_hash),
        claim_projection_hash: hex_lower(&intent.claim_projection_hash),
        api_evidence_projection_hash: hex_lower(&intent.api_evidence_projection_hash),
        successor_atom_type: request.successor_atom_type.to_u32(),
        old_node: old_atom.node_num,
        successor_node,
        supersedes_relation_id: hex_lower(&intent.supersedes_relation_id),
        old_provenance_hash: hex_lower(&old_atom.provenance_hash),
        successor_provenance_hash: hex_lower(&successor_provenance_hash),
        successor_source_attachment_hash: hex_lower(&intent.successor_source_attachment_hash),
        history_event_id: hex_lower(&history.event_id),
        history_semantic_hash: hex_lower(&history.semantic_hash),
        component_root_hash: hex_lower(&component_root),
        logical_state_digest: hex_lower(&logical_digest),
        components: descriptors.clone(),
    })?;
    let prepare_bytes = prepare.canonical_bytes()?;
    let manifest = UpdateGenerationManifestV1::from_body(UpdateGenerationManifestBodyV1 {
        schema: PRODUCTION_UPDATE_MANIFEST_SCHEMA.to_owned(),
        version: 1,
        format_version: 2,
        generation,
        parent_commit_hash: hex_lower(&state.head.commit_hash),
        prepare_hash: production_hash_hex(&prepare_bytes),
        transaction_id: envelope.transaction_id.clone(),
        semantic_time_unix_ns: envelope.semantic_time_unix_ns,
        base_binding_hash: hex_lower(&envelope.base_binding_hash),
        envelope_hash: hex_lower(&envelope.hash),
        operation: "update_atom".to_owned(),
        intent_hash: hex_lower(&intent.hash),
        operation_registry_id: PRODUCTION_UPDATE_OPERATION_REGISTRY_ID.to_owned(),
        limits_id: PRODUCTION_UPDATE_LIMITS_ID.to_owned(),
        old_atom_id: hex_lower(&intent.old_atom_id),
        successor_atom_id: hex_lower(&intent.successor_atom_id),
        successor_body_hash: hex_lower(&intent.successor_body_hash),
        claim_projection_hash: hex_lower(&intent.claim_projection_hash),
        api_evidence_projection_hash: hex_lower(&intent.api_evidence_projection_hash),
        successor_atom_type: request.successor_atom_type.to_u32(),
        old_node: old_atom.node_num,
        successor_node,
        supersedes_relation_id: hex_lower(&intent.supersedes_relation_id),
        old_provenance_hash: hex_lower(&old_atom.provenance_hash),
        successor_provenance_hash: hex_lower(&successor_provenance_hash),
        successor_source_attachment_hash: hex_lower(&intent.successor_source_attachment_hash),
        history_event_id: hex_lower(&history.event_id),
        history_semantic_hash: hex_lower(&history.semantic_hash),
        history_event_count: state.history_leaves.len() as u64 + 1,
        relation_count: state.graph_leaves.len() as u64 + 1,
        post_atom_count: post_atoms.len() as u64,
        graph_leaf_count: post_graph.len() as u64,
        component_root_hash: hex_lower(&component_root),
        logical_state_digest: hex_lower(&logical_digest),
        components: descriptors,
    })?;
    production_update_resource_preflight(
        request,
        &staged,
        &prepare_bytes,
        &manifest.canonical_bytes()?,
    )?;
    Ok(ProductionUpdatePlanV1 {
        generation,
        old_atom,
        successor_atom,
        relation_id: intent.supersedes_relation_id,
        history,
        graph_leaf,
        staged,
        component_root,
        logical_digest,
        prepare,
        manifest,
    })
}

fn production_install_create(root: &Path, relative: &str, bytes: &[u8]) -> io::Result<()> {
    let target = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
    canonical_relative_path(root, &target)?;
    if path_entry_exists(&target)? {
        let metadata = fs::symlink_metadata(&target)?;
        if is_link_or_reparse(&target, &metadata) || !metadata.is_file() {
            return Err(invalid_data("update create target is not a regular file"));
        }
        let existing =
            read_bytes_bounded_under(root, &target, PRODUCTION_UPDATE_MAX_COMPONENT_BYTES)?;
        if existing == bytes {
            return Ok(());
        }
        return Err(invalid_data(
            "update create target conflicts with committed bytes",
        ));
    }
    let parent = target
        .parent()
        .ok_or_else(|| invalid_data("update create target has no parent"))?;
    fs::create_dir_all(parent)?;
    let guard = AncestorGuard::acquire(root, parent)?;
    guard.verify()?;
    production_write_new(&target, bytes)?;
    sync_directory(parent)?;
    guard.verify()
}

fn production_install_update_spv1(root: &Path, relative: &str, bytes: &[u8]) -> io::Result<()> {
    production_update_decode_spv1(bytes)?;
    let target = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
    let existing = match fs::symlink_metadata(&target) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Ok(metadata) if metadata.is_file() && !is_link_or_reparse(&target, &metadata) => {
            read_bytes_bounded_under(root, &target, PRODUCTION_UPDATE_MAX_COMPONENT_BYTES)?
        }
        Ok(_) => return Err(invalid_data("update SPV1 target is not a regular file")),
        Err(error) => return Err(error),
    };
    if existing.len() % 102 != 0 {
        return Err(invalid_data("update SPV1 target is noncanonical"));
    }
    for prior in existing.as_chunks::<102>().0 {
        production_update_decode_spv1(prior)?;
    }
    if existing.ends_with(bytes) {
        return Ok(());
    }
    if existing
        .as_chunks::<102>()
        .0
        .iter()
        .any(|prior| prior.as_slice() == bytes)
    {
        return Err(invalid_data(
            "update SPV1 record is duplicated out of order",
        ));
    }
    let mut post = existing;
    post.extend_from_slice(bytes);
    production_install_replacement(root, relative, &post)
}

fn production_install_update_history(root: &Path, relative: &str, bytes: &[u8]) -> io::Result<()> {
    production_update_decode_history(bytes)?;
    let target = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
    let existing = match fs::symlink_metadata(&target) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Ok(metadata) if metadata.is_file() && !is_link_or_reparse(&target, &metadata) => {
            read_bytes_bounded_under(root, &target, PRODUCTION_UPDATE_MAX_AGGREGATE_CONTROL_BYTES)?
        }
        Ok(_) => return Err(invalid_data("update history target is not a regular file")),
        Err(error) => return Err(error),
    };
    if !existing.is_empty() && !existing.ends_with(b"\n") {
        return Err(invalid_data("update history target is noncanonical"));
    }
    if existing
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .any(|line| line == bytes)
    {
        return if existing.ends_with(&[bytes, b"\n"].concat()) {
            Ok(())
        } else {
            Err(invalid_data(
                "update history event is duplicated out of order",
            ))
        };
    }
    let mut post = existing;
    post.extend_from_slice(bytes);
    post.push(b'\n');
    if post.len() as u64 > PRODUCTION_UPDATE_MAX_AGGREGATE_CONTROL_BYTES {
        return Err(invalid_data("resource_limit_exceeded"));
    }
    production_install_replacement(root, relative, &post)
}

fn production_read_update_component(
    root: &Path,
    generation_dir: &Path,
    descriptor: &UpdateComponentDescriptorV1,
) -> io::Result<Vec<u8>> {
    production_update_descriptor_binary(descriptor)?;
    let bytes = read_bytes_bounded_under(
        root,
        &generation_dir.join(
            descriptor
                .stage_path
                .replace('/', std::path::MAIN_SEPARATOR_STR),
        ),
        PRODUCTION_UPDATE_MAX_COMPONENT_BYTES,
    )?;
    if bytes.len() as u64 != descriptor.byte_length
        || production_hash_hex(&bytes) != descriptor.byte_hash
    {
        return Err(invalid_data("update staged component hash is invalid"));
    }
    if let Some(semantic_hash) = production_update_component_semantic_hash(descriptor, &bytes)?
        && semantic_hash
            != parse_hash_hex(&descriptor.semantic_hash, "update component semantic hash")?
    {
        return Err(invalid_data(
            "update staged component semantic hash is invalid",
        ));
    }
    Ok(bytes)
}

fn production_install_update_generation<Phase>(
    token: &BorrowedOwnerQuiescence<'_, Phase>,
    generation_dir: &Path,
    manifest: &UpdateGenerationManifestV1,
) -> io::Result<()> {
    token.verify()?;
    for descriptor in &manifest.components {
        let bytes =
            production_read_update_component(token.canonical_root(), generation_dir, descriptor)?;
        match descriptor.registry_order {
            20 => {
                production_install_create(token.canonical_root(), &descriptor.target_path, &bytes)?
            }
            30 | 40 | 70 => production_install_replacement(
                token.canonical_root(),
                &descriptor.target_path,
                &bytes,
            )?,
            50 => production_install_update_spv1(
                token.canonical_root(),
                &descriptor.target_path,
                &bytes,
            )?,
            60 => production_install_update_history(
                token.canonical_root(),
                &descriptor.target_path,
                &bytes,
            )?,
            80 => {
                production_install_create(token.canonical_root(), &descriptor.target_path, &bytes)?
            }
            90 => production_install_replacement(
                token.canonical_root(),
                &descriptor.target_path,
                &bytes,
            )?,
            _ => return Err(invalid_data("update install order is unsupported")),
        }
    }
    token.verify()
}

#[cfg(test)]
thread_local! {
    static PRODUCTION_UPDATE_TEST_STOP: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn production_update_test_stop(stage: &'static str) -> io::Result<()> {
    PRODUCTION_UPDATE_TEST_STOP.with(|selected| {
        if selected.get() == Some(stage) {
            selected.set(None);
            Err(io::Error::other(format!("injected update stop at {stage}")))
        } else {
            Ok(())
        }
    })
}

#[cfg(not(test))]
fn production_update_test_stop(_stage: &'static str) -> io::Result<()> {
    Ok(())
}

fn production_stage_update(
    token: &BorrowedOwnerQuiescence<'_, QuiescentWrite>,
    plan: &ProductionUpdatePlanV1,
    envelope: &ProductionUpdateEnvelopeV1,
) -> io::Result<PathBuf> {
    token.verify()?;
    let root = token.canonical_root();
    let generations = production_txn_root(root).join(GENERATIONS_DIR_NAME);
    let pending = generations.join(format!(".pending-{}", envelope.transaction_id));
    let published = production_generation_path(root, plan.generation);
    if path_entry_exists(&pending)? || path_entry_exists(&published)? {
        return Err(invalid_data("production update namespace already exists"));
    }
    fs::create_dir(&pending)?;
    fs::create_dir(pending.join(COMPONENTS_DIR_NAME))?;
    for (descriptor, bytes) in &plan.staged {
        production_write_new(
            &pending.join(
                descriptor
                    .stage_path
                    .replace('/', std::path::MAIN_SEPARATOR_STR),
            ),
            bytes,
        )?;
    }
    sync_directory(&pending.join(COMPONENTS_DIR_NAME))?;
    let cas = plan
        .staged
        .iter()
        .find(|(descriptor, _)| descriptor.registry_order == 20)
        .ok_or_else(|| invalid_data("update CAS component is missing"))?;
    production_install_create(root, &cas.0.target_path, &cas.1)?;
    production_write_new(
        &pending.join(PREPARE_FILE_NAME),
        &plan.prepare.canonical_bytes()?,
    )?;
    sync_directory(&pending)?;
    production_update_test_stop("after_prepare_before_commit")?;
    production_write_new(
        &pending.join(COMMIT_FILE_NAME),
        &plan.manifest.canonical_bytes()?,
    )?;
    sync_directory(&pending)?;
    token.verify()?;
    move_directory_write_through(&pending, &published)?;
    production_update_test_stop("after_commit_before_install")?;
    Ok(published)
}

/// Explicit direct-library production-v2 carrier for the bounded N5-B P0-C
/// one-new-atom slice. Existing `MemoryX` transports remain unchanged and fail
/// closed until their own transaction adapters are ratified.
pub struct ProductionMemoryX {
    authority: super::base_lease::LiveOwnerAuthority,
    state: ProductionRuntimeStateV1,
}

impl ProductionMemoryX {
    /// Open an explicitly admitted production-v2 base, recover its committed
    /// generation, install it idempotently, and only then expose the runtime.
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let authority =
            super::base_lease::LiveOwnerAuthority::acquire(root.as_ref()).map_err(|error| {
                match error {
                    BaseLeaseError::Busy { root } => io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!("production base writer lease is held: {}", root.display()),
                    ),
                    BaseLeaseError::NotDirectory { root } => io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "production base root is not a directory: {}",
                            root.display()
                        ),
                    ),
                    BaseLeaseError::Io { source, .. } => source,
                }
            })?;
        let state = {
            let startup = authority.borrow_startup()?;
            production_open_runtime(&startup)?
        };
        Ok(Self { authority, state })
    }

    pub fn committed_generation(&self) -> u64 {
        self.state.head.generation
    }

    pub fn committed_logical_digest(&self) -> [u8; 32] {
        self.state.head.logical_digest
    }

    pub fn startup_admission_bytes(&self) -> &[u8] {
        &self.state.admission_bytes
    }

    pub fn committed_atom_id(&self) -> Option<AtomId> {
        self.state.atom.as_ref().map(|atom| atom.atom_id)
    }

    /// Execute one explicit-envelope direct ingest. Same-transaction retry is
    /// checked against the immutable ledger before current-parent binding.
    #[allow(clippy::too_many_arguments)]
    pub fn direct_ingest(
        &mut self,
        transaction_id: &str,
        semantic_time_unix_ns: u64,
        body: &[u8],
        atom_type: AtomType,
        claim_projection: &[u8],
        evidence_projection: &[u8],
    ) -> Result<DirectIngestReceiptV1, DirectIngestFailureV1> {
        let mut transaction = transaction_id.to_owned();
        if validate_production_uuid(transaction_id).is_err() {
            return Err(DirectIngestFailureV1::not_started(
                DirectIngestFailureCodeV1::InvalidTransactionId,
                "transaction UUID is not canonical RFC-variant lowercase ASCII",
                None,
                None,
            ));
        }
        if semantic_time_unix_ns == 0 {
            return Err(DirectIngestFailureV1::not_started(
                DirectIngestFailureCodeV1::InvalidSemanticTime,
                "semantic time must be nonzero",
                Some(transaction),
                None,
            ));
        }
        if !claim_projection.is_empty() || !evidence_projection.is_empty() {
            return Err(DirectIngestFailureV1::not_started(
                DirectIngestFailureCodeV1::CompositeOperationNotAdmitted,
                "edge-producing claim or evidence projections are not admitted by P0-C",
                Some(transaction),
                None,
            ));
        }
        let owner_lifetime_transactions = self.state.owner_lifetime_transactions.clone();
        let recovered = (|| -> io::Result<ProductionRuntimeStateV1> {
            let startup = self.authority.borrow_startup()?;
            production_open_runtime(&startup)
        })();
        match recovered {
            Ok(mut state) => {
                state.owner_lifetime_transactions = owner_lifetime_transactions;
                self.state = state;
            }
            Err(error) => {
                return Err(DirectIngestFailureV1::not_started(
                    DirectIngestFailureCodeV1::RecoveryRequired,
                    error.to_string(),
                    Some(transaction),
                    None,
                ));
            }
        }
        if self.state.batch_transactions.contains_key(transaction_id)
            || self.state.update_transactions.contains_key(transaction_id)
            || matches!(
                self.state.owner_lifetime_transactions.get(transaction_id),
                Some(
                    ProductionOwnerLifetimeTransactionV1::Batch(_)
                        | ProductionOwnerLifetimeTransactionV1::Update(_)
                )
            )
        {
            return Err(DirectIngestFailureV1::not_started(
                DirectIngestFailureCodeV1::ConflictingTransactionReuse,
                "transaction identity belongs to a batch operation",
                Some(transaction),
                None,
            ));
        }
        let atom_id = match compute_atom_id_from_payload(body) {
            Ok(atom_id) => atom_id,
            Err(error) => {
                return Err(DirectIngestFailureV1::not_started(
                    DirectIngestFailureCodeV1::InvalidRequest,
                    format!("invalid canonical atom body: {error}"),
                    None,
                    None,
                ));
            }
        };
        if let Some(ProductionOwnerLifetimeTransactionV1::Direct(receipt)) = self
            .state
            .owner_lifetime_transactions
            .get(transaction_id)
            .cloned()
        {
            let parent = ProductionCommittedHead {
                generation: receipt.committed_generation,
                commit_hash: receipt.commit_hash,
                logical_digest: receipt.logical_digest,
            };
            let retry = ProductionBaseBindingV1::from_identity(
                self.authority.physical_identity(),
                parent,
            )
            .and_then(|binding| {
                let intent = ProductionDirectIntentV1::create(
                    binding.hash(),
                    atom_id,
                    atom_type.to_u32() as u8,
                    body,
                    claim_projection,
                    evidence_projection,
                )?;
                let _envelope = ProductionDirectEnvelopeV1::create(
                    transaction_id,
                    semantic_time_unix_ns,
                    binding.hash(),
                    intent.hash(),
                )?;
                if semantic_time_unix_ns != receipt.semantic_time_unix_ns
                    || binding.hash() != receipt.base_binding_hash
                    || intent.hash() != receipt.intent_hash
                {
                    return Err(invalid_data(
                        "owner-lifetime direct transaction identity was reused with a different intent",
                    ));
                }
                Ok(())
            });
            return match retry {
                Ok(()) => Ok(receipt),
                Err(error) => Err(DirectIngestFailureV1::not_started(
                    DirectIngestFailureCodeV1::ConflictingTransactionReuse,
                    error.to_string(),
                    Some(transaction),
                    Some(receipt.intent_hash),
                )),
            };
        }
        if let Some(committed) = self.state.committed_transactions.get(transaction_id) {
            let binding = ProductionBaseBindingV1::from_identity(
                self.authority.physical_identity(),
                committed.parent.clone(),
            )
            .and_then(|binding| {
                if binding.hash() != committed.base_binding_hash {
                    Err(invalid_data(
                        "committed transaction base binding cannot be reconstructed",
                    ))
                } else {
                    Ok(binding)
                }
            });
            let retry = binding.and_then(|binding| {
                let intent = ProductionDirectIntentV1::create(
                    binding.hash(),
                    atom_id,
                    atom_type.to_u32() as u8,
                    body,
                    claim_projection,
                    evidence_projection,
                )?;
                let envelope = ProductionDirectEnvelopeV1::create(
                    transaction_id,
                    semantic_time_unix_ns,
                    binding.hash(),
                    intent.hash(),
                )?;
                if semantic_time_unix_ns != committed.semantic_time_unix_ns
                    || intent.hash() != committed.intent_hash
                    || envelope.hash() != committed.envelope_hash
                {
                    return Err(invalid_data(
                        "transaction identity was reused with a different intent",
                    ));
                }
                Ok(())
            });
            return match retry {
                Ok(()) => Ok(self
                    .state
                    .receipt_for_transaction(transaction_id)
                    .expect("validated committed transaction has a receipt")
                    .clone()),
                Err(error) => Err(DirectIngestFailureV1::not_started(
                    DirectIngestFailureCodeV1::ConflictingTransactionReuse,
                    error.to_string(),
                    Some(transaction),
                    Some(committed.intent_hash),
                )),
            };
        }

        let binding = self.state.base_binding.clone();
        let intent = match ProductionDirectIntentV1::create(
            binding.hash(),
            atom_id,
            atom_type.to_u32() as u8,
            body,
            claim_projection,
            evidence_projection,
        ) {
            Ok(intent) => intent,
            Err(error) => {
                return Err(DirectIngestFailureV1::not_started(
                    DirectIngestFailureCodeV1::InvalidIntent,
                    error.to_string(),
                    Some(transaction),
                    None,
                ));
            }
        };
        let envelope = match ProductionDirectEnvelopeV1::create(
            transaction_id,
            semantic_time_unix_ns,
            binding.hash(),
            intent.hash(),
        ) {
            Ok(envelope) => envelope,
            Err(error) => {
                return Err(DirectIngestFailureV1::not_started(
                    DirectIngestFailureCodeV1::InvalidTransactionId,
                    error.to_string(),
                    Some(std::mem::take(&mut transaction)),
                    Some(intent.hash()),
                ));
            }
        };
        if let Some(existing) = &self.state.atom {
            if existing.atom_id != atom_id {
                return Err(DirectIngestFailureV1::not_started(
                    DirectIngestFailureCodeV1::CompositeOperationNotAdmitted,
                    "P0-C admits one committed atom only",
                    Some(transaction),
                    Some(intent.hash()),
                ));
            }
            let committed_body = match production_read_exact_committed_body(
                self.authority.canonical_root(),
                existing,
            ) {
                Ok(committed_body) => committed_body,
                Err(error) => {
                    return Err(DirectIngestFailureV1::not_started(
                        DirectIngestFailureCodeV1::UnsupportedOrCorrupt,
                        error.to_string(),
                        Some(transaction),
                        Some(intent.hash()),
                    ));
                }
            };
            if existing.atom_type != atom_type || committed_body != body {
                return Err(DirectIngestFailureV1::not_started(
                    DirectIngestFailureCodeV1::CanonicalRepresentationConflict,
                    "canonical AtomId is already committed with different exact body bytes",
                    Some(transaction),
                    Some(intent.hash()),
                ));
            }
            let receipt = DirectIngestReceiptV1::create(
                DirectIngestResultKindV1::ReusedCommitted,
                transaction.clone(),
                semantic_time_unix_ns,
                intent.hash(),
                binding.hash(),
                self.state.head.generation,
                self.state.head.commit_hash,
                self.state.head.logical_digest,
                atom_id,
                existing.node_num,
                None,
            )
            .map_err(|error| {
                DirectIngestFailureV1::not_started(
                    DirectIngestFailureCodeV1::UnsupportedOrCorrupt,
                    error.to_string(),
                    None,
                    Some(intent.hash()),
                )
            })?;
            self.state.owner_lifetime_transactions.insert(
                transaction,
                ProductionOwnerLifetimeTransactionV1::Direct(receipt.clone()),
            );
            return Ok(receipt);
        }

        let request = ProductionDirectRequestV1 {
            transaction_id: transaction.clone(),
            semantic_time_unix_ns,
            base_binding_bytes: binding.bytes().to_vec(),
            body: body.to_vec(),
            atom_type,
            claim_projection: claim_projection.to_vec(),
            evidence_projection: evidence_projection.to_vec(),
        };
        let mut committed = false;
        let result = (|| -> io::Result<()> {
            let write = self.authority.borrow_write()?;
            if request.transaction_id != envelope.transaction_id
                || request.semantic_time_unix_ns != envelope.semantic_time_unix_ns
                || request.base_binding_bytes != binding.bytes()
            {
                return Err(invalid_data(
                    "production request envelope changed before staging",
                ));
            }
            let published =
                production_stage_direct_ingest(&write, &self.state, &request, &intent, &envelope)?;
            committed = true;
            let (manifest, _) = production_read_control(
                write.canonical_root(),
                &published.join(COMMIT_FILE_NAME),
                "commit.bin",
                ProductionGenerationManifestV1::decode,
            )?;
            production_install_generation(&write, &published, &manifest)
        })();
        if let Err(error) = result {
            if !committed
                && error.kind() == io::ErrorKind::Unsupported
                && error.to_string().starts_with("migration_required:")
            {
                return Err(DirectIngestFailureV1::not_started(
                    DirectIngestFailureCodeV1::MigrationRequired,
                    error.to_string(),
                    Some(transaction),
                    Some(intent.hash()),
                ));
            }
            return Err(if committed {
                DirectIngestFailureV1::recovery_required(
                    error.to_string(),
                    transaction,
                    intent.hash(),
                    true,
                )
            } else {
                DirectIngestFailureV1::recovery_required(
                    error.to_string(),
                    transaction,
                    intent.hash(),
                    false,
                )
            });
        }
        let owner_lifetime_transactions = self.state.owner_lifetime_transactions.clone();
        let reopened = (|| -> io::Result<ProductionRuntimeStateV1> {
            let startup = self.authority.borrow_startup()?;
            production_open_runtime(&startup)
        })();
        match reopened {
            Ok(mut state) => {
                state.owner_lifetime_transactions = owner_lifetime_transactions;
                self.state = state;
                Ok(self
                    .state
                    .receipt_for_transaction(transaction_id)
                    .expect("installed commit has a reconstructed receipt")
                    .clone())
            }
            Err(error) => Err(DirectIngestFailureV1::recovery_required(
                error.to_string(),
                transaction,
                intent.hash(),
                true,
            )),
        }
    }
}

impl ProductionMemoryX {
    /// Execute one explicit, edge-free, direct-library batch transaction.
    /// Every item and resource decision is sealed before `.pending-*` exists;
    /// the already-held owner authority is borrowed and no nested lease can be
    /// acquired by this path.
    pub fn batch_ingest(
        &mut self,
        transaction_id: &str,
        semantic_time_unix_ns: u64,
        items: &[BatchIngestItemV1],
    ) -> Result<BatchIngestReceiptV1, BatchIngestFailureV1> {
        let transaction = transaction_id.to_owned();
        if validate_production_uuid(transaction_id).is_err() {
            return Err(BatchIngestFailureV1::new(
                DirectIngestFailureCodeV1::InvalidTransactionId,
                "transaction UUID is not canonical RFC-variant lowercase ASCII",
                None,
                None,
                None,
                false,
            ));
        }
        if semantic_time_unix_ns == 0 {
            return Err(BatchIngestFailureV1::new(
                DirectIngestFailureCodeV1::InvalidSemanticTime,
                "semantic time must be nonzero",
                Some(transaction),
                None,
                None,
                false,
            ));
        }
        let owner_lifetime_transactions = self.state.owner_lifetime_transactions.clone();
        let recovered = (|| -> io::Result<ProductionRuntimeStateV1> {
            let startup = self.authority.borrow_startup()?;
            production_open_runtime(&startup)
        })();
        let mut recovered = match recovered {
            Ok(state) => state,
            Err(error) => {
                return Err(BatchIngestFailureV1::new(
                    DirectIngestFailureCodeV1::RecoveryRequired,
                    error.to_string(),
                    Some(transaction),
                    None,
                    None,
                    false,
                ));
            }
        };
        recovered.owner_lifetime_transactions = owner_lifetime_transactions;
        self.state = recovered;
        if self
            .state
            .committed_transactions
            .contains_key(transaction_id)
            || self.state.update_transactions.contains_key(transaction_id)
        {
            return Err(BatchIngestFailureV1::new(
                DirectIngestFailureCodeV1::ConflictingTransactionReuse,
                "transaction identity belongs to a direct-v1 operation",
                Some(transaction),
                None,
                None,
                false,
            ));
        }
        if let Some(owner_transaction) = self
            .state
            .owner_lifetime_transactions
            .get(transaction_id)
            .cloned()
        {
            let receipt = match owner_transaction {
                ProductionOwnerLifetimeTransactionV1::Direct(_)
                | ProductionOwnerLifetimeTransactionV1::Update(_) => {
                    return Err(BatchIngestFailureV1::new(
                        DirectIngestFailureCodeV1::ConflictingTransactionReuse,
                        "transaction identity belongs to a direct-v1 operation",
                        Some(transaction),
                        None,
                        None,
                        false,
                    ));
                }
                ProductionOwnerLifetimeTransactionV1::Batch(receipt) => receipt,
            };
            let parent = ProductionCommittedHead {
                generation: receipt.committed_generation,
                commit_hash: receipt.commit_hash,
                logical_digest: receipt.logical_digest,
            };
            let retry = ProductionBaseBindingV1::from_identity(
                self.authority.physical_identity(),
                parent,
            )
            .and_then(|binding| {
                let intent = ProductionBatchIntentV1::create(binding.hash(), items)?;
                let _envelope = ProductionBatchEnvelopeV1::create(
                    transaction_id,
                    semantic_time_unix_ns,
                    binding.hash(),
                    intent.hash,
                )?;
                if receipt.semantic_time_unix_ns != semantic_time_unix_ns
                    || receipt.base_binding_hash != binding.hash()
                    || receipt.intent_hash != intent.hash
                {
                    return Err(invalid_data(
                        "owner-lifetime batch transaction identity was reused with a different intent",
                    ));
                }
                Ok(())
            });
            return match retry {
                Ok(()) => Ok(receipt),
                Err(error) => Err(BatchIngestFailureV1::new(
                    DirectIngestFailureCodeV1::ConflictingTransactionReuse,
                    error.to_string(),
                    Some(transaction),
                    Some(receipt.intent_hash),
                    None,
                    false,
                )),
            };
        }
        if let Some(committed) = self.state.batch_transactions.get(transaction_id).cloned() {
            let binding = ProductionBaseBindingV1::from_identity(
                self.authority.physical_identity(),
                committed.parent.clone(),
            )
            .map_err(|error| {
                BatchIngestFailureV1::new(
                    DirectIngestFailureCodeV1::UnsupportedOrCorrupt,
                    error.to_string(),
                    Some(transaction.clone()),
                    Some(committed.intent_hash),
                    None,
                    false,
                )
            })?;
            let intent =
                ProductionBatchIntentV1::create(binding.hash(), items).map_err(|error| {
                    BatchIngestFailureV1::new(
                        classify_batch_intent_error(&error),
                        error.to_string(),
                        Some(transaction.clone()),
                        Some(committed.intent_hash),
                        None,
                        false,
                    )
                })?;
            let envelope = ProductionBatchEnvelopeV1::create(
                transaction_id,
                semantic_time_unix_ns,
                binding.hash(),
                intent.hash,
            )
            .map_err(|error| {
                BatchIngestFailureV1::new(
                    DirectIngestFailureCodeV1::InvalidIntent,
                    error.to_string(),
                    Some(transaction.clone()),
                    Some(intent.hash),
                    None,
                    false,
                )
            })?;
            if committed.semantic_time_unix_ns != semantic_time_unix_ns
                || committed.base_binding_hash != binding.hash()
                || committed.intent_hash != intent.hash
                || committed.envelope_hash != envelope.hash
            {
                return Err(BatchIngestFailureV1::new(
                    DirectIngestFailureCodeV1::ConflictingTransactionReuse,
                    "transaction identity was reused with a divergent batch intent",
                    Some(transaction),
                    Some(intent.hash),
                    None,
                    false,
                ));
            }
            let parent_state = ProductionRuntimeStateV1 {
                head: committed.parent.clone(),
                base_binding: binding,
                admission_bytes: Vec::new(),
                atom: committed.parent_atoms.first().cloned(),
                atoms: committed.parent_atoms.clone(),
                history_leaves: committed.parent_history_leaves.clone(),
                graph_leaves: Vec::new(),
                superseded_by: BTreeMap::new(),
                committed_receipts: BTreeMap::new(),
                committed_transactions: BTreeMap::new(),
                batch_transactions: BTreeMap::new(),
                update_transactions: BTreeMap::new(),
                owner_lifetime_transactions: BTreeMap::new(),
            };
            let plan = production_plan_batch(
                self.authority.canonical_root(),
                &parent_state,
                &intent,
                &envelope,
            )
            .map_err(|error| {
                BatchIngestFailureV1::new(
                    DirectIngestFailureCodeV1::ConflictingTransactionReuse,
                    error.to_string(),
                    Some(transaction.clone()),
                    Some(intent.hash),
                    None,
                    false,
                )
            })?;
            if plan
                .outcomes
                .iter()
                .map(|outcome| outcome.decision_hash)
                .collect::<Vec<_>>()
                != committed.decision_hashes
            {
                return Err(BatchIngestFailureV1::new(
                    DirectIngestFailureCodeV1::ConflictingTransactionReuse,
                    "batch retry decisions differ from the immutable commit",
                    Some(transaction),
                    Some(intent.hash),
                    None,
                    false,
                ));
            }
            return BatchIngestReceiptV1::create(
                BatchIngestResultKindV1::Committed,
                transaction,
                semantic_time_unix_ns,
                intent.hash,
                intent.base_binding_hash,
                committed.parent.generation + 1,
                committed.commit_hash,
                committed.logical_digest,
                plan.outcomes,
                Some(committed.history_event_id),
            )
            .map_err(|error| {
                BatchIngestFailureV1::new(
                    DirectIngestFailureCodeV1::UnsupportedOrCorrupt,
                    error.to_string(),
                    Some(transaction_id.to_owned()),
                    Some(intent.hash),
                    None,
                    false,
                )
            });
        }

        let binding = self.state.base_binding.clone();
        let intent = ProductionBatchIntentV1::create(binding.hash(), items).map_err(|error| {
            BatchIngestFailureV1::new(
                classify_batch_intent_error(&error),
                error.to_string(),
                Some(transaction.clone()),
                None,
                None,
                false,
            )
        })?;
        let envelope = ProductionBatchEnvelopeV1::create(
            transaction_id,
            semantic_time_unix_ns,
            binding.hash(),
            intent.hash,
        )
        .map_err(|error| {
            BatchIngestFailureV1::new(
                DirectIngestFailureCodeV1::InvalidIntent,
                error.to_string(),
                Some(transaction.clone()),
                Some(intent.hash),
                None,
                false,
            )
        })?;
        let plan = production_plan_batch(
            self.authority.canonical_root(),
            &self.state,
            &intent,
            &envelope,
        )
        .map_err(|error| {
            BatchIngestFailureV1::new(
                DirectIngestFailureCodeV1::InvalidIntent,
                error.to_string(),
                Some(transaction.clone()),
                Some(intent.hash),
                None,
                false,
            )
        })?;
        if plan.created_ordinals.is_empty() {
            let receipt = BatchIngestReceiptV1::create(
                BatchIngestResultKindV1::Unchanged,
                transaction.clone(),
                semantic_time_unix_ns,
                intent.hash,
                binding.hash(),
                self.state.head.generation,
                self.state.head.commit_hash,
                self.state.head.logical_digest,
                plan.outcomes,
                None,
            )
            .map_err(|error| {
                BatchIngestFailureV1::new(
                    DirectIngestFailureCodeV1::UnsupportedOrCorrupt,
                    error.to_string(),
                    Some(transaction.clone()),
                    Some(intent.hash),
                    None,
                    false,
                )
            })?;
            self.state.owner_lifetime_transactions.insert(
                transaction,
                ProductionOwnerLifetimeTransactionV1::Batch(receipt.clone()),
            );
            return Ok(receipt);
        }

        let mut committed = false;
        let staged = (|| -> io::Result<PathBuf> {
            let write = self.authority.borrow_write()?;
            let published =
                production_stage_batch_ingest(&write, &self.state, &intent, &envelope, &plan)?;
            committed = true;
            let commit_bytes = read_bytes_bounded_under(
                write.canonical_root(),
                &published.join(COMMIT_FILE_NAME),
                MAX_CONTROL_RECORD_BYTES,
            )?;
            let manifest = BatchGenerationManifestV1::decode(&commit_bytes, "batch commit.bin")?;
            production_install_batch_generation(&write, &published, &manifest)?;
            Ok(published)
        })();
        if let Err(error) = staged {
            return Err(BatchIngestFailureV1::new(
                DirectIngestFailureCodeV1::RecoveryRequired,
                error.to_string(),
                Some(transaction),
                Some(intent.hash),
                None,
                committed,
            ));
        }
        let owner_lifetime_transactions = self.state.owner_lifetime_transactions.clone();
        let reopened = (|| -> io::Result<ProductionRuntimeStateV1> {
            let startup = self.authority.borrow_startup()?;
            production_open_runtime(&startup)
        })();
        let mut reopened = reopened.map_err(|error| {
            BatchIngestFailureV1::new(
                DirectIngestFailureCodeV1::RecoveryRequired,
                error.to_string(),
                Some(transaction.clone()),
                Some(intent.hash),
                None,
                true,
            )
        })?;
        reopened.owner_lifetime_transactions = owner_lifetime_transactions;
        self.state = reopened;
        let committed = self
            .state
            .batch_transactions
            .get(transaction_id)
            .ok_or_else(|| {
                BatchIngestFailureV1::new(
                    DirectIngestFailureCodeV1::UnsupportedOrCorrupt,
                    "reopened batch transaction is absent from the immutable ledger",
                    Some(transaction.clone()),
                    Some(intent.hash),
                    None,
                    true,
                )
            })?;
        BatchIngestReceiptV1::create(
            BatchIngestResultKindV1::Committed,
            transaction,
            semantic_time_unix_ns,
            intent.hash,
            binding.hash(),
            self.state.head.generation,
            committed.commit_hash,
            committed.logical_digest,
            plan.outcomes,
            Some(committed.history_event_id),
        )
        .map_err(|error| {
            BatchIngestFailureV1::new(
                DirectIngestFailureCodeV1::UnsupportedOrCorrupt,
                error.to_string(),
                Some(transaction_id.to_owned()),
                Some(intent.hash),
                None,
                true,
            )
        })
    }

    /// Commit one sealed direct-library successor and one source-free
    /// `SUPERSEDES(successor, old)` lineage edge. Recovery is completed before
    /// planning, and the existing live-owner lease is only borrowed.
    pub fn update_atom(
        &mut self,
        request: &UpdateAtomRequestV1,
    ) -> Result<UpdateAtomReceiptV1, UpdateAtomFailureV1> {
        let transaction = request.transaction_id.clone();
        if validate_production_uuid(&transaction).is_err() {
            return Err(UpdateAtomFailureV1::new(
                UpdateAtomFailureCodeV1::InvalidTransactionId,
                "transaction UUID is not canonical RFC-variant lowercase ASCII",
                None,
                None,
                false,
            ));
        }
        if request.semantic_time_unix_ns == 0 {
            return Err(UpdateAtomFailureV1::new(
                UpdateAtomFailureCodeV1::InvalidSemanticTime,
                "semantic time must be nonzero",
                Some(transaction),
                None,
                false,
            ));
        }
        let owner_lifetime_transactions = self.state.owner_lifetime_transactions.clone();
        let recovered = (|| -> io::Result<ProductionRuntimeStateV1> {
            let startup = self.authority.borrow_startup()?;
            production_open_runtime(&startup)
        })();
        let mut recovered = recovered.map_err(|error| {
            UpdateAtomFailureV1::new(
                UpdateAtomFailureCodeV1::RecoveryRequired,
                error.to_string(),
                Some(transaction.clone()),
                None,
                false,
            )
        })?;
        recovered.owner_lifetime_transactions = owner_lifetime_transactions;
        self.state = recovered;
        if self.state.committed_transactions.contains_key(&transaction)
            || self.state.batch_transactions.contains_key(&transaction)
            || matches!(
                self.state.owner_lifetime_transactions.get(&transaction),
                Some(
                    ProductionOwnerLifetimeTransactionV1::Direct(_)
                        | ProductionOwnerLifetimeTransactionV1::Batch(_)
                )
            )
        {
            return Err(UpdateAtomFailureV1::new(
                UpdateAtomFailureCodeV1::ConflictingTransactionReuse,
                "transaction identity belongs to another operation kind",
                Some(transaction),
                None,
                false,
            ));
        }
        let intent = ProductionUpdateIntentV1::create(request).map_err(|error| {
            let text = error.to_string();
            let code = if text.contains("same_atom_id") {
                UpdateAtomFailureCodeV1::SameAtomIdUpdate
            } else if text.contains("provenance_projection_conflict") {
                UpdateAtomFailureCodeV1::ProvenanceProjectionConflict
            } else if text.contains("resource") || text.contains("bound") {
                UpdateAtomFailureCodeV1::ResourceLimitExceeded
            } else {
                UpdateAtomFailureCodeV1::InvalidSuccessor
            };
            UpdateAtomFailureV1::new(code, text, Some(transaction.clone()), None, false)
        })?;
        if let Some(committed) = self.state.update_transactions.get(&transaction) {
            let receipt = committed.receipt.clone();
            let binding = ProductionBaseBindingV1::from_identity(
                self.authority.physical_identity(),
                ProductionCommittedHead {
                    generation: receipt.parent_generation,
                    commit_hash: receipt.parent_commit_hash,
                    logical_digest: {
                        if receipt.parent_generation == 0 {
                            production_empty_baseline_digest(self.authority.canonical_root())
                                .map_err(|error| {
                                    UpdateAtomFailureV1::new(
                                        UpdateAtomFailureCodeV1::UnsupportedOrCorrupt,
                                        error.to_string(),
                                        Some(transaction.clone()),
                                        Some(receipt.intent_hash),
                                        false,
                                    )
                                })?
                        } else {
                            let parent_dir = production_generation_path(
                                self.authority.canonical_root(),
                                receipt.parent_generation,
                            );
                            let parent_bytes = read_bytes_bounded_under(
                                self.authority.canonical_root(),
                                &parent_dir.join(COMMIT_FILE_NAME),
                                MAX_CONTROL_RECORD_BYTES,
                            )
                            .map_err(|error| {
                                UpdateAtomFailureV1::new(
                                    UpdateAtomFailureCodeV1::UnsupportedOrCorrupt,
                                    error.to_string(),
                                    Some(transaction.clone()),
                                    Some(receipt.intent_hash),
                                    false,
                                )
                            })?;
                            let value: serde_json::Value = serde_json::from_slice(&parent_bytes)
                                .map_err(|error| {
                                    UpdateAtomFailureV1::new(
                                        UpdateAtomFailureCodeV1::UnsupportedOrCorrupt,
                                        error.to_string(),
                                        Some(transaction.clone()),
                                        Some(receipt.intent_hash),
                                        false,
                                    )
                                })?;
                            let parent_logical_digest = value
                                .get("logical_state_digest")
                                .and_then(serde_json::Value::as_str)
                                .ok_or_else(|| {
                                    UpdateAtomFailureV1::new(
                                        UpdateAtomFailureCodeV1::UnsupportedOrCorrupt,
                                        "parent logical digest is absent",
                                        Some(transaction.clone()),
                                        Some(receipt.intent_hash),
                                        false,
                                    )
                                })?;
                            parse_hash_hex(parent_logical_digest, "parent logical digest").map_err(
                                |error| {
                                    UpdateAtomFailureV1::new(
                                        UpdateAtomFailureCodeV1::UnsupportedOrCorrupt,
                                        error.to_string(),
                                        Some(transaction.clone()),
                                        Some(receipt.intent_hash),
                                        false,
                                    )
                                },
                            )?
                        }
                    },
                },
            )
            .map_err(|error| {
                UpdateAtomFailureV1::new(
                    UpdateAtomFailureCodeV1::UnsupportedOrCorrupt,
                    error.to_string(),
                    Some(transaction.clone()),
                    Some(receipt.intent_hash),
                    false,
                )
            })?;
            let envelope = ProductionUpdateEnvelopeV1::create(
                &transaction,
                request.semantic_time_unix_ns,
                binding.hash(),
                intent.hash,
            )
            .map_err(|error| {
                UpdateAtomFailureV1::new(
                    UpdateAtomFailureCodeV1::ConflictingTransactionReuse,
                    error.to_string(),
                    Some(transaction.clone()),
                    Some(intent.hash),
                    false,
                )
            })?;
            let successor_provenance_hash = production_sha256(&request.successor_provenance);
            if request.semantic_time_unix_ns != receipt.semantic_time_unix_ns
                || binding.hash() != receipt.base_binding_hash
                || intent.hash != receipt.intent_hash
                || envelope.hash != receipt.envelope_hash
                || intent.successor_body_hash != committed.successor_body_hash
                || intent.claim_projection_hash != committed.claim_projection_hash
                || intent.api_evidence_projection_hash != committed.api_evidence_projection_hash
                || intent.successor_source_attachment_hash
                    != committed.successor_source_attachment_hash
                || successor_provenance_hash != receipt.successor_provenance_hash
            {
                return Err(UpdateAtomFailureV1::new(
                    UpdateAtomFailureCodeV1::ConflictingTransactionReuse,
                    "transaction identity was reused with a divergent update intent",
                    Some(transaction),
                    Some(intent.hash),
                    false,
                ));
            }
            return Ok(receipt);
        }
        let binding = self.state.base_binding.clone();
        let envelope = ProductionUpdateEnvelopeV1::create(
            &transaction,
            request.semantic_time_unix_ns,
            binding.hash(),
            intent.hash,
        )
        .map_err(|error| {
            UpdateAtomFailureV1::new(
                UpdateAtomFailureCodeV1::InvalidTransactionId,
                error.to_string(),
                Some(transaction.clone()),
                Some(intent.hash),
                false,
            )
        })?;
        let plan = production_plan_update(
            self.authority.canonical_root(),
            &self.state,
            request,
            &intent,
            &envelope,
        )
        .map_err(|error| {
            let text = error.to_string();
            let code = if text.contains("old_atom_missing") {
                UpdateAtomFailureCodeV1::OldAtomMissing
            } else if text.contains("already_superseded") {
                UpdateAtomFailureCodeV1::AlreadySuperseded
            } else if text.contains("ambiguous_supersession") {
                UpdateAtomFailureCodeV1::AmbiguousSupersessionState
            } else if text.contains("relation_backed") {
                UpdateAtomFailureCodeV1::RelationBackedAtomRequiresCompositeOperation
            } else if text.contains("successor_collision") {
                UpdateAtomFailureCodeV1::SuccessorCollision
            } else if text.contains("provenance") {
                UpdateAtomFailureCodeV1::ProvenanceProjectionConflict
            } else if text.contains("graph_compaction") {
                UpdateAtomFailureCodeV1::GraphCompactionRequired
            } else if text.contains("resource") || text.contains("bound") {
                UpdateAtomFailureCodeV1::ResourceLimitExceeded
            } else {
                UpdateAtomFailureCodeV1::UnsupportedOrCorrupt
            };
            UpdateAtomFailureV1::new(
                code,
                text,
                Some(transaction.clone()),
                Some(intent.hash),
                false,
            )
        })?;
        let published = {
            let write = self.authority.borrow_write().map_err(|error| {
                UpdateAtomFailureV1::new(
                    UpdateAtomFailureCodeV1::RecoveryRequired,
                    error.to_string(),
                    Some(transaction.clone()),
                    Some(intent.hash),
                    false,
                )
            })?;
            match production_stage_update(&write, &plan, &envelope) {
                Ok(published) => {
                    production_install_update_generation(&write, &published, &plan.manifest)
                        .map_err(|error| {
                            UpdateAtomFailureV1::new(
                                UpdateAtomFailureCodeV1::RecoveryRequired,
                                error.to_string(),
                                Some(transaction.clone()),
                                Some(intent.hash),
                                true,
                            )
                        })?;
                    published
                }
                Err(error) => {
                    let committed = path_entry_exists(&production_generation_path(
                        write.canonical_root(),
                        plan.generation,
                    ))
                    .unwrap_or(false);
                    return Err(UpdateAtomFailureV1::new(
                        UpdateAtomFailureCodeV1::RecoveryRequired,
                        error.to_string(),
                        Some(transaction.clone()),
                        Some(intent.hash),
                        committed,
                    ));
                }
            }
        };
        let _ = published;
        let owner_lifetime_transactions = self.state.owner_lifetime_transactions.clone();
        let mut reopened = {
            let startup = self.authority.borrow_startup().map_err(|error| {
                UpdateAtomFailureV1::new(
                    UpdateAtomFailureCodeV1::RecoveryRequired,
                    error.to_string(),
                    Some(transaction.clone()),
                    Some(intent.hash),
                    true,
                )
            })?;
            production_open_runtime(&startup).map_err(|error| {
                UpdateAtomFailureV1::new(
                    UpdateAtomFailureCodeV1::RecoveryRequired,
                    error.to_string(),
                    Some(transaction.clone()),
                    Some(intent.hash),
                    true,
                )
            })?
        };
        reopened.owner_lifetime_transactions = owner_lifetime_transactions;
        let receipt = reopened
            .update_transactions
            .get(&transaction)
            .map(|committed| committed.receipt.clone())
            .ok_or_else(|| {
                UpdateAtomFailureV1::new(
                    UpdateAtomFailureCodeV1::UnsupportedOrCorrupt,
                    "reopened update transaction is absent from the immutable ledger",
                    Some(transaction.clone()),
                    Some(intent.hash),
                    true,
                )
            })?;
        reopened.owner_lifetime_transactions.insert(
            transaction,
            ProductionOwnerLifetimeTransactionV1::Update(Box::new(receipt.clone())),
        );
        self.state = reopened;
        Ok(receipt)
    }

    pub fn committed_atom_ids(&self) -> Vec<AtomId> {
        self.state.atoms.iter().map(|atom| atom.atom_id).collect()
    }

    pub fn current_atom_ids(&self) -> Vec<AtomId> {
        self.state
            .atoms
            .iter()
            .filter(|atom| !self.state.superseded_by.contains_key(&atom.atom_id))
            .map(|atom| atom.atom_id)
            .collect()
    }

    pub fn successor_for(&self, old_atom_id: AtomId) -> Option<AtomId> {
        self.state.superseded_by.get(&old_atom_id).copied()
    }
}

fn production_read_control<T>(
    root: &Path,
    path: &Path,
    label: &str,
    decode: impl FnOnce(&[u8], &str) -> io::Result<T>,
) -> io::Result<(T, Vec<u8>)> {
    let bytes = read_bytes_bounded_under(root, path, MAX_CONTROL_RECORD_BYTES)?;
    let value = decode(&bytes, label)?;
    Ok((value, bytes))
}

fn production_validate_format(record: &ProductionFormatRecordV2) -> io::Result<()> {
    if record.schema != PRODUCTION_FORMAT_SCHEMA
        || record.version != 1
        || record.format_version != 2
        || record.codec_id != PRODUCTION_CODEC_ID
        || record.registry_id != PRODUCTION_REGISTRY_ID
        || record.digest_id != PRODUCTION_DIGEST_ID
        || record.component_root_id != PRODUCTION_COMPONENT_ROOT_ID
        || record.orphan_digest_id != PRODUCTION_ORPHAN_DIGEST_ID
        || record.limits_id != PRODUCTION_LIMITS_ID
        || record.legacy_layout_id != PRODUCTION_LEGACY_LAYOUT_ID
        || record.downgrade_policy_id != PRODUCTION_DOWNGRADE_POLICY_ID
        || record.minimum_writer_capability != PRODUCTION_MINIMUM_WRITER
        || !is_hash(&record.baseline_hash)
        || !is_hash(&record.migration_hash)
    {
        return Err(invalid_data(
            "production format.v2 record is unsupported or corrupt",
        ));
    }
    Ok(())
}

fn production_validate_migration(
    record: &ProductionMigrationRecordV1,
    baseline_hash: &str,
) -> io::Result<()> {
    if record.schema != "memoryx.production-migration.v1"
        || record.version != 1
        || record.source_layout_id != PRODUCTION_LEGACY_LAYOUT_ID
        || record.target_format_version != 2
        || record.registry_id != PRODUCTION_REGISTRY_ID
        || record.limits_id != PRODUCTION_LIMITS_ID
        || record.baseline_manifest_hash != baseline_hash
        || !is_hash(&record.backup_manifest_hash)
        || !record.source_untouched
        || record.rollback_policy != PRODUCTION_ROLLBACK_POLICY
        || record.first_commit_generation != 0
    {
        return Err(invalid_data(
            "production migration.v2 record is unsupported or corrupt",
        ));
    }
    Ok(())
}

fn production_validate_startup_admission(
    record: &ProductionStartupAdmissionV1,
    binding: &ProductionBaseBindingV1,
    head: &ProductionCommittedHead,
) -> io::Result<()> {
    if record.schema != PRODUCTION_STARTUP_SCHEMA
        || record.version != 1
        || record.format_version != 2
        || record.classification != "production_v2"
        || record.codec_id != PRODUCTION_CODEC_ID
        || record.registry_id != PRODUCTION_REGISTRY_ID
        || record.limits_id != PRODUCTION_LIMITS_ID
        || record.base_binding_hash != hex_lower(&binding.hash())
        || record.head_generation != head.generation
        || record.head_commit_hash != hex_lower(&head.commit_hash)
        || record.head_logical_digest != hex_lower(&head.logical_digest)
        || record.install_state != "installed_state_verified"
        || record.component_open_mode != "open_existing_no_repair"
        || record.live_view_state != "not_published"
        || (record.head_generation == 0) != (record.head_commit_hash == PRODUCTION_ZERO_HASH)
    {
        return Err(invalid_data(
            "production startup admission fields are invalid",
        ));
    }
    Ok(())
}

fn production_hash_bytes(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

fn production_hash_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn production_append_u32_frame(target: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
    let length = u32::try_from(bytes.len())
        .map_err(|_| invalid_data("production u32 frame length overflow"))?;
    target.extend_from_slice(&length.to_le_bytes());
    target.extend_from_slice(bytes);
    Ok(())
}

fn production_component_root(
    generation: u64,
    components: &[ProductionComponentDescriptorV1],
    pairs: &[ProductionPairDescriptorV1],
) -> io::Result<[u8; 32]> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(PRODUCTION_COMPONENT_ROOT_ID.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&2u32.to_le_bytes());
    bytes.extend_from_slice(&generation.to_le_bytes());
    production_append_u32_frame(&mut bytes, PRODUCTION_REGISTRY_ID.as_bytes())?;
    bytes.extend_from_slice(
        &u64::try_from(components.len())
            .map_err(|_| invalid_data("production component count overflow"))?
            .to_le_bytes(),
    );
    for component in components {
        production_append_u32_frame(&mut bytes, &component.canonical_bytes()?)?;
    }
    bytes.extend_from_slice(
        &u64::try_from(pairs.len())
            .map_err(|_| invalid_data("production pair count overflow"))?
            .to_le_bytes(),
    );
    for pair in pairs {
        production_append_u32_frame(&mut bytes, &pair.canonical_bytes()?)?;
    }
    Ok(production_hash_bytes(&bytes))
}

fn production_orphan_inventory_digest(orphans: &[CasOrphanDescriptorV1]) -> io::Result<[u8; 32]> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(PRODUCTION_ORPHAN_DIGEST_ID.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(
        &u64::try_from(orphans.len())
            .map_err(|_| invalid_data("production orphan count overflow"))?
            .to_le_bytes(),
    );
    for orphan in orphans {
        production_append_u32_frame(&mut bytes, &orphan.canonical_bytes()?)?;
    }
    Ok(production_hash_bytes(&bytes))
}

fn production_metadata_leaf(
    atom_id: &AtomId,
    node_num: u64,
    atom_type: AtomType,
    created_at_ns: u64,
    trust_level: u16,
    domain_mask: u64,
    source_id: u32,
) -> io::Result<Vec<u8>> {
    if node_num == u64::MAX || trust_level > 10_000 || !(1..=13).contains(&atom_type.to_u32()) {
        return Err(invalid_data(
            "production metadata semantic fields are invalid",
        ));
    }
    let mut bytes = Vec::with_capacity(102);
    bytes.extend_from_slice(PRODUCTION_METADATA_LEAF_ID.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(atom_id);
    bytes.extend_from_slice(&node_num.to_le_bytes());
    bytes.extend_from_slice(&atom_type.to_u32().to_le_bytes());
    bytes.extend_from_slice(&created_at_ns.to_le_bytes());
    bytes.extend_from_slice(&trust_level.to_le_bytes());
    bytes.extend_from_slice(&domain_mask.to_le_bytes());
    bytes.extend_from_slice(&source_id.to_le_bytes());
    if bytes.len() != 102 {
        return Err(invalid_data(
            "production metadata leaf width is not 102 bytes",
        ));
    }
    Ok(bytes)
}

fn production_zero_provenance_leaf(atom_id: &AtomId) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(PRODUCTION_PROVENANCE_LEAF_ID.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(atom_id);
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes
}

fn production_anchor_leaves(root: &Path) -> io::Result<Vec<(u16, bool, Vec<u8>)>> {
    let mut anchors = Vec::new();
    for entry in PRODUCTION_DIRECT_REGISTRY
        .iter()
        .filter(|entry| (150..=220).contains(&entry.order))
    {
        let relative = entry.target.expect("anchor registry target");
        let path = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(invalid_data("production anchor is not a regular file"));
                }
                let bytes = read_bytes_bounded_under(root, &path, 17_179_869_184)?;
                anchors.push((entry.order, true, bytes));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                anchors.push((entry.order, false, Vec::new()));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(anchors)
}

fn production_anchor_leaves_from_descriptors(
    root: &Path,
    descriptors: &[ProductionComponentDescriptorV1],
) -> io::Result<Vec<(u16, bool, Vec<u8>)>> {
    let mut anchors = Vec::new();
    for entry in PRODUCTION_DIRECT_REGISTRY
        .iter()
        .filter(|entry| (150..=220).contains(&entry.order))
    {
        let descriptor = descriptors
            .iter()
            .find(|descriptor| {
                descriptor.registry_order == entry.order && descriptor.registry_key == entry.key
            })
            .ok_or_else(|| invalid_data("production generation anchor descriptor is absent"))?;
        match descriptor.mode.as_str() {
            "anchor_absent" if descriptor.byte_length == 0 => {
                anchors.push((entry.order, false, Vec::new()));
            }
            "anchor_present" => {
                let relative = descriptor
                    .target_path
                    .as_deref()
                    .ok_or_else(|| invalid_data("production anchor target is absent"))?;
                let bytes = read_bytes_bounded_under(
                    root,
                    &root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR)),
                    PRODUCTION_UPDATE_MAX_COMPONENT_BYTES,
                )?;
                if bytes.len() as u64 != descriptor.byte_length
                    || production_hash_hex(&bytes) != descriptor.byte_hash
                {
                    return Err(invalid_data(
                        "production historical anchor bytes are unavailable",
                    ));
                }
                anchors.push((entry.order, true, bytes));
            }
            _ => return Err(invalid_data("production anchor descriptor is invalid")),
        }
    }
    Ok(anchors)
}

fn production_logical_digest(
    generation: u64,
    parent_commit_hash: [u8; 32],
    atom: Option<&ProductionAtomStateV1>,
    anchors: &[(u16, bool, Vec<u8>)],
) -> io::Result<[u8; 32]> {
    let atoms = atom.into_iter().collect::<Vec<_>>();
    let histories = atom
        .into_iter()
        .map(|atom| atom.history_leaf.as_slice())
        .collect::<Vec<_>>();
    production_logical_digest_multi(generation, parent_commit_hash, &atoms, &histories, anchors)
}

fn production_logical_digest_multi(
    generation: u64,
    parent_commit_hash: [u8; 32],
    atoms: &[&ProductionAtomStateV1],
    histories: &[&[u8]],
    anchors: &[(u16, bool, Vec<u8>)],
) -> io::Result<[u8; 32]> {
    production_logical_digest_multi_with_graph(
        generation,
        parent_commit_hash,
        atoms,
        &[],
        histories,
        anchors,
    )
}

fn production_logical_digest_multi_with_graph(
    generation: u64,
    parent_commit_hash: [u8; 32],
    atoms: &[&ProductionAtomStateV1],
    graph_leaves: &[&[u8]],
    histories: &[&[u8]],
    anchors: &[(u16, bool, Vec<u8>)],
) -> io::Result<[u8; 32]> {
    if generation > PRODUCTION_MAX_GENERATIONS || anchors.len() != 8 {
        return Err(invalid_data("production logical digest bounds are invalid"));
    }
    if atoms.len() > 100_000_000
        || graph_leaves.len() > 100_000_000
        || histories.len() > PRODUCTION_MAX_GENERATIONS as usize
    {
        return Err(invalid_data(
            "production logical state count exceeds its bound",
        ));
    }
    let atom_count = atoms.len() as u64;
    let graph_leaf_count = graph_leaves.len() as u64;
    let history_count = histories.len() as u64;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(PRODUCTION_DIGEST_ID.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&2u32.to_le_bytes());
    bytes.extend_from_slice(&generation.to_le_bytes());
    bytes.extend_from_slice(&parent_commit_hash);
    bytes.extend_from_slice(&atom_count.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&graph_leaf_count.to_le_bytes());
    bytes.extend_from_slice(&atom_count.to_le_bytes());
    bytes.extend_from_slice(&history_count.to_le_bytes());
    bytes.extend_from_slice(&(anchors.len() as u64).to_le_bytes());
    let mut ordered_atoms = atoms.to_vec();
    ordered_atoms.sort_by_key(|atom| atom.atom_id);
    for atom in &ordered_atoms {
        let metadata_leaf = production_metadata_leaf(
            &atom.atom_id,
            atom.node_num,
            atom.atom_type,
            atom.created_at_ns,
            atom.trust_level,
            atom.domain_mask,
            atom.source_id,
        )?;
        bytes.push(0x01);
        bytes.extend_from_slice(&atom.atom_id);
        bytes.push(1);
        bytes.extend_from_slice(&atom.body_len.to_le_bytes());
        bytes.extend_from_slice(&atom.body_crc32.to_le_bytes());
        bytes.extend_from_slice(&atom.body_hash);
        bytes.extend_from_slice(&atom.segment_id.to_le_bytes());
        bytes.extend_from_slice(&atom.record_offset.to_le_bytes());
        bytes.extend_from_slice(&atom.record_extent_len.to_le_bytes());
        bytes.extend_from_slice(&atom.node_num.to_le_bytes());
        bytes.extend_from_slice(&atom.domain_mask.to_le_bytes());
        bytes.push(0);
        bytes.extend_from_slice(blake3::hash(&metadata_leaf).as_bytes());
        bytes.extend_from_slice(&atom.provenance_hash);
    }
    let graph_prefix_len = PRODUCTION_GRAPH_LEAF_ID.len() + 1;
    let mut ordered_graph = graph_leaves.to_vec();
    ordered_graph.sort_by_key(|leaf| {
        let src = leaf
            .get(graph_prefix_len + 2..graph_prefix_len + 10)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u64::from_le_bytes)
            .unwrap_or(u64::MAX);
        let edge_type = leaf
            .get(graph_prefix_len + 10..graph_prefix_len + 14)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_le_bytes)
            .unwrap_or(u32::MAX);
        let dst = leaf
            .get(graph_prefix_len + 14..graph_prefix_len + 22)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u64::from_le_bytes)
            .unwrap_or(u64::MAX);
        (src, edge_type, dst)
    });
    for pair in ordered_graph.windows(2) {
        if pair[0] == pair[1] {
            return Err(invalid_data("production graph semantic leaf is duplicated"));
        }
    }
    for leaf in ordered_graph {
        if leaf.len() != 87
            || !leaf.starts_with(PRODUCTION_GRAPH_LEAF_ID.as_bytes())
            || leaf.get(PRODUCTION_GRAPH_LEAF_ID.len()) != Some(&0)
            || leaf.get(graph_prefix_len..graph_prefix_len + 2) != Some(&1u16.to_le_bytes())
        {
            return Err(invalid_data("production graph semantic leaf is invalid"));
        }
        bytes.push(0x03);
        production_append_u32_frame(&mut bytes, leaf)?;
    }
    for atom in &ordered_atoms {
        let metadata_leaf = production_metadata_leaf(
            &atom.atom_id,
            atom.node_num,
            atom.atom_type,
            atom.created_at_ns,
            atom.trust_level,
            atom.domain_mask,
            atom.source_id,
        )?;
        bytes.push(0x04);
        production_append_u32_frame(&mut bytes, &metadata_leaf)?;
    }
    for history in histories {
        bytes.push(0x05);
        production_append_u32_frame(&mut bytes, history)?;
    }
    for (order, present, anchor_bytes) in anchors {
        bytes.push(0x06);
        bytes.extend_from_slice(&order.to_le_bytes());
        bytes.push(u8::from(*present));
        bytes.extend_from_slice(&(anchor_bytes.len() as u64).to_le_bytes());
        bytes.extend_from_slice(blake3::hash(anchor_bytes).as_bytes());
    }
    Ok(production_hash_bytes(&bytes))
}

fn production_empty_baseline_digest(root: &Path) -> io::Result<[u8; 32]> {
    let anchors = production_anchor_leaves(root)?;
    production_logical_digest(0, [0; 32], None, &anchors)
}

fn production_stage_path(order: u16, ordinal: u32) -> String {
    format!("components/{order:03}_{ordinal:08}.bin")
}

fn production_planned_components(root: &Path) -> io::Result<Vec<ProductionPlannedComponentV1>> {
    let mut planned = Vec::new();
    for entry in PRODUCTION_DIRECT_REGISTRY {
        let mode = if (150..=220).contains(&entry.order) {
            let target = root.join(entry.target.expect("anchor target"));
            match fs::symlink_metadata(target) {
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                    "anchor_present"
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => "anchor_absent",
                Ok(_) => return Err(invalid_data("production anchor is not a regular file")),
                Err(error) => return Err(error),
            }
        } else {
            entry.mode
        };
        let stage_path =
            matches!(mode, "replace" | "orphan").then(|| production_stage_path(entry.order, 0));
        planned.push(ProductionPlannedComponentV1 {
            registry_key: entry.key.to_owned(),
            registry_order: entry.order,
            ordinal: 0,
            mode: mode.to_owned(),
            target_path: entry.target.map(ToOwned::to_owned),
            stage_path,
            content_codec_id: entry.codec.to_owned(),
            pair_id: entry.pair_id.map(ToOwned::to_owned),
        });
    }
    Ok(planned)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    use base64::Engine as _;
    use tempfile::tempdir;

    use super::*;

    const TX1: &str = "019fca57-b841-79a2-88e5-e6b78a52e550";
    const TX2: &str = "019fca57-b841-79a2-88e5-e6b78a52e551";

    fn production_empty_idx1() -> Vec<u8> {
        let header = IndexFileHeader::new(0);
        let bloom = BloomFilter::new(1024).to_bytes();
        let mut bytes = unsafe {
            std::slice::from_raw_parts(
                &header as *const IndexFileHeader as *const u8,
                IndexFileHeader::SIZE,
            )
            .to_vec()
        };
        bytes.extend_from_slice(&(bloom.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&bloom);
        bytes
    }

    fn production_empty_component_bytes(key: &str) -> Vec<u8> {
        match key {
            "cas.segment-data.skf1.v1" | "meta.history-once.v1" => Vec::new(),
            "cas.segment-index.idx1.v1" => production_empty_idx1(),
            "index.location-state.loc1.v1" => {
                let mut bytes = Vec::new();
                bytes.extend_from_slice(&0x4c4f4331u32.to_le_bytes());
                bytes.extend_from_slice(&1u16.to_le_bytes());
                bytes.push(8);
                bytes.push(0);
                bytes.extend_from_slice(&0u64.to_le_bytes());
                bytes.extend_from_slice(&0u64.to_le_bytes());
                bytes
            }
            "index.idloc.idl1.v1" => IdLocBuilder::new(8).build_to_vec(),
            "index.lexicon.lex1.v1" => {
                let mut bytes = vec![0; LexHeader::SIZE];
                assert!(LexHeader::new(128).write_to_bytes(&mut bytes));
                bytes
            }
            "index.postings.pst1.v1" => {
                let mut bytes = vec![0; PostHeader::SIZE];
                assert!(PostHeader::new().write_to_bytes(&mut bytes));
                bytes
            }
            "graph.manifest.v1" => production_graph_manifest_bytes(0).unwrap(),
            "meta.atom-state.met1.v1" => {
                let mut bytes = Vec::new();
                bytes.extend_from_slice(&0x4d455431u32.to_le_bytes());
                bytes.extend_from_slice(&1u16.to_le_bytes());
                bytes.extend_from_slice(&0u16.to_le_bytes());
                bytes.extend_from_slice(&0u64.to_le_bytes());
                bytes
            }
            _ => panic!("unsupported empty production component {key}"),
        }
    }

    fn activate_empty_production_base(root: &Path) {
        fs::create_dir_all(root).unwrap();
        let txn_root = production_txn_root(root);
        fs::create_dir_all(txn_root.join(GENERATIONS_DIR_NAME)).unwrap();
        let baseline_keys = [
            "cas.segment-data.skf1.v1",
            "cas.segment-index.idx1.v1",
            "index.location-state.loc1.v1",
            "index.idloc.idl1.v1",
            "index.lexicon.lex1.v1",
            "index.postings.pst1.v1",
            "graph.manifest.v1",
            "meta.atom-state.met1.v1",
            "meta.history-once.v1",
        ];
        let mut components = Vec::new();
        for key in baseline_keys {
            let entry = production_registry_entry(key).unwrap();
            let bytes = production_empty_component_bytes(key);
            let target = entry.target.unwrap();
            let path = root.join(target);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, &bytes).unwrap();
            components.push(
                production_component_descriptor(
                    entry,
                    0,
                    "baseline_present",
                    Some(target.to_owned()),
                    None,
                    &bytes,
                    if key == "cas.segment-data.skf1.v1" {
                        0
                    } else {
                        u64::from(key != "meta.history-once.v1")
                    },
                )
                .unwrap(),
            );
        }
        for entry in PRODUCTION_DIRECT_REGISTRY
            .iter()
            .filter(|entry| (150..=220).contains(&entry.order))
        {
            components.push(
                production_component_descriptor(
                    entry,
                    0,
                    "baseline_absent",
                    entry.target.map(ToOwned::to_owned),
                    None,
                    &[],
                    0,
                )
                .unwrap(),
            );
        }
        components.sort_by(|left, right| {
            (left.registry_order, &left.target_path, left.ordinal).cmp(&(
                right.registry_order,
                &right.target_path,
                right.ordinal,
            ))
        });
        let by_key = |key: &str| {
            components
                .iter()
                .find(|component| component.registry_key == key)
                .unwrap()
        };
        let pairs = vec![
            production_pair_descriptor(
                "memoryx.location-idloc-pair.v1",
                by_key("index.location-state.loc1.v1"),
                by_key("index.idloc.idl1.v1"),
                0,
                0,
            )
            .unwrap(),
            production_pair_descriptor(
                "memoryx.lexical-postings-pair.v1",
                by_key("index.lexicon.lex1.v1"),
                by_key("index.postings.pst1.v1"),
                0,
                0,
            )
            .unwrap(),
        ];
        let logical = production_empty_baseline_digest(root).unwrap();
        let component_root = production_component_root(0, &components, &pairs).unwrap();
        let total_bytes = components
            .iter()
            .map(|component| component.byte_length)
            .sum();
        let baseline = ProductionBaselineManifestV1::from_body(ProductionBaselineManifestBodyV1 {
            schema: "memoryx.production-baseline-manifest.v1".to_owned(),
            version: 1,
            format_version: 2,
            source_layout_id: PRODUCTION_LEGACY_LAYOUT_ID.to_owned(),
            registry_id: PRODUCTION_REGISTRY_ID.to_owned(),
            limits_id: PRODUCTION_LIMITS_ID.to_owned(),
            component_root_hash: hex_lower(&component_root),
            legacy_semantic_digest: hex_lower(&logical),
            root_tree_digest: hex_lower(&component_root),
            component_count: components.len() as u64,
            total_bytes,
            components,
            pairs,
        })
        .unwrap();
        let baseline_bytes = baseline.canonical_bytes().unwrap();
        production_write_new(
            &txn_root.join(PRODUCTION_BASELINE_FILE_NAME),
            &baseline_bytes,
        )
        .unwrap();
        let baseline_hash = production_hash_hex(&baseline_bytes);
        let migration = ProductionMigrationRecordV1::from_body(ProductionMigrationBodyV1 {
            schema: "memoryx.production-migration.v1".to_owned(),
            version: 1,
            source_layout_id: PRODUCTION_LEGACY_LAYOUT_ID.to_owned(),
            target_format_version: 2,
            registry_id: PRODUCTION_REGISTRY_ID.to_owned(),
            limits_id: PRODUCTION_LIMITS_ID.to_owned(),
            baseline_manifest_hash: baseline_hash.clone(),
            backup_manifest_hash: production_hash_hex(b"test-only-backup-manifest"),
            component_count: baseline.component_count,
            total_bytes: baseline.total_bytes,
            required_free_bytes: baseline.total_bytes + 268_435_456,
            source_untouched: true,
            rollback_policy: PRODUCTION_ROLLBACK_POLICY.to_owned(),
            first_commit_generation: 0,
        })
        .unwrap();
        let migration_bytes = migration.canonical_bytes().unwrap();
        production_write_new(
            &txn_root.join(PRODUCTION_MIGRATION_FILE_NAME),
            &migration_bytes,
        )
        .unwrap();
        let format = ProductionFormatRecordV2::from_body(ProductionFormatBodyV2 {
            schema: PRODUCTION_FORMAT_SCHEMA.to_owned(),
            version: 1,
            format_version: 2,
            codec_id: PRODUCTION_CODEC_ID.to_owned(),
            registry_id: PRODUCTION_REGISTRY_ID.to_owned(),
            digest_id: PRODUCTION_DIGEST_ID.to_owned(),
            component_root_id: PRODUCTION_COMPONENT_ROOT_ID.to_owned(),
            orphan_digest_id: PRODUCTION_ORPHAN_DIGEST_ID.to_owned(),
            limits_id: PRODUCTION_LIMITS_ID.to_owned(),
            legacy_layout_id: PRODUCTION_LEGACY_LAYOUT_ID.to_owned(),
            downgrade_policy_id: PRODUCTION_DOWNGRADE_POLICY_ID.to_owned(),
            baseline_hash,
            migration_hash: production_hash_hex(&migration_bytes),
            minimum_writer_capability: PRODUCTION_MINIMUM_WRITER.to_owned(),
        })
        .unwrap();
        production_write_new(
            &txn_root.join(PRODUCTION_FORMAT_FILE_NAME),
            &format.canonical_bytes().unwrap(),
        )
        .unwrap();
    }

    fn minimal_production_atom() -> &'static [u8] {
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/ORCHESTRATION_SYSTEM/modules/MX-80-schemas-migrations/evidence/fixtures/",
            "n5b_p0c_production_v2_codec_v1/positive/atom_body_minimal_v1.bin"
        ))
    }

    fn production_codec_fixture(relative: &str) -> Vec<u8> {
        fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("ORCHESTRATION_SYSTEM/modules/MX-80-schemas-migrations/evidence/fixtures")
                .join("n5b_p0c_production_v2_codec_v1")
                .join(relative),
        )
        .unwrap()
    }

    fn production_batch_fixture(relative: &str) -> Vec<u8> {
        fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("ORCHESTRATION_SYSTEM/modules/MX-80-schemas-migrations/evidence/fixtures")
                .join("n5b_p0c_batch_edgefree_v1")
                .join(relative),
        )
        .unwrap()
    }

    fn production_logical_fixture(relative: &str) -> Vec<u8> {
        fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("ORCHESTRATION_SYSTEM/modules/MX-80-schemas-migrations/evidence/fixtures")
                .join("n5b_p0c_logical_digest_v2")
                .join(relative),
        )
        .unwrap()
    }

    fn production_body_variant(atom_type: u32, created_at_unix_ns: u64) -> Vec<u8> {
        let mut body = minimal_production_atom().to_vec();
        body[8..16].copy_from_slice(&created_at_unix_ns.to_le_bytes());
        body[32..36].copy_from_slice(&atom_type.to_le_bytes());
        body
    }

    fn production_batch_mixed_items(semantic_time_unix_ns: u64) -> Vec<BatchIngestItemV1> {
        let parent = minimal_production_atom().to_vec();
        let parent_type = AtomBodyHeader::from_bytes(&parent)
            .unwrap()
            .atom_type()
            .unwrap();
        let type2 = AtomType::from_u32(2).unwrap();
        let type3 = AtomType::from_u32(3).unwrap();
        let type4 = AtomType::from_u32(4).unwrap();
        let body2 = production_body_variant(2, semantic_time_unix_ns);
        let body3 = production_body_variant(3, semantic_time_unix_ns);
        let conflict = production_body_variant(1, semantic_time_unix_ns + 1);
        let invalid_binding = production_body_variant(5, semantic_time_unix_ns);
        let body4 = production_body_variant(4, semantic_time_unix_ns);
        let parent_id = compute_atom_id_from_payload(&parent).unwrap();
        let body2_id = compute_atom_id_from_payload(&body2).unwrap();
        let body3_id = compute_atom_id_from_payload(&body3).unwrap();
        let body4_id = compute_atom_id_from_payload(&body4).unwrap();
        let mut missing_evidence = vec![0x99; 32];
        missing_evidence.extend_from_slice(&1u32.to_le_bytes());
        missing_evidence.extend_from_slice(&0u64.to_le_bytes());
        missing_evidence.extend_from_slice(&1u64.to_le_bytes());
        missing_evidence.extend_from_slice(&5000u16.to_le_bytes());
        vec![
            BatchIngestItemV1::new(body2_id, body2.clone(), type2, [], []),
            BatchIngestItemV1::new(parent_id, parent, parent_type, [], []),
            BatchIngestItemV1::new(body3_id, body3, type3, [], []),
            BatchIngestItemV1::new(body2_id, body2, type2, [], []),
            BatchIngestItemV1::new(parent_id, conflict, parent_type, [], []),
            BatchIngestItemV1::new(body4_id, invalid_binding, type4, [], []),
            BatchIngestItemV1::new(body4_id, body4, type4, [], missing_evidence),
        ]
    }

    fn production_tree_snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
        fn visit(base: &Path, current: &Path, output: &mut BTreeMap<String, Vec<u8>>) {
            let mut entries = fs::read_dir(current)
                .unwrap()
                .map(|entry| entry.unwrap())
                .collect::<Vec<_>>();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                let relative = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if relative == ".memoryx.writer.lock" {
                    continue;
                }
                if entry.file_type().unwrap().is_dir() {
                    visit(base, &path, output);
                } else {
                    output.insert(relative, fs::read(path).unwrap());
                }
            }
        }
        let mut output = BTreeMap::new();
        visit(root, root, &mut output);
        output
    }

    fn reseal_update_component(
        root: &Path,
        generation: u64,
        order: u16,
        bytes: Vec<u8>,
        semantic_hash: [u8; 32],
    ) {
        let generation_dir = production_generation_path(root, generation);
        let commit_path = generation_dir.join(COMMIT_FILE_NAME);
        let manifest_bytes = fs::read(&commit_path).unwrap();
        let manifest =
            UpdateGenerationManifestV1::decode(&manifest_bytes, "test update manifest").unwrap();
        let mut manifest_body = manifest.body();
        let index = manifest_body
            .components
            .iter()
            .position(|descriptor| descriptor.registry_order == order)
            .unwrap();
        let prior = manifest_body.components[index].clone();
        let descriptor = production_update_descriptor(
            prior.registry_order,
            &prior.registry_key,
            &prior.mode,
            prior.target_path.clone(),
            &prior.content_codec_id,
            &bytes,
            semantic_hash,
        )
        .unwrap();
        fs::write(
            generation_dir.join(
                descriptor
                    .stage_path
                    .replace('/', std::path::MAIN_SEPARATOR_STR),
            ),
            &bytes,
        )
        .unwrap();
        manifest_body.components[index] = descriptor;
        let component_root = production_update_component_root(&manifest_body.components).unwrap();
        manifest_body.component_root_hash = hex_lower(&component_root);

        let prepare_path = generation_dir.join(PREPARE_FILE_NAME);
        let prepare =
            UpdatePrepareV1::decode(&fs::read(&prepare_path).unwrap(), "test update prepare")
                .unwrap();
        let mut prepare_body = prepare.body();
        prepare_body.components = manifest_body.components.clone();
        prepare_body.component_root_hash = hex_lower(&component_root);
        let prepare = UpdatePrepareV1::from_body(prepare_body).unwrap();
        let prepare_bytes = prepare.canonical_bytes().unwrap();
        fs::write(&prepare_path, &prepare_bytes).unwrap();

        manifest_body.prepare_hash = production_hash_hex(&prepare_bytes);
        let manifest = UpdateGenerationManifestV1::from_body(manifest_body).unwrap();
        fs::write(commit_path, manifest.canonical_bytes().unwrap()).unwrap();
    }

    fn update_generation_component(root: &Path, generation: u64, order: u16) -> Vec<u8> {
        let generation_dir = production_generation_path(root, generation);
        let manifest = UpdateGenerationManifestV1::decode(
            &fs::read(generation_dir.join(COMMIT_FILE_NAME)).unwrap(),
            "test update manifest",
        )
        .unwrap();
        let descriptor = manifest
            .components
            .iter()
            .find(|descriptor| descriptor.registry_order == order)
            .unwrap();
        fs::read(
            generation_dir.join(
                descriptor
                    .stage_path
                    .replace('/', std::path::MAIN_SEPARATOR_STR),
            ),
        )
        .unwrap()
    }

    fn production_update_source_attachment(atom_id: AtomId, projection: &str) -> Vec<u8> {
        format!(
            "{PRODUCTION_UPDATE_SOURCE_ATTACHMENT_PREFIX}{}{PRODUCTION_UPDATE_SOURCE_ATTACHMENT_PAYLOAD}{projection}",
            hex_lower(&atom_id)
        )
        .into_bytes()
    }

    fn production_update_request(
        transaction_id: &str,
        semantic_time_unix_ns: u64,
        old_atom_id: AtomId,
    ) -> UpdateAtomRequestV1 {
        let successor_body = production_body_variant(2, semantic_time_unix_ns);
        let successor_atom_id = compute_atom_id_from_payload(&successor_body).unwrap();
        let source_attachment = production_update_source_attachment(
            successor_atom_id,
            "successor-source-attachment-v1",
        );
        UpdateAtomRequestV1::from_successor_body(
            transaction_id,
            semantic_time_unix_ns,
            old_atom_id,
            successor_body,
            AtomType::from_u32(2).unwrap(),
            [],
            [],
            source_attachment,
            b"successor-provenance-v1".as_slice(),
        )
        .unwrap()
    }

    fn production_update_vectors() -> serde_json::Value {
        serde_json::from_slice(&fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join(
                "ORCHESTRATION_SYSTEM/modules/MX-80-schemas-migrations/evidence/fixtures/n5b_p1_update_atom_v1/vectors.json",
            ),
        ).unwrap()).unwrap()
    }

    #[test]
    fn production_update_atom_create_retry_reopen_lineage_and_history_once() {
        const UPDATE_TX: &str = "823e4567-e89b-42d3-a456-426614174008";
        const TIME: u64 = 1_720_000_000_000_000_800;
        let temp = tempdir().unwrap();
        let root = temp.path().join("production-update-success");
        activate_empty_production_base(&root);
        let old_body = minimal_production_atom();
        let old_type = AtomBodyHeader::from_bytes(old_body)
            .unwrap()
            .atom_type()
            .unwrap();
        let old_id = compute_atom_id_from_payload(old_body).unwrap();
        let mut store = ProductionMemoryX::open(&root).unwrap();
        store
            .direct_ingest(TX1, TIME - 1, old_body, old_type, &[], &[])
            .unwrap();
        let old_cas = fs::read(root.join("cas/seg_00000.dat")).unwrap();
        let request = production_update_request(UPDATE_TX, TIME, old_id);
        let receipt = store.update_atom(&request).unwrap();
        assert_eq!(receipt.parent_generation, 1);
        assert_eq!(receipt.committed_generation, 2);
        assert_eq!(receipt.old_atom_id, old_id);
        assert_eq!(receipt.successor_atom_id, request.successor_atom_id);
        assert_ne!(
            receipt.old_provenance_hash,
            receipt.successor_provenance_hash
        );
        assert_eq!(store.successor_for(old_id), Some(request.successor_atom_id));
        assert_eq!(store.current_atom_ids(), vec![request.successor_atom_id]);
        assert_eq!(store.committed_atom_ids().len(), 2);
        assert_eq!(fs::read(root.join("cas/seg_00000.dat")).unwrap(), old_cas);
        let successor_cas = fs::read(root.join("cas/segments/seg_00000002.skf1")).unwrap();
        assert_eq!(
            &successor_cas[RecordHeader::SIZE..RecordHeader::SIZE + request.successor_body.len()],
            request.successor_body
        );
        let history = fs::read(root.join("meta/history.log")).unwrap();
        assert_eq!(history.iter().filter(|byte| **byte == b'\n').count(), 2);
        let provenance = fs::read(root.join("meta/atom_sources.jsonl")).unwrap();
        assert_eq!(provenance.len(), 102);
        assert_eq!(&provenance[..4], b"SPV1");
        assert_eq!(&provenance[6..38], &request.successor_atom_id);
        assert!(
            !provenance
                .windows(old_id.len())
                .any(|window| window == old_id)
        );
        let delta = fs::read(root.join("index/graph/deltas/d_00000001.edges")).unwrap();
        let edge = EdgeListEntry::from_bytes(&delta[DeltaHeader::SIZE..]).unwrap();
        assert_eq!(edge.src_node, receipt.successor_node);
        assert_eq!(edge.dst_node, 0);
        assert_eq!(edge.edge_type, EdgeType::SUPERSEDES.to_u32());
        assert_eq!(edge.confidence_q, 5000);
        let tree = production_tree_snapshot(&root);
        let retry = store.update_atom(&request).unwrap();
        assert_eq!(retry.canonical_bytes(), receipt.canonical_bytes());
        assert_eq!(production_tree_snapshot(&root), tree);

        let mut divergent = request.clone();
        divergent.successor_provenance = b"divergent-successor-provenance".to_vec();
        assert_eq!(
            store.update_atom(&divergent).unwrap_err().code,
            UpdateAtomFailureCodeV1::ConflictingTransactionReuse
        );
        assert_eq!(production_tree_snapshot(&root), tree);
        assert_eq!(
            store
                .direct_ingest(
                    UPDATE_TX,
                    TIME,
                    &request.successor_body,
                    request.successor_atom_type,
                    &[],
                    &[],
                )
                .unwrap_err()
                .code,
            DirectIngestFailureCodeV1::ConflictingTransactionReuse
        );
        drop(store);

        let mut first = ProductionMemoryX::open(&root).unwrap();
        assert_eq!(first.current_atom_ids(), vec![request.successor_atom_id]);
        assert_eq!(
            first.update_atom(&request).unwrap().canonical_bytes(),
            receipt.canonical_bytes()
        );
        drop(first);
        let second = ProductionMemoryX::open(&root).unwrap();
        assert_eq!(second.committed_generation(), 2);
        assert_eq!(second.committed_logical_digest(), receipt.logical_digest);
        assert_eq!(
            second.successor_for(old_id),
            Some(request.successor_atom_id)
        );
        assert_eq!(production_tree_snapshot(&root), tree);
    }

    #[test]
    fn production_update_atom_precommit_s0_and_postcommit_s1_recovery() {
        const TIME: u64 = 1_720_000_000_000_000_900;
        for (name, transaction, stop, committed) in [
            (
                "precommit",
                "923e4567-e89b-42d3-a456-426614174009",
                "after_prepare_before_commit",
                false,
            ),
            (
                "postcommit",
                "a23e4567-e89b-42d3-a456-42661417400a",
                "after_commit_before_install",
                true,
            ),
        ] {
            let temp = tempdir().unwrap();
            let root = temp.path().join(format!("production-update-{name}"));
            activate_empty_production_base(&root);
            let old_body = minimal_production_atom();
            let old_type = AtomBodyHeader::from_bytes(old_body)
                .unwrap()
                .atom_type()
                .unwrap();
            let old_id = compute_atom_id_from_payload(old_body).unwrap();
            let mut store = ProductionMemoryX::open(&root).unwrap();
            store
                .direct_ingest(TX1, TIME - 1, old_body, old_type, &[], &[])
                .unwrap();
            let request = production_update_request(transaction, TIME, old_id);
            PRODUCTION_UPDATE_TEST_STOP.with(|selected| selected.set(Some(stop)));
            let failure = store.update_atom(&request).unwrap_err();
            assert_eq!(failure.code, UpdateAtomFailureCodeV1::RecoveryRequired);
            assert_eq!(
                failure.commit_disposition,
                if committed {
                    DirectIngestCommitDispositionV1::CommittedInstallPending
                } else {
                    DirectIngestCommitDispositionV1::NotCommitted
                }
            );
            drop(store);
            let mut first = ProductionMemoryX::open(&root).unwrap();
            if committed {
                assert_eq!(first.committed_generation(), 2);
                assert_eq!(first.current_atom_ids(), vec![request.successor_atom_id]);
            } else {
                assert_eq!(first.committed_generation(), 1);
                assert_eq!(first.current_atom_ids(), vec![old_id]);
                assert!(!root.join("cas/segments/seg_00000002.skf1").exists());
            }
            let receipt = first.update_atom(&request).unwrap();
            assert_eq!(receipt.committed_generation, 2);
            drop(first);
            let second = ProductionMemoryX::open(&root).unwrap();
            assert_eq!(second.current_atom_ids(), vec![request.successor_atom_id]);
            assert_eq!(
                second.successor_for(old_id),
                Some(request.successor_atom_id)
            );
            assert_eq!(
                fs::read(root.join("meta/history.log"))
                    .unwrap()
                    .iter()
                    .filter(|byte| **byte == b'\n')
                    .count(),
                2
            );
        }
    }

    #[test]
    fn production_update_atom_preflight_is_mutation_free_and_journal_authoritative() {
        const TIME: u64 = 1_720_000_000_000_001_000;
        let temp = tempdir().unwrap();
        let root = temp.path().join("production-update-preflight");
        activate_empty_production_base(&root);
        let old_body = minimal_production_atom();
        let old_type = AtomBodyHeader::from_bytes(old_body)
            .unwrap()
            .atom_type()
            .unwrap();
        let old_id = compute_atom_id_from_payload(old_body).unwrap();
        let mut store = ProductionMemoryX::open(&root).unwrap();
        store
            .direct_ingest(TX1, TIME - 1, old_body, old_type, &[], &[])
            .unwrap();
        let parent = production_tree_snapshot(&root);

        let mut missing =
            production_update_request("b23e4567-e89b-42d3-a456-42661417400b", TIME, [0x55; 32]);
        assert_eq!(
            store.update_atom(&missing).unwrap_err().code,
            UpdateAtomFailureCodeV1::OldAtomMissing
        );
        assert_eq!(production_tree_snapshot(&root), parent);
        missing.old_atom_id = missing.successor_atom_id;
        assert_eq!(
            store.update_atom(&missing).unwrap_err().code,
            UpdateAtomFailureCodeV1::SameAtomIdUpdate
        );
        assert_eq!(production_tree_snapshot(&root), parent);

        let vectors = production_update_vectors();
        let current = &vectors["relation_journals"]["current"][0];
        let exact_relation = base64::engine::general_purpose::STANDARD
            .decode(current["record_base64"].as_str().unwrap())
            .unwrap();
        let exact = UpdateRelationJournalV1::decode(&exact_relation, "exact relation").unwrap();
        assert_eq!(exact.canonical_bytes().unwrap(), exact_relation);
        let mut relation = UpdateRelationJournalV1::from_body(UpdateRelationJournalBodyV1 {
            schema: exact.schema.clone(),
            version: exact.version,
            journal_kind: exact.journal_kind.clone(),
            ordinal: exact.ordinal,
            relation_atom_id: exact.relation_atom_id.clone(),
            subject_atom_id: hex_lower(&old_id),
            predicate_id: exact.predicate_id,
            object_atom_id: exact.object_atom_id.clone(),
            current: exact.current,
            historical: exact.historical,
        })
        .unwrap()
        .canonical_bytes()
        .unwrap();
        relation.push(b'\n');
        fs::write(root.join("meta/relations.jsonl"), relation).unwrap();
        let with_journal = production_tree_snapshot(&root);
        let request =
            production_update_request("c23e4567-e89b-42d3-a456-42661417400c", TIME + 1, old_id);
        assert_eq!(
            store.update_atom(&request).unwrap_err().code,
            UpdateAtomFailureCodeV1::RelationBackedAtomRequiresCompositeOperation
        );
        assert_eq!(production_tree_snapshot(&root), with_journal);
        fs::remove_file(root.join("meta/relations.jsonl")).unwrap();

        let future = UpdateRelationJournalV1::from_body(UpdateRelationJournalBodyV1 {
            version: 2,
            ..exact.body()
        })
        .unwrap()
        .canonical_bytes()
        .unwrap();
        fs::write(root.join("meta/relations.jsonl"), future).unwrap();
        let future_tree = production_tree_snapshot(&root);
        let future_request =
            production_update_request("e23e4567-e89b-42d3-a456-42661417400e", TIME + 2, old_id);
        assert_eq!(
            store.update_atom(&future_request).unwrap_err().code,
            UpdateAtomFailureCodeV1::UnsupportedOrCorrupt
        );
        assert_eq!(production_tree_snapshot(&root), future_tree);
        fs::remove_file(root.join("meta/relations.jsonl")).unwrap();

        let mut bounded =
            production_update_request("d23e4567-e89b-42d3-a456-42661417400d", TIME + 3, old_id);
        bounded
            .successor_body
            .resize(PRODUCTION_MAX_BODY_BYTES as usize + 1, 0);
        assert_eq!(
            store.update_atom(&bounded).unwrap_err().code,
            UpdateAtomFailureCodeV1::ResourceLimitExceeded
        );
        assert_eq!(production_tree_snapshot(&root), parent);
    }

    #[test]
    fn production_update_atom_source_attachment_is_successor_bound_before_prepare() {
        const TIME: u64 = 1_720_000_000_000_001_100;
        let temp = tempdir().unwrap();
        let root = temp.path().join("production-update-source-binding");
        activate_empty_production_base(&root);
        let old_body = minimal_production_atom();
        let old_type = AtomBodyHeader::from_bytes(old_body)
            .unwrap()
            .atom_type()
            .unwrap();
        let old_id = compute_atom_id_from_payload(old_body).unwrap();
        let mut store = ProductionMemoryX::open(&root).unwrap();
        store
            .direct_ingest(TX1, TIME - 1, old_body, old_type, &[], &[])
            .unwrap();
        let positive =
            production_update_request("f23e4567-e89b-42d3-a456-42661417400f", TIME, old_id);
        ProductionUpdateIntentV1::create(&positive).unwrap();
        let before = production_tree_snapshot(&root);
        let cases = [
            production_update_source_attachment([0; 32], "zero"),
            production_update_source_attachment(old_id, "old"),
            production_update_source_attachment([0x33; 32], "foreign"),
            b"memoryx.atom-source-attachment-projection.v1|atom_id=bad|projection=x".to_vec(),
            format!(
                "{PRODUCTION_UPDATE_SOURCE_ATTACHMENT_PREFIX}{}{PRODUCTION_UPDATE_SOURCE_ATTACHMENT_PAYLOAD}non canonical",
                hex_lower(&positive.successor_atom_id)
            )
            .into_bytes(),
        ];
        for (index, source_attachment) in cases.into_iter().enumerate() {
            let mut request = positive.clone();
            request.transaction_id = format!("{index:08x}-e89b-42d3-a456-426614174100");
            request.successor_source_attachment_projection = source_attachment;
            let failure = store.update_atom(&request).unwrap_err();
            assert_eq!(
                failure.code,
                UpdateAtomFailureCodeV1::ProvenanceProjectionConflict
            );
            assert_eq!(production_tree_snapshot(&root), before);
            assert!(!production_generation_path(&root, 2).exists());
        }
    }

    #[test]
    fn production_update_atom_control_record_bound_is_exact_and_mutation_free() {
        const TIME: u64 = 1_720_000_000_000_001_200;
        let temp = tempdir().unwrap();
        let root = temp.path().join("production-update-control-bound");
        activate_empty_production_base(&root);
        let old_body = minimal_production_atom();
        let old_type = AtomBodyHeader::from_bytes(old_body)
            .unwrap()
            .atom_type()
            .unwrap();
        let old_id = compute_atom_id_from_payload(old_body).unwrap();
        let mut store = ProductionMemoryX::open(&root).unwrap();
        store
            .direct_ingest(TX1, TIME - 1, old_body, old_type, &[], &[])
            .unwrap();
        let request =
            production_update_request("123e4567-e89b-42d3-a456-426614174120", TIME, old_id);
        let intent = ProductionUpdateIntentV1::create(&request).unwrap();
        let envelope = ProductionUpdateEnvelopeV1::create(
            &request.transaction_id,
            TIME,
            store.state.base_binding.hash(),
            intent.hash,
        )
        .unwrap();
        let plan =
            production_plan_update(&root, &store.state, &request, &intent, &envelope).unwrap();
        let manifest_bytes = plan.manifest.canonical_bytes().unwrap();
        let before = production_tree_snapshot(&root);
        assert_eq!(
            PRODUCTION_UPDATE_MAX_CONTROL_BYTES,
            MAX_CONTROL_RECORD_BYTES
        );
        assert!(
            production_update_resource_preflight(
                &request,
                &plan.staged,
                &vec![0; PRODUCTION_UPDATE_MAX_CONTROL_BYTES as usize],
                &manifest_bytes,
            )
            .is_ok()
        );
        assert!(
            production_update_resource_preflight(
                &request,
                &plan.staged,
                &vec![0; PRODUCTION_UPDATE_MAX_CONTROL_BYTES as usize + 1],
                &manifest_bytes,
            )
            .is_err()
        );
        assert_eq!(production_tree_snapshot(&root), before);
        assert!(!production_generation_path(&root, 2).exists());
    }

    #[test]
    fn production_update_atom_reopen_recomputes_every_component_semantic_class() {
        const TIME: u64 = 1_720_000_000_000_001_300;
        for order in [20u16, 30, 40, 50, 60, 70, 80, 90] {
            let temp = tempdir().unwrap();
            let root = temp
                .path()
                .join(format!("production-update-semantic-{order}"));
            activate_empty_production_base(&root);
            let old_body = minimal_production_atom();
            let old_type = AtomBodyHeader::from_bytes(old_body)
                .unwrap()
                .atom_type()
                .unwrap();
            let old_id = compute_atom_id_from_payload(old_body).unwrap();
            let mut store = ProductionMemoryX::open(&root).unwrap();
            store
                .direct_ingest(TX1, TIME - 1, old_body, old_type, &[], &[])
                .unwrap();
            let request = production_update_request(
                &format!("{order:08x}-e89b-42d3-a456-426614174130"),
                TIME,
                old_id,
            );
            store.update_atom(&request).unwrap();
            drop(store);
            let bytes = update_generation_component(&root, 2, order);
            reseal_update_component(&root, 2, order, bytes, [order as u8; 32]);
            let sealed = production_tree_snapshot(&root);
            assert!(ProductionMemoryX::open(&root).is_err(), "order {order}");
            assert_eq!(production_tree_snapshot(&root), sealed, "order {order}");
            assert!(ProductionMemoryX::open(&root).is_err(), "order {order}");
            assert_eq!(production_tree_snapshot(&root), sealed, "order {order}");
        }
    }

    #[test]
    fn production_update_atom_reopen_rejects_noncanonical_spv1_and_history_components() {
        const TIME: u64 = 1_720_000_000_000_001_400;
        for order in [50u16, 60] {
            let temp = tempdir().unwrap();
            let root = temp.path().join(format!("production-update-exact-{order}"));
            activate_empty_production_base(&root);
            let old_body = minimal_production_atom();
            let old_type = AtomBodyHeader::from_bytes(old_body)
                .unwrap()
                .atom_type()
                .unwrap();
            let old_id = compute_atom_id_from_payload(old_body).unwrap();
            let mut store = ProductionMemoryX::open(&root).unwrap();
            store
                .direct_ingest(TX1, TIME - 1, old_body, old_type, &[], &[])
                .unwrap();
            let request = production_update_request(
                &format!("{order:08x}-e89b-42d3-a456-426614174140"),
                TIME,
                old_id,
            );
            let receipt = store.update_atom(&request).unwrap();
            drop(store);
            let exact = update_generation_component(&root, 2, order);
            let (mutated, semantic_hash) = if order == 50 {
                let mut mutated = vec![b'X'];
                mutated.extend_from_slice(&exact);
                (
                    mutated.clone(),
                    production_update_semantic(PRODUCTION_UPDATE_SPV1_SEMANTIC_ID, &mutated),
                )
            } else {
                let mut mutated = exact.clone();
                mutated.push(b'\n');
                mutated.extend_from_slice(&exact);
                (mutated, receipt.history_semantic_hash)
            };
            reseal_update_component(&root, 2, order, mutated, semantic_hash);
            let sealed = production_tree_snapshot(&root);
            assert!(ProductionMemoryX::open(&root).is_err());
            assert_eq!(production_tree_snapshot(&root), sealed);
            assert!(ProductionMemoryX::open(&root).is_err());
            assert_eq!(production_tree_snapshot(&root), sealed);
        }
    }

    #[test]
    fn production_update_atom_reopen_recomputes_history_event_and_semantic_identity() {
        const TIME: u64 = 1_720_000_000_000_001_450;
        let temp = tempdir().unwrap();
        let root = temp.path().join("production-update-history-semantic");
        activate_empty_production_base(&root);
        let old_body = minimal_production_atom();
        let old_type = AtomBodyHeader::from_bytes(old_body)
            .unwrap()
            .atom_type()
            .unwrap();
        let old_id = compute_atom_id_from_payload(old_body).unwrap();
        let mut store = ProductionMemoryX::open(&root).unwrap();
        store
            .direct_ingest(TX1, TIME - 1, old_body, old_type, &[], &[])
            .unwrap();
        let request =
            production_update_request("723e4567-e89b-42d3-a456-426614174145", TIME, old_id);
        let receipt = store.update_atom(&request).unwrap();
        drop(store);

        let history_bytes = update_generation_component(&root, 2, 60);
        let history = UpdateHistoryEventV1::decode(&history_bytes, "test update history").unwrap();
        let mut history_body = history.body();
        history_body.transaction_id = "f23e4567-e89b-42d3-a456-426614174160".to_owned();
        let substituted = UpdateHistoryEventV1::from_body(history_body)
            .unwrap()
            .canonical_bytes()
            .unwrap();
        reseal_update_component(&root, 2, 60, substituted, receipt.history_semantic_hash);

        let sealed = production_tree_snapshot(&root);
        assert!(ProductionMemoryX::open(&root).is_err());
        assert_eq!(production_tree_snapshot(&root), sealed);
        assert!(ProductionMemoryX::open(&root).is_err());
        assert_eq!(production_tree_snapshot(&root), sealed);
    }

    #[test]
    fn production_update_atom_reopen_joins_delt_and_grm1_to_exact_lineage() {
        const TIME: u64 = 1_720_000_000_000_001_500;
        for graph_case in ["reverse-delt", "mismatched-grm1"] {
            let temp = tempdir().unwrap();
            let root = temp.path().join(format!("production-update-{graph_case}"));
            activate_empty_production_base(&root);
            let old_body = minimal_production_atom();
            let old_type = AtomBodyHeader::from_bytes(old_body)
                .unwrap()
                .atom_type()
                .unwrap();
            let old_id = compute_atom_id_from_payload(old_body).unwrap();
            let mut store = ProductionMemoryX::open(&root).unwrap();
            store
                .direct_ingest(TX1, TIME - 1, old_body, old_type, &[], &[])
                .unwrap();
            let request = production_update_request(
                if graph_case == "reverse-delt" {
                    "523e4567-e89b-42d3-a456-426614174150"
                } else {
                    "623e4567-e89b-42d3-a456-426614174151"
                },
                TIME,
                old_id,
            );
            let receipt = store.update_atom(&request).unwrap();
            let old_node = store
                .state
                .atoms
                .iter()
                .find(|atom| atom.atom_id == old_id)
                .unwrap()
                .node_num;
            drop(store);
            if graph_case == "reverse-delt" {
                let (bytes, semantic_hash) =
                    production_update_delta_bytes(1, 0, old_node, receipt.successor_node).unwrap();
                reseal_update_component(&root, 2, 80, bytes, semantic_hash);
                fs::remove_file(root.join("index/graph/deltas/d_00000001.edges")).unwrap();
            } else {
                let graph_leaf = production_update_graph_leaf(receipt.successor_node, old_node);
                let (bytes, semantic_hash) = production_update_graph_manifest(
                    1,
                    0,
                    receipt.successor_node + 2,
                    &[graph_leaf],
                )
                .unwrap();
                reseal_update_component(&root, 2, 90, bytes, semantic_hash);
            }
            let sealed = production_tree_snapshot(&root);
            assert!(ProductionMemoryX::open(&root).is_err());
            assert_eq!(production_tree_snapshot(&root), sealed);
            assert!(ProductionMemoryX::open(&root).is_err());
            assert_eq!(production_tree_snapshot(&root), sealed);
        }
    }

    #[test]
    fn production_update_atom_mx80_exact_goldens_and_strict_records() {
        let vectors = production_update_vectors();
        let positive = &vectors["positive_vectors"][0];
        for (name, expected) in [
            ("successor_body", "successor_body_hash"),
            ("claim_projection", "claim_projection_hash"),
            ("api_evidence_projection", "api_evidence_projection_hash"),
            (
                "successor_source_attachment_projection",
                "successor_source_attachment_hash",
            ),
        ] {
            assert_eq!(
                hex_lower(&production_sha256(
                    vectors["hash_preimages"][name]["bytes_utf8"]
                        .as_str()
                        .unwrap()
                        .as_bytes(),
                )),
                positive[expected].as_str().unwrap()
            );
        }
        let old = parse_hash_hex(positive["old_atom_id"].as_str().unwrap(), "old").unwrap();
        let successor =
            parse_hash_hex(positive["successor_atom_id"].as_str().unwrap(), "successor").unwrap();
        assert_eq!(
            hex_lower(&production_update_relation_id(successor, old)),
            positive["supersedes_relation_id"].as_str().unwrap()
        );
        let graph_leaf = production_update_graph_leaf(43, 42);
        let (delta, delta_semantic) = production_update_delta_bytes(1, 7, 43, 42).unwrap();
        assert_eq!(
            hex_lower(&delta),
            positive["graph_delta_hex"].as_str().unwrap()
        );
        assert_eq!(
            hex_lower(&delta_semantic),
            positive["delt_component_semantic_hash"].as_str().unwrap()
        );
        let (grm1, grm1_semantic) =
            production_update_graph_manifest(1, 7, 44, &[graph_leaf]).unwrap();
        assert_eq!(hex_lower(&grm1), positive["grm1_hex"].as_str().unwrap());
        assert_eq!(
            hex_lower(&grm1_semantic),
            positive["grm1_component_semantic_hash"].as_str().unwrap()
        );

        let intent_hash =
            parse_hash_hex(positive["intent_hash"].as_str().unwrap(), "intent").unwrap();
        let envelope = ProductionUpdateEnvelopeV1::create(
            positive["transaction_id"].as_str().unwrap(),
            positive["semantic_time_unix_ns"].as_u64().unwrap(),
            [1; 32],
            intent_hash,
        )
        .unwrap();
        let history = production_update_history(
            &envelope,
            1,
            successor,
            old,
            production_update_relation_id(successor, old),
            parse_hash_hex(
                positive["successor_provenance_hash"].as_str().unwrap(),
                "successor provenance",
            )
            .unwrap(),
            parse_hash_hex(
                positive["old_provenance_hash"].as_str().unwrap(),
                "old provenance",
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            history.record_bytes,
            base64::engine::general_purpose::STANDARD
                .decode(positive["history_event_base64"].as_str().unwrap())
                .unwrap()
        );
        assert_eq!(
            hex_lower(&history.event_id),
            positive["history_event_id"].as_str().unwrap()
        );
        assert_eq!(
            hex_lower(&history.semantic_hash),
            positive["history_semantic_hash"].as_str().unwrap()
        );

        let mut descriptors = Vec::new();
        for vector in vectors["component_descriptor_corpus"].as_array().unwrap() {
            let content = base64::engine::general_purpose::STANDARD
                .decode(vector["content_base64"].as_str().unwrap())
                .unwrap();
            let descriptor = production_update_descriptor(
                vector["registry_order"].as_u64().unwrap() as u16,
                vector["registry_key"].as_str().unwrap(),
                vector["mode"].as_str().unwrap(),
                vector["target_path"].as_str().unwrap().to_owned(),
                vector["content_codec_id"].as_str().unwrap(),
                &content,
                parse_hash_hex(vector["semantic_hash"].as_str().unwrap(), "semantic").unwrap(),
            )
            .unwrap();
            let expected = base64::engine::general_purpose::STANDARD
                .decode(vector["descriptor_json_base64"].as_str().unwrap())
                .unwrap();
            assert_eq!(descriptor.canonical_bytes().unwrap(), expected);
            let mut appended = expected.clone();
            appended.push(b'\n');
            assert!(UpdateComponentDescriptorV1::decode(&appended, "update descriptor").is_err());
            descriptors.push(descriptor);
        }
        assert_eq!(
            hex_lower(&production_update_component_root(&descriptors).unwrap()),
            vectors["component_root"]["blake3"].as_str().unwrap()
        );
        assert_eq!(
            hex_lower(&production_sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn production_batch_edgefree_mixed_commit_retry_reopen_and_history_once() {
        const BATCH_TX: &str = "423e4567-e89b-42d3-a456-426614174003";
        const BATCH_TIME: u64 = 1_720_000_000_000_000_100;
        let temp = tempdir().unwrap();
        let root = temp.path().join("production-batch-mixed");
        activate_empty_production_base(&root);
        let parent_body = minimal_production_atom();
        let parent_type = AtomBodyHeader::from_bytes(parent_body)
            .unwrap()
            .atom_type()
            .unwrap();
        let mut store = ProductionMemoryX::open(&root).unwrap();
        store
            .direct_ingest(TX1, BATCH_TIME - 100, parent_body, parent_type, &[], &[])
            .unwrap();
        let items = production_batch_mixed_items(BATCH_TIME);

        let receipt = store.batch_ingest(BATCH_TX, BATCH_TIME, &items).unwrap();
        assert_eq!(receipt.result, BatchIngestResultKindV1::Committed);
        assert_eq!(receipt.committed_generation, 2);
        assert_eq!(
            receipt
                .outcomes
                .iter()
                .map(|outcome| (outcome.result, outcome.reason))
                .collect::<Vec<_>>(),
            vec![
                (
                    BatchIngestItemResultV1::Created,
                    BatchIngestItemReasonV1::Created
                ),
                (
                    BatchIngestItemResultV1::Reused,
                    BatchIngestItemReasonV1::AlreadyCommitted,
                ),
                (
                    BatchIngestItemResultV1::Created,
                    BatchIngestItemReasonV1::Created
                ),
                (
                    BatchIngestItemResultV1::Refused,
                    BatchIngestItemReasonV1::DuplicateInput,
                ),
                (
                    BatchIngestItemResultV1::Refused,
                    BatchIngestItemReasonV1::CanonicalConflict,
                ),
                (
                    BatchIngestItemResultV1::Refused,
                    BatchIngestItemReasonV1::InvalidItem,
                ),
                (
                    BatchIngestItemResultV1::Refused,
                    BatchIngestItemReasonV1::EvidenceSourceNotLive,
                ),
            ]
        );
        assert_eq!(store.committed_atom_ids().len(), 3);
        let history = fs::read(root.join("meta/history.log")).unwrap();
        assert_eq!(history.iter().filter(|byte| **byte == b'\n').count(), 2);
        let tree_after_commit = production_tree_snapshot(&root);
        let retry = store.batch_ingest(BATCH_TX, BATCH_TIME, &items).unwrap();
        assert_eq!(retry.canonical_bytes(), receipt.canonical_bytes());
        assert_eq!(production_tree_snapshot(&root), tree_after_commit);

        let mut divergent = items.clone();
        divergent.swap(0, 1);
        let failure = store
            .batch_ingest(BATCH_TX, BATCH_TIME, &divergent)
            .unwrap_err();
        assert_eq!(
            failure.code,
            DirectIngestFailureCodeV1::ConflictingTransactionReuse
        );
        assert_eq!(production_tree_snapshot(&root), tree_after_commit);
        drop(store);

        let mut first = ProductionMemoryX::open(&root).unwrap();
        assert_eq!(first.committed_generation(), 2);
        assert_eq!(first.committed_atom_ids().len(), 3);
        let retry_after_reopen = first.batch_ingest(BATCH_TX, BATCH_TIME, &items).unwrap();
        assert_eq!(
            retry_after_reopen.canonical_bytes(),
            receipt.canonical_bytes()
        );
        drop(first);
        let second = ProductionMemoryX::open(&root).unwrap();
        assert_eq!(second.committed_logical_digest(), receipt.logical_digest);
        assert_eq!(second.committed_atom_ids().len(), 3);
        assert_eq!(production_tree_snapshot(&root), tree_after_commit);
    }

    #[test]
    fn production_batch_edgefree_codecs_match_exact_mx80_bytes_and_reject_adversaries() {
        const BATCH_TX: &str = "423e4567-e89b-42d3-a456-426614174003";
        const BATCH_TIME: u64 = 1_720_000_000_000_000_100;
        let binding_bytes =
            production_codec_fixture("positive/base_binding_reuse_parent_windows_v1.bin");
        let binding = ProductionBaseBindingV1::decode(&binding_bytes).unwrap();
        let items = production_batch_mixed_items(BATCH_TIME);
        assert_eq!(
            production_body_variant(2, BATCH_TIME),
            production_batch_fixture("positive/body_created_type2.bin")
        );
        assert_eq!(
            production_body_variant(3, BATCH_TIME),
            production_batch_fixture("positive/body_created_type3.bin")
        );
        assert_eq!(
            production_body_variant(1, BATCH_TIME + 1),
            production_batch_fixture("positive/body_conflict_same_atom_id.bin")
        );
        assert_eq!(
            production_body_variant(5, BATCH_TIME),
            production_batch_fixture("positive/body_invalid_type_binding.bin")
        );
        for (ordinal, item) in items.iter().enumerate() {
            let encoded = ProductionBatchItemCodecV1::create(ordinal as u32, item).unwrap();
            assert_eq!(
                encoded.bytes,
                production_batch_fixture(&format!("positive/item_{ordinal:02}.bin"))
            );
        }
        let intent = ProductionBatchIntentV1::create(binding.hash(), &items).unwrap();
        let intent_bytes = production_batch_fixture("positive/batch_intent_committed.bin");
        assert_eq!(intent.bytes, intent_bytes);
        assert_eq!(
            ProductionBatchIntentV1::decode(&intent_bytes, &items)
                .unwrap()
                .hash,
            intent.hash
        );
        let envelope =
            ProductionBatchEnvelopeV1::create(BATCH_TX, BATCH_TIME, binding.hash(), intent.hash)
                .unwrap();
        let envelope_bytes = production_batch_fixture("positive/batch_envelope_committed.bin");
        assert_eq!(envelope.bytes, envelope_bytes);
        assert_eq!(
            ProductionBatchEnvelopeV1::decode(&envelope_bytes)
                .unwrap()
                .hash,
            envelope.hash
        );

        let parent_receipt = DirectIngestReceiptV1::decode(&production_codec_fixture(
            "positive/direct_ingest_receipt_created_v1.json",
        ))
        .unwrap();
        let parent_segment = production_batch_fixture("positive/parent_seg_00000.dat");
        let parent_body = minimal_production_atom();
        let parent_header = AtomBodyHeader::from_bytes(parent_body).unwrap();
        let parent_history = production_logical_fixture("positive/history_digest_leaf_v1.bin");
        let parent_atom = ProductionAtomStateV1 {
            atom_id: parent_receipt.atom_id,
            atom_type: parent_header.atom_type().unwrap(),
            node_num: parent_receipt.node_num,
            committed_generation: parent_receipt.committed_generation,
            body_len: parent_body.len() as u64,
            body_crc32: crc32(parent_body),
            body_hash: production_hash_bytes(parent_body),
            segment_id: 0,
            record_offset: 0,
            record_extent_len: parent_segment.len() as u64,
            domain_mask: 0xffff,
            created_at_ns: parent_header.created_at_unix_ns,
            trust_level: 5000,
            source_id: 0,
            provenance_hash: production_hash_bytes(&production_zero_provenance_leaf(
                &parent_receipt.atom_id,
            )),
            history_event_id: parent_receipt.history_event_id.unwrap(),
            history_leaf: parent_history.clone(),
        };
        let temp = tempdir().unwrap();
        fs::create_dir_all(temp.path().join("cas")).unwrap();
        fs::write(temp.path().join("cas/seg_00000.dat"), &parent_segment).unwrap();
        let parent_state = ProductionRuntimeStateV1 {
            head: ProductionCommittedHead {
                generation: parent_receipt.committed_generation,
                commit_hash: parent_receipt.commit_hash,
                logical_digest: parent_receipt.logical_digest,
            },
            base_binding: binding.clone(),
            admission_bytes: Vec::new(),
            atom: Some(parent_atom.clone()),
            atoms: vec![parent_atom],
            history_leaves: vec![parent_history],
            graph_leaves: Vec::new(),
            superseded_by: BTreeMap::new(),
            committed_receipts: BTreeMap::new(),
            committed_transactions: BTreeMap::new(),
            batch_transactions: BTreeMap::new(),
            update_transactions: BTreeMap::new(),
            owner_lifetime_transactions: BTreeMap::new(),
        };
        let plan = production_plan_batch(temp.path(), &parent_state, &intent, &envelope).unwrap();
        for (ordinal, decision) in plan.decision_bytes.iter().enumerate() {
            assert_eq!(
                decision,
                &production_batch_fixture(&format!("positive/decision_{ordinal:02}.bin"))
            );
        }
        assert_eq!(
            plan.preflight_bytes,
            production_batch_fixture("positive/preflight_committed.bin")
        );

        let component = production_batch_fixture("positive/component_descriptor_00.json");
        assert_eq!(
            ProductionComponentDescriptorV1::decode(&component, "batch component")
                .unwrap()
                .canonical_bytes()
                .unwrap(),
            component
        );
        let pair = production_batch_fixture("positive/pair_descriptor_00.json");
        assert_eq!(
            ProductionPairDescriptorV1::decode(&pair, "batch pair")
                .unwrap()
                .canonical_bytes()
                .unwrap(),
            pair
        );
        let append = production_batch_fixture("positive/append_descriptor_00.json");
        assert_eq!(
            BatchCasAppendDescriptorV1::decode(&append, "batch append")
                .unwrap()
                .canonical_bytes()
                .unwrap(),
            append
        );
        let prepare = production_batch_fixture("positive/batch_prepare.json");
        assert_eq!(
            BatchPrepareV1::decode(&prepare, "batch prepare")
                .unwrap()
                .canonical_bytes()
                .unwrap(),
            prepare
        );
        let commit = production_batch_fixture("positive/batch_commit.json");
        assert_eq!(
            BatchGenerationManifestV1::decode(&commit, "batch commit")
                .unwrap()
                .canonical_bytes()
                .unwrap(),
            commit
        );
        let receipt = production_batch_fixture("positive/batch_receipt_committed.json");
        let commit_hash = production_hash_bytes(&commit);
        let manifest = BatchGenerationManifestV1::decode(&commit, "batch commit").unwrap();
        let history: ProductionBatchHistoryWire = decode_production_json(
            &production_batch_fixture("positive/batch_history_event.json"),
            "batch history event",
        )
        .unwrap();
        let rebuilt_receipt = BatchIngestReceiptV1::create(
            BatchIngestResultKindV1::Committed,
            BATCH_TX.to_owned(),
            BATCH_TIME,
            intent.hash,
            binding.hash(),
            manifest.generation,
            commit_hash,
            parse_hash_hex(&manifest.logical_state_digest, "fixture logical digest").unwrap(),
            plan.outcomes.clone(),
            Some(parse_hash_hex(&history.event_id, "fixture history event").unwrap()),
        )
        .unwrap();
        assert_eq!(rebuilt_receipt.canonical_bytes(), receipt);
        assert_eq!(
            validate_batch_receipt_wire(&receipt)
                .unwrap()
                .canonical_bytes()
                .unwrap(),
            receipt
        );
        let unchanged = production_batch_fixture("positive/all_refused_receipt.json");
        let all_items = items[4..].to_vec();
        for (ordinal, item) in all_items.iter().enumerate() {
            assert_eq!(
                ProductionBatchItemCodecV1::create(ordinal as u32, item)
                    .unwrap()
                    .bytes,
                production_batch_fixture(&format!("positive/all_refused_item_{ordinal:02}.bin"))
            );
        }
        let all_intent = ProductionBatchIntentV1::create(binding.hash(), &all_items).unwrap();
        assert_eq!(
            all_intent.bytes,
            production_batch_fixture("positive/all_refused_intent.bin")
        );
        let all_envelope = ProductionBatchEnvelopeV1::create(
            "523e4567-e89b-42d3-a456-426614174004",
            BATCH_TIME + 1,
            binding.hash(),
            all_intent.hash,
        )
        .unwrap();
        assert_eq!(
            all_envelope.bytes,
            production_batch_fixture("positive/all_refused_envelope.bin")
        );
        let all_plan =
            production_plan_batch(temp.path(), &parent_state, &all_intent, &all_envelope).unwrap();
        assert!(all_plan.created_ordinals.is_empty());
        for (ordinal, decision) in all_plan.decision_bytes.iter().enumerate() {
            assert_eq!(
                decision,
                &production_batch_fixture(&format!(
                    "positive/all_refused_decision_{ordinal:02}.bin"
                ))
            );
        }
        assert_eq!(
            all_plan.preflight_bytes,
            production_batch_fixture("positive/all_refused_preflight.bin")
        );
        assert_eq!(
            BatchIngestReceiptV1::create(
                BatchIngestResultKindV1::Unchanged,
                all_envelope.transaction_id.clone(),
                all_envelope.semantic_time_unix_ns,
                all_intent.hash,
                binding.hash(),
                parent_state.head.generation,
                parent_state.head.commit_hash,
                parent_state.head.logical_digest,
                all_plan.outcomes,
                None,
            )
            .unwrap()
            .canonical_bytes(),
            unchanged
        );
        assert_eq!(
            validate_batch_receipt_wire(&unchanged)
                .unwrap()
                .canonical_bytes()
                .unwrap(),
            unchanged
        );
        let failure = production_batch_fixture("positive/batch_failure_install_pending.json");
        assert_eq!(
            validate_batch_failure_wire(&failure)
                .unwrap()
                .canonical_bytes()
                .unwrap(),
            failure
        );
        assert_eq!(
            hex_lower(&production_batch_fixture(
                "positive/logical_state_v2_digest.bin"
            )),
            "7be02326c956cd34503980a7f425930564ef89aeb7ac30aebb4cef632b61ba0d"
        );

        let item0 = &items[0];
        for relative in [
            "negative/item_future_version.bin",
            "negative/item_trailing.bin",
        ] {
            assert!(
                ProductionBatchItemCodecV1::decode(&production_batch_fixture(relative), item0)
                    .is_err()
            );
        }
        for relative in [
            "negative/intent_future_version.bin",
            "negative/intent_unknown_operation.bin",
            "negative/intent_reordered_items.bin",
            "negative/intent_trailing.bin",
            "negative/intent_truncated.bin",
        ] {
            assert!(
                ProductionBatchIntentV1::decode(&production_batch_fixture(relative), &items)
                    .is_err()
            );
        }
        for relative in [
            "negative/envelope_future_version.bin",
            "negative/envelope_zero_uuid.bin",
            "negative/envelope_trailing.bin",
        ] {
            assert!(
                ProductionBatchEnvelopeV1::decode(&production_batch_fixture(relative)).is_err()
            );
        }
        assert!(
            validate_batch_receipt_wire(&production_batch_fixture(
                "negative/receipt_future_version.json"
            ))
            .is_err()
        );
        assert!(
            validate_batch_failure_wire(&production_batch_fixture(
                "negative/failure_unknown_code.json"
            ))
            .is_err()
        );
        for (label, bytes) in [
            ("batch component", component),
            ("batch pair", pair),
            ("batch append", append),
            ("batch prepare", prepare),
            ("batch commit", commit),
        ] {
            let mut appended = bytes;
            appended.push(b'\n');
            let rejected = match label {
                "batch component" => {
                    ProductionComponentDescriptorV1::decode(&appended, label).is_err()
                }
                "batch pair" => ProductionPairDescriptorV1::decode(&appended, label).is_err(),
                "batch append" => BatchCasAppendDescriptorV1::decode(&appended, label).is_err(),
                "batch prepare" => BatchPrepareV1::decode(&appended, label).is_err(),
                "batch commit" => BatchGenerationManifestV1::decode(&appended, label).is_err(),
                _ => unreachable!(),
            };
            assert!(rejected, "{label} admitted appended bytes");
        }
    }

    #[test]
    fn production_batch_edgefree_all_refused_is_exact_parent_noop() {
        const NOOP_TX: &str = "523e4567-e89b-42d3-a456-426614174004";
        const NOOP_TIME: u64 = 1_720_000_000_000_000_101;
        let temp = tempdir().unwrap();
        let root = temp.path().join("production-batch-noop");
        activate_empty_production_base(&root);
        let parent_body = minimal_production_atom();
        let parent_type = AtomBodyHeader::from_bytes(parent_body)
            .unwrap()
            .atom_type()
            .unwrap();
        let mut store = ProductionMemoryX::open(&root).unwrap();
        store
            .direct_ingest(TX1, NOOP_TIME - 101, parent_body, parent_type, &[], &[])
            .unwrap();
        let mut items = production_batch_mixed_items(NOOP_TIME - 1)[4..].to_vec();
        let parent_id = compute_atom_id_from_payload(parent_body).unwrap();
        let mut live_evidence = parent_id.to_vec();
        live_evidence.extend_from_slice(&1u32.to_le_bytes());
        live_evidence.extend_from_slice(&0u64.to_le_bytes());
        live_evidence.extend_from_slice(&1u64.to_le_bytes());
        live_evidence.extend_from_slice(&5000u16.to_le_bytes());
        items.push(
            BatchIngestItemV1::from_body(
                production_body_variant(6, NOOP_TIME),
                AtomType::from_u32(6).unwrap(),
                [],
                live_evidence,
            )
            .unwrap(),
        );
        items.push(
            BatchIngestItemV1::from_body(
                production_body_variant(7, NOOP_TIME),
                AtomType::from_u32(7).unwrap(),
                [0u8; 25],
                [],
            )
            .unwrap(),
        );
        let parent_tree = production_tree_snapshot(&root);
        let parent_digest = store.committed_logical_digest();
        let receipt = store.batch_ingest(NOOP_TX, NOOP_TIME, &items).unwrap();
        assert_eq!(receipt.result, BatchIngestResultKindV1::Unchanged);
        assert_eq!(receipt.committed_generation, 1);
        assert_eq!(receipt.logical_digest, parent_digest);
        assert!(receipt.history_event_id.is_none());
        assert!(
            receipt
                .outcomes
                .iter()
                .all(|outcome| outcome.result == BatchIngestItemResultV1::Refused)
        );
        assert!(
            receipt
                .outcomes
                .iter()
                .skip(3)
                .all(|outcome| outcome.reason == BatchIngestItemReasonV1::InvalidItem)
        );
        assert_eq!(production_tree_snapshot(&root), parent_tree);
        assert_eq!(
            store
                .batch_ingest(NOOP_TX, NOOP_TIME, &items)
                .unwrap()
                .canonical_bytes(),
            receipt.canonical_bytes()
        );
        let mut divergent = items.clone();
        divergent.swap(0, 1);
        assert_eq!(
            store
                .batch_ingest(NOOP_TX, NOOP_TIME, &divergent)
                .unwrap_err()
                .code,
            DirectIngestFailureCodeV1::ConflictingTransactionReuse
        );
        assert_eq!(production_tree_snapshot(&root), parent_tree);
        drop(store);

        let mut first = ProductionMemoryX::open(&root).unwrap();
        assert_eq!(
            first
                .batch_ingest(NOOP_TX, NOOP_TIME, &items)
                .unwrap()
                .canonical_bytes(),
            receipt.canonical_bytes()
        );
        drop(first);
        let second = ProductionMemoryX::open(&root).unwrap();
        assert_eq!(second.committed_generation(), 1);
        assert_eq!(second.committed_logical_digest(), parent_digest);
        drop(second);
        assert_eq!(production_tree_snapshot(&root), parent_tree);
        let transport =
            match crate::store::api::MemoryX::new(crate::store::api::StoreConfig::new(root)) {
                Err(error) => error,
                Ok(_) => panic!("legacy transport unexpectedly opened production format"),
            };
        assert!(
            transport
                .to_string()
                .contains("legacy mutable open is refused")
        );
    }

    #[test]
    fn mx50_adversary_unchanged_receipt_remains_terminal_after_later_commit() {
        const NOOP_TX: &str = "623e4567-e89b-42d3-a456-426614174006";
        const LATER_TX: &str = "723e4567-e89b-42d3-a456-426614174007";
        const TIME: u64 = 1_720_000_000_000_000_300;
        let temp = tempdir().unwrap();
        let root = temp.path().join("mx50-unchanged-after-commit");
        activate_empty_production_base(&root);
        let parent_body = minimal_production_atom();
        let parent_type = AtomBodyHeader::from_bytes(parent_body)
            .unwrap()
            .atom_type()
            .unwrap();
        let mut store = ProductionMemoryX::open(&root).unwrap();
        store
            .direct_ingest(TX1, TIME - 2, parent_body, parent_type, &[], &[])
            .unwrap();

        let refused = production_batch_mixed_items(TIME)[5].clone();
        let first = store
            .batch_ingest(NOOP_TX, TIME, std::slice::from_ref(&refused))
            .unwrap();
        assert_eq!(first.result, BatchIngestResultKindV1::Unchanged);
        assert!(first.history_event_id.is_none());

        let later = BatchIngestItemV1::from_body(
            production_body_variant(8, TIME + 1),
            AtomType::from_u32(8).unwrap(),
            [],
            [],
        )
        .unwrap();
        store.batch_ingest(LATER_TX, TIME + 1, &[later]).unwrap();

        let after_later_commit = production_tree_snapshot(&root);
        let retry = store
            .batch_ingest(NOOP_TX, TIME, std::slice::from_ref(&refused))
            .unwrap();
        assert_eq!(retry.canonical_bytes(), first.canonical_bytes());
        assert_eq!(retry.committed_generation, first.committed_generation);
        assert_eq!(retry.commit_hash, first.commit_hash);
        assert_eq!(retry.logical_digest, first.logical_digest);
        assert_eq!(retry.outcomes, first.outcomes);
        assert_eq!(retry.history_event_id, None);
        assert_eq!(production_tree_snapshot(&root), after_later_commit);
    }

    #[test]
    fn mx50_adversary_unchanged_batch_uuid_cannot_commit_direct_result() {
        const SHARED_TX: &str = "823e4567-e89b-42d3-a456-426614174008";
        const TIME: u64 = 1_720_000_000_000_000_400;
        let temp = tempdir().unwrap();
        let root = temp.path().join("mx50-cross-operation-uuid");
        activate_empty_production_base(&root);
        let mut store = ProductionMemoryX::open(&root).unwrap();
        let refused = production_batch_mixed_items(TIME)[5].clone();
        let first = store
            .batch_ingest(SHARED_TX, TIME, std::slice::from_ref(&refused))
            .unwrap();
        assert_eq!(first.result, BatchIngestResultKindV1::Unchanged);
        let before_direct = production_tree_snapshot(&root);

        let body = minimal_production_atom();
        let atom_type = AtomBodyHeader::from_bytes(body)
            .unwrap()
            .atom_type()
            .unwrap();
        let failure = store
            .direct_ingest(SHARED_TX, TIME, body, atom_type, &[], &[])
            .unwrap_err();
        assert_eq!(
            failure.code,
            DirectIngestFailureCodeV1::ConflictingTransactionReuse
        );
        assert_eq!(store.committed_generation(), 0);
        assert_eq!(store.committed_atom_id(), None);
        assert_eq!(production_tree_snapshot(&root), before_direct);
        assert_eq!(
            store
                .batch_ingest(SHARED_TX, TIME, std::slice::from_ref(&refused))
                .unwrap()
                .canonical_bytes(),
            first.canonical_bytes()
        );
    }

    #[test]
    fn production_transaction_uuid_namespace_rejects_direct_created_then_batch() {
        const SHARED_TX: &str = "923e4567-e89b-42d3-a456-426614174009";
        const TIME: u64 = 1_720_000_000_000_000_500;
        let temp = tempdir().unwrap();
        let root = temp
            .path()
            .join("production-direct-created-cross-operation");
        activate_empty_production_base(&root);
        let body = minimal_production_atom();
        let atom_type = AtomBodyHeader::from_bytes(body)
            .unwrap()
            .atom_type()
            .unwrap();
        let mut store = ProductionMemoryX::open(&root).unwrap();
        let created = store
            .direct_ingest(SHARED_TX, TIME, body, atom_type, &[], &[])
            .unwrap();
        assert_eq!(created.result, DirectIngestResultKindV1::Created);
        assert_eq!(
            store
                .direct_ingest(SHARED_TX, TIME, body, atom_type, &[], &[])
                .unwrap()
                .canonical_bytes(),
            created.canonical_bytes()
        );

        let item = BatchIngestItemV1::from_body(
            production_body_variant(2, TIME + 1),
            AtomType::from_u32(2).unwrap(),
            [],
            [],
        )
        .unwrap();
        let before_batch = production_tree_snapshot(&root);
        let failure = store.batch_ingest(SHARED_TX, TIME, &[item]).unwrap_err();
        assert_eq!(
            failure.code,
            DirectIngestFailureCodeV1::ConflictingTransactionReuse
        );
        assert_eq!(production_tree_snapshot(&root), before_batch);
    }

    #[test]
    fn production_transaction_uuid_namespace_preserves_identical_direct_and_batch_retries() {
        const DIRECT_NOOP_TX: &str = "a23e4567-e89b-42d3-a456-42661417400a";
        const BATCH_TX: &str = "b23e4567-e89b-42d3-a456-42661417400b";
        const TIME: u64 = 1_720_000_000_000_000_600;
        let temp = tempdir().unwrap();
        let root = temp.path().join("production-shared-transaction-namespace");
        activate_empty_production_base(&root);
        let body = minimal_production_atom();
        let atom_type = AtomBodyHeader::from_bytes(body)
            .unwrap()
            .atom_type()
            .unwrap();
        let mut store = ProductionMemoryX::open(&root).unwrap();
        store
            .direct_ingest(TX1, TIME - 2, body, atom_type, &[], &[])
            .unwrap();
        let direct_noop = store
            .direct_ingest(DIRECT_NOOP_TX, TIME - 1, body, atom_type, &[], &[])
            .unwrap();
        assert_eq!(
            direct_noop.result,
            DirectIngestResultKindV1::ReusedCommitted
        );

        let item = BatchIngestItemV1::from_body(
            production_body_variant(2, TIME),
            AtomType::from_u32(2).unwrap(),
            [],
            [],
        )
        .unwrap();
        let batch = store
            .batch_ingest(BATCH_TX, TIME, std::slice::from_ref(&item))
            .unwrap();
        assert_eq!(batch.result, BatchIngestResultKindV1::Committed);
        let after_batch = production_tree_snapshot(&root);

        assert_eq!(
            store
                .direct_ingest(DIRECT_NOOP_TX, TIME - 1, body, atom_type, &[], &[])
                .unwrap()
                .canonical_bytes(),
            direct_noop.canonical_bytes()
        );
        assert_eq!(
            store
                .batch_ingest(BATCH_TX, TIME, std::slice::from_ref(&item))
                .unwrap()
                .canonical_bytes(),
            batch.canonical_bytes()
        );
        assert_eq!(production_tree_snapshot(&root), after_batch);

        assert_eq!(
            store
                .batch_ingest(DIRECT_NOOP_TX, TIME - 1, &[item])
                .unwrap_err()
                .code,
            DirectIngestFailureCodeV1::ConflictingTransactionReuse
        );
        assert_eq!(
            store
                .direct_ingest(BATCH_TX, TIME, body, atom_type, &[], &[])
                .unwrap_err()
                .code,
            DirectIngestFailureCodeV1::ConflictingTransactionReuse
        );
        assert_eq!(production_tree_snapshot(&root), after_batch);
    }

    #[test]
    fn production_batch_edgefree_preflight_bounds_precede_pending_and_nested_lease() {
        const BATCH_TX: &str = "623e4567-e89b-42d3-a456-426614174005";
        let temp = tempdir().unwrap();
        let root = temp.path().join("production-batch-preflight");
        activate_empty_production_base(&root);
        let body = production_body_variant(2, 1_720_000_000_000_000_200);
        let atom_type = AtomType::from_u32(2).unwrap();
        let item = BatchIngestItemV1::from_body(body.clone(), atom_type, [], []).unwrap();
        let mut store = ProductionMemoryX::open(&root).unwrap();
        let before = production_tree_snapshot(&root);
        let nested = match ProductionMemoryX::open(&root) {
            Err(error) => error,
            Ok(_) => panic!("nested production owner unexpectedly opened"),
        };
        assert_eq!(nested.kind(), io::ErrorKind::WouldBlock);

        let too_many = vec![item.clone(); PRODUCTION_BATCH_MAX_ITEMS + 1];
        assert_eq!(
            store
                .batch_ingest(BATCH_TX, 1_720_000_000_000_000_200, &too_many)
                .unwrap_err()
                .code,
            DirectIngestFailureCodeV1::BoundsExceeded
        );
        let malformed_projection =
            BatchIngestItemV1::new(item.atom_id, body, atom_type, vec![0], Vec::<u8>::new());
        assert_eq!(
            store
                .batch_ingest(
                    "623e4567-e89b-42d3-a456-426614174006",
                    1_720_000_000_000_000_201,
                    &[malformed_projection],
                )
                .unwrap_err()
                .code,
            DirectIngestFailureCodeV1::InvalidBatchItem
        );
        assert_eq!(production_tree_snapshot(&root), before);
        let generations = production_txn_root(&root).join(GENERATIONS_DIR_NAME);
        assert!(fs::read_dir(generations).unwrap().next().is_none());
    }

    #[test]
    fn production_batch_edgefree_later_generation_preserves_older_retry_and_indexes() {
        const FIRST_TX: &str = "423e4567-e89b-42d3-a456-426614174003";
        const SECOND_TX: &str = "623e4567-e89b-42d3-a456-426614174007";
        const TIME: u64 = 1_720_000_000_000_000_300;
        let temp = tempdir().unwrap();
        let root = temp.path().join("production-batch-chain");
        activate_empty_production_base(&root);
        let parent_body = minimal_production_atom();
        let parent_type = AtomBodyHeader::from_bytes(parent_body)
            .unwrap()
            .atom_type()
            .unwrap();
        let mut store = ProductionMemoryX::open(&root).unwrap();
        store
            .direct_ingest(TX1, TIME - 100, parent_body, parent_type, &[], &[])
            .unwrap();
        let first_items = production_batch_mixed_items(TIME);
        let first = store.batch_ingest(FIRST_TX, TIME, &first_items).unwrap();
        let body4 = production_body_variant(4, TIME + 1);
        let item4 =
            BatchIngestItemV1::from_body(body4, AtomType::from_u32(4).unwrap(), [], []).unwrap();
        let second = store.batch_ingest(SECOND_TX, TIME + 1, &[item4]).unwrap();
        assert_eq!(second.committed_generation, 3);
        assert_eq!(store.committed_atom_ids().len(), 4);
        let after_second = production_tree_snapshot(&root);
        let older_retry = store.batch_ingest(FIRST_TX, TIME, &first_items).unwrap();
        assert_eq!(older_retry.canonical_bytes(), first.canonical_bytes());
        assert_eq!(production_tree_snapshot(&root), after_second);
        let graph = GraphManifest::read_from_file(root.join("graph/graph.manifest")).unwrap();
        assert_eq!(graph.node_count, 4);
        assert_eq!(graph.delta_count, 0);
        assert_eq!(graph.edge_type_mask, 0);
        assert!(
            !fs::read(production_generation_path(&root, 3).join(COMMIT_FILE_NAME))
                .unwrap()
                .windows(4)
                .any(|window| window == b"DELT")
        );
        let index = fs::read(root.join("cas/seg_00000.idx")).unwrap();
        assert_eq!(IndexFileHeader::from_bytes(&index).unwrap().entry_count, 4);
        let history = fs::read(root.join("meta/history.log")).unwrap();
        assert_eq!(history.iter().filter(|byte| **byte == b'\n').count(), 3);
        drop(store);

        let first_reopen = ProductionMemoryX::open(&root).unwrap();
        assert_eq!(first_reopen.committed_generation(), 3);
        assert_eq!(first_reopen.committed_atom_ids().len(), 4);
        assert_eq!(
            first_reopen.committed_logical_digest(),
            second.logical_digest
        );
        drop(first_reopen);
        let second_reopen = ProductionMemoryX::open(&root).unwrap();
        assert_eq!(second_reopen.committed_atom_ids().len(), 4);
        assert_eq!(production_tree_snapshot(&root), after_second);
    }

    #[test]
    fn production_batch_edgefree_precommit_rollback_and_postcommit_rollforward_are_stable() {
        const BATCH_TX: &str = "723e4567-e89b-42d3-a456-426614174008";
        const TIME: u64 = 1_720_000_000_000_000_400;
        for postcommit in [false, true] {
            let temp = tempdir().unwrap();
            let root = temp.path().join(if postcommit {
                "production-batch-postcommit"
            } else {
                "production-batch-precommit"
            });
            activate_empty_production_base(&root);
            let parent_body = minimal_production_atom();
            let parent_type = AtomBodyHeader::from_bytes(parent_body)
                .unwrap()
                .atom_type()
                .unwrap();
            let mut store = ProductionMemoryX::open(&root).unwrap();
            store
                .direct_ingest(TX1, TIME - 100, parent_body, parent_type, &[], &[])
                .unwrap();
            let parent_tree = production_tree_snapshot(&root);
            let item = BatchIngestItemV1::from_body(
                production_body_variant(2, TIME),
                AtomType::from_u32(2).unwrap(),
                [],
                [],
            )
            .unwrap();
            let receipt = store
                .batch_ingest(BATCH_TX, TIME, std::slice::from_ref(&item))
                .unwrap();
            drop(store);
            let generation = production_generation_path(&root, 2);
            if !postcommit {
                let pending = production_txn_root(&root)
                    .join(GENERATIONS_DIR_NAME)
                    .join(format!(".pending-{BATCH_TX}"));
                fs::rename(&generation, &pending).unwrap();
                fs::remove_file(pending.join(COMMIT_FILE_NAME)).unwrap();
            }
            for (relative, bytes) in &parent_tree {
                if relative.starts_with("operation_txn/") || relative == "cas/seg_00000.dat" {
                    continue;
                }
                fs::write(
                    root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR)),
                    bytes,
                )
                .unwrap();
            }

            if postcommit {
                let recovered = ProductionMemoryX::open(&root).unwrap();
                assert_eq!(recovered.committed_generation(), 2);
                assert_eq!(recovered.committed_logical_digest(), receipt.logical_digest);
                drop(recovered);
                let repeated = ProductionMemoryX::open(&root).unwrap();
                assert_eq!(repeated.committed_atom_ids().len(), 2);
            } else {
                let mut recovered = ProductionMemoryX::open(&root).unwrap();
                assert_eq!(recovered.committed_generation(), 1);
                assert_eq!(recovered.committed_atom_ids().len(), 1);
                let retry = recovered.batch_ingest(BATCH_TX, TIME, &[item]).unwrap();
                assert_eq!(retry.committed_generation, 2);
                assert_eq!(recovered.committed_atom_ids().len(), 2);
                drop(recovered);
                let repeated = ProductionMemoryX::open(&root).unwrap();
                assert_eq!(repeated.committed_logical_digest(), retry.logical_digest);
            }
        }
    }

    #[test]
    fn production_direct_ingest_create_retry_reuse_and_reopen() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("production-base");
        activate_empty_production_base(&root);
        let body = minimal_production_atom();
        let atom_type = AtomBodyHeader::from_bytes(body)
            .unwrap()
            .atom_type()
            .unwrap();

        let mut store = ProductionMemoryX::open(&root).unwrap();
        assert_eq!(store.committed_generation(), 0);
        let created = store
            .direct_ingest(TX1, 1_700_000_000_000_000_000, body, atom_type, &[], &[])
            .unwrap();
        assert_eq!(created.result, DirectIngestResultKindV1::Created);
        assert_eq!(store.committed_generation(), 1);
        assert_eq!(store.committed_atom_id(), Some(created.atom_id));
        let exact_retry = store
            .direct_ingest(TX1, 1_700_000_000_000_000_000, body, atom_type, &[], &[])
            .unwrap();
        assert_eq!(exact_retry.canonical_bytes(), created.canonical_bytes());
        let conflict = store
            .direct_ingest(TX1, 1_700_000_000_000_000_001, body, atom_type, &[], &[])
            .unwrap_err();
        assert_eq!(
            conflict.code,
            DirectIngestFailureCodeV1::ConflictingTransactionReuse
        );
        let reused = store
            .direct_ingest(TX2, 1_700_000_000_000_000_002, body, atom_type, &[], &[])
            .unwrap();
        assert_eq!(reused.result, DirectIngestResultKindV1::ReusedCommitted);
        assert_eq!(reused.committed_generation, 1);
        assert_eq!(reused.history_event_id, None);
        let before_reuse = production_tree_snapshot(&root);
        let repeated_reuse = store
            .direct_ingest(
                "019fca57-b841-79a2-88e5-e6b78a52e552",
                1_700_000_000_000_000_003,
                body,
                atom_type,
                &[],
                &[],
            )
            .unwrap();
        assert_eq!(
            repeated_reuse.result,
            DirectIngestResultKindV1::ReusedCommitted
        );
        assert_eq!(production_tree_snapshot(&root), before_reuse);
        let nested = match ProductionMemoryX::open(&root) {
            Err(error) => error,
            Ok(_) => panic!("nested production owner unexpectedly opened"),
        };
        assert_eq!(nested.kind(), io::ErrorKind::WouldBlock);
        drop(store);

        let first = ProductionMemoryX::open(&root).unwrap();
        let first_digest = first.committed_logical_digest();
        assert_eq!(first.committed_atom_id(), Some(created.atom_id));
        drop(first);
        let second = ProductionMemoryX::open(&root).unwrap();
        assert_eq!(second.committed_logical_digest(), first_digest);
        assert_eq!(second.committed_atom_id(), Some(created.atom_id));
        drop(second);
        let legacy =
            match crate::store::api::MemoryX::new(crate::store::api::StoreConfig::new(root)) {
                Err(error) => error,
                Ok(_) => panic!("legacy mutable writer unexpectedly opened format.v2"),
            };
        assert!(
            legacy
                .to_string()
                .contains("legacy mutable open is refused")
        );
    }

    #[test]
    fn production_direct_reuse_rejects_same_atom_id_with_different_exact_body() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("production-body-conflict");
        activate_empty_production_base(&root);
        let body = minimal_production_atom();
        let atom_type = AtomBodyHeader::from_bytes(body)
            .unwrap()
            .atom_type()
            .unwrap();
        let mut changed = body.to_vec();
        changed[8..16].copy_from_slice(&1u64.to_le_bytes());
        assert_ne!(changed, body);
        assert_eq!(
            compute_atom_id_from_payload(&changed).unwrap(),
            compute_atom_id_from_payload(body).unwrap()
        );

        let mut store = ProductionMemoryX::open(&root).unwrap();
        store
            .direct_ingest(TX1, 1_700_000_000_000_000_000, body, atom_type, &[], &[])
            .unwrap();
        let before = production_tree_snapshot(&root);
        let conflict = store
            .direct_ingest(
                TX2,
                1_700_000_000_000_000_001,
                &changed,
                atom_type,
                &[],
                &[],
            )
            .unwrap_err();
        assert_eq!(
            conflict.code,
            DirectIngestFailureCodeV1::CanonicalRepresentationConflict
        );
        assert_eq!(production_tree_snapshot(&root), before);
        drop(store);
        let reopened = ProductionMemoryX::open(&root).unwrap();
        assert_eq!(
            reopened.committed_atom_id(),
            Some(compute_atom_id_from_payload(body).unwrap())
        );
    }

    #[test]
    fn production_direct_preflight_refuses_nonempty_cas_before_pending_namespace() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("production-preflight");
        activate_empty_production_base(&root);
        let body = minimal_production_atom();
        let atom_type = AtomBodyHeader::from_bytes(body)
            .unwrap()
            .atom_type()
            .unwrap();
        let store = ProductionMemoryX::open(&root).unwrap();
        fs::write(root.join("cas/seg_00000.dat"), b"legacy-cas-record").unwrap();
        let before = production_tree_snapshot(&root);
        let atom_id = compute_atom_id_from_payload(body).unwrap();
        let binding = store.state.base_binding.clone();
        let intent = ProductionDirectIntentV1::create(
            binding.hash(),
            atom_id,
            atom_type.to_u32() as u8,
            body,
            &[],
            &[],
        )
        .unwrap();
        let envelope = ProductionDirectEnvelopeV1::create(
            TX1,
            1_700_000_000_000_000_000,
            binding.hash(),
            intent.hash(),
        )
        .unwrap();
        let request = ProductionDirectRequestV1 {
            transaction_id: TX1.to_owned(),
            semantic_time_unix_ns: 1_700_000_000_000_000_000,
            base_binding_bytes: binding.bytes().to_vec(),
            body: body.to_vec(),
            atom_type,
            claim_projection: Vec::new(),
            evidence_projection: Vec::new(),
        };
        let write = store.authority.borrow_write().unwrap();
        let failure =
            production_stage_direct_ingest(&write, &store.state, &request, &intent, &envelope)
                .unwrap_err();
        assert_eq!(failure.kind(), io::ErrorKind::Unsupported);
        assert!(failure.to_string().starts_with("migration_required:"));
        assert_eq!(production_tree_snapshot(&root), before);
        assert!(
            !production_txn_root(&root)
                .join(GENERATIONS_DIR_NAME)
                .join(format!(".pending-{TX1}"))
                .exists()
        );
    }

    #[cfg(windows)]
    #[test]
    fn production_install_replacement_refuses_target_parent_reparse() {
        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let external = outside.path().join("state.bin");
        fs::write(&external, b"outside-before").unwrap();
        let linked_parent = root.path().join("meta");
        if std::os::windows::fs::symlink_dir(outside.path(), &linked_parent).is_err() {
            return;
        }

        let error = production_install_replacement(root.path(), "meta/state.bin", b"replacement")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(external).unwrap(), b"outside-before");
    }

    #[test]
    fn production_direct_codecs_match_ratified_positive_vectors() {
        let binding_bytes = production_codec_fixture("positive/base_binding_windows_v1.bin");
        let binding = ProductionBaseBindingV1::decode(&binding_bytes).unwrap();
        assert_eq!(binding.bytes(), binding_bytes);
        let intent_bytes = production_codec_fixture("positive/direct_ingest_intent_v1.bin");
        let intent = ProductionDirectIntentV1::decode(&intent_bytes).unwrap();
        let rebuilt_intent = ProductionDirectIntentV1::create(
            binding.hash(),
            intent.atom_id,
            intent.atom_type,
            minimal_production_atom(),
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(rebuilt_intent.bytes(), intent_bytes);
        let envelope_bytes = production_codec_fixture("positive/direct_ingest_envelope_v1.bin");
        let envelope = ProductionDirectEnvelopeV1::decode(&envelope_bytes).unwrap();
        let rebuilt_envelope = ProductionDirectEnvelopeV1::create(
            &envelope.transaction_id,
            envelope.semantic_time_unix_ns,
            envelope.base_binding_hash,
            envelope.intent_hash,
        )
        .unwrap();
        assert_eq!(rebuilt_envelope.bytes(), envelope_bytes);
        let receipt_bytes =
            production_codec_fixture("positive/direct_ingest_receipt_created_v1.json");
        let receipt = DirectIngestReceiptV1::decode(&receipt_bytes).unwrap();
        let rebuilt_receipt = DirectIngestReceiptV1::create(
            receipt.result,
            receipt.transaction_id.clone(),
            receipt.semantic_time_unix_ns,
            receipt.intent_hash,
            receipt.base_binding_hash,
            receipt.committed_generation,
            receipt.commit_hash,
            receipt.logical_digest,
            receipt.atom_id,
            receipt.node_num,
            receipt.history_event_id,
        )
        .unwrap();
        assert_eq!(rebuilt_receipt.canonical_bytes(), receipt_bytes);
        let startup_bytes =
            production_codec_fixture("positive/startup_no_repair_admission_v1.json");
        let startup = ProductionStartupAdmissionV1::decode(&startup_bytes, "startup").unwrap();
        assert_eq!(startup.canonical_bytes().unwrap(), startup_bytes);
    }

    #[test]
    fn production_direct_open_and_request_fail_closed_before_mutation() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("not-admitted");
        let error = match ProductionMemoryX::open(&root) {
            Err(error) => error,
            Ok(_) => panic!("unadmitted production base unexpectedly opened"),
        };
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(!production_txn_root(&root).exists());

        let admitted = temp.path().join("admitted");
        activate_empty_production_base(&admitted);
        let before = production_tree_snapshot(&admitted);
        let mut store = ProductionMemoryX::open(&admitted).unwrap();
        let body = minimal_production_atom();
        let atom_type = AtomBodyHeader::from_bytes(body)
            .unwrap()
            .atom_type()
            .unwrap();
        let invalid_uuid = store
            .direct_ingest("NOT-A-UUID", 1, body, atom_type, &[], &[])
            .unwrap_err();
        assert_eq!(
            invalid_uuid.code,
            DirectIngestFailureCodeV1::InvalidTransactionId
        );
        let invalid_time = store
            .direct_ingest(TX1, 0, body, atom_type, &[], &[])
            .unwrap_err();
        assert_eq!(
            invalid_time.code,
            DirectIngestFailureCodeV1::InvalidSemanticTime
        );
        let projection = store
            .direct_ingest(TX1, 1, body, atom_type, &[0; 25], &[])
            .unwrap_err();
        assert_eq!(
            projection.code,
            DirectIngestFailureCodeV1::CompositeOperationNotAdmitted
        );
        assert_eq!(production_tree_snapshot(&admitted), before);
    }

    #[test]
    fn production_direct_open_rejects_corrupt_commit_without_repair() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("corrupt-commit");
        activate_empty_production_base(&root);
        let body = minimal_production_atom();
        let atom_type = AtomBodyHeader::from_bytes(body)
            .unwrap()
            .atom_type()
            .unwrap();
        let mut store = ProductionMemoryX::open(&root).unwrap();
        store
            .direct_ingest(TX1, 1_700_000_000_000_000_000, body, atom_type, &[], &[])
            .unwrap();
        drop(store);
        let commit = production_generation_path(&root, 1).join(COMMIT_FILE_NAME);
        let mut bytes = fs::read(&commit).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 1;
        fs::write(&commit, bytes).unwrap();
        let before = production_tree_snapshot(&root);
        assert!(ProductionMemoryX::open(&root).is_err());
        assert_eq!(production_tree_snapshot(&root), before);
    }

    #[test]
    fn production_direct_open_recovers_valid_pending_and_refuses_unknown_pending() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("pending-recovery");
        activate_empty_production_base(&root);
        let baseline = production_tree_snapshot(&root);
        let generations = production_txn_root(&root).join(GENERATIONS_DIR_NAME);
        let pending = generations.join(format!(".pending-{TX1}"));
        fs::create_dir(&pending).unwrap();
        fs::create_dir(pending.join(COMPONENTS_DIR_NAME)).unwrap();
        fs::write(pending.join(PREPARE_FILE_NAME), b"incomplete-precommit").unwrap();
        let store = ProductionMemoryX::open(&root).unwrap();
        assert_eq!(store.committed_generation(), 0);
        drop(store);
        assert!(!pending.exists());
        assert_eq!(production_tree_snapshot(&root), baseline);

        let malformed = generations.join(format!(".pending-{TX2}"));
        fs::create_dir(&malformed).unwrap();
        fs::create_dir(malformed.join(COMPONENTS_DIR_NAME)).unwrap();
        fs::write(malformed.join("unknown.bin"), b"do-not-delete").unwrap();
        let before = production_tree_snapshot(&root);
        assert!(ProductionMemoryX::open(&root).is_err());
        assert_eq!(production_tree_snapshot(&root), before);
    }

    #[test]
    fn production_direct_open_rolls_forward_committed_install() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("install-recovery");
        activate_empty_production_base(&root);
        let baseline = production_tree_snapshot(&root);
        let body = minimal_production_atom();
        let atom_type = AtomBodyHeader::from_bytes(body)
            .unwrap()
            .atom_type()
            .unwrap();
        let mut store = ProductionMemoryX::open(&root).unwrap();
        let receipt = store
            .direct_ingest(TX1, 1_700_000_000_000_000_000, body, atom_type, &[], &[])
            .unwrap();
        drop(store);

        for (relative, bytes) in &baseline {
            if relative.starts_with("operation_txn/") || relative == "cas/seg_00000.dat" {
                continue;
            }
            let path = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
            fs::write(path, bytes).unwrap();
        }
        let recovered = ProductionMemoryX::open(&root).unwrap();
        assert_eq!(recovered.committed_generation(), 1);
        assert_eq!(recovered.committed_atom_id(), Some(receipt.atom_id));
        let first = production_tree_snapshot(&root);
        drop(recovered);
        let reopened = ProductionMemoryX::open(&root).unwrap();
        assert_eq!(reopened.committed_logical_digest(), receipt.logical_digest);
        drop(reopened);
        assert_eq!(production_tree_snapshot(&root), first);
    }

    #[test]
    fn production_direct_open_rolls_back_exact_precommit_cas_suffix() {
        for partial in [false, true] {
            let temp = tempdir().unwrap();
            let root = temp.path().join(if partial {
                "precommit-partial-orphan"
            } else {
                "precommit-full-orphan"
            });
            activate_empty_production_base(&root);
            let baseline = production_tree_snapshot(&root);
            let body = minimal_production_atom();
            let atom_type = AtomBodyHeader::from_bytes(body)
                .unwrap()
                .atom_type()
                .unwrap();
            let mut store = ProductionMemoryX::open(&root).unwrap();
            store
                .direct_ingest(TX1, 1_700_000_000_000_000_000, body, atom_type, &[], &[])
                .unwrap();
            drop(store);

            let generations = production_txn_root(&root).join(GENERATIONS_DIR_NAME);
            let pending = generations.join(format!(".pending-{TX1}"));
            fs::rename(production_generation_path(&root, 1), &pending).unwrap();
            fs::remove_file(pending.join(COMMIT_FILE_NAME)).unwrap();
            for (relative, bytes) in &baseline {
                if relative.starts_with("operation_txn/") || relative == "cas/seg_00000.dat" {
                    continue;
                }
                fs::write(
                    root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR)),
                    bytes,
                )
                .unwrap();
            }
            if partial {
                let segment = root.join("cas/seg_00000.dat");
                let length = fs::metadata(&segment).unwrap().len();
                OpenOptions::new()
                    .write(true)
                    .open(&segment)
                    .unwrap()
                    .set_len(length / 2)
                    .unwrap();
            }

            production_rollback_pending_cas(&root, TX1, &pending).unwrap();
            assert_eq!(
                fs::metadata(root.join("cas/seg_00000.dat")).unwrap().len(),
                0
            );

            let reopened = ProductionMemoryX::open(&root).unwrap();
            assert_eq!(reopened.committed_generation(), 0);
            assert_eq!(reopened.committed_atom_id(), None);
            drop(reopened);
            assert_eq!(production_tree_snapshot(&root), baseline);
            let repeated = ProductionMemoryX::open(&root).unwrap();
            assert_eq!(repeated.committed_generation(), 0);
        }
    }

    #[test]
    fn production_direct_installed_components_cross_validate_with_current_readers() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("reader-cross-validation");
        activate_empty_production_base(&root);
        let body = minimal_production_atom();
        let atom_type = AtomBodyHeader::from_bytes(body)
            .unwrap()
            .atom_type()
            .unwrap();
        let mut store = ProductionMemoryX::open(&root).unwrap();
        let receipt = store
            .direct_ingest(TX1, 1_700_000_000_000_000_000, body, atom_type, &[], &[])
            .unwrap();
        drop(store);

        let segment = fs::read(root.join("cas/seg_00000.dat")).unwrap();
        let record = RecordHeader::from_bytes(&segment[..RecordHeader::SIZE]).unwrap();
        assert_eq!(record.atom_id, receipt.atom_id);
        assert_eq!(record.body_len, body.len() as u64);
        assert_eq!(
            &segment[RecordHeader::SIZE..RecordHeader::SIZE + body.len()],
            body
        );
        let index = fs::read(root.join("cas/seg_00000.idx")).unwrap();
        let index_header = IndexFileHeader::from_bytes(&index).unwrap();
        assert!(index_header.is_valid());
        assert_eq!(index_header.entry_count, 1);
        let index_entry = IndexEntry::from_bytes(&index[IndexFileHeader::SIZE..]).unwrap();
        assert_eq!(index_entry.seg_offset, 0);
        assert_eq!(index_entry.body_len, body.len() as u32);

        let idloc = crate::index::IdLocIndex::open(&root.join("index/idloc.mmap")).unwrap();
        let location = idloc.locate(&receipt.atom_id).unwrap();
        assert_eq!(location.seg_id, 0);
        assert_eq!(location.offset, 0);
        assert_eq!(location.len, body.len() as u32);
        assert_eq!(location.node_num, receipt.node_num);
        let lexicon = crate::index::Lexicon::read_from_file(&root.join("index/terms.lex")).unwrap();
        let postings =
            crate::index::Postings::read_from_file(&root.join("index/terms.post")).unwrap();
        assert_eq!(lexicon.len(), 0);
        assert_eq!(postings.len(), 0);
        assert_eq!(postings.total_docs(), 0);
        let graph = GraphManifest::read_from_file(root.join("graph/graph.manifest")).unwrap();
        assert_eq!(graph.node_count, 1);
        assert_eq!(graph.delta_count, 0);
        assert_eq!(graph.edge_type_mask, 0);
        let metadata = fs::read(root.join("meta/meta_state.bin")).unwrap();
        assert_eq!(u64::from_le_bytes(metadata[8..16].try_into().unwrap()), 1);
        assert_eq!(&metadata[16..48], &receipt.atom_id);
        assert_eq!(
            u64::from_le_bytes(metadata[48..56].try_into().unwrap()),
            receipt.node_num
        );
        let history = fs::read(root.join("meta/history.log")).unwrap();
        let (event, _) = production_history_from_line(&history).unwrap();
        assert_eq!(event.transaction_id, TX1);
        assert_eq!(event.atom_ids, vec![hex_lower(&receipt.atom_id)]);
    }

    #[derive(Clone, Default)]
    struct RecordingFailpoint {
        events: Arc<Mutex<Vec<(OperationStage, usize)>>>,
        fail_at: Option<(OperationStage, usize)>,
        mutation: Option<(OperationStage, PathBuf, Vec<u8>)>,
    }

    impl OperationFailpoint for RecordingFailpoint {
        fn hit(&mut self, stage: OperationStage, occurrence: usize) -> io::Result<()> {
            self.events.lock().unwrap().push((stage, occurrence));
            if let Some((mutation_stage, path, bytes)) = &self.mutation
                && *mutation_stage == stage
                && occurrence == 0
            {
                fs::write(path, bytes)?;
                self.mutation = None;
            }
            if self.fail_at == Some((stage, occurrence)) {
                return Err(io::Error::other("injected N5-A boundary failure"));
            }
            Ok(())
        }
    }

    #[derive(Clone, Copy)]
    enum NamespaceSubstitution {
        PublicationParent,
        PublicationObject,
    }

    struct NamespaceSubstitutionFailpoint {
        root: PathBuf,
        pending: PathBuf,
        kind: NamespaceSubstitution,
        substituted: Arc<Mutex<bool>>,
    }

    impl OperationFailpoint for NamespaceSubstitutionFailpoint {
        fn hit(&mut self, stage: OperationStage, occurrence: usize) -> io::Result<()> {
            if stage != OperationStage::BeforeCommitPublish || occurrence != 0 {
                return Ok(());
            }
            let generations = generations_dir(&self.root);
            let displaced = match self.kind {
                NamespaceSubstitution::PublicationParent => {
                    transaction_dir(&self.root).join("generations.displaced")
                }
                NamespaceSubstitution::PublicationObject => generations.join("pending.displaced"),
            };
            let source = match self.kind {
                NamespaceSubstitution::PublicationParent => generations.clone(),
                NamespaceSubstitution::PublicationObject => self.pending.clone(),
            };
            if fs::rename(&source, &displaced).is_err() {
                return Ok(());
            }
            match self.kind {
                NamespaceSubstitution::PublicationParent => fs::create_dir(&generations)?,
                NamespaceSubstitution::PublicationObject => {
                    fs::create_dir(&self.pending)?;
                    fs::write(self.pending.join("replacement.sentinel"), b"replacement")?;
                }
            }
            *self.substituted.lock().unwrap() = true;
            Ok(())
        }
    }

    struct HardLinkPublicationFailpoint {
        source: PathBuf,
        alias: PathBuf,
    }

    impl OperationFailpoint for HardLinkPublicationFailpoint {
        fn hit(&mut self, stage: OperationStage, occurrence: usize) -> io::Result<()> {
            if stage == OperationStage::BeforeGenerationPublish && occurrence == 0 {
                fs::hard_link(&self.source, &self.alias)?;
            }
            Ok(())
        }
    }

    struct FinalNamespaceSubstitutionSeam {
        selected: NamespaceMutationKind,
        outside: PathBuf,
        attempted: Arc<Mutex<bool>>,
        substituted: Arc<Mutex<bool>>,
        fail_after_attempt: bool,
        public_failure: Option<OperationStage>,
    }

    impl OperationFailpoint for FinalNamespaceSubstitutionSeam {
        fn hit(&mut self, stage: OperationStage, occurrence: usize) -> io::Result<()> {
            if self.public_failure == Some(stage) && occurrence == 0 {
                return Err(io::Error::other(
                    "injected public failure before handle-bound cleanup",
                ));
            }
            Ok(())
        }

        fn before_namespace_mutation(
            &mut self,
            kind: NamespaceMutationKind,
            source: &Path,
            _target: Option<&Path>,
        ) -> io::Result<()> {
            if kind != self.selected || *self.attempted.lock().unwrap() {
                return Ok(());
            }
            *self.attempted.lock().unwrap() = true;
            let displaced = self.outside.join("displaced-final-component");
            let source_is_directory = source.is_dir();
            if fs::rename(source, &displaced).is_ok() {
                if source_is_directory {
                    fs::create_dir(source)?;
                    fs::write(source.join("replacement.sentinel"), b"replacement")?;
                } else {
                    fs::write(source, b"replacement")?;
                }
                *self.substituted.lock().unwrap() = true;
            }
            if self.fail_after_attempt {
                return Err(io::Error::other(
                    "injected error after final-component substitution attempt",
                ));
            }
            Ok(())
        }
    }

    struct FinalHardLinkSeam {
        source_kind: NamespaceMutationKind,
        alias: PathBuf,
        linked: Arc<Mutex<bool>>,
    }

    impl OperationFailpoint for FinalHardLinkSeam {
        fn hit(&mut self, _stage: OperationStage, _occurrence: usize) -> io::Result<()> {
            Ok(())
        }

        fn before_namespace_mutation(
            &mut self,
            kind: NamespaceMutationKind,
            source: &Path,
            _target: Option<&Path>,
        ) -> io::Result<()> {
            if kind == self.source_kind && !*self.linked.lock().unwrap() {
                fs::hard_link(source, &self.alias)?;
                *self.linked.lock().unwrap() = true;
            }
            Ok(())
        }
    }

    struct AfterRemovalFailpoint {
        suffix: String,
        attempted: Arc<Mutex<bool>>,
    }

    impl OperationFailpoint for AfterRemovalFailpoint {
        fn hit(&mut self, _stage: OperationStage, _occurrence: usize) -> io::Result<()> {
            Ok(())
        }

        fn after_namespace_mutation(
            &mut self,
            _kind: NamespaceMutationKind,
            path: &Path,
        ) -> io::Result<()> {
            let normalized = path.to_string_lossy().replace('\\', "/");
            if !*self.attempted.lock().unwrap()
                && (path.file_name().and_then(|name| name.to_str()) == Some(self.suffix.as_str())
                    || normalized.ends_with(&format!("/{}", self.suffix)))
            {
                *self.attempted.lock().unwrap() = true;
                return Err(io::Error::other(
                    "injected failure after exact tombstone child removal",
                ));
            }
            Ok(())
        }
    }

    struct LateChildMutationSeam {
        relative: &'static str,
        outside: PathBuf,
        hard_link: bool,
        fail_after_attempt: bool,
        attempted: Arc<Mutex<bool>>,
        substituted: Arc<Mutex<bool>>,
    }

    impl OperationFailpoint for LateChildMutationSeam {
        fn hit(&mut self, _stage: OperationStage, _occurrence: usize) -> io::Result<()> {
            Ok(())
        }

        fn before_namespace_mutation(
            &mut self,
            kind: NamespaceMutationKind,
            source: &Path,
            _target: Option<&Path>,
        ) -> io::Result<()> {
            if kind != NamespaceMutationKind::DirectoryRename || *self.attempted.lock().unwrap() {
                return Ok(());
            }
            *self.attempted.lock().unwrap() = true;
            let child = source.join(self.relative);
            let displaced = self.outside.join("late-child-original");
            if self.hard_link {
                fs::hard_link(&child, &displaced)?;
                *self.substituted.lock().unwrap() = true;
            } else if fs::rename(&child, &displaced).is_ok() {
                fs::write(&child, b"late replacement")?;
                *self.substituted.lock().unwrap() = true;
            }
            if self.fail_after_attempt {
                return Err(io::Error::other(
                    "injected error after late child substitution attempt",
                ));
            }
            Ok(())
        }
    }

    struct AfterCloseReplacementSeam {
        selected: &'static str,
        attempted: Arc<Mutex<bool>>,
    }

    impl OperationFailpoint for AfterCloseReplacementSeam {
        fn hit(&mut self, _stage: OperationStage, _occurrence: usize) -> io::Result<()> {
            Ok(())
        }

        fn after_namespace_handle_close(
            &mut self,
            _kind: NamespaceMutationKind,
            path: &Path,
        ) -> io::Result<()> {
            if !*self.attempted.lock().unwrap()
                && path.file_name().and_then(|name| name.to_str()) == Some(self.selected)
            {
                *self.attempted.lock().unwrap() = true;
                fs::write(path, b"post-close replacement")?;
            }
            Ok(())
        }
    }

    struct BeforeVisibilityFailure {
        attempted: Arc<Mutex<bool>>,
    }

    impl OperationFailpoint for BeforeVisibilityFailure {
        fn hit(&mut self, _stage: OperationStage, _occurrence: usize) -> io::Result<()> {
            Ok(())
        }

        fn before_write_through_visibility(
            &mut self,
            _source: &Path,
            _target: &Path,
        ) -> io::Result<()> {
            *self.attempted.lock().unwrap() = true;
            Err(io::Error::other(
                "injected failure before write-through visibility",
            ))
        }
    }

    struct BeforeVisibilityChildLinkSeam {
        relative: &'static str,
        alias: PathBuf,
        attempted: Arc<Mutex<bool>>,
    }

    impl OperationFailpoint for BeforeVisibilityChildLinkSeam {
        fn hit(&mut self, _stage: OperationStage, _occurrence: usize) -> io::Result<()> {
            Ok(())
        }

        fn before_write_through_visibility(
            &mut self,
            source: &Path,
            _target: &Path,
        ) -> io::Result<()> {
            if !*self.attempted.lock().unwrap() {
                fs::hard_link(source.join(self.relative), &self.alias)?;
                *self.attempted.lock().unwrap() = true;
            }
            Ok(())
        }
    }

    fn transaction_id(value: &str) -> TransactionId {
        TransactionId::parse(value).unwrap()
    }

    fn write_fixture(root: &Path) {
        fs::create_dir_all(root.join("n5-fixture")).unwrap();
        fs::write(root.join(FIXTURE_STATE_PATH), b"counter=0\n").unwrap();
        fs::write(root.join(FIXTURE_HISTORY_PATH), b"").unwrap();
    }

    fn activate(root: &Path) -> BaselineMigrationResult {
        let lease = ExclusiveBaselineLease::acquire_disposable(root).unwrap();
        create_legacy_baseline(&lease).unwrap()
    }

    fn begin_new(root: &Path, id: &str, intent: &[u8]) -> OperationTransaction {
        match OperationTransaction::begin(root, transaction_id(id), OperationKind::Ingest, intent)
            .unwrap()
        {
            TransactionAdmission::New(transaction) => transaction,
            TransactionAdmission::AlreadyCommitted(_) => {
                panic!("expected a new transaction")
            }
        }
    }

    fn stage_transition(transaction: &mut OperationTransaction, counter: u64, history: &[&str]) {
        let state = format!("counter={counter}\n");
        let history = if history.is_empty() {
            String::new()
        } else {
            format!("{}\n", history.join("\n"))
        };
        transaction
            .stage_component(Path::new(FIXTURE_STATE_PATH), state.as_bytes())
            .unwrap();
        transaction
            .stage_component(Path::new(FIXTURE_HISTORY_PATH), history.as_bytes())
            .unwrap();
    }

    fn apply_live_transition(root: &Path, counter: u64, history: &[&str]) {
        fs::write(
            root.join(FIXTURE_STATE_PATH),
            format!("counter={counter}\n"),
        )
        .unwrap();
        let history = if history.is_empty() {
            String::new()
        } else {
            format!("{}\n", history.join("\n"))
        };
        fs::write(root.join(FIXTURE_HISTORY_PATH), history).unwrap();
    }

    fn copy_baseline_for_test(root: &Path) {
        let source = baseline_dir(root);
        let target = baseline_temp_dir(root);
        fs::create_dir(&target).unwrap();
        fs::create_dir(target.join(COMPONENTS_DIR_NAME)).unwrap();
        fs::create_dir(target.join(COMPONENTS_DIR_NAME).join("n5-fixture")).unwrap();
        fs::copy(
            source.join(BASELINE_MANIFEST_FILE_NAME),
            target.join(BASELINE_MANIFEST_FILE_NAME),
        )
        .unwrap();
        fs::copy(
            source.join(COMPONENTS_DIR_NAME).join(FIXTURE_STATE_PATH),
            target.join(COMPONENTS_DIR_NAME).join(FIXTURE_STATE_PATH),
        )
        .unwrap();
        fs::copy(
            source.join(COMPONENTS_DIR_NAME).join(FIXTURE_HISTORY_PATH),
            target.join(COMPONENTS_DIR_NAME).join(FIXTURE_HISTORY_PATH),
        )
        .unwrap();
    }

    fn commit_transition(root: &Path, id: &str, counter: u64, history: &[&str]) -> RecoveryState {
        let mut transaction = begin_new(root, id, b"fixture counter transition");
        stage_transition(&mut transaction, counter, history);
        apply_live_transition(root, counter, history);
        transaction.commit().unwrap()
    }

    #[test]
    fn legacy_recovery_is_read_only_and_transactions_require_typed_activation() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("legacy.marker"), b"legacy").unwrap();
        assert_eq!(
            OperationTransaction::recover(directory.path()).unwrap(),
            RecoveryState::legacy()
        );
        let error = match OperationTransaction::begin(
            directory.path(),
            transaction_id(TX1),
            OperationKind::Ingest,
            b"not activated",
        ) {
            Ok(_) => panic!("unactivated base must not admit a transaction"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            fs::read(directory.path().join("legacy.marker")).unwrap(),
            b"legacy"
        );
    }

    #[test]
    fn registry_excludes_runtime_control_and_rejects_unclassified_or_production_files() {
        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        fs::write(
            directory.path().join(CONTROL_FILE_NAME),
            b"runtime-secret-a",
        )
        .unwrap();
        fs::write(
            directory.path().join(CONTROL_TEMP_FILE_NAME),
            b"runtime-temp",
        )
        .unwrap();
        activate(directory.path());
        fs::write(
            directory.path().join(CONTROL_FILE_NAME),
            b"runtime-secret-b",
        )
        .unwrap();
        assert_eq!(
            OperationTransaction::recover(directory.path())
                .unwrap()
                .generation,
            0
        );

        let unknown = tempdir().unwrap();
        write_fixture(unknown.path());
        fs::write(unknown.path().join("unknown.dat"), b"unknown").unwrap();
        let lease = ExclusiveBaselineLease::acquire_disposable(unknown.path()).unwrap();
        assert!(
            create_legacy_baseline(&lease)
                .unwrap_err()
                .to_string()
                .contains("unclassified")
        );

        let production = tempdir().unwrap();
        write_fixture(production.path());
        fs::create_dir_all(production.path().join("cas")).unwrap();
        fs::write(
            production.path().join("cas/seg_00001.dat"),
            b"not semantically validated",
        )
        .unwrap();
        let lease = ExclusiveBaselineLease::acquire_disposable(production.path()).unwrap();
        assert_eq!(
            create_legacy_baseline(&lease).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[test]
    fn lease_quiescence_and_capacity_preflight_fail_before_activation_writes() {
        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        let lease = ExclusiveBaselineLease::acquire_disposable(directory.path()).unwrap();
        assert_eq!(
            ExclusiveBaselineLease::acquire_disposable(directory.path())
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        let error =
            create_legacy_baseline_with_options(&lease, None, DEFAULT_LIMITS, Some(0)).unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::StorageFull | io::ErrorKind::Other
        ));
        assert!(!transaction_dir(directory.path()).exists());
        drop(lease);

        let writer = BaseLease::acquire(directory.path()).unwrap();
        assert_eq!(
            ExclusiveBaselineLease::acquire_disposable(directory.path())
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(!directory.path().join(ACTIVATION_LOCK_FILE_NAME).exists());
        assert!(!transaction_dir(directory.path()).exists());
        drop(writer);

        let activation = ExclusiveBaselineLease::acquire_disposable(directory.path()).unwrap();
        assert!(matches!(
            BaseLease::acquire(directory.path()),
            Err(BaseLeaseError::Busy { .. })
        ));
        drop(activation);
        assert!(directory.path().join(WRITER_LOCK_FILE_NAME).is_file());
        drop(ExclusiveBaselineLease::acquire_disposable(directory.path()).unwrap());
    }

    #[test]
    fn returned_lease_boundary_errors_remove_only_the_owned_lock() {
        for stage in [
            OperationStage::LeaseBeforeWrite,
            OperationStage::LeaseAfterWrite,
            OperationStage::LeaseAfterFlush,
            OperationStage::LeaseAfterSync,
            OperationStage::LeaseAfterParentSync,
        ] {
            let directory = tempdir().unwrap();
            write_fixture(directory.path());
            let result = ExclusiveBaselineLease::acquire_disposable_with_failpoint(
                directory.path(),
                Some(Box::new(RecordingFailpoint {
                    fail_at: Some((stage, 0)),
                    ..RecordingFailpoint::default()
                })),
            );
            assert!(result.is_err(), "{}", stage.as_str());
            assert!(
                !directory.path().join(ACTIVATION_LOCK_FILE_NAME).exists(),
                "{}",
                stage.as_str()
            );
            drop(ExclusiveBaselineLease::acquire_disposable(directory.path()).unwrap());
        }
    }

    #[test]
    fn component_count_path_aggregate_and_record_bounds_fail_closed() {
        let count = tempdir().unwrap();
        write_fixture(count.path());
        let lease = ExclusiveBaselineLease::acquire_disposable(count.path()).unwrap();
        let mut limits = DEFAULT_LIMITS;
        limits.max_component_count = 1;
        assert!(create_legacy_baseline_with_options(&lease, None, limits, Some(u64::MAX)).is_err());
        assert!(!transaction_dir(count.path()).exists());

        let aggregate = tempdir().unwrap();
        write_fixture(aggregate.path());
        let lease = ExclusiveBaselineLease::acquire_disposable(aggregate.path()).unwrap();
        let mut limits = DEFAULT_LIMITS;
        limits.max_aggregate_bytes = 1;
        assert!(create_legacy_baseline_with_options(&lease, None, limits, Some(u64::MAX)).is_err());

        let path = tempdir().unwrap();
        write_fixture(path.path());
        let lease = ExclusiveBaselineLease::acquire_disposable(path.path()).unwrap();
        let mut limits = DEFAULT_LIMITS;
        limits.max_path_bytes = 4;
        assert!(create_legacy_baseline_with_options(&lease, None, limits, Some(u64::MAX)).is_err());

        let record = tempdir().unwrap();
        fs::write(record.path().join("record"), vec![0u8; 32]).unwrap();
        assert!(read_bytes_bounded(&record.path().join("record"), 16).is_err());
    }

    #[test]
    fn concurrent_mutation_between_preflight_and_copy_fails_closed() {
        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        let lease = ExclusiveBaselineLease::acquire_disposable(directory.path()).unwrap();
        let failpoint = RecordingFailpoint {
            mutation: Some((
                OperationStage::BaselineAfterScan,
                directory.path().join(FIXTURE_STATE_PATH),
                b"counter=9\n".to_vec(),
            )),
            ..RecordingFailpoint::default()
        };
        assert!(create_legacy_baseline_with_failpoint(&lease, Some(Box::new(failpoint))).is_err());
        assert!(!baseline_dir(directory.path()).exists());
    }

    #[test]
    fn symlink_or_reparse_component_is_rejected_when_platform_can_create_it() {
        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        let outside = tempdir().unwrap();
        let outside_file = outside.path().join("outside-state");
        fs::write(&outside_file, b"counter=0\n").unwrap();
        fs::remove_file(directory.path().join(FIXTURE_STATE_PATH)).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside_file, directory.path().join(FIXTURE_STATE_PATH))
            .unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(
            &outside_file,
            directory.path().join(FIXTURE_STATE_PATH),
        )
        .is_err()
        {
            return;
        }
        let lease = ExclusiveBaselineLease::acquire_disposable(directory.path()).unwrap();
        assert!(create_legacy_baseline(&lease).is_err());
        assert_eq!(fs::read(outside_file).unwrap(), b"counter=0\n");
    }

    #[test]
    fn substituted_ancestor_and_immutable_control_record_fail_closed() {
        let ancestor = tempdir().unwrap();
        write_fixture(ancestor.path());
        let outside = tempdir().unwrap();
        fs::write(outside.path().join("state.v1"), b"counter=0\n").unwrap();
        fs::write(outside.path().join("history.v1"), b"").unwrap();
        fs::remove_dir_all(ancestor.path().join("n5-fixture")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), ancestor.path().join("n5-fixture")).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_dir(outside.path(), ancestor.path().join("n5-fixture"))
            .is_err()
        {
            return;
        }
        let lease = ExclusiveBaselineLease::acquire_disposable(ancestor.path()).unwrap();
        assert!(create_legacy_baseline(&lease).is_err());
        drop(lease);

        let control = tempdir().unwrap();
        write_fixture(control.path());
        activate(control.path());
        let format_path = transaction_dir(control.path()).join(FORMAT_FILE_NAME);
        let outside_format = outside.path().join("format.v1");
        fs::write(&outside_format, fs::read(&format_path).unwrap()).unwrap();
        fs::remove_file(&format_path).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside_format, &format_path).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(&outside_format, &format_path).is_err() {
            return;
        }
        assert!(OperationTransaction::recover(control.path()).is_err());
    }

    #[test]
    fn activation_report_reopen_and_rollback_boundary_are_explicit() {
        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        let result = activate(directory.path());
        assert_eq!(result.state.generation, 0);
        assert_eq!(result.component_count, 2);
        assert_eq!(result.total_bytes, 10);
        assert!(result.available_copy_bytes >= result.total_bytes);
        assert_eq!(result.rollback_policy, ROLLBACK_POLICY_ID);
        let first = OperationTransaction::recover(directory.path()).unwrap();
        let second = OperationTransaction::recover(directory.path()).unwrap();
        assert_eq!(first, second);
        let report = read_record::<MigrationRecord>(
            &transaction_dir(directory.path()).join(MIGRATION_FILE_NAME),
            "migration",
        )
        .unwrap();
        assert_eq!(report.body.source_layout, SOURCE_LAYOUT_ID);
        assert!(report.body.source_files_untouched);
    }

    #[test]
    fn interrupted_migration_report_is_resume_stable_and_count_checked() {
        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        let lease = ExclusiveBaselineLease::acquire_disposable(directory.path()).unwrap();
        let failure = create_legacy_baseline_with_options(
            &lease,
            Some(Box::new(RecordingFailpoint {
                fail_at: Some((OperationStage::BaselineBeforeStagingCreate, 0)),
                ..RecordingFailpoint::default()
            })),
            DEFAULT_LIMITS,
            Some(4 * 1024 * 1024),
        );
        assert!(failure.is_err());
        let preflight_path = transaction_dir(directory.path()).join(MIGRATION_TEMP_FILE_NAME);
        let original = fs::read(&preflight_path).unwrap();
        drop(lease);

        let lease = ExclusiveBaselineLease::acquire_disposable(directory.path()).unwrap();
        let low_space =
            create_legacy_baseline_with_options(&lease, None, DEFAULT_LIMITS, Some(0)).unwrap_err();
        assert_eq!(low_space.kind(), io::ErrorKind::StorageFull);
        assert_eq!(fs::read(&preflight_path).unwrap(), original);
        let result = create_legacy_baseline_with_options(
            &lease,
            None,
            DEFAULT_LIMITS,
            Some(8 * 1024 * 1024),
        )
        .unwrap();
        assert_eq!(result.status, BaselineMigrationStatus::Created);
        let published = transaction_dir(directory.path()).join(MIGRATION_FILE_NAME);
        assert_eq!(fs::read(&published).unwrap(), original);
        let first = OperationTransaction::recover(directory.path()).unwrap();
        let second = OperationTransaction::recover(directory.path()).unwrap();
        assert_eq!(first, second);

        for component_count in [3, u64::MAX] {
            let mut report = read_record::<MigrationRecord>(&published, "migration").unwrap();
            report.body.component_count = component_count;
            report.crc32 = record_crc("migration", &report.body).unwrap();
            let malformed = encode_record(&report).unwrap();
            fs::write(&published, &malformed).unwrap();
            let first_error = OperationTransaction::recover(directory.path())
                .unwrap_err()
                .to_string();
            let second_error = OperationTransaction::recover(directory.path())
                .unwrap_err()
                .to_string();
            assert_eq!(first_error, second_error);
            assert_eq!(fs::read(&published).unwrap(), malformed);
            fs::write(&published, &original).unwrap();
        }
    }

    #[test]
    fn generation_limit_is_checked_before_any_staging_artifact() {
        let mut reduced = DEFAULT_LIMITS;
        reduced.max_generations = 2;
        assert_eq!(admitted_next_generation(1, &reduced).unwrap(), 2);
        assert!(admitted_next_generation(2, &reduced).is_err());
        assert!(admitted_next_generation(3, &reduced).is_err());

        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        activate(directory.path());
        let before = read_directory_bounded(&generations_dir(directory.path()), &DEFAULT_LIMITS)
            .unwrap()
            .len();
        reduced.max_generations = 0;
        assert!(
            OperationTransaction::begin_with_options(
                directory.path(),
                transaction_id(TX1),
                OperationKind::Ingest,
                b"over reduced generation limit",
                None,
                reduced,
            )
            .is_err()
        );
        let after = read_directory_bounded(&generations_dir(directory.path()), &DEFAULT_LIMITS)
            .unwrap()
            .len();
        assert_eq!(before, after);
    }

    #[test]
    fn current_writer_gate_and_future_format_fail_closed_without_historical_claims() {
        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        require_legacy_writer_compatible(directory.path()).unwrap();
        activate(directory.path());
        let downgrade = require_legacy_writer_compatible(directory.path()).unwrap_err();
        assert_eq!(downgrade.kind(), io::ErrorKind::Unsupported);
        assert!(downgrade.to_string().contains(DOWNGRADE_GUARD_ID));

        let format_path = transaction_dir(directory.path()).join(FORMAT_FILE_NAME);
        let mut future = read_record::<FormatRecord>(&format_path, "format").unwrap();
        future.version = FORMAT_VERSION + 1;
        future.crc32 = format_crc(&future).unwrap();
        fs::write(format_path, encode_record(&future).unwrap()).unwrap();
        let error = OperationTransaction::recover(directory.path()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported operation transaction format")
        );
    }

    #[test]
    fn transaction_id_retry_is_idempotent_and_conflicting_reuse_fails() {
        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        activate(directory.path());
        let committed = commit_transition(directory.path(), TX1, 1, &[TX1]);
        assert_eq!(committed.generation, 1);
        assert_eq!(
            OperationTransaction::recover(directory.path()).unwrap(),
            committed
        );
        assert_eq!(
            OperationTransaction::recover(directory.path()).unwrap(),
            committed
        );
        match OperationTransaction::begin(
            directory.path(),
            transaction_id(TX1),
            OperationKind::Ingest,
            b"fixture counter transition",
        )
        .unwrap()
        {
            TransactionAdmission::AlreadyCommitted(state) => assert_eq!(state, committed),
            TransactionAdmission::New(_) => panic!("same transaction ID must be idempotent"),
        }
        let latest = commit_transition(directory.path(), TX2, 2, &[TX1, TX2]);
        assert_eq!(latest.generation, 2);
        match OperationTransaction::begin(
            directory.path(),
            transaction_id(TX1),
            OperationKind::Ingest,
            b"fixture counter transition",
        )
        .unwrap()
        {
            TransactionAdmission::AlreadyCommitted(state) => assert_eq!(state, committed),
            TransactionAdmission::New(_) => panic!("retry must return its own committed identity"),
        }
        let conflict = match OperationTransaction::begin(
            directory.path(),
            transaction_id(TX1),
            OperationKind::DeleteAtom,
            b"conflicting intent",
        ) {
            Ok(_) => panic!("conflicting transaction ID reuse must fail"),
            Err(error) => error,
        };
        assert!(conflict.to_string().contains("conflicting"));
        assert_eq!(
            fs::read_to_string(directory.path().join(FIXTURE_HISTORY_PATH)).unwrap(),
            format!("{TX1}\n{TX2}\n")
        );
    }

    #[test]
    fn prepared_same_id_retry_cleans_only_its_incomplete_generation() {
        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        activate(directory.path());
        let mut first = begin_new(directory.path(), TX1, b"retryable");
        stage_transition(&mut first, 1, &[TX1]);
        drop(first);
        let retried = OperationTransaction::begin(
            directory.path(),
            transaction_id(TX1),
            OperationKind::Ingest,
            b"retryable",
        )
        .unwrap();
        match retried {
            TransactionAdmission::New(transaction) => assert_eq!(transaction.generation(), 1),
            TransactionAdmission::AlreadyCommitted(_) => panic!("prepare-only is not committed"),
        }
    }

    #[test]
    fn typed_adapter_rejects_missing_pair_and_unrelated_paths() {
        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        activate(directory.path());
        let mut transaction = begin_new(directory.path(), TX1, b"missing history");
        transaction
            .stage_component(Path::new(FIXTURE_STATE_PATH), b"counter=1\n")
            .unwrap();
        assert!(transaction.commit().is_err());

        let other = tempdir().unwrap();
        write_fixture(other.path());
        activate(other.path());
        let mut transaction = begin_new(other.path(), TX1, b"unrelated");
        assert!(
            transaction
                .stage_component(Path::new("cas/seg_00001.dat"), b"truncate")
                .is_err()
        );
    }

    #[test]
    fn exact_generation_tree_rejects_extra_file_and_link() {
        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        activate(directory.path());
        let mut transaction = begin_new(directory.path(), TX1, b"exact tree");
        stage_transition(&mut transaction, 1, &[TX1]);
        apply_live_transition(directory.path(), 1, &[TX1]);
        let state = transaction.commit().unwrap();
        let generation = generations_dir(directory.path()).join(generation_name(1));
        fs::write(generation.join(COMPONENTS_DIR_NAME).join("extra"), b"extra").unwrap();
        assert!(OperationTransaction::recover(directory.path()).is_err());
        fs::remove_file(generation.join(COMPONENTS_DIR_NAME).join("extra")).unwrap();
        assert_eq!(
            OperationTransaction::recover(directory.path()).unwrap(),
            state
        );
    }

    #[test]
    fn mx95_n5_004_exact_directories_and_cumulative_tree_bounds_fail_closed() {
        let generation = tempdir().unwrap();
        write_fixture(generation.path());
        activate(generation.path());
        let transaction = begin_new(generation.path(), TX1, b"extra empty directory");
        fs::create_dir(
            transaction
                .generation_dir()
                .join(COMPONENTS_DIR_NAME)
                .join("unknown-empty"),
        )
        .unwrap();
        drop(transaction);
        assert!(OperationTransaction::recover(generation.path()).is_err());

        let baseline = tempdir().unwrap();
        write_fixture(baseline.path());
        activate(baseline.path());
        fs::create_dir(
            baseline_dir(baseline.path())
                .join(COMPONENTS_DIR_NAME)
                .join("unknown-empty"),
        )
        .unwrap();
        assert!(OperationTransaction::recover(baseline.path()).is_err());

        let control = tempdir().unwrap();
        write_fixture(control.path());
        activate(control.path());
        fs::create_dir(transaction_dir(control.path()).join("unknown-empty")).unwrap();
        assert!(OperationTransaction::recover(control.path()).is_err());

        let tree = tempdir().unwrap();
        fs::create_dir(tree.path().join("n5-fixture")).unwrap();
        fs::write(tree.path().join(FIXTURE_STATE_PATH), b"counter=0\n").unwrap();
        fs::write(tree.path().join(FIXTURE_HISTORY_PATH), b"").unwrap();
        let expected = golden_descriptors();
        let mut limits = DEFAULT_LIMITS;
        limits.max_tree_entries = 2;
        assert!(
            collect_exact_component_tree(tree.path(), tree.path(), &expected, &limits).is_err()
        );
        let mut limits = DEFAULT_LIMITS;
        limits.max_tree_depth = 1;
        assert!(
            collect_exact_component_tree(tree.path(), tree.path(), &expected, &limits).is_err()
        );
        let mut limits = DEFAULT_LIMITS;
        limits.max_tree_path_bytes = 8;
        assert!(
            collect_exact_component_tree(tree.path(), tree.path(), &expected, &limits).is_err()
        );
    }

    #[test]
    fn mx95_n5_005_nested_creation_and_sync_failpoints_are_cross_sequence_visible() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        let lease = ExclusiveBaselineLease::acquire_disposable(directory.path()).unwrap();
        create_legacy_baseline_with_failpoint(
            &lease,
            Some(Box::new(RecordingFailpoint {
                events: events.clone(),
                ..RecordingFailpoint::default()
            })),
        )
        .unwrap();
        drop(lease);
        let mut transaction = match OperationTransaction::begin_with_failpoint(
            directory.path(),
            transaction_id(TX1),
            OperationKind::Ingest,
            b"nested sync sequence",
            Some(Box::new(RecordingFailpoint {
                events: events.clone(),
                ..RecordingFailpoint::default()
            })),
        )
        .unwrap()
        {
            TransactionAdmission::New(transaction) => transaction,
            TransactionAdmission::AlreadyCommitted(_) => unreachable!(),
        };
        stage_transition(&mut transaction, 1, &[TX1]);
        let observed = events.lock().unwrap().clone();
        for stage in [
            OperationStage::BaselineBeforeControlCreate,
            OperationStage::BaselineAfterControlSync,
            OperationStage::BaselineAfterRootSync,
            OperationStage::BaselineBeforeComponentParentCreate,
            OperationStage::BaselineAfterComponentParentSync,
            OperationStage::BaselineAfterComponentsParentSync,
            OperationStage::BeforeGenerationComponentsCreate,
            OperationStage::AfterGenerationComponentsSync,
            OperationStage::AfterGenerationComponentsParentSync,
            OperationStage::BeforeComponentParentCreate,
            OperationStage::AfterComponentParentSync,
            OperationStage::AfterComponentComponentsSync,
        ] {
            assert!(
                observed.iter().any(|(found, _)| *found == stage),
                "{}",
                stage.as_str()
            );
        }
    }

    #[test]
    fn mx95_n5_006_publication_parent_and_object_substitution_fail_closed() {
        for kind in [
            NamespaceSubstitution::PublicationParent,
            NamespaceSubstitution::PublicationObject,
        ] {
            let directory = tempdir().unwrap();
            write_fixture(directory.path());
            activate(directory.path());
            let substituted = Arc::new(Mutex::new(false));
            let mut transaction = match OperationTransaction::begin_with_failpoint(
                directory.path(),
                transaction_id(TX1),
                OperationKind::Ingest,
                b"namespace substitution",
                None,
            )
            .unwrap()
            {
                TransactionAdmission::New(transaction) => transaction,
                TransactionAdmission::AlreadyCommitted(_) => unreachable!(),
            };
            stage_transition(&mut transaction, 1, &[TX1]);
            apply_live_transition(directory.path(), 1, &[TX1]);
            let pending = transaction.generation_dir();
            transaction.failpoint = Some(Box::new(NamespaceSubstitutionFailpoint {
                root: directory.path().to_path_buf(),
                pending: pending.clone(),
                kind,
                substituted: substituted.clone(),
            }));
            let result = transaction.commit();
            let did_substitute = *substituted.lock().unwrap();
            assert!(!did_substitute);
            let committed = result.unwrap();
            assert!(
                generations_dir(directory.path())
                    .join(generation_name(1))
                    .is_dir()
            );
            assert!(!pending.exists());
            let first = OperationTransaction::recover(directory.path()).unwrap();
            let second = OperationTransaction::recover(directory.path()).unwrap();
            assert_eq!(first, second);
            assert_eq!(first, committed);
            assert_eq!(first.generation, 1);
            match OperationTransaction::begin(
                directory.path(),
                transaction_id(TX1),
                OperationKind::Ingest,
                b"namespace substitution",
            )
            .unwrap()
            {
                TransactionAdmission::AlreadyCommitted(state) => assert_eq!(state, first),
                TransactionAdmission::New(_) => {
                    panic!("resolved returned error must be history-once")
                }
            }
            assert_eq!(
                fs::read_to_string(directory.path().join(FIXTURE_HISTORY_PATH)).unwrap(),
                format!("{TX1}\n")
            );
        }
    }

    #[test]
    fn mx95_n5_006_final_file_and_directory_seams_resolve_exact_post_state() {
        for selected in [
            NamespaceMutationKind::FileRename,
            NamespaceMutationKind::DirectoryRename,
        ] {
            let directory = tempdir().unwrap();
            let outside = tempdir().unwrap();
            write_fixture(directory.path());
            activate(directory.path());
            fs::write(outside.path().join("outside.sentinel"), b"outside").unwrap();
            let attempted = Arc::new(Mutex::new(false));
            let substituted = Arc::new(Mutex::new(false));
            let mut transaction = begin_new(directory.path(), TX1, b"final mutation seam");
            stage_transition(&mut transaction, 1, &[TX1]);
            apply_live_transition(directory.path(), 1, &[TX1]);
            transaction.failpoint = Some(Box::new(FinalNamespaceSubstitutionSeam {
                selected,
                outside: outside.path().to_path_buf(),
                attempted: attempted.clone(),
                substituted: substituted.clone(),
                fail_after_attempt: true,
                public_failure: None,
            }));
            let error = transaction.commit().unwrap_err();
            assert!(
                error.to_string().contains("exact committed post-state"),
                "{error}"
            );
            assert!(*attempted.lock().unwrap());
            assert!(!*substituted.lock().unwrap());
            assert_eq!(
                fs::read(outside.path().join("outside.sentinel")).unwrap(),
                b"outside"
            );
            let first = OperationTransaction::recover(directory.path()).unwrap();
            let second = OperationTransaction::recover(directory.path()).unwrap();
            assert_eq!(first, second);
            assert_eq!(first.generation, 1);
            match OperationTransaction::begin(
                directory.path(),
                transaction_id(TX1),
                OperationKind::Ingest,
                b"final mutation seam",
            )
            .unwrap()
            {
                TransactionAdmission::AlreadyCommitted(state) => assert_eq!(state, first),
                TransactionAdmission::New(_) => panic!("same-ID retry duplicated history"),
            }
            assert_eq!(
                fs::read_to_string(directory.path().join(FIXTURE_HISTORY_PATH)).unwrap(),
                format!("{TX1}\n")
            );
        }
    }

    #[test]
    fn mx95_n5_006_final_hard_link_seam_preserves_exact_pre_state() {
        let directory = tempdir().unwrap();
        let outside = tempdir().unwrap();
        write_fixture(directory.path());
        activate(directory.path());
        let alias = outside.path().join("commit-alias");
        let linked = Arc::new(Mutex::new(false));
        let mut transaction = begin_new(directory.path(), TX1, b"final hard-link seam");
        stage_transition(&mut transaction, 1, &[TX1]);
        transaction.failpoint = Some(Box::new(FinalHardLinkSeam {
            source_kind: NamespaceMutationKind::FileRename,
            alias: alias.clone(),
            linked: linked.clone(),
        }));
        assert!(transaction.commit().is_err());
        assert!(*linked.lock().unwrap());
        let alias_bytes = fs::read(&alias).unwrap();
        assert!(!alias_bytes.is_empty());
        assert!(
            !generations_dir(directory.path())
                .join(generation_name(1))
                .exists()
        );
        let first = OperationTransaction::recover(directory.path()).unwrap();
        let second = OperationTransaction::recover(directory.path()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.generation, 0);
        let retry = begin_new(directory.path(), TX1, b"final hard-link seam");
        assert_eq!(retry.generation(), 1);
        drop(retry);
        assert_eq!(fs::read(alias).unwrap(), alias_bytes);
    }

    #[test]
    fn mx95_n5_006_activation_pending_and_baseline_cleanup_seams_are_retry_stable() {
        let activation = tempdir().unwrap();
        let activation_outside = tempdir().unwrap();
        write_fixture(activation.path());
        fs::write(
            activation_outside.path().join("outside.sentinel"),
            b"outside",
        )
        .unwrap();
        let attempted = Arc::new(Mutex::new(false));
        let substituted = Arc::new(Mutex::new(false));
        let result = ExclusiveBaselineLease::acquire_disposable_with_failpoint(
            activation.path(),
            Some(Box::new(FinalNamespaceSubstitutionSeam {
                selected: NamespaceMutationKind::FileRemove,
                outside: activation_outside.path().to_path_buf(),
                attempted: attempted.clone(),
                substituted: substituted.clone(),
                fail_after_attempt: true,
                public_failure: Some(OperationStage::LeaseAfterWrite),
            })),
        );
        assert!(result.is_err());
        assert!(*attempted.lock().unwrap());
        assert!(!*substituted.lock().unwrap());
        assert!(!activation.path().join(ACTIVATION_LOCK_FILE_NAME).exists());
        drop(ExclusiveBaselineLease::acquire_disposable(activation.path()).unwrap());
        assert_eq!(
            fs::read(activation_outside.path().join("outside.sentinel")).unwrap(),
            b"outside"
        );

        let pending = tempdir().unwrap();
        let pending_outside = tempdir().unwrap();
        write_fixture(pending.path());
        activate(pending.path());
        let mut first = begin_new(pending.path(), TX1, b"pending cleanup seam");
        stage_transition(&mut first, 1, &[TX1]);
        drop(first);
        let attempted = Arc::new(Mutex::new(false));
        let substituted = Arc::new(Mutex::new(false));
        let retry_error = OperationTransaction::begin_with_failpoint(
            pending.path(),
            transaction_id(TX1),
            OperationKind::Ingest,
            b"pending cleanup seam",
            Some(Box::new(FinalNamespaceSubstitutionSeam {
                selected: NamespaceMutationKind::DirectoryRemove,
                outside: pending_outside.path().to_path_buf(),
                attempted: attempted.clone(),
                substituted: substituted.clone(),
                fail_after_attempt: true,
                public_failure: None,
            })),
        );
        assert!(retry_error.is_err());
        assert!(*attempted.lock().unwrap());
        assert!(!*substituted.lock().unwrap());
        let first_reopen = OperationTransaction::recover(pending.path()).unwrap();
        let second_reopen = OperationTransaction::recover(pending.path()).unwrap();
        assert_eq!(first_reopen, second_reopen);
        assert_eq!(first_reopen.generation, 0);
        drop(begin_new(pending.path(), TX1, b"pending cleanup seam"));

        let baseline = tempdir().unwrap();
        let baseline_outside = tempdir().unwrap();
        write_fixture(baseline.path());
        let lease = ExclusiveBaselineLease::acquire_disposable(baseline.path()).unwrap();
        assert!(
            create_legacy_baseline_with_failpoint(
                &lease,
                Some(Box::new(RecordingFailpoint {
                    fail_at: Some((OperationStage::BaselineBeforePublish, 0)),
                    ..RecordingFailpoint::default()
                })),
            )
            .is_err()
        );
        drop(lease);
        let attempted = Arc::new(Mutex::new(false));
        let substituted = Arc::new(Mutex::new(false));
        let lease = ExclusiveBaselineLease::acquire_disposable(baseline.path()).unwrap();
        assert!(
            create_legacy_baseline_with_failpoint(
                &lease,
                Some(Box::new(FinalNamespaceSubstitutionSeam {
                    selected: NamespaceMutationKind::DirectoryRemove,
                    outside: baseline_outside.path().to_path_buf(),
                    attempted: attempted.clone(),
                    substituted: substituted.clone(),
                    fail_after_attempt: true,
                    public_failure: None,
                })),
            )
            .is_err()
        );
        drop(lease);
        assert!(*attempted.lock().unwrap());
        assert!(!*substituted.lock().unwrap());
        let first_reopen = OperationTransaction::recover(baseline.path()).unwrap();
        let second_reopen = OperationTransaction::recover(baseline.path()).unwrap();
        assert_eq!(first_reopen, second_reopen);
        assert!(first_reopen.legacy);
        let lease = ExclusiveBaselineLease::acquire_disposable(baseline.path()).unwrap();
        create_legacy_baseline(&lease).unwrap();
        drop(lease);
        let first_reopen = OperationTransaction::recover(baseline.path()).unwrap();
        let second_reopen = OperationTransaction::recover(baseline.path()).unwrap();
        assert_eq!(first_reopen, second_reopen);
    }

    #[test]
    fn mx10_n5a_013_generation_tombstone_resumes_after_each_child_removal() {
        for suffix in [
            "history.v1",
            "state.v1",
            "n5-fixture",
            COMPONENTS_DIR_NAME,
            PREPARE_FILE_NAME,
        ] {
            let directory = tempdir().unwrap();
            write_fixture(directory.path());
            activate(directory.path());
            let mut transaction = begin_new(directory.path(), TX1, b"child removal resume");
            stage_transition(&mut transaction, 1, &[TX1]);
            drop(transaction);
            let attempted = Arc::new(Mutex::new(false));
            let error = OperationTransaction::begin_with_failpoint(
                directory.path(),
                transaction_id(TX1),
                OperationKind::Ingest,
                b"child removal resume",
                Some(Box::new(AfterRemovalFailpoint {
                    suffix: suffix.to_owned(),
                    attempted: attempted.clone(),
                })),
            )
            .err()
            .expect("selected post-removal seam must fail");
            assert!(error.to_string().contains("after exact tombstone"));
            assert!(*attempted.lock().unwrap(), "{suffix}");
            let first = OperationTransaction::recover(directory.path()).unwrap();
            let second = OperationTransaction::recover(directory.path()).unwrap();
            assert_eq!(first, second, "{suffix}");
            assert_eq!(first.generation, 0, "{suffix}");
            let retry = begin_new(directory.path(), TX1, b"child removal resume");
            assert_eq!(retry.generation(), 1, "{suffix}");
            drop(retry);
        }
    }

    #[test]
    fn mx10_n5a_013_baseline_tombstone_resumes_after_each_child_removal() {
        for suffix in [
            "history.v1",
            "state.v1",
            "n5-fixture",
            COMPONENTS_DIR_NAME,
            BASELINE_MANIFEST_FILE_NAME,
        ] {
            let directory = tempdir().unwrap();
            write_fixture(directory.path());
            let lease = ExclusiveBaselineLease::acquire_disposable(directory.path()).unwrap();
            assert!(
                create_legacy_baseline_with_failpoint(
                    &lease,
                    Some(Box::new(RecordingFailpoint {
                        fail_at: Some((OperationStage::BaselineBeforePublish, 0)),
                        ..RecordingFailpoint::default()
                    })),
                )
                .is_err()
            );
            drop(lease);
            let attempted = Arc::new(Mutex::new(false));
            let lease = ExclusiveBaselineLease::acquire_disposable(directory.path()).unwrap();
            let error = create_legacy_baseline_with_failpoint(
                &lease,
                Some(Box::new(AfterRemovalFailpoint {
                    suffix: suffix.to_owned(),
                    attempted: attempted.clone(),
                })),
            )
            .unwrap_err();
            drop(lease);
            assert!(error.to_string().contains("after exact tombstone"));
            assert!(*attempted.lock().unwrap(), "{suffix}");
            let first = OperationTransaction::recover(directory.path()).unwrap();
            let second = OperationTransaction::recover(directory.path()).unwrap();
            assert_eq!(first, second, "{suffix}");
            assert!(first.legacy, "{suffix}");
            activate(directory.path());
            let first = OperationTransaction::recover(directory.path()).unwrap();
            let second = OperationTransaction::recover(directory.path()).unwrap();
            assert_eq!(first, second, "{suffix}");
            assert!(!first.legacy, "{suffix}");
        }
    }

    #[test]
    fn mx10_n5a_014_late_child_link_and_replacement_obey_exact_oracle() {
        let linked = tempdir().unwrap();
        let linked_outside = tempdir().unwrap();
        write_fixture(linked.path());
        activate(linked.path());
        let attempted = Arc::new(Mutex::new(false));
        let substituted = Arc::new(Mutex::new(false));
        let mut transaction = begin_new(linked.path(), TX1, b"late child hard link");
        stage_transition(&mut transaction, 1, &[TX1]);
        apply_live_transition(linked.path(), 1, &[TX1]);
        let alias = linked_outside.path().join("late-child-original");
        transaction.failpoint = Some(Box::new(LateChildMutationSeam {
            relative: COMMIT_FILE_NAME,
            outside: linked_outside.path().to_path_buf(),
            hard_link: true,
            fail_after_attempt: false,
            attempted: attempted.clone(),
            substituted: substituted.clone(),
        }));
        let error = transaction.commit().unwrap_err();
        assert!(error.to_string().contains("exact pre-state"), "{error}");
        assert!(*attempted.lock().unwrap());
        assert!(*substituted.lock().unwrap());
        let alias_bytes = fs::read(&alias).unwrap();
        assert!(!alias_bytes.is_empty());
        assert!(
            !path_entry_exists(&generations_dir(linked.path()).join(generation_name(1))).unwrap()
        );
        let first = OperationTransaction::recover(linked.path()).unwrap();
        let second = OperationTransaction::recover(linked.path()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.generation, 0);
        assert_eq!(
            fs::read(linked.path().join(FIXTURE_STATE_PATH)).unwrap(),
            b"counter=0\n"
        );
        assert_eq!(
            fs::read(linked.path().join(FIXTURE_HISTORY_PATH)).unwrap(),
            b""
        );
        assert_eq!(fs::read(&alias).unwrap(), alias_bytes);
        drop(begin_new(linked.path(), TX1, b"late child hard link"));

        let replaced = tempdir().unwrap();
        let replaced_outside = tempdir().unwrap();
        write_fixture(replaced.path());
        activate(replaced.path());
        let attempted = Arc::new(Mutex::new(false));
        let substituted = Arc::new(Mutex::new(false));
        let mut transaction = begin_new(replaced.path(), TX1, b"late child replacement");
        stage_transition(&mut transaction, 1, &[TX1]);
        apply_live_transition(replaced.path(), 1, &[TX1]);
        transaction.failpoint = Some(Box::new(LateChildMutationSeam {
            relative: COMMIT_FILE_NAME,
            outside: replaced_outside.path().to_path_buf(),
            hard_link: false,
            fail_after_attempt: true,
            attempted: attempted.clone(),
            substituted: substituted.clone(),
        }));
        let error = transaction.commit().unwrap_err();
        assert!(error.to_string().contains("exact pre-state"), "{error}");
        assert!(*attempted.lock().unwrap());
        assert!(*substituted.lock().unwrap());
        let displaced = replaced_outside.path().join("late-child-original");
        let displaced_bytes = fs::read(&displaced).unwrap();
        assert!(!displaced_bytes.is_empty());
        assert!(
            !path_entry_exists(&generations_dir(replaced.path()).join(generation_name(1))).unwrap()
        );
        let first = OperationTransaction::recover(replaced.path()).unwrap();
        let second = OperationTransaction::recover(replaced.path()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.generation, 0);
        assert_eq!(fs::read(&displaced).unwrap(), displaced_bytes);
        assert_eq!(
            fs::read(replaced.path().join(FIXTURE_STATE_PATH)).unwrap(),
            b"counter=0\n"
        );
        assert_eq!(
            fs::read(replaced.path().join(FIXTURE_HISTORY_PATH)).unwrap(),
            b""
        );
        drop(begin_new(replaced.path(), TX1, b"late child replacement"));
    }

    #[test]
    fn mx10_n5a_014_visibility_seam_child_link_is_revalidated_before_publish() {
        let directory = tempdir().unwrap();
        let outside = tempdir().unwrap();
        write_fixture(directory.path());
        activate(directory.path());
        let attempted = Arc::new(Mutex::new(false));
        let alias = outside.path().join("visibility-seam-commit");
        let mut transaction = begin_new(directory.path(), TX1, b"visibility child link");
        stage_transition(&mut transaction, 1, &[TX1]);
        apply_live_transition(directory.path(), 1, &[TX1]);
        transaction.failpoint = Some(Box::new(BeforeVisibilityChildLinkSeam {
            relative: COMMIT_FILE_NAME,
            alias: alias.clone(),
            attempted: attempted.clone(),
        }));

        let error = transaction.commit().unwrap_err();
        assert!(error.to_string().contains("exact pre-state"), "{error}");
        assert!(*attempted.lock().unwrap());
        let alias_bytes = fs::read(&alias).unwrap();
        assert!(!alias_bytes.is_empty());
        assert!(
            !path_entry_exists(&generations_dir(directory.path()).join(generation_name(1)))
                .unwrap()
        );
        let first = OperationTransaction::recover(directory.path()).unwrap();
        let second = OperationTransaction::recover(directory.path()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.generation, 0);
        assert_eq!(
            fs::read(directory.path().join(FIXTURE_STATE_PATH)).unwrap(),
            b"counter=0\n"
        );
        assert_eq!(
            fs::read(directory.path().join(FIXTURE_HISTORY_PATH)).unwrap(),
            b""
        );
        assert_eq!(fs::read(&alias).unwrap(), alias_bytes);

        let mut retried = begin_new(directory.path(), TX1, b"visibility child link");
        stage_transition(&mut retried, 1, &[TX1]);
        apply_live_transition(directory.path(), 1, &[TX1]);
        let committed = retried.commit().unwrap();
        let first = OperationTransaction::recover(directory.path()).unwrap();
        let second = OperationTransaction::recover(directory.path()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first, committed);
        assert_eq!(first.generation, 1);
        match OperationTransaction::begin(
            directory.path(),
            transaction_id(TX1),
            OperationKind::Ingest,
            b"visibility child link",
        )
        .unwrap()
        {
            TransactionAdmission::AlreadyCommitted(state) => assert_eq!(state, committed),
            TransactionAdmission::New(_) => panic!("committed retry must be idempotent"),
        }
        assert_eq!(
            fs::read_to_string(directory.path().join(FIXTURE_HISTORY_PATH)).unwrap(),
            format!("{TX1}\n")
        );
        assert_eq!(fs::read(&alias).unwrap(), alias_bytes);
    }

    #[test]
    fn mx10_n5a_012_before_write_through_failure_resolves_exact_post_state() {
        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        activate(directory.path());
        let attempted = Arc::new(Mutex::new(false));
        let mut transaction = begin_new(directory.path(), TX1, b"write-through seam");
        stage_transition(&mut transaction, 1, &[TX1]);
        apply_live_transition(directory.path(), 1, &[TX1]);
        transaction.failpoint = Some(Box::new(BeforeVisibilityFailure {
            attempted: attempted.clone(),
        }));
        let error = transaction.commit().unwrap_err();
        assert!(
            error.to_string().contains("exact committed post-state"),
            "{error}"
        );
        assert!(*attempted.lock().unwrap());
        let first = OperationTransaction::recover(directory.path()).unwrap();
        let second = OperationTransaction::recover(directory.path()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.generation, 1);
        assert_eq!(
            fs::read_to_string(directory.path().join(FIXTURE_HISTORY_PATH)).unwrap(),
            format!("{TX1}\n")
        );
    }

    #[test]
    fn mx10_n5a_016_post_close_replacement_is_not_accepted_as_absence() {
        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        activate(directory.path());
        let mut transaction = begin_new(directory.path(), TX1, b"post-close replacement");
        stage_transition(&mut transaction, 1, &[TX1]);
        drop(transaction);
        let attempted = Arc::new(Mutex::new(false));
        let error = OperationTransaction::begin_with_failpoint(
            directory.path(),
            transaction_id(TX1),
            OperationKind::Ingest,
            b"post-close replacement",
            Some(Box::new(AfterCloseReplacementSeam {
                selected: PREPARE_FILE_NAME,
                attempted: attempted.clone(),
            })),
        )
        .err()
        .expect("replacement at absence boundary must fail closed");
        assert!(
            error.to_string().contains("remained or was replaced"),
            "{error}"
        );
        assert!(*attempted.lock().unwrap());
        let first = OperationTransaction::recover(directory.path()).unwrap_err();
        let second = OperationTransaction::recover(directory.path()).unwrap_err();
        assert_eq!(first.kind(), second.kind());
        assert_eq!(first.to_string(), second.to_string());
    }

    #[test]
    fn mx95_n5_007_all_temporary_control_records_obey_the_exact_byte_limit() {
        let exact = tempdir().unwrap();
        write_fixture(exact.path());
        activate(exact.path());
        let transaction = begin_new(exact.path(), TX1, b"exact temporary record limit");
        let temporary = transaction.generation_dir().join(COMMIT_TEMP_FILE_NAME);
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .unwrap()
            .set_len(MAX_CONTROL_RECORD_BYTES)
            .unwrap();
        drop(transaction);
        let first = OperationTransaction::recover(exact.path()).unwrap();
        let second = OperationTransaction::recover(exact.path()).unwrap();
        assert_eq!(first, second);
        let retry = begin_new(exact.path(), TX1, b"exact temporary record limit");
        assert!(!temporary.exists());
        drop(retry);

        let oversized = tempdir().unwrap();
        write_fixture(oversized.path());
        activate(oversized.path());
        let transaction = begin_new(oversized.path(), TX1, b"oversized sparse temporary");
        let published_incomplete = generations_dir(oversized.path()).join(generation_name(1));
        fs::rename(transaction.generation_dir(), &published_incomplete).unwrap();
        let temporary = published_incomplete.join(COMMIT_TEMP_FILE_NAME);
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .unwrap()
            .set_len(MAX_CONTROL_RECORD_BYTES + 1)
            .unwrap();
        drop(transaction);
        assert!(OperationTransaction::recover(oversized.path()).is_err());
        assert!(OperationTransaction::recover(oversized.path()).is_err());
        assert!(
            OperationTransaction::begin(
                oversized.path(),
                transaction_id(TX1),
                OperationKind::Ingest,
                b"oversized sparse temporary",
            )
            .is_err()
        );
        fs::remove_file(&temporary).unwrap();
        let retry = begin_new(oversized.path(), TX1, b"oversized sparse temporary");
        drop(retry);
    }

    #[test]
    fn mx95_n5_008_future_tree_preflight_matches_recovery_and_generation_cap() {
        assert_eq!(
            MAX_TREE_ENTRIES,
            MAX_FIXED_TREE_ENTRIES + MAX_GENERATIONS * MAX_GENERATION_TREE_ENTRIES
        );
        const {
            assert!(
                MAX_TREE_PATH_BYTES
                    >= MAX_FIXED_TREE_PATH_BYTES
                        + MAX_GENERATIONS * MAX_GENERATION_TREE_PATH_BYTES
                        + MAX_PENDING_TREE_PATH_BYTES
            );
        }
        let generation = generation_name(MAX_GENERATIONS as u64);
        let prefix = format!("{GENERATIONS_DIR_NAME}/{generation}");
        let exact_generation_paths = [
            prefix.clone(),
            format!("{prefix}/{COMPONENTS_DIR_NAME}"),
            format!("{prefix}/{PREPARE_FILE_NAME}"),
            format!("{prefix}/{COMMIT_FILE_NAME}"),
            format!("{prefix}/{COMPONENTS_DIR_NAME}/n5-fixture"),
            format!("{prefix}/{COMPONENTS_DIR_NAME}/{FIXTURE_STATE_PATH}"),
            format!("{prefix}/{COMPONENTS_DIR_NAME}/{FIXTURE_HISTORY_PATH}"),
        ];
        assert_eq!(exact_generation_paths.len(), MAX_GENERATION_TREE_ENTRIES);
        assert!(
            exact_generation_paths
                .iter()
                .map(String::len)
                .sum::<usize>()
                <= MAX_GENERATION_TREE_PATH_BYTES
        );
        assert_eq!(
            admitted_next_generation((MAX_GENERATIONS - 1) as u64, &DEFAULT_LIMITS).unwrap(),
            MAX_GENERATIONS as u64
        );
        assert!(admitted_next_generation(MAX_GENERATIONS as u64, &DEFAULT_LIMITS).is_err());

        let exact = tempdir().unwrap();
        write_fixture(exact.path());
        activate(exact.path());
        let mut transaction = begin_new(exact.path(), TX1, b"exact future tree budget");
        stage_transition(&mut transaction, 1, &[TX1]);
        let budget = future_generation_tree_budget(
            exact.path(),
            &transaction.generation_dir(),
            &transaction.published_generation_dir(),
            &DEFAULT_LIMITS,
        )
        .unwrap();
        let commit_path_bytes = format!(
            "{GENERATIONS_DIR_NAME}/{}/{COMMIT_FILE_NAME}",
            generation_name(1)
        )
        .len();
        transaction.limits.max_tree_entries = budget.entries + 1;
        transaction.limits.max_tree_path_bytes = budget.path_bytes + commit_path_bytes;
        apply_live_transition(exact.path(), 1, &[TX1]);
        let committed = transaction.commit().unwrap();
        assert_eq!(
            OperationTransaction::recover(exact.path()).unwrap(),
            committed
        );

        let below = tempdir().unwrap();
        write_fixture(below.path());
        activate(below.path());
        let mut transaction = begin_new(below.path(), TX1, b"below future tree budget");
        stage_transition(&mut transaction, 1, &[TX1]);
        let budget = future_generation_tree_budget(
            below.path(),
            &transaction.generation_dir(),
            &transaction.published_generation_dir(),
            &DEFAULT_LIMITS,
        )
        .unwrap();
        transaction.limits.max_tree_path_bytes = budget.path_bytes + commit_path_bytes - 1;
        apply_live_transition(below.path(), 1, &[TX1]);
        assert!(transaction.commit().is_err());
        assert!(
            !generations_dir(below.path())
                .join(generation_name(1))
                .exists()
        );
    }

    #[test]
    fn mx95_n5_009_coexisting_activation_publications_are_stably_refused() {
        for baseline_pair in [true, false] {
            let directory = tempdir().unwrap();
            write_fixture(directory.path());
            activate(directory.path());
            fs::remove_file(transaction_dir(directory.path()).join(FORMAT_FILE_NAME)).unwrap();
            let (temporary, published) = if baseline_pair {
                copy_baseline_for_test(directory.path());
                (
                    baseline_temp_dir(directory.path()),
                    baseline_dir(directory.path()),
                )
            } else {
                let published = transaction_dir(directory.path()).join(MIGRATION_FILE_NAME);
                let temporary = transaction_dir(directory.path()).join(MIGRATION_TEMP_FILE_NAME);
                fs::copy(&published, &temporary).unwrap();
                (temporary, published)
            };
            let temporary_before = if temporary.is_file() {
                fs::read(&temporary).unwrap()
            } else {
                fs::read(temporary.join(BASELINE_MANIFEST_FILE_NAME)).unwrap()
            };
            let published_before = if published.is_file() {
                fs::read(&published).unwrap()
            } else {
                fs::read(published.join(BASELINE_MANIFEST_FILE_NAME)).unwrap()
            };
            let lease = ExclusiveBaselineLease::acquire_disposable(directory.path()).unwrap();
            assert!(create_legacy_baseline(&lease).is_err());
            assert!(create_legacy_baseline(&lease).is_err());
            assert!(
                !transaction_dir(directory.path())
                    .join(FORMAT_FILE_NAME)
                    .exists()
            );
            assert!(OperationTransaction::recover(directory.path()).is_err());
            assert!(OperationTransaction::recover(directory.path()).is_err());
            let temporary_after = if temporary.is_file() {
                fs::read(&temporary).unwrap()
            } else {
                fs::read(temporary.join(BASELINE_MANIFEST_FILE_NAME)).unwrap()
            };
            let published_after = if published.is_file() {
                fs::read(&published).unwrap()
            } else {
                fs::read(published.join(BASELINE_MANIFEST_FILE_NAME)).unwrap()
            };
            assert_eq!(temporary_before, temporary_after);
            assert_eq!(published_before, published_after);
        }
    }

    #[test]
    fn mx95_n5_010_hard_linked_immutable_control_records_fail_closed() {
        for record_index in 0..5 {
            let directory = tempdir().unwrap();
            let external = tempdir().unwrap();
            write_fixture(directory.path());
            activate(directory.path());
            if record_index >= 3 {
                commit_transition(directory.path(), TX1, 1, &[TX1]);
            }
            let target = match record_index {
                0 => transaction_dir(directory.path()).join(FORMAT_FILE_NAME),
                1 => transaction_dir(directory.path()).join(MIGRATION_FILE_NAME),
                2 => baseline_dir(directory.path()).join(BASELINE_MANIFEST_FILE_NAME),
                3 => generations_dir(directory.path())
                    .join(generation_name(1))
                    .join(PREPARE_FILE_NAME),
                4 => generations_dir(directory.path())
                    .join(generation_name(1))
                    .join(COMMIT_FILE_NAME),
                _ => unreachable!(),
            };
            let alias = external.path().join("immutable-control.external-link");
            fs::hard_link(&target, &alias).unwrap();
            assert!(OperationTransaction::recover(directory.path()).is_err());
            fs::write(&alias, b"mutated through external hard link").unwrap();
            assert!(OperationTransaction::recover(directory.path()).is_err());
        }

        let directory = tempdir().unwrap();
        let external = tempdir().unwrap();
        write_fixture(directory.path());
        activate(directory.path());
        let mut transaction = begin_new(directory.path(), TX1, b"prepublication hard link");
        stage_transition(&mut transaction, 1, &[TX1]);
        apply_live_transition(directory.path(), 1, &[TX1]);
        let pending_commit = transaction.generation_dir().join(COMMIT_FILE_NAME);
        let alias = external.path().join("commit.prepublication-link");
        transaction.failpoint = Some(Box::new(HardLinkPublicationFailpoint {
            source: pending_commit,
            alias: alias.clone(),
        }));
        assert!(transaction.commit().is_err());
        assert!(alias.is_file());
        assert!(
            !generations_dir(directory.path())
                .join(generation_name(1))
                .exists()
        );
    }

    #[cfg(windows)]
    #[test]
    fn mx10_n5a_012_write_through_directory_visibility_supports_carrier_shape() {
        let directory = tempdir().unwrap();
        let generations = directory.path().join(GENERATIONS_DIR_NAME);
        let publication = generations.join(PUBLICATION_DIRECTORY_NAME);
        let blocked_source = publication.join("blocked-carrier");
        let blocked_target = generations.join("blocked-target");
        fs::create_dir_all(&blocked_source).unwrap();
        fs::write(blocked_source.join(COMMIT_FILE_NAME), b"blocked").unwrap();
        let hard_parent_pin = open_directory_no_follow(&generations).unwrap();
        move_directory_write_through(&blocked_source, &blocked_target).unwrap();
        assert!(!path_entry_exists(&blocked_source).unwrap());
        assert!(path_entry_exists(&blocked_target).unwrap());
        drop(hard_parent_pin);

        let source = publication.join("identity-carrier");
        let target = generations.join(generation_name(1));
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join(COMMIT_FILE_NAME), b"bound").unwrap();
        let source_pin = open_verification_handle_while_mutating(&source, true).unwrap();
        let source_identity = stable_identity(&source_pin).unwrap();

        move_directory_write_through(&source, &target).unwrap();

        assert!(!path_entry_exists(&source).unwrap());
        assert!(same_file_object(
            &stable_identity(&source_pin).unwrap(),
            &source_identity
        ));
        assert_eq!(fs::read(target.join(COMMIT_FILE_NAME)).unwrap(), b"bound");

        let pending = generations.join("native-pending");
        let carrier = publication.join("native-carrier");
        let native_target = generations.join(generation_name(2));
        fs::create_dir_all(&pending).unwrap();
        fs::write(pending.join(COMMIT_FILE_NAME), b"native-bound").unwrap();
        let pending_handle = open_generation_publication_handle(&pending).unwrap();
        let publication_parent = open_directory_publication_parent(&publication).unwrap();
        rename_opened_object_by_handle(&pending_handle, &publication_parent, &carrier).unwrap();
        let carrier_pin = open_verification_handle_while_mutating(&carrier, true).unwrap();
        let carrier_identity = stable_identity(&carrier_pin).unwrap();
        drop(pending_handle);
        drop(publication_parent);
        move_directory_write_through(&carrier, &native_target).unwrap();
        assert!(same_file_object(
            &stable_identity(&carrier_pin).unwrap(),
            &carrier_identity
        ));
        assert_eq!(
            fs::read(native_target.join(COMMIT_FILE_NAME)).unwrap(),
            b"native-bound"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn production_directory_visibility_uses_same_parent_atomic_rename() {
        let directory = tempdir().unwrap();
        let generations = directory.path().join(GENERATIONS_DIR_NAME);
        fs::create_dir(&generations).unwrap();
        let source = generations.join(".pending-portable");
        let target = generations.join(generation_name(1));
        fs::create_dir(&source).unwrap();
        production_write_new(&source.join(COMMIT_FILE_NAME), b"portable-bound").unwrap();
        sync_directory(&source).unwrap();

        move_directory_write_through(&source, &target).unwrap();

        assert!(!path_entry_exists(&source).unwrap());
        assert_eq!(
            fs::read(target.join(COMMIT_FILE_NAME)).unwrap(),
            b"portable-bound"
        );

        let second = generations.join(".pending-second");
        fs::create_dir(&second).unwrap();
        assert!(move_directory_write_through(&second, &target).is_err());
        assert!(path_entry_exists(&second).unwrap());
    }

    #[test]
    fn mx10_n5a_016_dangling_link_is_not_treated_as_absent_when_supported() {
        let directory = tempdir().unwrap();
        let target = directory.path().join("missing-target");
        let link = directory.path().join("dangling-link");
        #[cfg(windows)]
        let created = std::os::windows::fs::symlink_file(&target, &link).is_ok();
        #[cfg(unix)]
        let created = std::os::unix::fs::symlink(&target, &link).is_ok();
        if !created {
            return;
        }
        assert!(path_entry_exists(&link).unwrap());
        assert!(require_path_entry_absent(&link, "dangling-link ratchet").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn mx10_n5a_015_unix_preflight_refuses_before_any_tree_mutation() {
        fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
            fn visit(root: &Path, directory: &Path, output: &mut BTreeMap<String, Vec<u8>>) {
                let mut entries = fs::read_dir(directory)
                    .unwrap()
                    .map(|entry| entry.unwrap())
                    .collect::<Vec<_>>();
                entries.sort_by_key(|entry| entry.file_name());
                for entry in entries {
                    let relative = entry
                        .path()
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    let metadata = fs::symlink_metadata(entry.path()).unwrap();
                    if metadata.is_dir() {
                        output.insert(format!("d:{relative}"), Vec::new());
                        visit(root, &entry.path(), output);
                    } else {
                        output.insert(format!("f:{relative}"), fs::read(entry.path()).unwrap());
                    }
                }
            }
            let mut output = BTreeMap::new();
            visit(root, root, &mut output);
            output
        }

        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        let before = snapshot(directory.path());
        let activation = ExclusiveBaselineLease::acquire_disposable(directory.path()).unwrap_err();
        assert_eq!(activation.kind(), io::ErrorKind::Unsupported);
        assert_eq!(snapshot(directory.path()), before);
        let begin = match OperationTransaction::begin(
            directory.path(),
            transaction_id(TX1),
            OperationKind::Ingest,
            b"unix refusal",
        ) {
            Err(error) => error,
            Ok(_) => panic!("Unix private transaction admission unexpectedly succeeded"),
        };
        assert_eq!(begin.kind(), io::ErrorKind::Unsupported);
        assert_eq!(snapshot(directory.path()), before);
        assert!(
            fs::symlink_metadata(transaction_dir(directory.path()))
                .is_err_and(|error| { error.kind() == io::ErrorKind::NotFound })
        );
        assert!(
            fs::symlink_metadata(directory.path().join(ACTIVATION_LOCK_FILE_NAME))
                .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn fully_staged_generation_tree_has_one_visibility_publication() {
        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        activate(directory.path());
        let mut transaction = begin_new(directory.path(), TX1, b"directory publication");
        stage_transition(&mut transaction, 1, &[TX1]);
        assert!(!transaction.published_generation_dir().exists());
        assert!(
            transaction
                .generation_dir()
                .join(COMPONENTS_DIR_NAME)
                .join(FIXTURE_STATE_PATH)
                .is_file()
        );
        apply_live_transition(directory.path(), 1, &[TX1]);
        transaction.commit().unwrap();
        let published = generations_dir(directory.path()).join(generation_name(1));
        assert!(published.join(COMMIT_FILE_NAME).is_file());
        assert!(
            published
                .join(COMPONENTS_DIR_NAME)
                .join(FIXTURE_HISTORY_PATH)
                .is_file()
        );
        let entries = read_directory_bounded(&generations_dir(directory.path()), &DEFAULT_LIMITS)
            .unwrap()
            .into_iter()
            .map(|entry| entry.file_name().into_string().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            entries,
            BTreeSet::from([PUBLICATION_DIRECTORY_NAME.to_owned(), generation_name(1),])
        );
        assert!(
            read_directory_bounded(&publication_dir(directory.path()), &DEFAULT_LIMITS)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn strict_codec_rejects_unknown_fields_noncanonical_bytes_and_future_artifacts() {
        let format = FormatRecord::new().unwrap();
        let bytes = encode_record(&format).unwrap();
        let mut unknown = String::from_utf8(bytes.clone()).unwrap();
        unknown.pop();
        unknown.push_str(",\"future\":1}");
        assert!(decode_record::<FormatRecord>(unknown.as_bytes(), "format").is_err());
        let mut spaced = b" ".to_vec();
        spaced.extend_from_slice(&bytes);
        assert!(decode_record::<FormatRecord>(&spaced, "format").is_err());

        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        activate(directory.path());
        fs::write(
            transaction_dir(directory.path()).join("format.v2"),
            b"future",
        )
        .unwrap();
        assert!(OperationTransaction::recover(directory.path()).is_err());

        let interrupted = tempdir().unwrap();
        write_fixture(interrupted.path());
        fs::create_dir_all(baseline_temp_dir(interrupted.path())).unwrap();
        fs::write(
            baseline_temp_dir(interrupted.path()).join("unclassified.partial"),
            b"unknown",
        )
        .unwrap();
        let lease = ExclusiveBaselineLease::acquire_disposable(interrupted.path()).unwrap();
        assert!(create_legacy_baseline(&lease).is_err());
        assert!(
            baseline_temp_dir(interrupted.path())
                .join("unclassified.partial")
                .exists()
        );
    }

    #[test]
    fn strict_codec_rejects_unknown_fields_in_baseline_descriptor_prepare_and_commit() {
        let descriptors = golden_descriptors();
        let baseline = golden_baseline(&descriptors);
        let prepare = golden_prepare();
        let commit = golden_commit(&prepare);
        for (label, bytes) in [
            ("baseline", encode_record(&baseline).unwrap()),
            ("prepare", encode_record(&prepare).unwrap()),
            ("commit", encode_record(&commit).unwrap()),
        ] {
            let mut text = String::from_utf8(bytes).unwrap();
            text.pop();
            text.push_str(",\"unknown\":true}");
            match label {
                "baseline" => {
                    assert!(decode_record::<BaselineRecord>(text.as_bytes(), label).is_err())
                }
                "prepare" => {
                    assert!(decode_record::<PrepareRecord>(text.as_bytes(), label).is_err())
                }
                "commit" => assert!(decode_record::<CommitRecord>(text.as_bytes(), label).is_err()),
                _ => unreachable!(),
            }
        }
        let mut descriptor = serde_json::to_string(&descriptors[0]).unwrap();
        descriptor.pop();
        descriptor.push_str(",\"future\":0}");
        assert!(serde_json::from_str::<ComponentDescriptor>(&descriptor).is_err());
    }

    fn assert_record_encoding_adversaries<T>(bytes: &[u8], label: &str)
    where
        T: for<'de> Deserialize<'de> + Serialize,
    {
        for suffix in [b"\n".as_slice(), b"\r\n", b" ", b"\t", b"{}"] {
            let mut mutated = bytes.to_vec();
            mutated.extend_from_slice(suffix);
            assert!(decode_record::<T>(&mutated, label).is_err(), "{label}");
        }
        let mut unknown = bytes[..bytes.len() - 1].to_vec();
        unknown.extend_from_slice(b",\"unknown\":0}");
        assert!(decode_record::<T>(&unknown, label).is_err(), "{label}");

        let value = serde_json::from_slice::<serde_json::Value>(bytes).unwrap();
        let reordered = serde_json::to_vec(&value).unwrap();
        if reordered != bytes {
            assert!(decode_record::<T>(&reordered, label).is_err(), "{label}");
        }
    }

    #[test]
    fn every_persisted_record_rejects_noncanonical_and_future_forms() {
        let descriptors = golden_descriptors();
        let baseline = golden_baseline(&descriptors);
        let prepare = golden_prepare();
        let records = [
            (
                "format",
                encode_record(&FormatRecord::new().unwrap()).unwrap(),
            ),
            ("baseline", encode_record(&baseline).unwrap()),
            ("prepare", encode_record(&prepare).unwrap()),
            ("commit", encode_record(&golden_commit(&prepare)).unwrap()),
            (
                "migration",
                encode_record(&golden_migration(&baseline)).unwrap(),
            ),
            (
                "activation lease",
                encode_record(&golden_activation_lease()).unwrap(),
            ),
        ];
        assert_record_encoding_adversaries::<FormatRecord>(&records[0].1, records[0].0);
        assert_record_encoding_adversaries::<BaselineRecord>(&records[1].1, records[1].0);
        assert_record_encoding_adversaries::<PrepareRecord>(&records[2].1, records[2].0);
        assert_record_encoding_adversaries::<CommitRecord>(&records[3].1, records[3].0);
        assert_record_encoding_adversaries::<MigrationRecord>(&records[4].1, records[4].0);
        assert_record_encoding_adversaries::<ActivationLeaseRecord>(&records[5].1, records[5].0);

        let mut duplicate_format = records[0].1[..records[0].1.len() - 1].to_vec();
        duplicate_format.extend_from_slice(b",\"version\":1}");
        assert!(decode_record::<FormatRecord>(&duplicate_format, "format").is_err());
        let malformed_numeric = String::from_utf8(records[0].1.clone()).unwrap().replacen(
            "\"version\":1",
            "\"version\":01",
            1,
        );
        assert!(decode_record::<FormatRecord>(malformed_numeric.as_bytes(), "format").is_err());
        let mut future_format = FormatRecord::new().unwrap();
        future_format.version = FORMAT_VERSION + 1;
        future_format.crc32 = format_crc(&future_format).unwrap();
        assert!(validate_format(&future_format).is_err());
        let mut future_baseline = baseline.clone();
        future_baseline.body.version = FORMAT_VERSION + 1;
        future_baseline.crc32 = record_crc("baseline", &future_baseline.body).unwrap();
        assert!(future_baseline.body.version > FORMAT_VERSION);
        let mut future_prepare = prepare.clone();
        future_prepare.body.version = FORMAT_VERSION + 1;
        future_prepare.crc32 = record_crc("prepare", &future_prepare.body).unwrap();
        let future_path = tempdir().unwrap();
        fs::write(
            future_path.path().join("prepare.bin"),
            encode_record(&future_prepare).unwrap(),
        )
        .unwrap();
        assert!(read_prepare(future_path.path(), &future_path.path().join("prepare.bin")).is_err());
    }

    #[test]
    fn future_coexisting_markers_and_both_publication_orders_fail_closed() {
        let coexisting = tempdir().unwrap();
        write_fixture(coexisting.path());
        activate(coexisting.path());
        fs::write(
            transaction_dir(coexisting.path()).join(FORMAT_TEMP_FILE_NAME),
            encode_record(&FormatRecord::new().unwrap()).unwrap(),
        )
        .unwrap();
        assert!(OperationTransaction::recover(coexisting.path()).is_err());

        let format_first = tempdir().unwrap();
        write_fixture(format_first.path());
        fs::create_dir(transaction_dir(format_first.path())).unwrap();
        fs::create_dir(generations_dir(format_first.path())).unwrap();
        fs::write(
            transaction_dir(format_first.path()).join(FORMAT_FILE_NAME),
            encode_record(&FormatRecord::new().unwrap()).unwrap(),
        )
        .unwrap();
        assert!(OperationTransaction::recover(format_first.path()).is_err());

        let commit_first = tempdir().unwrap();
        write_fixture(commit_first.path());
        activate(commit_first.path());
        let mut transaction = begin_new(commit_first.path(), TX1, b"commit publication order");
        stage_transition(&mut transaction, 1, &[TX1]);
        apply_live_transition(commit_first.path(), 1, &[TX1]);
        transaction.commit().unwrap();
        fs::remove_file(
            generations_dir(commit_first.path())
                .join(generation_name(1))
                .join(PREPARE_FILE_NAME),
        )
        .unwrap();
        assert!(OperationTransaction::recover(commit_first.path()).is_err());
    }

    #[test]
    fn baseline_snapshot_crc_digest_and_generation_one_corruption_fail_closed() {
        let snapshot = tempdir().unwrap();
        write_fixture(snapshot.path());
        activate(snapshot.path());
        commit_transition(snapshot.path(), TX1, 1, &[TX1]);
        fs::write(
            baseline_dir(snapshot.path())
                .join(COMPONENTS_DIR_NAME)
                .join(FIXTURE_STATE_PATH),
            b"counter=9\n",
        )
        .unwrap();
        assert!(OperationTransaction::recover(snapshot.path()).is_err());

        let crc = tempdir().unwrap();
        write_fixture(crc.path());
        activate(crc.path());
        let manifest = baseline_dir(crc.path()).join(BASELINE_MANIFEST_FILE_NAME);
        let mut record = read_record::<BaselineRecord>(&manifest, "baseline").unwrap();
        record.crc32 ^= 1;
        fs::write(&manifest, encode_record(&record).unwrap()).unwrap();
        assert!(
            OperationTransaction::recover(crc.path())
                .unwrap_err()
                .to_string()
                .contains("CRC")
        );

        let digest = tempdir().unwrap();
        write_fixture(digest.path());
        activate(digest.path());
        let manifest = baseline_dir(digest.path()).join(BASELINE_MANIFEST_FILE_NAME);
        let mut record = read_record::<BaselineRecord>(&manifest, "baseline").unwrap();
        record.body.logical_state_digest = empty_hash();
        record.crc32 = record_crc("baseline", &record.body).unwrap();
        fs::write(&manifest, encode_record(&record).unwrap()).unwrap();
        assert!(
            OperationTransaction::recover(digest.path())
                .unwrap_err()
                .to_string()
                .contains("digest")
        );
    }

    #[test]
    fn generation_zero_and_uncommitted_or_direct_mutations_fail_closed() {
        let zero = tempdir().unwrap();
        write_fixture(zero.path());
        activate(zero.path());
        fs::write(zero.path().join(FIXTURE_STATE_PATH), b"counter=1\n").unwrap();
        assert!(OperationTransaction::recover(zero.path()).is_err());

        let uncommitted = tempdir().unwrap();
        write_fixture(uncommitted.path());
        activate(uncommitted.path());
        let mut transaction = begin_new(uncommitted.path(), TX1, b"uncommitted");
        stage_transition(&mut transaction, 1, &[TX1]);
        apply_live_transition(uncommitted.path(), 1, &[TX1]);
        drop(transaction);
        assert!(OperationTransaction::recover(uncommitted.path()).is_err());

        let direct = tempdir().unwrap();
        write_fixture(direct.path());
        activate(direct.path());
        commit_transition(direct.path(), TX1, 1, &[TX1]);
        fs::write(direct.path().join(FIXTURE_STATE_PATH), b"counter=2\n").unwrap();
        assert!(OperationTransaction::recover(direct.path()).is_err());
    }

    #[test]
    fn failpoint_ids_and_repeated_occurrences_are_complete_and_deterministic() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        let lease = ExclusiveBaselineLease::acquire_disposable_with_failpoint(
            directory.path(),
            Some(Box::new(RecordingFailpoint {
                events: events.clone(),
                ..RecordingFailpoint::default()
            })),
        )
        .unwrap();
        create_legacy_baseline_with_failpoint(
            &lease,
            Some(Box::new(RecordingFailpoint {
                events: events.clone(),
                ..RecordingFailpoint::default()
            })),
        )
        .unwrap();
        drop(lease);

        let mut transaction = match OperationTransaction::begin_with_failpoint(
            directory.path(),
            transaction_id(TX1),
            OperationKind::Ingest,
            b"events",
            Some(Box::new(RecordingFailpoint {
                events: events.clone(),
                ..RecordingFailpoint::default()
            })),
        )
        .unwrap()
        {
            TransactionAdmission::New(transaction) => transaction,
            TransactionAdmission::AlreadyCommitted(_) => unreachable!(),
        };
        stage_transition(&mut transaction, 1, &[TX1]);
        apply_live_transition(directory.path(), 1, &[TX1]);
        transaction.commit().unwrap();
        OperationTransaction::recover_with_failpoint(
            directory.path(),
            Some(Box::new(RecordingFailpoint {
                events: events.clone(),
                ..RecordingFailpoint::default()
            })),
        )
        .unwrap();

        let prepared = begin_new(directory.path(), TX2, b"cleanup");
        drop(prepared);
        let _ = OperationTransaction::begin_with_failpoint(
            directory.path(),
            transaction_id(TX2),
            OperationKind::Ingest,
            b"cleanup",
            Some(Box::new(RecordingFailpoint {
                events: events.clone(),
                ..RecordingFailpoint::default()
            })),
        )
        .unwrap();

        let observed = events.lock().unwrap().clone();
        let observed_stages = observed
            .iter()
            .map(|(stage, _)| *stage)
            .collect::<BTreeSet<_>>();
        let expected_stages = ALL_OPERATION_STAGES
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        assert_eq!(observed_stages, expected_stages);
        for repeated in [
            OperationStage::BaselineBeforeComponentOpen,
            OperationStage::BaselineBeforeComponentWrite,
            OperationStage::BaselineAfterComponentWrite,
            OperationStage::BaselineAfterComponentFlush,
            OperationStage::BaselineAfterComponentFileSync,
            OperationStage::BaselineAfterComponentSync,
            OperationStage::BaselineBeforeComponentParentCreate,
            OperationStage::BaselineAfterComponentParentCreate,
            OperationStage::BaselineAfterComponentParentSync,
            OperationStage::BaselineAfterComponentsParentSync,
            OperationStage::BeforeComponentParentCreate,
            OperationStage::AfterComponentParentCreate,
            OperationStage::AfterComponentParentSync,
            OperationStage::AfterComponentComponentsSync,
            OperationStage::BeforeComponentWrite,
            OperationStage::AfterComponentWrite,
            OperationStage::AfterComponentFlush,
            OperationStage::AfterComponentFileSync,
            OperationStage::AfterComponentSync,
        ] {
            let occurrences = observed
                .iter()
                .filter_map(|(stage, occurrence)| (*stage == repeated).then_some(*occurrence))
                .collect::<BTreeSet<_>>();
            assert!(occurrences.contains(&0));
            assert!(occurrences.contains(&1));
        }
        let ids = ALL_OPERATION_STAGES
            .iter()
            .map(|stage| stage.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), ALL_OPERATION_STAGES.len());
    }

    #[test]
    fn returned_preprepare_errors_remain_same_id_retryable() {
        for stage in [
            OperationStage::BeforeGenerationCreate,
            OperationStage::AfterGenerationCreate,
            OperationStage::AfterGenerationSync,
            OperationStage::AfterGenerationsParentSync,
            OperationStage::BeforeGenerationComponentsCreate,
            OperationStage::AfterGenerationComponentsCreate,
            OperationStage::AfterGenerationComponentsSync,
            OperationStage::AfterGenerationComponentsParentSync,
            OperationStage::BeforePrepareWrite,
        ] {
            let directory = tempdir().unwrap();
            write_fixture(directory.path());
            activate(directory.path());
            let first = OperationTransaction::begin_with_failpoint(
                directory.path(),
                transaction_id(TX1),
                OperationKind::Ingest,
                b"preprepare retry",
                Some(Box::new(RecordingFailpoint {
                    fail_at: Some((stage, 0)),
                    ..RecordingFailpoint::default()
                })),
            );
            assert!(first.is_err(), "{}", stage.as_str());
            assert_eq!(
                OperationTransaction::recover(directory.path())
                    .unwrap()
                    .generation,
                0,
                "{}",
                stage.as_str()
            );
            let retried = OperationTransaction::begin(
                directory.path(),
                transaction_id(TX1),
                OperationKind::Ingest,
                b"preprepare retry",
            )
            .unwrap();
            assert!(
                matches!(retried, TransactionAdmission::New(_)),
                "{}",
                stage.as_str()
            );
        }
    }

    #[test]
    fn returned_error_failpoint_preserves_retryable_pre_state() {
        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        let lease = ExclusiveBaselineLease::acquire_disposable(directory.path()).unwrap();
        let error = create_legacy_baseline_with_failpoint(
            &lease,
            Some(Box::new(RecordingFailpoint {
                fail_at: Some((OperationStage::BaselineAfterManifestWrite, 0)),
                ..RecordingFailpoint::default()
            })),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        drop(lease);
        assert_eq!(
            OperationTransaction::recover(directory.path()).unwrap(),
            RecoveryState::legacy()
        );
        activate(directory.path());
        let first = OperationTransaction::recover(directory.path()).unwrap();
        let second = OperationTransaction::recover(directory.path()).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn subprocess_failpoint_child() {
        let Ok(root) = std::env::var("MEMORYX_N5_CHILD_ROOT") else {
            return;
        };
        let lease = ExclusiveBaselineLease::acquire_disposable(Path::new(&root)).unwrap();
        let _ = create_legacy_baseline_with_failpoint(&lease, Some(Box::new(EnvironmentFailpoint)));
    }

    #[test]
    fn transaction_subprocess_failpoint_child() {
        let Ok(root) = std::env::var("MEMORYX_N5_TXN_CHILD_ROOT") else {
            return;
        };
        let _ = OperationTransaction::begin_with_failpoint(
            Path::new(&root),
            transaction_id(TX1),
            OperationKind::Ingest,
            b"owned child preprepare",
            Some(Box::new(EnvironmentFailpoint)),
        );
    }

    #[test]
    fn cleanup_subprocess_failpoint_child() {
        let Ok(root) = std::env::var("MEMORYX_N5_CLEANUP_CHILD_ROOT") else {
            return;
        };
        let _ = OperationTransaction::begin_with_failpoint(
            Path::new(&root),
            transaction_id(TX1),
            OperationKind::Ingest,
            b"owned child cleanup",
            Some(Box::new(EnvironmentFailpoint)),
        );
    }

    #[cfg(windows)]
    #[test]
    fn activation_owner_exit_code_child() {
        let Ok(root) = std::env::var("MEMORYX_N5_EXIT_OWNER_ROOT") else {
            return;
        };
        let code: i32 = std::env::var("MEMORYX_N5_EXIT_OWNER_CODE")
            .unwrap()
            .parse()
            .unwrap();
        let _lease = ExclusiveBaselineLease::acquire_disposable(Path::new(&root)).unwrap();
        std::process::exit(code);
    }

    #[cfg(windows)]
    #[test]
    fn mx10_n5a_018_signaled_owner_is_dead_even_when_exit_code_is_259() {
        for exit_code in [42, 259] {
            let directory = tempdir().unwrap();
            write_fixture(directory.path());
            let mut child = Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("store::operation_txn::tests::activation_owner_exit_code_child")
                .arg("--nocapture")
                .env("MEMORYX_N5_EXIT_OWNER_ROOT", directory.path())
                .env("MEMORYX_N5_EXIT_OWNER_CODE", exit_code.to_string())
                .spawn()
                .unwrap();
            let status = child.wait().unwrap();
            assert_eq!(status.code(), Some(exit_code));

            let lease = ExclusiveBaselineLease::acquire_disposable(directory.path()).unwrap();
            drop(lease);
        }
    }

    #[test]
    fn mx10_n5a_013_owned_abort_after_prepare_removal_is_retryable() {
        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        activate(directory.path());
        let mut transaction = begin_new(directory.path(), TX1, b"owned child cleanup");
        stage_transition(&mut transaction, 1, &[TX1]);
        drop(transaction);
        let output = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("store::operation_txn::tests::cleanup_subprocess_failpoint_child")
            .arg("--nocapture")
            .env("MEMORYX_N5_CLEANUP_CHILD_ROOT", directory.path())
            .env("MEMORYX_N5_NAMESPACE_AFTER", PREPARE_FILE_NAME)
            .env("MEMORYX_N5_FAIL_ACTION", "abort")
            .output()
            .unwrap();
        assert!(!output.status.success());
        let first = OperationTransaction::recover(directory.path()).unwrap();
        let second = OperationTransaction::recover(directory.path()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.generation, 0);
        let retry = begin_new(directory.path(), TX1, b"owned child cleanup");
        assert_eq!(retry.generation(), 1);
        drop(retry);
    }

    #[test]
    fn subprocess_abort_before_prepare_is_retryable_and_history_once() {
        let directory = tempdir().unwrap();
        write_fixture(directory.path());
        activate(directory.path());
        let output = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("store::operation_txn::tests::transaction_subprocess_failpoint_child")
            .arg("--nocapture")
            .env("MEMORYX_N5_TXN_CHILD_ROOT", directory.path())
            .env(
                "MEMORYX_N5_FAILPOINT",
                "n5.txn.components_directory.after_create#0",
            )
            .env("MEMORYX_N5_FAIL_ACTION", "abort")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(
            OperationTransaction::recover(directory.path())
                .unwrap()
                .generation,
            0
        );
        let mut retried = match OperationTransaction::begin(
            directory.path(),
            transaction_id(TX1),
            OperationKind::Ingest,
            b"owned child preprepare",
        )
        .unwrap()
        {
            TransactionAdmission::New(transaction) => transaction,
            TransactionAdmission::AlreadyCommitted(_) => unreachable!(),
        };
        stage_transition(&mut retried, 1, &[TX1]);
        apply_live_transition(directory.path(), 1, &[TX1]);
        let committed = retried.commit().unwrap();
        match OperationTransaction::begin(
            directory.path(),
            transaction_id(TX1),
            OperationKind::Ingest,
            b"owned child preprepare",
        )
        .unwrap()
        {
            TransactionAdmission::AlreadyCommitted(state) => assert_eq!(state, committed),
            TransactionAdmission::New(_) => panic!("committed retry must not append history"),
        }
        assert_eq!(
            fs::read_to_string(directory.path().join(FIXTURE_HISTORY_PATH)).unwrap(),
            format!("{TX1}\n")
        );
    }

    #[test]
    fn subprocess_abort_seam_is_deterministic_and_retryable() {
        for _ in 0..8 {
            let directory = tempdir().unwrap();
            write_fixture(directory.path());
            let output = Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("store::operation_txn::tests::subprocess_failpoint_child")
                .arg("--nocapture")
                .env("MEMORYX_N5_CHILD_ROOT", directory.path())
                .env("MEMORYX_N5_FAILPOINT", "n5.baseline.manifest.after_write#0")
                .env("MEMORYX_N5_FAIL_ACTION", "abort")
                .output()
                .unwrap();
            assert!(!output.status.success());
            activate(directory.path());
            let first = OperationTransaction::recover(directory.path()).unwrap();
            let second = OperationTransaction::recover(directory.path()).unwrap();
            assert_eq!(first, second);
        }
    }

    fn golden_descriptors() -> Vec<ComponentDescriptor> {
        let mut descriptors = vec![
            ComponentDescriptor {
                kind: DurableComponentKind::FixtureState,
                relative_path: FIXTURE_STATE_PATH.to_owned(),
                length: 10,
                blake3_hash: hash_hex(b"counter=0\n"),
            },
            ComponentDescriptor {
                kind: DurableComponentKind::FixtureHistory,
                relative_path: FIXTURE_HISTORY_PATH.to_owned(),
                length: 0,
                blake3_hash: hash_hex(b""),
            },
        ];
        descriptors.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        descriptors
    }

    fn golden_baseline(descriptors: &[ComponentDescriptor]) -> BaselineRecord {
        BaselineRecord::new(BaselineBody {
            magic: BASELINE_MAGIC.to_owned(),
            version: FORMAT_VERSION,
            codec: PRIVATE_CODEC_ID.to_owned(),
            component_registry: COMPONENT_REGISTRY_ID.to_owned(),
            source_generation: 0,
            components: descriptors.to_vec(),
            logical_state_digest: canonical_logical_state_digest(descriptors).unwrap(),
            downgrade_guard: DOWNGRADE_GUARD_ID.to_owned(),
        })
        .unwrap()
    }

    fn golden_prepare() -> PrepareRecord {
        PrepareRecord::new(PrepareBody {
            magic: FORMAT_MAGIC.to_owned(),
            version: FORMAT_VERSION,
            codec: PRIVATE_CODEC_ID.to_owned(),
            generation: 1,
            parent_commit_hash: empty_hash(),
            transaction_id: transaction_id(TX1),
            operation: OperationKind::Ingest,
            intent_hash: hash_hex(b"golden intent"),
        })
        .unwrap()
    }

    fn golden_commit(prepare: &PrepareRecord) -> CommitRecord {
        let components = vec![
            ComponentDescriptor {
                kind: DurableComponentKind::FixtureHistory,
                relative_path: FIXTURE_HISTORY_PATH.to_owned(),
                length: 37,
                blake3_hash: hash_hex(format!("{TX1}\n").as_bytes()),
            },
            ComponentDescriptor {
                kind: DurableComponentKind::FixtureState,
                relative_path: FIXTURE_STATE_PATH.to_owned(),
                length: 10,
                blake3_hash: hash_hex(b"counter=1\n"),
            },
        ];
        let prepare_hash = hash_hex(&encode_record(prepare).unwrap());
        CommitRecord::new(CommitBody {
            magic: FORMAT_MAGIC.to_owned(),
            version: FORMAT_VERSION,
            codec: PRIVATE_CODEC_ID.to_owned(),
            generation: 1,
            parent_commit_hash: empty_hash(),
            prepare_hash: prepare_hash.clone(),
            transaction_id: transaction_id(TX1),
            operation: OperationKind::Ingest,
            intent_hash: prepare.body.intent_hash.clone(),
            logical_snapshot_hash: logical_snapshot_hash(&empty_hash(), &prepare_hash, &components)
                .unwrap(),
            components,
        })
        .unwrap()
    }

    fn golden_migration(baseline: &BaselineRecord) -> MigrationRecord {
        migration_record(baseline, 2 * 1024 * 1024).unwrap()
    }

    fn golden_activation_lease() -> ActivationLeaseRecord {
        ActivationLeaseRecord {
            schema: ACTIVATION_LEASE_SCHEMA.to_owned(),
            owner_pid: 4242,
            canonical_root: "C:/memoryx/n5-fixture".to_owned(),
        }
    }

    #[test]
    fn checked_in_golden_v1_records_and_crc_hash_inputs_are_exact() {
        let format = encode_record(&FormatRecord::new().unwrap()).unwrap();
        let descriptors = golden_descriptors();
        let baseline_record = golden_baseline(&descriptors);
        let baseline = encode_record(&baseline_record).unwrap();
        let prepare_record = golden_prepare();
        let prepare = encode_record(&prepare_record).unwrap();
        let commit = encode_record(&golden_commit(&prepare_record)).unwrap();
        let migration = encode_record(&golden_migration(&baseline_record)).unwrap();
        let activation = encode_record(&golden_activation_lease()).unwrap();
        assert_eq!(
            format.as_slice(),
            include_bytes!("../../docs/crash-recovery/golden/v1/format.json")
        );
        assert_eq!(
            baseline.as_slice(),
            include_bytes!("../../docs/crash-recovery/golden/v1/baseline.json")
        );
        assert_eq!(
            prepare.as_slice(),
            include_bytes!("../../docs/crash-recovery/golden/v1/prepare.json")
        );
        assert_eq!(
            commit.as_slice(),
            include_bytes!("../../docs/crash-recovery/golden/v1/commit.json")
        );
        assert_eq!(
            migration.as_slice(),
            include_bytes!("../../docs/crash-recovery/golden/v1/migration.json")
        );
        assert_eq!(
            activation.as_slice(),
            include_bytes!("../../docs/crash-recovery/golden/v1/activation-lease.json")
        );
        let decoded = decode_record::<CommitRecord>(&commit, "commit").unwrap();
        assert_eq!(decoded.crc32, golden_commit(&prepare_record).crc32);
        assert_eq!(hash_hex(&prepare), decoded.body.prepare_hash);
    }
}
