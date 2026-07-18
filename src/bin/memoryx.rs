//! MemoryX CLI - Production-ready command-line interface for MemoryX SKF-1.1
//!
//! This CLI provides comprehensive management of MemoryX knowledge bases:
//! - Ingest: Load atoms from JSON/YAML files
//! - Query: Execute natural language queries
//! - Compact: Optimize storage through compaction
//! - Export/Import: Data exchange in multiple formats
//! - Stats: Storage analytics and reporting
//! - Serve: MCP over stdio or HTTP federation server
//!
//! # Usage
//!
//! ```bash
//! # Ingest atoms from a file
//! memoryx ingest --base /path/to/base data.json
//!
//! # Execute a query
//! memoryx query --base /path/to/base "find all CAUSES relations"
//!
//! # Show statistics
//! memoryx stats --base /path/to/base
//!
//! # Start production MCP over stdio
//! memoryx serve --base /path/to/base --stdio
//!
//! # Start HTTP federation server
//! memoryx serve --base /path/to/base --port 8080
//! ```

#![recursion_limit = "256"]

#[cfg(feature = "mcp")]
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use clap::{Parser, Subcommand, ValueEnum};
use colored::*;
use csv::{ReaderBuilder, WriterBuilder};
use fs2::FileExt;
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};

// MemoryX imports
use memoryx::cas::AtomBodyHeader;
use memoryx::cas::claims::ClaimRecord;
use memoryx::ingest::{ExtractorIdentity, IngestExtractor};
use memoryx::prelude::*;
use memoryx::query::{QueryContract, QueryContractCompiler};
#[cfg(feature = "mcp")]
use memoryx::store::api::QueryFilters;
use memoryx::store::api::{BatchAtom, EvidenceRef, MemoryX, StoreConfig, StoreError};
#[cfg(feature = "mcp")]
use memoryx::store::api::{BranchReason, DeleteReason};
use memoryx::vm::ClaimData;

// ============================================================================
// CLI Structure
// ============================================================================

/// MemoryX CLI - Knowledge management system
#[derive(Parser)]
#[command(name = "memoryx")]
#[command(about = "MemoryX CLI - Knowledge management system")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(arg_required_else_help = true)]
struct Cli {
    /// Path to config file (~/.memoryx/config.toml by default)
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// Base storage scope when a full path is not provided
    #[arg(long, global = true, value_enum)]
    base_scope: Option<BaseScope>,

    /// Base name within the selected storage scope
    #[arg(long, global = true)]
    base_name: Option<String>,

    /// Output format (table, json, yaml)
    #[arg(short, long, global = true, default_value = "table")]
    format: OutputFormat,

    /// Suppress color output
    #[arg(long, global = true)]
    no_color: bool,

    /// Enable verbose output
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

/// Output format for CLI results
#[derive(Clone, Copy, Debug, ValueEnum)]
enum OutputFormat {
    /// Human-readable table format
    Table,
    /// JSON format for scripting
    Json,
    /// YAML format
    Yaml,
}

/// Allowed storage scopes for the knowledge base.
#[derive(Clone, Copy, Debug, ValueEnum, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum BaseScope {
    /// Store the base under the current project/work directory
    Project,
    /// Store the base under the user's shared MemoryX directory
    User,
}

/// Available CLI commands
#[derive(Subcommand)]
enum Commands {
    /// Ingest atoms from JSON/YAML files
    Ingest {
        /// MemoryX base path or base name
        #[arg(short = 'd', long)]
        base: Option<PathBuf>,

        /// Input file(s) to ingest
        #[arg(required = true)]
        files: Vec<PathBuf>,

        /// Atom type for ingested atoms
        #[arg(short, long, default_value = "fact")]
        atom_type: String,

        /// Batch size for bulk ingestion
        #[arg(short, long, default_value = "100")]
        batch_size: usize,

        /// Extract candidate claims from plain text instead of ingesting atom JSON/YAML
        #[arg(long)]
        extract_claims: bool,

        /// Preview extracted candidate claims without writing to the database
        #[arg(long)]
        dry_run: bool,

        /// Extractor identity for provenance in dry-run extraction output
        #[arg(long, default_value = "memoryx-deterministic-text-extractor")]
        extractor: String,
    },

    /// Execute query and return results
    Query {
        /// MemoryX base path or base name
        #[arg(short, long)]
        base: Option<PathBuf>,

        /// Query string
        #[arg(required_unless_present = "contract")]
        query: Option<String>,

        /// Execute a strict QueryContract from JSON/YAML file
        #[arg(long)]
        contract: Option<PathBuf>,

        /// Compile the natural query into QueryContract and print it without execution
        #[arg(long)]
        emit_contract: bool,

        /// Include retrieval/action trace in human-readable output
        #[arg(long)]
        include_trace: bool,

        /// Explain candidates rejected by hard QueryContract constraints
        #[arg(long)]
        explain_rejections: bool,

        /// Context policy ID
        #[arg(short = 'p', long, default_value = "0")]
        ctx_policy: u32,

        /// Maximum results to return
        #[arg(short, long)]
        limit: Option<usize>,

        /// Minimum trust level (0-10000)
        #[arg(long)]
        min_trust: Option<u16>,
    },

    /// Run compaction on storage
    Compact {
        /// MemoryX base path or base name
        #[arg(short, long)]
        base: Option<PathBuf>,

        /// Compaction type
        #[arg(short = 't', long, default_value = "all")]
        compaction_type: CompactionType,

        /// Dry run (show what would be done)
        #[arg(long)]
        dry_run: bool,
    },

    /// Export data to various formats
    Export {
        /// MemoryX base path or base name
        #[arg(short, long)]
        base: Option<PathBuf>,

        /// Output format
        #[arg(short, long, default_value = "json")]
        format: ExportFormat,

        /// Output file (stdout if not specified)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Query to filter exported data
        #[arg(short = 'q', long)]
        filter: Option<String>,
    },

    /// Import data from various formats
    Import {
        /// MemoryX base path or base name
        #[arg(short, long)]
        base: Option<PathBuf>,

        /// Input format
        #[arg(short, long, default_value = "json")]
        format: ExportFormat,

        /// Input file
        #[arg(required = true)]
        input: PathBuf,

        /// Skip validation
        #[arg(long)]
        skip_validation: bool,
    },

    /// Show storage statistics
    Stats {
        /// MemoryX base path or base name
        #[arg(short, long)]
        base: Option<PathBuf>,

        /// Show detailed statistics
        #[arg(short, long)]
        detailed: bool,
    },

    /// Verify integrity of all live atoms in the base
    VerifyIntegrity {
        /// MemoryX base path or base name
        #[arg(short, long)]
        base: Option<PathBuf>,
    },

    /// Rebuild lexical indexes from live CAS atom payloads
    RebuildIndex {
        /// MemoryX base path or base name
        #[arg(short, long)]
        base: Option<PathBuf>,

        /// Dry run (verify only; do not rewrite indexes)
        #[arg(long)]
        dry_run: bool,
    },

    /// Run safe repair: verify, rebuild indexes, verify again
    Repair {
        /// MemoryX base path or base name
        #[arg(short, long)]
        base: Option<PathBuf>,
    },

    /// Show recent durable write-operation history
    History {
        /// MemoryX base path or base name
        #[arg(short, long)]
        base: Option<PathBuf>,

        /// Maximum number of newest entries to return
        #[arg(short, long, default_value = "20")]
        limit: usize,
    },

    /// Show current knowledge snapshot identity
    Snapshot {
        /// MemoryX base path or base name
        #[arg(short, long)]
        base: Option<PathBuf>,

        /// Context ID to bind into the snapshot
        #[arg(long, default_value = "0")]
        ctx: u32,
    },

    /// Create an entity from CLI fields or a JSON/YAML form
    CreateEntity {
        /// MemoryX base path or base name
        #[arg(short, long)]
        base: Option<PathBuf>,

        /// Entity canonical name
        #[arg(long)]
        name: Option<String>,

        /// Entity type
        #[arg(long)]
        entity_type: Option<String>,

        /// JSON/YAML form with canonical_name/entity_type/aliases
        #[arg(long)]
        form: Option<PathBuf>,
    },

    /// Add a semi-structured claim to an entity
    AddEntityClaim {
        /// MemoryX base path or base name
        #[arg(short, long)]
        base: Option<PathBuf>,

        /// Subject entity ID
        #[arg(long)]
        entity: u64,

        /// Predicate symbol ID
        #[arg(long)]
        predicate: u32,

        /// Object value as unsigned integer
        #[arg(long)]
        object: u64,

        /// Object tag (U64, I64, BOOL, SYM, REF, NODENUM)
        #[arg(long)]
        object_tag: Option<String>,

        /// Context ID
        #[arg(long, default_value = "0")]
        ctx: u32,
    },

    /// Create an atom-backed relation between two entities
    CreateRelation {
        /// MemoryX base path or base name
        #[arg(short, long)]
        base: Option<PathBuf>,

        /// Subject entity ID
        #[arg(long)]
        subject: u64,

        /// Predicate symbol ID
        #[arg(long)]
        predicate: u32,

        /// Object entity ID
        #[arg(long)]
        object: u64,

        /// Context ID
        #[arg(long, default_value = "0")]
        ctx: u32,
    },

    /// Start MCP stdio transport or HTTP federation server
    Serve {
        /// MemoryX base path or base name
        #[arg(short, long)]
        base: Option<PathBuf>,

        /// Server port
        #[arg(short = 'P', long, default_value = "8080")]
        port: u16,

        /// Server host
        #[arg(short = 'H', long, default_value = "127.0.0.1")]
        host: String,

        /// Enable stdio transport instead of HTTP
        #[arg(long)]
        stdio: bool,
    },

    /// Initialize a new MemoryX base directory
    Init {
        /// MemoryX base path or base name
        #[arg(short, long)]
        base: Option<PathBuf>,

        /// Force initialization even if directory exists
        #[arg(short = 'F', long)]
        force: bool,
    },
}

/// Compaction types
#[derive(Clone, Copy, Debug, ValueEnum)]
enum CompactionType {
    /// Compact all storage components
    All,
    /// Compact CAS segments only
    Cas,
    /// Compact index files only
    Index,
    /// Compact graph store only
    Graph,
    /// Compact metadata only
    Meta,
}

/// Export formats
#[derive(Clone, Copy, Debug, ValueEnum)]
enum ExportFormat {
    /// JSON format
    Json,
    /// YAML format
    Yaml,
    /// NDJSON (newline-delimited JSON)
    Ndjson,
    /// CSV format
    Csv,
}

// ============================================================================
// Configuration
// ============================================================================

/// MemoryX configuration file structure
#[derive(Debug, Serialize, Deserialize, Default)]
struct MemoryXConfig {
    /// Default base directory
    #[serde(default)]
    default_base: Option<PathBuf>,

    /// Default base storage scope when using named bases
    #[serde(default)]
    default_base_scope: Option<BaseScope>,

    /// Default base name within the chosen scope
    #[serde(default)]
    default_base_name: Option<String>,

    /// Default output format
    #[serde(default)]
    default_format: Option<String>,

    /// Server configuration
    #[serde(default)]
    server: ServerConfig,

    /// Ingestion settings
    #[serde(default)]
    ingest: IngestConfig,

    /// Query settings
    #[serde(default)]
    query: QueryConfig,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct ServerConfig {
    #[serde(default = "default_host")]
    host: String,
    #[serde(default = "default_port")]
    port: u16,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct IngestConfig {
    #[serde(default = "default_batch_size")]
    batch_size: usize,
    #[serde(default = "default_atom_type")]
    default_atom_type: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct QueryConfig {
    #[serde(default = "default_min_trust")]
    min_trust: u16,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_batch_size() -> usize {
    100
}

fn default_atom_type() -> String {
    "fact".to_string()
}

fn default_min_trust() -> u16 {
    0
}

fn default_limit() -> usize {
    100
}

// ============================================================================
// Data Structures for Import/Export
// ============================================================================

/// Atom representation for import/export
#[derive(Debug, Serialize, Deserialize)]
struct AtomExport {
    /// Atom ID (hex-encoded BLAKE3 hash)
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,

    /// Atom type
    atom_type: String,

    /// Claims in the atom
    claims: Vec<ClaimExport>,

    /// Evidence references
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    evidence: Vec<EvidenceExport>,

    /// Metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<AtomMetadataExport>,

    /// Payload content (base64 encoded)
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<String>,
}

/// Claim representation for import/export
#[derive(Debug, Serialize, Deserialize)]
struct ClaimExport {
    subject: u64,
    predicate: u64,
    object_tag: u8,
    object_value: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    qualifiers: Option<HashMap<String, serde_json::Value>>,
}

impl From<&ClaimData> for ClaimExport {
    fn from(claim: &ClaimData) -> Self {
        ClaimExport {
            subject: claim.subj,
            predicate: claim.pred,
            object_tag: claim.obj_tag,
            object_value: claim.obj_val,
            qualifiers: None,
        }
    }
}

impl From<ClaimExport> for ClaimData {
    fn from(claim: ClaimExport) -> Self {
        ClaimData {
            subj: claim.subject,
            pred: claim.predicate,
            obj_tag: claim.object_tag,
            obj_val: claim.object_value,
            qualifiers_mask: 0,
        }
    }
}

/// Evidence representation for import/export
#[derive(Debug, Serialize, Deserialize)]
struct EvidenceExport {
    atom_id: String,
    section_kind: String,
    offset: u64,
    length: u64,
    trust: u16,
}

/// CSV row representation for import/export.
///
/// The row carries one claim and atom-level fields that can be repeated across
/// rows with the same `id`. This keeps CSV readable while still allowing
/// faithful reconstruction into `AtomExport`.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
struct CsvAtomRow {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    atom_type: Option<String>,
    #[serde(default)]
    subject: Option<u64>,
    #[serde(default)]
    predicate: Option<u64>,
    #[serde(default)]
    object_tag: Option<String>,
    #[serde(default)]
    object_value: Option<u64>,
    #[serde(default)]
    qualifiers_json: Option<String>,
    #[serde(default)]
    created_at: Option<u64>,
    #[serde(default)]
    trust_level: Option<u16>,
    #[serde(default)]
    domain_mask: Option<u64>,
    #[serde(default)]
    source_id: Option<u32>,
    #[serde(default)]
    payload: Option<String>,
    #[serde(default)]
    evidence_json: Option<String>,
    #[serde(default)]
    evidence_atom_id: Option<String>,
    #[serde(default)]
    evidence_section_kind: Option<String>,
    #[serde(default)]
    evidence_offset: Option<u64>,
    #[serde(default)]
    evidence_length: Option<u64>,
    #[serde(default)]
    evidence_trust: Option<u16>,
}

/// Metadata representation for import/export
#[derive(Debug, Serialize, Deserialize)]
struct AtomMetadataExport {
    created_at: u64,
    trust_level: u16,
    domain_mask: u64,
    source_id: u32,
}

/// Statistics representation
#[derive(Debug, Serialize)]
struct StorageStats {
    total_atoms: usize,
    total_claims: usize,
    atom_types: HashMap<String, usize>,
    storage_size_bytes: u64,
    index_size_bytes: u64,
    graph_edges: usize,
    contexts: usize,
    conflicts: usize,
}

// ============================================================================
// Error Handling
// ============================================================================

/// CLI-specific error type
#[derive(Debug)]
enum CliError {
    Io(std::io::Error),
    Store(String),
    Config(String),
    Parse(String),
    Validation(String),
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::Io(e) => write!(f, "IO error: {}", e),
            CliError::Store(e) => write!(f, "Store error: {}", e),
            CliError::Config(e) => write!(f, "Config error: {}", e),
            CliError::Parse(e) => write!(f, "Parse error: {}", e),
            CliError::Validation(e) => write!(f, "Validation error: {}", e),
        }
    }
}

impl std::error::Error for CliError {}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        CliError::Io(e)
    }
}

impl From<serde_json::Error> for CliError {
    fn from(e: serde_json::Error) -> Self {
        CliError::Parse(e.to_string())
    }
}

impl From<serde_yaml::Error> for CliError {
    fn from(e: serde_yaml::Error) -> Self {
        CliError::Parse(e.to_string())
    }
}

impl From<csv::Error> for CliError {
    fn from(e: csv::Error) -> Self {
        CliError::Parse(e.to_string())
    }
}

impl From<StoreError> for CliError {
    fn from(e: StoreError) -> Self {
        CliError::Store(e.to_string())
    }
}

impl From<walkdir::Error> for CliError {
    fn from(e: walkdir::Error) -> Self {
        CliError::Io(e.into())
    }
}

type CliResult<T> = Result<T, CliError>;

// ============================================================================
// Utility Functions
// ============================================================================

/// Load configuration from file
fn load_config(config_path: Option<&Path>) -> CliResult<MemoryXConfig> {
    let path = match config_path {
        Some(p) => p.to_path_buf(),
        None => {
            let home = dirs::home_dir().ok_or_else(|| {
                CliError::Config("Could not determine home directory".to_string())
            })?;
            home.join(".memoryx").join("config.toml")
        }
    };

    if !path.exists() {
        return Ok(MemoryXConfig::default());
    }

    let content = std::fs::read_to_string(&path)?;
    let config: MemoryXConfig = toml::from_str(&content)
        .map_err(|e| CliError::Config(format!("Failed to parse config: {}", e)))?;

    Ok(config)
}

/// Save configuration to file
#[allow(dead_code)]
fn save_config(config: &MemoryXConfig, config_path: Option<&Path>) -> CliResult<()> {
    let path = match config_path {
        Some(p) => p.to_path_buf(),
        None => {
            let home = dirs::home_dir().ok_or_else(|| {
                CliError::Config("Could not determine home directory".to_string())
            })?;
            let config_dir = home.join(".memoryx");
            std::fs::create_dir_all(&config_dir)?;
            config_dir.join("config.toml")
        }
    };

    let content = toml::to_string_pretty(config)
        .map_err(|e| CliError::Config(format!("Failed to serialize config: {}", e)))?;

    std::fs::write(&path, content)?;
    Ok(())
}

/// Format atom ID as hex string
fn atom_id_to_hex(atom_id: &AtomId) -> String {
    hex::encode(atom_id)
}

/// Parse atom ID from hex string
fn hex_to_atom_id(hex: &str) -> CliResult<AtomId> {
    let bytes =
        hex::decode(hex).map_err(|e| CliError::Validation(format!("Invalid hex: {}", e)))?;
    if bytes.len() != 32 {
        return Err(CliError::Validation("Atom ID must be 32 bytes".to_string()));
    }
    let mut atom_id = [0u8; 32];
    atom_id.copy_from_slice(&bytes);
    Ok(atom_id)
}

/// Parse atom type from string
fn parse_atom_type(s: &str) -> CliResult<AtomType> {
    match s.to_lowercase().as_str() {
        "definition" => Ok(AtomType::DEFINITION),
        "fact" => Ok(AtomType::FACT),
        "rule" => Ok(AtomType::RULE),
        "procedure" => Ok(AtomType::PROCEDURE),
        "observation" => Ok(AtomType::OBSERVATION),
        "hypothesis" => Ok(AtomType::HYPOTHESIS),
        "example" => Ok(AtomType::EXAMPLE),
        "counterexample" => Ok(AtomType::COUNTEREXAMPLE),
        "dataset" => Ok(AtomType::DATASET),
        "measurement" => Ok(AtomType::MEASUREMENT),
        "decision" => Ok(AtomType::DECISION),
        "conflict" => Ok(AtomType::CONFLICT),
        "map" => Ok(AtomType::MAP),
        _ => Err(CliError::Validation(format!("Unknown atom type: {}", s))),
    }
}

fn atom_type_to_label(atom_type: AtomType) -> String {
    format!("{:?}", atom_type)
}

fn section_kind_label(section_kind: SectionKind) -> &'static str {
    match section_kind {
        SectionKind::SYMBOLS => "SYMBOLS",
        SectionKind::REFS => "REFS",
        SectionKind::CLAIMS => "CLAIMS",
        SectionKind::INVARIANTS => "INVARIANTS",
        SectionKind::EDGES => "EDGES",
        SectionKind::EVIDENCE => "EVIDENCE",
        SectionKind::META => "META",
    }
}

fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn parse_section_kind(raw: &str) -> CliResult<SectionKind> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(CliError::Validation(
            "Section kind cannot be empty".to_string(),
        ));
    }

    if let Ok(value) = trimmed.parse::<u32>() {
        return SectionKind::from_u32(value)
            .ok_or_else(|| CliError::Validation(format!("Unsupported section kind: {}", value)));
    }

    match trimmed.to_ascii_uppercase().as_str() {
        "SYMBOLS" => Ok(SectionKind::SYMBOLS),
        "REFS" => Ok(SectionKind::REFS),
        "CLAIMS" => Ok(SectionKind::CLAIMS),
        "INVARIANTS" => Ok(SectionKind::INVARIANTS),
        "EDGES" => Ok(SectionKind::EDGES),
        "EVIDENCE" => Ok(SectionKind::EVIDENCE),
        "META" => Ok(SectionKind::META),
        other => Err(CliError::Validation(format!(
            "Unsupported section kind: {}",
            other
        ))),
    }
}

fn parse_object_tag(raw: Option<&str>) -> CliResult<u8> {
    let Some(raw) = raw else {
        return Ok(ObjTag::U64.to_u8());
    };

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(ObjTag::U64.to_u8());
    }

    if let Ok(value) = trimmed.parse::<u8>() {
        return ObjTag::from_u8(value)
            .map(ObjTag::to_u8)
            .ok_or_else(|| CliError::Validation(format!("Unsupported object tag: {}", value)));
    }

    match trimmed.to_ascii_uppercase().as_str() {
        "NULL" => Ok(ObjTag::NULL.to_u8()),
        "BOOL" => Ok(ObjTag::BOOL.to_u8()),
        "I64" => Ok(ObjTag::I64.to_u8()),
        "U64" => Ok(ObjTag::U64.to_u8()),
        "F64" => Ok(ObjTag::F64.to_u8()),
        "BYTES" => Ok(ObjTag::BYTES.to_u8()),
        "SYM" => Ok(ObjTag::SYM.to_u8()),
        "REF" => Ok(ObjTag::REF.to_u8()),
        "NODENUM" => Ok(ObjTag::NODENUM.to_u8()),
        other => Err(CliError::Validation(format!(
            "Unsupported object tag: {}",
            other
        ))),
    }
}

fn parse_qualifiers_json(
    raw: Option<String>,
) -> CliResult<Option<HashMap<String, serde_json::Value>>> {
    let Some(raw) = normalize_optional_string(raw) else {
        return Ok(None);
    };

    let qualifiers: HashMap<String, serde_json::Value> = serde_json::from_str(&raw)
        .map_err(|e| CliError::Validation(format!("Invalid qualifiers JSON: {}", e)))?;
    Ok(Some(qualifiers))
}

fn parse_evidence_json(raw: Option<String>) -> CliResult<Vec<EvidenceExport>> {
    let Some(raw) = normalize_optional_string(raw) else {
        return Ok(Vec::new());
    };

    serde_json::from_str::<Vec<EvidenceExport>>(&raw)
        .map_err(|e| CliError::Validation(format!("Invalid evidence JSON: {}", e)))
}

fn decode_payload_bytes(raw: Option<String>) -> CliResult<Option<Vec<u8>>> {
    let Some(raw) = normalize_optional_string(raw) else {
        return Ok(None);
    };

    let bytes = BASE64_STANDARD
        .decode(raw.as_bytes())
        .map_err(|e| CliError::Validation(format!("Invalid base64 payload: {}", e)))?;
    Ok(Some(bytes))
}

fn parse_csv_rows(content: &str) -> CliResult<Vec<AtomExport>> {
    let mut reader = ReaderBuilder::new()
        .trim(csv::Trim::All)
        .flexible(true)
        .from_reader(content.as_bytes());

    let mut order: Vec<String> = Vec::new();
    let mut grouped: HashMap<String, AtomCsvGroup> = HashMap::new();

    for (index, row_result) in reader.deserialize::<CsvAtomRow>().enumerate() {
        let row = row_result?;
        let atom_type_raw = normalize_optional_string(row.atom_type.clone()).ok_or_else(|| {
            CliError::Validation(format!("CSV row {} is missing atom_type", index))
        })?;
        let atom_type = parse_atom_type(&atom_type_raw)?;
        let subject = row
            .subject
            .ok_or_else(|| CliError::Validation(format!("CSV row {} is missing subject", index)))?;
        let predicate = row.predicate.ok_or_else(|| {
            CliError::Validation(format!("CSV row {} is missing predicate", index))
        })?;
        let object_value = row.object_value.ok_or_else(|| {
            CliError::Validation(format!("CSV row {} is missing object_value", index))
        })?;
        let object_tag = parse_object_tag(row.object_tag.as_deref())?;
        let qualifiers = parse_qualifiers_json(row.qualifiers_json.clone())?;
        let evidence_json = parse_evidence_json(row.evidence_json.clone())?;

        let mut evidence = evidence_json;
        if let Some(atom_id) = normalize_optional_string(row.evidence_atom_id.clone()) {
            let section_kind =
                parse_section_kind(row.evidence_section_kind.as_deref().unwrap_or("EVIDENCE"))?;
            let offset = row.evidence_offset.ok_or_else(|| {
                CliError::Validation(format!("CSV row {} is missing evidence_offset", index))
            })?;
            let length = row.evidence_length.ok_or_else(|| {
                CliError::Validation(format!("CSV row {} is missing evidence_length", index))
            })?;
            let trust = row.evidence_trust.ok_or_else(|| {
                CliError::Validation(format!("CSV row {} is missing evidence_trust", index))
            })?;
            evidence.push(EvidenceExport {
                atom_id,
                section_kind: section_kind_label(section_kind).to_string(),
                offset,
                length,
                trust,
            });
        }

        let claim = ClaimExport {
            subject,
            predicate,
            object_tag,
            object_value,
            qualifiers,
        };

        let key =
            normalize_optional_string(row.id.clone()).unwrap_or_else(|| format!("__row_{}", index));

        let entry = grouped.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            AtomCsvGroup {
                id: normalize_optional_string(row.id.clone()),
                atom_type: atom_type_to_label(atom_type),
                claims: Vec::new(),
                evidence: Vec::new(),
                metadata: None,
                payload: None,
            }
        });

        if entry.atom_type != atom_type_to_label(atom_type) {
            return Err(CliError::Validation(format!(
                "CSV row {} conflicts with atom_type '{}' in its atom group",
                index, entry.atom_type
            )));
        }

        if let Some(existing) = &entry.payload {
            if let Some(current) = normalize_optional_string(row.payload.clone())
                && existing != &current
            {
                return Err(CliError::Validation(format!(
                    "CSV row {} has conflicting payload for atom group",
                    index
                )));
            }
        } else {
            entry.payload = normalize_optional_string(row.payload.clone());
        }

        let row_metadata = match (
            row.created_at,
            row.trust_level,
            row.domain_mask,
            row.source_id,
        ) {
            (None, None, None, None) => None,
            (Some(created_at), Some(trust_level), Some(domain_mask), Some(source_id)) => {
                Some(AtomMetadataExport {
                    created_at,
                    trust_level,
                    domain_mask,
                    source_id,
                })
            }
            _ => {
                return Err(CliError::Validation(format!(
                    "CSV row {} has incomplete metadata fields",
                    index
                )));
            }
        };

        if let Some(metadata) = row_metadata {
            match &entry.metadata {
                Some(existing) => {
                    if existing.created_at != metadata.created_at
                        || existing.trust_level != metadata.trust_level
                        || existing.domain_mask != metadata.domain_mask
                        || existing.source_id != metadata.source_id
                    {
                        return Err(CliError::Validation(format!(
                            "CSV row {} has conflicting metadata for atom group",
                            index
                        )));
                    }
                }
                None => entry.metadata = Some(metadata),
            }
        }

        if !entry.claims.iter().any(|existing| {
            existing.subject == claim.subject
                && existing.predicate == claim.predicate
                && existing.object_tag == claim.object_tag
                && existing.object_value == claim.object_value
                && existing.qualifiers == claim.qualifiers
        }) {
            entry.claims.push(claim);
        }

        for evidence_ref in evidence {
            if !entry.evidence.iter().any(|existing| {
                existing.atom_id == evidence_ref.atom_id
                    && existing.section_kind == evidence_ref.section_kind
                    && existing.offset == evidence_ref.offset
                    && existing.length == evidence_ref.length
                    && existing.trust == evidence_ref.trust
            }) {
                entry.evidence.push(evidence_ref);
            }
        }
    }

    let mut atoms = Vec::with_capacity(order.len());
    for key in order {
        let entry = grouped.remove(&key).ok_or_else(|| {
            CliError::Validation(format!(
                "CSV atom group '{}' disappeared during parsing",
                key
            ))
        })?;
        atoms.push(AtomExport {
            id: entry.id,
            atom_type: entry.atom_type,
            claims: entry.claims,
            evidence: entry.evidence,
            metadata: entry.metadata,
            payload: entry.payload,
        });
    }

    Ok(atoms)
}

fn write_csv_atoms(atoms: &[AtomExport]) -> CliResult<String> {
    let mut writer = WriterBuilder::new().has_headers(true).from_writer(vec![]);

    for atom in atoms {
        let base_metadata = atom.metadata.as_ref();
        let evidence_json = if atom.evidence.is_empty() {
            None
        } else {
            Some(
                serde_json::to_string(&atom.evidence)
                    .map_err(|e| CliError::Parse(e.to_string()))?,
            )
        };

        if atom.claims.is_empty() {
            writer.serialize(CsvAtomRow {
                id: atom.id.clone(),
                atom_type: Some(atom.atom_type.clone()),
                subject: None,
                predicate: None,
                object_tag: None,
                object_value: None,
                qualifiers_json: None,
                created_at: base_metadata.map(|metadata| metadata.created_at),
                trust_level: base_metadata.map(|metadata| metadata.trust_level),
                domain_mask: base_metadata.map(|metadata| metadata.domain_mask),
                source_id: base_metadata.map(|metadata| metadata.source_id),
                payload: atom.payload.clone(),
                evidence_json: evidence_json.clone(),
                evidence_atom_id: None,
                evidence_section_kind: None,
                evidence_offset: None,
                evidence_length: None,
                evidence_trust: None,
            })?;
            continue;
        }

        for claim in &atom.claims {
            writer.serialize(CsvAtomRow {
                id: atom.id.clone(),
                atom_type: Some(atom.atom_type.clone()),
                subject: Some(claim.subject),
                predicate: Some(claim.predicate),
                object_tag: Some(claim.object_tag.to_string()),
                object_value: Some(claim.object_value),
                qualifiers_json: claim
                    .qualifiers
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(|e| CliError::Parse(e.to_string()))?,
                created_at: base_metadata.map(|metadata| metadata.created_at),
                trust_level: base_metadata.map(|metadata| metadata.trust_level),
                domain_mask: base_metadata.map(|metadata| metadata.domain_mask),
                source_id: base_metadata.map(|metadata| metadata.source_id),
                payload: atom.payload.clone(),
                evidence_json: evidence_json.clone(),
                evidence_atom_id: None,
                evidence_section_kind: None,
                evidence_offset: None,
                evidence_length: None,
                evidence_trust: None,
            })?;
        }
    }

    let bytes = writer
        .into_inner()
        .map_err(|e| CliError::Parse(e.error().to_string()))?;
    String::from_utf8(bytes).map_err(|e| CliError::Parse(e.to_string()))
}

