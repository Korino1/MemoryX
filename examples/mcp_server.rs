//! MemoryX MCP (Model Context Protocol) Server
//!
//! This module implements a complete MCP server for MemoryX, exposing
//! a hybrid API:
//! - **MCP Tools**: Limited API for AI assistants (10 tools)
//! - **Native API**: Full access for Rust code via direct MemoryX store
//!
//! # Architecture
//!
//! The server uses a hybrid architecture:
//! 1. MCP layer: JSON-RPC based tool exposure for AI assistants
//! 2. Native layer: Direct MemoryX API access with mmap I/O, batch operations
//! 3. Streaming: Async streaming answers for large queries
//!
//! # MCP Tools
//!
//! | Tool | Description |
//! |------|-------------|
//! | `query` | Query knowledge base with natural language |
//! | `ingest_text` | Ingest text content as knowledge atom |
//! | `get_atom` | Retrieve atom by ID |
//! | `search_lex` | Lexical search by terms |
//! | `graph_walk` | Graph traversal from seed nodes |
//! | `create_context` | Create new context branch |
//! | `list_contexts` | List all contexts |
//! | `get_provenance` | Get evidence/provenance for atom |
//! | `verify_atom` | Verify atom integrity |
//! | `list_conflicts` | List conflicts in context |
//!
//! # Usage
//!
//! ```bash
//! # Run MCP server
//! cargo run --example mcp_server --features mcp
//!
//! # Or with explicit project-scoped base
//! cargo run --example mcp_server --features mcp -- --base-scope project --base-name default
//! ```
//!
//! # Security Notes
//!
//! - MCP tools expose limited, sanitized API
//! - Native API requires direct Rust integration
//! - All inputs are validated before processing
//! - Confidence scores and limitations included in responses

#![cfg_attr(not(feature = "mcp"), allow(dead_code, unused_imports))]

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[cfg(feature = "mcp")]
use tokio::sync::RwLock;

// MemoryX imports
use memoryx::cas::{
    AtomBodyHeader, SectionDesc,
    claims::ClaimsSection,
    evidence::EvidenceSection,
    invariants::InvariantsSection,
    meta::{MetaField, MetaFieldKind, MetaSection, MetaValue},
    symbols::SymbolsSection,
};
use memoryx::prelude::*;
use memoryx::store::api::{CtxId, CtxPolicyId, MemoryX, StoreConfig, StoreError};

fn project_base_root() -> Result<PathBuf, String> {
    std::env::current_dir()
        .map(|p| p.join(".memoryx").join("bases"))
        .map_err(|e| format!("Failed to determine current directory: {}", e))
}

fn user_base_root() -> Result<PathBuf, String> {
    dirs::home_dir()
        .map(|p| p.join(".memoryx").join("bases"))
        .ok_or_else(|| "Failed to determine home directory".to_string())
}

fn scoped_base_path(scope: &str, base_name: &str) -> Result<PathBuf, String> {
    let root = match scope {
        "project" => project_base_root()?,
        "user" => user_base_root()?,
        other => {
            return Err(format!(
                "Invalid --base-scope '{}'. Expected 'project' or 'user'",
                other
            ));
        }
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

fn validate_allowed_data_dir(path: &Path) -> Result<PathBuf, String> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("Failed to determine current directory: {}", e))?
            .join(path)
    };

    let project_root = project_base_root()?;
    let user_root = user_base_root()?;

    if candidate.starts_with(&project_root) || candidate.starts_with(&user_root) {
        Ok(candidate)
    } else {
        Err(format!(
            "Data directory '{}' must be inside '{}' or '{}'",
            candidate.display(),
            project_root.display(),
            user_root.display()
        ))
    }
}

fn resolve_data_dir(
    data_dir_arg: Option<PathBuf>,
    base_scope: &str,
    base_name: Option<String>,
) -> Result<PathBuf, String> {
    if let Some(path) = data_dir_arg {
        if is_simple_base_name(&path) {
            return scoped_base_path(base_scope, &path.to_string_lossy());
        }
        return validate_allowed_data_dir(&path);
    }

    scoped_base_path(base_scope, base_name.as_deref().unwrap_or("default"))
}

// ============================================================================
// MCP Protocol Types
// ============================================================================

/// JSON-RPC 2.0 message ID
type RpcId = serde_json::Value;

/// JSON-RPC 2.0 request
#[derive(Debug, Clone, serde::Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    method: String,
    #[serde(default)]
    id: Option<RpcId>,
    #[serde(default)]
    params: serde_json::Value,
}

/// JSON-RPC 2.0 response
#[derive(Debug, Clone, serde::Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Option<RpcId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

/// JSON-RPC error
#[derive(Debug, Clone, serde::Serialize)]
struct RpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
}

impl RpcError {
    fn parse_error(message: String) -> Self {
        RpcError {
            code: -32700,
            message,
            data: None,
        }
    }

    #[allow(dead_code)]
    fn invalid_request(message: String) -> Self {
        RpcError {
            code: -32600,
            message,
            data: None,
        }
    }

    fn method_not_found(message: String) -> Self {
        RpcError {
            code: -32601,
            message,
            data: None,
        }
    }

    fn invalid_params(message: String) -> Self {
        RpcError {
            code: -32602,
            message,
            data: None,
        }
    }

    #[allow(dead_code)]
    fn internal_error(message: String) -> Self {
        RpcError {
            code: -32603,
            message,
            data: None,
        }
    }

    fn tool_error(code: i32, message: String, data: Option<serde_json::Value>) -> Self {
        RpcError {
            code,
            message,
            data,
        }
    }
}

// ============================================================================
// MCP Tool Definitions
// ============================================================================

/// Tool input parameter schema
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ToolInputSchema {
    #[serde(rename = "type")]
    schema_type: String,
    #[serde(default)]
    properties: HashMap<String, ToolProperty>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    required: Vec<String>,
}

/// Tool property definition
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ToolProperty {
    #[serde(rename = "type")]
    prop_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default: Option<serde_json::Value>,
}

/// MCP Tool definition
#[derive(Debug, Clone, serde::Serialize)]
struct Tool {
    name: String,
    description: String,
    input_schema: ToolInputSchema,
}

/// Tool result with content and metadata
#[derive(Debug, Clone, serde::Serialize)]
struct ToolResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<Vec<ToolContent>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Confidence score (0.0 - 1.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<f32>,
    /// Known limitations
    #[serde(skip_serializing_if = "Vec::is_empty")]
    limitations: Vec<String>,
    /// Alternative answers available
    #[serde(skip_serializing_if = "Option::is_none")]
    alternates_count: Option<usize>,
}

/// Tool content block
#[derive(Debug, Clone, serde::Serialize)]
struct ToolContent {
    #[serde(rename = "type")]
    content_type: String,
    text: String,
}

impl ToolContent {
    fn text(text: String) -> Self {
        ToolContent {
            content_type: "text".to_string(),
            text,
        }
    }
}

// ============================================================================
// MemoryX MCP Server
// ============================================================================

