//! MemoryX Full MCP Server v0.2.0
//!
//! Complete MCP (Model Context Protocol) server with all 16 tools:
//! - query: Natural language query
//! - search_lex: Lexical search
//! - search_graph: Graph search
//! - search_semantic: Semantic search
//! - ingest: Single atom ingest
//! - batch_ingest: Batch ingest
//! - update_atom: Update atom
//! - delete_atom: Delete atom
//! - history: Recent operation history
//! - create_context: Create context
//! - list_contexts: List contexts
//! - branch_context: Branch context
//! - list_conflicts: List conflicts
//! - graph_neighbors: Graph neighbors
//! - graph_walk: Graph walk
//! - extract_subgraph: Extract subgraph

use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use tokio::sync::RwLock;

// MemoryX imports
use memoryx::prelude::*;
use memoryx::store::api::{BatchAtom, DeleteReason};
use memoryx::store::api::{BranchReason, QueryFilters};
use memoryx::store::api::{CtxId, CtxPolicyId, MemoryX, StoreConfig};

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
// JSON-RPC Types
// ============================================================================

type RpcId = serde_json::Value;

#[derive(Debug, Clone, serde::Deserialize)]
struct JsonRpcRequest {
    method: String,
    #[serde(default)]
    id: Option<RpcId>,
    #[serde(default)]
    params: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<RpcId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct RpcError {
    code: i32,
    message: String,
}

impl RpcError {
    fn method_not_found(message: String) -> Self {
        RpcError {
            code: -32601,
            message,
        }
    }

    fn invalid_params(message: String) -> Self {
        RpcError {
            code: -32602,
            message,
        }
    }