fn parse_atom_exports_from_content(
    format: ExportFormat,
    content: &str,
) -> CliResult<Vec<AtomExport>> {
    match format {
        ExportFormat::Json | ExportFormat::Ndjson => {
            if content.trim().starts_with('[') {
                serde_json::from_str(content).map_err(Into::into)
            } else {
                content
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(serde_json::from_str)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(Into::into)
            }
        }
        ExportFormat::Yaml => serde_yaml::from_str(content).map_err(Into::into),
        ExportFormat::Csv => parse_csv_rows(content),
    }
}

fn build_batch_atoms_from_exports(
    atoms: Vec<AtomExport>,
    skip_validation: bool,
) -> CliResult<Vec<BatchAtom>> {
    atoms
        .into_iter()
        .map(|atom| {
            let atom_type = parse_atom_type(&atom.atom_type)?;
            let claims: Vec<ClaimData> = atom.claims.into_iter().map(Into::into).collect();

            let evidence: Vec<EvidenceRef> = atom
                .evidence
                .into_iter()
                .map(|e| {
                    Ok(EvidenceRef {
                        atom_id: hex_to_atom_id(&e.atom_id)?,
                        section_kind: parse_section_kind(&e.section_kind)?,
                        offset: e.offset,
                        length: e.length,
                        trust: e.trust,
                    })
                })
                .collect::<CliResult<Vec<_>>>()?;

            let payload = match decode_payload_bytes(atom.payload)? {
                Some(payload) => payload,
                None => create_minimal_atom_body(atom_type, &claims),
            };

            if payload.len() < AtomBodyHeader::SIZE {
                return Err(CliError::Validation(format!(
                    "Payload for atom type {:?} is too small: {} bytes",
                    atom_type,
                    payload.len()
                )));
            }

            if !skip_validation && claims.is_empty() {
                return Err(CliError::Validation(
                    "Imported atom must contain at least one claim".to_string(),
                ));
            }

            Ok(BatchAtom::new(payload, atom_type, claims, evidence))
        })
        .collect()
}

fn atom_matches_export_filter(atom: &AtomExport, filter: Option<&str>) -> bool {
    let Some(filter) = filter
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    else {
        return true;
    };

    let filter = filter.to_ascii_lowercase();

    if atom
        .id
        .as_deref()
        .map(|id| id.to_ascii_lowercase().contains(&filter))
        .unwrap_or(false)
    {
        return true;
    }

    if atom.atom_type.to_ascii_lowercase().contains(&filter) {
        return true;
    }

    if atom.claims.iter().any(|claim| {
        format!(
            "{}:{}:{}:{}",
            claim.subject, claim.predicate, claim.object_tag, claim.object_value
        )
        .to_ascii_lowercase()
        .contains(&filter)
    }) {
        return true;
    }

    if atom.evidence.iter().any(|evidence| {
        format!(
            "{}:{}:{}:{}:{}",
            evidence.atom_id,
            evidence.section_kind,
            evidence.offset,
            evidence.length,
            evidence.trust
        )
        .to_ascii_lowercase()
        .contains(&filter)
    }) {
        return true;
    }

    atom.metadata
        .as_ref()
        .map(|metadata| {
            format!(
                "{}:{}:{}:{}",
                metadata.created_at, metadata.trust_level, metadata.domain_mask, metadata.source_id
            )
            .to_ascii_lowercase()
            .contains(&filter)
        })
        .unwrap_or(false)
}

fn atom_export_from_store(store: &MemoryX, atom_id: &AtomId) -> CliResult<AtomExport> {
    use memoryx::cas::claims::ClaimsSection;

    let payload = store.get_atom_payload(atom_id).map_err(|e| {
        CliError::Store(format!(
            "Failed to load payload for atom {}: {}",
            atom_id_to_hex(atom_id),
            e
        ))
    })?;
    let body_header = AtomBodyHeader::from_bytes(&payload).map_err(|e| {
        CliError::Validation(format!(
            "Invalid atom payload for {}: {}",
            atom_id_to_hex(atom_id),
            e
        ))
    })?;
    let atom_type = body_header.atom_type().ok_or_else(|| {
        CliError::Validation(format!(
            "Payload for atom {} has an unsupported atom_type",
            atom_id_to_hex(atom_id)
        ))
    })?;

    let claim_to_export = |record: &ClaimRecord| -> CliResult<ClaimExport> {
        let object_value = match record.object_tag {
            ObjTag::NULL => 0,
            ObjTag::BOOL => record.object_value.first().copied().unwrap_or(0) as u64,
            ObjTag::I64 => {
                if record.object_value.len() != 8 {
                    return Err(CliError::Validation(
                        "Invalid I64 claim payload length".to_string(),
                    ));
                }
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(&record.object_value[..8]);
                i64::from_le_bytes(bytes) as u64
            }
            ObjTag::U64 => {
                if record.object_value.len() != 8 {
                    return Err(CliError::Validation(
                        "Invalid U64 claim payload length".to_string(),
                    ));
                }
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(&record.object_value[..8]);
                u64::from_le_bytes(bytes)
            }
            ObjTag::F64 => {
                if record.object_value.len() != 8 {
                    return Err(CliError::Validation(
                        "Invalid F64 claim payload length".to_string(),
                    ));
                }
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(&record.object_value[..8]);
                f64::from_le_bytes(bytes).to_bits()
            }
            ObjTag::BYTES => {
                if record.object_value.len() < 4 {
                    return Err(CliError::Validation(
                        "Invalid BYTES claim payload length".to_string(),
                    ));
                }
                let mut bytes = [0u8; 4];
                bytes.copy_from_slice(&record.object_value[..4]);
                u32::from_le_bytes(bytes) as u64
            }
            ObjTag::SYM => {
                if record.object_value.len() != 4 {
                    return Err(CliError::Validation(
                        "Invalid SYM claim payload length".to_string(),
                    ));
                }
                let mut bytes = [0u8; 4];
                bytes.copy_from_slice(&record.object_value[..4]);
                u32::from_le_bytes(bytes) as u64
            }
            ObjTag::REF => {
                if record.object_value.len() != 32 {
                    return Err(CliError::Validation(
                        "Invalid REF claim payload length".to_string(),
                    ));
                }
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(&record.object_value[..8]);
                u64::from_le_bytes(bytes)
            }
            ObjTag::NODENUM => {
                if record.object_value.len() != 8 {
                    return Err(CliError::Validation(
                        "Invalid NODENUM claim payload length".to_string(),
                    ));
                }
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(&record.object_value[..8]);
                u64::from_le_bytes(bytes)
            }
        };

        Ok(ClaimExport {
            subject: record.subject_local,
            predicate: record.predicate_local as u64,
            object_tag: record.object_tag.to_u8(),
            object_value,
            qualifiers: None,
        })
    };

    let section_table_start = body_header.section_table_off as usize;
    let section_count = body_header.section_count as usize;
    let section_table_bytes = section_count.saturating_mul(memoryx::cas::SectionDesc::SIZE);
    if section_table_start + section_table_bytes > payload.len() {
        return Err(CliError::Validation(format!(
            "Invalid atom payload for {}: section table exceeds payload length",
            atom_id_to_hex(atom_id)
        )));
    }

    let mut claims = Vec::new();
    let mut metadata = AtomMetadataExport {
        created_at: body_header.created_at_unix_ns,
        trust_level: 5000,
        domain_mask: 0xFFFF,
        source_id: 0,
    };

    for i in 0..section_count {
        let section_offset = section_table_start + i * memoryx::cas::SectionDesc::SIZE;
        let section_desc = memoryx::cas::SectionDesc::from_bytes(&payload[section_offset..])
            .map_err(|e| {
                CliError::Validation(format!(
                    "Invalid section descriptor in atom {}: {}",
                    atom_id_to_hex(atom_id),
                    e
                ))
            })?;
        let section_kind = section_desc.kind().ok_or_else(|| {
            CliError::Validation(format!(
                "Atom {} contains an unsupported section kind",
                atom_id_to_hex(atom_id)
            ))
        })?;
        let sec_start = section_desc.off as usize;
        let sec_len = section_desc.len as usize;
        if sec_start + sec_len > payload.len() {
            continue;
        }

        match section_kind {
            memoryx::cas::SectionKind::CLAIMS => {
                let claims_section = ClaimsSection::from_bytes(
                    &payload[sec_start..sec_start + sec_len],
                )
                .map_err(|e| {
                    CliError::Validation(format!(
                        "Invalid claims section in atom {}: {}",
                        atom_id_to_hex(atom_id),
                        e
                    ))
                })?;
                claims = claims_section
                    .claims
                    .iter()
                    .map(claim_to_export)
                    .collect::<CliResult<Vec<_>>>()?;
            }
            memoryx::cas::SectionKind::META if sec_len >= 14 => {
                metadata.trust_level =
                    u16::from_le_bytes([payload[sec_start], payload[sec_start + 1]]);
                metadata.domain_mask = u64::from_le_bytes([
                    payload[sec_start + 2],
                    payload[sec_start + 3],
                    payload[sec_start + 4],
                    payload[sec_start + 5],
                    payload[sec_start + 6],
                    payload[sec_start + 7],
                    payload[sec_start + 8],
                    payload[sec_start + 9],
                ]);
                metadata.source_id = u32::from_le_bytes([
                    payload[sec_start + 10],
                    payload[sec_start + 11],
                    payload[sec_start + 12],
                    payload[sec_start + 13],
                ]);
            }
            _ => {}
        }
    }

    let evidence_refs = store.get_provenance_legacy(atom_id).map_err(|e| {
        CliError::Store(format!(
            "Failed to load provenance for atom {}: {}",
            atom_id_to_hex(atom_id),
            e
        ))
    })?;

    Ok(AtomExport {
        id: Some(atom_id_to_hex(atom_id)),
        atom_type: atom_type_to_label(atom_type),
        claims,
        evidence: evidence_refs
            .into_iter()
            .map(|e| EvidenceExport {
                atom_id: atom_id_to_hex(&e.atom_id),
                section_kind: section_kind_label(e.section_kind).to_string(),
                offset: e.offset,
                length: e.length,
                trust: e.trust,
            })
            .collect(),
        metadata: Some(metadata),
        payload: Some(BASE64_STANDARD.encode(payload)),
    })
}

#[derive(Debug, Default)]
struct AtomCsvGroup {
    id: Option<String>,
    atom_type: String,
    claims: Vec<ClaimExport>,
    evidence: Vec<EvidenceExport>,
    metadata: Option<AtomMetadataExport>,
    payload: Option<String>,
}

/// Create a progress bar
fn create_progress_bar(total: u64, message: &str) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("#>-"),
    );
    pb.set_message(message.to_string());
    pb
}

/// Print success message
fn print_success(message: &str) {
    println!("{} {}", "✓".green().bold(), message);
}

/// Print error message
fn print_error(message: &str) {
    eprintln!("{} {}", "✗".red().bold(), message);
}

/// Print info message
fn print_info(message: &str) {
    println!("{} {}", "ℹ".blue(), message);
}

/// Print warning message
fn print_warning(message: &str) {
    println!("{} {}", "⚠".yellow(), message);
}

#[cfg(feature = "mcp")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagnosticSink {
    Stdout,
    Stderr,
}

#[cfg(feature = "mcp")]
#[derive(Clone, Debug, Serialize)]
struct McpBaseInfo {
    base_ref: String,
    scope: String,
    name: String,
    path: PathBuf,
    connected: bool,
    active: bool,
}

#[cfg(feature = "mcp")]
struct McpServerState {
    active_base_ref: String,
    bases: BTreeMap<String, McpBaseInfo>,
    stores: HashMap<String, MemoryX>,
}

#[cfg(feature = "mcp")]
impl McpServerState {
    fn new(_active_path: PathBuf, store: MemoryX) -> CliResult<Self> {
        let active_path = store.config().root_path.clone();
        let active_name = active_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("active")
            .to_string();
        let active_info = McpBaseInfo {
            base_ref: "active".to_string(),
            scope: infer_base_scope_label(&active_path),
            name: active_name,
            path: active_path,
            connected: true,
            active: true,
        };

        let mut bases = BTreeMap::new();
        bases.insert(active_info.base_ref.clone(), active_info);

        let mut stores = HashMap::new();
        stores.insert("active".to_string(), store);

        Ok(Self {
            active_base_ref: "active".to_string(),
            bases,
            stores,
        })
    }

    fn active_base(&self) -> Option<&McpBaseInfo> {
        self.bases.get(&self.active_base_ref)
    }

    fn base_for_args(
        &mut self,
        args: Option<&serde_json::Value>,
    ) -> Result<(String, &mut MemoryX), String> {
        let requested = args
            .and_then(|value| value.as_object())
            .and_then(|args| args.get("base_ref"))
            .and_then(|value| value.as_str())
            .unwrap_or(&self.active_base_ref)
            .to_string();

        self.store_for_ref(&requested)
    }

    fn store_for_ref(&mut self, base_ref: &str) -> Result<(String, &mut MemoryX), String> {
        if !self.stores.contains_key(base_ref) {
            let info = self
                .bases
                .get(base_ref)
                .ok_or_else(|| format!("Unknown base_ref '{}'", base_ref))?
                .clone();
            let canonical_path = canonical_base_identity(&info.path)?;
            if let Some(existing_ref) =
                self.connected_ref_for_path(&canonical_path, Some(base_ref))?
            {
                return Err(format!(
                    "Base '{}' resolves to a physical root already connected as '{}'; use the existing base_ref",
                    base_ref, existing_ref
                ));
            }
            let store = MemoryX::new(StoreConfig::new(info.path.clone()))
                .map_err(|err| format!("Failed to open base '{}': {}", base_ref, err))?;
            let canonical_path = store.config().root_path.clone();
            self.stores.insert(base_ref.to_string(), store);
            if let Some(base) = self.bases.get_mut(base_ref) {
                base.connected = true;
                base.path = canonical_path;
            }
        }

        self.stores
            .get_mut(base_ref)
            .map(|store| (base_ref.to_string(), store))
            .ok_or_else(|| format!("Failed to access base_ref '{}'", base_ref))
    }

    fn connect_base(
        &mut self,
        base_ref: Option<&str>,
        scope: Option<BaseScope>,
        name: Option<&str>,
        path: Option<&str>,
    ) -> Result<McpBaseInfo, String> {
        let path = if let Some(path) = path {
            validate_allowed_base_path(Path::new(path)).map_err(|err| err.to_string())?
        } else {
            let scope = scope.unwrap_or(BaseScope::Project);
            let name = name.unwrap_or("default");
            validate_allowed_base_path(
                &scoped_base_path(scope, name).map_err(|err| err.to_string())?,
            )
            .map_err(|err| err.to_string())?
        };

        let name = name
            .map(str::to_string)
            .or_else(|| {
                path.file_name()
                    .and_then(|value| value.to_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "base".to_string());
        let inferred_scope = scope
            .map(|scope| scope.as_str().to_string())
            .unwrap_or_else(|| infer_base_scope_label(&path));
        let base_ref = base_ref
            .map(str::to_string)
            .unwrap_or_else(|| format!("{}:{}", inferred_scope, name));

        let canonical_candidate = canonical_base_identity(&path)?;
        if self.stores.contains_key(&base_ref) {
            let existing = self.bases.get(&base_ref).ok_or_else(|| {
                format!("Connected base_ref '{}' has no registry entry", base_ref)
            })?;
            let canonical_existing = canonical_base_identity(&existing.path)?;
            if canonical_existing == canonical_candidate {
                return Ok(existing.clone());
            }
            return Err(format!(
                "base_ref '{}' is already connected to '{}' and cannot be rebound to '{}'",
                base_ref,
                existing.path.display(),
                path.display()
            ));
        }
        if let Some(existing_ref) =
            self.connected_ref_for_path(&canonical_candidate, Some(&base_ref))?
        {
            return Err(format!(
                "Physical base '{}' is already connected as '{}'; reuse that base_ref instead of creating alias '{}'",
                canonical_candidate.display(),
                existing_ref,
                base_ref
            ));
        }

        let store = MemoryX::new(StoreConfig::new(path.clone()))
            .map_err(|err| format!("Failed to open base '{}': {}", base_ref, err))?;
        let canonical_path = store.config().root_path.clone();
        let info = McpBaseInfo {
            base_ref: base_ref.clone(),
            scope: inferred_scope,
            name,
            path: canonical_path,
            connected: true,
            active: base_ref == self.active_base_ref,
        };
        self.stores.insert(base_ref.clone(), store);
        self.bases.insert(base_ref, info.clone());
        self.refresh_active_markers();
        Ok(info)
    }

    fn connected_ref_for_path(
        &self,
        canonical_path: &Path,
        except_ref: Option<&str>,
    ) -> Result<Option<String>, String> {
        for (base_ref, base) in &self.bases {
            if except_ref == Some(base_ref.as_str()) || !self.stores.contains_key(base_ref) {
                continue;
            }
            if canonical_base_identity(&base.path)? == canonical_path {
                return Ok(Some(base_ref.clone()));
            }
        }
        Ok(None)
    }

    fn switch_base(&mut self, base_ref: &str) -> Result<McpBaseInfo, String> {
        self.store_for_ref(base_ref)?;
        self.active_base_ref = base_ref.to_string();
        self.refresh_active_markers();
        self.bases
            .get(base_ref)
            .cloned()
            .ok_or_else(|| format!("Unknown base_ref '{}'", base_ref))
    }

    fn list_bases(&mut self) -> Vec<McpBaseInfo> {
        self.discover_scoped_bases(BaseScope::Project);
        self.discover_scoped_bases(BaseScope::User);
        self.refresh_active_markers();
        self.bases.values().cloned().collect()
    }

    fn discover_scoped_bases(&mut self, scope: BaseScope) {
        let Ok(root) = (match scope {
            BaseScope::Project => project_base_root(),
            BaseScope::User => user_base_root(),
        }) else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(&root) else {
            return;
        };

        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let scope_label = scope.as_str().to_string();
            let base_ref = format!("{}:{}", scope_label, name);
            self.bases.entry(base_ref.clone()).or_insert(McpBaseInfo {
                base_ref,
                scope: scope_label,
                name,
                path: entry.path(),
                connected: false,
                active: false,
            });
        }
    }

    fn refresh_active_markers(&mut self) {
        for base in self.bases.values_mut() {
            base.connected = base.connected || self.stores.contains_key(&base.base_ref);
            base.active = base.base_ref == self.active_base_ref;
        }
    }
}

#[cfg(feature = "mcp")]
impl BaseScope {
    fn as_str(self) -> &'static str {
        match self {
            BaseScope::Project => "project",
            BaseScope::User => "user",
        }
    }
}

#[cfg(feature = "mcp")]
fn infer_base_scope_label(path: &Path) -> String {
    let canonical_path = canonical_physical_path(path).ok();
    let project_root = project_base_root()
        .ok()
        .and_then(|root| canonical_physical_path(&root).ok());
    let user_root = user_base_root()
        .ok()
        .and_then(|root| canonical_physical_path(&root).ok());
    if project_root.as_ref().is_some_and(|root| {
        canonical_path
            .as_ref()
            .is_some_and(|path| path.starts_with(root))
    }) {
        "project".to_string()
    } else if user_root.as_ref().is_some_and(|root| {
        canonical_path
            .as_ref()
            .is_some_and(|path| path.starts_with(root))
    }) {
        "user".to_string()
    } else {
        "path".to_string()
    }
}

#[cfg(feature = "mcp")]
fn canonical_base_identity(path: &Path) -> Result<PathBuf, String> {
    canonical_physical_path(path).map_err(|err| err.to_string())
}

#[cfg(feature = "mcp")]
fn serve_diagnostic_sink(stdio: bool) -> DiagnosticSink {
    if stdio {
        DiagnosticSink::Stderr
    } else {
        DiagnosticSink::Stdout
    }
}

#[cfg(feature = "mcp")]
fn print_info_to(sink: DiagnosticSink, message: &str) {
    match sink {
        DiagnosticSink::Stdout => print_info(message),
        DiagnosticSink::Stderr => eprintln!("{} {}", "ℹ".blue(), message),
    }
}

fn project_base_root() -> CliResult<PathBuf> {
    Ok(std::env::current_dir()?.join(".memoryx").join("bases"))
}

fn user_base_root() -> CliResult<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| CliError::Config("Could not determine home directory".to_string()))?;
    Ok(home.join(".memoryx").join("bases"))
}

fn scoped_base_path(scope: BaseScope, base_name: &str) -> CliResult<PathBuf> {
    let root = match scope {
        BaseScope::Project => project_base_root()?,
        BaseScope::User => user_base_root()?,
    };
    Ok(root.join(base_name))
}

fn is_simple_base_name(path: &Path) -> bool {
    let mut components = path.components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    )
}

fn absolute_or_cwd(path: &Path) -> CliResult<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn normalize_absolute_path(path: PathBuf) -> CliResult<PathBuf> {
    if !path.is_absolute() {
        return Err(CliError::Validation(format!(
            "Base path '{}' must be absolute",
            path.display()
        )));
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(CliError::Validation(format!(
                        "Base path '{}' escapes its filesystem root",
                        path.display()
                    )));
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

/// Resolve existing ancestors physically while preserving missing leaf components.
///
/// A dangling symlink is rejected rather than treated as a missing directory,
/// so a later create cannot escape an authorized storage root through it.
fn canonical_physical_path(path: &Path) -> CliResult<PathBuf> {
    let mut current = normalize_absolute_path(absolute_or_cwd(path)?)?;
    let mut missing_components: Vec<OsString> = Vec::new();

    loop {
        match std::fs::symlink_metadata(&current) {
            Ok(_) => {
                let mut canonical = std::fs::canonicalize(&current).map_err(|err| {
                    CliError::Validation(format!(
                        "Base path '{}' cannot be resolved physically: {}",
                        current.display(),
                        err
                    ))
                })?;
                for component in missing_components.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let component = current.file_name().ok_or_else(|| {
                    CliError::Validation(format!(
                        "Base path '{}' has no existing filesystem ancestor",
                        path.display()
                    ))
                })?;
                missing_components.push(component.to_os_string());
                if !current.pop() {
                    return Err(CliError::Validation(format!(
                        "Base path '{}' has no existing filesystem ancestor",
                        path.display()
                    )));
                }
            }
            Err(err) => return Err(CliError::Io(err)),
        }
    }
}

fn validate_allowed_base_path(path: &Path) -> CliResult<PathBuf> {
    let candidate = canonical_physical_path(path)?;
    let project_root = canonical_physical_path(&project_base_root()?)?;
    let user_root = canonical_physical_path(&user_base_root()?)?;

    if candidate.starts_with(&project_root) || candidate.starts_with(&user_root) {
        Ok(candidate)
    } else {
        Err(CliError::Validation(format!(
            "Base path '{}' must be inside project storage '{}' or shared user storage '{}'",
            candidate.display(),
            project_root.display(),
            user_root.display()
        )))
    }
}

#[cfg(feature = "mcp")]
fn federation_base_id_path(base: &Path) -> PathBuf {
    base.join("meta").join("federation_base_id.hex")
}

#[cfg(feature = "mcp")]
fn load_or_create_federation_base_id(base: &Path) -> CliResult<[u8; 32]> {
    let path = federation_base_id_path(base);
    if path.exists() {
        let raw = std::fs::read_to_string(&path)?;
        return parse_federation_base_id(raw.trim());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let canonical_base = base
        .canonicalize()
        .unwrap_or_else(|_| absolute_or_cwd(base).unwrap_or_else(|_| base.to_path_buf()));
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"memoryx:federation-base-id:v1");
    hasher.update(canonical_base.to_string_lossy().as_bytes());
    hasher.update(&now_ns.to_le_bytes());
    hasher.update(&std::process::id().to_le_bytes());
    let base_id = *hasher.finalize().as_bytes();

    std::fs::write(&path, hex::encode(base_id))?;
    Ok(base_id)
}

#[cfg(feature = "mcp")]
fn parse_federation_base_id(raw: &str) -> CliResult<[u8; 32]> {
    let bytes = hex::decode(raw)
        .map_err(|e| CliError::Validation(format!("Invalid federation BaseId: {}", e)))?;
    if bytes.len() != 32 {
        return Err(CliError::Validation(format!(
            "Invalid federation BaseId length: expected 32 bytes, got {}",
            bytes.len()
        )));
    }
    let mut base_id = [0u8; 32];
    base_id.copy_from_slice(&bytes);
    Ok(base_id)
}

fn resolve_base_path(
    base_arg: Option<&PathBuf>,
    cli_scope: Option<BaseScope>,
    cli_name: Option<&str>,
    config: &MemoryXConfig,
) -> CliResult<PathBuf> {
    if let Some(base) = base_arg {
        if is_simple_base_name(base) {
            let scope = cli_scope
                .or(config.default_base_scope)
                .unwrap_or(BaseScope::Project);
            return validate_allowed_base_path(&scoped_base_path(scope, &base.to_string_lossy())?);
        }
        return validate_allowed_base_path(base);
    }

    if let Some(default_base) = config.default_base.as_ref() {
        return validate_allowed_base_path(default_base);
    }

    let scope = cli_scope
        .or(config.default_base_scope)
        .unwrap_or(BaseScope::Project);
    let name = cli_name
        .or(config.default_base_name.as_deref())
        .unwrap_or("default");

    validate_allowed_base_path(&scoped_base_path(scope, name)?)
}

// ============================================================================
// Command Implementations
// ============================================================================

/// Initialize a new MemoryX base directory
fn cmd_init(base: &Path, force: bool) -> CliResult<()> {
    if base.exists() && !force {
        return Err(CliError::Validation(format!(
            "Directory '{}' already exists. Use --force to overwrite.",
            base.display()
        )));
    }

    // Create directory structure
    let dirs = ["cas", "index", "graph", "meta", "inverted"];
    for dir in &dirs {
        let path = base.join(dir);
        std::fs::create_dir_all(&path)?;
    }

    // Create initial store to validate
    let config = StoreConfig::new(base.to_path_buf());
    let _store = MemoryX::new(config)
        .map_err(|e| CliError::Store(format!("Failed to create store: {}", e)))?;

    print_success(&format!(
        "Initialized MemoryX base directory at '{}'",
        base.display()
    ));

    Ok(())
}

struct IngestCliOptions<'a> {
    base: &'a Path,
    files: &'a [PathBuf],
    atom_type: &'a str,
    batch_size: usize,
    extract_claims: bool,
    dry_run: bool,
    extractor_name: &'a str,
    output_format: OutputFormat,
    verbose: bool,
}

#[derive(Debug, Deserialize)]
struct EntityCreateForm {
    canonical_name: String,
    entity_type: String,
    #[serde(default)]
    aliases: Vec<String>,
}

fn read_entity_create_form(path: &Path) -> CliResult<EntityCreateForm> {
    let content = std::fs::read_to_string(path)?;
    if path
        .extension()
        .map(|ext| ext == "yaml" || ext == "yml")
        .unwrap_or(false)
    {
        Ok(serde_yaml::from_str(&content)?)
    } else {
        Ok(serde_json::from_str(&content)?)
    }
}

/// Ingest atoms from files
fn cmd_ingest(options: IngestCliOptions<'_>) -> CliResult<()> {
    let IngestCliOptions {
        base,
        files,
        atom_type,
        batch_size: _batch_size,
        extract_claims,
        dry_run,
        extractor_name,
        output_format,
        verbose,
    } = options;

    if extract_claims {
        if !dry_run {
            return Err(CliError::Validation(
                "--extract-claims currently requires --dry-run; confirm proposals through authoring APIs/MCP before writing facts".to_owned(),
            ));
        }
        return cmd_ingest_extract_claims(files, extractor_name, output_format, verbose);
    }

    // Open store
    let config = StoreConfig::new(base.to_path_buf());
    let mut store = MemoryX::new(config)
        .map_err(|e| CliError::Store(format!("Failed to open store: {}", e)))?;

    let atom_type = parse_atom_type(atom_type)?;

    let total_files = files.len();
    let pb = create_progress_bar(total_files as u64, "Ingesting files");

    let mut total_atoms = 0usize;
    let mut total_errors = 0usize;

    for file in files {
        if verbose {
            print_info(&format!("Processing: {}", file.display()));
        }

        // Read file content
        let content = std::fs::read_to_string(file)?;

        // Parse based on file extension
        let atoms: Vec<AtomExport> = if file
            .extension()
            .map(|e| e == "yaml" || e == "yml")
            .unwrap_or(false)
        {
            serde_yaml::from_str(&content)?
        } else {
            serde_json::from_str(&content)?
        };

        // Convert to batch atoms
        let batch_atoms: Vec<BatchAtom> = atoms
            .into_iter()
            .map(|atom| {
                let claims: Vec<ClaimData> = atom.claims.into_iter().map(Into::into).collect();
                let evidence: Vec<EvidenceRef> = atom
                    .evidence
                    .into_iter()
                    .map(|e| EvidenceRef {
                        atom_id: hex_to_atom_id(&e.atom_id).unwrap_or([0u8; 32]),
                        section_kind: SectionKind::EVIDENCE,
                        offset: e.offset,
                        length: e.length,
                        trust: e.trust,
                    })
                    .collect();

                // Create minimal valid atom body
                let payload = create_minimal_atom_body(atom_type, &claims);

                BatchAtom::new(payload, atom_type, claims, evidence)
            })
            .collect();

        // Batch ingest
        let result = store
            .batch_ingest(batch_atoms)
            .map_err(|e| CliError::Store(format!("Batch ingest failed: {}", e)))?;

        total_atoms += result.success_count();
        total_errors += result.error_count();

        if verbose && !result.errors.is_empty() {
            for error in &result.errors {
                print_warning(&format!(
                    "Failed to ingest atom at index {}: {}",
                    error.index, error.error
                ));
            }
        }

        pb.inc(1);
    }

    pb.finish_and_clear();

    print_success(&format!(
        "Ingested {} atoms ({} errors)",
        total_atoms, total_errors
    ));

    Ok(())
}

fn cmd_create_entity(
    base: &Path,
    name: Option<&str>,
    entity_type: Option<&str>,
    form: Option<&Path>,
    output_format: OutputFormat,
) -> CliResult<()> {
    let config = StoreConfig::new(base.to_path_buf());
    let mut store = MemoryX::new(config)
        .map_err(|e| CliError::Store(format!("Failed to open store: {}", e)))?;

    let form = match form {
        Some(path) => read_entity_create_form(path)?,
        None => EntityCreateForm {
            canonical_name: name
                .ok_or_else(|| {
                    CliError::Validation("--name is required without --form".to_owned())
                })?
                .to_owned(),
            entity_type: entity_type
                .ok_or_else(|| {
                    CliError::Validation("--entity-type is required without --form".to_owned())
                })?
                .to_owned(),
            aliases: Vec::new(),
        },
    };

    let mut entity = store
        .create_entity(form.canonical_name, form.entity_type)
        .map_err(|e| CliError::Store(format!("Create entity failed: {}", e)))?;
    for alias in form.aliases {
        entity = store
            .alias_entity(entity.entity_id, alias)
            .map_err(|e| CliError::Store(format!("Alias entity failed: {}", e)))?;
    }

    print_serialized(&entity, output_format)
}

fn cmd_add_entity_claim(
    base: &Path,
    entity: u64,
    predicate: u32,
    object: u64,
    object_tag: Option<&str>,
    ctx: u32,
    output_format: OutputFormat,
) -> CliResult<()> {
    let config = StoreConfig::new(base.to_path_buf());
    let mut store = MemoryX::new(config)
        .map_err(|e| CliError::Store(format!("Failed to open store: {}", e)))?;
    let object_tag = ObjTag::from_u8(parse_object_tag(object_tag)?)
        .ok_or_else(|| CliError::Validation("invalid object tag".to_owned()))?;
    let result = store
        .add_entity_claim(entity, predicate, object_tag, object, ctx, Vec::new())
        .map_err(|e| CliError::Store(format!("Add entity claim failed: {}", e)))?;

    print_serialized(&result, output_format)
}

fn cmd_create_relation(
    base: &Path,
    subject: u64,
    predicate: u32,
    object: u64,
    ctx: u32,
    output_format: OutputFormat,
) -> CliResult<()> {
    let config = StoreConfig::new(base.to_path_buf());
    let mut store = MemoryX::new(config)
        .map_err(|e| CliError::Store(format!("Failed to open store: {}", e)))?;
    let result = store
        .assert_relation(subject, predicate, object, ctx, Vec::new())
        .map_err(|e| CliError::Store(format!("Create relation failed: {}", e)))?;

    print_serialized(&result, output_format)
}

fn cmd_ingest_extract_claims(
    files: &[PathBuf],
    extractor_name: &str,
    output_format: OutputFormat,
    verbose: bool,
) -> CliResult<()> {
    let extractor = ExtractorIdentity {
        extractor: extractor_name.to_owned(),
        ..Default::default()
    };

    let mut plans = Vec::with_capacity(files.len());
    for file in files {
        if verbose {
            print_info(&format!("Extracting candidate claims: {}", file.display()));
        }
        let content = std::fs::read_to_string(file)?;
        plans.push(IngestExtractor::dry_run_extract(
            file.display().to_string(),
            &content,
            extractor.clone(),
        ));
    }

    match output_format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&plans)?);
        }
        OutputFormat::Yaml => {
            println!("{}", serde_yaml::to_string(&plans)?);
        }
        OutputFormat::Table => {
            for plan in &plans {
                println!("Source: {}", plan.source);
                println!("Candidate claims: {}", plan.candidate_claims.len());
                println!("Entity mentions: {}", plan.entity_mentions.len());
                println!("Suggested relations: {}", plan.suggested_relations.len());
                println!("Confirmation required: {}", plan.confirmation_required);
                for claim in &plan.candidate_claims {
                    println!(
                        "  - [{}] {} --{}--> {} (confidence {:.2})",
                        serde_json::to_string(&claim.status)?,
                        claim.subject,
                        claim.predicate,
                        claim.object,
                        claim.confidence
                    );
                }
            }
        }
    }

    Ok(())
}