/// MemoryX MCP Server with hybrid architecture
///
/// Provides:
/// - MCP Tools: Limited API for AI assistants
/// - Native API: Direct MemoryX access for Rust code
/// - Streaming: Async streaming for large queries
struct MemoryXServer {
    /// Core MemoryX store
    store: Arc<RwLock<MemoryX>>,
    /// Active context ID
    active_ctx: CtxId,
    /// Data directory
    #[allow(dead_code)]
    data_dir: PathBuf,
    /// Server configuration
    config: ServerConfig,
}

/// Server configuration
#[derive(Debug, Clone)]
struct ServerConfig {
    /// Enable mmap I/O
    mmap_mode: bool,
    /// Enable batch operations
    #[allow(dead_code)]
    batch_enabled: bool,
    /// Batch size
    #[allow(dead_code)]
    batch_size: usize,
    /// Streaming chunk size
    #[allow(dead_code)]
    stream_chunk_size: usize,
    /// Maximum query depth
    max_query_depth: u8,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            mmap_mode: true,
            batch_enabled: true,
            batch_size: 1024,
            stream_chunk_size: 4096,
            max_query_depth: 10,
        }
    }
}

/// Batch ingest item payload used by the native API.
type BatchIngestItem = (Vec<u8>, AtomType, Vec<ClaimData>, Vec<EvidenceRef>);

/// Native API for direct Rust access (full capabilities)
#[allow(dead_code)]
struct NativeApi {
    store: Arc<RwLock<MemoryX>>,
}

#[allow(dead_code)]
impl NativeApi {
    /// Get direct store access
    async fn store(&self) -> tokio::sync::RwLockReadGuard<'_, MemoryX> {
        self.store.read().await
    }

    /// Get mutable store access
    async fn store_mut(&self) -> tokio::sync::RwLockWriteGuard<'_, MemoryX> {
        self.store.write().await
    }

    /// Batch ingest multiple atoms
    async fn batch_ingest(&self, items: Vec<BatchIngestItem>) -> Result<Vec<AtomId>, StoreError> {
        let mut store = self.store.write().await;
        let mut results = Vec::with_capacity(items.len());

        for (payload, atom_type, claims, evidence) in items {
            let atom_id = store.ingest(&payload, atom_type, &claims, &evidence)?;
            results.push(atom_id);
        }

        Ok(results)
    }

    /// Stream query results
    async fn stream_query(
        &self,
        query: &str,
        ctx_policy: CtxPolicyId,
        _chunk_size: usize,
    ) -> Result<AnswerPack, StoreError> {
        let store = self.store.read().await;
        store.answer(query, ctx_policy)
    }

    /// Get raw CAS record bytes for atom data.
    ///
    /// Returns the exact record from the existing store/CAS path:
    /// header + body + body CRC, without fabricating any synthetic view.
    async fn get_atom_mmap(&self, atom_id: &AtomId) -> Result<Vec<u8>, StoreError> {
        let store = self.store.read().await;
        store.read_raw_record(atom_id)
    }
}
impl MemoryXServer {
    /// Create a new MemoryX MCP server
    fn new(data_dir: PathBuf, config: ServerConfig) -> Result<Self, String> {
        let store_config = StoreConfig::new(data_dir.clone()).with_mmap_mode(config.mmap_mode);

        let store = MemoryX::new(store_config)
            .map_err(|e| format!("Failed to create MemoryX store: {}", e))?;

        let active_ctx = store.active_context();

        Ok(MemoryXServer {
            store: Arc::new(RwLock::new(store)),
            active_ctx,
            data_dir,
            config,
        })
    }

    /// Get native API for direct Rust access
    #[allow(dead_code)]
    fn native_api(&self) -> NativeApi {
        NativeApi {
            store: Arc::clone(&self.store),
        }
    }

    /// Define all MCP tools
    fn define_tools() -> Vec<Tool> {
        vec![
            Tool {
                name: "query".to_string(),
                description: "Query knowledge base with natural language. Returns answer with confidence score, evidence, and limitations.".to_string(),
                input_schema: ToolInputSchema {
                    schema_type: "object".to_string(),
                    properties: HashMap::from([
                        ("question".to_string(), ToolProperty {
                            prop_type: "string".to_string(),
                            description: Some("Natural language question to query".to_string()),
                            default: None,
                        }),
                        ("ctx_id".to_string(), ToolProperty {
                            prop_type: "integer".to_string(),
                            description: Some("Optional context ID (default: active context)".to_string()),
                            default: None,
                        }),
                    ]),
                    required: vec!["question".to_string()],
                },
            },
            Tool {
                name: "ingest_text".to_string(),
                description: "Ingest text content as a knowledge atom. Builds a real text atom body with symbols and empty claims.".to_string(),
                input_schema: ToolInputSchema {
                    schema_type: "object".to_string(),
                    properties: HashMap::from([
                        ("title".to_string(), ToolProperty {
                            prop_type: "string".to_string(),
                            description: Some("Title for the knowledge atom".to_string()),
                            default: None,
                        }),
                        ("content".to_string(), ToolProperty {
                            prop_type: "string".to_string(),
                            description: Some("Text content to ingest".to_string()),
                            default: None,
                        }),
                        ("atom_type".to_string(), ToolProperty {
                            prop_type: "string".to_string(),
                            description: Some("Type: FACT, DEFINITION, PROCEDURE, CONSTRAINT, OBSERVATION".to_string()),
                            default: Some(serde_json::json!("FACT")),
                        }),
                    ]),
                    required: vec!["title".to_string(), "content".to_string()],
                },
            },
            Tool {
                name: "get_atom".to_string(),
                description: "Retrieve atom by ID with full metadata and claims.".to_string(),
                input_schema: ToolInputSchema {
                    schema_type: "object".to_string(),
                    properties: HashMap::from([
                        ("atom_id".to_string(), ToolProperty {
                            prop_type: "string".to_string(),
                            description: Some("Atom ID (hex string or base64)".to_string()),
                            default: None,
                        }),
                    ]),
                    required: vec!["atom_id".to_string()],
                },
            },
            Tool {
                name: "search_lex".to_string(),
                description: "Lexical search by terms. Returns matching atoms with relevance scores.".to_string(),
                input_schema: ToolInputSchema {
                    schema_type: "object".to_string(),
                    properties: HashMap::from([
                        ("query".to_string(), ToolProperty {
                            prop_type: "string".to_string(),
                            description: Some("Search terms".to_string()),
                            default: None,
                        }),
                        ("limit".to_string(), ToolProperty {
                            prop_type: "integer".to_string(),
                            description: Some("Maximum results (default: 100)".to_string()),
                            default: Some(serde_json::json!(100)),
                        }),
                    ]),
                    required: vec!["query".to_string()],
                },
            },
            Tool {
                name: "graph_walk".to_string(),
                description: "Graph traversal from seed nodes. Returns subgraph with edges.".to_string(),
                input_schema: ToolInputSchema {
                    schema_type: "object".to_string(),
                    properties: HashMap::from([
                        ("seed_nodes".to_string(), ToolProperty {
                            prop_type: "array".to_string(),
                            description: Some("Starting node IDs".to_string()),
                            default: None,
                        }),
                        ("edge_types".to_string(), ToolProperty {
                            prop_type: "array".to_string(),
                            description: Some("Edge types to traverse".to_string()),
                            default: None,
                        }),
                        ("depth".to_string(), ToolProperty {
                            prop_type: "integer".to_string(),
                            description: Some("Maximum depth (default: 3)".to_string()),
                            default: Some(serde_json::json!(3)),
                        }),
                    ]),
                    required: vec!["seed_nodes".to_string()],
                },
            },
            Tool {
                name: "create_context".to_string(),
                description: "Create new context branch for hypothesis exploration.".to_string(),
                input_schema: ToolInputSchema {
                    schema_type: "object".to_string(),
                    properties: HashMap::from([
                        ("policy_id".to_string(), ToolProperty {
                            prop_type: "integer".to_string(),
                            description: Some("Context policy ID".to_string()),
                            default: Some(serde_json::json!(0)),
                        }),
                    ]),
                    required: vec!["policy_id".to_string()],
                },
            },
            Tool {
                name: "list_contexts".to_string(),
                description: "List all contexts with status and parent relationships.".to_string(),
                input_schema: ToolInputSchema {
                    schema_type: "object".to_string(),
                    properties: HashMap::new(),
                    required: vec![],
                },
            },
            Tool {
                name: "get_provenance".to_string(),
                description: "Get evidence and provenance chain for an atom.".to_string(),
                input_schema: ToolInputSchema {
                    schema_type: "object".to_string(),
                    properties: HashMap::from([
                        ("atom_id".to_string(), ToolProperty {
                            prop_type: "string".to_string(),
                            description: Some("Atom ID".to_string()),
                            default: None,
                        }),
                    ]),
                    required: vec!["atom_id".to_string()],
                },
            },
            Tool {
                name: "verify_atom".to_string(),
                description: "Verify atom integrity (CRC, magic, bounds checks).".to_string(),
                input_schema: ToolInputSchema {
                    schema_type: "object".to_string(),
                    properties: HashMap::from([
                        ("atom_id".to_string(), ToolProperty {
                            prop_type: "string".to_string(),
                            description: Some("Atom ID to verify".to_string()),
                            default: None,
                        }),
                    ]),
                    required: vec!["atom_id".to_string()],
                },
            },
            Tool {
                name: "list_conflicts".to_string(),
                description: "List conflicts in a context with severity and resolution options.".to_string(),
                input_schema: ToolInputSchema {
                    schema_type: "object".to_string(),
                    properties: HashMap::from([
                        ("ctx_id".to_string(), ToolProperty {
                            prop_type: "integer".to_string(),
                            description: Some("Context ID".to_string()),
                            default: None,
                        }),
                    ]),
                    required: vec!["ctx_id".to_string()],
                },
            },
        ]
    }

