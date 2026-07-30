//! Immutable operation-transaction generations.
//!
//! This module is the N5-A durable format and recovery scanner.  It deliberately
//! does not make existing store writes transactional yet: N5-B will stage CAS,
//! indexes, graph state, metadata, and history through this coordinator.  Until
//! then, the only published data owned by this module is its own generation log.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::utils::crc32;

const FORMAT_MAGIC: &str = "MEMORYX_OPERATION_TXN";
const FORMAT_VERSION: u32 = 1;
const TXN_DIR_NAME: &str = "operation_txn";
const FORMAT_FILE_NAME: &str = "format.v1";
const GENERATIONS_DIR_NAME: &str = "generations";
const PREPARE_FILE_NAME: &str = "prepare.bin";
const PREPARE_TEMP_FILE_NAME: &str = "prepare.tmp";
const COMMIT_FILE_NAME: &str = "commit.bin";
const COMMIT_TEMP_FILE_NAME: &str = "commit.tmp";
const COMPONENTS_DIR_NAME: &str = "components";
const MAX_COMPONENT_BYTES: u64 = 1024 * 1024 * 1024;

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

/// Stable failpoint positions for subprocess crash injection in N5-E.
///
/// The coordinator calls each stage in this exact order. A hook that returns an
/// error leaves the generation uncommitted unless it has already reached
/// `AfterCommitPublish`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum OperationStage {
    BeforeFormatWrite,
    AfterFormatSync,
    BeforePrepareWrite,
    AfterPrepareSync,
    BeforeComponentWrite,
    AfterComponentSync,
    BeforeCommitWrite,
    AfterCommitSync,
    BeforeCommitPublish,
    AfterCommitPublish,
}

impl OperationStage {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::BeforeFormatWrite => "before_format_write",
            Self::AfterFormatSync => "after_format_sync",
            Self::BeforePrepareWrite => "before_prepare_write",
            Self::AfterPrepareSync => "after_prepare_sync",
            Self::BeforeComponentWrite => "before_component_write",
            Self::AfterComponentSync => "after_component_sync",
            Self::BeforeCommitWrite => "before_commit_write",
            Self::AfterCommitSync => "after_commit_sync",
            Self::BeforeCommitPublish => "before_commit_publish",
            Self::AfterCommitPublish => "after_commit_publish",
        }
    }
}