/// Create minimal valid atom body
fn create_minimal_atom_body(atom_type: AtomType, claims: &[ClaimData]) -> Vec<u8> {
    use memoryx::cas::claims::ClaimsSection;
    use memoryx::cas::evidence::EvidenceSection;
    use memoryx::cas::invariants::InvariantsSection;
    use memoryx::cas::meta::{MetaField, MetaFieldKind, MetaSection, MetaValue};
    use memoryx::cas::symbols::SymbolsSection;

    let mut symbols_section = SymbolsSection::new();
    symbols_section.intern("test_entity".to_string());
    symbols_section.intern("test_relation".to_string());

    let mut claims_section = ClaimsSection::new();
    for claim in claims {
        let subj_sym = symbols_section.intern(format!("subject_{}", claim.subj));
        let pred_sym = symbols_section.intern(format!("predicate_{}", claim.pred));
        claims_section.add_claim(
            ClaimRecord::from_scalar(
                u64::from(subj_sym),
                pred_sym,
                ObjTag::from_u8(claim.obj_tag).unwrap_or(ObjTag::U64),
                claim.obj_val,
            )
            .expect("test claim must be scalar"),
        );
    }

    let symbols_bytes = symbols_section.to_bytes();
    let refs_bytes = Vec::new();
    let claims_bytes = claims_section.to_bytes();
    let invariants_bytes = InvariantsSection::new().to_bytes();
    let edges_bytes = Vec::new();
    let evidence_bytes = EvidenceSection::new().to_bytes();

    let mut meta_section = MetaSection::new();
    meta_section.add_field(MetaField::new(
        MetaFieldKind::TRUST_SCORE,
        MetaValue::F32(0.5),
    ));
    meta_section.add_field(MetaField::new(
        MetaFieldKind::DOMAIN_MASK,
        MetaValue::U32(0xFFFF),
    ));
    let meta_bytes = meta_section.to_bytes();

    let sections_data_start: usize = 48 + 7 * 32;
    let mut current_off = sections_data_start;
    let symbols_off = current_off;
    current_off += symbols_bytes.len();
    let refs_off = current_off;
    current_off += refs_bytes.len();
    let claims_off = current_off;
    current_off += claims_bytes.len();
    let invariants_off = current_off;
    current_off += invariants_bytes.len();
    let edges_off = current_off;
    current_off += edges_bytes.len();
    let evidence_off = current_off;
    current_off += evidence_bytes.len();
    let meta_off = current_off;

    let mut payload = Vec::new();
    payload.extend_from_slice(&0x41544F4Du32.to_le_bytes()); // body_magic "ATOM"
    payload.extend_from_slice(&0x0001u16.to_le_bytes()); // body_ver
    payload.extend_from_slice(&0u16.to_le_bytes()); // body_flags
    payload.extend_from_slice(&0u64.to_le_bytes()); // created_at_unix_ns
    payload.extend_from_slice(&0u64.to_le_bytes()); // valid_from_unix_ns
    payload.extend_from_slice(&u64::MAX.to_le_bytes()); // valid_to_unix_ns
    payload.extend_from_slice(&atom_type.to_u32().to_le_bytes()); // atom_type
    payload.extend_from_slice(&7u32.to_le_bytes()); // section_count
    payload.extend_from_slice(&48u64.to_le_bytes()); // section_table_off

    let mut add_section_desc = |kind: u32, off: usize, data: &[u8]| {
        let crc = memoryx::cas::crc32(data);
        payload.extend_from_slice(&kind.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&(off as u64).to_le_bytes());
        payload.extend_from_slice(&(data.len() as u64).to_le_bytes());
        payload.extend_from_slice(&crc.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
    };

    add_section_desc(0x01, symbols_off, &symbols_bytes);
    add_section_desc(0x02, refs_off, &refs_bytes);
    add_section_desc(0x03, claims_off, &claims_bytes);
    add_section_desc(0x04, invariants_off, &invariants_bytes);
    add_section_desc(0x05, edges_off, &edges_bytes);
    add_section_desc(0x06, evidence_off, &evidence_bytes);
    add_section_desc(0x07, meta_off, &meta_bytes);

    payload.extend_from_slice(&symbols_bytes);
    payload.extend_from_slice(&refs_bytes);
    payload.extend_from_slice(&claims_bytes);
    payload.extend_from_slice(&invariants_bytes);
    payload.extend_from_slice(&edges_bytes);
    payload.extend_from_slice(&evidence_bytes);
    payload.extend_from_slice(&meta_bytes);

    payload
}

/// Execute a query
struct QueryCliOptions<'a> {
    base: &'a Path,
    query: Option<&'a str>,
    contract_path: Option<&'a Path>,
    emit_contract: bool,
    ctx_policy: u32,
    _limit: Option<usize>,
    _min_trust: Option<u16>,
    include_trace: bool,
    explain_rejections: bool,
    format: OutputFormat,
}

fn cmd_query(options: QueryCliOptions<'_>) -> CliResult<()> {
    let start = Instant::now();

    if options.contract_path.is_some() && options.query.is_some() {
        return Err(CliError::Validation(
            "use either a natural query or --contract, not both".to_string(),
        ));
    }

    if options.contract_path.is_none() && options.query.is_none() {
        return Err(CliError::Validation(
            "query text or --contract is required".to_string(),
        ));
    }

    let (contract, query_label) = if let Some(path) = options.contract_path {
        let contract = read_query_contract(path)?;
        contract
            .validate()
            .map_err(|e| CliError::Validation(e.to_string()))?;
        (contract, format!("contract:{}", path.display()))
    } else {
        let query_text = options.query.unwrap_or_default();
        let contract = QueryContractCompiler::compile_contract(query_text);
        (contract, query_text.to_string())
    };

    if options.emit_contract {
        if options.contract_path.is_some() {
            return Err(CliError::Validation(
                "--emit-contract requires natural query text, not --contract".to_string(),
            ));
        }
        print_serialized(&contract, options.format)?;
        return Ok(());
    }

    // Open store
    let config = StoreConfig::new(options.base.to_path_buf());
    let store = MemoryX::new(config)
        .map_err(|e| CliError::Store(format!("Failed to open store: {}", e)))?;

    if matches!(options.format, OutputFormat::Table) {
        print_info(&format!("Executing query: '{}'", query_label));
    }

    // Execute query
    let answer = store
        .answer_contract(contract, options.ctx_policy)
        .map_err(|e| CliError::Store(format!("Query failed: {}", e)))?;

    let elapsed = start.elapsed();

    // Format and output results
    match options.format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&answer_pack_json(&answer))?
            );
        }
        OutputFormat::Yaml => {
            println!("{}", serde_yaml::to_string(&answer_pack_json(&answer))?);
        }
        OutputFormat::Table => {
            println!("\n{}", "Query Results".bold().underline());
            println!("  Query:      {}", query_label.cyan());
            println!(
                "  Confidence: {:.1}%",
                (answer.confidence * 100.0).to_string().yellow()
            );
            println!("  Context:    {}", answer.selected_ctx);
            println!("  Claims:     {}", answer.claims.len());
            println!("  Evidence:   {}", answer.evidence.len());
            println!("  Time:       {:?}", elapsed);

            if !answer.limitations.is_empty() {
                println!("\n{}", "Limitations:".yellow().bold());
                for limitation in &answer.limitations {
                    let severity_icon = match limitation.severity {
                        LimitationSeverity::Info => "ℹ",
                        LimitationSeverity::Warning => "⚠",
                        LimitationSeverity::Critical => "✗",
                    };
                    let severity_color = match limitation.severity {
                        LimitationSeverity::Info => "blue",
                        LimitationSeverity::Warning => "yellow",
                        LimitationSeverity::Critical => "red",
                    };
                    println!(
                        "  {} [{}] {}",
                        severity_icon,
                        format!("{:?}", limitation.code).color(severity_color),
                        limitation.description
                    );
                }
            }

            if options.include_trace && !answer.query_trace.retrieval_actions.is_empty() {
                println!("\n{}", "Query Trace:".bold());
                for action in &answer.query_trace.retrieval_actions {
                    println!(
                        "  gap={} utility={:.4} selected={} reason={}",
                        action.gap_id, action.utility, action.selected, action.reason
                    );
                }
            }

            if options.explain_rejections && !answer.rejected_candidates.is_empty() {
                println!("\n{}", "Rejected Candidates:".red().bold());
                for rejected in &answer.rejected_candidates {
                    let atom = rejected
                        .atom_id
                        .as_ref()
                        .map(memoryx::cas::hex_encode)
                        .unwrap_or_else(|| "none".to_string());
                    println!(
                        "  atom={} backend={} reason={}",
                        atom, rejected.source_backend, rejected.reason
                    );
                }
            }
        }
    }

    Ok(())
}

fn read_query_contract(path: &Path) -> CliResult<QueryContract> {
    let data = std::fs::read_to_string(path)?;
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("yaml" | "yml") => Ok(serde_yaml::from_str(&data)?),
        _ => Ok(serde_json::from_str(&data)?),
    }
}

fn answer_graph_json(graph: &memoryx::store::api::AnswerGraph) -> serde_json::Value {
    let nodes = graph
        .nodes
        .iter()
        .map(|node| {
            serde_json::json!({
                "atom_id": hex::encode(node.atom_ref.atom_id),
                "node_num": node.atom_ref.node_num,
                "atom_type": node.atom_type.to_string(),
                "trust": node.trust,
                "branch_ctx_id": node.branch_ctx_id,
                "evidence_ref_count": node.evidence_refs.len(),
                "source_link_count": node
                    .direct_evidence
                    .iter()
                    .filter(|record| record.source_id.is_some())
                    .count(),
                "direct_evidence": node.direct_evidence,
            })
        })
        .collect::<Vec<_>>();
    let edges = graph
        .edges
        .iter()
        .map(|edge| {
            serde_json::json!({
                "src_idx": edge.src_idx,
                "dst_idx": edge.dst_idx,
                "edge_type": format!("{:?}", edge.edge_type),
                "confidence": edge.confidence,
                "derived": edge.derived,
            })
        })
        .collect::<Vec<_>>();

    serde_json::json!({
        "ctx_id": graph.ctx_id,
        "node_count": graph.node_count(),
        "edge_count": graph.edge_count(),
        "evidence_ref_count": graph.evidence_ref_count(),
        "evidence_record_count": graph.evidence_record_count(),
        "source_link_count": graph.source_link_count(),
        "branch_lineage": graph.branch_lineage,
        "nodes": nodes,
        "edges": edges,
    })
}

fn answer_pack_json(answer: &memoryx::store::api::AnswerPack) -> serde_json::Value {
    let full = answer_pack_json_unbounded(answer);
    let original_bytes = full.to_string().len();
    if original_bytes <= answer.response_limits.max_bytes as usize {
        return full;
    }

    let mut limits = answer.response_limits.clone();
    limits.bytes_truncated = true;
    limits.original_bytes = Some(original_bytes);
    let mut bounded = serde_json::json!({
        "status": "Partial",
        "selected_ctx": answer.selected_ctx,
        "snapshot": answer.snapshot,
        "coverage_report": answer.coverage_report,
        "limitations": [{
            "code": "BudgetExhausted",
            "description": format!(
                "serialized response exceeded output_contract.max_bytes={}; collections were omitted from this response only and remain durable in the base",
                limits.max_bytes
            ),
            "severity": "Warning"
        }],
        "response_limits": limits,
    });
    for _ in 0..4 {
        let emitted = bounded.to_string().len();
        if bounded["response_limits"]["emitted_bytes"].as_u64() == Some(emitted as u64) {
            break;
        }
        bounded["response_limits"]["emitted_bytes"] = serde_json::json!(emitted);
    }
    debug_assert!(bounded.to_string().len() <= answer.response_limits.max_bytes as usize);
    bounded
}

fn answer_pack_json_unbounded(answer: &memoryx::store::api::AnswerPack) -> serde_json::Value {
    serde_json::json!({
        "status": format!("{:?}", answer.status),
        "selected_ctx": answer.selected_ctx,
        "confidence": answer.confidence,
        "snapshot": answer.snapshot,
        "graph": answer_graph_json(&answer.graph),
        "claims": answer.claims,
        "claims_v2": answer.claims_v2,
        "evidence": answer.evidence,
        "evidence_records": answer.evidence_records,
        "coverage_report": answer.coverage_report,
        "rejected_candidates": answer.rejected_candidates,
        "limitations": answer.limitations.iter().map(|l| {
            serde_json::json!({
                "code": format!("{:?}", l.code),
                "description": l.description,
                "severity": format!("{:?}", l.severity),
            })
        }).collect::<Vec<_>>(),
        "alternates": answer.alternates.iter().map(answer_pack_json_unbounded).collect::<Vec<_>>(),
        "conflicts": answer.conflicts,
        "conflict_sets": answer.conflict_sets,
        "query_trace": answer.query_trace,
        "proposed_text": answer.proposed_text,
        "response_limits": answer.response_limits,
    })
}

/// Run compaction
fn cmd_compact(base: &Path, compaction_type: CompactionType, dry_run: bool) -> CliResult<()> {
    use memoryx::cas::io::{CasStore as CasIoStore, Compactor};
    use memoryx::graph::GraphStore;

    let (base, _lease) = acquire_compaction_lease(base)?;

    print_info(&format!(
        "Running compaction on '{}' (type: {:?})",
        base.display(),
        compaction_type
    ));

    if dry_run {
        print_info("DRY RUN - showing what would be compacted");
    }

    // Compact CAS segments
    if matches!(compaction_type, CompactionType::All | CompactionType::Cas) {
        let cas_dir = base.join("cas");
        if cas_dir.exists() {
            // Collect segment files
            let mut seg_files: Vec<String> = Vec::new();
            let mut seg_total_size: u64 = 0;
            for entry in std::fs::read_dir(&cas_dir).map_err(CliError::Io)? {
                let entry = entry.map_err(CliError::Io)?;
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with("seg_") && name.ends_with(".dat") {
                    seg_total_size += entry.metadata().map(|m| m.len()).unwrap_or(0);
                    seg_files.push(name);
                }
            }
            seg_files.sort();

            if seg_files.len() > 1 {
                if dry_run {
                    print_info(&format!(
                        "Would compact {} CAS segments ({} bytes) into 1 segment",
                        seg_files.len(),
                        format_bytes(seg_total_size)
                    ));
                } else {
                    print_info(&format!(
                        "Compacting {} CAS segments ({} bytes)...",
                        seg_files.len(),
                        format_bytes(seg_total_size)
                    ));

                    let pb = create_progress_bar(seg_files.len() as u64, "Compacting segments");

                    // Compact segments
                    let cas_store = CasIoStore::open(&cas_dir, None)
                        .map_err(|e| CliError::Store(format!("Failed to open CAS: {}", e)))?;
                    cas_store.init_writer().unwrap();
                    cas_store.init_reader().unwrap();

                    // Extract segment IDs from filenames
                    let mut source_ids: Vec<u32> = Vec::new();
                    for name in &seg_files {
                        if let Some(num_str) = name
                            .strip_prefix("seg_")
                            .and_then(|s| s.strip_suffix(".dat"))
                            && let Ok(num) = num_str.parse::<u32>()
                        {
                            source_ids.push(num);
                        }
                    }

                    let target_id = source_ids.iter().max().copied().unwrap_or(0) + 1;
                    let records_compacted = cas_store
                        .compact(&source_ids, target_id)
                        .map_err(|e| CliError::Store(format!("Compact failed: {}", e)))?;

                    // Delete old segments
                    if records_compacted > 0 {
                        let compactor = Compactor::new(&cas_dir, None);
                        compactor.delete_segments(&source_ids).map_err(|e| {
                            CliError::Store(format!("Failed to delete segments: {}", e))
                        })?;
                    }

                    pb.finish_and_clear();

                    // Calculate savings
                    let mut new_size: u64 = 0;
                    if let Ok(entries) = std::fs::read_dir(&cas_dir) {
                        for entry in entries.filter_map(|e| e.ok()) {
                            let name = entry.file_name().to_string_lossy().to_string();
                            if name.starts_with("seg_") && name.ends_with(".dat") {
                                new_size += entry.metadata().map(|m| m.len()).unwrap_or(0);
                            }
                        }
                    }

                    let saved = if seg_total_size > new_size {
                        format_bytes(seg_total_size - new_size)
                    } else {
                        "0 bytes".to_string()
                    };

                    print_success(&format!(
                        "CAS compacted: {} segments → 1 segment (compacted {} records, saved {})",
                        seg_files.len(),
                        records_compacted,
                        saved
                    ));
                }
            } else {
                print_info(&format!(
                    "Only {} CAS segment(s) found, skipping CAS compaction",
                    seg_files.len()
                ));
            }
        }
    }

    // Compact GraphStore
    if matches!(compaction_type, CompactionType::All | CompactionType::Graph) {
        let graph_dir = base.join("graph");
        if graph_dir.exists()
            && let Ok(mut graph) = GraphStore::load(&graph_dir)
        {
            let before_edges = graph.edge_count();
            let before_delta = graph.delta_count();

            if dry_run {
                print_info(&format!(
                    "Would compact graph: {} edges, {} delta layers",
                    before_edges, before_delta
                ));
            } else if graph.needs_compaction() {
                print_info("Compacting graph...");
                graph
                    .compact()
                    .map_err(|e| CliError::Store(format!("Graph compact failed: {}", e)))?;

                let after_delta = graph.delta_count();
                print_success(&format!(
                    "Graph compacted: {} delta layers remained ({} edges total)",
                    after_delta,
                    graph.edge_count()
                ));
            } else {
                print_info("Graph does not need compaction (no delta layers)");
            }
        }
    }

    // Clean up old meta files
    if matches!(compaction_type, CompactionType::All | CompactionType::Meta) {
        let meta_dir = base.join("meta");
        if meta_dir.exists() {
            let mut wal_count = 0;
            for entry in std::fs::read_dir(&meta_dir).map_err(CliError::Io)? {
                let entry = entry.map_err(CliError::Io)?;
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with("meta_wal_") {
                    wal_count += 1;
                }
            }
            if dry_run && wal_count > 0 {
                print_info(&format!("Would process {} meta WAL files", wal_count));
            } else if wal_count > 0 {
                print_info(&format!("Processing {} meta WAL files...", wal_count));
                print_success("Meta WAL files processed");
            }
        }
    }

    Ok(())
}

const BASE_LEASE_FILE_NAME: &str = ".memoryx.writer.lock";

/// CLI-local guard for the same OS lock used by `BaseLease`.
///
/// `BaseLease` is intentionally crate-private, so the binary acquires the
/// identical physical lock without constructing a second MemoryX store.
struct CompactionLease {
    file: File,
}

impl Drop for CompactionLease {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn is_lock_contended(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::WouldBlock {
        return true;
    }

    #[cfg(windows)]
    {
        matches!(error.raw_os_error(), Some(32 | 33))
    }

    #[cfg(not(windows))]
    {
        false
    }
}

fn acquire_compaction_lease(base: &Path) -> CliResult<(PathBuf, CompactionLease)> {
    std::fs::create_dir_all(base)?;
    let canonical_root = canonical_physical_path(base)?;
    let lock_path = canonical_root.join(BASE_LEASE_FILE_NAME);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;

    match file.try_lock_exclusive() {
        Ok(()) => Ok((canonical_root, CompactionLease { file })),
        Err(error) if is_lock_contended(&error) => Err(CliError::Store(format!(
            "Cannot compact '{}': exclusive writer lease is already held by another MemoryX instance",
            canonical_root.display()
        ))),
        Err(error) => Err(CliError::Io(error)),
    }
}

/// Format bytes to human-readable string
fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} bytes", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

/// Export data
fn cmd_export(
    base: &Path,
    format: ExportFormat,
    output: Option<&Path>,
    filter: Option<&str>,
) -> CliResult<()> {
    print_info(&format!("Exporting data from '{}'", base.display()));

    // Open store
    let config = StoreConfig::new(base.to_path_buf());
    let store = MemoryX::new(config)
        .map_err(|e| CliError::Store(format!("Failed to open store: {}", e)))?;

    let mut atoms = Vec::new();
    for atom_id in store.list_atom_ids() {
        let export = atom_export_from_store(&store, &atom_id)?;
        if atom_matches_export_filter(&export, filter) {
            atoms.push(export);
        }
    }

    let output_str = match format {
        ExportFormat::Json => serde_json::to_string_pretty(&atoms)?,
        ExportFormat::Yaml => serde_yaml::to_string(&atoms)?,
        ExportFormat::Ndjson => atoms
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?
            .join("\n"),
        ExportFormat::Csv => write_csv_atoms(&atoms)?,
    };

    // Output
    match output {
        Some(path) => {
            std::fs::write(path, output_str)?;
            print_success(&format!(
                "Exported {} atoms to '{}'",
                atoms.len(),
                path.display()
            ));
        }
        None => {
            println!("{}", output_str);
        }
    }

    Ok(())
}

/// Import data
fn cmd_import(
    base: &Path,
    format: ExportFormat,
    input: &Path,
    skip_validation: bool,
) -> CliResult<()> {
    print_info(&format!("Importing data from '{}'", input.display()));

    // Open store
    let config = StoreConfig::new(base.to_path_buf());
    let mut store = MemoryX::new(config)
        .map_err(|e| CliError::Store(format!("Failed to open store: {}", e)))?;

    // Read input
    let content = std::fs::read_to_string(input)?;

    let atoms = parse_atom_exports_from_content(format, &content)?;

    if !skip_validation {
        for (index, atom) in atoms.iter().enumerate() {
            if atom.atom_type.trim().is_empty() {
                return Err(CliError::Validation(format!(
                    "Atom at index {} has no atom_type",
                    index
                )));
            }
            if atom.claims.is_empty() {
                return Err(CliError::Validation(format!(
                    "Atom at index {} has no claims",
                    index
                )));
            }
        }
    }

    let batch_atoms = build_batch_atoms_from_exports(atoms, skip_validation)?;

    let result = store
        .batch_ingest(batch_atoms)
        .map_err(|e| CliError::Store(format!("Import failed: {}", e)))?;

    print_success(&format!(
        "Imported {} atoms ({} errors)",
        result.success_count(),
        result.error_count()
    ));

    Ok(())
}

/// Show statistics
fn cmd_stats(base: &Path, detailed: bool, format: OutputFormat) -> CliResult<()> {
    use memoryx::cas::io::{
        CasStore as CasIoStore, INDEX_EXTENSION, SEGMENT_EXTENSION, SEGMENT_PREFIX,
    };
    use memoryx::graph::GraphStore;

    print_info(&format!("Collecting statistics for '{}'", base.display()));

    let cas_dir = base.join("cas");
    let graph_dir = base.join("graph");
    let meta_dir = base.join("meta");
    let _index_dir = base.join("index");

    // --- CAS segments ---
    let mut seg_count = 0usize;
    let mut seg_total_size = 0u64;
    let mut total_records = 0u32;

    if cas_dir.exists()
        && let Ok(cas_store) = CasIoStore::open(&cas_dir, None)
    {
        cas_store.init_reader().unwrap();

        for entry in std::fs::read_dir(&cas_dir)
            .map_err(CliError::Io)?
            .filter_map(|e| e.ok())
        {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(SEGMENT_PREFIX) && name.ends_with(SEGMENT_EXTENSION) {
                seg_count += 1;
                seg_total_size += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }

        // Count records via index files
        for entry in std::fs::read_dir(&cas_dir)
            .map_err(CliError::Io)?
            .filter_map(|e| e.ok())
        {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(SEGMENT_PREFIX)
                && name.ends_with(INDEX_EXTENSION)
                && let Ok(data) = std::fs::read(entry.path())
                && data.len() >= 12
            {
                let entry_count = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                total_records += entry_count;
            }
        }
    }

    // --- Index files ---
    let mut idx_count = 0usize;
    let mut idx_total_size = 0u64;

    if cas_dir.exists() {
        for entry in std::fs::read_dir(&cas_dir)
            .map_err(CliError::Io)?
            .filter_map(|e| e.ok())
        {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(SEGMENT_PREFIX) && name.ends_with(INDEX_EXTENSION) {
                idx_count += 1;
                idx_total_size += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }

    // --- Graph stats ---
    let mut node_count: u64 = 0;
    let mut edge_count: u64 = 0;
    let mut delta_layers: usize = 0;

    if graph_dir.exists()
        && let Ok(graph) = GraphStore::load(&graph_dir)
    {
        node_count = graph.node_count();
        edge_count = graph.edge_count();
        delta_layers = graph.delta_count();
    }

    // --- Meta stats ---
    let mut snapshot_count = 0usize;
    let mut wal_count = 0usize;
    let mut meta_size = 0u64;

    if meta_dir.exists() {
        for entry in std::fs::read_dir(&meta_dir)
            .map_err(CliError::Io)?
            .filter_map(|e| e.ok())
        {
            let name = entry.file_name().to_string_lossy().to_string();
            meta_size += entry.metadata().map(|m| m.len()).unwrap_or(0);

            if name.starts_with("meta_snapshot_") {
                snapshot_count += 1;
            } else if name.starts_with("meta_wal_") {
                wal_count += 1;
            }
        }
    }

    // --- Total directory size ---
    let mut total_dir_size = 0u64;
    for entry in walkdir::WalkDir::new(base) {
        let entry = entry?;
        if entry.file_type().is_file() {
            total_dir_size += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }

    let stats = StorageStats {
        total_atoms: total_records as usize,
        total_claims: 0, // Would need to read each atom to count claims
        atom_types: HashMap::new(),
        storage_size_bytes: seg_total_size,
        index_size_bytes: idx_total_size,
        graph_edges: edge_count as usize,
        contexts: 1,
        conflicts: 0,
    };

    // --- Output ---
    match format {
        OutputFormat::Json => {
            let json = serde_json::json!({
                "total_atoms": stats.total_atoms,
                "total_claims": stats.total_claims,
                "cas_segments": seg_count,
                "cas_size_bytes": seg_total_size,
                "index_files": idx_count,
                "index_size_bytes": idx_total_size,
                "graph_nodes": node_count,
                "graph_edges": stats.graph_edges,
                "graph_delta_layers": delta_layers,
                "meta_snapshots": snapshot_count,
                "meta_wal_files": wal_count,
                "total_dir_size_bytes": total_dir_size,
            });
            println!("{}", serde_json::to_string_pretty(&json)?);
        }
        OutputFormat::Yaml => {
            let yaml = serde_json::json!({
                "total_atoms": stats.total_atoms,
                "cas_segments": seg_count,
                "cas_size_bytes": seg_total_size,
                "graph_nodes": node_count,
                "graph_edges": stats.graph_edges,
                "graph_delta_layers": delta_layers,
                "meta_snapshots": snapshot_count,
                "meta_wal_files": wal_count,
                "total_dir_size_bytes": total_dir_size,
            });
            println!("{}", serde_yaml::to_string(&yaml)?);
        }
        OutputFormat::Table => {
            println!("\n{}", "Storage Statistics".bold().underline());
            println!("  Base Directory:  {}", base.display().to_string().cyan());
            println!(
                "  CAS Segments:    {} files ({} bytes)",
                seg_count,
                format_bytes(seg_total_size).yellow()
            );
            println!(
                "  Index Files:     {} files ({} bytes)",
                idx_count,
                format_bytes(idx_total_size)
            );
            println!("  Index Entries:   {}", total_records);
            println!("  Graph Nodes:     {}", node_count.to_string().green());
            println!(
                "  Graph Edges:     {}",
                stats.graph_edges.to_string().green()
            );
            println!("  Delta Layers:    {}", delta_layers);
            println!("  Meta Snapshots:  {}", snapshot_count);
            println!("  Meta WAL Files:  {}", wal_count);
            println!("  Total Size:      {}", format_bytes(total_dir_size).bold());

            if detailed {
                println!("\n{}", "Storage Breakdown:".bold());
                println!(
                    "  CAS Data:      {} bytes ({} files)",
                    seg_total_size, seg_count
                );
                println!(
                    "  Index Data:    {} bytes ({} files)",
                    idx_total_size, idx_count
                );
                println!(
                    "  Meta Data:     {} bytes ({} files)",
                    meta_size,
                    snapshot_count + wal_count
                );
                println!(
                    "  Graph Data:    {} bytes",
                    if graph_dir.exists() {
                        walkdir::WalkDir::new(&graph_dir)
                            .into_iter()
                            .filter_map(|e| e.ok())
                            .filter(|e| e.file_type().is_file())
                            .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
                            .sum::<u64>()
                    } else {
                        0
                    }
                );
            }
        }
    }

    Ok(())
}

fn print_serialized<T: Serialize>(value: &T, format: OutputFormat) -> CliResult<()> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(value)?),
        OutputFormat::Yaml => println!("{}", serde_yaml::to_string(value)?),
        OutputFormat::Table => {}
    }
    Ok(())
}

fn cmd_verify_integrity(base: &Path, format: OutputFormat) -> CliResult<()> {
    let store = MemoryX::new(StoreConfig::new(base.to_path_buf()))?;
    let summary = store.verify_integrity()?;

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        print_serialized(&summary, format)?;
    } else {
        println!("\n{}", "Integrity Verification".bold().underline());
        println!("  Base:          {}", base.display().to_string().cyan());
        println!("  Checked atoms: {}", summary.checked_atoms);
        println!(
            "  Valid atoms:   {}",
            summary.valid_atoms.to_string().green()
        );
        println!(
            "  Invalid atoms: {}",
            summary.invalid_atoms.to_string().red()
        );
        println!(
            "  Missing atoms: {}",
            summary.missing_atoms.to_string().red()
        );
        if summary.errors.is_empty() {
            print_success("Integrity verification passed");
        } else {
            for error in &summary.errors {
                print_error(error);
            }
        }
    }

    if summary.is_valid() {
        Ok(())
    } else {
        Err(CliError::Store("integrity verification failed".to_string()))
    }
}