    // =========================================================================
    // MCP Tool Implementations
    // =========================================================================

    /// Query knowledge base with natural language
    async fn query(&self, question: String, ctx_id: Option<u64>) -> Result<ToolResult, String> {
        let store = self.store.read().await;
        let ctx = ctx_id.unwrap_or(self.active_ctx as u64) as CtxPolicyId;

        match store.answer(&question, ctx) {
            Ok(answer) => {
                let formatted = format_answer(&answer);
                let limitations: Vec<String> = answer
                    .limitations
                    .iter()
                    .map(|l| format!("{}: {}", l.code, l.description))
                    .collect();

                Ok(ToolResult {
                    content: Some(vec![ToolContent::text(formatted)]),
                    data: Some(serde_json::json!({
                        "confidence": answer.confidence,
                        "graph_nodes": answer.graph.node_count(),
                        "graph_edges": answer.graph.edge_count(),
                        "claims_count": answer.claims.len(),
                        "evidence_count": answer.evidence.len(),
                    })),
                    error: None,
                    confidence: Some(answer.confidence),
                    limitations,
                    alternates_count: Some(answer.alternates.len()),
                })
            }
            Err(e) => Err(format!("Query failed: {}", e)),
        }
    }

    /// Ingest text content as knowledge atom
    async fn ingest_text(
        &mut self,
        title: String,
        content: String,
        atom_type: String,
    ) -> Result<ToolResult, String> {
        // Parse atom type
        let parsed_type = parse_atom_type(&atom_type)
            .ok_or_else(|| format!("Invalid atom type: {}. Valid types: FACT, DEFINITION, PROCEDURE, CONSTRAINT, OBSERVATION", atom_type))?;

        // Build a real atom body from the source text without fabricating claims.
        let symbols = collect_text_symbols(&title, &content);
        let payload = build_text_atom_payload(parsed_type, &symbols, 5000, 0xFFFF)?;

        let mut store = self.store.write().await;
        match store.ingest(&payload, parsed_type, &[], &[]) {
            Ok(atom_id) => {
                let hex_id = hex_encode(&atom_id);
                Ok(ToolResult {
                    content: Some(vec![ToolContent::text(format!(
                        "Successfully ingested text atom '{}'
Atom ID: {}
Type: {}
Text symbols indexed: {}
Structured claims: 0",
                        title,
                        hex_id,
                        atom_type,
                        symbols.len()
                    ))]),
                    data: Some(serde_json::json!({
                        "atom_id": hex_id,
                        "atom_type": atom_type,
                        "symbols_count": symbols.len(),
                        "claims_count": 0,
                        "payload_size": payload.len(),
                    })),
                    error: None,
                    confidence: Some(1.0),
                    limitations: vec![],
                    alternates_count: None,
                })
            }
            Err(e) => Err(format!("Ingest failed: {}", e)),
        }
    }

    /// Retrieve atom by ID
    async fn get_atom(&self, atom_id_str: String) -> Result<ToolResult, String> {
        let atom_id = parse_atom_id(&atom_id_str).ok_or_else(|| {
            format!(
                "Invalid atom ID format: {}. Expected 32-byte hex string",
                atom_id_str
            )
        })?;

        let store = self.store.read().await;
        match store.get_atom(&atom_id) {
            Ok(view) => {
                let mut output = String::new();
                writeln!(output, "Atom: {}", atom_id_str).unwrap();
                writeln!(output, "Type: {:?}", view.atom_type).unwrap();
                writeln!(output, "Trust: {}/10000", view.trust_level).unwrap();
                writeln!(output, "Claims: {}", view.claims.len()).unwrap();

                writeln!(output, "Claims: {}", view.claims.len()).unwrap();

                for (i, claim) in view.claims.iter().enumerate() {
                    writeln!(output, "  [{}] {:?}", i, claim).unwrap();
                }

                Ok(ToolResult {
                    content: Some(vec![ToolContent::text(output)]),
                    data: Some(serde_json::json!({
                        "atom_id": atom_id_str,
                        "atom_type": format!("{:?}", view.atom_type),
                        "trust": view.trust_level,
                        "claims_count": view.claims.len(),
                        "evidence_count": 0,
                    })),
                    error: None,
                    confidence: Some(1.0),
                    limitations: vec![],
                    alternates_count: None,
                })
            }
            Err(e) => Err(format!("Atom not found: {}", e)),
        }
    }