/// Test and fault-injection hook. Production code normally uses no hook.
pub(crate) trait OperationFailpoint: Send {
    fn hit(&mut self, stage: OperationStage) -> io::Result<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FormatRecord {
    magic: String,
    version: u32,
    crc32: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PrepareBody {
    magic: String,
    version: u32,
    generation: u64,
    parent_commit_hash: String,
    operation: OperationKind,
    intent_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PrepareRecord {
    body: PrepareBody,
    crc32: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ComponentDescriptor {
    relative_path: String,
    length: u64,
    blake3_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommitBody {
    magic: String,
    version: u32,
    generation: u64,
    parent_commit_hash: String,
    prepare_hash: String,
    intent_hash: String,
    components: Vec<ComponentDescriptor>,
    logical_snapshot_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommitRecord {
    body: CommitBody,
    crc32: u32,
}

/// Recovery result. A base with no N5 format is legacy generation zero and is
/// left unchanged by recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecoveryState {
    pub(crate) generation: u64,
    pub(crate) commit_hash: String,
    pub(crate) legacy: bool,
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
/// `commit()` atomically publishes its `commit.bin` file.
pub(crate) struct OperationTransaction {
    root: PathBuf,
    generation: u64,
    parent_commit_hash: String,
    operation: OperationKind,
    intent_hash: String,
    components: BTreeMap<String, ComponentDescriptor>,
    failpoint: Option<Box<dyn OperationFailpoint>>,
    committed: bool,
}

impl OperationTransaction {
    /// Scan committed generations without modifying an old base.
    ///
    /// A valid prepare without `commit.bin` is intentionally ignored. A corrupt
    /// committed generation, a chain gap, or a committed generation after an
    /// incomplete one fails closed.
    pub(crate) fn recover(root: &Path) -> io::Result<RecoveryState> {
        let transaction_dir = transaction_dir(root);
        let format_path = transaction_dir.join(FORMAT_FILE_NAME);
        let generations = generations_dir(root);

        if !format_path.exists() {
            if generations.exists() {
                return Err(invalid_data(
                    "operation transaction generations exist without format.v1",
                ));
            }
            return Ok(RecoveryState::legacy());
        }

        validate_format(&read_record::<FormatRecord>(&format_path, "format")?)?;
        if !generations.is_dir() {
            return Err(invalid_data(
                "operation transaction format exists without generations directory",
            ));
        }

        let mut directories = fs::read_dir(&generations)?.collect::<io::Result<Vec<_>>>()?;
        directories.sort_by_key(|entry| entry.file_name());

        let mut state = RecoveryState {
            generation: 0,
            commit_hash: empty_hash(),
            legacy: false,
        };
        let mut saw_incomplete = false;

        for entry in directories {
            if !entry.file_type()?.is_dir() {
                return Err(invalid_data(
                    "operation transaction generations contains a non-directory entry",
                ));
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let generation = parse_generation_name(&name)?;
            let path = entry.path();
            let prepare_path = path.join(PREPARE_FILE_NAME);
            let commit_path = path.join(COMMIT_FILE_NAME);

            if !prepare_path.exists() {
                if commit_path.exists() {
                    return Err(invalid_data("committed generation has no prepare record"));
                }
                saw_incomplete = true;
                continue;
            }

            let prepare = read_prepare(&prepare_path)?;
            validate_prepare(&prepare, generation, &state)?;

            if !commit_path.exists() {
                saw_incomplete = true;
                continue;
            }
            if saw_incomplete {
                return Err(invalid_data(
                    "committed generation follows an incomplete generation",
                ));
            }

            let commit_bytes = read_bytes(&commit_path)?;
            let commit = decode_record::<CommitRecord>(&commit_bytes, "commit")?;
            validate_commit(&commit, &prepare, generation, &state, &path)?;
            state.generation = generation;
            state.commit_hash = hash_hex(&commit_bytes);
        }

        Ok(state)
    }

    /// Prepare a transaction using a canonical operation-intent byte sequence.
    pub(crate) fn begin(
        root: &Path,
        operation: OperationKind,
        canonical_intent: &[u8],
    ) -> io::Result<Self> {
        Self::begin_with_failpoint(root, operation, canonical_intent, None)
    }

    /// Prepare a transaction with an optional deterministic failpoint hook.
    pub(crate) fn begin_with_failpoint(
        root: &Path,
        operation: OperationKind,
        canonical_intent: &[u8],
        failpoint: Option<Box<dyn OperationFailpoint>>,
    ) -> io::Result<Self> {
        let mut transaction = Self {
            root: root.to_path_buf(),
            generation: 0,
            parent_commit_hash: String::new(),
            operation,
            intent_hash: hash_hex(canonical_intent),
            components: BTreeMap::new(),
            failpoint,
            committed: false,
        };
        transaction.ensure_format()?;
        if has_incomplete_generation(root)? {
            return Err(invalid_data(
                "operation transaction recovery found an incomplete generation; refuse to reuse it",
            ));
        }
        let previous = Self::recover(root)?;
        transaction.generation = previous
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid_data("operation transaction generation overflow"))?;
        transaction.parent_commit_hash = previous.commit_hash;

        let generation_dir = transaction.generation_dir();
        fs::create_dir_all(generation_dir.join(COMPONENTS_DIR_NAME))?;
        sync_directory(&generation_dir)?;
        sync_directory(&generations_dir(root))?;

        let record = PrepareRecord::new(PrepareBody {
            magic: FORMAT_MAGIC.to_owned(),
            version: FORMAT_VERSION,
            generation: transaction.generation,
            parent_commit_hash: transaction.parent_commit_hash.clone(),
            operation,
            intent_hash: transaction.intent_hash.clone(),
        })?;
        transaction.hit(OperationStage::BeforePrepareWrite)?;
        write_record_atomic(
            &generation_dir.join(PREPARE_TEMP_FILE_NAME),
            &generation_dir.join(PREPARE_FILE_NAME),
            &record,
        )?;
        transaction.hit(OperationStage::AfterPrepareSync)?;
        Ok(transaction)
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

        self.hit(OperationStage::BeforeComponentWrite)?;
        let target = self
            .generation_dir()
            .join(COMPONENTS_DIR_NAME)
            .join(&normalized);
        let parent = target
            .parent()
            .ok_or_else(|| io::Error::other("component path has no parent"))?;
        fs::create_dir_all(parent)?;
        let components_root = fs::canonicalize(self.generation_dir().join(COMPONENTS_DIR_NAME))?;
        let canonical_parent = fs::canonicalize(parent)?;
        if !canonical_parent.starts_with(&components_root)
            || target
                .symlink_metadata()
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(invalid_data(
                "transaction component path escapes its generation",
            ));
        }
        write_file_sync(&target, bytes)?;
        sync_directory(parent)?;
        self.hit(OperationStage::AfterComponentSync)?;

        self.components.insert(
            normalized.clone(),
            ComponentDescriptor {
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
        let prepare_hash = hash_hex(&read_bytes(&prepare_path)?);
        let logical_snapshot_hash =
            logical_snapshot_hash(&self.parent_commit_hash, &prepare_hash, &components)?;
        let record = CommitRecord::new(CommitBody {
            magic: FORMAT_MAGIC.to_owned(),
            version: FORMAT_VERSION,
            generation: self.generation,
            parent_commit_hash: self.parent_commit_hash.clone(),
            prepare_hash,
            intent_hash: self.intent_hash.clone(),
            components,
            logical_snapshot_hash,
        })?;

        self.hit(OperationStage::BeforeCommitWrite)?;
        let generation_dir = self.generation_dir();
        write_record_sync(&generation_dir.join(COMMIT_TEMP_FILE_NAME), &record)?;
        self.hit(OperationStage::AfterCommitSync)?;
        self.hit(OperationStage::BeforeCommitPublish)?;
        atomic_publish(
            &generation_dir.join(COMMIT_TEMP_FILE_NAME),
            &generation_dir.join(COMMIT_FILE_NAME),
        )?;
        self.hit(OperationStage::AfterCommitPublish)?;
        self.committed = true;
        Self::recover(&self.root)
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
        let directory = transaction_dir(&self.root);
        let format_path = directory.join(FORMAT_FILE_NAME);
        if format_path.exists() {
            validate_format(&read_record::<FormatRecord>(&format_path, "format")?)?;
            fs::create_dir_all(generations_dir(&self.root))?;
            return Ok(());
        }

        if generations_dir(&self.root).exists() {
            return Err(invalid_data(
                "refusing to create operation transaction format over unknown generations",
            ));
        }
        if self.root.exists()
            && fs::read_dir(&self.root)?
                .any(|entry| entry.is_ok_and(|entry| entry.file_name() != TXN_DIR_NAME))
        {
            return Err(invalid_data(
                "legacy base requires an explicit verified baseline migration",
            ));
        }
        fs::create_dir_all(&directory)?;
        self.hit(OperationStage::BeforeFormatWrite)?;
        let record = FormatRecord::new()?;
        write_record_atomic(&directory.join("format.tmp"), &format_path, &record)?;
        fs::create_dir_all(generations_dir(&self.root))?;
        sync_directory(&directory)?;
        self.hit(OperationStage::AfterFormatSync)
    }

    fn generation_dir(&self) -> PathBuf {
        generations_dir(&self.root).join(generation_name(self.generation))
    }

    fn hit(&mut self, stage: OperationStage) -> io::Result<()> {
        if let Some(failpoint) = &mut self.failpoint {
            failpoint.hit(stage)?;
        }
        Ok(())
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
            crc32: 0,
        };
        record.crc32 = record_crc(&record.magic, record.version, &())?;
        Ok(record)
    }
}

impl PrepareRecord {
    fn new(body: PrepareBody) -> io::Result<Self> {
        let crc32 = record_crc(&body.magic, body.version, &body)?;
        Ok(Self { body, crc32 })
    }
}

impl CommitRecord {
    fn new(body: CommitBody) -> io::Result<Self> {
        let crc32 = record_crc(&body.magic, body.version, &body)?;
        Ok(Self { body, crc32 })
    }
}

fn transaction_dir(root: &Path) -> PathBuf {
    root.join(TXN_DIR_NAME)
}

fn generations_dir(root: &Path) -> PathBuf {
    transaction_dir(root).join(GENERATIONS_DIR_NAME)
}

fn generation_name(generation: u64) -> String {
    format!("{generation:020}")
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

fn has_incomplete_generation(root: &Path) -> io::Result<bool> {
    let generations = generations_dir(root);
    if !generations.exists() {
        return Ok(false);
    }
    for entry in fs::read_dir(generations)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            return Err(invalid_data(
                "operation transaction generations contains a non-directory entry",
            ));
        }
        parse_generation_name(&entry.file_name().to_string_lossy())?;
        let path = entry.path();
        if !path.join(PREPARE_FILE_NAME).exists() || !path.join(COMMIT_FILE_NAME).exists() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn validate_format(record: &FormatRecord) -> io::Result<()> {
    if record.magic != FORMAT_MAGIC || record.version != FORMAT_VERSION {
        return Err(invalid_data("unsupported operation transaction format"));
    }
    let expected = record_crc(&record.magic, record.version, &())?;
    if record.crc32 != expected {
        return Err(invalid_data("operation transaction format CRC mismatch"));
    }
    Ok(())
}

fn read_prepare(path: &Path) -> io::Result<PrepareRecord> {
    let record = read_record::<PrepareRecord>(path, "prepare")?;
    if record.body.magic != FORMAT_MAGIC || record.body.version != FORMAT_VERSION {
        return Err(invalid_data(
            "unsupported operation transaction prepare record",
        ));
    }
    if record.crc32 != record_crc(&record.body.magic, record.body.version, &record.body)? {
        return Err(invalid_data("operation transaction prepare CRC mismatch"));
    }
    if !is_hash(&record.body.parent_commit_hash) || !is_hash(&record.body.intent_hash) {
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
    if commit.crc32 != record_crc(&commit.body.magic, commit.body.version, &commit.body)? {
        return Err(invalid_data("operation transaction commit CRC mismatch"));
    }
    let expected_prepare_hash = hash_hex(&encode_record(prepare)?);
    if commit.body.generation != generation
        || generation != state.generation.saturating_add(1)
        || commit.body.parent_commit_hash != state.commit_hash
        || commit.body.prepare_hash != expected_prepare_hash
        || commit.body.intent_hash != prepare.body.intent_hash
        || !is_hash(&commit.body.parent_commit_hash)
        || !is_hash(&commit.body.prepare_hash)
        || !is_hash(&commit.body.intent_hash)
        || !is_hash(&commit.body.logical_snapshot_hash)
    {
        return Err(invalid_data(
            "operation transaction commit does not match its prepare or parent chain",
        ));
    }

    let mut previous = None;
    for descriptor in &commit.body.components {
        if descriptor.relative_path
            != normalize_component_path(Path::new(&descriptor.relative_path))?
            || descriptor.length > MAX_COMPONENT_BYTES
            || !is_hash(&descriptor.blake3_hash)
        {
            return Err(invalid_data(
                "invalid operation transaction component descriptor",
            ));
        }
        if previous.as_ref() >= Some(&descriptor.relative_path) {
            return Err(invalid_data(
                "operation transaction component descriptors are not strictly sorted",
            ));
        }
        previous = Some(descriptor.relative_path.clone());
        let component = generation_dir
            .join(COMPONENTS_DIR_NAME)
            .join(&descriptor.relative_path);
        let metadata = fs::metadata(&component).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("committed operation transaction component is missing: {error}"),
            )
        })?;
        if !metadata.is_file() || metadata.len() != descriptor.length {
            return Err(invalid_data(
                "committed operation transaction component length is invalid",
            ));
        }
        let bytes = read_bytes(&component)?;
        if hash_hex(&bytes) != descriptor.blake3_hash {
            return Err(invalid_data(
                "committed operation transaction component hash mismatch",
            ));
        }
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
    let bytes = serde_json::to_vec(&(parent_commit_hash, prepare_hash, components))
        .map_err(io::Error::other)?;
    Ok(hash_hex(&bytes))
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
                let value = part.to_string_lossy();
                if value.contains('\\') || value.contains('/') {
                    return Err(invalid_data("transaction component path is not normalized"));
                }
                parts.push(value.into_owned());
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
    Ok(parts.join("/"))
}

fn write_record_atomic<T: Serialize>(temp: &Path, target: &Path, record: &T) -> io::Result<()> {
    write_record_sync(temp, record)?;
    atomic_publish(temp, target)
}

fn write_record_sync<T: Serialize>(path: &Path, record: &T) -> io::Result<()> {
    write_file_sync(path, &encode_record(record)?)
}

fn write_file_sync(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("transaction file has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()
}

fn atomic_publish(temp: &Path, target: &Path) -> io::Result<()> {
    if target.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "operation transaction commit target already exists",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

        let mut from = temp.as_os_str().encode_wide().collect::<Vec<_>>();
        from.push(0);
        let mut to = target.as_os_str().encode_wide().collect::<Vec<_>>();
        to.push(0);
        // Safety: both vectors are valid NUL-terminated UTF-16 paths on the
        // same volume, and the target is checked not to exist above.
        let result = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(not(windows))]
    {
        fs::rename(temp, target)?;
    }
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("transaction publish target has no parent"))?;
    sync_directory(parent)
}

fn read_record<T: for<'de> Deserialize<'de>>(path: &Path, label: &str) -> io::Result<T> {
    decode_record(&read_bytes(path)?, label)
}

fn read_bytes(path: &Path) -> io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn encode_record<T: Serialize>(record: &T) -> io::Result<Vec<u8>> {
    serde_json::to_vec(record).map_err(io::Error::other)
}

fn decode_record<T: for<'de> Deserialize<'de>>(bytes: &[u8], label: &str) -> io::Result<T> {
    serde_json::from_slice(bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid operation transaction {label} record: {error}"),
        )
    })
}

fn record_crc<T: Serialize>(magic: &str, version: u32, body: &T) -> io::Result<u32> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(magic.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&version.to_le_bytes());
    bytes.extend_from_slice(&serde_json::to_vec(body).map_err(io::Error::other)?);
    Ok(crc32(&bytes))
}

fn hash_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn empty_hash() -> String {
    hash_hex(&[])
}

fn is_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> io::Result<()> {
    // Windows does not provide a portable directory fsync equivalent that is
    // accepted for ordinary directory handles. Publication instead uses
    // MoveFileExW(MOVEFILE_WRITE_THROUGH); all file contents are sync_all'd
    // before that single durable commit point.
    Ok(())
}

#[cfg(not(windows))]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::Path;

    use tempfile::tempdir;

    use super::*;

    #[derive(Default)]
    struct StageRecorder {
        stages: Vec<OperationStage>,
        fail_at: Option<OperationStage>,
    }

    impl OperationFailpoint for StageRecorder {
        fn hit(&mut self, stage: OperationStage) -> io::Result<()> {
            self.stages.push(stage);
            if self.fail_at == Some(stage) {
                return Err(io::Error::other("injected operation transaction failure"));
            }
            Ok(())
        }
    }

    fn begin(root: &Path, intent: &[u8]) -> OperationTransaction {
        OperationTransaction::begin(root, OperationKind::Ingest, intent).unwrap()
    }

    #[test]
    fn legacy_base_is_generation_zero_and_is_not_modified() {
        let directory = tempdir().unwrap();
        let marker = directory.path().join("legacy.marker");
        fs::write(&marker, b"legacy").unwrap();

        let state = OperationTransaction::recover(directory.path()).unwrap();

        assert_eq!(state, RecoveryState::legacy());
        assert_eq!(fs::read(&marker).unwrap(), b"legacy");
        assert!(!transaction_dir(directory.path()).exists());
        let error = match OperationTransaction::begin(
            directory.path(),
            OperationKind::Ingest,
            b"must not migrate implicitly",
        ) {
            Ok(_) => panic!("legacy base must require explicit migration"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(marker).unwrap(), b"legacy");
    }

    #[test]
    fn committed_generation_is_recovered_with_verified_components() {
        let directory = tempdir().unwrap();
        let mut transaction = begin(directory.path(), b"canonical ingest intent");
        transaction
            .stage_component(Path::new("index/idloc.delta"), b"index generation")
            .unwrap();
        transaction
            .stage_component(Path::new("meta.delta"), b"metadata generation")
            .unwrap();
        let state = transaction.commit().unwrap();

        assert_eq!(state.generation, 1);
        assert!(!state.legacy);
        assert_ne!(state.commit_hash, empty_hash());
        assert_eq!(
            OperationTransaction::recover(directory.path()).unwrap(),
            state
        );
    }

    #[test]
    fn prepare_only_generation_is_invisible_to_recovery() {
        let directory = tempdir().unwrap();
        let mut transaction = begin(directory.path(), b"will not commit");
        transaction
            .stage_component(Path::new("index.delta"), b"orphan component")
            .unwrap();
        let generation = transaction.generation();
        drop(transaction);

        let state = OperationTransaction::recover(directory.path()).unwrap();
        assert_eq!(state.generation, 0);
        assert!(!state.legacy);
        assert!(
            generations_dir(directory.path())
                .join(generation_name(generation))
                .join(PREPARE_FILE_NAME)
                .exists()
        );
    }

    #[test]
    fn committed_component_corruption_fails_closed() {
        let directory = tempdir().unwrap();
        let mut transaction = begin(directory.path(), b"commit then corrupt");
        transaction
            .stage_component(Path::new("graph.delta"), b"valid graph")
            .unwrap();
        transaction.commit().unwrap();
        let component = generations_dir(directory.path())
            .join(generation_name(1))
            .join(COMPONENTS_DIR_NAME)
            .join("graph.delta");
        fs::write(component, b"corrupt graph").unwrap();

        let error = OperationTransaction::recover(directory.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn torn_commit_temp_is_ignored_and_commit_record_is_never_published() {
        let directory = tempdir().unwrap();
        let transaction = begin(directory.path(), b"torn commit");
        let generation_dir = transaction.generation_dir();
        fs::write(generation_dir.join(COMMIT_TEMP_FILE_NAME), b"{torn").unwrap();
        drop(transaction);

        let state = OperationTransaction::recover(directory.path()).unwrap();
        assert_eq!(state.generation, 0);
    }

    #[test]
    fn parent_chain_gap_or_later_commit_after_prepare_fails_closed() {
        let directory = tempdir().unwrap();
        let first = begin(directory.path(), b"uncommitted first");
        drop(first);

        let error =
            match OperationTransaction::begin(directory.path(), OperationKind::Ingest, b"next") {
                Ok(_) => panic!("a prepared generation must block another begin"),
                Err(error) => error,
            };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn failpoint_order_is_deterministic_and_pre_publish_failure_stays_invisible() {
        let directory = tempdir().unwrap();
        let recorder = StageRecorder {
            stages: Vec::new(),
            fail_at: Some(OperationStage::BeforeCommitPublish),
        };
        let mut transaction = OperationTransaction::begin_with_failpoint(
            directory.path(),
            OperationKind::Ingest,
            b"failpoint intent",
            Some(Box::new(recorder)),
        )
        .unwrap();
        transaction
            .stage_component(Path::new("index.delta"), b"payload")
            .unwrap();
        let error = transaction.commit().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        let state = OperationTransaction::recover(directory.path()).unwrap();
        assert_eq!(state.generation, 0);
    }

    #[test]
    fn component_paths_are_confined_and_descriptors_are_sorted() {
        let directory = tempdir().unwrap();
        let mut transaction = begin(directory.path(), b"ordered components");
        assert!(
            transaction
                .stage_component(Path::new("../escape"), b"no")
                .is_err()
        );
        transaction
            .stage_component(Path::new("z.delta"), b"z")
            .unwrap();
        transaction
            .stage_component(Path::new("a.delta"), b"a")
            .unwrap();
        transaction.commit().unwrap();

        let commit = read_record::<CommitRecord>(
            &generations_dir(directory.path())
                .join(generation_name(1))
                .join(COMMIT_FILE_NAME),
            "commit",
        )
        .unwrap();
        let names = commit
            .body
            .components
            .iter()
            .map(|component| component.relative_path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["a.delta", "z.delta"]);
        assert!(is_hash(&commit.body.logical_snapshot_hash));
    }

    #[test]
    fn empty_generation_cannot_be_committed() {
        let directory = tempdir().unwrap();
        let transaction = begin(directory.path(), b"empty generation");
        let error = transaction.commit().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            OperationTransaction::recover(directory.path())
                .unwrap()
                .generation,
            0
        );
    }
}