fn cmd_rebuild_index(base: &Path, dry_run: bool, format: OutputFormat) -> CliResult<()> {
    let mut store = MemoryX::new(StoreConfig::new(base.to_path_buf()))?;

    if dry_run {
        let summary = store.verify_integrity()?;
        if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
            print_serialized(&summary, format)?;
        } else {
            println!("\n{}", "Rebuild Index Dry Run".bold().underline());
            println!("  Base:          {}", base.display().to_string().cyan());
            println!("  Live atoms:    {}", summary.checked_atoms);
            println!("  Index rewrite: skipped (--dry-run)");
        }
        return Ok(());
    }

    let report = store.rebuild_indexes()?;
    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        print_serialized(&report, format)?;
    } else {
        println!("\n{}", "Index Rebuild".bold().underline());
        println!("  Base:          {}", base.display().to_string().cyan());
        println!(
            "  Indexed atoms: {}",
            report.indexed_atoms.to_string().green()
        );
        println!("  Indexed terms: {}", report.indexed_terms);
        println!(
            "  Skipped atoms: {}",
            report.skipped_atoms.to_string().yellow()
        );
        for error in &report.errors {
            print_error(error);
        }
        print_success("Index rebuild complete");
    }

    Ok(())
}

fn cmd_repair(base: &Path, format: OutputFormat) -> CliResult<()> {
    let mut store = MemoryX::new(StoreConfig::new(base.to_path_buf()))?;
    let report = store.repair()?;

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        print_serialized(&report, format)?;
    } else {
        println!("\n{}", "Repair".bold().underline());
        println!(
            "  Base:               {}",
            base.display().to_string().cyan()
        );
        println!("  Before valid atoms: {}", report.before.valid_atoms);
        println!("  Before invalid:     {}", report.before.invalid_atoms);
        println!(
            "  Reindexed atoms:    {}",
            report.rebuild.indexed_atoms.to_string().green()
        );
        println!("  Reindexed terms:    {}", report.rebuild.indexed_terms);
        println!("  After valid atoms:  {}", report.after.valid_atoms);
        println!(
            "  After invalid:      {}",
            report.after.invalid_atoms.to_string().red()
        );
        for error in report
            .before
            .errors
            .iter()
            .chain(report.rebuild.errors.iter())
            .chain(report.after.errors.iter())
        {
            print_error(error);
        }
    }

    if report.after.is_valid() {
        print_success("Repair completed and final integrity check passed");
        Ok(())
    } else {
        Err(CliError::Store(
            "repair completed but final integrity check failed".to_string(),
        ))
    }
}

fn cmd_history(base: &Path, limit: usize, format: OutputFormat) -> CliResult<()> {
    let store = MemoryX::new(StoreConfig::new(base.to_path_buf()))?;
    let entries = store.history(limit)?;

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        print_serialized(&entries, format)?;
    } else {
        println!("\n{}", "Operation History".bold().underline());
        println!("  Base:    {}", base.display().to_string().cyan());
        println!("  Entries: {}", entries.len());

        for (idx, entry) in entries.iter().enumerate() {
            println!(
                "\n[{}] {:?} @ {}",
                idx, entry.operation, entry.timestamp_unix_ns
            );
            if !entry.atom_ids.is_empty() {
                println!("  Atom IDs: {}", entry.atom_ids.join(", "));
            }
            for (key, value) in &entry.details {
                println!("  {}: {}", key, value);
            }
        }
    }

    Ok(())
}

fn cmd_snapshot(base: &Path, ctx: u32, format: OutputFormat) -> CliResult<()> {
    let store = MemoryX::new(StoreConfig::new(base.to_path_buf()))?;
    let snapshot = store.knowledge_snapshot(ctx)?;

    if matches!(format, OutputFormat::Json | OutputFormat::Yaml) {
        print_serialized(&snapshot, format)?;
    } else {
        println!("\n{}", "Knowledge Snapshot".bold().underline());
        println!("  Base:       {}", base.display().to_string().cyan());
        println!("  Logical ID: {}", snapshot.logical_id());
        println!("  CAS atoms:  {}", snapshot.cas_atom_count);
        println!(
            "  Graph:      {} nodes / {} edges",
            snapshot.graph_node_count, snapshot.graph_edge_count
        );
        println!("  Context:    {}", snapshot.context_id);
        println!("  Solver:     {}", snapshot.solver_version);
    }

    Ok(())
}

/// Start MCP stdio transport or HTTP federation server.
#[cfg(feature = "mcp")]
const MAX_MCP_REQUEST_LINE_BYTES: usize = 8 * 1024 * 1024;

#[cfg(feature = "mcp")]
enum McpInputLine {
    Eof,
    Line(Vec<u8>),
    TooLarge,
}

#[cfg(feature = "mcp")]
async fn read_bounded_mcp_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<McpInputLine> {
    use tokio::io::AsyncBufReadExt;

    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if line.is_empty() {
                McpInputLine::Eof
            } else {
                McpInputLine::Line(line)
            });
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            if line.len().saturating_add(newline) > MAX_MCP_REQUEST_LINE_BYTES {
                reader.consume(newline + 1);
                return Ok(McpInputLine::TooLarge);
            }
            line.extend_from_slice(&available[..newline]);
            reader.consume(newline + 1);
            return Ok(McpInputLine::Line(line));
        }
        let available_len = available.len();
        if line.len().saturating_add(available_len) > MAX_MCP_REQUEST_LINE_BYTES {
            reader.consume(available_len);
            loop {
                let remaining = reader.fill_buf().await?;
                if remaining.is_empty() {
                    return Ok(McpInputLine::TooLarge);
                }
                let remaining_len = remaining.len();
                if let Some(newline) = remaining.iter().position(|byte| *byte == b'\n') {
                    reader.consume(newline + 1);
                    return Ok(McpInputLine::TooLarge);
                }
                reader.consume(remaining_len);
            }
        }
        line.extend_from_slice(available);
        reader.consume(available_len);
    }
}

#[cfg(feature = "mcp")]
fn cmd_serve(base: &Path, port: u16, host: &str, stdio: bool) -> CliResult<()> {
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;
    use tokio::runtime::Runtime;

    let diagnostic_sink = serve_diagnostic_sink(stdio);

    print_info_to(
        diagnostic_sink,
        &format!("Starting server for '{}'", base.display()),
    );
    print_info_to(
        diagnostic_sink,
        &format!(
            "Base directory: {}",
            base.canonicalize()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| base.display().to_string())
        ),
    );

    if stdio {
        print_info_to(diagnostic_sink, "Transport: stdio (MCP)");
    } else {
        print_info_to(diagnostic_sink, "Transport: HTTP");
        print_info_to(diagnostic_sink, &format!("Listener: {}:{}", host, port));
    }

    let store = MemoryX::new(StoreConfig::new(base.to_path_buf()))
        .map_err(|e| CliError::Store(format!("Failed to open store: {}", e)))?;
    let mut mcp_state = McpServerState::new(base.to_path_buf(), store)?;

    let rt = Runtime::new().map_err(CliError::Io)?;

    rt.block_on(async {
        if stdio {
            // Stdio MCP transport
            let stdin = tokio::io::stdin();
            let mut stdout = tokio::io::stdout();
            let mut reader = tokio::io::BufReader::new(stdin);

            print_info_to(
                diagnostic_sink,
                "Stdio MCP server running. Send JSON-RPC requests.",
            );

            loop {
                match read_bounded_mcp_line(&mut reader).await {
                    Ok(McpInputLine::Line(bytes)) => {
                        let line = match String::from_utf8(bytes) {
                            Ok(line) => line,
                            Err(error) => {
                                let response = mcp_error(
                                    serde_json::Value::Null,
                                    -32700,
                                    format!("MCP request is not UTF-8: {error}"),
                                );
                                stdout
                                    .write_all(response.to_string().as_bytes())
                                    .await
                                    .map_err(CliError::Io)?;
                                stdout.write_all(b"\n").await.map_err(CliError::Io)?;
                                stdout.flush().await.map_err(CliError::Io)?;
                                continue;
                            }
                        };
                        if let Some(response) = process_mcp_request(&mut mcp_state, &line).await {
                            stdout
                                .write_all(response.as_bytes())
                                .await
                                .map_err(CliError::Io)?;
                            stdout.write_all(b"\n").await.map_err(CliError::Io)?;
                            stdout.flush().await.map_err(CliError::Io)?;
                        }
                    }
                    Ok(McpInputLine::TooLarge) => {
                        let response = mcp_error(
                            serde_json::Value::Null,
                            -32700,
                            format!("MCP request exceeds {MAX_MCP_REQUEST_LINE_BYTES} bytes"),
                        );
                        stdout
                            .write_all(response.to_string().as_bytes())
                            .await
                            .map_err(CliError::Io)?;
                        stdout.write_all(b"\n").await.map_err(CliError::Io)?;
                        stdout.flush().await.map_err(CliError::Io)?;
                    }
                    Ok(McpInputLine::Eof) => break,
                    Err(e) => {
                        eprintln!("Error reading stdin: {}", e);
                        break;
                    }
                }
            }
        } else {
            // HTTP Federation server
            use memoryx::federation::{FederationConfig, FederationServer, Gateway};

            let base_id = load_or_create_federation_base_id(base)?;
            let config = FederationConfig::new(base_id);
            let gateway = Arc::new(tokio::sync::RwLock::new(Gateway::new(
                std::sync::Arc::new(mcp_state.stores.remove("active").ok_or_else(|| {
                    CliError::Store("Active federation store missing".to_string())
                })?),
                config,
            )));

            // Parse socket address
            let addr_str = format!("{}:{}", host, port);
            let sock_addr: std::net::SocketAddr = addr_str.parse().map_err(|e| {
                CliError::Store(format!("Invalid listen address '{}': {}", addr_str, e))
            })?;

            let mut server = FederationServer::new(gateway, sock_addr);

            print_info_to(diagnostic_sink, "HTTP Federation server starting...");
            print_info_to(
                diagnostic_sink,
                "Routes: /fetch, /negotiate, /sync, /discover, /health",
            );

            server
                .start()
                .map_err(|e| CliError::Store(format!("Server error: {}", e)))?;

            // Block forever so server keeps running
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        }

        Ok(()) as CliResult<()>
    })?;

    Ok(())
}

#[cfg(not(feature = "mcp"))]
fn cmd_serve(_base: &Path, _port: u16, _host: &str, _stdio: bool) -> CliResult<()> {
    Err(CliError::Validation(
        "MCP server requires 'mcp' feature. Rebuild with --features mcp".to_string(),
    ))
}

#[cfg(feature = "mcp")]
const BASE_SELECTABLE_MCP_TOOLS: &[&str] = &[
    "query",
    "query_base",
    "explain_answer_graph",
    "get_provenance_path",
    "search_lex",
    "search_graph",
    "search_semantic",
    "ingest",
    "batch_ingest",
    "update_atom",
    "supersede_claim",
    "correct_claim",
    "delete_atom",
    "history",
    "register_source",
    "list_sources",
    "attach_atom_source",
    "register_predicate",
    "list_predicates",
    "get_predicate",
    "resolve_predicate",
    "create_entity",
    "list_entities",
    "alias_entity",
    "merge_entities",
    "split_entity",
    "add_claim",
    "assert_relation",
    "correct_relation",
    "create_context",
    "list_contexts",
    "branch_context",
    "list_conflicts",
    "graph_neighbors",
    "graph_walk",
    "extract_subgraph",
];

#[cfg(feature = "mcp")]
fn add_base_ref_to_store_tool_schemas(response: &mut serde_json::Value) {
    let Some(tools) = response
        .pointer_mut("/result/tools")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };

    for tool in tools {
        let is_base_selectable = tool
            .get("name")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|name| BASE_SELECTABLE_MCP_TOOLS.contains(&name));
        if !is_base_selectable {
            continue;
        }

        let Some(schema) = tool
            .get_mut("inputSchema")
            .and_then(serde_json::Value::as_object_mut)
        else {
            continue;
        };
        let Some(properties) = schema
            .get_mut("properties")
            .and_then(serde_json::Value::as_object_mut)
        else {
            continue;
        };

        properties.entry("base_ref".to_string()).or_insert_with(|| {
            serde_json::json!({
                "type": "string",
                "description": "Optional connected base_ref. Omit it to use the active base."
            })
        });

        // A static base_ref example would only be executable after that base
        // has been connected. Keep each tool's self-contained active-base
        // examples and document base_ref through the optional property.
    }
}

#[cfg(feature = "mcp")]
const SUPPORTED_MCP_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2024-11-05"];

#[cfg(feature = "mcp")]
fn query_contract_mcp_schema() -> serde_json::Value {
    let strength = serde_json::json!({
        "oneOf": [
            { "type": "string", "enum": ["must", "must_not"] },
            { "type": "object", "properties": {
                "should": { "type": "object", "properties": {
                    "weight": { "type": "number", "minimum": 0.0, "maximum": 1.0 }
                }, "required": ["weight"], "additionalProperties": false }
            }, "required": ["should"], "additionalProperties": false }
        ]
    });
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://memoryx.local/schemas/query-contract-1.0.5.json",
        "type": "object",
        "description": "Executable QueryContract. Omitted policy, output, and budget fields use documented MemoryX defaults.",
        "$defs": {
            "constraint_value": { "oneOf": [
                { "type": "string", "enum": ["none"] },
                { "type": "object", "properties": { "bool": { "type": "boolean" } }, "required": ["bool"], "additionalProperties": false },
                { "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"], "additionalProperties": false },
                { "type": "object", "properties": { "number": { "type": "number" } }, "required": ["number"], "additionalProperties": false },
                { "type": "object", "properties": { "ref": { "type": "string" } }, "required": ["ref"], "additionalProperties": false },
                { "type": "object", "properties": { "time_range": { "type": "object", "properties": {
                    "from_unix_ns": { "type": ["integer", "null"], "minimum": 0 },
                    "to_unix_ns": { "type": ["integer", "null"], "minimum": 0 }
                }, "additionalProperties": false } }, "required": ["time_range"], "additionalProperties": false },
                { "type": "object", "properties": { "list": { "type": "array", "items": { "$ref": "#/$defs/constraint_value" } } }, "required": ["list"], "additionalProperties": false }
            ] }
        },
        "properties": {
            "intent": { "type": "string", "enum": ["lookup", "verify", "explain", "derive", "compare", "plan", "define"] },
            "targets": { "type": "array", "maxItems": 256, "items": {
                "type": "object",
                "properties": {
                    "id": { "type": ["string", "null"] },
                    "label": { "type": ["string", "null"] },
                    "entity_type": { "type": ["string", "null"] },
                    "aliases": { "type": "array", "items": { "type": "string" } },
                    "domain_mask": { "type": ["integer", "null"], "minimum": 0 }
                },
                "additionalProperties": false
            } },
            "semantic_vectors": { "type": "array", "items": { "type": "array", "minItems": 1, "items": { "type": "number" } } },
            "relations": { "type": "array", "items": {
                "type": "object",
                "properties": {
                    "subject": { "type": "string" },
                    "predicate": { "type": "string" },
                    "object": { "type": "string" },
                    "strength": strength.clone()
                },
                "required": ["subject", "predicate", "object", "strength"],
                "additionalProperties": false
            } },
            "constraints": { "type": "array", "maxItems": 1024, "items": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "strength": strength,
                    "target": { "oneOf": [
                        { "type": "string", "enum": ["entity", "entity_type", "predicate", "relation", "source", "evidence", "time", "context", "domain", "numeric_metric", "text"] },
                        { "type": "object", "properties": { "custom": { "type": "string" } }, "required": ["custom"], "additionalProperties": false }
                    ] },
                    "operator": { "type": "string", "enum": ["eq", "ne", "contains", "exists", "matches", "before", "after", "during", "within", "gte", "lte"] },
                    "value": { "$ref": "#/$defs/constraint_value" },
                    "description": { "type": ["string", "null"] }
                },
                "required": ["id", "strength", "target", "operator", "value"],
                "additionalProperties": false
            } },
            "quantifiers": { "type": "array", "items": {
                "type": "object",
                "properties": {
                    "quantifier": { "oneOf": [
                        { "type": "string", "enum": ["all", "any"] },
                        { "type": "object", "properties": { "at_least": { "type": "integer", "minimum": 0 } }, "required": ["at_least"], "additionalProperties": false },
                        { "type": "object", "properties": { "exactly": { "type": "integer", "minimum": 0 } }, "required": ["exactly"], "additionalProperties": false }
                    ] },
                    "constraint_ids": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["quantifier", "constraint_ids"],
                "additionalProperties": false
            } },
            "temporal_scope": { "type": "object", "properties": {
                "time_range": { "type": ["object", "null"], "properties": {
                    "from_unix_ns": { "type": ["integer", "null"], "minimum": 0 },
                    "to_unix_ns": { "type": ["integer", "null"], "minimum": 0 }
                }, "additionalProperties": false },
                "before_unix_ns": { "type": ["integer", "null"], "minimum": 0 },
                "after_unix_ns": { "type": ["integer", "null"], "minimum": 0 },
                "valid_at_unix_ns": { "type": ["integer", "null"], "minimum": 0 },
                "observed_at_unix_ns": { "type": ["integer", "null"], "minimum": 0 },
                "latest_count": { "type": ["integer", "null"], "minimum": 0 },
                "require_current": { "type": "boolean" }
            }, "required": ["require_current"], "additionalProperties": false },
            "context_scope": { "type": "object", "properties": {
                "policy_id": { "type": ["integer", "null"], "minimum": 0 },
                "selectors": { "type": "array", "items": { "oneOf": [
                    { "type": "string", "enum": ["active", "user_global"] },
                    { "type": "object", "properties": { "named": { "type": "string" } }, "required": ["named"], "additionalProperties": false },
                    { "type": "object", "properties": { "branch": { "type": "string" } }, "required": ["branch"], "additionalProperties": false },
                    { "type": "object", "properties": { "project": { "type": "string" } }, "required": ["project"], "additionalProperties": false },
                    { "type": "object", "properties": { "assumption_set": { "type": "string" } }, "required": ["assumption_set"], "additionalProperties": false }
                ] } },
                "branch_ids": { "type": "array", "items": { "type": "string" } },
                "include_conflicting_branches": { "type": "boolean" }
            }, "required": ["branch_ids", "include_conflicting_branches"], "additionalProperties": false },
            "source_policy": { "type": "object", "properties": {
                "allowed_sources": { "type": "array", "items": { "type": "string" } },
                "forbidden_sources": { "type": "array", "items": { "type": "string" } },
                "require_provenance": { "type": "boolean" },
                "allow_federated_sources": { "type": "boolean" }
            }, "required": ["allowed_sources", "forbidden_sources", "require_provenance", "allow_federated_sources"], "additionalProperties": false },
            "evidence_policy": { "type": "object", "properties": {
                "min_evidence_items": { "type": "integer", "minimum": 0 },
                "require_direct_evidence": { "type": "boolean" },
                "allow_inferred_claims": { "type": "boolean" },
                "include_rejected_candidates": { "type": "boolean" }
            }, "required": ["min_evidence_items", "require_direct_evidence", "allow_inferred_claims", "include_rejected_candidates"], "additionalProperties": false },
            "freshness_policy": { "type": "object", "properties": {
                "max_age_unix_ns": { "type": ["integer", "null"], "minimum": 0 },
                "stale_behavior": { "type": "string", "enum": ["allow", "mark_stale", "reject"] }
            }, "required": ["stale_behavior"], "additionalProperties": false },
            "ambiguity_policy": { "type": "object", "properties": {
                "allow_ambiguous_targets": { "type": "boolean" },
                "require_disambiguation_notes": { "type": "boolean" }
            }, "required": ["allow_ambiguous_targets", "require_disambiguation_notes"], "additionalProperties": false },
            "conflict_policy": { "type": "object", "properties": {
                "mode": { "type": "string", "enum": ["fail", "branch", "include_alternatives", "prefer_trusted", "prefer_recent"] },
                "include_conflicts": { "type": "boolean" },
                "fail_on_hard_conflict": { "type": "boolean" },
                "prefer_latest_branch": { "type": "boolean" }
            }, "required": ["mode", "include_conflicts", "fail_on_hard_conflict", "prefer_latest_branch"], "additionalProperties": false },
            "completeness_policy": { "type": "object", "properties": {
                "require_minimal_proof_subgraph": { "type": "boolean" },
                "expose_unknowns": { "type": "boolean" },
                "expose_unsatisfied_constraints": { "type": "boolean" }
            }, "required": ["require_minimal_proof_subgraph", "expose_unknowns", "expose_unsatisfied_constraints"], "additionalProperties": false },
            "output_contract": { "type": "object", "properties": {
                "format": { "type": "string", "enum": ["structured_json", "text_summary", "evidence_table", "minimal_graph"] },
                "include_answer_graph": { "type": "boolean" },
                "include_confidence": { "type": "boolean" },
                "include_provenance": { "type": "boolean" },
                "include_execution_trace": { "type": "boolean" },
                "max_items": { "type": "integer", "minimum": 1, "maximum": 4096 },
                "max_bytes": { "type": "integer", "minimum": 2048, "maximum": 8388608 }
            }, "additionalProperties": false },
            "budgets": { "type": "object", "properties": {
                "max_iterations": { "type": "integer", "minimum": 1, "maximum": 1000 },
                "max_atoms": { "type": "integer", "minimum": 1, "maximum": 65536 },
                "max_edges": { "type": "integer", "minimum": 0, "maximum": 262144 },
                "max_io_bytes": { "type": "integer", "minimum": 0, "maximum": 1073741824 },
                "max_time_ms": { "type": "integer", "minimum": 0, "maximum": 300000 },
                "max_federated_calls": { "type": "integer", "minimum": 0, "maximum": 128, "description": "Maximum remote retrieval calls; local MCP query execution performs zero." }
            }, "additionalProperties": false }
        },
        "required": ["intent"],
        "additionalProperties": false
    })
}

/// MCP initialize policy: echo an explicitly supported client date-version.
/// Missing, non-string, and non-allowlisted versions are rejected rather than
/// claiming compatibility with an arbitrary protocol revision.
#[cfg(feature = "mcp")]
fn mcp_initialize_response(
    request: &serde_json::Value,
    id: serde_json::Value,
) -> serde_json::Value {
    let requested_version = request.pointer("/params/protocolVersion");
    let protocol_version = match requested_version {
        Some(serde_json::Value::String(version))
            if SUPPORTED_MCP_PROTOCOL_VERSIONS.contains(&version.as_str()) =>
        {
            version
        }
        Some(serde_json::Value::String(version)) => {
            return serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32602,
                    "message": format!(
                        "Unsupported MCP protocolVersion '{version}'"
                    ),
                    "data": {
                        "supportedProtocolVersions": SUPPORTED_MCP_PROTOCOL_VERSIONS
                    }
                }
            });
        }
        Some(_) => {
            return serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32602,
                    "message": "Invalid initialize params: protocolVersion must be a string",
                    "data": {
                        "supportedProtocolVersions": SUPPORTED_MCP_PROTOCOL_VERSIONS
                    }
                }
            });
        }
        None => {
            return serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32602,
                    "message": "Invalid initialize params: protocolVersion is required",
                    "data": {
                        "supportedProtocolVersions": SUPPORTED_MCP_PROTOCOL_VERSIONS
                    }
                }
            });
        }
    };

    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": protocol_version,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "memoryx",
                "version": env!("CARGO_PKG_VERSION")
            }
        }
    })
}