    /// Lexical search
    async fn search_lex(&self, query: String, limit: Option<u32>) -> Result<ToolResult, String> {
        let limit = limit.unwrap_or(100);
        let store = self.store.read().await;

        let node_nums = store.search_lex(&query, None);
        let total_matches = node_nums.len();
        let limited: Vec<_> = node_nums.into_iter().take(limit as usize).collect();

        let mut output = String::new();
        writeln!(output, "Search results for '{}':", query).unwrap();
        writeln!(
            output,
            "Found {} matches (showing {})",
            total_matches,
            limited.len()
        )
        .unwrap();

        for (i, &node_num) in limited.iter().enumerate() {
            writeln!(output, "  [{}] Node: {}", i, node_num).unwrap();
        }

        Ok(ToolResult {
            content: Some(vec![ToolContent::text(output)]),
            data: Some(serde_json::json!({
                "query": query,
                "total_matches": total_matches,
                "returned": limited.len(),
                "nodes": limited,
            })),
            error: None,
            confidence: Some(1.0),
            limitations: vec![],
            alternates_count: None,
        })
    }

    /// Graph walk from seed nodes
    async fn graph_walk(
        &self,
        seed_nodes: Vec<u64>,
        edge_types: Vec<String>,
        depth: u8,
    ) -> Result<ToolResult, String> {
        let depth = if depth == 0 {
            3
        } else {
            depth.min(self.config.max_query_depth)
        };

        // Parse edge types
        let parsed_types: Vec<EdgeType> = edge_types
            .iter()
            .filter_map(|s| parse_edge_type(s))
            .collect();

        let edge_types = if parsed_types.is_empty() {
            // Default to common edge types
            vec![EdgeType::CAUSES, EdgeType::DEPENDS_ON, EdgeType::SUPPORTS]
        } else {
            parsed_types
        };

        let store = self.store.read().await;
        let edges = store.graph_walk(&seed_nodes, &edge_types, depth, None);

        let mut output = String::new();
        writeln!(output, "Graph walk from {} seed nodes:", seed_nodes.len()).unwrap();
        writeln!(
            output,
            "Edge types: {:?}",
            edge_types
                .iter()
                .map(|e| format!("{:?}", e))
                .collect::<Vec<_>>()
                .join(", ")
        )
        .unwrap();
        writeln!(output, "Depth: {}, Found edges: {}", depth, edges.len()).unwrap();

        for (i, (src, dst, etype)) in edges.iter().take(50).enumerate() {
            writeln!(output, "  [{}] {} --{:?}--> {}", i, src, etype, dst).unwrap();
        }

        if edges.len() > 50 {
            writeln!(output, "  ... and {} more", edges.len() - 50).unwrap();
        }

        Ok(ToolResult {
            content: Some(vec![ToolContent::text(output)]),
            data: Some(serde_json::json!({
                "seed_nodes": seed_nodes,
                "edge_types": edge_types.iter().map(|e| format!("{:?}", e)).collect::<Vec<_>>(),
                "depth": depth,
                "total_edges": edges.len(),
                "edges": edges.iter().take(100).map(|(s, d, t)| format!("{} -> {} ({:?})", s, d, t)).collect::<Vec<_>>(),
            })),
            error: None,
            confidence: Some(1.0),
            limitations: vec![],
            alternates_count: None,
        })
    }

    /// Create new context
    async fn create_context(&mut self, policy_id: u64) -> Result<ToolResult, String> {
        let mut store = self.store.write().await;
        let new_ctx = store.create_context(policy_id as CtxPolicyId);
        self.active_ctx = new_ctx;

        Ok(ToolResult {
            content: Some(vec![ToolContent::text(format!(
                "Created new context {}\nPolicy ID: {}\nThis is now the active context",
                new_ctx, policy_id
            ))]),
            data: Some(serde_json::json!({
                "ctx_id": new_ctx,
                "policy_id": policy_id,
                "active": true,
            })),
            error: None,
            confidence: Some(1.0),
            limitations: vec![],
            alternates_count: None,
        })
    }

    /// List all contexts
    async fn list_contexts(&self) -> Result<ToolResult, String> {
        let store = self.store.read().await;
        let active_ctx = store.active_context();
        let contexts = store.list_contexts();

        let mut output = String::from("Contexts:\n");
        for ctx in &contexts {
            let parent = ctx
                .parent_ctx
                .map(|id| id.to_string())
                .unwrap_or_else(|| "root".to_string());
            output.push_str(&format!(
                "  ctx={} active={} parent={} claims={} conflicts={} policy={}\n",
                ctx.ctx_id,
                ctx.active,
                parent,
                ctx.active_claims.len(),
                ctx.conflicts.len(),
                ctx.policy_id
            ));
        }

        Ok(ToolResult {
            content: Some(vec![ToolContent::text(output)]),
            data: Some(serde_json::json!({
                "active_ctx": active_ctx,
                "contexts": contexts.iter().map(|ctx| serde_json::json!({
                    "ctx_id": ctx.ctx_id,
                    "parent_ctx": ctx.parent_ctx,
                    "policy_id": ctx.policy_id,
                    "active": ctx.active,
                    "active_claim_count": ctx.active_claims.len(),
                    "conflict_count": ctx.conflicts.len(),
                })).collect::<Vec<_>>(),
            })),
            error: None,
            confidence: Some(1.0),
            limitations: vec![],
            alternates_count: None,
        })
    }

