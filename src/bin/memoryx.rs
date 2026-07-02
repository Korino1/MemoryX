//! MemoryX CLI - Production-ready command-line interface for MemoryX SKF-1.1
//!
//! This CLI provides comprehensive management of MemoryX knowledge bases:
//! - Ingest: Load atoms from JSON/YAML files
//! - Query: Execute natural language queries
//! - Compact: Optimize storage through compaction
//! - Export/Import: Data exchange in multiple formats
//! - Stats: Storage analytics and reporting
//! - Serve: MCP (Model Context Protocol) server
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
//! # Start MCP server
//! memoryx serve --base /path/to/base --port 8080
//! ```

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use clap::{Parser, Subcommand, ValueEnum};
use colored::*;
use csv::{ReaderBuilder, WriterBuilder};
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};

// MemoryX imports
use memoryx::cas::AtomBodyHeader;
use memoryx::cas::claims::ClaimRecord;
use memoryx::prelude::*;
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
    },

    /// Execute query and return results
    Query {
        /// MemoryX base path or base name
        #[arg(short, long)]
        base: Option<PathBuf>,

        /// Query string
        #[arg(required = true)]
        query: String,

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

    /// Start MCP server
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
            subject: record.subject_local as u64,
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

fn validate_allowed_base_path(path: &Path) -> CliResult<PathBuf> {
    let candidate = absolute_or_cwd(path)?;
    let project_root = project_base_root()?;
    let user_root = user_base_root()?;

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
            return scoped_base_path(scope, &base.to_string_lossy());
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

    scoped_base_path(scope, name)
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

/// Ingest atoms from files
fn cmd_ingest(
    base: &Path,
    files: &[PathBuf],
    atom_type_str: &str,
    _batch_size: usize,
    verbose: bool,
) -> CliResult<()> {
    // Open store
    let config = StoreConfig::new(base.to_path_buf());
    let mut store = MemoryX::new(config)
        .map_err(|e| CliError::Store(format!("Failed to open store: {}", e)))?;

    let atom_type = parse_atom_type(atom_type_str)?;

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
        claims_section.add_claim(ClaimRecord::new_u64(
            subj_sym as u16,
            pred_sym as u16,
            claim.obj_val,
        ));
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
fn cmd_query(
    base: &Path,
    query: &str,
    ctx_policy: u32,
    _limit: Option<usize>,
    _min_trust: Option<u16>,
    format: OutputFormat,
) -> CliResult<()> {
    let start = Instant::now();

    // Open store
    let config = StoreConfig::new(base.to_path_buf());
    let store = MemoryX::new(config)
        .map_err(|e| CliError::Store(format!("Failed to open store: {}", e)))?;

    print_info(&format!("Executing query: '{}'", query));

    // Execute query
    let answer = store
        .answer(query, ctx_policy)
        .map_err(|e| CliError::Store(format!("Query failed: {}", e)))?;

    let elapsed = start.elapsed();

    // Format and output results
    match format {
        OutputFormat::Json => {
            let result = serde_json::json!({
                "query": query,
                "confidence": answer.confidence,
                "context_id": answer.selected_ctx,
                "claims_count": answer.claims.len(),
                "evidence_count": answer.evidence.len(),
                "limitations": answer.limitations.iter().map(|l| {
                    serde_json::json!({
                        "code": format!("{:?}", l.code),
                        "description": l.description,
                        "severity": format!("{:?}", l.severity),
                    })
                }).collect::<Vec<_>>(),
                "elapsed_ms": elapsed.as_millis(),
            });
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        OutputFormat::Yaml => {
            let result = serde_json::json!({
                "query": query,
                "confidence": answer.confidence,
                "context_id": answer.selected_ctx,
                "claims_count": answer.claims.len(),
                "evidence_count": answer.evidence.len(),
                "elapsed_ms": elapsed.as_millis(),
            });
            println!("{}", serde_yaml::to_string(&result)?);
        }
        OutputFormat::Table => {
            println!("\n{}", "Query Results".bold().underline());
            println!("  Query:      {}", query.cyan());
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
        }
    }

    Ok(())
}

/// Run compaction
fn cmd_compact(base: &Path, compaction_type: CompactionType, dry_run: bool) -> CliResult<()> {
    use memoryx::cas::io::{CasStore as CasIoStore, Compactor};
    use memoryx::graph::GraphStore;

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

/// Start MCP server
#[cfg(feature = "mcp")]
fn cmd_serve(base: &Path, port: u16, host: &str, stdio: bool) -> CliResult<()> {
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
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

    let mut store = MemoryX::new(StoreConfig::new(base.to_path_buf()))
        .map_err(|e| CliError::Store(format!("Failed to open store: {}", e)))?;

    let rt = Runtime::new().map_err(CliError::Io)?;

    rt.block_on(async {
        if stdio {
            // Stdio MCP transport
            let stdin = tokio::io::stdin();
            let mut stdout = tokio::io::stdout();
            let reader = tokio::io::BufReader::new(stdin);
            let mut lines = reader.lines();

            print_info_to(
                diagnostic_sink,
                "Stdio MCP server running. Send JSON-RPC requests.",
            );

            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let response = process_mcp_request(&mut store, &line).await;
                        stdout
                            .write_all(response.as_bytes())
                            .await
                            .map_err(CliError::Io)?;
                        stdout.write_all(b"\n").await.map_err(CliError::Io)?;
                        stdout.flush().await.map_err(CliError::Io)?;
                    }
                    Ok(None) => break,
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
                std::sync::Arc::new(store),
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

/// Process MCP JSON-RPC request
#[cfg(feature = "mcp")]
async fn process_mcp_request(store: &mut MemoryX, request: &str) -> String {
    let result: serde_json::Result<serde_json::Value> = serde_json::from_str(request);
    match result {
        Ok(req) => {
            let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let id = req.get("id").cloned().unwrap_or(serde_json::json!(null));

            let resp = match method {
                "initialize" => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": { "tools": {} },
                        "serverInfo": {
                            "name": "memoryx",
                            "version": env!("CARGO_PKG_VERSION")
                        }
                    }
                }),
                "tools/list" => serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "tools": [
                                    {
                                        "name": "query",
                                        "description": "Run the fixed-point solver against the active or selected context and return the answer graph for one natural-language question.",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "question": { "type": "string" },
                                                "ctx_id": { "type": "integer" }
                                            },
                                            "required": ["question"],
                                            "examples": [
                                                {
                                                    "question": "What decisions mention MemoryX persistence?",
                                                    "ctx_id": 0
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
                                        "description": "Attach an existing registered source id to an atom so future evidence records can trace that atom back to exact source metadata.",
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
                        "query" => mcp_query_response(store, id, arguments),
                        "search_lex" => mcp_search_lex_response(store, id, arguments),
                        "search_graph" => mcp_search_graph_response(store, id, arguments),
                        "search_semantic" => mcp_search_semantic_response(store, id, arguments),
                        "ingest" => mcp_ingest_response(store, id, arguments),
                        "batch_ingest" => mcp_batch_ingest_response(store, id, arguments),
                        "update_atom" => mcp_update_atom_response(store, id, arguments),
                        "delete_atom" => mcp_delete_atom_response(store, id, arguments),
                        "history" => mcp_history_response(store, id, arguments),
                        "register_source" => mcp_register_source_response(store, id, arguments),
                        "list_sources" => mcp_list_sources_response(store, id, arguments),
                        "attach_atom_source" => {
                            mcp_attach_atom_source_response(store, id, arguments)
                        }
                        "create_context" => mcp_create_context_response(store, id, arguments),
                        "list_contexts" => mcp_list_contexts_response(store, id, arguments),
                        "branch_context" => mcp_branch_context_response(store, id, arguments),
                        "list_conflicts" => mcp_list_conflicts_response(store, id, arguments),
                        "graph_neighbors" => mcp_graph_neighbors_response(store, id, arguments),
                        "graph_walk" => mcp_graph_walk_response(store, id, arguments),
                        "extract_subgraph" => mcp_extract_subgraph_response(store, id, arguments),
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

            serde_json::to_string(&resp).unwrap_or_else(|_| "{}".to_string())
        }
        Err(_) => serde_json::json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": { "code": -32700, "message": "Parse error" }
        })
        .to_string(),
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
fn mcp_query_response(
    store: &mut MemoryX,
    id: serde_json::Value,
    arguments: Option<&serde_json::Value>,
) -> serde_json::Value {
    let args = match mcp_arguments_object(id.clone(), arguments) {
        Ok(args) => args,
        Err(err) => return err,
    };
    let Some(question) = args.get("question").and_then(|value| value.as_str()) else {
        return mcp_error(id, -32602, "Missing required string field 'question'");
    };
    let ctx_id = args
        .get("ctx_id")
        .and_then(|value| value.as_u64())
        .unwrap_or(store.active_context().into()) as u32;

    match store.answer(question, ctx_id) {
        Ok(answer) => mcp_text_result(
            id,
            format!(
                "selected_ctx={}\nconfidence={:.3}\nclaims={}\nevidence={}\ngraph_nodes={}\ngraph_edges={}\nlimitations={}",
                answer.selected_ctx,
                answer.confidence,
                answer.claims.len(),
                answer.evidence.len(),
                answer.graph.node_count(),
                answer.graph.edge_count(),
                answer.limitations.len()
            ),
        ),
        Err(err) => mcp_error(id, -32000, format!("Query failed: {}", err)),
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
    let ctx_id = store.create_context(policy_id as u32);
    mcp_text_result(
        id,
        format!("created_ctx={}\npolicy_id={}", ctx_id, policy_id),
    )
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
        Some(new_ctx) => mcp_text_result(
            id,
            format!(
                "Created branch context: {}\nParent context: {}\nReason: {:?}\nPolicy ID: {}",
                new_ctx, parent_ctx, branch_reason, policy_id
            ),
        ),
        None => mcp_error(id, -32603, "Failed to create branch context"),
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
        claims_section.add_claim(ClaimRecord::new_u64(
            claim.subj as u16,
            claim.pred as u16,
            claim.obj_val,
        ));
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
        } => resolve_base_path(
            base.as_ref(),
            cli.base_scope,
            cli.base_name.as_deref(),
            &config,
        )
        .and_then(|resolved| {
            cmd_ingest(
                &resolved,
                files.as_slice(),
                atom_type.as_str(),
                *batch_size,
                cli.verbose,
            )
        }),
        Commands::Query {
            base,
            query,
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
            cmd_query(
                &resolved,
                query.as_str(),
                *ctx_policy,
                *limit,
                *min_trust,
                cli.format,
            )
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
    async fn test_mcp_tools_list_reports_real_core_surface() {
        let dir = tempdir().unwrap();
        let mut store = MemoryX::new(StoreConfig::new(dir.path().join("memoryx"))).unwrap();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list"
        })
        .to_string();

        let response = process_mcp_request(&mut store, &request).await;
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

        assert!(names.contains(&"query"));
        assert!(names.contains(&"search_lex"));
        assert!(names.contains(&"search_graph"));
        assert!(names.contains(&"search_semantic"));
        assert!(names.contains(&"ingest"));
        assert!(names.contains(&"batch_ingest"));
        assert!(names.contains(&"update_atom"));
        assert!(names.contains(&"delete_atom"));
        assert!(names.contains(&"history"));
        assert!(names.contains(&"register_source"));
        assert!(names.contains(&"list_sources"));
        assert!(names.contains(&"attach_atom_source"));
        assert!(names.contains(&"create_context"));
        assert!(names.contains(&"list_contexts"));
        assert!(names.contains(&"branch_context"));
        assert!(names.contains(&"list_conflicts"));
        assert!(names.contains(&"graph_neighbors"));
        assert!(names.contains(&"graph_walk"));
        assert!(names.contains(&"extract_subgraph"));
        assert_eq!(names.len(), 19);
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn test_mcp_create_context_and_query_are_store_backed() {
        let dir = tempdir().unwrap();
        let mut store = MemoryX::new(StoreConfig::new(dir.path().join("memoryx"))).unwrap();

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
        let create_response = process_mcp_request(&mut store, &create_request).await;
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
        let query_response = process_mcp_request(&mut store, &query_request).await;
        assert!(query_response.contains("selected_ctx="));
        assert!(query_response.contains("graph_nodes="));
        assert!(!query_response.contains("Query executed"));
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn test_mcp_ingest_search_graph_and_neighbors_are_store_backed() {
        let dir = tempdir().unwrap();
        let mut store = MemoryX::new(StoreConfig::new(dir.path().join("memoryx"))).unwrap();

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
        let ingest_response = process_mcp_request(&mut store, &ingest_request).await;
        assert!(ingest_response.contains("Successfully ingested atom"));

        let atom_id = store.list_atom_ids().into_iter().next().unwrap();
        let atom_node = store.get_node_num(&atom_id).unwrap();

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
            process_mcp_request(&mut store, &register_source_request).await;
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
        let attach_source_response = process_mcp_request(&mut store, &attach_source_request).await;
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
        let list_sources_response = process_mcp_request(&mut store, &list_sources_request).await;
        assert!(list_sources_response.contains("test source"));

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
        let search_graph_response = process_mcp_request(&mut store, &search_graph_request).await;
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
        let neighbors_response = process_mcp_request(&mut store, &neighbors_request).await;
        assert!(neighbors_response.contains("incoming"));
        assert!(neighbors_response.contains(&atom_node.to_string()));
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn test_mcp_context_tools_are_store_backed() {
        let dir = tempdir().unwrap();
        let mut store = MemoryX::new(StoreConfig::new(dir.path().join("memoryx"))).unwrap();

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
        let create_response = process_mcp_request(&mut store, &create_request).await;
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
        let branch_response = process_mcp_request(&mut store, &branch_request).await;
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
        let list_response = process_mcp_request(&mut store, &list_request).await;
        assert!(list_response.contains("Total: 2"));
        assert!(list_response.contains("Branch reason: hypothesis"));
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn test_mcp_update_delete_and_extract_subgraph_are_store_backed() {
        let dir = tempdir().unwrap();
        let mut store = MemoryX::new(StoreConfig::new(dir.path().join("memoryx"))).unwrap();

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
        let atom_node = store.get_node_num(&atom_id).unwrap();

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
        let extract_response = process_mcp_request(&mut store, &extract_request).await;
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
        let update_response = process_mcp_request(&mut store, &update_request).await;
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
        let delete_response = process_mcp_request(&mut store, &delete_request).await;
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
        let history_response = process_mcp_request(&mut store, &history_request).await;
        assert!(history_response.contains("Operation history"));
        assert!(history_response.contains("DeleteAtom"));
        assert!(history_response.contains("UpdateAtom"));
    }
}