/// Process MCP JSON-RPC request
#[cfg(feature = "mcp")]
async fn process_mcp_request(state: &mut McpServerState, request: &str) -> Option<String> {
    let result: serde_json::Result<serde_json::Value> = serde_json::from_str(request);
    match result {
        Ok(req) => {
            let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let is_notification = req.get("id").is_none()
                && req
                    .get("method")
                    .and_then(serde_json::Value::as_str)
                    .is_some();
            let id = req.get("id").cloned().unwrap_or(serde_json::json!(null));

            let mut resp = match method {
                "initialize" => mcp_initialize_response(&req, id),
                "notifications/initialized" => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {}
                }),
                "tools/list" => serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "tools": [
                                    {
                                        "name": "query",
                                        "description": "Run the fixed-point solver against the active base or a base selected by optional base_ref. Input must provide either query_text/question for natural-language compilation or contract for strict QueryContract execution. Returns an AnswerPack-shaped JSON payload, not a free-form text answer.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "query_text": { "type": "string" },
                                                "question": { "type": "string" },
                                                "contract": query_contract_mcp_schema(),
                                                "base_ref": { "type": "string" },
                                                "ctx_id": { "type": "integer" }
                                            },
                                            "oneOf": [
                                                { "required": ["query_text"] },
                                                { "required": ["question"] },
                                                { "required": ["contract"] }
                                            ],
                                            "examples": [
                                                {
                                                    "query_text": "What decisions mention MemoryX persistence?",
                                                    "ctx_id": 0
                                                },
                                                {
                                                    "contract": {
                                                        "intent": "lookup",
                                                        "targets": [
                                                            { "label": "term:1" }
                                                        ],
                                                        "relations": [],
                                                        "constraints": []
                                                    },
                                                    "ctx_id": 0
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "list_bases",
                                        "description": "List project/user bases visible to this MCP process plus bases already connected in the multi-base registry.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {},
                                            "examples": [{}]
                                        }
                                    },
                                    {
                                        "name": "active_base",
                                        "description": "Return the currently active base_ref used by tools when base_ref is omitted.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {},
                                            "examples": [{}]
                                        }
                                    },
                                    {
                                        "name": "connect_base",
                                        "description": "Connect a project/user/path base to this MCP process. Use base_ref later to query or write that base without starting another MCP server.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "base_ref": { "type": "string" },
                                                "scope": { "type": "string", "enum": ["project", "user"] },
                                                "name": { "type": "string" },
                                                "path": { "type": "string" }
                                            },
                                            "examples": [
                                                { "base_ref": "project:client-a", "scope": "project", "name": "client-a" },
                                                { "base_ref": "global", "scope": "user", "name": "default" }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "switch_base",
                                        "description": "Switch the active base_ref for subsequent MCP calls that omit base_ref.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "base_ref": { "type": "string" }
                                            },
                                            "required": ["base_ref"],
                                            "examples": [
                                                { "base_ref": "project:client-a" }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "query_base",
                                        "description": "Explicit multi-base query. Requires base_ref and otherwise accepts the same arguments as query.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "base_ref": { "type": "string" },
                                                "query_text": { "type": "string" },
                                                "question": { "type": "string" },
                                                "contract": query_contract_mcp_schema(),
                                                "ctx_id": { "type": "integer" }
                                            },
                                            "required": ["base_ref"],
                                            "examples": [
                                                { "base_ref": "project:client-a", "query_text": "What decisions mention persistence?", "ctx_id": 0 }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "compile_query_contract",
                                        "description": "Compile natural-language query_text into the explicit QueryContract JSON that an agent can inspect, edit, validate, and pass back to query.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "query_text": { "type": "string" }
                                            },
                                            "required": ["query_text"],
                                            "examples": [
                                                {
                                                    "query_text": "Explain MemoryX MCP and require provenance"
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "validate_query_contract",
                                        "description": "Validate a QueryContract object without executing it. Returns valid=true or a concrete validation error.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "contract": query_contract_mcp_schema()
                                            },
                                            "required": ["contract"],
                                            "examples": [
                                                {
                                                    "contract": {
                                                        "intent": "lookup",
                                                        "targets": [{ "label": "term:1" }],
                                                        "relations": [],
                                                        "constraints": [],
                                                        "quantifiers": [],
                                                        "temporal_scope": { "require_current": false },
                                                        "context_scope": { "branch_ids": [], "include_conflicting_branches": true },
                                                        "source_policy": { "allowed_sources": [], "forbidden_sources": [], "require_provenance": true, "allow_federated_sources": true },
                                                        "evidence_policy": { "min_evidence_items": 0, "require_direct_evidence": false, "allow_inferred_claims": true, "include_rejected_candidates": true },
                                                        "freshness_policy": { "max_age_unix_ns": null, "stale_behavior": "mark_stale" },
                                                        "ambiguity_policy": { "allow_ambiguous_targets": true, "require_disambiguation_notes": true },
                                                        "conflict_policy": { "mode": "branch", "include_conflicts": true, "fail_on_hard_conflict": false, "prefer_latest_branch": false },
                                                        "completeness_policy": { "require_minimal_proof_subgraph": true, "expose_unknowns": true, "expose_unsatisfied_constraints": true },
                                                        "output_contract": { "format": "structured_json", "include_answer_graph": true, "include_confidence": true, "include_provenance": true, "include_execution_trace": false, "max_items": 1, "max_bytes": 2048 },
                                                        "budgets": { "max_iterations": 1, "max_atoms": 1, "max_edges": 0, "max_io_bytes": 0, "max_time_ms": 0, "max_federated_calls": 0 }
                                                    }
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "explain_answer_graph",
                                        "description": "Execute query_text or contract and return only the answer graph explanation fields: status, snapshot, graph counts, branch lineage, coverage, retrieval trace, and rejected candidates.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "query_text": { "type": "string" },
                                                "contract": query_contract_mcp_schema(),
                                                "ctx_id": { "type": "integer" }
                                            },
                                            "oneOf": [
                                                { "required": ["query_text"] },
                                                { "required": ["contract"] }
                                            ],
                                            "examples": [
                                                {
                                                    "query_text": "Find facts about MemoryX persistence",
                                                    "ctx_id": 0
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "get_provenance_path",
                                        "description": "Return the proof-grade ProvenanceChain for one atom_id, including derivation nodes, DERIVED_FROM edges, direct evidence links, confidence, and trust.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "atom_id": { "type": "string" }
                                            },
                                            "required": ["atom_id"],
                                            "examples": [
                                                {
                                                    "atom_id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "search_lex",
                                        "description": "Return atoms whose indexed terms match the requested lexical term using the inverted index; this is exact term lookup, not semantic ranking.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "term": { "type": "string" },
                                                "min_trust": { "type": "integer" },
                                                "domain_mask": { "type": "integer" }
                                            },
                                            "required": ["term"],
                                            "examples": [
                                                {
                                                    "term": "memoryx",
                                                    "min_trust": 1000,
                                                    "domain_mask": 65535
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "search_graph",
                                        "description": "Match graph edges against the literal pattern 'src -> EDGE_TYPE -> dst', where either node id or the edge type can be '*' as a wildcard.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "pattern": { "type": "string" },
                                                "limit": { "type": "integer" }
                                            },
                                            "required": ["pattern"],
                                            "examples": [
                                                {
                                                    "pattern": "42 -> DEPENDS_ON -> *",
                                                    "limit": 10
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "search_semantic",
                                        "description": "Perform ANN vector search over embeddings and return nearest atoms filtered by trust or domain mask; this is vector retrieval, not keyword lookup.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "vector": {
                                                    "type": "array",
                                                    "items": { "type": "number" }
                                                },
                                                "min_trust": { "type": "integer" },
                                                "domain_mask": { "type": "integer" }
                                            },
                                            "required": ["vector"],
                                            "examples": [
                                                {
                                                    "vector": [0.12, -0.44, 0.88],
                                                    "min_trust": 1000,
                                                    "domain_mask": 65535
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "ingest",
                                        "description": "Create one atom from claim tuples and write it to the current base; each claim must use numeric fields {subj, pred, obj_tag, obj_val, qualifiers_mask}.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "atom_type": { "type": "string" },
                                                "claims": { "type": "array" },
                                                "symbols": {
                                                    "type": "array",
                                                    "items": { "type": "string" }
                                                },
                                                "trust_level": { "type": "integer" },
                                                "domain_mask": { "type": "integer" }
                                            },
                                            "required": ["atom_type", "claims"],
                                            "examples": [
                                                {
                                                    "atom_type": "FACT",
                                                    "claims": [
                                                        {
                                                            "subj": 1,
                                                            "pred": 2,
                                                            "obj_tag": 0,
                                                            "obj_val": 3,
                                                            "qualifiers_mask": 0
                                                        }
                                                    ],
                                                    "symbols": ["memoryx", "persistence"],
                                                    "trust_level": 5000,
                                                    "domain_mask": 65535
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "batch_ingest",
                                        "description": "Create multiple atoms in one call; each batch item uses the same payload shape as ingest and becomes a separate stored atom.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "atoms": { "type": "array" }
                                            },
                                            "required": ["atoms"],
                                            "examples": [
                                                {
                                                    "atoms": [
                                                        {
                                                            "atom_type": "FACT",
                                                            "claims": [
                                                                {
                                                                    "subj": 1,
                                                                    "pred": 2,
                                                                    "obj_tag": 0,
                                                                    "obj_val": 3,
                                                                    "qualifiers_mask": 0
                                                                }
                                                            ],
                                                            "symbols": ["memoryx"]
                                                        },
                                                        {
                                                            "atom_type": "DECISION",
                                                            "claims": [
                                                                {
                                                                    "subj": 4,
                                                                    "pred": 5,
                                                                    "obj_tag": 0,
                                                                    "obj_val": 6,
                                                                    "qualifiers_mask": 0
                                                                }
                                                            ],
                                                            "symbols": ["global", "projects"]
                                                        }
                                                    ]
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "update_atom",
                                        "description": "Create a new version of an existing atom id and preserve the old version as superseded provenance.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "atom_id": { "type": "string" },
                                                "atom_type": { "type": "string" },
                                                "claims": { "type": "array" },
                                                "symbols": {
                                                    "type": "array",
                                                    "items": { "type": "string" }
                                                }
                                            },
                                            "required": ["atom_id", "atom_type", "claims"],
                                            "examples": [
                                                {
                                                    "atom_id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                                                    "atom_type": "DECISION",
                                                    "claims": [
                                                        {
                                                            "subj": 1,
                                                            "pred": 2,
                                                            "obj_tag": 0,
                                                            "obj_val": 7,
                                                            "qualifiers_mask": 0
                                                        }
                                                    ],
                                                    "symbols": ["memoryx", "mcp"]
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "supersede_claim",
                                        "description": "Supersede a claim-bearing atom by writing a new atom version; this is the claim-level MCP alias for update_atom and preserves provenance.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "atom_id": { "type": "string" },
                                                "atom_type": { "type": "string" },
                                                "claims": { "type": "array" },
                                                "symbols": {
                                                    "type": "array",
                                                    "items": { "type": "string" }
                                                }
                                            },
                                            "required": ["atom_id", "atom_type", "claims"],
                                            "examples": [
                                                {
                                                    "atom_id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                                                    "atom_type": "FACT",
                                                    "claims": [
                                                        {
                                                            "subj": 1,
                                                            "pred": 2,
                                                            "obj_tag": 0,
                                                            "obj_val": 8,
                                                            "qualifiers_mask": 0
                                                        }
                                                    ],
                                                    "symbols": ["corrected", "claim"]
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "correct_claim",
                                        "description": "Correct a claim-bearing atom by writing a superseding atom version; no old content is erased.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "atom_id": { "type": "string" },
                                                "atom_type": { "type": "string" },
                                                "claims": { "type": "array" },
                                                "symbols": {
                                                    "type": "array",
                                                    "items": { "type": "string" }
                                                }
                                            },
                                            "required": ["atom_id", "atom_type", "claims"],
                                            "examples": [
                                                {
                                                    "atom_id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                                                    "atom_type": "FACT",
                                                    "claims": [
                                                        {
                                                            "subj": 1,
                                                            "pred": 2,
                                                            "obj_tag": 0,
                                                            "obj_val": 9,
                                                            "qualifiers_mask": 0
                                                        }
                                                    ],
                                                    "symbols": ["corrected", "claim"]
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "delete_atom",
                                        "description": "Create a tombstone for the given atom id and preserve deletion provenance instead of physically erasing the atom.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "atom_id": { "type": "string" },
                                                "reason": { "type": "string" }
                                            },
                                            "required": ["atom_id"],
                                            "examples": [
                                                {
                                                    "atom_id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                                                    "reason": "Superseded by a later decision"
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "history",
                                        "description": "Return newest-first durable write-operation history from the active base; use this to inspect recent ingest, update, delete, repair, and rebuild actions.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "limit": { "type": "integer" }
                                            },
                                            "examples": [
                                                {
                                                    "limit": 20
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "register_source",
                                        "description": "Register a durable provenance source such as a file, page, repository, commit, API response, message, table, measurement, human, or agent.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "kind": { "type": "string" },
                                                "label": { "type": "string" },
                                                "path": { "type": "string" },
                                                "url": { "type": "string" },
                                                "commit_hash": { "type": "string" },
                                                "line_start": { "type": "integer" },
                                                "line_end": { "type": "integer" },
                                                "source_version": { "type": "string" }
                                            },
                                            "required": ["kind", "label"],
                                            "examples": [
                                                {
                                                    "kind": "file",
                                                    "label": "SKF concept",
                                                    "path": "Concept/SKF.txt",
                                                    "line_start": 1,
                                                    "line_end": 40,
                                                    "source_version": "draft"
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "list_sources",
                                        "description": "List durable provenance sources registered in the active base with their exact location/version metadata.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {},
                                            "examples": [
                                                {}
                                            ]
                                        }
                                    },
                                    {
                                        "name": "attach_atom_source",
                                        "description": "Accumulate one existing registered source id on an atom. Distinct sources are preserved durably; repeating the same atom/source pair is idempotent and never replaces prior attachments.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "atom_id": { "type": "string" },
                                                "source_id": { "type": "integer" }
                                            },
                                            "required": ["atom_id", "source_id"],
                                            "examples": [
                                                {
                                                    "atom_id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                                                    "source_id": 1
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "register_predicate",
                                        "description": "Register an immutable project predicate contract and return its durable managed numeric ID. Repeating the identical contract is idempotent; reusing its stable key or canonical name with different semantics fails closed.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "stable_key": { "type": "string", "description": "Project-qualified immutable key, for example hpf:depends_on." },
                                                "canonical_name": { "type": "string" },
                                                "description": { "type": "string", "description": "Precise semantic contract for authors and reviewers." },
                                                "direction": { "type": "string", "enum": ["directed", "symmetric"] },
                                                "inverse_stable_key": { "type": "string" },
                                                "cardinality": { "type": "string", "enum": ["one_to_one", "one_to_many", "many_to_one", "many_to_many"] }
                                            },
                                            "required": ["stable_key", "canonical_name", "description"],
                                            "examples": [{
                                                "stable_key": "project:depends_on",
                                                "canonical_name": "depends_on",
                                                "description": "Subject requires object before it can be completed.",
                                                "direction": "directed",
                                                "inverse_stable_key": "project:required_by",
                                                "cardinality": "many_to_many"
                                            }]
                                        }
                                    },
                                    {
                                        "name": "list_predicates",
                                        "description": "List every managed predicate ID and its complete immutable semantic contract in the selected base.",
                                        "inputSchema": { "type": "object", "properties": {}, "examples": [{}] }
                                    },
                                    {
                                        "name": "get_predicate",
                                        "description": "Inspect one managed predicate contract by its durable numeric predicate ID before using add_claim or assert_relation.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": { "predicate_id": { "type": "integer" } },
                                            "required": ["predicate_id"],
                                            "examples": [{ "predicate_id": 2147483648u64 }]
                                        }
                                    },
                                    {
                                        "name": "resolve_predicate",
                                        "description": "Resolve an exact stable key or canonical predicate name to its durable numeric ID and full contract; this operation never creates a predicate.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": { "name_or_key": { "type": "string" } },
                                            "required": ["name_or_key"],
                                            "examples": [{ "name_or_key": "project:depends_on" }]
                                        }
                                    },
                                    {
                                        "name": "create_entity",
                                        "description": "Create a high-level entity record for authoring knowledge without manually constructing atoms.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "canonical_name": { "type": "string" },
                                                "entity_type": { "type": "string" }
                                            },
                                            "required": ["canonical_name", "entity_type"],
                                            "examples": [
                                                {
                                                    "canonical_name": "MemoryX",
                                                    "entity_type": "project"
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "list_entities",
                                        "description": "List latest high-level entity records with canonical names, aliases, type, and merge/split lineage.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {},
                                            "examples": [
                                                {}
                                            ]
                                        }
                                    },
                                    {
                                        "name": "alias_entity",
                                        "description": "Add an alias to an existing high-level entity record.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "entity_id": { "type": "integer" },
                                                "alias": { "type": "string" }
                                            },
                                            "required": ["entity_id", "alias"],
                                            "examples": [
                                                {
                                                    "entity_id": 1,
                                                    "alias": "memoryx-db"
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "merge_entities",
                                        "description": "Merge a source entity into a target entity, preserving aliases, claims, and merge lineage.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "target_entity": { "type": "integer" },
                                                "source_entity": { "type": "integer" }
                                            },
                                            "required": ["target_entity", "source_entity"],
                                            "examples": [
                                                {
                                                    "target_entity": 1,
                                                    "source_entity": 2
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "split_entity",
                                        "description": "Create a new entity split from an existing source entity while preserving split lineage.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "source_entity": { "type": "integer" },
                                                "canonical_name": { "type": "string" },
                                                "entity_type": { "type": "string" }
                                            },
                                            "required": ["source_entity", "canonical_name", "entity_type"],
                                            "examples": [
                                                {
                                                    "source_entity": 1,
                                                    "canonical_name": "MemoryX CLI",
                                                    "entity_type": "component"
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "add_claim",
                                        "description": "Add a semi-structured atom-backed claim to an existing high-level entity; this writes an atom and asserts it in the selected context.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "entity_id": { "type": "integer" },
                                                "predicate": { "type": "integer" },
                                                "object": { "type": "integer" },
                                                "object_tag": { "type": "string" },
                                                "ctx_id": { "type": "integer" }
                                            },
                                            "required": ["entity_id", "predicate", "object"],
                                            "examples": [
                                                {
                                                    "entity_id": 1,
                                                    "predicate": 7,
                                                    "object": 4090,
                                                    "object_tag": "U64",
                                                    "ctx_id": 0
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "assert_relation",
                                        "description": "Create an atom-backed relation claim between two high-level entities and assert it in a context.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "subject": { "type": "integer" },
                                                "predicate": { "type": "integer" },
                                                "object": { "type": "integer" },
                                                "ctx_id": { "type": "integer" }
                                            },
                                            "required": ["subject", "predicate", "object"],
                                            "examples": [
                                                {
                                                    "subject": 1,
                                                    "predicate": 42,
                                                    "object": 2,
                                                    "ctx_id": 0
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "correct_relation",
                                        "description": "Correct an existing relation by writing a superseding atom-backed relation.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "relation_id": { "type": "integer" },
                                                "subject": { "type": "integer" },
                                                "predicate": { "type": "integer" },
                                                "object": { "type": "integer" },
                                                "ctx_id": { "type": "integer" }
                                            },
                                            "required": ["relation_id", "subject", "predicate", "object"],
                                            "examples": [
                                                {
                                                    "relation_id": 1,
                                                    "subject": 1,
                                                    "predicate": 42,
                                                    "object": 3,
                                                    "ctx_id": 0
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "create_context",
                                        "description": "Create a new context in the active base, optionally using the provided policy id when you need a specific policy branch.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "policy_id": { "type": "integer" }
                                            },
                                            "examples": [
                                                {
                                                    "policy_id": 1
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "list_contexts",
                                        "description": "Return every context in the base with its id, parent, policy, state, and claim counts.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {},
                                            "examples": [
                                                {}
                                            ]
                                        }
                                    },
                                    {
                                        "name": "branch_context",
                                        "description": "Create a child context from an existing live parent context id and optionally assign a new policy id and branch reason.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "parent_ctx": { "type": "integer" },
                                                "reason": { "type": "string" },
                                                "policy_id": { "type": "integer" }
                                            },
                                            "required": ["parent_ctx"],
                                            "examples": [
                                                {
                                                    "parent_ctx": 1,
                                                    "policy_id": 2,
                                                    "reason": "Project-specific working branch"
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "list_conflicts",
                                        "description": "Return all conflict records for the requested context id, or for the active context when ctx_id is omitted.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "ctx_id": { "type": "integer" }
                                            },
                                            "examples": [
                                                {
                                                    "ctx_id": 1
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "graph_neighbors",
                                        "description": "Return the direct incoming and outgoing neighbors of one graph node, optionally filtered by edge type; this is a one-hop query only.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "node_num": { "type": "integer" },
                                                "edge_types": {
                                                    "type": "array",
                                                    "items": { "type": "string" }
                                                }
                                            },
                                            "required": ["node_num"],
                                            "examples": [
                                                {
                                                    "node_num": 42,
                                                    "edge_types": ["DEPENDS_ON", "SUPPORTS"]
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "graph_walk",
                                        "description": "Traverse the graph from the provided seed nodes up to the requested depth and return visited edges; this is multi-hop traversal, not subgraph extraction.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "seed_nodes": {
                                                    "type": "array",
                                                    "items": { "type": "integer" }
                                                },
                                                "edge_types": {
                                                    "type": "array",
                                                    "items": { "type": "string" }
                                                },
                                                "depth": { "type": "integer" }
                                            },
                                            "required": ["seed_nodes"],
                                            "examples": [
                                                {
                                                    "seed_nodes": [42],
                                                    "edge_types": ["DEPENDS_ON"],
                                                    "depth": 2
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "name": "extract_subgraph",
                                        "description": "Extract the bounded neighborhood around one center node using graph traversal within the requested radius.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "center_node": { "type": "integer" },
                                                "radius": { "type": "integer" },
                                                "edge_types": {
                                                    "type": "array",
                                                    "items": { "type": "string" }
                                                }
                                            },
                                            "required": ["center_node"],
                                            "examples": [
                                                {
                                                    "center_node": 42,
                                                    "radius": 2,
                                                    "edge_types": ["DEPENDS_ON", "SUPPORTS"]
                                                }
                                            ]
                                        }
                                    }
                                ]
                    }
                }),
                "tools/call" => {
                    let tool_name = req
                        .get("params")
                        .and_then(|p| p.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("");
                    let arguments = req.get("params").and_then(|p| p.get("arguments"));

                    match tool_name {
                        "list_bases" => mcp_list_bases_response(state, id),
                        "active_base" => mcp_active_base_response(state, id),
                        "connect_base" => mcp_connect_base_response(state, id, arguments),
                        "switch_base" => mcp_switch_base_response(state, id, arguments),
                        "query" => match mcp_store_for_arguments(state, id.clone(), arguments) {
                            Ok((_base_ref, store)) => mcp_query_response(store, id, arguments),
                            Err(err) => err,
                        },
                        "query_base" => {
                            if let Some(args) = arguments.and_then(|value| value.as_object()) {
                                if !args.get("base_ref").is_some_and(|value| value.is_string()) {
                                    mcp_error(
                                        id,
                                        -32602,
                                        "Missing required string field 'base_ref'",
                                    )
                                } else {
                                    match mcp_store_for_arguments(state, id.clone(), arguments) {
                                        Ok((_base_ref, store)) => {
                                            mcp_query_response(store, id, arguments)
                                        }
                                        Err(err) => err,
                                    }
                                }
                            } else {
                                mcp_error(id, -32602, "Missing tool arguments")
                            }
                        }
                        "compile_query_contract" => {
                            mcp_compile_query_contract_response(id, arguments)
                        }
                        "validate_query_contract" => {
                            mcp_validate_query_contract_response(id, arguments)
                        }
                        "explain_answer_graph" => mcp_with_selected_store(
                            state,
                            id,
                            arguments,
                            mcp_explain_answer_graph_response,
                        ),
                        "get_provenance_path" => mcp_with_selected_store(
                            state,
                            id,
                            arguments,
                            mcp_get_provenance_path_response,
                        ),
                        "search_lex" => {
                            mcp_with_selected_store(state, id, arguments, mcp_search_lex_response)
                        }
                        "search_graph" => {
                            mcp_with_selected_store(state, id, arguments, mcp_search_graph_response)
                        }
                        "search_semantic" => mcp_with_selected_store(
                            state,
                            id,
                            arguments,
                            mcp_search_semantic_response,
                        ),
                        "ingest" => {
                            mcp_with_selected_store(state, id, arguments, mcp_ingest_response)
                        }
                        "batch_ingest" => {
                            mcp_with_selected_store(state, id, arguments, mcp_batch_ingest_response)
                        }
                        "update_atom" => {
                            mcp_with_selected_store(state, id, arguments, mcp_update_atom_response)
                        }
                        "supersede_claim" => {
                            mcp_with_selected_store(state, id, arguments, mcp_update_atom_response)
                        }
                        "correct_claim" => {
                            mcp_with_selected_store(state, id, arguments, mcp_update_atom_response)
                        }
                        "delete_atom" => {
                            mcp_with_selected_store(state, id, arguments, mcp_delete_atom_response)
                        }
                        "history" => {
                            mcp_with_selected_store(state, id, arguments, mcp_history_response)
                        }
                        "register_source" => mcp_with_selected_store(
                            state,
                            id,
                            arguments,
                            mcp_register_source_response,
                        ),
                        "list_sources" => {
                            mcp_with_selected_store(state, id, arguments, mcp_list_sources_response)
                        }
                        "attach_atom_source" => mcp_with_selected_store(
                            state,
                            id,
                            arguments,
                            mcp_attach_atom_source_response,
                        ),
                        "register_predicate" => mcp_with_selected_store(
                            state,
                            id,
                            arguments,
                            mcp_register_predicate_response,
                        ),
                        "list_predicates" => mcp_with_selected_store(
                            state,
                            id,
                            arguments,
                            mcp_list_predicates_response,
                        ),
                        "get_predicate" => mcp_with_selected_store(
                            state,
                            id,
                            arguments,
                            mcp_get_predicate_response,
                        ),
                        "resolve_predicate" => mcp_with_selected_store(
                            state,
                            id,
                            arguments,
                            mcp_resolve_predicate_response,
                        ),
                        "create_entity" => mcp_with_selected_store(
                            state,
                            id,
                            arguments,
                            mcp_create_entity_response,
                        ),
                        "list_entities" => mcp_with_selected_store(
                            state,
                            id,
                            arguments,
                            mcp_list_entities_response,
                        ),
                        "alias_entity" => {
                            mcp_with_selected_store(state, id, arguments, mcp_alias_entity_response)
                        }
                        "merge_entities" => mcp_with_selected_store(
                            state,
                            id,
                            arguments,
                            mcp_merge_entities_response,
                        ),
                        "split_entity" => {
                            mcp_with_selected_store(state, id, arguments, mcp_split_entity_response)
                        }
                        "add_claim" => {
                            mcp_with_selected_store(state, id, arguments, mcp_add_claim_response)
                        }
                        "assert_relation" => mcp_with_selected_store(
                            state,
                            id,
                            arguments,
                            mcp_assert_relation_response,
                        ),
                        "correct_relation" => mcp_with_selected_store(
                            state,
                            id,
                            arguments,
                            mcp_correct_relation_response,
                        ),
                        "create_context" => mcp_with_selected_store(
                            state,
                            id,
                            arguments,
                            mcp_create_context_response,
                        ),
                        "list_contexts" => mcp_with_selected_store(
                            state,
                            id,
                            arguments,
                            mcp_list_contexts_response,
                        ),
                        "branch_context" => mcp_with_selected_store(
                            state,
                            id,
                            arguments,
                            mcp_branch_context_response,
                        ),
                        "list_conflicts" => mcp_with_selected_store(
                            state,
                            id,
                            arguments,
                            mcp_list_conflicts_response,
                        ),
                        "graph_neighbors" => mcp_with_selected_store(
                            state,
                            id,
                            arguments,
                            mcp_graph_neighbors_response,
                        ),
                        "graph_walk" => {
                            mcp_with_selected_store(state, id, arguments, mcp_graph_walk_response)
                        }
                        "extract_subgraph" => mcp_with_selected_store(
                            state,
                            id,
                            arguments,
                            mcp_extract_subgraph_response,
                        ),
                        _ => serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32601, "message": "Tool not found" }
                        }),
                    }
                }
                _ => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "Method not found" }
                }),
            };

            if method == "tools/list" {
                add_base_ref_to_store_tool_schemas(&mut resp);
            }

            if is_notification {
                None
            } else {
                Some(serde_json::to_string(&resp).unwrap_or_else(|_| "{}".to_string()))
            }
        }
        Err(_) => Some(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32700, "message": "Parse error" }
            })
            .to_string(),
        ),
    }
}

#[cfg(feature = "mcp")]
fn mcp_text_result(id: serde_json::Value, text: String) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{"type": "text", "text": text}]
        }
    })
}

#[cfg(feature = "mcp")]
fn mcp_answer_result(
    id: serde_json::Value,
    answer: &memoryx::store::api::AnswerPack,
) -> serde_json::Value {
    let mut payload = answer_pack_json(answer);
    let mut response = mcp_text_result(id.clone(), payload.to_string());
    for _ in 0..4 {
        let emitted = response.to_string().len();
        if payload["response_limits"]["emitted_bytes"].as_u64() == Some(emitted as u64) {
            break;
        }
        payload["response_limits"]["emitted_bytes"] = serde_json::json!(emitted);
        response = mcp_text_result(id.clone(), payload.to_string());
    }
    let original_bytes = response.to_string().len();
    if original_bytes <= answer.response_limits.max_bytes as usize {
        return response;
    }
    let payload = serde_json::json!({
        "status": "Partial",
        "selected_ctx": answer.selected_ctx,
        "limitations": [{
            "code": "BudgetExhausted",
            "description": "MCP response framing exceeded output_contract.max_bytes; result collections were omitted from this response only and remain durable in the base",
            "severity": "Warning"
        }],
        "response_limits": {
            "max_items": answer.response_limits.max_items,
            "max_bytes": answer.response_limits.max_bytes,
            "items_truncated": answer.response_limits.items_truncated,
            "bytes_truncated": true,
            "original_items": answer.response_limits.original_items,
            "retained_items": 0,
            "original_bytes": original_bytes,
            "emitted_bytes": null
        }
    });
    let mut payload = payload;
    let mut bounded = mcp_text_result(id.clone(), payload.to_string());
    for _ in 0..4 {
        let emitted = bounded.to_string().len();
        if payload["response_limits"]["emitted_bytes"].as_u64() == Some(emitted as u64) {
            break;
        }
        payload["response_limits"]["emitted_bytes"] = serde_json::json!(emitted);
        bounded = mcp_text_result(id.clone(), payload.to_string());
    }
    debug_assert!(bounded.to_string().len() <= answer.response_limits.max_bytes as usize);
    bounded
}

#[cfg(feature = "mcp")]
fn mcp_error(id: serde_json::Value, code: i64, message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message.into()
        }
    })
}

#[cfg(feature = "mcp")]
fn mcp_arguments_object(
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> Result<&serde_json::Map<String, serde_json::Value>, serde_json::Value> {
    arguments
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| mcp_error(id, -32602, "Tool arguments must be an object"))
}

#[cfg(feature = "mcp")]
fn mcp_store_for_arguments<'a>(
    state: &'a mut McpServerState,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> Result<(String, &'a mut MemoryX), serde_json::Value> {
    state
        .base_for_args(arguments)
        .map_err(|err| mcp_error(id, -32602, err))
}

#[cfg(feature = "mcp")]
fn mcp_with_selected_store(
    state: &mut McpServerState,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
    handler: fn(&mut MemoryX, serde_json::Value, Option<&serde_json::Value>) -> serde_json::Value,
) -> serde_json::Value {
    match mcp_store_for_arguments(state, id.clone(), arguments) {
        Ok((_base_ref, store)) => handler(store, id, arguments),
        Err(err) => err,
    }
}

#[cfg(feature = "mcp")]
fn mcp_list_bases_response(state: &mut McpServerState, id: serde_json::Value) -> serde_json::Value {
    let bases = state.list_bases();
    mcp_text_result(
        id,
        serde_json::to_string_pretty(&serde_json::json!({
            "active_base_ref": state.active_base_ref,
            "bases": bases,
        }))
        .unwrap_or_default(),
    )
}

#[cfg(feature = "mcp")]
fn mcp_active_base_response(state: &McpServerState, id: serde_json::Value) -> serde_json::Value {
    mcp_text_result(
        id,
        serde_json::to_string_pretty(&serde_json::json!({
            "active_base_ref": state.active_base_ref,
            "active_base": state.active_base(),
        }))
        .unwrap_or_default(),
    )
}

#[cfg(feature = "mcp")]
fn mcp_parse_scope_arg(value: Option<&serde_json::Value>) -> Result<Option<BaseScope>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(scope) = value.as_str() else {
        return Err("scope must be 'project' or 'user'".to_string());
    };
    match scope.to_ascii_lowercase().as_str() {
        "project" => Ok(Some(BaseScope::Project)),
        "user" => Ok(Some(BaseScope::User)),
        _ => Err(format!(
            "Invalid scope '{}'. Expected 'project' or 'user'",
            scope
        )),
    }
}

#[cfg(feature = "mcp")]
fn mcp_connect_base_response(
    state: &mut McpServerState,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let scope = match mcp_parse_scope_arg(args.get("scope")) {
        Ok(scope) => scope,
        Err(err) => return mcp_error(id, -32602, err),
    };
    let base_ref = args.get("base_ref").and_then(|value| value.as_str());
    let name = args.get("name").and_then(|value| value.as_str());
    let path = args.get("path").and_then(|value| value.as_str());

    match state.connect_base(base_ref, scope, name, path) {
        Ok(base) => mcp_text_result(id, serde_json::to_string_pretty(&base).unwrap_or_default()),
        Err(err) => mcp_error(id, -32602, err),
    }
}

#[cfg(feature = "mcp")]
fn mcp_switch_base_response(
    state: &mut McpServerState,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(base_ref) = args.get("base_ref").and_then(|value| value.as_str()) else {
        return mcp_error(id, -32602, "Missing required string field 'base_ref'");
    };

    match state.switch_base(base_ref) {
        Ok(base) => mcp_text_result(id, serde_json::to_string_pretty(&base).unwrap_or_default()),
        Err(err) => mcp_error(id, -32602, err),
    }
}

#[cfg(feature = "mcp")]
fn mcp_parse_filters(args: &serde_json::Map<String, serde_json::Value>) -> Option<QueryFilters> {
    let min_trust = args
        .get("min_trust")
        .and_then(|value| value.as_u64())
        .map(|value| u16::try_from(value).unwrap_or(u16::MAX))
        .unwrap_or(0);
    let domain_mask = args
        .get("domain_mask")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);

    if min_trust == 0 && domain_mask == 0 {
        None
    } else {
        Some(QueryFilters::new(min_trust, domain_mask))
    }
}

#[cfg(feature = "mcp")]
fn mcp_parse_edge_types(values: Option<&serde_json::Value>) -> Result<Vec<EdgeType>, String> {
    let Some(values) = values else {
        return Ok(Vec::new());
    };
    let Some(items) = values.as_array() else {
        return Err("edge_types must be an array of strings".to_string());
    };

    let mut edge_types = Vec::with_capacity(items.len());
    for item in items {
        let Some(name) = item.as_str() else {
            return Err("edge_types entries must be strings".to_string());
        };
        let edge_type = match name.to_ascii_uppercase().as_str() {
            "DEFINES" => EdgeType::DEFINES,
            "REFINES" => EdgeType::REFINES,
            "GENERALIZES" => EdgeType::GENERALIZES,
            "IMPLIES" => EdgeType::IMPLIES,
            "SUPPORTS" => EdgeType::SUPPORTS,
            "CONTRADICTS" => EdgeType::CONTRADICTS,
            "SAME_AS" => EdgeType::SAME_AS,
            "NEAR_DUP" => EdgeType::NEAR_DUP,
            "DERIVED_FROM" => EdgeType::DERIVED_FROM,
            "DEPENDS_ON" => EdgeType::DEPENDS_ON,
            "CAUSES" => EdgeType::CAUSES,
            "ENABLES" => EdgeType::ENABLES,
            "PREVENTS" => EdgeType::PREVENTS,
            "STEP_OF" => EdgeType::STEP_OF,
            "INPUT_OF" => EdgeType::INPUT_OF,
            "OUTPUT_OF" => EdgeType::OUTPUT_OF,
            "MAPS_TO" => EdgeType::MAPS_TO,
            "IMPORTED_FROM" => EdgeType::IMPORTED_FROM,
            "GATEWAY_TO" => EdgeType::GATEWAY_TO,
            "SUPERSEDES" => EdgeType::SUPERSEDES,
            "TOMBSTONE_LINK" => EdgeType::TOMBSTONE_LINK,
            other => return Err(format!("Unsupported edge type '{}'", other)),
        };
        edge_types.push(edge_type);
    }

    Ok(edge_types)
}

#[cfg(feature = "mcp")]
fn mcp_all_edge_types() -> Vec<EdgeType> {
    vec![
        EdgeType::DEFINES,
        EdgeType::REFINES,
        EdgeType::GENERALIZES,
        EdgeType::IMPLIES,
        EdgeType::SUPPORTS,
        EdgeType::CONTRADICTS,
        EdgeType::SAME_AS,
        EdgeType::NEAR_DUP,
        EdgeType::DERIVED_FROM,
        EdgeType::DEPENDS_ON,
        EdgeType::CAUSES,
        EdgeType::ENABLES,
        EdgeType::PREVENTS,
        EdgeType::STEP_OF,
        EdgeType::INPUT_OF,
        EdgeType::OUTPUT_OF,
        EdgeType::MAPS_TO,
        EdgeType::IMPORTED_FROM,
        EdgeType::GATEWAY_TO,
        EdgeType::SUPERSEDES,
        EdgeType::TOMBSTONE_LINK,
    ]
}

#[cfg(feature = "mcp")]
type McpGraphPattern = (Option<u64>, Option<EdgeType>, Option<u64>);

#[cfg(feature = "mcp")]
fn mcp_parse_graph_pattern(pattern: &str) -> Result<McpGraphPattern, String> {
    let parts: Vec<&str> = pattern.split("->").map(str::trim).collect();
    if parts.len() != 3 {
        return Err(
            "pattern must use 'src -> EDGE_TYPE -> dst' syntax, with '*' as wildcard".to_string(),
        );
    }

    let parse_node = |raw: &str| -> Result<Option<u64>, String> {
        if raw.is_empty() || raw == "*" {
            Ok(None)
        } else {
            raw.parse::<u64>()
                .map(Some)
                .map_err(|_| format!("Invalid graph node pattern: {}", raw))
        }
    };

    let src = parse_node(parts[0])?;
    let dst = parse_node(parts[2])?;
    let edge = if parts[1].is_empty() || parts[1] == "*" {
        None
    } else {
        Some(
            mcp_parse_edge_types(Some(&serde_json::json!([parts[1]])))?
                .into_iter()
                .next()
                .ok_or_else(|| format!("Invalid edge type in pattern: {}", parts[1]))?,
        )
    };

    Ok((src, edge, dst))
}