    /// Get provenance for atom
    async fn get_provenance(&self, atom_id_str: String) -> Result<ToolResult, String> {
        let atom_id = parse_atom_id(&atom_id_str)
            .ok_or_else(|| format!("Invalid atom ID format: {}", atom_id_str))?;

        let store = self.store.read().await;
        match store.get_provenance(&atom_id) {
            Ok(chain) => {
                let mut output = String::new();
                writeln!(output, "Provenance for atom {}:", atom_id_str).unwrap();
                writeln!(output, "Root atom: {:?}", chain.root_atom_id).unwrap();
                writeln!(output, "Nodes: {}", chain.nodes.len()).unwrap();
                writeln!(output, "Derivation edges: {}", chain.derivation_edges.len()).unwrap();
                writeln!(output, "Direct evidence: {}", chain.direct_evidence.len()).unwrap();
                writeln!(output, "Max depth: {}", chain.max_depth).unwrap();
                writeln!(
                    output,
                    "Overall confidence: {:.4}",
                    chain.overall_confidence
                )
                .unwrap();
                writeln!(output, "Overall trust: {}/10000", chain.overall_trust).unwrap();
                writeln!(output).unwrap();

                if !chain.nodes.is_empty() {
                    writeln!(output, "Nodes:").unwrap();
                    for (i, node) in chain.nodes.iter().take(10).enumerate() {
                        writeln!(
                            output,
                            "  [{}] Atom: {:?}, Node: {}, Type: {:?}, Depth: {}, Trust: {}/10000, Evidence links: {}",
                            i,
                            node.atom_id,
                            node.node_num,
                            node.atom_type,
                            node.depth,
                            node.cumulative_trust,
                            node.evidence_links.len()
                        )
                        .unwrap();
                    }
                    if chain.nodes.len() > 10 {
                        writeln!(output, "  ... and {} more nodes", chain.nodes.len() - 10)
                            .unwrap();
                    }
                    writeln!(output).unwrap();
                }

                if !chain.direct_evidence.is_empty() {
                    writeln!(output, "Direct evidence:").unwrap();
                    for (i, ev) in chain.direct_evidence.iter().take(10).enumerate() {
                        writeln!(
                            output,
                            "  [{}] Kind: {:?}, Section: {:?}, Offset: {}, Length: {}, Trust: {}/10000, Depth: {}, Source: {:?}",
                            i,
                            ev.evidence_kind,
                            ev.section_kind,
                            ev.offset,
                            ev.length,
                            ev.trust,
                            ev.derivation_depth,
                            ev.source_atom_id
                        )
                        .unwrap();
                    }
                    if chain.direct_evidence.len() > 10 {
                        writeln!(
                            output,
                            "  ... and {} more evidence links",
                            chain.direct_evidence.len() - 10
                        )
                        .unwrap();
                    }
                    writeln!(output).unwrap();
                }

                if !chain.derivation_edges.is_empty() {
                    writeln!(output, "Derivation edges:").unwrap();
                    for (i, edge) in chain.derivation_edges.iter().take(10).enumerate() {
                        writeln!(
                            output,
                            "  [{}] Derived: {:?}, Source: {:?}, Edge type: {}, Depth: {}, Confidence: {:.4}, Trust: {}/10000",
                            i,
                            edge.derived_atom_id,
                            edge.source_atom_id,
                            edge.edge_type,
                            edge.depth,
                            edge.confidence,
                            edge.propagated_trust
                        )
                        .unwrap();
                    }
                    if chain.derivation_edges.len() > 10 {
                        writeln!(
                            output,
                            "  ... and {} more derivation edges",
                            chain.derivation_edges.len() - 10
                        )
                        .unwrap();
                    }
                    writeln!(output).unwrap();
                }

                Ok(ToolResult {
                    content: Some(vec![ToolContent::text(output)]),
                    data: Some(serde_json::json!({
                        "atom_id": atom_id_str,
                        "root_atom_id": format!("{:?}", chain.root_atom_id),
                        "node_count": chain.nodes.len(),
                        "derivation_edge_count": chain.derivation_edges.len(),
                        "direct_evidence_count": chain.direct_evidence.len(),
                        "max_depth": chain.max_depth,
                        "overall_confidence": chain.overall_confidence,
                        "overall_trust": chain.overall_trust,
                        "nodes": chain.nodes.iter().map(|node| serde_json::json!({
                            "atom_id": format!("{:?}", node.atom_id),
                            "node_num": node.node_num,
                            "atom_type": format!("{:?}", node.atom_type),
                            "depth": node.depth,
                            "cumulative_trust": node.cumulative_trust,
                            "evidence_links_count": node.evidence_links.len(),
                        })).collect::<Vec<_>>(),
                        "direct_evidence": chain.direct_evidence.iter().map(|e| serde_json::json!({
                            "source_atom_id": format!("{:?}", e.source_atom_id),
                            "evidence_kind": format!("{:?}", e.evidence_kind),
                            "confidence": e.confidence,
                            "trust": e.trust,
                            "trust_decay_factor": e.trust_decay_factor,
                            "section_kind": format!("{:?}", e.section_kind),
                            "offset": e.offset,
                            "length": e.length,
                            "derivation_depth": e.derivation_depth,
                            "method_sym": e.method_sym,
                            "timestamp_ns": e.timestamp_ns,
                        })).collect::<Vec<_>>(),
                        "derivation_edges": chain.derivation_edges.iter().map(|edge| serde_json::json!({
                            "derived_atom_id": format!("{:?}", edge.derived_atom_id),
                            "source_atom_id": format!("{:?}", edge.source_atom_id),
                            "edge_type": edge.edge_type,
                            "depth": edge.depth,
                            "confidence": edge.confidence,
                            "propagated_trust": edge.propagated_trust,
                        })).collect::<Vec<_>>(),
                    })),
                    error: None,
                    confidence: Some(1.0),
                    limitations: vec![],
                    alternates_count: None,
                })
            }
            Err(e) => Err(format!("Failed to get provenance: {}", e)),
        }
    }
    /// Verify atom integrity
    async fn verify_atom(&self, atom_id_str: String) -> Result<ToolResult, String> {
        let atom_id = parse_atom_id(&atom_id_str)
            .ok_or_else(|| format!("Invalid atom ID format: {}", atom_id_str))?;

        let store = self.store.read().await;
        match store.verify_atom(&atom_id) {
            Ok(valid) => {
                let status = if valid { "VALID" } else { "INVALID/CORRUPTED" };
                let output = format!(
                    "Atom {}\nStatus: {}\nVerification: CRC, magic, bounds checks",
                    atom_id_str, status
                );

                Ok(ToolResult {
                    content: Some(vec![ToolContent::text(output)]),
                    data: Some(serde_json::json!({
                        "atom_id": atom_id_str,
                        "valid": valid,
                        "checks": ["CRC32", "Magic", "Bounds"],
                    })),
                    error: None,
                    confidence: Some(1.0),
                    limitations: vec![],
                    alternates_count: None,
                })
            }
            Err(e) => Err(format!("Verification failed: {}", e)),
        }
    }

    /// List conflicts in context
    async fn list_conflicts(&self, ctx_id: u64) -> Result<ToolResult, String> {
        let store = self.store.read().await;
        let conflicts = store.list_conflicts(ctx_id as CtxId);

        let mut output = String::new();
        writeln!(output, "Conflicts in context {}:", ctx_id).unwrap();
        writeln!(output, "Total conflicts: {}", conflicts.len()).unwrap();

        if conflicts.is_empty() {
            writeln!(output, "No conflicts detected").unwrap();
        } else {
            for (i, conflict) in conflicts.iter().enumerate() {
                writeln!(
                    output,
                    "  [{}] Type: {:?}, Severity: {:?}",
                    i, conflict.conflict_type, conflict.severity
                )
                .unwrap();
                writeln!(
                    output,
                    "      Atoms: {:?} vs {:?}",
                    hex_encode(&conflict.atom_a),
                    hex_encode(&conflict.atom_b)
                )
                .unwrap();
            }
        }

        Ok(ToolResult {
            content: Some(vec![ToolContent::text(output)]),
            data: Some(serde_json::json!({
                "ctx_id": ctx_id,
                "conflicts_count": conflicts.len(),
                "conflicts": conflicts.iter().map(|c| serde_json::json!({
                    "type": format!("{:?}", c.conflict_type),
                    "severity": format!("{:?}", c.severity),
                    "atom_a": hex_encode(&c.atom_a),
                    "atom_b": hex_encode(&c.atom_b),
                })).collect::<Vec<_>>(),
            })),
            error: None,
            confidence: Some(1.0),
            limitations: vec![],
            alternates_count: None,
        })
    }