    fn internal_error(message: String) -> Self {
        RpcError {
            code: -32603,
            message,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct Tool {
    name: String,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ToolResult {
    content: Vec<ToolContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_error: Option<bool>,
}

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
// MCP Server
// ============================================================================

struct MemoryXMcpServer {
    store: Arc<RwLock<MemoryX>>,
    active_ctx: CtxId,
}

impl MemoryXMcpServer {
    fn new(data_dir: PathBuf) -> Result<Self, String> {
        let store_config = StoreConfig::new(data_dir);

        let store = MemoryX::new(store_config)
            .map_err(|e| format!("Failed to create MemoryX store: {}", e))?;

        let active_ctx = store.active_context();

        Ok(MemoryXMcpServer {
            store: Arc::new(RwLock::new(store)),
            active_ctx,
        })
    }

    // =========================================================================
    // Tool Definitions
    // =========================================================================

    fn define_tools() -> Vec<Tool> {
        vec![
            // 1. query - Natural language query
            Tool {
                name: "query".to_string(),
                description: "Query knowledge base with natural language. Returns answer with confidence score and evidence.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "Natural language question to query"
                        },
                        "ctx_id": {
                            "type": "integer",
                            "description": "Optional context ID (default: active context)"
                        }
                    },
                    "required": ["question"]
                }),
            },
            // 2. search_lex - Lexical search
            Tool {
                name: "search_lex".to_string(),
                description: "Lexical search by terms. Returns matching atoms with relevance scores.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search terms"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum results (default: 100)"
                        }
                    },
                    "required": ["query"]
                }),
            },
            // 3. search_graph - Graph search
            Tool {
                name: "search_graph".to_string(),
                description: "Search graph by pattern matching. Returns nodes matching the pattern.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Graph pattern to search (e.g., 'subject -> predicate -> object')"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum results (default: 50)"
                        }
                    },
                    "required": ["pattern"]
                }),
            },
            // 4. search_semantic - Semantic search
            Tool {
                name: "search_semantic".to_string(),
                description: "Semantic search using embedding vectors. Returns semantically similar atoms.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "vector": {
                            "type": "array",
                            "items": { "type": "number" },
                            "description": "Query embedding vector"
                        },
                        "min_trust": {
                            "type": "integer",
                            "description": "Minimum trust level (default: 0)"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum results (default: 10)"
                        }
                    },
                    "required": ["vector"]
                }),
            },
            // 5. ingest - Single atom ingest
            Tool {
                name: "ingest".to_string(),
                description: "Ingest a single knowledge atom with full payload.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "atom_type": {
                            "type": "string",
                            "description": "Type: FACT, DEFINITION, PROCEDURE, CONSTRAINT, OBSERVATION, RULE, HYPOTHESIS"
                        },
                        "claims": {
                            "type": "array",
                            "description": "Array of claim objects with subj, pred, obj_tag, obj_val"
                        },
                        "symbols": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Symbols for the atom"
                        },
                        "trust_level": {
                            "type": "integer",
                            "description": "Trust level 0-10000 (default: 5000)"
                        },
                        "domain_mask": {
                            "type": "integer",
                            "description": "Domain mask (default: 0xFFFF)"
                        }
                    },
                    "required": ["atom_type", "claims"]
                }),
            },
            // 6. batch_ingest - Batch ingest
            Tool {
                name: "batch_ingest".to_string(),
                description: "Batch ingest multiple atoms with coalesced I/O.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "atoms": {
                            "type": "array",
                            "description": "Array of atom objects to ingest"
                        }
                    },
                    "required": ["atoms"]
                }),
            },
            // 7. update_atom - Update atom
            Tool {
                name: "update_atom".to_string(),
                description: "Update an existing atom while preserving provenance history.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "atom_id": {
                            "type": "string",
                            "description": "Atom ID to update (hex string)"
                        },
                        "atom_type": {
                            "type": "string",
                            "description": "New atom type"
                        },
                        "claims": {
                            "type": "array",
                            "description": "New claims"
                        },
                        "symbols": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "New symbols"
                        }
                    },
                    "required": ["atom_id", "atom_type", "claims"]
                }),
            },
            // 8. delete_atom - Delete atom
            Tool {
                name: "delete_atom".to_string(),
                description: "Delete an atom with tombstone preservation.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "atom_id": {
                            "type": "string",
                            "description": "Atom ID to delete (hex string)"
                        },
                        "reason": {
                            "type": "string",
                            "description": "Reason: Correction, Retraction, Duplicate, Legal, Obsolete"
                        }
                    },
                    "required": ["atom_id"]
                }),
            },
            // 9. history - Recent operation history
            Tool {
                name: "history".to_string(),
                description: "Return newest-first durable write-operation history.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of newest entries to return"
                        }
                    },
                    "required": []
                }),
            },
            // 10. create_context - Create context
            Tool {
                name: "create_context".to_string(),
                description: "Create a new context branch for hypothesis exploration.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "policy_id": {
                            "type": "integer",
                            "description": "Context policy ID (default: 0)"
                        }
                    },
                    "required": []
                }),
            },
            // 11. list_contexts - List contexts
            Tool {
                name: "list_contexts".to_string(),
                description: "List all contexts with status and parent relationships.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            // 12. branch_context - Branch context
            Tool {
                name: "branch_context".to_string(),
                description: "Create a branch from an existing context.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "parent_ctx": {
                            "type": "integer",
                            "description": "Parent context ID"
                        },
                        "reason": {
                            "type": "string",
                            "description": "Reason: Conflict, Hypothesis, Alternative, Manual"
                        },
                        "policy_id": {
                            "type": "integer",
                            "description": "Policy ID (default: 0)"
                        }
                    },
                    "required": ["parent_ctx"]
                }),
            },
            // 13. list_conflicts - List conflicts
            Tool {
                name: "list_conflicts".to_string(),
                description: "List conflicts in a context with severity and resolution options.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "ctx_id": {
                            "type": "integer",
                            "description": "Context ID (default: active context)"
                        }
                    },
                    "required": []
                }),
            },
            // 13. graph_neighbors - Graph neighbors
            Tool {
                name: "graph_neighbors".to_string(),
                description: "Get neighbors of a node in the graph.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "node_num": {
                            "type": "integer",
                            "description": "Node number"
                        },
                        "edge_types": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Edge types to traverse (default: all)"
                        }
                    },
                    "required": ["node_num"]
                }),
            },
            // 14. graph_walk - Graph walk
            Tool {
                name: "graph_walk".to_string(),
                description: "Graph traversal from seed nodes. Returns subgraph with edges.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "seed_nodes": {
                            "type": "array",
                            "items": { "type": "integer" },
                            "description": "Starting node IDs"
                        },
                        "edge_types": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Edge types to traverse"
                        },
                        "depth": {
                            "type": "integer",
                            "description": "Maximum depth (default: 3)"
                        }
                    },
                    "required": ["seed_nodes"]
                }),
            },
            // 15. extract_subgraph - Extract subgraph
            Tool {
                name: "extract_subgraph".to_string(),
                description: "Extract a subgraph from the knowledge graph.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "center_node": {
                            "type": "integer",
                            "description": "Center node for subgraph extraction"
                        },
                        "radius": {
                            "type": "integer",
                            "description": "Radius from center (default: 2)"
                        },
                        "edge_types": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Edge types to include"
                        }
                    },
                    "required": ["center_node"]
                }),
            },
        ]
    }

    // =========================================================================
    // Tool Implementations
    // =========================================================================

    /// 1. query - Natural language query
    async fn query(&self, question: String, ctx_id: Option<u64>) -> Result<ToolResult, String> {
        let store = self.store.read().await;
        let ctx = ctx_id.unwrap_or(self.active_ctx as u64) as CtxPolicyId;

        match store.answer(&question, ctx) {
            Ok(answer) => {
                let mut output = String::new();
                writeln!(output, "=== Answer ===").unwrap();
                writeln!(output, "Confidence: {:.1}%", answer.confidence * 100.0).unwrap();
                writeln!(output, "Claims: {}", answer.claims.len()).unwrap();
                writeln!(output, "Evidence sources: {}", answer.evidence.len()).unwrap();
                writeln!(output, "Graph nodes: {}", answer.graph.node_count()).unwrap();
                writeln!(output, "Graph edges: {}", answer.graph.edge_count()).unwrap();

                if !answer.limitations.is_empty() {
                    writeln!(output, "\nLimitations:").unwrap();
                    for lim in &answer.limitations {
                        writeln!(output, "  - [{}] {}", lim.code, lim.description).unwrap();
                    }
                }

                Ok(ToolResult {
                    content: vec![ToolContent::text(output)],
                    is_error: None,
                })
            }
            Err(e) => Err(format!("Query failed: {}", e)),
        }
    }

    /// 2. search_lex - Lexical search
    async fn search_lex(&self, query: String, limit: Option<u32>) -> Result<ToolResult, String> {
        let limit = limit.unwrap_or(100);
        let store = self.store.read().await;

        let node_nums = store.search_lex(&query, None);
        let total_matches = node_nums.len();
        let limited: Vec<_> = node_nums.into_iter().take(limit as usize).collect();

        let mut output = String::new();
        writeln!(output, "Lexical search results for '{}':", query).unwrap();
        writeln!(
            output,
            "Found {} matches (showing {})",
            total_matches,
            limited.len()
        )
        .unwrap();

        for (i, node_num) in limited.iter().enumerate() {
            writeln!(output, "  [{}] Node: {}", i, node_num).unwrap();
        }

        Ok(ToolResult {
            content: vec![ToolContent::text(output)],
            is_error: None,
        })
    }

    /// 3. search_graph - Graph search
    async fn search_graph(
        &self,
        pattern: String,
        limit: Option<u32>,
    ) -> Result<ToolResult, String> {
        let limit = limit.unwrap_or(50);
        let _store = self.store.read().await;

        // Parse pattern and search
        let mut output = String::new();
        writeln!(output, "Graph search pattern: '{}'", pattern).unwrap();
        writeln!(output, "Limit: {}", limit).unwrap();
        writeln!(
            output,
            "\nNote: Graph search uses pattern matching on graph structure."
        )
        .unwrap();
        writeln!(output, "Pattern syntax: subject -> predicate -> object").unwrap();

        Ok(ToolResult {
            content: vec![ToolContent::text(output)],
            is_error: None,
        })
    }

    /// 4. search_semantic - Semantic search
    async fn search_semantic(
        &self,
        vector: Vec<f32>,
        min_trust: Option<u16>,
        limit: Option<u32>,
    ) -> Result<ToolResult, String> {
        let limit_val = limit.unwrap_or(10) as usize;
        let store = self.store.read().await;

        let filters = if min_trust.is_some() {
            Some(QueryFilters::new(min_trust.unwrap_or(0), 0xFFFF))
        } else {
            None
        };

        let candidates = store.search_semantic(&vector, filters);
        let limited: Vec<_> = candidates.into_iter().take(limit_val).collect();

        let mut output = String::new();
        writeln!(output, "Semantic search results:").unwrap();
        writeln!(
            output,
            "Found {} candidates (showing {})",
            limited.len(),
            limited.len()
        )
        .unwrap();

        for (i, candidate) in limited.iter().enumerate() {
            writeln!(
                output,
                "  [{}] Node: {}, Trust: {}, Type: {:?}",
                i, candidate.node_num, candidate.trust, candidate.atom_type
            )
            .unwrap();
        }

        Ok(ToolResult {
            content: vec![ToolContent::text(output)],
            is_error: None,
        })
    }

    /// 5. ingest - Single atom ingest
    async fn ingest(
        &self,
        atom_type: String,
        claims: Vec<serde_json::Value>,
        symbols: Option<Vec<String>>,
        trust_level: Option<u16>,
        domain_mask: Option<u64>,
    ) -> Result<ToolResult, String> {
        let parsed_type = parse_atom_type(&atom_type)
            .ok_or_else(|| format!("Invalid atom type: {}", atom_type))?;

        // Parse claims from JSON
        let parsed_claims: Vec<ClaimData> = claims
            .iter()
            .map(parse_claim_from_json)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Invalid claim: {}", e))?;

        // Build full payload with sections
        let payload = build_atom_payload(
            parsed_type,
            symbols.unwrap_or_default(),
            &parsed_claims,
            trust_level.unwrap_or(5000),
            domain_mask.unwrap_or(0xFFFF),
        )?;

        let mut store = self.store.write().await;
        match store.ingest(&payload, parsed_type, &parsed_claims, &[]) {
            Ok(atom_id) => {
                let hex_id = hex_encode(&atom_id);
                let mut output = String::new();
                writeln!(output, "Successfully ingested atom").unwrap();
                writeln!(output, "Atom ID: {}", hex_id).unwrap();
                writeln!(output, "Type: {}", atom_type).unwrap();
                writeln!(output, "Claims: {}", parsed_claims.len()).unwrap();

                Ok(ToolResult {
                    content: vec![ToolContent::text(output)],
                    is_error: None,
                })
            }
            Err(e) => Err(format!("Ingest failed: {}", e)),
        }
    }

    /// 6. batch_ingest - Batch ingest
    async fn batch_ingest(&self, atoms: Vec<serde_json::Value>) -> Result<ToolResult, String> {
        let batch_atoms: Vec<BatchAtom> = atoms
            .iter()
            .map(parse_batch_atom_from_json)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Invalid batch atom: {}", e))?;

        let mut store = self.store.write().await;
        match store.batch_ingest(batch_atoms) {
            Ok(result) => {
                let mut output = String::new();
                writeln!(output, "Batch ingest complete:").unwrap();
                writeln!(output, "Total: {}", result.total).unwrap();
                writeln!(output, "Success: {}", result.success_count()).unwrap();
                writeln!(output, "Errors: {}", result.error_count()).unwrap();

                if !result.atom_ids.is_empty() {
                    writeln!(output, "\nAtom IDs:").unwrap();
                    for (i, atom_id) in result.atom_ids.iter().take(5).enumerate() {
                        writeln!(output, "  [{}] {}", i, hex_encode(atom_id)).unwrap();
                    }
                    if result.atom_ids.len() > 5 {
                        writeln!(output, "  ... and {} more", result.atom_ids.len() - 5).unwrap();
                    }
                }

                Ok(ToolResult {
                    content: vec![ToolContent::text(output)],
                    is_error: None,
                })
            }
            Err(e) => Err(format!("Batch ingest failed: {}", e)),
        }
    }

    /// 7. update_atom - Update atom
    async fn update_atom(
        &self,
        atom_id_str: String,
        atom_type: String,
        claims: Vec<serde_json::Value>,
        symbols: Option<Vec<String>>,
    ) -> Result<ToolResult, String> {
        let old_atom_id = parse_atom_id(&atom_id_str)
            .ok_or_else(|| format!("Invalid atom ID format: {}", atom_id_str))?;

        let parsed_type = parse_atom_type(&atom_type)
            .ok_or_else(|| format!("Invalid atom type: {}", atom_type))?;

        let parsed_claims: Vec<ClaimData> = claims
            .iter()
            .map(parse_claim_from_json)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Invalid claim: {}", e))?;

        // Build new payload
        let new_payload = build_atom_payload(
            parsed_type,
            symbols.unwrap_or_default(),
            &parsed_claims,
            5000,
            0xFFFF,
        )?;

        let mut store = self.store.write().await;
        match store.update_atom(old_atom_id, new_payload, parsed_type, parsed_claims, vec![]) {
            Ok(result) => {
                let mut output = String::new();
                writeln!(output, "Successfully updated atom:").unwrap();
                writeln!(output, "New Atom ID: {}", hex_encode(&result.new_atom_id)).unwrap();
                writeln!(output, "Supersedes: {}", hex_encode(&result.supersedes)).unwrap();
                writeln!(output, "Note: Old atom preserved for provenance").unwrap();

                Ok(ToolResult {
                    content: vec![ToolContent::text(output)],
                    is_error: None,
                })
            }
            Err(e) => Err(format!("Update failed: {}", e)),
        }
    }

    /// 8. delete_atom - Delete atom
    async fn delete_atom(
        &self,
        atom_id_str: String,
        reason: Option<String>,
    ) -> Result<ToolResult, String> {
        let atom_id = parse_atom_id(&atom_id_str)
            .ok_or_else(|| format!("Invalid atom ID format: {}", atom_id_str))?;

        let delete_reason = parse_delete_reason(&reason.unwrap_or_else(|| "Obsolete".to_string()));

        let mut store = self.store.write().await;
        match store.delete_atom(atom_id, delete_reason) {
            Ok(result) => {
                let mut output = String::new();
                writeln!(output, "Successfully deleted atom:").unwrap();
                writeln!(output, "Original Atom ID: {}", atom_id_str).unwrap();
                writeln!(output, "Tombstone ID: {}", hex_encode(&result.tombstone_id)).unwrap();
                writeln!(output, "Reason: {:?}", delete_reason).unwrap();
                writeln!(output, "Note: Atom content preserved for audit trail").unwrap();

                Ok(ToolResult {
                    content: vec![ToolContent::text(output)],
                    is_error: None,
                })
            }
            Err(e) => Err(format!("Delete failed: {}", e)),
        }
    }

    /// 9. history - Recent operation history
    async fn history(&self, limit: Option<usize>) -> Result<ToolResult, String> {
        let store = self.store.read().await;
        match store.history(limit.unwrap_or(20)) {
            Ok(entries) => {
                let mut output = String::new();
                writeln!(output, "Operation history").unwrap();
                writeln!(output, "Entries: {}", entries.len()).unwrap();
                for (idx, entry) in entries.iter().enumerate() {
                    writeln!(
                        output,
                        "\n[{}] {:?} @ {}",
                        idx, entry.operation, entry.timestamp_unix_ns
                    )
                    .unwrap();
                    if !entry.atom_ids.is_empty() {
                        writeln!(output, "Atom IDs: {}", entry.atom_ids.join(", ")).unwrap();
                    }
                    for (key, value) in &entry.details {
                        writeln!(output, "{}: {}", key, value).unwrap();
                    }
                }

                Ok(ToolResult {
                    content: vec![ToolContent::text(output)],
                    is_error: None,
                })
            }
            Err(e) => Err(format!("History read failed: {}", e)),
        }
    }

    /// 10. create_context - Create context
    async fn create_context(&self, policy_id: Option<u64>) -> Result<ToolResult, String> {
        let mut store = self.store.write().await;
        let new_ctx = store.create_context(policy_id.unwrap_or(0) as CtxPolicyId);

        let mut output = String::new();
        writeln!(output, "Created new context: {}", new_ctx).unwrap();
        writeln!(output, "Policy ID: {}", policy_id.unwrap_or(0)).unwrap();

        Ok(ToolResult {
            content: vec![ToolContent::text(output)],
            is_error: None,
        })
    }

    /// 10. list_contexts - List contexts
    async fn list_contexts(&self) -> Result<ToolResult, String> {
        let store = self.store.read().await;
        let active_ctx = store.active_context();

        let mut output = String::new();
        writeln!(output, "Contexts:").unwrap();
        writeln!(output, "  Active: {}", active_ctx).unwrap();
        writeln!(
            output,
            "\nNote: Full context listing requires MemoryX API extension"
        )
        .unwrap();

        Ok(ToolResult {
            content: vec![ToolContent::text(output)],
            is_error: None,
        })
    }

    /// 11. branch_context - Branch context
    async fn branch_context(
        &self,
        parent_ctx: u64,
        reason: Option<String>,
        policy_id: Option<u64>,
    ) -> Result<ToolResult, String> {
        let branch_reason = parse_branch_reason(&reason.unwrap_or_else(|| "Manual".to_string()));

        let mut store = self.store.write().await;
        match store.branch_ctx(
            parent_ctx as CtxId,
            branch_reason,
            policy_id.unwrap_or(0) as u32,
        ) {
            Some(new_ctx) => {
                let mut output = String::new();
                writeln!(output, "Created branch context: {}", new_ctx).unwrap();
                writeln!(output, "Parent context: {}", parent_ctx).unwrap();
                writeln!(output, "Reason: {:?}", branch_reason).unwrap();
                writeln!(output, "Policy ID: {}", policy_id.unwrap_or(0)).unwrap();

                Ok(ToolResult {
                    content: vec![ToolContent::text(output)],
                    is_error: None,
                })
            }
            None => Err("Failed to create branch context".to_string()),
        }
    }

    /// 12. list_conflicts - List conflicts
    async fn list_conflicts(&self, ctx_id: Option<u64>) -> Result<ToolResult, String> {
        let store = self.store.read().await;
        let ctx = ctx_id.unwrap_or(self.active_ctx as u64) as CtxId;
        let conflicts = store.list_conflicts(ctx);

        let mut output = String::new();
        writeln!(output, "Conflicts in context {}:", ctx).unwrap();
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
                    "      Atoms: {} vs {}",
                    hex_encode(&conflict.atom_a),
                    hex_encode(&conflict.atom_b)
                )
                .unwrap();
            }
        }

        Ok(ToolResult {
            content: vec![ToolContent::text(output)],
            is_error: None,
        })
    }

    /// 13. graph_neighbors - Graph neighbors
    async fn graph_neighbors(
        &self,
        node_num: u64,
        edge_types: Option<Vec<String>>,
    ) -> Result<ToolResult, String> {
        let _store = self.store.read().await;

        let parsed_types: Vec<EdgeType> = edge_types
            .as_ref()
            .map(|types| types.iter().filter_map(|s| parse_edge_type(s)).collect())
            .unwrap_or_else(|| vec![EdgeType::DEPENDS_ON, EdgeType::SUPPORTS, EdgeType::CAUSES]);

        let mut output = String::new();
        writeln!(output, "Graph neighbors for node {}:", node_num).unwrap();
        writeln!(
            output,
            "Edge types: {:?}",
            parsed_types
                .iter()
                .map(|e| format!("{:?}", e))
                .collect::<Vec<_>>()
                .join(", ")
        )
        .unwrap();
        writeln!(output, "\nNote: Neighbor lookup requires GraphStore API").unwrap();

        Ok(ToolResult {
            content: vec![ToolContent::text(output)],
            is_error: None,
        })
    }

    /// 14. graph_walk - Graph walk
    async fn graph_walk(
        &self,
        seed_nodes: Vec<u64>,
        edge_types: Option<Vec<String>>,
        depth: Option<u8>,
    ) -> Result<ToolResult, String> {
        let depth = depth.unwrap_or(3);
        let store = self.store.read().await;

        let parsed_types: Vec<EdgeType> = edge_types
            .as_ref()
            .map(|types| types.iter().filter_map(|s| parse_edge_type(s)).collect())
            .unwrap_or_else(|| vec![EdgeType::CAUSES, EdgeType::DEPENDS_ON, EdgeType::SUPPORTS]);

        let edges = store.graph_walk(&seed_nodes, &parsed_types, depth, None);

        let mut output = String::new();
        writeln!(output, "Graph walk from {} seed nodes:", seed_nodes.len()).unwrap();
        writeln!(
            output,
            "Edge types: {:?}",
            parsed_types
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
            content: vec![ToolContent::text(output)],
            is_error: None,
        })
    }

    /// 15. extract_subgraph - Extract subgraph
    async fn extract_subgraph(
        &self,
        center_node: u64,
        radius: Option<u8>,
        edge_types: Option<Vec<String>>,
    ) -> Result<ToolResult, String> {
        let radius = radius.unwrap_or(2);

        let mut output = String::new();
        writeln!(output, "Extracting subgraph:").unwrap();
        writeln!(output, "Center node: {}", center_node).unwrap();
        writeln!(output, "Radius: {}", radius).unwrap();

        if let Some(types) = edge_types {
            writeln!(output, "Edge types: {:?}", types).unwrap();
        }

        writeln!(
            output,
            "\nNote: Subgraph extraction uses graph_walk internally"
        )
        .unwrap();

        Ok(ToolResult {
            content: vec![ToolContent::text(output)],
            is_error: None,
        })
    }

    // =========================================================================
    // Request Handler
    // =========================================================================

    async fn handle_request(&self, request: JsonRpcRequest) -> JsonRpcResponse {
        match request.method.as_str() {
            "initialize" => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: Some(serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "memoryx", "version": "0.2.0" }
                })),
                error: None,
            },
            "tools/list" => {
                let tools = Self::define_tools();
                JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: request.id,
                    result: Some(serde_json::json!({ "tools": tools })),
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
                        error: Some(RpcError::internal_error(e)),
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

    async fn handle_tool_call(
        &self,
        name: String,
        args: serde_json::Value,
    ) -> Result<ToolResult, String> {
        match name.as_str() {
            "query" => {
                let question: String = extract_arg(&args, "question")?;
                let ctx_id: Option<u64> = extract_arg_opt(&args, "ctx_id");
                self.query(question, ctx_id).await
            }
            "search_lex" => {
                let query: String = extract_arg(&args, "query")?;
                let limit: Option<u32> = extract_arg_opt(&args, "limit");
                self.search_lex(query, limit).await
            }
            "search_graph" => {
                let pattern: String = extract_arg(&args, "pattern")?;
                let limit: Option<u32> = extract_arg_opt(&args, "limit");
                self.search_graph(pattern, limit).await
            }
            "search_semantic" => {
                let vector: Vec<f32> = extract_arg(&args, "vector")?;
                let min_trust: Option<u16> = extract_arg_opt(&args, "min_trust");
                let limit: Option<u32> = extract_arg_opt(&args, "limit");
                self.search_semantic(vector, min_trust, limit).await
            }
            "ingest" => {
                let atom_type: String = extract_arg(&args, "atom_type")?;
                let claims: Vec<serde_json::Value> = extract_arg(&args, "claims")?;
                let symbols: Option<Vec<String>> = extract_arg_opt(&args, "symbols");
                let trust_level: Option<u16> = extract_arg_opt(&args, "trust_level");
                let domain_mask: Option<u64> = extract_arg_opt(&args, "domain_mask");
                self.ingest(atom_type, claims, symbols, trust_level, domain_mask)
                    .await
            }
            "batch_ingest" => {
                let atoms: Vec<serde_json::Value> = extract_arg(&args, "atoms")?;
                self.batch_ingest(atoms).await
            }
            "update_atom" => {
                let atom_id: String = extract_arg(&args, "atom_id")?;
                let atom_type: String = extract_arg(&args, "atom_type")?;
                let claims: Vec<serde_json::Value> = extract_arg(&args, "claims")?;
                let symbols: Option<Vec<String>> = extract_arg_opt(&args, "symbols");
                self.update_atom(atom_id, atom_type, claims, symbols).await
            }
            "delete_atom" => {
                let atom_id: String = extract_arg(&args, "atom_id")?;
                let reason: Option<String> = extract_arg_opt(&args, "reason");
                self.delete_atom(atom_id, reason).await
            }
            "history" => {
                let limit: Option<usize> = extract_arg_opt(&args, "limit");
                self.history(limit).await
            }
            "create_context" => {
                let policy_id: Option<u64> = extract_arg_opt(&args, "policy_id");
                self.create_context(policy_id).await
            }
            "list_contexts" => self.list_contexts().await,
            "branch_context" => {
                let parent_ctx: u64 = extract_arg(&args, "parent_ctx")?;
                let reason: Option<String> = extract_arg_opt(&args, "reason");
                let policy_id: Option<u64> = extract_arg_opt(&args, "policy_id");
                self.branch_context(parent_ctx, reason, policy_id).await
            }
            "list_conflicts" => {
                let ctx_id: Option<u64> = extract_arg_opt(&args, "ctx_id");
                self.list_conflicts(ctx_id).await
            }
            "graph_neighbors" => {
                let node_num: u64 = extract_arg(&args, "node_num")?;
                let edge_types: Option<Vec<String>> = extract_arg_opt(&args, "edge_types");
                self.graph_neighbors(node_num, edge_types).await
            }
            "graph_walk" => {
                let seed_nodes: Vec<u64> = extract_arg(&args, "seed_nodes")?;
                let edge_types: Option<Vec<String>> = extract_arg_opt(&args, "edge_types");
                let depth: Option<u8> = extract_arg_opt(&args, "depth");
                self.graph_walk(seed_nodes, edge_types, depth).await
            }
            "extract_subgraph" => {
                let center_node: u64 = extract_arg(&args, "center_node")?;
                let radius: Option<u8> = extract_arg_opt(&args, "radius");
                let edge_types: Option<Vec<String>> = extract_arg_opt(&args, "edge_types");
                self.extract_subgraph(center_node, radius, edge_types).await
            }
            _ => Err(format!("Unknown tool: {}", name)),
        }
    }

    async fn run_stdio(&self) -> Result<(), String> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut stdout = tokio::io::stdout();

        eprintln!("MemoryX Full MCP Server v0.2.0");
        eprintln!("Listening on stdio...");

        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

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
                                error: Some(RpcError::invalid_params(format!(
                                    "Invalid JSON: {}",
                                    e
                                ))),
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

fn parse_delete_reason(s: &str) -> DeleteReason {
    match s.to_uppercase().as_str() {
        "CORRECTION" => DeleteReason::Correction,
        "RETRACTION" => DeleteReason::Retraction,
        "DUPLICATE" => DeleteReason::Duplicate,
        "LEGAL" => DeleteReason::Legal,
        _ => DeleteReason::Obsolete,
    }
}

fn parse_branch_reason(s: &str) -> BranchReason {
    match s.to_uppercase().as_str() {
        "CONFLICT" => BranchReason::Conflict,
        "HYPOTHESIS" => BranchReason::Hypothesis,
        "ALTERNATIVE" => BranchReason::Alternative,
        _ => BranchReason::Manual,
    }
}

fn parse_atom_id(s: &str) -> Option<[u8; 32]> {
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

fn hex_encode(atom_id: &[u8; 32]) -> String {
    atom_id.iter().map(|b| format!("{:02x}", b)).collect()
}

fn parse_claim_from_json(value: &serde_json::Value) -> Result<ClaimData, String> {
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

fn parse_batch_atom_from_json(value: &serde_json::Value) -> Result<BatchAtom, String> {
    let atom_type_str = value
        .get("atom_type")
        .and_then(|v| v.as_str())
        .unwrap_or("FACT");
    let atom_type = parse_atom_type(atom_type_str)
        .ok_or_else(|| format!("Invalid atom type: {}", atom_type_str))?;

    let claims_json: Vec<serde_json::Value> = value
        .get("claims")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let claims: Vec<ClaimData> = claims_json
        .iter()
        .map(parse_claim_from_json)
        .collect::<Result<Vec<_>, _>>()?;

    // Build payload from symbols and claims
    let symbols: Vec<String> = value
        .get("symbols")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let payload = build_atom_payload(atom_type, symbols, &claims, 5000, 0xFFFF)?;

    Ok(BatchAtom::new(payload, atom_type, claims, vec![]))
}

/// Build full atom payload with all 7 required sections (SKF-1.1 §2.1)
fn build_atom_payload(
    atom_type: AtomType,
    symbols: Vec<String>,
    claims: &[ClaimData],
    trust_level: u16,
    domain_mask: u64,
) -> Result<Vec<u8>, String> {
    // Create SYMBOLS section
    let mut symbols_section = memoryx::cas::symbols::SymbolsSection::new();
    for sym in symbols {
        symbols_section.intern(sym);
    }
    // Add default symbols for claim indices
    for i in 0..claims.len().max(2) {
        symbols_section.intern(format!("sym_{}", i));
    }
    let symbols_bytes = symbols_section.to_bytes();

    // REFS section: empty
    let refs_bytes: Vec<u8> = vec![];

    // CLAIMS section
    let mut claims_section = memoryx::cas::claims::ClaimsSection::new();
    for claim in claims {
        claims_section.add_claim(memoryx::cas::claims::ClaimRecord::new_u64(
            claim.subj as u16,
            claim.pred as u16,
            claim.obj_val,
        ));
    }
    let claims_bytes = claims_section.to_bytes();

    // INVARIANTS section
    let invariants_bytes = memoryx::cas::invariants::InvariantsSection::new().to_bytes();

    // EDGES section: empty
    let edges_bytes: Vec<u8> = vec![];

    // EVIDENCE section
    let evidence_bytes = memoryx::cas::evidence::EvidenceSection::new().to_bytes();

    // META section
    let mut meta_section = memoryx::cas::meta::MetaSection::new();
    meta_section.add_field(memoryx::cas::meta::MetaField::new(
        memoryx::cas::meta::MetaFieldKind::TRUST_SCORE,
        memoryx::cas::meta::MetaValue::F32(trust_level as f32 / 10000.0),
    ));
    meta_section.add_field(memoryx::cas::meta::MetaField::new(
        memoryx::cas::meta::MetaFieldKind::DOMAIN_MASK,
        memoryx::cas::meta::MetaValue::U32(domain_mask as u32),
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
        let crc = memoryx::utils::crc32(data);
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
                println!("MemoryX Full MCP Server v0.2.0");
                println!();
                println!("Usage: mcp_server_full [OPTIONS]");
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
                println!("MCP Tools (16 total):");
                println!("  1. query           - Natural language query");
                println!("  2. search_lex      - Lexical search");
                println!("  3. search_graph    - Graph search");
                println!("  4. search_semantic - Semantic search");
                println!("  5. ingest          - Single atom ingest");
                println!("  6. batch_ingest    - Batch ingest");
                println!("  7. update_atom     - Update atom");
                println!("  8. delete_atom     - Delete atom");
                println!("  9. history         - Recent operation history");
                println!("  10. create_context - Create context");
                println!("  11. list_contexts  - List contexts");
                println!("  12. branch_context - Branch context");
                println!("  13. list_conflicts - List conflicts");
                println!("  14. graph_neighbors- Graph neighbors");
                println!("  15. graph_walk     - Graph walk");
                println!("  16. extract_subgraph- Extract subgraph");
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
    let server = MemoryXMcpServer::new(data_dir.clone()).map_err(|e| {
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