#[cfg(feature = "mcp")]
fn mcp_contract_from_args(
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<(QueryContract, String), String> {
    if let Some(contract_value) = args.get("contract") {
        let contract: QueryContract = serde_json::from_value(contract_value.clone())
            .map_err(|err| format!("Invalid contract: {}", err))?;
        contract
            .validate()
            .map_err(|err| format!("Invalid contract: {}", err))?;
        Ok((contract, "contract".to_string()))
    } else if let Some(query_text) = args
        .get("query_text")
        .or_else(|| args.get("question"))
        .and_then(|value| value.as_str())
    {
        Ok((
            QueryContractCompiler::compile_contract(query_text),
            query_text.to_string(),
        ))
    } else {
        Err("Missing required field 'query_text' or 'contract'".to_string())
    }
}

#[cfg(feature = "mcp")]
fn mcp_query_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let (contract, _label) = match mcp_contract_from_args(args) {
        Ok(contract) => contract,
        Err(err) => return mcp_error(id, -32602, err),
    };
    let ctx_id = args
        .get("ctx_id")
        .and_then(|value| value.as_u64())
        .unwrap_or(store.active_context().into()) as u32;

    match store.answer_contract(contract, ctx_id) {
        Ok(answer) => mcp_answer_result(id, &answer),
        Err(err) => mcp_error(id, -32000, format!("Query failed: {}", err)),
    }
}

#[cfg(feature = "mcp")]
fn mcp_compile_query_contract_response(
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(query_text) = args.get("query_text").and_then(|value| value.as_str()) else {
        return mcp_error(id, -32602, "Missing required string field 'query_text'");
    };
    let contract = QueryContractCompiler::compile_contract(query_text);
    mcp_text_result(
        id,
        serde_json::to_string_pretty(&contract).unwrap_or_default(),
    )
}

#[cfg(feature = "mcp")]
fn mcp_validate_query_contract_response(
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(contract_value) = args.get("contract") else {
        return mcp_error(id, -32602, "Missing required object field 'contract'");
    };
    let contract: QueryContract = match serde_json::from_value(contract_value.clone()) {
        Ok(contract) => contract,
        Err(err) => return mcp_error(id, -32602, format!("Invalid contract JSON: {}", err)),
    };
    match contract.validate() {
        Ok(()) => mcp_text_result(id, serde_json::json!({"valid": true}).to_string()),
        Err(err) => mcp_text_result(
            id,
            serde_json::json!({"valid": false, "error": err.to_string()}).to_string(),
        ),
    }
}

#[cfg(feature = "mcp")]
fn mcp_explain_answer_graph_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let (contract, label) = match mcp_contract_from_args(args) {
        Ok(contract) => contract,
        Err(err) => return mcp_error(id, -32602, err),
    };
    let ctx_id = args
        .get("ctx_id")
        .and_then(|value| value.as_u64())
        .unwrap_or(store.active_context().into()) as u32;

    match store.answer_contract(contract, ctx_id) {
        Ok(answer) => mcp_text_result(
            id,
            serde_json::json!({
                "query": label,
                "status": format!("{:?}", answer.status),
                "snapshot": answer.snapshot,
                "selected_ctx": answer.selected_ctx,
                "graph": answer_graph_json(&answer.graph),
                "coverage_report": answer.coverage_report,
                "query_trace": answer.query_trace,
                "rejected_candidates": answer.rejected_candidates,
            })
            .to_string(),
        ),
        Err(err) => mcp_error(id, -32000, format!("Explain failed: {}", err)),
    }
}

#[cfg(feature = "mcp")]
fn mcp_get_provenance_path_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(atom_id_str) = args.get("atom_id").and_then(|value| value.as_str()) else {
        return mcp_error(id, -32602, "Missing required string field 'atom_id'");
    };
    let Some(atom_id) = mcp_parse_atom_id(atom_id_str) else {
        return mcp_error(id, -32602, format!("Invalid atom ID: {}", atom_id_str));
    };

    match store.get_provenance(&atom_id) {
        Ok(chain) => mcp_text_result(id, serde_json::to_string_pretty(&chain).unwrap_or_default()),
        Err(err) => mcp_error(id, -32000, format!("Provenance lookup failed: {}", err)),
    }
}

#[cfg(feature = "mcp")]
fn mcp_search_lex_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(term) = args.get("term").and_then(|value| value.as_str()) else {
        return mcp_error(id, -32602, "Missing required string field 'term'");
    };
    let filters = mcp_parse_filters(args);
    let nodes = store.search_lex(term, filters);
    mcp_text_result(
        id,
        format!(
            "term={}\ncount={}\nnodes={}",
            term,
            nodes.len(),
            nodes
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ),
    )
}

#[cfg(feature = "mcp")]
fn mcp_search_semantic_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(vector_values) = args.get("vector").and_then(|value| value.as_array()) else {
        return mcp_error(id, -32602, "Missing required numeric array field 'vector'");
    };

    let mut vector = Vec::with_capacity(vector_values.len());
    for value in vector_values {
        let Some(component) = value.as_f64() else {
            return mcp_error(id, -32602, "vector entries must be numbers");
        };
        vector.push(component as f32);
    }

    let filters = mcp_parse_filters(args);
    let candidates = store.search_semantic(&vector, filters);
    let lines = candidates
        .iter()
        .map(|candidate| {
            format!(
                "node={} atom={} trust={} type={:?}",
                candidate.node_num,
                hex::encode(candidate.atom_id),
                candidate.trust,
                candidate.atom_type
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    mcp_text_result(id, format!("count={}\n{}", candidates.len(), lines))
}

#[cfg(feature = "mcp")]
fn mcp_create_context_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let policy_id = arguments
        .and_then(|value| value.as_object())
        .and_then(|args| args.get("policy_id"))
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    match store.create_context(policy_id as u32) {
        Ok(ctx_id) => mcp_text_result(
            id,
            format!("created_ctx={}\npolicy_id={}", ctx_id, policy_id),
        ),
        Err(error) => mcp_error(id, -32603, format!("Create context failed: {error}")),
    }
}

#[cfg(feature = "mcp")]
fn mcp_list_conflicts_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let ctx_id = arguments
        .and_then(|value| value.as_object())
        .and_then(|args| args.get("ctx_id"))
        .and_then(|value| value.as_u64())
        .unwrap_or(store.active_context().into()) as u32;
    let conflicts = store.list_conflicts(ctx_id);
    let lines = conflicts
        .iter()
        .map(|conflict| {
            format!(
                "c_id={} type={:?} severity={:?} atom_a={} atom_b={}",
                conflict.c_id,
                conflict.conflict_type,
                conflict.severity,
                hex::encode(conflict.atom_a),
                hex::encode(conflict.atom_b)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    mcp_text_result(
        id,
        format!("ctx_id={}\ncount={}\n{}", ctx_id, conflicts.len(), lines),
    )
}

#[cfg(feature = "mcp")]
fn mcp_graph_walk_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(seed_nodes_json) = args.get("seed_nodes").and_then(|value| value.as_array()) else {
        return mcp_error(
            id,
            -32602,
            "Missing required integer array field 'seed_nodes'",
        );
    };

    let mut seed_nodes = Vec::with_capacity(seed_nodes_json.len());
    for value in seed_nodes_json {
        let Some(node) = value.as_u64() else {
            return mcp_error(id, -32602, "seed_nodes entries must be integers");
        };
        seed_nodes.push(node);
    }

    let edge_types = match mcp_parse_edge_types(args.get("edge_types")) {
        Ok(edge_types) => edge_types,
        Err(err) => return mcp_error(id, -32602, err),
    };
    let depth = args
        .get("depth")
        .and_then(|value| value.as_u64())
        .map(|value| u8::try_from(value).unwrap_or(u8::MAX))
        .unwrap_or(2);

    let edges = store.graph_walk(&seed_nodes, &edge_types, depth, None);
    let lines = edges
        .iter()
        .map(|(src, dst, edge_type)| format!("{} -> {} ({:?})", src, dst, edge_type))
        .collect::<Vec<_>>()
        .join("\n");
    mcp_text_result(
        id,
        format!(
            "seed_nodes={}\ndepth={}\nedge_count={}\n{}",
            seed_nodes
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(","),
            depth,
            edges.len(),
            lines
        ),
    )
}

#[cfg(feature = "mcp")]
fn mcp_search_graph_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(pattern) = args.get("pattern").and_then(|value| value.as_str()) else {
        return mcp_error(id, -32602, "Missing required string field 'pattern'");
    };
    let limit = args
        .get("limit")
        .and_then(|value| value.as_u64())
        .map(|value| u32::try_from(value).unwrap_or(u32::MAX))
        .unwrap_or(50);

    let (src_pattern, edge_pattern, dst_pattern) = match mcp_parse_graph_pattern(pattern) {
        Ok(parts) => parts,
        Err(err) => return mcp_error(id, -32602, err),
    };

    let edge_types = match edge_pattern {
        Some(edge_type) => vec![edge_type],
        None => mcp_all_edge_types(),
    };
    let candidate_sources: Vec<u64> = if let Some(src_node) = src_pattern {
        vec![src_node]
    } else {
        store
            .list_atom_ids()
            .into_iter()
            .filter_map(|atom_id| store.get_node_num(&atom_id))
            .collect()
    };

    let mut matches = Vec::new();
    for src_node in candidate_sources {
        let edges = store.graph_walk(&[src_node], &edge_types, 1, None);
        for (src, dst, edge_type) in edges {
            if src != src_node {
                continue;
            }
            if let Some(expected_dst) = dst_pattern
                && dst != expected_dst
            {
                continue;
            }

            matches.push((src, dst, edge_type));
            if matches.len() >= limit as usize {
                break;
            }
        }
        if matches.len() >= limit as usize {
            break;
        }
    }

    let mut lines = vec![
        format!("pattern={}", pattern),
        format!("limit={}", limit),
        format!("match_count={}", matches.len()),
    ];
    for (idx, (src, dst, edge_type)) in matches.iter().enumerate() {
        lines.push(format!("[{}] {} --{:?}--> {}", idx, src, edge_type, dst));
    }

    mcp_text_result(id, lines.join("\n"))
}

#[cfg(feature = "mcp")]
fn mcp_ingest_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(atom_type_str) = args.get("atom_type").and_then(|value| value.as_str()) else {
        return mcp_error(id, -32602, "Missing required string field 'atom_type'");
    };
    let atom_type = match mcp_parse_atom_type(atom_type_str) {
        Some(t) => t,
        None => return mcp_error(id, -32602, format!("Invalid atom type: {}", atom_type_str)),
    };
    let Some(claims_json) = args.get("claims").and_then(|value| value.as_array()) else {
        return mcp_error(id, -32602, "Missing required array field 'claims'");
    };
    let claims: Vec<ClaimData> = match claims_json
        .iter()
        .map(mcp_parse_claim_from_json)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(c) => c,
        Err(e) => return mcp_error(id, -32602, format!("Invalid claim: {}", e)),
    };
    let symbols: Vec<String> = args
        .get("symbols")
        .and_then(|value| value.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let trust_level = args
        .get("trust_level")
        .and_then(|value| value.as_u64())
        .map(|value| u16::try_from(value).unwrap_or(5000))
        .unwrap_or(5000);
    let domain_mask = args
        .get("domain_mask")
        .and_then(|value| value.as_u64())
        .unwrap_or(0xFFFF);

    let payload =
        match mcp_build_atom_payload(atom_type, &symbols, &claims, trust_level, domain_mask) {
            Ok(p) => p,
            Err(e) => return mcp_error(id, -32603, format!("Failed to build payload: {}", e)),
        };

    match store.ingest(&payload, atom_type, &claims, &[]) {
        Ok(atom_id) => mcp_text_result(
            id,
            format!(
                "Successfully ingested atom\nAtom ID: {}\nType: {:?}\nClaims: {}",
                hex::encode(atom_id),
                atom_type,
                claims.len()
            ),
        ),
        Err(e) => mcp_error(id, -32603, format!("Ingest failed: {}", e)),
    }
}

#[cfg(feature = "mcp")]
fn mcp_batch_ingest_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(atoms_json) = args.get("atoms").and_then(|value| value.as_array()) else {
        return mcp_error(id, -32602, "Missing required array field 'atoms'");
    };
    let batch_atoms: Vec<BatchAtom> = match atoms_json
        .iter()
        .map(mcp_parse_batch_atom_from_json)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(a) => a,
        Err(e) => return mcp_error(id, -32602, format!("Invalid batch atom: {}", e)),
    };

    match store.batch_ingest(batch_atoms) {
        Ok(result) => {
            let mut output = format!(
                "Batch ingest complete:\nTotal: {}\nSuccess: {}\nErrors: {}",
                result.total,
                result.success_count(),
                result.error_count()
            );
            if !result.atom_ids.is_empty() {
                output.push_str("\n\nAtom IDs:");
                for (i, atom_id) in result.atom_ids.iter().take(5).enumerate() {
                    output.push_str(&format!("\n [{}] {}", i, hex::encode(atom_id)));
                }
                if result.atom_ids.len() > 5 {
                    output.push_str(&format!("\n ... and {} more", result.atom_ids.len() - 5));
                }
            }
            mcp_text_result(id, output)
        }
        Err(e) => mcp_error(id, -32603, format!("Batch ingest failed: {}", e)),
    }
}

#[cfg(feature = "mcp")]
fn mcp_update_atom_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(atom_id_str) = args.get("atom_id").and_then(|value| value.as_str()) else {
        return mcp_error(id, -32602, "Missing required string field 'atom_id'");
    };
    let old_atom_id = match mcp_parse_atom_id(atom_id_str) {
        Some(id) => id,
        None => {
            return mcp_error(
                id,
                -32602,
                format!("Invalid atom ID format: {}", atom_id_str),
            );
        }
    };
    let Some(atom_type_str) = args.get("atom_type").and_then(|value| value.as_str()) else {
        return mcp_error(id, -32602, "Missing required string field 'atom_type'");
    };
    let atom_type = match mcp_parse_atom_type(atom_type_str) {
        Some(t) => t,
        None => return mcp_error(id, -32602, format!("Invalid atom type: {}", atom_type_str)),
    };
    let Some(claims_json) = args.get("claims").and_then(|value| value.as_array()) else {
        return mcp_error(id, -32602, "Missing required array field 'claims'");
    };
    let claims: Vec<ClaimData> = match claims_json
        .iter()
        .map(mcp_parse_claim_from_json)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(c) => c,
        Err(e) => return mcp_error(id, -32602, format!("Invalid claim: {}", e)),
    };
    let symbols: Vec<String> = args
        .get("symbols")
        .and_then(|value| value.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let new_payload = match mcp_build_atom_payload(atom_type, &symbols, &claims, 5000, 0xFFFF) {
        Ok(p) => p,
        Err(e) => return mcp_error(id, -32603, format!("Failed to build payload: {}", e)),
    };

    match store.update_atom(old_atom_id, new_payload, atom_type, claims, vec![]) {
        Ok(result) => mcp_text_result(
            id,
            format!(
                "Successfully updated atom:\nNew Atom ID: {}\nSupersedes: {}\nNote: Old atom preserved for provenance",
                hex::encode(result.new_atom_id),
                hex::encode(result.supersedes)
            ),
        ),
        Err(e) => mcp_error(id, -32603, format!("Update failed: {}", e)),
    }
}

#[cfg(feature = "mcp")]
fn mcp_delete_atom_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(atom_id_str) = args.get("atom_id").and_then(|value| value.as_str()) else {
        return mcp_error(id, -32602, "Missing required string field 'atom_id'");
    };
    let atom_id = match mcp_parse_atom_id(atom_id_str) {
        Some(id) => id,
        None => {
            return mcp_error(
                id,
                -32602,
                format!("Invalid atom ID format: {}", atom_id_str),
            );
        }
    };
    let reason_str = args
        .get("reason")
        .and_then(|value| value.as_str())
        .unwrap_or("Obsolete");
    let delete_reason = mcp_parse_delete_reason(reason_str);

    match store.delete_atom(atom_id, delete_reason) {
        Ok(result) => mcp_text_result(
            id,
            format!(
                "Successfully deleted atom:\nOriginal Atom ID: {}\nTombstone ID: {}\nReason: {:?}\nNote: Atom content preserved for audit trail",
                atom_id_str,
                hex::encode(result.tombstone_id),
                delete_reason
            ),
        ),
        Err(e) => mcp_error(id, -32603, format!("Delete failed: {}", e)),
    }
}

#[cfg(feature = "mcp")]
fn mcp_history_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let limit = arguments
        .and_then(|args| args.get("limit"))
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(20);

    match store.history(limit) {
        Ok(entries) => {
            let mut output = format!("Operation history\nEntries: {}", entries.len());
            for (idx, entry) in entries.iter().enumerate() {
                output.push_str(&format!(
                    "\n\n[{}] {:?} @ {}",
                    idx, entry.operation, entry.timestamp_unix_ns
                ));
                if !entry.atom_ids.is_empty() {
                    output.push_str(&format!("\nAtom IDs: {}", entry.atom_ids.join(", ")));
                }
                for (key, value) in &entry.details {
                    output.push_str(&format!("\n{}: {}", key, value));
                }
            }
            mcp_text_result(id, output)
        }
        Err(e) => mcp_error(id, -32603, format!("History read failed: {}", e)),
    }
}

#[cfg(feature = "mcp")]
fn mcp_register_source_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(kind_str) = args.get("kind").and_then(|value| value.as_str()) else {
        return mcp_error(id, -32602, "Missing required string field 'kind'");
    };
    let Some(label) = args.get("label").and_then(|value| value.as_str()) else {
        return mcp_error(id, -32602, "Missing required string field 'label'");
    };
    let Some(kind) = mcp_parse_source_kind(kind_str) else {
        return mcp_error(id, -32602, format!("Invalid source kind: {}", kind_str));
    };

    let line_range = match (
        args.get("line_start").and_then(|value| value.as_u64()),
        args.get("line_end").and_then(|value| value.as_u64()),
    ) {
        (Some(start), Some(end)) => Some((start, end)),
        _ => None,
    };
    let location = SourceLocation {
        path: args
            .get("path")
            .and_then(|value| value.as_str())
            .map(ToString::to_string),
        url: args
            .get("url")
            .and_then(|value| value.as_str())
            .map(ToString::to_string),
        commit_hash: args
            .get("commit_hash")
            .and_then(|value| value.as_str())
            .map(ToString::to_string),
        byte_range: None,
        line_range,
        timestamp_unix_ns: None,
        source_version: args
            .get("source_version")
            .and_then(|value| value.as_str())
            .map(ToString::to_string),
    };

    match store.register_source(kind, label, location) {
        Ok(source) => mcp_text_result(
            id,
            format!(
                "Registered source\nSource ID: {}\nKind: {:?}\nLabel: {}",
                source.source_id, source.kind, source.label
            ),
        ),
        Err(e) => mcp_error(id, -32603, format!("Source registration failed: {}", e)),
    }
}

#[cfg(feature = "mcp")]
fn mcp_list_sources_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    _arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    match store.list_sources() {
        Ok(sources) => {
            let mut output = format!("Sources\nTotal: {}", sources.len());
            for source in &sources {
                output.push_str(&format!(
                    "\n\nSource ID: {}\nKind: {:?}\nLabel: {}",
                    source.source_id, source.kind, source.label
                ));
                if let Some(path) = &source.location.path {
                    output.push_str(&format!("\nPath: {}", path));
                }
                if let Some(url) = &source.location.url {
                    output.push_str(&format!("\nURL: {}", url));
                }
                if let Some((start, end)) = source.location.line_range {
                    output.push_str(&format!("\nLines: {}-{}", start, end));
                }
                if let Some(version) = &source.location.source_version {
                    output.push_str(&format!("\nVersion: {}", version));
                }
            }
            mcp_text_result(id, output)
        }
        Err(e) => mcp_error(id, -32603, format!("Source list failed: {}", e)),
    }
}

#[cfg(feature = "mcp")]
fn mcp_attach_atom_source_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(atom_id_str) = args.get("atom_id").and_then(|value| value.as_str()) else {
        return mcp_error(id, -32602, "Missing required string field 'atom_id'");
    };
    let atom_id = match mcp_parse_atom_id(atom_id_str) {
        Some(id) => id,
        None => return mcp_error(id, -32602, format!("Invalid atom ID: {}", atom_id_str)),
    };
    let Some(source_id) = args
        .get("source_id")
        .and_then(|value| value.as_u64())
        .and_then(|value| SourceId::try_from(value).ok())
    else {
        return mcp_error(id, -32602, "Missing or invalid integer field 'source_id'");
    };

    match store.set_atom_source(atom_id, source_id) {
        Ok(()) => mcp_text_result(
            id,
            format!(
                "Attached source\nAtom ID: {}\nSource ID: {}",
                atom_id_str, source_id
            ),
        ),
        Err(e) => mcp_error(id, -32603, format!("Attach source failed: {}", e)),
    }
}

#[cfg(feature = "mcp")]
fn mcp_parse_predicate_direction(value: Option<&str>) -> Option<PredicateDirection> {
    match value.unwrap_or("directed").to_ascii_lowercase().as_str() {
        "directed" => Some(PredicateDirection::Directed),
        "symmetric" => Some(PredicateDirection::Symmetric),
        _ => None,
    }
}

#[cfg(feature = "mcp")]
fn mcp_parse_predicate_cardinality(value: Option<&str>) -> Option<PredicateCardinality> {
    match value
        .unwrap_or("many_to_many")
        .to_ascii_lowercase()
        .as_str()
    {
        "one_to_one" => Some(PredicateCardinality::OneToOne),
        "one_to_many" => Some(PredicateCardinality::OneToMany),
        "many_to_one" => Some(PredicateCardinality::ManyToOne),
        "many_to_many" => Some(PredicateCardinality::ManyToMany),
        _ => None,
    }
}

#[cfg(feature = "mcp")]
fn mcp_predicate_text(predicate: &PredicateRecord) -> String {
    serde_json::to_string_pretty(predicate)
        .unwrap_or_else(|error| format!("predicate serialization failed: {error}"))
}

#[cfg(feature = "mcp")]
fn mcp_register_predicate_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(error) => return error,
    };
    let Some(stable_key) = args.get("stable_key").and_then(serde_json::Value::as_str) else {
        return mcp_error(id, -32602, "Missing required string field 'stable_key'");
    };
    let Some(canonical_name) = args
        .get("canonical_name")
        .and_then(serde_json::Value::as_str)
    else {
        return mcp_error(id, -32602, "Missing required string field 'canonical_name'");
    };
    let Some(description) = args.get("description").and_then(serde_json::Value::as_str) else {
        return mcp_error(id, -32602, "Missing required string field 'description'");
    };
    let Some(direction) =
        mcp_parse_predicate_direction(args.get("direction").and_then(serde_json::Value::as_str))
    else {
        return mcp_error(id, -32602, "Invalid predicate direction");
    };
    let Some(cardinality) = mcp_parse_predicate_cardinality(
        args.get("cardinality").and_then(serde_json::Value::as_str),
    ) else {
        return mcp_error(id, -32602, "Invalid predicate cardinality");
    };
    let contract = PredicateContract {
        stable_key: stable_key.to_owned(),
        canonical_name: canonical_name.to_owned(),
        description: description.to_owned(),
        direction,
        inverse_stable_key: args
            .get("inverse_stable_key")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        cardinality,
    };
    match store.register_predicate(contract) {
        Ok(predicate) => mcp_text_result(id, mcp_predicate_text(&predicate)),
        Err(error) => mcp_error(
            id,
            -32602,
            format!("Predicate registration rejected: {error}"),
        ),
    }
}

#[cfg(feature = "mcp")]
fn mcp_list_predicates_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    _arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    match store.list_predicates() {
        Ok(predicates) => mcp_text_result(
            id,
            serde_json::to_string_pretty(&predicates)
                .unwrap_or_else(|error| format!("predicate serialization failed: {error}")),
        ),
        Err(error) => mcp_error(id, -32603, format!("Predicate list failed: {error}")),
    }
}

#[cfg(feature = "mcp")]
fn mcp_get_predicate_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(error) => return error,
    };
    let Some(predicate_id) = args
        .get("predicate_id")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| SymId::try_from(value).ok())
    else {
        return mcp_error(
            id,
            -32602,
            "Missing or invalid integer field 'predicate_id'",
        );
    };
    match store.get_predicate(predicate_id) {
        Ok(Some(predicate)) => mcp_text_result(id, mcp_predicate_text(&predicate)),
        Ok(None) => mcp_error(id, -32602, format!("Predicate {predicate_id} not found")),
        Err(error) => mcp_error(id, -32603, format!("Predicate lookup failed: {error}")),
    }
}

#[cfg(feature = "mcp")]
fn mcp_resolve_predicate_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(error) => return error,
    };
    let Some(name_or_key) = args.get("name_or_key").and_then(serde_json::Value::as_str) else {
        return mcp_error(id, -32602, "Missing required string field 'name_or_key'");
    };
    match store.resolve_predicate(name_or_key) {
        Ok(Some(predicate)) => mcp_text_result(id, mcp_predicate_text(&predicate)),
        Ok(None) => mcp_error(id, -32602, format!("Predicate '{name_or_key}' not found")),
        Err(error) => mcp_error(id, -32603, format!("Predicate resolve failed: {error}")),
    }
}

#[cfg(feature = "mcp")]
fn mcp_create_entity_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(canonical_name) = args.get("canonical_name").and_then(|value| value.as_str()) else {
        return mcp_error(id, -32602, "Missing required string field 'canonical_name'");
    };
    let Some(entity_type) = args.get("entity_type").and_then(|value| value.as_str()) else {
        return mcp_error(id, -32602, "Missing required string field 'entity_type'");
    };

    match store.create_entity(canonical_name, entity_type) {
        Ok(entity) => mcp_text_result(
            id,
            format!(
                "Created entity\nEntity ID: {}\nName: {}\nType: {}",
                entity.entity_id, entity.canonical_name, entity.entity_type
            ),
        ),
        Err(e) => mcp_error(id, -32603, format!("Create entity failed: {}", e)),
    }
}

#[cfg(feature = "mcp")]
fn mcp_list_entities_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    _arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    match store.list_entities() {
        Ok(entities) => {
            let mut output = format!("Entities\nTotal: {}", entities.len());
            for entity in &entities {
                output.push_str(&format!(
                    "\n\nEntity ID: {}\nName: {}\nType: {}",
                    entity.entity_id, entity.canonical_name, entity.entity_type
                ));
                if !entity.aliases.is_empty() {
                    output.push_str(&format!("\nAliases: {}", entity.aliases.join(", ")));
                }
                if let Some(split_from) = entity.split_from {
                    output.push_str(&format!("\nSplit from: {}", split_from));
                }
                if !entity.merged_from.is_empty() {
                    output.push_str(&format!("\nMerged from: {:?}", entity.merged_from));
                }
            }
            mcp_text_result(id, output)
        }
        Err(e) => mcp_error(id, -32603, format!("List entities failed: {}", e)),
    }
}

#[cfg(feature = "mcp")]
fn mcp_alias_entity_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(entity_id) = args.get("entity_id").and_then(|value| value.as_u64()) else {
        return mcp_error(id, -32602, "Missing required integer field 'entity_id'");
    };
    let Some(alias) = args.get("alias").and_then(|value| value.as_str()) else {
        return mcp_error(id, -32602, "Missing required string field 'alias'");
    };

    match store.alias_entity(entity_id, alias) {
        Ok(entity) => mcp_text_result(
            id,
            format!(
                "Aliased entity\nEntity ID: {}\nAliases: {}",
                entity.entity_id,
                entity.aliases.join(", ")
            ),
        ),
        Err(e) => mcp_error(id, -32603, format!("Alias entity failed: {}", e)),
    }
}

#[cfg(feature = "mcp")]
fn mcp_merge_entities_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(target_entity) = args.get("target_entity").and_then(|value| value.as_u64()) else {
        return mcp_error(id, -32602, "Missing required integer field 'target_entity'");
    };
    let Some(source_entity) = args.get("source_entity").and_then(|value| value.as_u64()) else {
        return mcp_error(id, -32602, "Missing required integer field 'source_entity'");
    };

    match store.merge_entities(target_entity, source_entity) {
        Ok(entity) => mcp_text_result(
            id,
            format!(
                "Merged entities\nTarget Entity ID: {}\nMerged from: {:?}\nAliases: {}",
                entity.entity_id,
                entity.merged_from,
                entity.aliases.join(", ")
            ),
        ),
        Err(e) => mcp_error(id, -32603, format!("Merge entities failed: {}", e)),
    }
}

#[cfg(feature = "mcp")]
fn mcp_split_entity_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(source_entity) = args.get("source_entity").and_then(|value| value.as_u64()) else {
        return mcp_error(id, -32602, "Missing required integer field 'source_entity'");
    };
    let Some(canonical_name) = args.get("canonical_name").and_then(|value| value.as_str()) else {
        return mcp_error(id, -32602, "Missing required string field 'canonical_name'");
    };
    let Some(entity_type) = args.get("entity_type").and_then(|value| value.as_str()) else {
        return mcp_error(id, -32602, "Missing required string field 'entity_type'");
    };

    match store.split_entity(source_entity, canonical_name, entity_type) {
        Ok(entity) => mcp_text_result(
            id,
            format!(
                "Split entity\nNew Entity ID: {}\nName: {}\nSplit from: {:?}",
                entity.entity_id, entity.canonical_name, entity.split_from
            ),
        ),
        Err(e) => mcp_error(id, -32603, format!("Split entity failed: {}", e)),
    }
}

#[cfg(feature = "mcp")]
fn mcp_add_claim_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(entity_id) = args.get("entity_id").and_then(|value| value.as_u64()) else {
        return mcp_error(id, -32602, "Missing required integer field 'entity_id'");
    };
    let Some(predicate) = args
        .get("predicate")
        .and_then(|value| value.as_u64())
        .and_then(|value| SymId::try_from(value).ok())
    else {
        return mcp_error(id, -32602, "Missing or invalid integer field 'predicate'");
    };
    let Some(object) = args.get("object").and_then(|value| value.as_u64()) else {
        return mcp_error(id, -32602, "Missing required integer field 'object'");
    };
    let object_tag = match args.get("object_tag").and_then(|value| value.as_str()) {
        Some(raw) => match parse_object_tag(Some(raw)).and_then(|tag| {
            ObjTag::from_u8(tag)
                .ok_or_else(|| CliError::Validation(format!("Unsupported object tag: {}", raw)))
        }) {
            Ok(tag) => tag,
            Err(err) => return mcp_error(id, -32602, err.to_string()),
        },
        None => ObjTag::U64,
    };
    let ctx_id = args
        .get("ctx_id")
        .and_then(|value| value.as_u64())
        .and_then(|value| CtxId::try_from(value).ok())
        .unwrap_or_else(|| store.active_context());

    match store.add_entity_claim(entity_id, predicate, object_tag, object, ctx_id, Vec::new()) {
        Ok(result) => mcp_text_result(
            id,
            format!(
                "Added entity claim\nEntity ID: {}\nAtom ID: {}\nContext: {}",
                entity_id,
                hex::encode(result.atom_id),
                result.ctx_id
            ),
        ),
        Err(e) => mcp_error(id, -32603, format!("Add claim failed: {}", e)),
    }
}