    // =========================================================================
    // MCP Protocol Handler
    // =========================================================================

    /// Handle MCP tool call
    async fn handle_tool_call(
        &mut self,
        name: String,
        args: serde_json::Value,
    ) -> Result<ToolResult, String> {
        match name.as_str() {
            "query" => {
                let question: String = extract_arg(&args, "question")?;
                let ctx_id: Option<u64> = extract_arg_opt(&args, "ctx_id");
                self.query(question, ctx_id).await
            }
            "ingest_text" => {
                let title: String = extract_arg(&args, "title")?;
                let content: String = extract_arg(&args, "content")?;
                let atom_type: String =
                    extract_arg(&args, "atom_type").unwrap_or_else(|_| "FACT".to_string());
                self.ingest_text(title, content, atom_type).await
            }
            "get_atom" => {
                let atom_id: String = extract_arg(&args, "atom_id")?;
                self.get_atom(atom_id).await
            }
            "search_lex" => {
                let query: String = extract_arg(&args, "query")?;
                let limit: Option<u32> = extract_arg_opt(&args, "limit");
                self.search_lex(query, limit).await
            }
            "graph_walk" => {
                let seed_nodes: Vec<u64> = extract_arg(&args, "seed_nodes")?;
                let edge_types: Vec<String> = extract_arg(&args, "edge_types").unwrap_or_default();
                let depth: u8 = extract_arg(&args, "depth").unwrap_or(3);
                self.graph_walk(seed_nodes, edge_types, depth).await
            }
            "create_context" => {
                let policy_id: u64 = extract_arg(&args, "policy_id").unwrap_or(0);
                self.create_context(policy_id).await
            }
            "list_contexts" => self.list_contexts().await,
            "get_provenance" => {
                let atom_id: String = extract_arg(&args, "atom_id")?;
                self.get_provenance(atom_id).await
            }
            "verify_atom" => {
                let atom_id: String = extract_arg(&args, "atom_id")?;
                self.verify_atom(atom_id).await
            }
            "list_conflicts" => {
                let ctx_id: u64 = extract_arg(&args, "ctx_id")?;
                self.list_conflicts(ctx_id).await
            }
            _ => Err(format!("Unknown tool: {}", name)),
        }
    }

    /// Handle JSON-RPC request
    async fn handle_request(&mut self, request: JsonRpcRequest) -> JsonRpcResponse {
        match request.method.as_str() {
            "initialize" => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: Some(serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "memoryx",
                        "version": "0.1.0"
                    }
                })),
                error: None,
            },
            "tools/list" => {
                let tools = Self::define_tools();
                JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: request.id,
                    result: Some(serde_json::json!({
                        "tools": tools
                    })),
                    error: None,
                }
            }
            "tools/call" => {
                let name: String = match extract_arg(&request.params, "name") {
                    Ok(n) => n,
                    Err(e) => {
                        return JsonRpcResponse {
                            jsonrpc: "2.0".to_string(),
                            id: request.id,
                            result: None,
                            error: Some(RpcError::invalid_params(e)),
                        };
                    }
                };

                let arguments: serde_json::Value =
                    extract_arg(&request.params, "arguments").unwrap_or(serde_json::json!({}));

                match self.handle_tool_call(name, arguments).await {
                    Ok(result) => JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: request.id,
                        result: Some(serde_json::json!(result)),
                        error: None,
                    },
                    Err(e) => JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: request.id,
                        result: None,
                        error: Some(RpcError::tool_error(-32001, e, None)),
                    },
                }
            }
            _ => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: None,
                error: Some(RpcError::method_not_found(format!(
                    "Unknown method: {}",
                    request.method
                ))),
            },
        }
    }

    /// Run the MCP server (stdio transport)
    async fn run_stdio(&mut self) -> Result<(), String> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut stdout = tokio::io::stdout();

        eprintln!("MemoryX MCP Server v0.1.0");
        eprintln!("Listening on stdio...");

        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    // Parse JSON-RPC request
                    match serde_json::from_str::<JsonRpcRequest>(trimmed) {
                        Ok(request) => {
                            let response = self.handle_request(request).await;
                            let response_json = serde_json::to_string(&response)
                                .map_err(|e| format!("Failed to serialize response: {}", e))?;

                            stdout
                                .write_all(response_json.as_bytes())
                                .await
                                .map_err(|e| format!("Failed to write response: {}", e))?;
                            stdout
                                .write_all(b"\n")
                                .await
                                .map_err(|e| format!("Failed to write newline: {}", e))?;
                            stdout
                                .flush()
                                .await
                                .map_err(|e| format!("Failed to flush: {}", e))?;
                        }
                        Err(e) => {
                            let error_response = JsonRpcResponse {
                                jsonrpc: "2.0".to_string(),
                                id: None,
                                result: None,
                                error: Some(RpcError::parse_error(format!("Invalid JSON: {}", e))),
                            };
                            let error_json = serde_json::to_string(&error_response).unwrap();
                            let _ = stdout.write_all(error_json.as_bytes()).await;
                            let _ = stdout.write_all(b"\n").await;
                            let _ = stdout.flush().await;
                        }
                    }
                }
                Err(e) => return Err(format!("Read error: {}", e)),
            }
        }

        Ok(())
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Format answer for human-readable output
fn format_answer(answer: &AnswerPack) -> String {
    let mut output = String::new();

    writeln!(output, "=== Answer ===").unwrap();
    writeln!(output).unwrap();

    // Confidence
    writeln!(output, "**Confidence**: {:.1}%", answer.confidence * 100.0).unwrap();
    writeln!(output).unwrap();

    // Claims
    if !answer.claims.is_empty() {
        writeln!(output, "**Claims** ({} found):", answer.claims.len()).unwrap();
        for (i, claim) in answer.claims.iter().take(10).enumerate() {
            writeln!(output, "  {}. {:?}", i + 1, claim).unwrap();
        }
        if answer.claims.len() > 10 {
            writeln!(output, "  ... and {} more", answer.claims.len() - 10).unwrap();
        }
        writeln!(output).unwrap();
    }

    // Evidence
    if !answer.evidence.is_empty() {
        writeln!(output, "**Evidence** ({} sources):", answer.evidence.len()).unwrap();
        for (i, ev) in answer.evidence.iter().take(5).enumerate() {
            writeln!(
                output,
                "  {}. {:?} (trust: {}/10000)",
                i + 1,
                ev.section_kind,
                ev.trust
            )
            .unwrap();
        }
        if answer.evidence.len() > 5 {
            writeln!(output, "  ... and {} more", answer.evidence.len() - 5).unwrap();
        }
        writeln!(output).unwrap();
    }

    // Limitations
    if !answer.limitations.is_empty() {
        writeln!(output, "**Limitations**:").unwrap();
        for lim in &answer.limitations {
            writeln!(output, "  - [{}] {}", lim.code, lim.description).unwrap();
        }
        writeln!(output).unwrap();
    }

    // Alternatives
    if !answer.alternates.is_empty() {
        writeln!(
            output,
            "**Alternative Answers** ({} available):",
            answer.alternates.len()
        )
        .unwrap();
        for (i, alt) in answer.alternates.iter().take(3).enumerate() {
            writeln!(
                output,
                "  {}. Confidence: {:.1}%, Claims: {}",
                i + 1,
                alt.confidence * 100.0,
                alt.claims.len()
            )
            .unwrap();
        }
        if answer.alternates.len() > 3 {
            writeln!(output, "  ... and {} more", answer.alternates.len() - 3).unwrap();
        }
        writeln!(output).unwrap();
    }

    // Graph info
    writeln!(output, "**Provenance Graph**").unwrap();
    writeln!(output, "  Nodes: {}", answer.graph.node_count()).unwrap();
    writeln!(output, "  Edges: {}", answer.graph.edge_count()).unwrap();
    writeln!(output, "  Cost: {:.2}", answer.graph.total_cost).unwrap();

    output
}