#[cfg(feature = "mcp")]
fn mcp_assert_relation_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(subject) = args.get("subject").and_then(|value| value.as_u64()) else {
        return mcp_error(id, -32602, "Missing required integer field 'subject'");
    };
    let Some(predicate) = args
        .get("predicate")
        .and_then(|value| value.as_u64())
        .and_then(|value| SymId::try_from(value).ok())
    else {
        return mcp_error(id, -32602, "Missing or invalid integer field 'predicate'");
    };
    let Some(object) = args.get("object").and_then(|value| value.as_u64()) else {
        return mcp_error(id, -32602, "Missing required integer field 'object'");
    };
    let ctx_id = args
        .get("ctx_id")
        .and_then(|value| value.as_u64())
        .and_then(|value| CtxId::try_from(value).ok())
        .unwrap_or_else(|| store.active_context());

    match store.assert_relation(subject, predicate, object, ctx_id, Vec::new()) {
        Ok(result) => mcp_text_result(
            id,
            format!(
                "Asserted relation\nRelation ID: {}\nAtom ID: {}\nContext: {}",
                result.relation_id.unwrap_or(0),
                hex::encode(result.atom_id),
                result.ctx_id
            ),
        ),
        Err(e) => mcp_error(id, -32603, format!("Assert relation failed: {}", e)),
    }
}

#[cfg(feature = "mcp")]
fn mcp_correct_relation_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(relation_id) = args.get("relation_id").and_then(|value| value.as_u64()) else {
        return mcp_error(id, -32602, "Missing required integer field 'relation_id'");
    };
    let Some(subject) = args.get("subject").and_then(|value| value.as_u64()) else {
        return mcp_error(id, -32602, "Missing required integer field 'subject'");
    };
    let Some(predicate) = args
        .get("predicate")
        .and_then(|value| value.as_u64())
        .and_then(|value| SymId::try_from(value).ok())
    else {
        return mcp_error(id, -32602, "Missing or invalid integer field 'predicate'");
    };
    let Some(object) = args.get("object").and_then(|value| value.as_u64()) else {
        return mcp_error(id, -32602, "Missing required integer field 'object'");
    };
    let ctx_id = args
        .get("ctx_id")
        .and_then(|value| value.as_u64())
        .and_then(|value| CtxId::try_from(value).ok())
        .unwrap_or_else(|| store.active_context());

    match store.correct_relation(relation_id, subject, predicate, object, ctx_id, Vec::new()) {
        Ok(result) => mcp_text_result(
            id,
            format!(
                "Corrected relation\nNew Relation ID: {}\nNew Atom ID: {}\nContext: {}",
                result.relation_id.unwrap_or(0),
                hex::encode(result.atom_id),
                result.ctx_id
            ),
        ),
        Err(e) => mcp_error(id, -32603, format!("Correct relation failed: {}", e)),
    }
}

#[cfg(feature = "mcp")]
fn mcp_list_contexts_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    _arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let active_ctx = store.active_context();
    let contexts = store.list_contexts();

    let mut output = String::new();
    output.push_str("Contexts:\n");
    output.push_str(&format!("  Active: {}\n", active_ctx));
    output.push_str(&format!("  Total: {}\n\n", contexts.len()));

    for (idx, ctx) in contexts.iter().enumerate() {
        let status = if ctx.active { "active" } else { "frozen" };

        let parent_str = match ctx.parent_ctx {
            Some(p) => format!("{}", p),
            None => "none".to_string(),
        };

        output.push_str(&format!(
            "[{}] ID: {}, Status: {}, Parent: {}",
            idx, ctx.ctx_id, status, parent_str
        ));

        // Add branch reason if not Manual
        let reason_str = match ctx.branch_reason {
            BranchReason::Manual => None,
            BranchReason::Conflict => Some("conflict"),
            BranchReason::Hypothesis => Some("hypothesis"),
            BranchReason::Alternative => Some("alternative"),
        };
        if let Some(reason) = reason_str {
            output.push_str(&format!(", Branch reason: {}", reason));
        }

        // Add policy ID if not default
        if ctx.policy_id != 0 {
            output.push_str(&format!(", Policy: {}", ctx.policy_id));
        }

        // Add claims count
        output.push_str(&format!(", Claims: {}", ctx.active_claims.len()));

        // Add conflicts count if any
        if !ctx.conflicts.is_empty() {
            output.push_str(&format!(", Conflicts: {}", ctx.conflicts.len()));
        }

        output.push('\n');
    }

    mcp_text_result(id, output)
}
#[cfg(feature = "mcp")]
fn mcp_branch_context_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(parent_ctx) = args.get("parent_ctx").and_then(|value| value.as_u64()) else {
        return mcp_error(id, -32602, "Missing required integer field 'parent_ctx'");
    };
    let reason_str = args
        .get("reason")
        .and_then(|value| value.as_str())
        .unwrap_or("Manual");
    let branch_reason = mcp_parse_branch_reason(reason_str);
    let policy_id = args
        .get("policy_id")
        .and_then(|value| value.as_u64())
        .map(|value| u32::try_from(value).unwrap_or(0))
        .unwrap_or(0);

    match store.branch_ctx(parent_ctx as u32, branch_reason, policy_id) {
        Ok(Some(new_ctx)) => mcp_text_result(
            id,
            format!(
                "Created branch context: {}\nParent context: {}\nReason: {:?}\nPolicy ID: {}",
                new_ctx, parent_ctx, branch_reason, policy_id
            ),
        ),
        Ok(None) => mcp_error(id, -32603, "Failed to create branch context"),
        Err(error) => mcp_error(id, -32603, format!("Branch context failed: {error}")),
    }
}

#[cfg(feature = "mcp")]
fn mcp_graph_neighbors_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(node_num) = args.get("node_num").and_then(|value| value.as_u64()) else {
        return mcp_error(id, -32602, "Missing required integer field 'node_num'");
    };
    let edge_types = match mcp_parse_edge_types(args.get("edge_types")) {
        Ok(edge_types) => edge_types,
        Err(err) => return mcp_error(id, -32602, err),
    };

    // Determine which edge types to query
    let types_to_query: Vec<EdgeType> = if edge_types.is_empty() {
        mcp_all_edge_types()
    } else {
        edge_types
    };

    // Collect outgoing neighbors using graph_walk from the requested node.
    let seed_nodes = vec![node_num];
    let edges = store.graph_walk(&seed_nodes, &types_to_query, 1, None);

    let mut outgoing_neighbors: Vec<(NodeNum, EdgeType)> = Vec::new();
    let mut incoming_neighbors: Vec<(NodeNum, EdgeType)> = Vec::new();

    for (src, dst, edge_type) in edges {
        if src == node_num {
            outgoing_neighbors.push((dst, edge_type));
        }
    }

    // Recover incoming edges by scanning live atom nodes and checking their depth-1 edges.
    // This keeps the implementation store-backed without reaching into private GraphStore internals.
    for atom_id in store.list_atom_ids() {
        let Some(candidate_src) = store.get_node_num(&atom_id) else {
            continue;
        };
        let candidate_edges = store.graph_walk(&[candidate_src], &types_to_query, 1, None);
        for (src, dst, edge_type) in candidate_edges {
            if src == candidate_src && dst == node_num {
                incoming_neighbors.push((src, edge_type));
            }
        }
    }

    // Build response
    let total_neighbors = outgoing_neighbors.len() + incoming_neighbors.len();
    if total_neighbors == 0 {
        return mcp_text_result(
            id,
            format!(
                "Graph neighbors for node {}:\n\nNo neighbors found.",
                node_num
            ),
        );
    }

    // Sort by edge type and node number for consistent output
    outgoing_neighbors.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    incoming_neighbors.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));

    let mut lines = vec![format!("Graph neighbors for node {}:", node_num)];
    lines.push(format!("Found {} neighbors:", total_neighbors));

    let mut idx = 0;
    for (neighbor, edge_type) in &outgoing_neighbors {
        lines.push(format!(
            "[{}] Node: {}, Edge: {:?} (outgoing)",
            idx, neighbor, edge_type
        ));
        idx += 1;
    }
    for (neighbor, edge_type) in &incoming_neighbors {
        lines.push(format!(
            "[{}] Node: {}, Edge: {:?} (incoming)",
            idx, neighbor, edge_type
        ));
        idx += 1;
    }

    mcp_text_result(id, lines.join("\n"))
}
#[cfg(feature = "mcp")]
fn mcp_extract_subgraph_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(center_node) = args.get("center_node").and_then(|value| value.as_u64()) else {
        return mcp_error(id, -32602, "Missing required integer field 'center_node'");
    };
    let radius = args
        .get("radius")
        .and_then(|value| value.as_u64())
        .map(|value| u8::try_from(value).unwrap_or(2))
        .unwrap_or(2);
    let edge_types = match mcp_parse_edge_types(args.get("edge_types")) {
        Ok(edge_types) => edge_types,
        Err(err) => return mcp_error(id, -32602, err),
    };

    // Use graph_walk to extract subgraph with BFS up to radius levels
    let seed_nodes = vec![center_node];
    let edges = store.graph_walk(&seed_nodes, &edge_types, radius, None);

    // Collect unique nodes from the edges
    let mut unique_nodes: std::collections::HashSet<u64> = std::collections::HashSet::new();
    unique_nodes.insert(center_node);

    for (src, dst, _) in &edges {
        unique_nodes.insert(*src);
        unique_nodes.insert(*dst);
    }

    // Convert to sorted vector for consistent output
    let mut nodes_vec: Vec<u64> = unique_nodes.into_iter().collect();
    nodes_vec.sort();

    // Build the response
    let mut lines = vec![
        "Extracted subgraph:".to_string(),
        format!("Center node: {}", center_node),
        format!("Radius: {}", radius),
        format!("Nodes: {}", nodes_vec.len()),
        format!("Edges: {}", edges.len()),
        String::new(),
        format!("Nodes: {:?}", nodes_vec),
        String::new(),
        String::from("Edges:"),
    ];

    // Format edges with index
    for (idx, (src, dst, edge_type)) in edges.iter().enumerate() {
        lines.push(format!("[{}] {} --{:?}--> {}", idx, src, edge_type, dst));
    }

    if edges.is_empty() {
        lines.push("No edges found in subgraph.".to_string());
    }

    mcp_text_result(id, lines.join("\n"))
}

// ============================================================================
// MCP Helper Functions
// ============================================================================

#[cfg(feature = "mcp")]
fn mcp_parse_atom_type(s: &str) -> Option<AtomType> {
    match s.to_uppercase().as_str() {
        "FACT" => Some(AtomType::FACT),
        "DEFINITION" => Some(AtomType::DEFINITION),
        "PROCEDURE" => Some(AtomType::PROCEDURE),
        "OBSERVATION" => Some(AtomType::OBSERVATION),
        "RULE" => Some(AtomType::RULE),
        "HYPOTHESIS" => Some(AtomType::HYPOTHESIS),
        "EXAMPLE" => Some(AtomType::EXAMPLE),
        "COUNTEREXAMPLE" => Some(AtomType::COUNTEREXAMPLE),
        "DATASET" => Some(AtomType::DATASET),
        "MEASUREMENT" => Some(AtomType::MEASUREMENT),
        "DECISION" => Some(AtomType::DECISION),
        "CONFLICT" => Some(AtomType::CONFLICT),
        "MAP" => Some(AtomType::MAP),
        _ => None,
    }
}

#[cfg(feature = "mcp")]
fn mcp_parse_claim_from_json(value: &serde_json::Value) -> Result<ClaimData, String> {
    let subj = value.get("subj").and_then(|v| v.as_u64()).unwrap_or(0);
    let pred = value.get("pred").and_then(|v| v.as_u64()).unwrap_or(0);
    let obj_tag = value.get("obj_tag").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
    let obj_val = value.get("obj_val").and_then(|v| v.as_u64()).unwrap_or(0);
    let qualifiers_mask = value
        .get("qualifiers_mask")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

    Ok(ClaimData {
        subj,
        pred,
        obj_tag,
        obj_val,
        qualifiers_mask,
    })
}

#[cfg(feature = "mcp")]
fn mcp_parse_batch_atom_from_json(value: &serde_json::Value) -> Result<BatchAtom, String> {
    let atom_type_str = value
        .get("atom_type")
        .and_then(|v| v.as_str())
        .unwrap_or("FACT");
    let atom_type = mcp_parse_atom_type(atom_type_str)
        .ok_or_else(|| format!("Invalid atom type: {}", atom_type_str))?;

    let claims_json: Vec<serde_json::Value> = value
        .get("claims")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let claims: Vec<ClaimData> = claims_json
        .iter()
        .map(mcp_parse_claim_from_json)
        .collect::<Result<Vec<_>, _>>()?;

    let symbols: Vec<String> = value
        .get("symbols")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let payload = mcp_build_atom_payload(atom_type, &symbols, &claims, 5000, 0xFFFF)
        .map_err(|e| format!("Failed to build payload: {}", e))?;

    Ok(BatchAtom::new(payload, atom_type, claims, vec![]))
}

#[cfg(feature = "mcp")]
fn mcp_parse_atom_id(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    let s = s.strip_prefix("0x").unwrap_or(s);

    if s.len() != 64 {
        return None;
    }

    let mut bytes = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let chunk_str = std::str::from_utf8(chunk).ok()?;
        bytes[i] = u8::from_str_radix(chunk_str, 16).ok()?;
    }

    Some(bytes)
}

#[cfg(feature = "mcp")]
fn mcp_parse_delete_reason(s: &str) -> DeleteReason {
    match s.to_uppercase().as_str() {
        "CORRECTION" => DeleteReason::Correction,
        "RETRACTION" => DeleteReason::Retraction,
        "DUPLICATE" => DeleteReason::Duplicate,
        "LEGAL" => DeleteReason::Legal,
        _ => DeleteReason::Obsolete,
    }
}

#[cfg(feature = "mcp")]
fn mcp_parse_source_kind(s: &str) -> Option<SourceKind> {
    match s.to_ascii_lowercase().as_str() {
        "file" => Some(SourceKind::File),
        "page" => Some(SourceKind::Page),
        "repository" => Some(SourceKind::Repository),
        "commit" => Some(SourceKind::Commit),
        "api" => Some(SourceKind::Api),
        "message" => Some(SourceKind::Message),
        "table" => Some(SourceKind::Table),
        "measurement" => Some(SourceKind::Measurement),
        "human" => Some(SourceKind::Human),
        "agent" => Some(SourceKind::Agent),
        _ => None,
    }
}

#[cfg(feature = "mcp")]
fn mcp_parse_branch_reason(s: &str) -> BranchReason {
    match s.to_uppercase().as_str() {
        "CONFLICT" => BranchReason::Conflict,
        "HYPOTHESIS" => BranchReason::Hypothesis,
        "ALTERNATIVE" => BranchReason::Alternative,
        _ => BranchReason::Manual,
    }
}