/// Parse atom type from string
fn parse_atom_type(s: &str) -> Option<AtomType> {
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

/// Parse edge type from string
fn parse_edge_type(s: &str) -> Option<EdgeType> {
    match s.to_uppercase().as_str() {
        "CAUSES" | "CAUSE" => Some(EdgeType::CAUSES),
        "SUPPORTS" | "SUPPORT" => Some(EdgeType::SUPPORTS),
        "CONTRADICTS" | "CONTRADICT" => Some(EdgeType::CONTRADICTS),
        "DEPENDS" | "DEPENDS_ON" => Some(EdgeType::DEPENDS_ON),
        "DEFINES" | "DEFINE" => Some(EdgeType::DEFINES),
        "REFINES" | "REFINE" => Some(EdgeType::REFINES),
        "GENERALIZES" | "GENERALIZE" => Some(EdgeType::GENERALIZES),
        "IMPLIES" | "IMPLY" => Some(EdgeType::IMPLIES),
        "ENABLES" | "ENABLE" => Some(EdgeType::ENABLES),
        "PREVENTS" | "PREVENT" => Some(EdgeType::PREVENTS),
        "SAME_AS" | "SAMEAS" => Some(EdgeType::SAME_AS),
        "DERIVED_FROM" | "DERIVED" => Some(EdgeType::DERIVED_FROM),
        _ => None,
    }
}

/// Parse atom ID from hex string
fn parse_atom_id(s: &str) -> Option<AtomId> {
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

fn collect_text_symbols(title: &str, content: &str) -> Vec<String> {
    fn push_symbol(symbols: &mut Vec<String>, seen: &mut HashSet<String>, value: String) {
        let symbol = value.trim().to_string();
        if !symbol.is_empty() && seen.insert(symbol.clone()) {
            symbols.push(symbol);
        }
    }

    fn push_text_tokens(symbols: &mut Vec<String>, seen: &mut HashSet<String>, text: &str) {
        let mut current = String::new();
        for ch in text.chars() {
            if ch.is_alphanumeric() || ch == '_' || ch == '-' {
                current.push(ch.to_ascii_lowercase());
            } else if !current.is_empty() {
                push_symbol(symbols, seen, std::mem::take(&mut current));
            }
        }

        if !current.is_empty() {
            push_symbol(symbols, seen, current);
        }
    }

    let mut symbols = Vec::new();
    let mut seen = HashSet::new();

    push_symbol(&mut symbols, &mut seen, title.to_string());
    push_text_tokens(&mut symbols, &mut seen, title);
    push_text_tokens(&mut symbols, &mut seen, content);

    if symbols.is_empty() {
        symbols.push("text".to_string());
    }

    symbols
}

fn build_text_atom_payload(
    atom_type: AtomType,
    symbols: &[String],
    trust_level: u16,
    domain_mask: u64,
) -> Result<Vec<u8>, String> {
    let mut symbols_section = SymbolsSection::new();
    for symbol in symbols {
        symbols_section.intern(symbol.clone());
    }
    let symbols_bytes = symbols_section.to_bytes();

    let refs_bytes = Vec::new();
    let claims_bytes = ClaimsSection::new().to_bytes();
    let invariants_bytes = InvariantsSection::new().to_bytes();
    let edges_bytes = Vec::new();
    let evidence_bytes = EvidenceSection::new().to_bytes();

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

    let sections_data_start = AtomBodyHeader::SIZE + 7 * SectionDesc::SIZE;
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

    body.extend_from_slice(&0x41544F4Du32.to_le_bytes());
    body.extend_from_slice(&0x0001u16.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&0u64.to_le_bytes());
    body.extend_from_slice(&0u64.to_le_bytes());
    body.extend_from_slice(&u64::MAX.to_le_bytes());
    body.extend_from_slice(&(atom_type as u32).to_le_bytes());
    body.extend_from_slice(&7u32.to_le_bytes());
    body.extend_from_slice(&48u64.to_le_bytes());

    let mut add_section_desc = |kind: u32, off: usize, data: &[u8]| {
        let crc = memoryx::utils::crc32(data);
        body.extend_from_slice(&kind.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&(off as u64).to_le_bytes());
        body.extend_from_slice(&(data.len() as u64).to_le_bytes());
        body.extend_from_slice(&crc.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
    };

    add_section_desc(0x01, symbols_off, &symbols_bytes);
    add_section_desc(0x02, refs_off, &refs_bytes);
    add_section_desc(0x03, claims_off, &claims_bytes);
    add_section_desc(0x04, invariants_off, &invariants_bytes);
    add_section_desc(0x05, edges_off, &edges_bytes);
    add_section_desc(0x06, evidence_off, &evidence_bytes);
    add_section_desc(0x07, meta_off, &meta_bytes);

    body.extend_from_slice(&symbols_bytes);
    body.extend_from_slice(&refs_bytes);
    body.extend_from_slice(&claims_bytes);
    body.extend_from_slice(&invariants_bytes);
    body.extend_from_slice(&edges_bytes);
    body.extend_from_slice(&evidence_bytes);
    body.extend_from_slice(&meta_bytes);

    Ok(body)
}

/// Extract argument from JSON value
fn extract_arg<T: serde::de::DeserializeOwned>(
    value: &serde_json::Value,
    key: &str,
) -> Result<T, String> {
    value
        .get(key)
        .ok_or_else(|| format!("Missing required argument: {}", key))
        .and_then(|v| {
            serde_json::from_value(v.clone())
                .map_err(|e| format!("Invalid argument '{}': {}", key, e))
        })
}

/// Extract optional argument from JSON value
fn extract_arg_opt<T: serde::de::DeserializeOwned>(
    value: &serde_json::Value,
    key: &str,
) -> Option<T> {
    value
        .get(key)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
}

// ============================================================================
// Main Entry Point
// ============================================================================

#[cfg(feature = "mcp")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::env;

    // Parse command line arguments
    let args: Vec<String> = env::args().collect();
    let mut data_dir_arg: Option<PathBuf> = None;
    let mut base_scope = "project".to_string();
    let mut base_name: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--data-dir" | "-d" => {
                if i + 1 < args.len() {
                    data_dir_arg = Some(PathBuf::from(&args[i + 1]));
                    i += 2;
                } else {
                    eprintln!("Error: --data-dir requires a path argument");
                    std::process::exit(1);
                }
            }
            "--base-scope" => {
                if i + 1 < args.len() {
                    base_scope = args[i + 1].clone();
                    i += 2;
                } else {
                    eprintln!("Error: --base-scope requires 'project' or 'user'");
                    std::process::exit(1);
                }
            }
            "--base-name" => {
                if i + 1 < args.len() {
                    base_name = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("Error: --base-name requires a value");
                    std::process::exit(1);
                }
            }
            "--help" | "-h" => {
                println!("MemoryX MCP Server v0.1.0");
                println!();
                println!("Usage: mcp_server [OPTIONS]");
                println!();
                println!("Options:");
                println!("  -d, --data-dir <PATH|NAME>  Base path or base name");
                println!("      --base-scope <SCOPE>    project|user (default: project)");
                println!(
                    "      --base-name <NAME>      Base name when no path is provided (default: default)"
                );
                println!("  -h, --help                  Show this help message");
                println!();
                println!("Resolved defaults:");
                println!("  project -> <cwd>/.memoryx/bases/default");
                println!("  user    -> <home>/.memoryx/bases/default");
                println!();
                println!("MCP Tools:");
                println!("  query          - Query knowledge base with natural language");
                println!("  ingest_text    - Ingest text content as knowledge atom");
                println!("  get_atom       - Retrieve atom by ID");
                println!("  search_lex     - Lexical search by terms");
                println!("  graph_walk     - Graph traversal from seed nodes");
                println!("  create_context - Create new context branch");
                println!("  list_contexts  - List all contexts");
                println!("  get_provenance - Get evidence for atom");
                println!("  verify_atom    - Verify atom integrity");
                println!("  list_conflicts - List conflicts in context");
                return Ok(());
            }
            _ => {
                eprintln!("Unknown option: {}", args[i]);
                std::process::exit(1);
            }
        }
    }

    let data_dir = match resolve_data_dir(data_dir_arg, &base_scope, base_name) {
        Ok(path) => path,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    // Create data directory if it doesn't exist
    std::fs::create_dir_all(&data_dir)
        .map_err(|e| format!("Failed to create data directory: {}", e))?;

    // Create server
    let config = ServerConfig::default();
    let mut server = MemoryXServer::new(data_dir.clone(), config).map_err(|e| {
        eprintln!("Failed to create server: {}", e);
        std::process::exit(1);
    })?;

    eprintln!("Data directory: {}", data_dir.display());

    // Run server
    server.run_stdio().await.map_err(|e| {
        eprintln!("Server error: {}", e);
        std::process::exit(1);
    })?;

    Ok(())
}