#[cfg(feature = "mcp")]
fn mcp_build_atom_payload(
    atom_type: AtomType,
    symbols: &[String],
    claims: &[ClaimData],
    trust_level: u16,
    domain_mask: u64,
) -> Result<Vec<u8>, String> {
    use memoryx::cas::claims::{ClaimRecord, ClaimsSection};
    use memoryx::cas::evidence::EvidenceSection;
    use memoryx::cas::invariants::InvariantsSection;
    use memoryx::cas::meta::{MetaField, MetaFieldKind, MetaSection, MetaValue};
    use memoryx::cas::symbols::SymbolsSection;
    use memoryx::utils::crc32;

    // Create SYMBOLS section
    let mut symbols_section = SymbolsSection::new();
    for sym in symbols {
        symbols_section.intern(sym.clone());
    }
    // Add default symbols for claim indices
    for i in 0..claims.len().max(2) {
        symbols_section.intern(format!("sym_{}", i));
    }
    let symbols_bytes = symbols_section.to_bytes();

    // REFS section: empty
    let refs_bytes: Vec<u8> = vec![];

    // CLAIMS section
    let mut claims_section = ClaimsSection::new();
    for claim in claims {
        let predicate = u32::try_from(claim.pred)
            .map_err(|_| format!("predicate {} exceeds SymId", claim.pred))?;
        let object_tag = ObjTag::from_u8(claim.obj_tag)
            .ok_or_else(|| format!("invalid object tag {}", claim.obj_tag))?;
        claims_section.add_claim(
            ClaimRecord::from_scalar(claim.subj, predicate, object_tag, claim.obj_val)
                .map_err(|error| error.to_string())?,
        );
    }
    let claims_bytes = claims_section.to_bytes();

    // INVARIANTS section
    let invariants_bytes = InvariantsSection::new().to_bytes();

    // EDGES section: empty
    let edges_bytes: Vec<u8> = vec![];

    // EVIDENCE section
    let evidence_bytes = EvidenceSection::new().to_bytes();

    // META section
    let mut meta_section = MetaSection::new();
    meta_section.add_field(MetaField::new(
        MetaFieldKind::TRUST_SCORE,
        MetaValue::F32(trust_level as f32 / 10000.0),
    ));
    meta_section.add_field(MetaField::new(
        MetaFieldKind::DOMAIN_MASK,
        MetaValue::U32(domain_mask as u32),
    ));
    let meta_bytes = meta_section.to_bytes();

    // Calculate offsets: header (48) + 7 descriptors (7*32=224) = 272 bytes
    let sections_data_start: usize = 48 + 7 * 32;

    let mut current_off = sections_data_start;
    let symbols_off = current_off;
    current_off += symbols_bytes.len();

    let refs_off = current_off;
    current_off += refs_bytes.len();

    let claims_off = current_off;
    current_off += claims_bytes.len();

    let invariants_off = current_off;
    current_off += invariants_bytes.len();

    let edges_off = current_off;
    current_off += edges_bytes.len();

    let evidence_off = current_off;
    current_off += evidence_bytes.len();

    let meta_off = current_off;

    let mut body = Vec::new();

    // AtomBodyHeader (48 bytes)
    body.extend_from_slice(&0x41544F4Du32.to_le_bytes()); // ATOM magic
    body.extend_from_slice(&0x0001u16.to_le_bytes()); // body_ver
    body.extend_from_slice(&0u16.to_le_bytes()); // body_flags
    body.extend_from_slice(&0u64.to_le_bytes()); // created_at
    body.extend_from_slice(&0u64.to_le_bytes()); // valid_from
    body.extend_from_slice(&u64::MAX.to_le_bytes()); // valid_to
    body.extend_from_slice(&(atom_type as u32).to_le_bytes()); // atom_type
    body.extend_from_slice(&7u32.to_le_bytes()); // section_count = 7
    body.extend_from_slice(&48u64.to_le_bytes()); // section_table_off

    // Helper to add section descriptor
    let mut add_section_desc = |kind: u32, off: usize, data: &[u8]| {
        let crc = crc32(data);
        body.extend_from_slice(&kind.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // flags
        body.extend_from_slice(&(off as u64).to_le_bytes());
        body.extend_from_slice(&(data.len() as u64).to_le_bytes());
        body.extend_from_slice(&crc.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // reserved
    };

    // Section descriptors (order matters)
    add_section_desc(0x01, symbols_off, &symbols_bytes); // SYMBOLS
    add_section_desc(0x02, refs_off, &refs_bytes); // REFS
    add_section_desc(0x03, claims_off, &claims_bytes); // CLAIMS
    add_section_desc(0x04, invariants_off, &invariants_bytes); // INVARIANTS
    add_section_desc(0x05, edges_off, &edges_bytes); // EDGES
    add_section_desc(0x06, evidence_off, &evidence_bytes); // EVIDENCE
    add_section_desc(0x07, meta_off, &meta_bytes); // META

    // Section data
    body.extend_from_slice(&symbols_bytes);
    body.extend_from_slice(&refs_bytes);
    body.extend_from_slice(&claims_bytes);
    body.extend_from_slice(&invariants_bytes);
    body.extend_from_slice(&edges_bytes);
    body.extend_from_slice(&evidence_bytes);
    body.extend_from_slice(&meta_bytes);

    Ok(body)
}

// ============================================================================
// Main Entry Point
// ============================================================================

fn main() {
    let cli = Cli::parse();

    // Disable colors if requested
    if cli.no_color {
        colored::control::set_override(false);
    }

    // Load configuration
    let config = match load_config(cli.config.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            print_error(&format!("Failed to load config: {}", e));
            std::process::exit(1);
        }
    };

    // Execute command
    let result = match &cli.command {
        Commands::Init { base, force } => resolve_base_path(
            base.as_ref(),
            cli.base_scope,
            cli.base_name.as_deref(),
            &config,
        )
        .and_then(|resolved| cmd_init(&resolved, *force)),
        Commands::Ingest {
            base,
            files,
            atom_type,
            batch_size,
            extract_claims,
            dry_run,
            extractor,
        } => resolve_base_path(
            base.as_ref(),
            cli.base_scope,
            cli.base_name.as_deref(),
            &config,
        )
        .and_then(|resolved| {
            cmd_ingest(IngestCliOptions {
                base: &resolved,
                files: files.as_slice(),
                atom_type: atom_type.as_str(),
                batch_size: *batch_size,
                extract_claims: *extract_claims,
                dry_run: *dry_run,
                extractor_name: extractor.as_str(),
                output_format: cli.format,
                verbose: cli.verbose,
            })
        }),
        Commands::Query {
            base,
            query,
            contract,
            emit_contract,
            include_trace,
            explain_rejections,
            ctx_policy,
            limit,
            min_trust,
        } => resolve_base_path(
            base.as_ref(),
            cli.base_scope,
            cli.base_name.as_deref(),
            &config,
        )
        .and_then(|resolved| {
            cmd_query(QueryCliOptions {
                base: &resolved,
                query: query.as_deref(),
                contract_path: contract.as_deref(),
                emit_contract: *emit_contract,
                ctx_policy: *ctx_policy,
                _limit: *limit,
                _min_trust: *min_trust,
                include_trace: *include_trace,
                explain_rejections: *explain_rejections,
                format: cli.format,
            })
        }),
        Commands::Compact {
            base,
            compaction_type,
            dry_run,
        } => resolve_base_path(
            base.as_ref(),
            cli.base_scope,
            cli.base_name.as_deref(),
            &config,
        )
        .and_then(|resolved| cmd_compact(&resolved, *compaction_type, *dry_run)),
        Commands::Export {
            base,
            format,
            output,
            filter,
        } => resolve_base_path(
            base.as_ref(),
            cli.base_scope,
            cli.base_name.as_deref(),
            &config,
        )
        .and_then(|resolved| cmd_export(&resolved, *format, output.as_deref(), filter.as_deref())),
        Commands::Import {
            base,
            format,
            input,
            skip_validation,
        } => resolve_base_path(
            base.as_ref(),
            cli.base_scope,
            cli.base_name.as_deref(),
            &config,
        )
        .and_then(|resolved| cmd_import(&resolved, *format, input.as_path(), *skip_validation)),
        Commands::Stats { base, detailed } => resolve_base_path(
            base.as_ref(),
            cli.base_scope,
            cli.base_name.as_deref(),
            &config,
        )
        .and_then(|resolved| cmd_stats(&resolved, *detailed, cli.format)),
        Commands::VerifyIntegrity { base } => resolve_base_path(
            base.as_ref(),
            cli.base_scope,
            cli.base_name.as_deref(),
            &config,
        )
        .and_then(|resolved| cmd_verify_integrity(&resolved, cli.format)),
        Commands::RebuildIndex { base, dry_run } => resolve_base_path(
            base.as_ref(),
            cli.base_scope,
            cli.base_name.as_deref(),
            &config,
        )
        .and_then(|resolved| cmd_rebuild_index(&resolved, *dry_run, cli.format)),
        Commands::Repair { base } => resolve_base_path(
            base.as_ref(),
            cli.base_scope,
            cli.base_name.as_deref(),
            &config,
        )
        .and_then(|resolved| cmd_repair(&resolved, cli.format)),
        Commands::History { base, limit } => resolve_base_path(
            base.as_ref(),
            cli.base_scope,
            cli.base_name.as_deref(),
            &config,
        )
        .and_then(|resolved| cmd_history(&resolved, *limit, cli.format)),
        Commands::Snapshot { base, ctx } => resolve_base_path(
            base.as_ref(),
            cli.base_scope,
            cli.base_name.as_deref(),
            &config,
        )
        .and_then(|resolved| cmd_snapshot(&resolved, *ctx, cli.format)),
        Commands::CreateEntity {
            base,
            name,
            entity_type,
            form,
        } => resolve_base_path(
            base.as_ref(),
            cli.base_scope,
            cli.base_name.as_deref(),
            &config,
        )
        .and_then(|resolved| {
            cmd_create_entity(
                &resolved,
                name.as_deref(),
                entity_type.as_deref(),
                form.as_deref(),
                cli.format,
            )
        }),
        Commands::AddEntityClaim {
            base,
            entity,
            predicate,
            object,
            object_tag,
            ctx,
        } => resolve_base_path(
            base.as_ref(),
            cli.base_scope,
            cli.base_name.as_deref(),
            &config,
        )
        .and_then(|resolved| {
            cmd_add_entity_claim(
                &resolved,
                *entity,
                *predicate,
                *object,
                object_tag.as_deref(),
                *ctx,
                cli.format,
            )
        }),
        Commands::CreateRelation {
            base,
            subject,
            predicate,
            object,
            ctx,
        } => resolve_base_path(
            base.as_ref(),
            cli.base_scope,
            cli.base_name.as_deref(),
            &config,
        )
        .and_then(|resolved| {
            cmd_create_relation(&resolved, *subject, *predicate, *object, *ctx, cli.format)
        }),
        Commands::Serve {
            base,
            port,
            host,
            stdio,
        } => resolve_base_path(
            base.as_ref(),
            cli.base_scope,
            cli.base_name.as_deref(),
            &config,
        )
        .and_then(|resolved| cmd_serve(&resolved, *port, host.as_str(), *stdio)),
    };

    if let Err(e) = result {
        print_error(&e.to_string());
        std::process::exit(1);
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn seed_single_fact_atom(base: &Path) -> (MemoryX, AtomId) {
        let mut store = MemoryX::new(StoreConfig::new(base.to_path_buf())).unwrap();
        let claims = vec![ClaimData {
            subj: 11,
            pred: 22,
            obj_tag: ObjTag::U64.to_u8(),
            obj_val: 33,
            qualifiers_mask: 0,
        }];
        let payload = create_minimal_atom_body(AtomType::FACT, &claims);
        let atom_id = store
            .ingest(&payload, AtomType::FACT, &claims, &[])
            .unwrap();
        (store, atom_id)
    }

    #[cfg(feature = "mcp")]
    fn mcp_text(response: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(response).unwrap();
        value["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[cfg(feature = "mcp")]
    async fn process_mcp_request(state: &mut McpServerState, request: &str) -> String {
        super::process_mcp_request(state, request)
            .await
            .expect("JSON-RPC request with an id must produce a response")
    }

    #[cfg(feature = "mcp")]
    fn assert_schema_accepts(schema: &serde_json::Value, value: &serde_json::Value) {
        fn matches(
            schema: &serde_json::Value,
            value: &serde_json::Value,
            inherited_root: &serde_json::Value,
        ) -> bool {
            let root = if schema.get("$id").is_some() {
                schema
            } else {
                inherited_root
            };
            if let Some(reference) = schema.get("$ref").and_then(serde_json::Value::as_str) {
                let Some(pointer) = reference.strip_prefix('#') else {
                    return false;
                };
                return root
                    .pointer(pointer)
                    .is_some_and(|resolved| matches(resolved, value, root));
            }
            if let Some(options) = schema.get("oneOf").and_then(serde_json::Value::as_array)
                && options
                    .iter()
                    .filter(|option| matches(option, value, root))
                    .count()
                    != 1
            {
                return false;
            }
            if let Some(allowed) = schema.get("enum").and_then(serde_json::Value::as_array)
                && !allowed.contains(value)
            {
                return false;
            }
            if let Some(types) = schema.get("type") {
                let accepts = |kind: &str| match kind {
                    "null" => value.is_null(),
                    "boolean" => value.is_boolean(),
                    "string" => value.is_string(),
                    "number" => value.is_number(),
                    "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
                    "array" => value.is_array(),
                    "object" => value.is_object(),
                    _ => false,
                };
                let valid_type = types.as_str().is_some_and(accepts)
                    || types.as_array().is_some_and(|items| {
                        items
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .any(accepts)
                    });
                if !valid_type {
                    return false;
                }
            }
            if let Some(number) = value.as_f64()
                && (schema
                    .get("minimum")
                    .and_then(serde_json::Value::as_f64)
                    .is_some_and(|minimum| number < minimum)
                    || schema
                        .get("maximum")
                        .and_then(serde_json::Value::as_f64)
                        .is_some_and(|maximum| number > maximum))
            {
                return false;
            }
            if let Some(array) = value.as_array() {
                if schema
                    .get("minItems")
                    .and_then(serde_json::Value::as_u64)
                    .is_some_and(|minimum| array.len() < minimum as usize)
                    || schema
                        .get("maxItems")
                        .and_then(serde_json::Value::as_u64)
                        .is_some_and(|maximum| array.len() > maximum as usize)
                {
                    return false;
                }
                if let Some(items) = schema.get("items")
                    && !array.iter().all(|item| matches(items, item, root))
                {
                    return false;
                }
            }
            if let Some(object) = value.as_object() {
                if schema
                    .get("required")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|required| {
                        required
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .any(|key| !object.contains_key(key))
                    })
                {
                    return false;
                }
                let properties = schema
                    .get("properties")
                    .and_then(serde_json::Value::as_object);
                if schema.get("additionalProperties") == Some(&serde_json::Value::Bool(false))
                    && object
                        .keys()
                        .any(|key| properties.is_none_or(|known| !known.contains_key(key)))
                {
                    return false;
                }
                if let Some(properties) = properties {
                    for (key, child) in object {
                        if let Some(child_schema) = properties.get(key)
                            && !matches(child_schema, child, root)
                        {
                            return false;
                        }
                    }
                }
            }
            true
        }

        assert!(matches(schema, value, schema), "schema rejected {value}");
    }

    #[cfg(feature = "mcp")]
    fn test_mcp_state(base: PathBuf) -> McpServerState {
        let store = MemoryX::new(StoreConfig::new(base.clone())).unwrap();
        McpServerState::new(base, store).unwrap()
    }

    #[cfg(feature = "mcp")]
    fn test_mcp_active_store_mut(state: &mut McpServerState) -> &mut MemoryX {
        state.stores.get_mut("active").unwrap()
    }

    #[test]
    fn test_atom_type_parsing() {
        assert_eq!(parse_atom_type("fact").unwrap(), AtomType::FACT);
        assert_eq!(parse_atom_type("FACT").unwrap(), AtomType::FACT);
        assert_eq!(parse_atom_type("rule").unwrap(), AtomType::RULE);
        assert!(parse_atom_type("unknown").is_err());
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn test_stdio_serve_uses_stderr_for_diagnostics() {
        assert_eq!(serve_diagnostic_sink(true), DiagnosticSink::Stderr);
        assert_eq!(serve_diagnostic_sink(false), DiagnosticSink::Stdout);
    }

    #[test]
    fn test_atom_id_hex_roundtrip() {
        let original = [42u8; 32];
        let hex = atom_id_to_hex(&original);
        let decoded = hex_to_atom_id(&hex).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn test_claim_export_roundtrip() {
        let original = ClaimExport {
            subject: 1,
            predicate: 2,
            object_tag: 3,
            object_value: 42,
            qualifiers: None,
        };
        let claim_data: ClaimData = original.into();
        let exported = ClaimExport::from(&claim_data);
        assert_eq!(exported.subject, 1);
        assert_eq!(exported.predicate, 2);
    }

    #[test]
    fn answer_pack_json_enforces_explicit_byte_envelope() {
        let mut answer = memoryx::store::api::AnswerPack::new(0);
        answer.response_limits.max_items = 64;
        answer.response_limits.max_bytes = 2_048;
        answer
            .limitations
            .push(memoryx::store::api::Limitation::warning(
                memoryx::store::api::LimitationCode::BudgetExhausted,
                "large".repeat(2_048),
            ));
        let value = answer_pack_json(&answer);
        let encoded = value.to_string();
        assert!(encoded.len() <= 2_048);
        assert_eq!(value["status"], "Partial");
        assert_eq!(value["response_limits"]["bytes_truncated"], true);
        assert!(value["response_limits"]["original_bytes"].as_u64().unwrap() > 2_048);
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn mcp_answer_result_bounds_complete_json_rpc_response() {
        let mut answer = memoryx::store::api::AnswerPack::new(0);
        answer.response_limits.max_items = 64;
        answer.response_limits.max_bytes = 2_048;
        answer
            .limitations
            .push(memoryx::store::api::Limitation::warning(
                memoryx::store::api::LimitationCode::BudgetExhausted,
                "large".repeat(2_048),
            ));
        let response = mcp_answer_result(serde_json::json!(7), &answer);
        let encoded = response.to_string();
        assert!(encoded.len() <= 2_048);
        let payload: serde_json::Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(payload["response_limits"]["bytes_truncated"], true);
        assert_eq!(payload["response_limits"]["emitted_bytes"], encoded.len());
    }

    #[test]
    fn test_export_import_json_roundtrip_uses_live_atoms() {
        let source_dir = tempdir().unwrap();
        let source_base = source_dir.path().join("source");
        let (source_store, atom_id) = seed_single_fact_atom(&source_base);
        let export = atom_export_from_store(&source_store, &atom_id).unwrap();

        let json_path = source_dir.path().join("atoms.json");
        let exported = serde_json::to_string_pretty(&vec![export]).unwrap();
        std::fs::write(&json_path, &exported).unwrap();
        assert!(exported.contains(&atom_id_to_hex(&atom_id)));
        assert!(
            !exported.contains("0101010101010101010101010101010101010101010101010101010101010101")
        );

        let imported_dir = tempdir().unwrap();
        let imported_base = imported_dir.path().join("imported");
        cmd_import(&imported_base, ExportFormat::Json, &json_path, false).unwrap();

        let reopened = MemoryX::new(StoreConfig::new(imported_base.clone())).unwrap();
        let live_ids = reopened.list_atom_ids();
        assert_eq!(live_ids, vec![atom_id]);
    }

    #[test]
    fn test_export_import_csv_roundtrip_uses_live_atoms() {
        let source_dir = tempdir().unwrap();
        let source_base = source_dir.path().join("source");
        let (source_store, atom_id) = seed_single_fact_atom(&source_base);
        let export = atom_export_from_store(&source_store, &atom_id).unwrap();

        let csv_path = source_dir.path().join("atoms.csv");
        let exported = write_csv_atoms(&[export]).unwrap();
        std::fs::write(&csv_path, &exported).unwrap();
        assert!(exported.contains("atom_type"));
        assert!(exported.contains(&atom_id_to_hex(&atom_id)));

        let imported_dir = tempdir().unwrap();
        let imported_base = imported_dir.path().join("imported");
        cmd_import(&imported_base, ExportFormat::Csv, &csv_path, false).unwrap();

        let reopened = MemoryX::new(StoreConfig::new(imported_base)).unwrap();
        assert_eq!(reopened.list_atom_ids(), vec![atom_id]);
    }

    #[test]
    fn test_create_minimal_atom_body() {
        let claims = vec![ClaimData {
            subj: 1,
            pred: 2,
            obj_tag: 3,
            obj_val: 42,
            qualifiers_mask: 0,
        }];
        let body = create_minimal_atom_body(AtomType::FACT, &claims);
        assert!(body.len() >= 48 + 224 + 128); // Minimum valid size
    }

    #[test]
    fn test_resolve_project_base_by_name() {
        let config = MemoryXConfig::default();
        let resolved = resolve_base_path(
            Some(&PathBuf::from("default")),
            Some(BaseScope::Project),
            None,
            &config,
        )
        .unwrap();

        assert!(resolved.ends_with(Path::new(".memoryx").join("bases").join("default")));
    }

    #[test]
    fn test_resolve_user_base_by_name() {
        let config = MemoryXConfig::default();
        let resolved = resolve_base_path(
            Some(&PathBuf::from("shared")),
            Some(BaseScope::User),
            None,
            &config,
        )
        .unwrap();

        assert!(resolved.ends_with(Path::new(".memoryx").join("bases").join("shared")));
    }

    #[test]
    fn test_reject_base_outside_allowed_roots() {
        let config = MemoryXConfig::default();
        let outside = std::env::temp_dir().join("memoryx_outside_root");
        let err = resolve_base_path(Some(&outside), None, None, &config).unwrap_err();

        assert!(matches!(err, CliError::Validation(_)));
    }

    #[test]
    fn test_validate_allowed_base_path_accepts_scoped_missing_leaf() {
        let base = project_base_root()
            .unwrap()
            .join(format!("mp_lock_02_valid_{}", std::process::id()));

        let validated = validate_allowed_base_path(&base).unwrap();
        let project_root = canonical_physical_path(&project_base_root().unwrap()).unwrap();

        assert!(validated.starts_with(project_root));
    }

    #[test]
    fn test_compact_rejects_held_memoryx_lease_then_succeeds_after_drop() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("memoryx");
        let store = MemoryX::new(StoreConfig::new(base.clone())).unwrap();

        let error = cmd_compact(&base, CompactionType::All, true).unwrap_err();
        assert!(matches!(error, CliError::Store(_)));
        assert!(error.to_string().contains("exclusive writer lease"));

        drop(store);
        cmd_compact(&base, CompactionType::All, true).unwrap();
    }

    #[cfg(unix)]
    fn create_directory_link(target: &Path, link: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_directory_link(target: &Path, link: &Path) -> io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn test_validate_allowed_base_path_rejects_scoped_symlink_escape() {
        let project_root = project_base_root().unwrap();
        std::fs::create_dir_all(&project_root).unwrap();
        let link = project_root.join(format!("mp_lock_02_escape_{}", std::process::id()));
        let outside = tempdir().unwrap();

        let link_result = create_directory_link(outside.path(), &link);
        if let Err(error) = link_result {
            if error.kind() == io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("failed to create test directory link: {error}");
        }

        let result = validate_allowed_base_path(&link.join("escaped_base"));
        let _ = std::fs::remove_dir(&link);

        assert!(matches!(result, Err(CliError::Validation(_))));
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn test_federation_base_id_is_persistent_and_unique_per_base() {
        let dir = tempdir().unwrap();
        let base_a = dir.path().join("base_a");
        let base_b = dir.path().join("base_b");

        let first_a = load_or_create_federation_base_id(&base_a).unwrap();
        let second_a = load_or_create_federation_base_id(&base_a).unwrap();
        let first_b = load_or_create_federation_base_id(&base_b).unwrap();

        assert_eq!(first_a, second_a);
        assert_ne!(first_a, first_b);
        assert!(federation_base_id_path(&base_a).exists());
        assert!(federation_base_id_path(&base_b).exists());
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn test_mcp_initialize_negotiates_allowlisted_versions_and_rejects_invalid_input() {
        let dir = tempdir().unwrap();
        let mut state = test_mcp_state(dir.path().join("memoryx"));

        for (id, protocol_version) in SUPPORTED_MCP_PROTOCOL_VERSIONS.iter().enumerate() {
            let request = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "initialize",
                "params": {
                    "protocolVersion": protocol_version,
                    "capabilities": {},
                    "clientInfo": {"name": "unit-test", "version": "1.0"}
                }
            })
            .to_string();
            let response = process_mcp_request(&mut state, &request).await;
            let value: serde_json::Value = serde_json::from_str(&response).unwrap();

            assert_eq!(value["id"], serde_json::json!(id));
            assert_eq!(value["result"]["protocolVersion"], *protocol_version);
            assert_eq!(
                value["result"]["capabilities"],
                serde_json::json!({"tools": {}})
            );
            assert_eq!(value["result"]["serverInfo"]["name"], "memoryx");
            assert!(value.get("error").is_none());
        }

        let invalid_versions = [
            serde_json::json!({}),
            serde_json::json!({"protocolVersion": 20251125}),
            serde_json::json!({"protocolVersion": "2025-03-26"}),
        ];
        for (index, params) in invalid_versions.into_iter().enumerate() {
            let id = 100 + index;
            let request = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "initialize",
                "params": params
            })
            .to_string();
            let response = process_mcp_request(&mut state, &request).await;
            let value: serde_json::Value = serde_json::from_str(&response).unwrap();

            assert_eq!(value["id"], serde_json::json!(id));
            assert_eq!(value["error"]["code"], -32602);
            assert_eq!(
                value["error"]["data"]["supportedProtocolVersions"],
                serde_json::json!(SUPPORTED_MCP_PROTOCOL_VERSIONS)
            );
            assert!(value.get("result").is_none());
        }
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn test_mcp_notifications_never_produce_responses() {
        let dir = tempdir().unwrap();
        let mut state = test_mcp_state(dir.path().join("memoryx"));

        for method in ["notifications/initialized", "notifications/unknown"] {
            let notification = serde_json::json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": {}
            })
            .to_string();

            assert!(
                super::process_mcp_request(&mut state, &notification)
                    .await
                    .is_none(),
                "notification {method} must not produce a JSON-RPC response"
            );
        }
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn test_mcp_tools_list_reports_real_core_surface() {
        let dir = tempdir().unwrap();
        let mut state = test_mcp_state(dir.path().join("memoryx"));
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list"
        })
        .to_string();

        let response = process_mcp_request(&mut state, &request).await;
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        let tools = value["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();

        for tool in tools {
            let description = tool["description"].as_str().unwrap_or("");
            assert!(
                !description.trim().is_empty(),
                "tool {} must have a non-empty description",
                tool["name"].as_str().unwrap_or("<unknown>")
            );
            let examples_len = tool["inputSchema"]["examples"]
                .as_array()
                .map(|examples| examples.len())
                .unwrap_or(0);
            assert!(
                examples_len > 0,
                "tool {} must have at least one example",
                tool["name"].as_str().unwrap_or("<unknown>")
            );
        }

        assert_eq!(BASE_SELECTABLE_MCP_TOOLS.len(), 36);
        for name in BASE_SELECTABLE_MCP_TOOLS {
            let tool = tools
                .iter()
                .find(|tool| tool["name"].as_str() == Some(name))
                .unwrap_or_else(|| panic!("base-selectable tool {name} is missing"));
            assert!(
                tool["inputSchema"]["properties"].get("base_ref").is_some(),
                "tool {name} must advertise optional base_ref"
            );
            // Static examples stay executable against the active base. The
            // optional base_ref property documents explicit routing without
            // pretending that an arbitrary named base is already connected.
        }

        assert!(names.contains(&"query"));
        assert!(names.contains(&"list_bases"));
        assert!(names.contains(&"active_base"));
        assert!(names.contains(&"connect_base"));
        assert!(names.contains(&"switch_base"));
        assert!(names.contains(&"query_base"));
        assert!(names.contains(&"compile_query_contract"));
        assert!(names.contains(&"validate_query_contract"));
        assert!(names.contains(&"explain_answer_graph"));
        assert!(names.contains(&"get_provenance_path"));
        assert!(names.contains(&"search_lex"));
        assert!(names.contains(&"search_graph"));
        assert!(names.contains(&"search_semantic"));
        assert!(names.contains(&"ingest"));
        assert!(names.contains(&"batch_ingest"));
        assert!(names.contains(&"update_atom"));
        assert!(names.contains(&"supersede_claim"));
        assert!(names.contains(&"correct_claim"));
        assert!(names.contains(&"delete_atom"));
        assert!(names.contains(&"history"));
        assert!(names.contains(&"register_source"));
        assert!(names.contains(&"list_sources"));
        assert!(names.contains(&"attach_atom_source"));
        assert!(names.contains(&"register_predicate"));
        assert!(names.contains(&"list_predicates"));
        assert!(names.contains(&"get_predicate"));
        assert!(names.contains(&"resolve_predicate"));
        assert!(names.contains(&"create_entity"));
        assert!(names.contains(&"list_entities"));
        assert!(names.contains(&"alias_entity"));
        assert!(names.contains(&"merge_entities"));
        assert!(names.contains(&"split_entity"));
        assert!(names.contains(&"add_claim"));
        assert!(names.contains(&"assert_relation"));
        assert!(names.contains(&"correct_relation"));
        assert!(names.contains(&"create_context"));
        assert!(names.contains(&"list_contexts"));
        assert!(names.contains(&"branch_context"));
        assert!(names.contains(&"list_conflicts"));
        assert!(names.contains(&"graph_neighbors"));
        assert!(names.contains(&"graph_walk"));
        assert!(names.contains(&"extract_subgraph"));
        assert_eq!(names.len(), 42);
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn test_mcp_multi_base_registry_and_query_base_are_store_backed() {
        let dir = tempdir().unwrap();
        let primary = dir.path().join("primary");
        let mut state = test_mcp_state(primary.clone());
        let secondary_name = format!("multi_base_test_{}", std::process::id());

        let active_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 101,
            "method": "tools/call",
            "params": {
                "name": "active_base",
                "arguments": {}
            }
        })
        .to_string();
        let active_response = process_mcp_request(&mut state, &active_request).await;
        assert!(mcp_text(&active_response).contains("\"active_base_ref\": \"active\""));

        let connect_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 102,
            "method": "tools/call",
            "params": {
                "name": "connect_base",
                "arguments": {
                    "base_ref": "secondary",
                    "scope": "project",
                    "name": secondary_name
                }
            }
        })
        .to_string();
        let connect_response = process_mcp_request(&mut state, &connect_request).await;
        let connect_text = mcp_text(&connect_response);
        assert!(connect_text.contains("\"base_ref\": \"secondary\""));
        assert!(connect_text.contains("\"connected\": true"));

        let stores_after_first_connect = state.stores.len();
        let repeated_connect_response = process_mcp_request(&mut state, &connect_request).await;
        assert!(!repeated_connect_response.contains("\"error\""));
        assert_eq!(state.stores.len(), stores_after_first_connect);

        let alias_connect_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1021,
            "method": "tools/call",
            "params": {
                "name": "connect_base",
                "arguments": {
                    "base_ref": "secondary_alias",
                    "scope": "project",
                    "name": secondary_name
                }
            }
        })
        .to_string();
        let alias_connect_response = process_mcp_request(&mut state, &alias_connect_request).await;
        assert!(alias_connect_response.contains("already connected as 'secondary'"));
        assert_eq!(state.stores.len(), stores_after_first_connect);

        let list_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 103,
            "method": "tools/call",
            "params": {
                "name": "list_bases",
                "arguments": {}
            }
        })
        .to_string();
        let list_response = process_mcp_request(&mut state, &list_request).await;
        let list_text = mcp_text(&list_response);
        assert!(list_text.contains("\"active_base_ref\": \"active\""));
        assert!(list_text.contains("\"base_ref\": \"secondary\""));

        let missing_base_ref_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1035,
            "method": "tools/call",
            "params": {
                "name": "query_base",
                "arguments": {
                    "query_text": "This should fail without an explicit base_ref"
                }
            }
        })
        .to_string();
        let missing_base_ref_response =
            process_mcp_request(&mut state, &missing_base_ref_request).await;
        assert!(missing_base_ref_response.contains("Missing required string field 'base_ref'"));

        let query_base_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 104,
            "method": "tools/call",
            "params": {
                "name": "query_base",
                "arguments": {
                    "base_ref": "secondary",
                    "query_text": "What is stored here?",
                    "ctx_id": 0
                }
            }
        })
        .to_string();
        let query_base_response = process_mcp_request(&mut state, &query_base_request).await;
        let query_base_text = mcp_text(&query_base_response);
        let query_base_value: serde_json::Value = serde_json::from_str(&query_base_text).unwrap();
        assert!(query_base_value.get("selected_ctx").is_some());
        assert_eq!(state.active_base_ref, "active");

        let switch_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 105,
            "method": "tools/call",
            "params": {
                "name": "switch_base",
                "arguments": {
                    "base_ref": "secondary"
                }
            }
        })
        .to_string();
        let switch_response = process_mcp_request(&mut state, &switch_request).await;
        assert!(mcp_text(&switch_response).contains("\"active\": true"));
        assert_eq!(state.active_base_ref, "secondary");

        let secondary_path = project_base_root().unwrap().join(&secondary_name);
        if secondary_path.exists() {
            std::fs::remove_dir_all(secondary_path).unwrap();
        }
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn test_mcp_create_context_and_query_are_store_backed() {
        let dir = tempdir().unwrap();
        let mut state = test_mcp_state(dir.path().join("memoryx"));

        let create_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "create_context",
                "arguments": {"policy_id": 7}
            }
        })
        .to_string();
        let create_response = process_mcp_request(&mut state, &create_request).await;
        assert!(create_response.contains("created_ctx="));
        assert!(create_response.contains("policy_id=7"));

        let query_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "query",
                "arguments": {"question": "What is this?", "ctx_id": 0}
            }
        })
        .to_string();
        let query_response = process_mcp_request(&mut state, &query_request).await;
        let query_text = mcp_text(&query_response);
        let query_value: serde_json::Value = serde_json::from_str(&query_text).unwrap();
        assert!(query_value.get("selected_ctx").is_some());
        assert!(query_value.get("graph").is_some());
        assert!(!query_response.contains("Query executed"));

        let compile_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "compile_query_contract",
                "arguments": {"query_text": "Explain MemoryX MCP"}
            }
        })
        .to_string();
        let compile_response = process_mcp_request(&mut state, &compile_request).await;
        let contract_text = mcp_text(&compile_response);
        assert!(contract_text.contains("\"intent\": \"explain\""));

        let contract_value: serde_json::Value = serde_json::from_str(&contract_text).unwrap();

        let validate_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "validate_query_contract",
                "arguments": {"contract": contract_value}
            }
        })
        .to_string();
        let validate_response = process_mcp_request(&mut state, &validate_request).await;
        assert!(mcp_text(&validate_response).contains("\"valid\":true"));

        let explain_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "explain_answer_graph",
                "arguments": {"query_text": "Explain MemoryX MCP", "ctx_id": 0}
            }
        })
        .to_string();
        let explain_response = process_mcp_request(&mut state, &explain_request).await;
        let explain_text = mcp_text(&explain_response);
        assert!(explain_text.contains("\"coverage_report\""));
        assert!(explain_text.contains("\"snapshot\""));
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn test_mcp_ingest_search_graph_and_neighbors_are_store_backed() {
        let dir = tempdir().unwrap();
        let mut state = test_mcp_state(dir.path().join("memoryx"));

        let ingest_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "ingest",
                "arguments": {
                    "atom_type": "FACT",
                    "claims": [{
                        "subj": 99,
                        "pred": 7,
                        "obj_tag": ObjTag::U64.to_u8(),
                        "obj_val": 123
                    }]
                }
            }
        })
        .to_string();
        let ingest_response = process_mcp_request(&mut state, &ingest_request).await;
        assert!(ingest_response.contains("Successfully ingested atom"));

        let (atom_id, atom_node) = {
            let store = test_mcp_active_store_mut(&mut state);
            let atom_id = store.list_atom_ids().into_iter().next().unwrap();
            let atom_node = store.get_node_num(&atom_id).unwrap();
            (atom_id, atom_node)
        };

        let provenance_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "tools/call",
            "params": {
                "name": "get_provenance_path",
                "arguments": {
                    "atom_id": atom_id_to_hex(&atom_id)
                }
            }
        })
        .to_string();
        let provenance_response = process_mcp_request(&mut state, &provenance_request).await;
        let provenance_text = mcp_text(&provenance_response);
        assert!(provenance_text.contains("\"root_atom_id\""));
        assert!(provenance_text.contains("\"overall_trust\""));

        let register_source_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 13,
            "method": "tools/call",
            "params": {
                "name": "register_source",
                "arguments": {
                    "kind": "file",
                    "label": "test source",
                    "path": "Concept/SKF.txt",
                    "line_start": 1,
                    "line_end": 3
                }
            }
        })
        .to_string();
        let register_source_response =
            process_mcp_request(&mut state, &register_source_request).await;
        assert!(register_source_response.contains("Registered source"));
        assert!(register_source_response.contains("Source ID: 1"));

        let attach_source_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 14,
            "method": "tools/call",
            "params": {
                "name": "attach_atom_source",
                "arguments": {
                    "atom_id": atom_id_to_hex(&atom_id),
                    "source_id": 1
                }
            }
        })
        .to_string();
        let attach_source_response = process_mcp_request(&mut state, &attach_source_request).await;
        assert!(attach_source_response.contains("Attached source"));

        let list_sources_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 15,
            "method": "tools/call",
            "params": {
                "name": "list_sources",
                "arguments": {}
            }
        })
        .to_string();
        let list_sources_response = process_mcp_request(&mut state, &list_sources_request).await;
        assert!(list_sources_response.contains("test source"));

        let create_entity_a_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 16,
            "method": "tools/call",
            "params": {
                "name": "create_entity",
                "arguments": {
                    "canonical_name": "Rust",
                    "entity_type": "language"
                }
            }
        })
        .to_string();
        let create_entity_a_response =
            process_mcp_request(&mut state, &create_entity_a_request).await;
        assert!(create_entity_a_response.contains("Entity ID: 1"));

        let create_entity_b_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 17,
            "method": "tools/call",
            "params": {
                "name": "create_entity",
                "arguments": {
                    "canonical_name": "Ownership",
                    "entity_type": "concept"
                }
            }
        })
        .to_string();
        let create_entity_b_response =
            process_mcp_request(&mut state, &create_entity_b_request).await;
        assert!(create_entity_b_response.contains("Entity ID: 2"));

        let alias_entity_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 18,
            "method": "tools/call",
            "params": {
                "name": "alias_entity",
                "arguments": {
                    "entity_id": 1,
                    "alias": "rust-lang"
                }
            }
        })
        .to_string();
        let alias_entity_response = process_mcp_request(&mut state, &alias_entity_request).await;
        assert!(alias_entity_response.contains("rust-lang"));

        let add_claim_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 23,
            "method": "tools/call",
            "params": {
                "name": "add_claim",
                "arguments": {
                    "entity_id": 1,
                    "predicate": 7,
                    "object": 2026,
                    "object_tag": "U64",
                    "ctx_id": 0
                }
            }
        })
        .to_string();
        let add_claim_response = process_mcp_request(&mut state, &add_claim_request).await;
        assert!(add_claim_response.contains("Added entity claim"));
        assert!(add_claim_response.contains("Atom ID:"));

        let split_entity_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 24,
            "method": "tools/call",
            "params": {
                "name": "split_entity",
                "arguments": {
                    "source_entity": 1,
                    "canonical_name": "Rust ownership",
                    "entity_type": "topic"
                }
            }
        })
        .to_string();
        let split_entity_response = process_mcp_request(&mut state, &split_entity_request).await;
        assert!(split_entity_response.contains("Split entity"));
        assert!(split_entity_response.contains("New Entity ID: 3"));

        let merge_entities_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 25,
            "method": "tools/call",
            "params": {
                "name": "merge_entities",
                "arguments": {
                    "target_entity": 1,
                    "source_entity": 3
                }
            }
        })
        .to_string();
        let merge_entities_response =
            process_mcp_request(&mut state, &merge_entities_request).await;
        assert!(merge_entities_response.contains("Merged entities"));
        assert!(merge_entities_response.contains("Merged from"));

        let assert_relation_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 19,
            "method": "tools/call",
            "params": {
                "name": "assert_relation",
                "arguments": {
                    "subject": 1,
                    "predicate": 42,
                    "object": 2,
                    "ctx_id": 0
                }
            }
        })
        .to_string();
        let assert_relation_response =
            process_mcp_request(&mut state, &assert_relation_request).await;
        assert!(assert_relation_response.contains("Asserted relation"));
        assert!(assert_relation_response.contains("Relation ID: 1"));

        let search_graph_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": {
                "name": "search_graph",
                "arguments": {
                    "pattern": "* -> DEPENDS_ON -> 99",
                    "limit": 10
                }
            }
        })
        .to_string();
        let search_graph_response = process_mcp_request(&mut state, &search_graph_request).await;
        assert!(search_graph_response.contains("match_count=1"));
        assert!(search_graph_response.contains(&format!("{} --DEPENDS_ON--> 99", atom_node)));

        let neighbors_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "tools/call",
            "params": {
                "name": "graph_neighbors",
                "arguments": {
                    "node_num": 99,
                    "edge_types": ["DEPENDS_ON"]
                }
            }
        })
        .to_string();
        let neighbors_response = process_mcp_request(&mut state, &neighbors_request).await;
        assert!(neighbors_response.contains("incoming"));
        assert!(neighbors_response.contains(&atom_node.to_string()));
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn test_mcp_context_tools_are_store_backed() {
        let dir = tempdir().unwrap();
        let mut state = test_mcp_state(dir.path().join("memoryx"));

        let create_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "tools/call",
            "params": {
                "name": "create_context",
                "arguments": {"policy_id": 3}
            }
        })
        .to_string();
        let create_response = process_mcp_request(&mut state, &create_request).await;
        assert!(create_response.contains("created_ctx="));

        let branch_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "tools/call",
            "params": {
                "name": "branch_context",
                "arguments": {
                    "parent_ctx": 0,
                    "reason": "Hypothesis",
                    "policy_id": 9
                }
            }
        })
        .to_string();
        let branch_response = process_mcp_request(&mut state, &branch_request).await;
        assert!(branch_response.contains("Created branch context"));
        assert!(branch_response.contains("Reason: Hypothesis"));

        let list_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 22,
            "method": "tools/call",
            "params": {
                "name": "list_contexts",
                "arguments": {}
            }
        })
        .to_string();
        let list_response = process_mcp_request(&mut state, &list_request).await;
        assert!(list_response.contains("Total: 2"));
        assert!(list_response.contains("Branch reason: hypothesis"));
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn test_mcp_update_delete_and_extract_subgraph_are_store_backed() {
        let dir = tempdir().unwrap();
        let mut state = test_mcp_state(dir.path().join("memoryx"));

        let (atom_id, atom_node) = {
            let claims = vec![ClaimData {
                subj: 11,
                pred: 22,
                obj_tag: ObjTag::U64.to_u8(),
                obj_val: 33,
                qualifiers_mask: 0,
            }];
            let payload = create_minimal_atom_body(AtomType::FACT, &claims);
            let store = test_mcp_active_store_mut(&mut state);
            let atom_id = store
                .ingest(&payload, AtomType::FACT, &claims, &[])
                .unwrap();
            let atom_node = store.get_node_num(&atom_id).unwrap();
            (atom_id, atom_node)
        };

        let extract_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 30,
            "method": "tools/call",
            "params": {
                "name": "extract_subgraph",
                "arguments": {
                    "center_node": atom_node,
                    "radius": 1,
                    "edge_types": ["DEPENDS_ON"]
                }
            }
        })
        .to_string();
        let extract_response = process_mcp_request(&mut state, &extract_request).await;
        assert!(extract_response.contains("Nodes: 2"));
        assert!(extract_response.contains("Edges: 1"));

        let update_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 31,
            "method": "tools/call",
            "params": {
                "name": "update_atom",
                "arguments": {
                    "atom_id": atom_id_to_hex(&atom_id),
                    "atom_type": "FACT",
                    "claims": [{
                        "subj": 44,
                        "pred": 55,
                        "obj_tag": ObjTag::U64.to_u8(),
                        "obj_val": 66
                    }]
                }
            }
        })
        .to_string();
        let update_response = process_mcp_request(&mut state, &update_request).await;
        assert!(update_response.contains("Successfully updated atom"));
        assert!(update_response.contains("Supersedes"));

        let delete_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 32,
            "method": "tools/call",
            "params": {
                "name": "delete_atom",
                "arguments": {
                    "atom_id": atom_id_to_hex(&atom_id),
                    "reason": "Obsolete"
                }
            }
        })
        .to_string();
        let delete_response = process_mcp_request(&mut state, &delete_request).await;
        assert!(delete_response.contains("Successfully deleted atom"));
        assert!(delete_response.contains("Tombstone ID"));

        let history_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 33,
            "method": "tools/call",
            "params": {
                "name": "history",
                "arguments": {"limit": 2}
            }
        })
        .to_string();
        let history_response = process_mcp_request(&mut state, &history_request).await;
        assert!(history_response.contains("Operation history"));
        assert!(history_response.contains("DeleteAtom"));
        assert!(history_response.contains("UpdateAtom"));
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn bounded_mcp_reader_rejects_oversize_and_recovers_next_line() {
        let mut input = vec![b'x'; MAX_MCP_REQUEST_LINE_BYTES + 1];
        input.extend_from_slice(b"\n{}\n");
        let mut reader = tokio::io::BufReader::new(input.as_slice());
        assert!(matches!(
            read_bounded_mcp_line(&mut reader).await.unwrap(),
            McpInputLine::TooLarge
        ));
        match read_bounded_mcp_line(&mut reader).await.unwrap() {
            McpInputLine::Line(line) => assert_eq!(line, b"{}"),
            _ => panic!("reader did not recover after oversized request"),
        }
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn query_tool_schema_examples_deserialize_and_execute() {
        let dir = tempdir().unwrap();
        let mut state = test_mcp_state(dir.path().join("schema-examples"));
        let listed = process_mcp_request(
            &mut state,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list",
                "params": {}
            })
            .to_string(),
        )
        .await;
        let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
        let tools = listed["result"]["tools"].as_array().unwrap();
        for name in ["query", "validate_query_contract", "explain_answer_graph"] {
            let tool = tools
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap_or_else(|| panic!("missing tool {name}"));
            let examples = tool["inputSchema"]["examples"].as_array().unwrap();
            let contract_schema = &tool["inputSchema"]["properties"]["contract"];
            assert!(
                contract_schema["properties"]["targets"]["items"]["properties"]["aliases"]
                    .is_object()
            );
            assert!(contract_schema["properties"]["relations"]["items"]["required"].is_array());
            assert!(contract_schema["properties"]["constraints"]["items"]["required"].is_array());
            assert!(
                contract_schema["properties"]["budgets"]["properties"]["max_atoms"]["maximum"]
                    .is_number()
            );
            for (index, example) in examples.iter().enumerate() {
                assert_schema_accepts(&tool["inputSchema"], example);
                if let Some(contract) = example.get("contract") {
                    let parsed: memoryx::query::QueryContract =
                        serde_json::from_value(contract.clone()).unwrap();
                    parsed.validate().unwrap();
                }
                let response = process_mcp_request(
                    &mut state,
                    &serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 100 + index,
                        "method": "tools/call",
                        "params": {"name": name, "arguments": example}
                    })
                    .to_string(),
                )
                .await;
                let response: serde_json::Value = serde_json::from_str(&response).unwrap();
                assert!(
                    response.get("error").is_none(),
                    "{name} example failed: {response}"
                );
            }
        }
    }
}