#[cfg(not(feature = "mcp"))]
fn main() {
    eprintln!("Error: This example requires the 'mcp' feature");
    eprintln!("Run with: cargo run --example mcp_server --features mcp");
    std::process::exit(1);
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_atom_type() {
        assert_eq!(parse_atom_type("FACT"), Some(AtomType::FACT));
        assert_eq!(parse_atom_type("fact"), Some(AtomType::FACT));
        assert_eq!(parse_atom_type("DEFINITION"), Some(AtomType::DEFINITION));
        assert_eq!(parse_atom_type("INVALID"), None);
    }

    #[test]
    fn test_parse_edge_type() {
        assert_eq!(parse_edge_type("CAUSES"), Some(EdgeType::CAUSES));
        assert_eq!(parse_edge_type("causes"), Some(EdgeType::CAUSES));
        assert_eq!(parse_edge_type("DEPENDS_ON"), Some(EdgeType::DEPENDS_ON));
        assert_eq!(parse_edge_type("SUPPORTS"), Some(EdgeType::SUPPORTS));
        assert_eq!(parse_edge_type("INVALID"), None);
    }

    #[test]
    fn test_parse_atom_id() {
        let hex = "0000000000000000000000000000000000000000000000000000000000000000";
        let result = parse_atom_id(hex);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), [0u8; 32]);

        // With 0x prefix
        let hex_prefix = format!("0x{}", hex);
        let result = parse_atom_id(&hex_prefix);
        assert!(result.is_some());

        // Invalid length
        assert!(parse_atom_id("abc").is_none());
        assert!(parse_atom_id(&hex[..30]).is_none());
    }

    #[test]
    fn test_build_text_atom_payload_has_symbols_and_empty_claims() {
        let symbols = collect_text_symbols("Test Title", "This is a sentence. Another sentence!");
        let payload = build_text_atom_payload(AtomType::FACT, &symbols, 5000, 0xFFFF).unwrap();

        let header = AtomBodyHeader::from_bytes(&payload).unwrap();
        assert_eq!(header.section_count, 7);
        assert_eq!(header.section_table_off as usize, AtomBodyHeader::SIZE);
        assert_eq!(header.atom_type(), Some(AtomType::FACT));

        let table_start = header.section_table_off as usize;
        let mut sections = Vec::new();
        for i in 0..header.section_count as usize {
            let start = table_start + i * SectionDesc::SIZE;
            let desc = SectionDesc::from_bytes(&payload[start..start + SectionDesc::SIZE]).unwrap();
            sections.push(desc);
        }

        let symbols_desc = &sections[0];
        let claims_desc = &sections[2];

        let symbols_start = symbols_desc.off as usize;
        let symbols_end = symbols_start + symbols_desc.len as usize;
        let symbols_section =
            SymbolsSection::from_bytes(&payload[symbols_start..symbols_end]).unwrap();
        assert!(symbols_section.find("Test Title").is_some());
        assert!(symbols_section.find("test").is_some());
        assert!(symbols_section.find("sentence").is_some());
        assert!(!symbols_section.is_empty());

        let claims_start = claims_desc.off as usize;
        let claims_end = claims_start + claims_desc.len as usize;
        let claims_section = ClaimsSection::from_bytes(&payload[claims_start..claims_end]).unwrap();
        assert!(claims_section.is_empty());
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn test_native_api_get_atom_mmap_returns_raw_record() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let server = MemoryXServer::new(dir.path().to_path_buf(), ServerConfig::default()).unwrap();
        let symbols = collect_text_symbols("Test Title", "This is a sentence. Another sentence!");
        let payload = build_text_atom_payload(AtomType::FACT, &symbols, 5000, 0xFFFF).unwrap();

        let atom_id = {
            let mut store = server.store.write().await;
            store.ingest(&payload, AtomType::FACT, &[], &[]).unwrap()
        };

        let native = server.native_api();
        let native_bytes = native.get_atom_mmap(&atom_id).await.unwrap();
        let store_bytes = {
            let store = server.store.read().await;
            let store_ref: &MemoryX = &*store;
            store_ref.read_raw_record(&atom_id).unwrap()
        };

        assert_eq!(native_bytes, store_bytes);
        assert!(!native_bytes.is_empty());
    }

    #[test]
    fn test_format_answer() {
        let answer = AnswerPack::new(0);
        let formatted = format_answer(&answer);
        assert!(formatted.contains("Confidence"));
        assert!(formatted.contains("Provenance Graph"));
    }
}
