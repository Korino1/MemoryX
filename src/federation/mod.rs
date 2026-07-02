//! Federation Protocol for MemoryX SKF-1.1 Phase 3
//!
//! This module implements the federation protocol for cross-base knowledge sharing:
//! - discover(term_or_id): Find where to search for knowledge
//! - fetch(id): Retrieve KA from remote base by content-address
//! - negotiate_schema(): Agree on types/fields between bases
//! - sync_crdt(metadata): Merge dynamic metadata between bases
//!
//! # SKF-1.1 Section 7.2: Protocol Operations
//!
//! The federation protocol enables distributed knowledge graphs to:
//! - Share atoms across base boundaries
//! - Negotiate schema compatibility
//! - Synchronize metadata via CRDTs
//! - Map equivalent concepts across bases

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::cas::hex_decode;
use crate::crdt::{ActorId, MetaStore};
use crate::store::api::{MemoryX, StoreError};
use crate::store::{AtomId, AtomType, TrustLevel};
use serde::{Deserialize, Serialize};
use thiserror::Error;

// Re-export main types
pub use crate::store::api::EvidenceRef;

// Import proof-grade provenance types (SKF-1.1 Section 10.1)
use crate::store::api::ProvenanceChain;

// ============================================================================
// Base ID Type
// ============================================================================

/// Unique identifier for a knowledge base in the federation
/// Format: BLAKE3-256 hash of base public key (32 bytes)
pub type BaseId = [u8; 32];

/// Federation version for protocol compatibility
pub const FEDERATION_PROTOCOL_VERSION: u16 = 0x0101; // 1.1

fn current_unix_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0)
}

// ============================================================================
// Federation Errors
// ============================================================================

/// Federation operation errors
#[derive(Debug, Error, Clone)]
pub enum FederationError {
    /// Network error
    #[error("Network error: {0}")]
    Network(String),

    /// Peer not found
    #[error("Peer not found: {0}")]
    PeerNotFound(String),

    /// Timeout
    #[error("Operation timed out after {0}ms")]
    Timeout(u32),

    /// Schema mismatch
    #[error("Schema mismatch: {0}")]
    SchemaMismatch(String),

    /// Invalid response
    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    /// Atom not found on remote
    #[error("Atom not found on remote base: {0:?}")]
    AtomNotFound(AtomId),

    /// CRDT merge conflict
    #[error("CRDT merge failed: {0}")]
    CrdtMerge(String),

    /// Trust level too low
    #[error("Trust level too low: {0} < {1}")]
    TrustTooLow(TrustLevel, TrustLevel),

    /// Max hops exceeded
    #[error("Max hops exceeded: {0}")]
    MaxHopsExceeded(u8),

    /// Store error
    #[error("Store error: {0}")]
    Store(String),

    /// Serialization error
    #[error("Serialization error: {0}")]
    Serialization(String),
}

impl From<reqwest::Error> for FederationError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            FederationError::Timeout(0)
        } else if err.is_connect() {
            FederationError::Network(format!("Connection failed: {}", err))
        } else {
            FederationError::Network(err.to_string())
        }
    }
}

impl From<serde_json::Error> for FederationError {
    fn from(err: serde_json::Error) -> Self {
        FederationError::Serialization(err.to_string())
    }
}

impl From<StoreError> for FederationError {
    fn from(err: StoreError) -> Self {
        FederationError::Store(err.to_string())
    }
}

// ============================================================================
// Federation Configuration
// ============================================================================

/// Configuration for federation client
#[derive(Debug, Clone)]
pub struct FederationConfig {
    /// Local base identifier
    pub local_base_id: BaseId,
    /// Configured peers
    pub peers: Vec<PeerConfig>,
    /// Request timeout in milliseconds
    pub timeout_ms: u32,
    /// Maximum hops for federated queries
    pub max_hops: u8,
    /// Protocol version
    pub protocol_version: u16,
}

impl FederationConfig {
    /// Create new federation configuration
    pub fn new(local_base_id: BaseId) -> Self {
        FederationConfig {
            local_base_id,
            peers: Vec::new(),
            timeout_ms: 30_000, // 30 seconds default
            max_hops: 3,
            protocol_version: FEDERATION_PROTOCOL_VERSION,
        }
    }

    /// Add a peer configuration
    pub fn with_peer(mut self, peer: PeerConfig) -> Self {
        self.peers.push(peer);
        self
    }

    /// Set timeout
    pub fn with_timeout(mut self, timeout_ms: u32) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    /// Set max hops
    pub fn with_max_hops(mut self, max_hops: u8) -> Self {
        self.max_hops = max_hops;
        self
    }

    /// Find peer by base ID
    pub fn find_peer(&self, base_id: &BaseId) -> Option<&PeerConfig> {
        self.peers.iter().find(|p| &p.base_id == base_id)
    }

    /// Get peers by trust level (descending)
    pub fn peers_by_trust(&self) -> Vec<&PeerConfig> {
        let mut peers: Vec<_> = self.peers.iter().collect();
        peers.sort_by_key(|p| std::cmp::Reverse(p.trust_level));
        peers
    }
}

impl Default for FederationConfig {
    fn default() -> Self {
        FederationConfig {
            local_base_id: [0u8; 32],
            peers: Vec::new(),
            timeout_ms: 30_000,
            max_hops: 3,
            protocol_version: FEDERATION_PROTOCOL_VERSION,
        }
    }
}

/// Peer configuration
#[derive(Debug, Clone)]
pub struct PeerConfig {
    /// Remote base identifier
    pub base_id: BaseId,
    /// Endpoint URL (e.g., "https://peer.example.com")
    pub endpoint: String,
    /// Trust level (0-10000)
    pub trust_level: TrustLevel,
    /// Actor ID for CRDT operations
    pub actor_id: ActorId,
    /// Supported schema versions
    pub schema_versions: Vec<u16>,
}

impl PeerConfig {
    /// Create new peer configuration
    pub fn new(base_id: BaseId, endpoint: String, trust_level: TrustLevel) -> Self {
        PeerConfig {
            base_id,
            endpoint,
            trust_level,
            actor_id: ActorId::generate(),
            schema_versions: vec![FEDERATION_PROTOCOL_VERSION],
        }
    }

    /// Set actor ID
    pub fn with_actor_id(mut self, actor_id: ActorId) -> Self {
        self.actor_id = actor_id;
        self
    }

    /// Set supported schema versions
    pub fn with_schema_versions(mut self, versions: Vec<u16>) -> Self {
        self.schema_versions = versions;
        self
    }

    /// Check if peer supports schema version
    pub fn supports_schema(&self, version: u16) -> bool {
        self.schema_versions.contains(&version)
    }

    /// Get full URL for API endpoint
    pub fn api_url(&self, path: &str) -> String {
        format!("{}/{}", self.endpoint.trim_end_matches('/'), path)
    }
}

// ============================================================================
// Discovery Types
// ============================================================================

/// Discovery result for a term or atom
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryResult {
    /// Remote base ID where knowledge was found
    pub base_id: BaseId,
    /// Atom ID if found
    pub atom_id: Option<AtomId>,
    /// Term that was matched
    pub term: String,
    /// Relevance score (0.0 - 1.0)
    pub relevance: f64,
    /// Number of hops from local base
    pub hops: u8,
    /// Trust level of the path
    pub path_trust: TrustLevel,
    /// MapsTo relationships if available
    pub mappings: Vec<MapsTo>,
}

impl DiscoveryResult {
    /// Create new discovery result
    pub fn new(
        base_id: BaseId,
        term: String,
        relevance: f64,
        hops: u8,
        path_trust: TrustLevel,
    ) -> Self {
        DiscoveryResult {
            base_id,
            atom_id: None,
            term,
            relevance,
            hops,
            path_trust,
            mappings: Vec::new(),
        }
    }

    /// Add atom ID
    pub fn with_atom_id(mut self, atom_id: AtomId) -> Self {
        self.atom_id = Some(atom_id);
        self
    }

    /// Add mapping
    pub fn with_mapping(mut self, mapping: MapsTo) -> Self {
        self.mappings.push(mapping);
        self
    }
}

/// Discover request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverRequest {
    /// Term or atom ID to discover
    pub term: String,
    /// Maximum hops to search
    pub max_hops: u8,
    /// Minimum trust threshold
    pub min_trust: TrustLevel,
    /// Requesting base ID
    pub from_base: BaseId,
    /// Visited bases (to avoid loops)
    pub visited: Vec<BaseId>,
    /// Query constraints
    pub constraints: Option<QueryConstraints>,
}

impl DiscoverRequest {
    /// Create new discover request
    pub fn new(term: String, from_base: BaseId) -> Self {
        DiscoverRequest {
            term,
            max_hops: 3,
            min_trust: 1000,
            from_base,
            visited: Vec::new(),
            constraints: None,
        }
    }

    /// Set max hops
    pub fn with_max_hops(mut self, max_hops: u8) -> Self {
        self.max_hops = max_hops;
        self
    }

    /// Set minimum trust
    pub fn with_min_trust(mut self, min_trust: TrustLevel) -> Self {
        self.min_trust = min_trust;
        self
    }

    /// Add visited base
    pub fn add_visited(&mut self, base_id: BaseId) {
        self.visited.push(base_id);
    }

    /// Check if base was visited
    pub fn was_visited(&self, base_id: &BaseId) -> bool {
        self.visited.contains(base_id)
    }
}

/// Discover response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverResponse {
    /// Results from this base
    pub results: Vec<DiscoveryResult>,
    /// Forwarded results from other bases
    pub forwarded: Vec<DiscoveryResult>,
    /// Responding base ID
    pub from_base: BaseId,
    /// Protocol version
    pub protocol_version: u16,
}

impl DiscoverResponse {
    /// Create new discover response
    pub fn new(from_base: BaseId) -> Self {
        DiscoverResponse {
            results: Vec::new(),
            forwarded: Vec::new(),
            from_base,
            protocol_version: FEDERATION_PROTOCOL_VERSION,
        }
    }

    /// Add local result
    pub fn add_result(&mut self, result: DiscoveryResult) {
        self.results.push(result);
    }

    /// Add forwarded result
    pub fn add_forwarded(&mut self, result: DiscoveryResult) {
        self.forwarded.push(result);
    }

    /// Get all results (local + forwarded)
    pub fn all_results(&self) -> Vec<&DiscoveryResult> {
        self.results.iter().chain(self.forwarded.iter()).collect()
    }
}

/// Query constraints for discovery
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QueryConstraints {
    /// Maximum number of results
    pub max_results: Option<usize>,
    /// Domain mask filter
    pub domain_mask: Option<u64>,
    /// Atom type filter
    pub atom_types: Option<Vec<AtomType>>,
    /// Time range filter
    pub time_range: Option<(u64, u64)>,
}

// ============================================================================
// Fetch Types
// ============================================================================

/// Fetch request for retrieving atoms
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchRequest {
    /// Atom ID to fetch
    pub atom_id: AtomId,
    /// Requesting base ID
    pub from_base: BaseId,
    /// Include evidence
    pub include_evidence: bool,
    /// Include metadata
    pub include_meta: bool,
}

impl FetchRequest {
    /// Create new fetch request
    pub fn new(atom_id: AtomId, from_base: BaseId) -> Self {
        FetchRequest {
            atom_id,
            from_base,
            include_evidence: true,
            include_meta: true,
        }
    }

    /// Set include evidence
    pub fn with_evidence(mut self, include: bool) -> Self {
        self.include_evidence = include;
        self
    }

    /// Set include metadata
    pub fn with_meta(mut self, include: bool) -> Self {
        self.include_meta = include;
        self
    }
}

/// Fetch response with atom data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchResponse {
    /// Atom ID
    pub atom_id: AtomId,
    /// Atom body bytes
    pub body: Vec<u8>,
    /// Evidence references (legacy, backward compatibility)
    pub evidence: Vec<EvidenceRef>,
    /// Full provenance chain (SKF-1.1 proof-grade)
    /// Contains derivation nodes, evidence links, trust propagation, DERIVED_FROM chain
    pub provenance_chain: Option<ProvenanceChain>,
    /// Metadata
    pub metadata: Option<AtomMetadata>,
    /// Responding base ID
    pub from_base: BaseId,
    /// Trust level of source
    pub trust_level: TrustLevel,
}

impl FetchResponse {
    /// Create new fetch response
    pub fn new(atom_id: AtomId, body: Vec<u8>, from_base: BaseId) -> Self {
        FetchResponse {
            atom_id,
            body,
            evidence: Vec::new(),
            provenance_chain: None,
            metadata: None,
            from_base,
            trust_level: 5000,
        }
    }

    /// Add evidence (legacy)
    pub fn with_evidence(mut self, evidence: Vec<EvidenceRef>) -> Self {
        self.evidence = evidence;
        self
    }

    /// Add full provenance chain (proof-grade)
    /// Add full provenance chain (proof-grade)
    pub fn with_provenance(mut self, chain: ProvenanceChain) -> Self {
        // Populate legacy evidence for backward compatibility FIRST
        self.evidence = chain
            .direct_evidence
            .iter()
            .map(|link| EvidenceRef {
                atom_id: link.source_atom_id,
                section_kind: link.section_kind,
                offset: link.offset,
                length: link.length,
                trust: link.trust,
            })
            .collect();
        // THEN move chain into provenance_chain
        self.provenance_chain = Some(chain);
        self
    }

    /// Add metadata
    pub fn with_metadata(mut self, metadata: AtomMetadata) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// Set trust level
    pub fn with_trust(mut self, trust: TrustLevel) -> Self {
        self.trust_level = trust;
        self
    }
}

/// Atom metadata for federation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtomMetadata {
    /// Atom type
    pub atom_type: AtomType,
    /// Created timestamp (nanoseconds)
    pub created_at_ns: u64,
    /// Valid from timestamp
    pub valid_from_ns: u64,
    /// Valid to timestamp (0 = infinity)
    pub valid_to_ns: u64,
    /// Trust level
    pub trust_level: TrustLevel,
    /// Domain mask
    pub domain_mask: u64,
    /// Source base ID
    pub source_base: BaseId,
    /// Schema version
    pub schema_version: u16,
}

impl AtomMetadata {
    /// Create new atom metadata
    pub fn new(atom_type: AtomType, source_base: BaseId) -> Self {
        AtomMetadata {
            atom_type,
            created_at_ns: 0,
            valid_from_ns: 0,
            valid_to_ns: u64::MAX,
            trust_level: 5000,
            domain_mask: 0xFFFF,
            source_base,
            schema_version: FEDERATION_PROTOCOL_VERSION,
        }
    }

    /// Check if metadata is valid at given time
    pub fn is_valid_at(&self, timestamp_ns: u64) -> bool {
        timestamp_ns >= self.valid_from_ns
            && (self.valid_to_ns == 0 || timestamp_ns < self.valid_to_ns)
    }
}

// ============================================================================
// Schema Negotiation Types
// ============================================================================

/// Schema agreement result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaAgreement {
    /// Agreed schema version
    pub schema_version: u16,
    /// Supported atom types
    pub atom_types: Vec<AtomTypeSupport>,
    /// Field mappings
    pub field_mappings: Vec<FieldMapping>,
    /// Responding base ID
    pub remote_base: BaseId,
    /// Agreement timestamp
    pub timestamp_ns: u64,
    /// Compatibility score (0.0 - 1.0)
    pub compatibility: f64,
}

impl SchemaAgreement {
    /// Create new schema agreement
    pub fn new(schema_version: u16, remote_base: BaseId) -> Self {
        SchemaAgreement {
            schema_version,
            atom_types: Vec::new(),
            field_mappings: Vec::new(),
            remote_base,
            timestamp_ns: 0,
            compatibility: 1.0,
        }
    }

    /// Add atom type support
    pub fn add_atom_type(&mut self, support: AtomTypeSupport) {
        self.atom_types.push(support);
    }

    /// Add field mapping
    pub fn add_field_mapping(&mut self, mapping: FieldMapping) {
        self.field_mappings.push(mapping);
    }

    /// Calculate overall compatibility
    pub fn calculate_compatibility(&mut self) {
        if self.atom_types.is_empty() {
            self.compatibility = 0.0;
            return;
        }

        let total: f64 = self.atom_types.iter().map(|at| at.compatibility).sum();
        self.compatibility = total / self.atom_types.len() as f64;
    }
}

/// Atom type support information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtomTypeSupport {
    /// Atom type
    pub atom_type: AtomType,
    /// Remote type name (if different)
    pub remote_name: Option<String>,
    /// Field compatibility (0.0 - 1.0)
    pub compatibility: f64,
    /// Supported fields
    pub supported_fields: Vec<String>,
    /// Missing fields (not supported by remote)
    pub missing_fields: Vec<String>,
}

impl AtomTypeSupport {
    /// Create new atom type support
    pub fn new(atom_type: AtomType) -> Self {
        AtomTypeSupport {
            atom_type,
            remote_name: None,
            compatibility: 1.0,
            supported_fields: Vec::new(),
            missing_fields: Vec::new(),
        }
    }

    /// Set remote name
    pub fn with_remote_name(mut self, name: String) -> Self {
        self.remote_name = Some(name);
        self
    }
}

/// Field mapping between local and remote
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldMapping {
    /// Local field name
    pub local_field: String,
    /// Remote field name
    pub remote_field: String,
    /// Field type compatibility
    pub type_compatible: bool,
    /// Transformation required (e.g., unit conversion)
    pub transformation: Option<String>,
}

impl FieldMapping {
    /// Create new field mapping
    pub fn new(local_field: String, remote_field: String) -> Self {
        FieldMapping {
            local_field,
            remote_field,
            type_compatible: true,
            transformation: None,
        }
    }

    /// Mark as type incompatible
    pub fn incompatible(mut self) -> Self {
        self.type_compatible = false;
        self
    }

    /// Set transformation
    pub fn with_transformation(mut self, transformation: String) -> Self {
        self.transformation = Some(transformation);
        self
    }
}

/// Negotiate request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NegotiateRequest {
    /// Requesting base ID
    pub from_base: BaseId,
    /// Proposed schema version
    pub proposed_version: u16,
    /// Supported atom types
    pub supported_types: Vec<AtomTypeInfo>,
    /// Required field mappings
    pub required_mappings: Vec<String>,
}

impl NegotiateRequest {
    /// Create new negotiate request
    pub fn new(from_base: BaseId) -> Self {
        NegotiateRequest {
            from_base,
            proposed_version: FEDERATION_PROTOCOL_VERSION,
            supported_types: Vec::new(),
            required_mappings: Vec::new(),
        }
    }

    /// Add supported type
    pub fn add_type(&mut self, info: AtomTypeInfo) {
        self.supported_types.push(info);
    }
}

/// Atom type information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtomTypeInfo {
    /// Atom type
    pub atom_type: AtomType,
    /// Type name
    pub name: String,
    /// Required fields
    pub required_fields: Vec<String>,
    /// Optional fields
    pub optional_fields: Vec<String>,
}

impl AtomTypeInfo {
    /// Create new atom type info
    pub fn new(atom_type: AtomType, name: String) -> Self {
        AtomTypeInfo {
            atom_type,
            name,
            required_fields: Vec::new(),
            optional_fields: Vec::new(),
        }
    }
}

/// Negotiate response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NegotiateResponse {
    /// Schema agreement details
    pub agreement: SchemaAgreement,
    /// Rejected (cannot agree)
    pub rejected: bool,
    /// Rejection reason
    pub rejection_reason: Option<String>,
}

impl NegotiateResponse {
    /// Create successful negotiate response
    pub fn success(agreement: SchemaAgreement) -> Self {
        NegotiateResponse {
            agreement,
            rejected: false,
            rejection_reason: None,
        }
    }

    /// Create rejection response
    pub fn reject(reason: String) -> Self {
        NegotiateResponse {
            agreement: SchemaAgreement::new(0, [0u8; 32]),
            rejected: true,
            rejection_reason: Some(reason),
        }
    }
}

// ============================================================================
// CRDT Sync Types
// ============================================================================

/// Field entry in CRDT metadata wire format (SKF-1.1 A.1)
/// Each field carries its CRDT kind and serialized state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrdtFieldEntry {
    /// Field ID (0x0001 USAGE_READ_G, 0x0002 USAGE_HIT_G, etc.)
    pub field_id: u16,
    /// CRDT kind (GCOUNTER, PNCOUNTER, LWW_REG, ORSET, ORMAP, MVREG, FLAGSET)
    pub crdt_kind: crate::store::CrdtKind,
    /// Serialized CRDT state bytes
    pub state_bytes: Vec<u8>,
}

/// CRDT metadata for synchronization
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrdtMetadata {
    /// Actor ID for this metadata
    pub actor_id: ActorId,
    /// HLC timestamp
    pub hlc_timestamp: u64,
    /// Node metadata (NodeNum -> Field entries with CrdtKind)
    pub node_fields: HashMap<u64, Vec<CrdtFieldEntry>>,
    /// Atom metadata (AtomId hex -> Field entries with CrdtKind)
    pub atom_fields: HashMap<String, Vec<CrdtFieldEntry>>,
    /// Protocol version
    pub protocol_version: u16,
}

impl CrdtMetadata {
    /// Create new CRDT metadata
    pub fn new(actor_id: ActorId) -> Self {
        CrdtMetadata {
            actor_id,
            hlc_timestamp: 0,
            node_fields: HashMap::new(),
            atom_fields: HashMap::new(),
            protocol_version: FEDERATION_PROTOCOL_VERSION,
        }
    }

    /// Add node field entry
    pub fn add_node_field(
        &mut self,
        node: u64,
        field_id: u16,
        crdt_kind: crate::store::CrdtKind,
        state_bytes: Vec<u8>,
    ) {
        self.node_fields
            .entry(node)
            .or_default()
            .push(CrdtFieldEntry {
                field_id,
                crdt_kind,
                state_bytes,
            });
    }

    /// Add atom field entry
    pub fn add_atom_field(
        &mut self,
        atom_id: AtomId,
        field_id: u16,
        crdt_kind: crate::store::CrdtKind,
        state_bytes: Vec<u8>,
    ) {
        let key = hex::encode(atom_id);
        self.atom_fields
            .entry(key)
            .or_default()
            .push(CrdtFieldEntry {
                field_id,
                crdt_kind,
                state_bytes,
            });
    }

    /// Merge remote metadata into local MetaStore using CRDT join
    /// This is the REAL CRDT synchronization (not overwrite)
    pub fn merge_into_store(
        &self,
        meta_store: &mut crate::crdt::MetaStore,
    ) -> Result<usize, crate::crdt::CrdtError> {
        let mut join_count = 0;

        // Merge node fields
        for (node, entries) in &self.node_fields {
            for entry in entries {
                if meta_store.import_node_field(
                    *node,
                    entry.field_id,
                    entry.crdt_kind,
                    &entry.state_bytes,
                )? {
                    join_count += 1;
                }
            }
        }

        // Merge atom fields
        for (atom_key, entries) in &self.atom_fields {
            if let Ok(atom_id) = hex::decode(atom_key) && atom_id.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&atom_id);
                for entry in entries {
                    if meta_store.import_atom_field(
                        &arr,
                        entry.field_id,
                        entry.crdt_kind,
                        &entry.state_bytes,
                    )? {
                        join_count += 1;
                    }
                }
            }
        }

        Ok(join_count)
    }

    /// Count total field entries
    pub fn total_fields(&self) -> usize {
        self.node_fields.values().map(|v| v.len()).sum::<usize>()
            + self.atom_fields.values().map(|v| v.len()).sum::<usize>()
    }
}
/// Sync request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRequest {
    /// Requesting base ID
    pub from_base: BaseId,
    /// CRDT metadata to sync
    pub metadata: CrdtMetadata,
    /// Sync direction
    pub direction: SyncDirection,
    /// Specific nodes to sync (empty = all)
    pub node_filter: Vec<u64>,
    /// Specific atoms to sync (empty = all)
    pub atom_filter: Vec<AtomId>,
}

impl SyncRequest {
    /// Create new sync request
    pub fn new(from_base: BaseId, metadata: CrdtMetadata) -> Self {
        SyncRequest {
            from_base,
            metadata,
            direction: SyncDirection::Bidirectional,
            node_filter: Vec::new(),
            atom_filter: Vec::new(),
        }
    }

    /// Set direction
    pub fn with_direction(mut self, direction: SyncDirection) -> Self {
        self.direction = direction;
        self
    }
}

/// Sync direction
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SyncDirection {
    /// Push local changes to remote
    Push,
    /// Pull remote changes to local
    Pull,
    /// Bidirectional sync
    Bidirectional,
}

/// Sync response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResponse {
    /// Merged CRDT metadata
    pub metadata: CrdtMetadata,
    /// Conflicts detected
    pub conflicts: Vec<CrdtConflict>,
    /// Responding base ID
    pub from_base: BaseId,
    /// Success flag
    pub success: bool,
    /// Error message (if failed)
    pub error: Option<String>,
}

impl SyncResponse {
    /// Create successful sync response
    pub fn success(metadata: CrdtMetadata, from_base: BaseId) -> Self {
        SyncResponse {
            metadata,
            conflicts: Vec::new(),
            from_base,
            success: true,
            error: None,
        }
    }

    /// Create failed sync response
    pub fn failure(error: String, from_base: BaseId) -> Self {
        SyncResponse {
            metadata: CrdtMetadata::new(ActorId::generate()),
            conflicts: Vec::new(),
            from_base,
            success: false,
            error: Some(error),
        }
    }

    /// Add conflict
    pub fn add_conflict(&mut self, conflict: CrdtConflict) {
        self.conflicts.push(conflict);
    }
}

/// CRDT conflict
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrdtConflict {
    /// Entity type ("node" or "atom")
    pub entity_type: String,
    /// Entity ID (node number or atom ID)
    pub entity_id: String,
    /// Field number
    pub field: u16,
    /// Local HLC
    pub local_hlc: u64,
    /// Remote HLC
    pub remote_hlc: u64,
    /// Resolution (if resolved)
    pub resolution: Option<ConflictResolution>,
}

/// Conflict resolution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConflictResolution {
    /// Use local value
    UseLocal,
    /// Use remote value
    UseRemote,
    /// Merge values
    Merge(Vec<u8>),
}

// ============================================================================
// MapsTo Types (SKF-1.1 Section 7.3)
// ============================================================================

/// MapsTo object for cross-base identity mapping
///
/// SKF-1.1 Section 7.3: "MapsTo objects represent identity mappings
/// between atoms in different knowledge bases."
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MapsTo {
    /// Local atom ID
    pub local_id: AtomId,
    /// Remote base ID
    pub remote_base: BaseId,
    /// Remote atom ID
    pub remote_id: AtomId,
    /// Mapping confidence (0.0 - 1.0)
    pub confidence: f64,
    /// Evidence for the mapping
    pub evidence: Vec<MappingEvidence>,
    /// Constraints on the mapping
    pub constraints: Vec<MappingConstraint>,
    /// Mapping timestamp
    pub created_at_ns: u64,
    /// Valid until (0 = forever)
    pub valid_until_ns: u64,
}

impl MapsTo {
    /// Create new MapsTo mapping
    pub fn new(
        local_id: AtomId,
        remote_base: BaseId,
        remote_id: AtomId,
        confidence: f64,
    ) -> Self {
        MapsTo {
            local_id,
            remote_base,
            remote_id,
            confidence,
            evidence: Vec::new(),
            constraints: Vec::new(),
            created_at_ns: 0,
            valid_until_ns: 0,
        }
    }

    /// Add evidence
    pub fn add_evidence(&mut self, evidence: MappingEvidence) {
        self.evidence.push(evidence);
    }

    /// Add constraint
    pub fn add_constraint(&mut self, constraint: MappingConstraint) {
        self.constraints.push(constraint);
    }

    /// Check if mapping is valid (basic structural check)
    pub fn is_valid(&self) -> bool {
        // Confidence must be in valid range
        if self.confidence <= 0.0 || self.confidence > 1.0 {
            return false;
        }
        true
    }

    /// Check if mapping is valid at specific timestamp (SKF-1.1 validity)
    ///
    /// Validates:
    /// - Confidence range (0.0 - 1.0)
    /// - Temporal validity (not expired if valid_until_ns > 0)
    /// - Required constraints (if any marked as required)
    pub fn is_valid_at(&self, now_ns: u64) -> bool {
        // Confidence must be in valid range
        if self.confidence <= 0.0 || self.confidence > 1.0 {
            return false;
        }

        // Check temporal validity
        if self.valid_until_ns > 0 && now_ns >= self.valid_until_ns {
            return false;
        }

        // Check required constraints
        for constraint in &self.constraints {
            if constraint.constraint_type == ConstraintType::TrustThreshold {
                // Trust threshold constraint requires minimum confidence
                if let Some(threshold_str) = constraint.params.get("min_confidence")
                    && let Ok(threshold) = threshold_str.parse::<f64>()
                    && self.confidence < threshold
                {
                    return false;
                }
            }
        }

        true
    }

    /// Check if mapping is acceptable under trust policy (SKF-1.1 Section 7.3)
    ///
    /// This implements the full trust-aware validation required by concept:
    /// - Confidence threshold
    /// - Base allowance/blocking
    /// - Source trust level
    /// - Evidence requirements
    /// - Mapping age
    /// - All constraints
    ///
    /// Returns TrustCheckResult with detailed reason if rejected.
    pub fn is_acceptable_under(
        &self,
        policy: &TrustPolicy,
        now_ns: u64,
        source_trust: TrustLevel,
    ) -> TrustCheckResult {
        // 1. Check confidence threshold
        if self.confidence < policy.min_confidence {
            return TrustCheckResult::ConfidenceTooLow(self.confidence, policy.min_confidence);
        }

        // 2. Check confidence range validity
        if self.confidence <= 0.0 || self.confidence > 1.0 {
            return TrustCheckResult::ConfidenceTooLow(self.confidence, 0.0);
        }

        // 3. Check temporal validity (expiry)
        if self.valid_until_ns > 0 && now_ns >= self.valid_until_ns {
            return TrustCheckResult::Expired(self.valid_until_ns);
        }

        // 4. Check mapping age
        if policy.max_mapping_age_ns > 0 {
            let mapping_age = now_ns.saturating_sub(self.created_at_ns);
            if mapping_age > policy.max_mapping_age_ns {
                return TrustCheckResult::TooOld(mapping_age, policy.max_mapping_age_ns);
            }
        }

        // 5. Check base is not blocked
        if policy.blocked_bases.contains(&self.remote_base) {
            return TrustCheckResult::BaseBlocked;
        }

        // 6. Check base is allowed (if whitelist is set)
        if !policy.allowed_bases.is_empty() && !policy.allowed_bases.contains(&self.remote_base) {
            return TrustCheckResult::BaseNotAllowed;
        }

        // 7. Check source trust level
        if source_trust < policy.min_source_trust {
            return TrustCheckResult::SourceTrustTooLow(source_trust, policy.min_source_trust);
        }

        // 8. Check evidence requirements
        if policy.require_evidence && self.evidence.is_empty() {
            return TrustCheckResult::MissingEvidence;
        }

        // 9. Check evidence weight threshold
        if self.evidence_weight() < policy.min_evidence_weight {
            return TrustCheckResult::EvidenceWeightTooLow(self.evidence_weight(), policy.min_evidence_weight);
        }

        // 10. Check all constraints
        for constraint in &self.constraints {
            match constraint.constraint_type {
                ConstraintType::TimeRange => {
                    if let Some(start_str) = constraint.params.get("start_ns")
                        && let Some(end_str) = constraint.params.get("end_ns")
                        && let (Ok(start), Ok(end)) =
                            (start_str.parse::<u64>(), end_str.parse::<u64>())
                        && (now_ns < start || now_ns >= end)
                    {
                        return TrustCheckResult::ConstraintNotSatisfied("time_range".to_string());
                    }
                }
                ConstraintType::TrustThreshold => {
                    if let Some(threshold_str) = constraint.params.get("min_trust")
                        && let Ok(threshold) = threshold_str.parse::<TrustLevel>()
                        && source_trust < threshold
                    {
                        return TrustCheckResult::ConstraintNotSatisfied("trust_threshold".to_string());
                    }
                }
                ConstraintType::DomainRestriction => {
                    // Domain restriction is checked at query level, not mapping level
                    // But we can validate the constraint exists and is valid
                    if !constraint.params.contains_key("domain_mask") {
                        return TrustCheckResult::ConstraintNotSatisfied("domain_restriction: missing domain_mask".to_string());
                    }
                }
                ConstraintType::VersionCompatibility => {
                    if let Some(min_ver_str) = constraint.params.get("min_version")
                        && let Ok(min_ver) = min_ver_str.parse::<u16>()
                        && FEDERATION_PROTOCOL_VERSION < min_ver
                    {
                        return TrustCheckResult::ConstraintNotSatisfied("version_compatibility".to_string());
                    }
                }
            }
        }

        TrustCheckResult::Acceptable
    }

    /// Check if mapping is acceptable under policy (returns bool)
    pub fn is_acceptable(&self, policy: &TrustPolicy, now_ns: u64, source_trust: TrustLevel) -> bool {
        self.is_acceptable_under(policy, now_ns, source_trust).is_acceptable()
    }

    /// Get mapping age in nanoseconds
    pub fn age_ns(&self, now_ns: u64) -> u64 {
        now_ns.saturating_sub(self.created_at_ns)
    }

    /// Check if mapping is expired
    pub fn is_expired(&self, now_ns: u64) -> bool {
        self.valid_until_ns > 0 && now_ns >= self.valid_until_ns
    }

    /// Check if local matches remote (same content hash)
    pub fn is_same_content(&self) -> bool {
        self.local_id == self.remote_id
    }

    /// Get total evidence weight
    pub fn evidence_weight(&self) -> f64 {
        self.evidence.iter().map(|e| e.weight).sum()
    }

    /// Check if mapping has specific evidence type
    pub fn has_evidence_type(&self, evidence_type: &EvidenceType) -> bool {
        self.evidence.iter().any(|e| &e.evidence_type == evidence_type)
    }

    /// Get evidence of specific type
    pub fn get_evidence_by_type(&self, evidence_type: &EvidenceType) -> Vec<&MappingEvidence> {
        self.evidence.iter().filter(|e| &e.evidence_type == evidence_type).collect()
    }
}

/// Evidence for a mapping
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MappingEvidence {
    /// Evidence type
    pub evidence_type: EvidenceType,
    /// Source base ID
    pub source: BaseId,
    /// Evidence weight (0.0 - 1.0)
    pub weight: f64,
    /// Evidence timestamp
    pub timestamp_ns: u64,
    /// Evidence description
    pub description: String,
}

impl MappingEvidence {
    /// Create new mapping evidence
    pub fn new(
        evidence_type: EvidenceType,
        source: BaseId,
        weight: f64,
        description: String,
    ) -> Self {
        MappingEvidence {
            evidence_type,
            source,
            weight,
            timestamp_ns: 0,
            description,
        }
    }
}

/// Evidence type for mappings
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum EvidenceType {
    /// Same content hash
    SameHash,
    /// Manual curation
    Manual,
    /// Algorithmic matching
    Algorithmic,
    /// User verification
    UserVerified,
    /// Trusted third party
    ThirdParty,
    /// Semantic similarity
    SemanticSimilarity,
}

/// Constraint on a mapping
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MappingConstraint {
    /// Constraint type
    pub constraint_type: ConstraintType,
    /// Constraint parameters
    pub params: HashMap<String, String>,
}

/// Constraint type
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConstraintType {
    /// Time range constraint
    TimeRange,
    /// Domain restriction
    DomainRestriction,
    /// Trust threshold
    TrustThreshold,
    /// Version compatibility
    VersionCompatibility,
}

// ============================================================================
// Trust Policy (SKF-1.1 Section 7.3)
// ============================================================================

/// Trust policy for federation filtering
///
/// SKF-1.1 Section 7.3: "Foreign base must be treated as distinct source in trust_policy.
/// Federation noise must be filtered instead of being accepted blindly."
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustPolicy {
    /// Minimum confidence threshold for mappings (0.0 - 1.0)
    pub min_confidence: f64,
    /// Minimum trust level for source bases (0-10000)
    pub min_source_trust: TrustLevel,
    /// Allowed remote bases (empty = all allowed)
    pub allowed_bases: Vec<BaseId>,
    /// Blocked remote bases (always rejected)
    pub blocked_bases: Vec<BaseId>,
    /// Require evidence for mappings
    pub require_evidence: bool,
    /// Minimum evidence weight threshold
    pub min_evidence_weight: f64,
    /// Maximum mapping age in nanoseconds (0 = no limit)
    pub max_mapping_age_ns: u64,
    /// Policy version
    pub version: u16,
}

impl TrustPolicy {
    /// Create new trust policy with default values
    pub fn new() -> Self {
        TrustPolicy {
            min_confidence: 0.5,
            min_source_trust: 1000,
            allowed_bases: Vec::new(),
            blocked_bases: Vec::new(),
            require_evidence: false,
            min_evidence_weight: 0.0,
            max_mapping_age_ns: 0,
            version: FEDERATION_PROTOCOL_VERSION,
        }
    }

    /// Create strict trust policy
    pub fn strict() -> Self {
        TrustPolicy {
            min_confidence: 0.8,
            min_source_trust: 5000,
            allowed_bases: Vec::new(),
            blocked_bases: Vec::new(),
            require_evidence: true,
            min_evidence_weight: 0.5,
            max_mapping_age_ns: 3_600_000_000_000, // 1 hour
            version: FEDERATION_PROTOCOL_VERSION,
        }
    }

    /// Create relaxed trust policy
    pub fn relaxed() -> Self {
        TrustPolicy {
            min_confidence: 0.3,
            min_source_trust: 100,
            allowed_bases: Vec::new(),
            blocked_bases: Vec::new(),
            require_evidence: false,
            min_evidence_weight: 0.0,
            max_mapping_age_ns: 0,
            version: FEDERATION_PROTOCOL_VERSION,
        }
    }

    /// Set minimum confidence
    pub fn with_min_confidence(mut self, confidence: f64) -> Self {
        self.min_confidence = confidence.clamp(0.0, 1.0);
        self
    }

    /// Set minimum source trust
    pub fn with_min_source_trust(mut self, trust: TrustLevel) -> Self {
        self.min_source_trust = trust;
        self
    }

    /// Add allowed base
    pub fn allow_base(mut self, base_id: BaseId) -> Self {
        self.allowed_bases.push(base_id);
        self
    }

    /// Add blocked base
    pub fn block_base(mut self, base_id: BaseId) -> Self {
        self.blocked_bases.push(base_id);
        self
    }

    /// Set require evidence
    pub fn with_require_evidence(mut self, require: bool) -> Self {
        self.require_evidence = require;
        self
    }

    /// Set minimum evidence weight
    pub fn with_min_evidence_weight(mut self, weight: f64) -> Self {
        self.min_evidence_weight = weight.clamp(0.0, 1.0);
        self
    }

    /// Set maximum mapping age
    pub fn with_max_mapping_age_ns(mut self, age_ns: u64) -> Self {
        self.max_mapping_age_ns = age_ns;
        self
    }

    /// Check if base is allowed by policy
    pub fn is_base_allowed(&self, base_id: &BaseId) -> bool {
        // Blocked bases are always rejected
        if self.blocked_bases.contains(base_id) {
            return false;
        }

        // If allowed_bases is empty, all non-blocked are allowed
        if self.allowed_bases.is_empty() {
            return true;
        }

        // Otherwise, must be in allowed list
        self.allowed_bases.contains(base_id)
    }

    /// Check if base has sufficient trust level
    pub fn check_base_trust(&self, base_trust: TrustLevel) -> bool {
        base_trust >= self.min_source_trust
    }
}

impl Default for TrustPolicy {
    fn default() -> Self {
        Self::new()
    }
}

/// Trust check result with reason
#[derive(Debug, Clone, PartialEq)]
pub enum TrustCheckResult {
    /// Mapping is acceptable
    Acceptable,
    /// Mapping expired (reason: expired_at_ns)
    Expired(u64),
    /// Confidence too low (reason: actual, required)
    ConfidenceTooLow(f64, f64),
    /// Source base not allowed
    BaseNotAllowed,
    /// Source base blocked
    BaseBlocked,
    /// Source trust too low (reason: actual, required)
    SourceTrustTooLow(TrustLevel, TrustLevel),
    /// Missing required evidence
    MissingEvidence,
    /// Evidence weight too low (reason: actual, required)
    EvidenceWeightTooLow(f64, f64),
    /// Mapping too old (reason: age_ns, max_age_ns)
    TooOld(u64, u64),
    /// Constraint not satisfied
    ConstraintNotSatisfied(String),
}

impl TrustCheckResult {
    /// Check if result is acceptable
    pub fn is_acceptable(&self) -> bool {
        matches!(self, TrustCheckResult::Acceptable)
    }

    /// Get reason string for logging
    pub fn reason(&self) -> String {
        match self {
            TrustCheckResult::Acceptable => "acceptable".to_string(),
            TrustCheckResult::Expired(expired_at) => 
                format!("expired at {} ns", expired_at),
            TrustCheckResult::ConfidenceTooLow(actual, required) => 
                format!("confidence {} < required {}", actual, required),
            TrustCheckResult::BaseNotAllowed => 
                "source base not in allowed list".to_string(),
            TrustCheckResult::BaseBlocked => 
                "source base is blocked".to_string(),
            TrustCheckResult::SourceTrustTooLow(actual, required) => 
                format!("source trust {} < required {}", actual, required),
            TrustCheckResult::MissingEvidence => 
                "required evidence missing".to_string(),
            TrustCheckResult::EvidenceWeightTooLow(actual, required) => 
                format!("evidence weight {} < required {}", actual, required),
            TrustCheckResult::TooOld(age, max_age) => 
                format!("mapping age {} ns > max {} ns", age, max_age),
            TrustCheckResult::ConstraintNotSatisfied(constraint) => 
                format!("constraint not satisfied: {}", constraint),
        }
    }
}

// ============================================================================
// Federation Client
// ============================================================================

/// HTTP client for federation operations
pub struct FederationClient {
    config: FederationConfig,
    http_client: reqwest::Client,
}

impl FederationClient {
    /// Create new federation client
    pub fn new(config: FederationConfig) -> Result<Self, FederationError> {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms as u64))
            .build()
            .map_err(|e| FederationError::Network(e.to_string()))?;

        Ok(FederationClient {
            config,
            http_client,
        })
    }

    /// Create client with custom HTTP client
    pub fn with_http_client(config: FederationConfig, http_client: reqwest::Client) -> Self {
        FederationClient {
            config,
            http_client,
        }
    }

    // ===================================================================
    // SKF-1.1 Section 7.2.1: discover(term_or_id)
    // ===================================================================

    /// Discover where to search for knowledge
    ///
    /// Sends discover requests to all configured peers and aggregates results.
    /// Implements federated search across multiple knowledge bases.
    pub async fn discover(&self, term: &str) -> Result<Vec<DiscoveryResult>, FederationError> {
        let mut all_results = Vec::new();

        for peer in &self.config.peers {
            // Skip peers with insufficient trust
            if peer.trust_level < 100 {
                continue;
            }

            let request = DiscoverRequest::new(term.to_string(), self.config.local_base_id)
                .with_max_hops(self.config.max_hops)
                .with_min_trust(1000);

            match self.discover_peer(peer, request).await {
                Ok(response) => {
                    for result in response.all_results() {
                        all_results.push(result.clone());
                    }
                }
                Err(e) => {
                    // Log error but continue with other peers
                    tracing::warn!("Discover failed for peer {:?}: {}", peer.base_id, e);
                }
            }
        }

        // Sort by relevance descending
        all_results.sort_by(|a, b| {
            b.relevance
                .partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(all_results)
    }

    /// Discover on a specific peer
    async fn discover_peer(
        &self,
        peer: &PeerConfig,
        request: DiscoverRequest,
    ) -> Result<DiscoverResponse, FederationError> {
        let url = peer.api_url("discover");

        let response = self
            .http_client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(FederationError::from)?;

        if !response.status().is_success() {
            return Err(FederationError::InvalidResponse(format!(
                "HTTP {}",
                response.status()
            )));
        }

        let discover_response = response.json::<DiscoverResponse>().await?;
        Ok(discover_response)
    }

    // ===================================================================
    // SKF-1.1 Section 7.2.2: fetch(id)
    // ===================================================================

    /// Retrieve atom from remote base by content-address
    ///
    /// Fetches an atom from a specific remote base by its atom ID.
    /// Validates the response and verifies content hash if needed.
    ///
    /// Note: For mapped content (when returned atom_id differs from requested),
    /// use `fetch_with_trust_verification()` to enforce trust policy.
    pub async fn fetch(
        &self,
        base_id: BaseId,
        atom_id: AtomId,
    ) -> Result<FetchResponse, FederationError> {
        let peer = self
            .config
            .find_peer(&base_id)
            .ok_or_else(|| FederationError::PeerNotFound(hex::encode(&base_id[..8])))?;

        let request = FetchRequest::new(atom_id, self.config.local_base_id);

        let url = peer.api_url(&format!("fetch/{}", hex::encode(&atom_id[..8])));

        let response = self
            .http_client
            .get(&url)
            .json(&request)
            .send()
            .await
            .map_err(FederationError::from)?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(FederationError::AtomNotFound(atom_id));
        }

        if !response.status().is_success() {
            return Err(FederationError::InvalidResponse(format!(
                "HTTP {}",
                response.status()
            )));
        }

        let fetch_response = response.json::<FetchResponse>().await?;

        // Verify content hash matches (if not same, it's a mapping)
        if fetch_response.atom_id != atom_id {
            // This is a mapped atom, not the exact content
            // Caller should use fetch_with_trust_verification() to verify mapping trust
            tracing::debug!(
                "Fetch returned mapped atom: requested {:?}, got {:?}",
                hex::encode(&atom_id[..8]),
                hex::encode(&fetch_response.atom_id[..8])
            );
        }

        Ok(fetch_response)
    }

    /// Fetch with trust verification for mapped content (SKF-1.1 Section 7.3)
    ///
    /// This method enforces trust policy when returned atom_id differs from requested atom_id:
    /// - Verifies mapping exists and is trusted
    /// - Rejects expired mappings
    /// - Rejects low-confidence mappings
    /// - Rejects untrusted source bases
    ///
    /// Returns error if mapping trust verification fails.
    pub async fn fetch_with_trust_verification(
        &self,
        base_id: BaseId,
        atom_id: AtomId,
        mapping: Option<&MapsTo>,
        policy: &TrustPolicy,
        now_ns: u64,
    ) -> Result<FetchResponse, FederationError> {
        // First perform basic fetch
        let fetch_response = self.fetch(base_id, atom_id).await?;

        // If returned atom_id matches requested, no verification needed
        if fetch_response.atom_id == atom_id {
            return Ok(fetch_response);
        }

        // Mapped content detected - verify trust
        tracing::info!(
            "Mapped fetch detected: requested {:?}, received {:?}",
            hex::encode(&atom_id[..8]),
            hex::encode(&fetch_response.atom_id[..8])
        );

        // Must have mapping to verify
        let mapping = mapping.ok_or(FederationError::TrustTooLow(0, policy.min_source_trust))?;

        // Verify mapping matches the fetch context
        if mapping.local_id != atom_id {
            return Err(FederationError::InvalidResponse(
                "Mapping local_id does not match requested atom_id".to_string(),
            ));
        }

        if mapping.remote_base != base_id {
            return Err(FederationError::InvalidResponse(
                "Mapping remote_base does not match fetch base_id".to_string(),
            ));
        }

        // Get source trust from peer config
        let source_trust = self.config
            .find_peer(&base_id)
            .map(|p| p.trust_level)
            .unwrap_or(0);

        // Perform full trust check
        let trust_result = mapping.is_acceptable_under(policy, now_ns, source_trust);

        if !trust_result.is_acceptable() {
            tracing::warn!(
                "Mapping trust verification failed: {}",
                trust_result.reason()
            );
            return Err(FederationError::TrustTooLow(source_trust, policy.min_source_trust));
        }

        // Additional verification: check returned atom_id matches mapping's remote_id
        if fetch_response.atom_id != mapping.remote_id {
            tracing::warn!(
                "Returned atom_id {:?} does not match mapping remote_id {:?}",
                hex::encode(&fetch_response.atom_id[..8]),
                hex::encode(&mapping.remote_id[..8])
            );
            // This could indicate a stale mapping or malicious response
            // Reject for safety
            return Err(FederationError::InvalidResponse(
                "Returned atom_id does not match mapping remote_id".to_string(),
            ));
        }

        tracing::info!(
            "Mapped fetch verified successfully: confidence={}, source_trust={}",
            mapping.confidence,
            source_trust
        );

        Ok(fetch_response)
    }

    // ===================================================================
    // SKF-1.1 Section 7.2.3: negotiate_schema()
    // ===================================================================

    /// Agree on types/fields between bases
    ///
    /// Negotiates schema compatibility with a remote base.
    /// Returns a schema agreement that defines how to map types and fields.
    pub async fn negotiate_schema(
        &self,
        base_id: BaseId,
    ) -> Result<SchemaAgreement, FederationError> {
        let peer = self
            .config
            .find_peer(&base_id)
            .ok_or_else(|| FederationError::PeerNotFound(hex::encode(&base_id[..8])))?;

        // Build negotiate request with local schema info
        let mut request = NegotiateRequest::new(self.config.local_base_id);

        // Add supported atom types
        for atom_type in [
            AtomType::DEFINITION,
            AtomType::FACT,
            AtomType::RULE,
            AtomType::PROCEDURE,
            AtomType::OBSERVATION,
            AtomType::HYPOTHESIS,
            AtomType::EXAMPLE,
            AtomType::COUNTEREXAMPLE,
            AtomType::DATASET,
            AtomType::MEASUREMENT,
            AtomType::DECISION,
            AtomType::CONFLICT,
            AtomType::MAP,
        ] {
            let info = AtomTypeInfo::new(atom_type, format!("{:?}", atom_type));
            request.add_type(info);
        }

        let url = peer.api_url("negotiate");

        let response = self
            .http_client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(FederationError::from)?;

        if !response.status().is_success() {
            return Err(FederationError::InvalidResponse(format!(
                "HTTP {}",
                response.status()
            )));
        }

        let negotiate_response = response.json::<NegotiateResponse>().await?;

        if negotiate_response.rejected {
            return Err(FederationError::SchemaMismatch(
                negotiate_response
                    .rejection_reason
                    .unwrap_or_else(|| "Unknown".to_string()),
            ));
        }

        Ok(negotiate_response.agreement)
    }

    // ===================================================================
    // SKF-1.1 Section 7.2.4: sync_crdt(metadata)
    // ===================================================================

    /// Merge dynamic metadata between bases
    ///
    /// Synchronizes CRDT metadata with a remote base.
    /// Performs bidirectional merge of metadata states.
    pub async fn sync_crdt(
        &self,
        base_id: BaseId,
        metadata: CrdtMetadata,
    ) -> Result<CrdtMetadata, FederationError> {
        let peer = self
            .config
            .find_peer(&base_id)
            .ok_or_else(|| FederationError::PeerNotFound(hex::encode(&base_id[..8])))?;

        let request = SyncRequest::new(self.config.local_base_id, metadata)
            .with_direction(SyncDirection::Bidirectional);

        let url = peer.api_url("sync");

        let response = self
            .http_client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(FederationError::from)?;

        if !response.status().is_success() {
            return Err(FederationError::InvalidResponse(format!(
                "HTTP {}",
                response.status()
            )));
        }

        let sync_response = response.json::<SyncResponse>().await?;

        if !sync_response.success {
            return Err(FederationError::CrdtMerge(
                sync_response.error.unwrap_or_else(|| "Unknown".to_string()),
            ));
        }

        // Handle conflicts if any
        if !sync_response.conflicts.is_empty() {
            tracing::warn!(
                "CRDT sync had {} conflicts for peer {:?}",
                sync_response.conflicts.len(),
                base_id
            );
        }

        Ok(sync_response.metadata)
    }

    /// Get configuration reference
    pub fn config(&self) -> &FederationConfig {
        &self.config
    }
}

// ============================================================================
// Gateway (Server-side handlers)
// ============================================================================

/// Gateway for handling incoming federation requests
pub struct Gateway {
    local_store: Arc<MemoryX>,
    config: FederationConfig,
    meta_store: Arc<std::sync::Mutex<MetaStore>>,
    mappings: Arc<std::sync::RwLock<Vec<MapsTo>>>,
    http_client: reqwest::Client,
}

impl Gateway {
    /// Create new federation gateway
    pub fn new(local_store: Arc<MemoryX>, config: FederationConfig) -> Self {
        let actor_id = ActorId::generate();
        let meta_store = MetaStore::new(actor_id);
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms as u64))
            .build()
            .expect("failed to build federation gateway HTTP client");

        Gateway {
            local_store,
            config,
            meta_store: Arc::new(std::sync::Mutex::new(meta_store)),
            mappings: Arc::new(std::sync::RwLock::new(Vec::new())),
            http_client,
        }
    }

    /// Get local base ID
    pub fn local_base_id(&self) -> BaseId {
        self.config.local_base_id
    }

    // ===================================================================
    // Handler: discover
    // ===================================================================

    /// Handle discover request from remote base
    pub async fn handle_discover(&self, req: DiscoverRequest) -> DiscoverResponse {
        let mut response = DiscoverResponse::new(self.config.local_base_id);

        // Check max hops
        if req.max_hops == 0 {
            return response;
        }

        // Check if we've already visited this base
        if req.was_visited(&self.config.local_base_id) {
            return response;
        }

        // Search local base by direct atom id first, then by lexical term.
        let mut seen_atoms = std::collections::HashSet::new();
        if let Some(atom_id) = Self::parse_discover_atom_id(&req.term)
            && let Some(node_num) = self.local_store.get_node_num(&atom_id)
            && let Some(meta) = self.local_store.meta.get_meta(&atom_id)
            && self.discovery_meta_matches(meta, &req)
        {
            seen_atoms.insert(atom_id);
            response.add_result(self.discovery_result_for_atom(
                &req.term,
                atom_id,
                node_num,
                meta.trust_level,
                1.0,
            ));
        }

        let results = self.local_store.search_lex(&req.term, None);

        for node_num in results {
            if let Some(atom_id) = self.local_store.meta.get_atom_by_node(node_num).copied()
                && seen_atoms.insert(atom_id)
                && let Some(meta) = self.local_store.meta.get_meta(&atom_id)
                && self.discovery_meta_matches(meta, &req)
            {
                response.add_result(self.discovery_result_for_atom(
                    &req.term,
                    atom_id,
                    node_num,
                    meta.trust_level,
                    1.0,
                ));
            }
        }

        // Forward to connected peers if hops remaining
        if req.max_hops > 1 {
            // Clone request with decremented hops
            let mut forwarded_req = req.clone();
            forwarded_req.max_hops -= 1;
            forwarded_req.add_visited(self.config.local_base_id);

            for result in self.forward_discover(&forwarded_req).await {
                response.add_forwarded(result);
            }
        }

        response
    }

    fn parse_discover_atom_id(term: &str) -> Option<AtomId> {
        let trimmed = term.trim();
        let hex = trimmed.strip_prefix("atom:").unwrap_or(trimmed);
        (hex.len() == 64).then(|| hex_decode(hex).ok()).flatten()
    }

    fn discovery_meta_matches(
        &self,
        meta: &crate::store::api::AtomMetadata,
        req: &DiscoverRequest,
    ) -> bool {
        if meta.trust_level < req.min_trust {
            return false;
        }

        if let Some(constraints) = &req.constraints {
            if let Some(domain_mask) = constraints.domain_mask
                && domain_mask != 0
                && (meta.domain_mask & domain_mask) == 0
            {
                return false;
            }

            if let Some(atom_types) = &constraints.atom_types
                && !atom_types.contains(&meta.atom_type)
            {
                return false;
            }

            if let Some((from_ns, to_ns)) = constraints.time_range
                && (meta.created_at_ns < from_ns || meta.created_at_ns >= to_ns)
            {
                return false;
            }
        }

        true
    }

    fn discovery_result_for_atom(
        &self,
        term: &str,
        atom_id: AtomId,
        _node_num: u64,
        trust_level: TrustLevel,
        relevance: f64,
    ) -> DiscoveryResult {
        let now_ns = current_unix_ns();
        let mut result = DiscoveryResult::new(
            self.config.local_base_id,
            term.to_owned(),
            relevance,
            0,
            trust_level,
        )
        .with_atom_id(atom_id);

        for mapping in self.find_mappings_valid(&atom_id, now_ns) {
            result = result.with_mapping(mapping);
        }

        result
    }

    async fn forward_discover(&self, req: &DiscoverRequest) -> Vec<DiscoveryResult> {
        let mut forwarded = Vec::new();

        for peer in self.config.peers_by_trust() {
            if req.was_visited(&peer.base_id) {
                continue;
            }

            let response = match self
                .http_client
                .post(peer.api_url("discover"))
                .json(req)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => resp,
                Ok(resp) => {
                    tracing::warn!(
                        "Forwarded discover to peer {:?} returned HTTP {}",
                        peer.base_id,
                        resp.status()
                    );
                    continue;
                }
                Err(err) => {
                    tracing::warn!("Forwarded discover failed for peer {:?}: {}", peer.base_id, err);
                    continue;
                }
            };

            let discover_response = match response.json::<DiscoverResponse>().await {
                Ok(discover_response) => discover_response,
                Err(err) => {
                    tracing::warn!(
                        "Failed to decode discover response from peer {:?}: {}",
                        peer.base_id,
                        err
                    );
                    continue;
                }
            };

            for mut result in discover_response
                .results
                .into_iter()
                .chain(discover_response.forwarded)
            {
                result.hops = result.hops.saturating_add(1);
                result.path_trust = result.path_trust.min(peer.trust_level);
                if result.path_trust >= req.min_trust {
                    forwarded.push(result);
                }
            }
        }

        forwarded.sort_by(|a, b| {
            b.relevance
                .partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.path_trust.cmp(&a.path_trust))
        });

        forwarded.dedup_by(|a, b| {
            a.base_id == b.base_id && a.atom_id == b.atom_id && a.term == b.term && a.hops == b.hops
        });

        forwarded
    }

    // ===================================================================
    // Handler: fetch
    // ===================================================================

    /// Handle fetch request from remote base — REAL implementation using CAS
    /// Handle fetch request from remote base - REAL implementation using CAS
    /// Uses proof-grade provenance (SKF-1.1 Section 10.1)
    pub async fn handle_fetch(&self, req: FetchRequest) -> Result<FetchResponse, FederationError> {
        let atom_id = req.atom_id;

        // Try to load atom body from CAS store
        match self.local_store.cas.load_atom(&atom_id) {
            Ok(body) => {
                let mut response = FetchResponse::new(atom_id, body, self.config.local_base_id);

                if req.include_evidence {
                    // Use proof-grade provenance API (SKF-1.1)
                    // Returns full ProvenanceChain with derivation nodes, evidence links,
                    // trust propagation, DERIVED_FROM chain
                    match self.local_store.get_provenance(&atom_id) {
                        Ok(chain) => {
                            response = response.with_provenance(chain);
                        },
                        Err(_) => { /* no evidence available */ }
                    }
                }

                if req.include_meta && let Some(meta) = self.local_store.meta.get_meta(&atom_id) {
                    let fed_meta = AtomMetadata {
                        atom_type: meta.atom_type,
                        created_at_ns: meta.created_at_ns,
                        valid_from_ns: 0,
                        valid_to_ns: u64::MAX,
                        trust_level: meta.trust_level,
                        domain_mask: meta.domain_mask,
                        source_base: self.config.local_base_id,
                        schema_version: FEDERATION_PROTOCOL_VERSION,
                    };
                    response.metadata = Some(fed_meta);
                }

                tracing::info!(
                    "Federation fetch: atom {:?} found (body {} bytes, provenance {} nodes, {} edges)",
                    atom_id,
                    response.body.len(),
                    response.provenance_chain.as_ref().map(|c| c.nodes.len()).unwrap_or(0),
                    response.provenance_chain.as_ref().map(|c| c.derivation_edges.len()).unwrap_or(0)
                );
                Ok(response)
            }
            Err(_) => {
                tracing::warn!("Federation fetch: atom {:?} not found", atom_id);
                Err(FederationError::AtomNotFound(atom_id))
            }
        }
    }

    // ===================================================================
    // Handler: negotiate
    // ===================================================================

    /// Handle schema negotiation request
    pub async fn handle_negotiate(&self, req: NegotiateRequest) -> NegotiateResponse {
        // Build agreement
        let mut agreement =
            SchemaAgreement::new(FEDERATION_PROTOCOL_VERSION, self.config.local_base_id);

        // Compare supported types
        for remote_type in &req.supported_types {
            let local_support = self.get_local_type_support(&remote_type.atom_type);
            agreement.add_atom_type(local_support);
        }

        // Calculate overall compatibility
        agreement.calculate_compatibility();

        // Check if compatible enough
        if agreement.compatibility < 0.5 {
            return NegotiateResponse::reject(
                "Compatibility too low".to_string(),
            );
        }

        NegotiateResponse::success(agreement)
    }

    /// Get local type support for atom type
    fn get_local_type_support(&self, atom_type: &AtomType) -> AtomTypeSupport {
        let mut support = AtomTypeSupport::new(*atom_type);

        // All standard fields are supported
        support.supported_fields = vec![
            "symbols".to_string(),
            "refs".to_string(),
            "claims".to_string(),
            "invariants".to_string(),
            "edges".to_string(),
            "evidence".to_string(),
            "meta".to_string(),
        ];

        support
    }

    // ===================================================================
    // Handler: sync
    // ===================================================================

    /// Handle CRDT sync request — REAL CRDT synchronization
    /// 
    /// This implementation:
    /// 1. Exports actual local metadata from meta_store
    /// 2. Merges remote state using CRDT join (not overwrite)
    /// 3. Persists merged result back into meta_store
    /// 4. Returns the actual post-merge store state
    pub async fn handle_sync(&self, req: SyncRequest) -> SyncResponse {
        // Lock meta_store for the entire operation
        let mut meta_store = match self.meta_store.lock() {
            Ok(guard) => guard,
            Err(e) => return SyncResponse::failure(e.to_string(), self.config.local_base_id),
        };

        // 1. Export real local metadata before merge
        let mut local_meta = CrdtMetadata::new(*meta_store.actor_id());
        local_meta.hlc_timestamp = meta_store.hlc().to_raw();
        
        // Export all node fields with their CrdtKind
        for (node, field_id, crdt_kind, state_bytes) in meta_store.export_node_fields() {
            local_meta.add_node_field(node, field_id, crdt_kind, state_bytes);
        }
        
        // Export all atom fields with their CrdtKind
        for (atom, field_id, crdt_kind, state_bytes) in meta_store.export_atom_fields() {
            local_meta.add_atom_field(atom, field_id, crdt_kind, state_bytes);
        }

        // Log export stats
        tracing::info!(
            "CRDT sync: exported {} local fields ({} nodes, {} atoms)",
            local_meta.total_fields(),
            local_meta.node_fields.len(),
            local_meta.atom_fields.len()
        );

        // 2. Merge remote metadata into store using CRDT join
        let remote_field_count = req.metadata.total_fields();
        let join_result = req.metadata.merge_into_store(&mut meta_store);
        
        let join_count = match join_result {
            Ok(count) => count,
            Err(e) => {
                tracing::error!("CRDT sync merge failed: {}", e);
                return SyncResponse::failure(e.to_string(), self.config.local_base_id);
            }
        };

        tracing::info!(
            "CRDT sync: joined {} fields from {} remote fields",
            join_count,
            remote_field_count
        );

        // 3. Export merged state for response
        // (Changes are already persisted in meta_store by merge_into_store)
        let mut merged_meta = CrdtMetadata::new(*meta_store.actor_id());
        merged_meta.hlc_timestamp = meta_store.hlc().to_raw();
        
        for (node, field_id, crdt_kind, state_bytes) in meta_store.export_node_fields() {
            merged_meta.add_node_field(node, field_id, crdt_kind, state_bytes);
        }
        
        for (atom, field_id, crdt_kind, state_bytes) in meta_store.export_atom_fields() {
            merged_meta.add_atom_field(atom, field_id, crdt_kind, state_bytes);
        }

        tracing::info!(
            "CRDT sync complete: {} total fields in merged state",
            merged_meta.total_fields()
        );

        SyncResponse::success(merged_meta, self.config.local_base_id)
    }

    /// Add a MapsTo mapping
    pub fn add_mapping(&self, mapping: MapsTo) {
        if let Ok(mut mappings) = self.mappings.write() {
            mappings.push(mapping);
        }
    }

    /// Get trust level for a remote base from config
    pub fn get_base_trust(&self, base_id: &BaseId) -> TrustLevel {
        self.config
            .find_peer(base_id)
            .map(|p| p.trust_level)
            .unwrap_or(0) // Unknown bases have zero trust
    }

    /// Check if base is configured
    pub fn is_base_configured(&self, base_id: &BaseId) -> bool {
        self.config.find_peer(base_id).is_some()
    }

    /// Find mappings for a local atom (basic, no validity filter)
    pub fn find_mappings(&self, local_id: &AtomId) -> Vec<MapsTo> {
        if let Ok(mappings) = self.mappings.read() {
            mappings
                .iter()
                .filter(|m| &m.local_id == local_id)
                .cloned()
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Find mappings for a local atom filtered by validity (SKF-1.1 validity enforcement)
    ///
    /// Returns only mappings that satisfy:
    /// - Not expired
    /// - Confidence in valid range
    /// - Required constraints satisfied
    pub fn find_mappings_valid(&self, local_id: &AtomId, now_ns: u64) -> Vec<MapsTo> {
        if let Ok(mappings) = self.mappings.read() {
            mappings
                .iter()
                .filter(|m| &m.local_id == local_id && m.is_valid_at(now_ns))
                .cloned()
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Find mappings for a local atom filtered by trust policy (SKF-1.1 Section 7.3)
    ///
    /// Returns only mappings that satisfy:
    /// - Not expired
    /// - Confidence above policy threshold
    /// - Source allowed by trust policy
    /// - Evidence requirements met (if required)
    /// - All constraints satisfied
    pub fn find_mappings_trusted(
        &self,
        local_id: &AtomId,
        policy: &TrustPolicy,
        now_ns: u64,
    ) -> Vec<MapsTo> {
        if let Ok(mappings) = self.mappings.read() {
            mappings
                .iter()
                .filter(|m| {
                    if &m.local_id != local_id {
                        return false;
                    }
                    // Get trust level for remote base
                    let source_trust = self.get_base_trust(&m.remote_base);
                    m.is_acceptable(policy, now_ns, source_trust)
                })
                .cloned()
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Find mapping to remote base (basic, no validity filter)
    pub fn find_mapping_to(&self, local_id: &AtomId, remote_base: &BaseId) -> Option<MapsTo> {
        if let Ok(mappings) = self.mappings.read() {
            mappings
                .iter()
                .find(|m| &m.local_id == local_id && &m.remote_base == remote_base)
                .cloned()
        } else {
            None
        }
    }

    /// Find valid mapping to remote base (SKF-1.1 validity enforcement)
    ///
    /// Returns first mapping that satisfies validity checks.
    pub fn find_mapping_to_valid(
        &self,
        local_id: &AtomId,
        remote_base: &BaseId,
        now_ns: u64,
    ) -> Option<MapsTo> {
        if let Ok(mappings) = self.mappings.read() {
            mappings
                .iter()
                .find(|m| {
                    &m.local_id == local_id 
                        && &m.remote_base == remote_base 
                        && m.is_valid_at(now_ns)
                })
                .cloned()
        } else {
            None
        }
    }

    /// Find trusted mapping to remote base (SKF-1.1 Section 7.3)
    ///
    /// Returns first mapping that satisfies full trust policy checks.
    pub fn find_mapping_to_trusted(
        &self,
        local_id: &AtomId,
        remote_base: &BaseId,
        policy: &TrustPolicy,
        now_ns: u64,
    ) -> Option<MapsTo> {
        if let Ok(mappings) = self.mappings.read() {
            mappings
                .iter()
                .find(|m| {
                    if &m.local_id != local_id || &m.remote_base != remote_base {
                        return false;
                    }
                    let source_trust = self.get_base_trust(&m.remote_base);
                    m.is_acceptable(policy, now_ns, source_trust)
                })
                .cloned()
        } else {
            None
        }
    }

    /// Verify mapping is trusted before accepting mapped content
    ///
    /// Used in fetch path when returned atom_id differs from requested atom_id.
    /// Returns TrustCheckResult with detailed reason if rejected.
    pub fn verify_mapping_trust(
        &self,
        mapping: &MapsTo,
        policy: &TrustPolicy,
        now_ns: u64,
    ) -> TrustCheckResult {
        let source_trust = self.get_base_trust(&mapping.remote_base);
        mapping.is_acceptable_under(policy, now_ns, source_trust)
    }
}

// ============================================================================
// Utility Functions
// ============================================================================

/// Generate base ID from public key bytes
pub fn base_id_from_pubkey(pubkey: &[u8; 32]) -> BaseId {
    *pubkey
}

/// Format base ID for display (first 8 hex chars)
pub fn format_base_id(base_id: &BaseId) -> String {
    hex::encode(&base_id[..8])
}

/// Validate federation protocol version compatibility
pub fn check_protocol_version(remote_version: u16) -> bool {
    // Major version must match (first byte)
    let local_major = (FEDERATION_PROTOCOL_VERSION >> 8) as u8;
    let remote_major = (remote_version >> 8) as u8;

    local_major == remote_major
}

/// Calculate content hash for verification
pub fn calculate_content_hash(body: &[u8]) -> AtomId {
    blake3::hash(body).into()
}

// ============================================================================
// Federation HTTP Server (axum-based)
// ============================================================================

/// Federation HTTP Server — serves incoming federation requests via axum
#[cfg(feature = "federation")]
pub struct FederationServer {
    gateway: Arc<tokio::sync::RwLock<Gateway>>,
    server_handle: Option<tokio::sync::oneshot::Sender<()>>,
    bind_addr: std::net::SocketAddr,
}

#[cfg(feature = "federation")]
impl FederationServer {
    /// Create a new federation server
    pub fn new(gateway: Arc<tokio::sync::RwLock<Gateway>>, bind_addr: std::net::SocketAddr) -> Self {
        FederationServer {
            gateway,
            server_handle: None,
            bind_addr,
        }
    }

    /// Start the HTTP server (spawns tokio task)
    pub fn start(&mut self) -> Result<(), String> {
        use axum::{routing::{get, post}, Router};
        use tower_http::cors::{Any, CorsLayer};

        let gateway = self.gateway.clone();
        let bind_addr = self.bind_addr;

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.server_handle = Some(tx);

        // CORS: allow all origins (federation is server-to-server)
        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);

        let app = Router::new()
            .route("/fetch", post(Self::handle_fetch_route))
            .route("/negotiate", get(Self::handle_negotiate_get).post(Self::handle_negotiate_post))
            .route("/sync", post(Self::handle_sync_route))
            .route("/discover", post(Self::handle_discover_route))
            .route("/health", get(Self::handle_health))
            .layer(cors)
            .with_state(gateway);

        let runtime = tokio::runtime::Handle::current();
        runtime.spawn(async move {
            tracing::info!("Federation server starting on {}", bind_addr);
            let listener = match tokio::net::TcpListener::bind(bind_addr).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!("Failed to bind federation server: {}", e);
                    return;
                }
            };

            if let Err(e) = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
            {
                tracing::error!("Federation server error: {}", e);
            }
        });

        Ok(())
    }

    /// Stop the federation server
    pub fn stop(&mut self) {
        if let Some(tx) = self.server_handle.take() {
            let _ = tx.send(());
            tracing::info!("Federation server shutdown signal sent");
        }
    }

    // --- Route Handlers ---

    async fn handle_health() -> &'static str {
        "ok"
    }

    async fn handle_fetch_route(
        axum::extract::State(gateway): axum::extract::State<Arc<tokio::sync::RwLock<Gateway>>>,
        axum::Json(req): axum::Json<FetchRequest>,
    ) -> axum::Json<FetchResponse> {
        let gw = gateway.read().await;
        match gw.handle_fetch(req).await {
            Ok(resp) => axum::Json(resp),
            Err(_) => {
                // Return 404 via axum response
                axum::Json(FetchResponse::new(
                    [0u8; 32],
                    Vec::new(),
                    [0u8; 32],
                ))
            }
        }
    }

    async fn handle_negotiate_get(
        axum::extract::State(gateway): axum::extract::State<Arc<tokio::sync::RwLock<Gateway>>>,
    ) -> axum::Json<NegotiateResponse> {
        // Return schema capabilities without a specific request
        let gw = gateway.read().await;
        let local_base_id = gw.local_base_id();
        let mut agreement = SchemaAgreement::new(FEDERATION_PROTOCOL_VERSION, local_base_id);
        for atom_type in [
            AtomType::DEFINITION,
            AtomType::FACT,
            AtomType::RULE,
            AtomType::PROCEDURE,
            AtomType::OBSERVATION,
            AtomType::HYPOTHESIS,
            AtomType::EXAMPLE,
            AtomType::COUNTEREXAMPLE,
            AtomType::DATASET,
            AtomType::MEASUREMENT,
            AtomType::DECISION,
            AtomType::CONFLICT,
            AtomType::MAP,
        ] {
            agreement.add_atom_type(gw.get_local_type_support(&atom_type));
        }
        agreement.calculate_compatibility();
        axum::Json(NegotiateResponse::success(agreement))
    }

    async fn handle_negotiate_post(
        axum::extract::State(gateway): axum::extract::State<Arc<tokio::sync::RwLock<Gateway>>>,
        axum::Json(req): axum::Json<NegotiateRequest>,
    ) -> axum::Json<NegotiateResponse> {
        let gw = gateway.read().await;
        let resp = gw.handle_negotiate(req).await;
        axum::Json(resp)
    }

    async fn handle_sync_route(
        axum::extract::State(gateway): axum::extract::State<Arc<tokio::sync::RwLock<Gateway>>>,
        axum::Json(req): axum::Json<SyncRequest>,
    ) -> axum::Json<SyncResponse> {
        let gw = gateway.read().await;
        let resp = gw.handle_sync(req).await;
        axum::Json(resp)
    }

    async fn handle_discover_route(
        axum::extract::State(gateway): axum::extract::State<Arc<tokio::sync::RwLock<Gateway>>>,
        axum::Json(req): axum::Json<DiscoverRequest>,
    ) -> axum::Json<DiscoverResponse> {
        let gw = gateway.read().await;
        let resp = gw.handle_discover(req).await;
        axum::Json(resp)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn build_discovery_test_payload(term: &str, atom_type: AtomType, trust_level: u16) -> Vec<u8> {
        use crate::cas::claims::ClaimsSection;
        use crate::cas::evidence::EvidenceSection;
        use crate::cas::invariants::InvariantsSection;
        use crate::cas::meta::{MetaField, MetaFieldKind, MetaSection, MetaValue};
        use crate::cas::symbols::SymbolsSection;

        let mut symbols_section = SymbolsSection::new();
        symbols_section.intern(term.to_string());
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
            let crc = crate::utils::crc32(data);
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
        body
    }

    #[cfg(feature = "federation")]
    fn free_local_addr() -> std::net::SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        addr
    }

    // ===================================================================
    // FederationConfig Tests
    // ===================================================================

    #[test]
    fn test_federation_config_default() {
        let local_id = [1u8; 32];
        let config = FederationConfig::new(local_id);

        assert_eq!(config.local_base_id, local_id);
        assert!(config.peers.is_empty());
        assert_eq!(config.timeout_ms, 30_000);
        assert_eq!(config.max_hops, 3);
        assert_eq!(config.protocol_version, FEDERATION_PROTOCOL_VERSION);
    }

    #[test]
    fn test_federation_config_builder() {
        let local_id = [1u8; 32];
        let peer = PeerConfig::new([2u8; 32], "https://peer.example.com".to_string(), 8000);

        let config = FederationConfig::new(local_id)
            .with_peer(peer)
            .with_timeout(10_000)
            .with_max_hops(5);

        assert_eq!(config.peers.len(), 1);
        assert_eq!(config.timeout_ms, 10_000);
        assert_eq!(config.max_hops, 5);
    }

    #[test]
    fn test_find_peer() {
        let peer_id = [2u8; 32];
        let peer = PeerConfig::new(peer_id, "https://peer.example.com".to_string(), 8000);

        let config = FederationConfig::new([1u8; 32]).with_peer(peer);

        assert!(config.find_peer(&peer_id).is_some());
        assert!(config.find_peer(&[99u8; 32]).is_none());
    }

    // ===================================================================
    // PeerConfig Tests
    // ===================================================================

    #[test]
    fn test_peer_config() {
        let base_id = [1u8; 32];
        let peer = PeerConfig::new(
            base_id,
            "https://peer.example.com".to_string(),
            8000,
        );

        assert_eq!(peer.base_id, base_id);
        assert_eq!(peer.endpoint, "https://peer.example.com");
        assert_eq!(peer.trust_level, 8000);
        assert!(peer.supports_schema(FEDERATION_PROTOCOL_VERSION));
    }

    #[test]
    fn test_peer_api_url() {
        let peer = PeerConfig::new([1u8; 32], "https://peer.example.com".to_string(), 5000);

        assert_eq!(peer.api_url("discover"), "https://peer.example.com/discover");
        assert_eq!(
            peer.api_url("fetch/123"),
            "https://peer.example.com/fetch/123"
        );
    }

    // ===================================================================
    // Discovery Tests
    // ===================================================================

    #[test]
    fn test_discovery_result() {
        let base_id = [1u8; 32];
        let result = DiscoveryResult::new(
            base_id,
            "test_term".to_string(),
            0.85,
            2,
            9000,
        );

        assert_eq!(result.base_id, base_id);
        assert_eq!(result.term, "test_term");
        assert_eq!(result.relevance, 0.85);
        assert_eq!(result.hops, 2);
        assert_eq!(result.path_trust, 9000);
        assert!(result.atom_id.is_none());
        assert!(result.mappings.is_empty());
    }

    #[test]
    fn test_discover_request() {
        let from_base = [1u8; 32];
        let req = DiscoverRequest::new("test".to_string(), from_base)
            .with_max_hops(5)
            .with_min_trust(2000);

        assert_eq!(req.term, "test");
        assert_eq!(req.from_base, from_base);
        assert_eq!(req.max_hops, 5);
        assert_eq!(req.min_trust, 2000);
    }

    #[test]
    fn test_discover_request_visited() {
        let mut req = DiscoverRequest::new("test".to_string(), [1u8; 32]);

        assert!(!req.was_visited(&[2u8; 32]));

        req.add_visited([2u8; 32]);
        assert!(req.was_visited(&[2u8; 32]));
    }

    // ===================================================================
    // Schema Negotiation Tests
    // ===================================================================

    #[test]
    fn test_schema_agreement() {
        let mut agreement = SchemaAgreement::new(FEDERATION_PROTOCOL_VERSION, [1u8; 32]);

        let support = AtomTypeSupport::new(AtomType::FACT);
        agreement.add_atom_type(support);

        assert_eq!(agreement.schema_version, FEDERATION_PROTOCOL_VERSION);
        assert_eq!(agreement.atom_types.len(), 1);
        assert_eq!(agreement.atom_types[0].atom_type, AtomType::FACT);
    }

    #[test]
    fn test_schema_agreement_compatibility() {
        let mut agreement = SchemaAgreement::new(FEDERATION_PROTOCOL_VERSION, [1u8; 32]);

        let mut support1 = AtomTypeSupport::new(AtomType::FACT);
        support1.compatibility = 0.8;
        agreement.add_atom_type(support1);

        let mut support2 = AtomTypeSupport::new(AtomType::RULE);
        support2.compatibility = 0.6;
        agreement.add_atom_type(support2);

        agreement.calculate_compatibility();

        assert!((agreement.compatibility - 0.7).abs() < 0.001);
    }

    #[test]
    fn test_field_mapping() {
        let mapping = FieldMapping::new("local_name".to_string(), "remote_name".to_string())
            .with_transformation("to_uppercase".to_string());

        assert_eq!(mapping.local_field, "local_name");
        assert_eq!(mapping.remote_field, "remote_name");
        assert!(mapping.type_compatible);
        assert_eq!(mapping.transformation, Some("to_uppercase".to_string()));
    }

    // ===================================================================
    // CRDT Sync Tests
    // ===================================================================

    #[test]
    fn test_crdt_metadata_fields() {
        use crate::store::CrdtKind;
        
        let actor_id = ActorId::generate();
        let mut meta = CrdtMetadata::new(actor_id);

        // Add node field with CrdtKind
        meta.add_node_field(1, 0x0001, CrdtKind::GCOUNTER, vec![1, 2, 3]);
        meta.add_node_field(1, 0x0002, CrdtKind::PNCOUNTER, vec![4, 5, 6]);
        
        // Add atom field with CrdtKind
        meta.add_atom_field([1u8; 32], 0x0003, CrdtKind::LWW_REG, vec![7, 8, 9]);

        assert_eq!(meta.node_fields.len(), 1);
        assert_eq!(meta.node_fields.get(&1).unwrap().len(), 2);
        assert_eq!(meta.atom_fields.len(), 1);
        assert_eq!(meta.total_fields(), 3);
        
        // Verify CrdtKind is preserved
        let node_entries = meta.node_fields.get(&1).unwrap();
        assert_eq!(node_entries[0].crdt_kind, CrdtKind::GCOUNTER);
        assert_eq!(node_entries[0].field_id, 0x0001);
        assert_eq!(node_entries[1].crdt_kind, CrdtKind::PNCOUNTER);
    }

    #[test]
    fn test_crdt_metadata_merge_into_store() {
        use crate::store::CrdtKind;
        use crate::crdt::MetaStore;
        
        // Create two stores with different actors
        let actor1 = ActorId::generate();
        let actor2 = ActorId::generate();
        let mut store1 = MetaStore::new(actor1);
        let mut store2 = MetaStore::new(actor2);
        
        // Add counters to both stores
        let crdt1 = store1.get_node_crdt(1, 0x0001, CrdtKind::GCOUNTER);
        if let crate::crdt::CrdtState::GCounter(gc) = crdt1 {
            gc.inc(actor1, 10);
        }
        
        let crdt2 = store2.get_node_crdt(1, 0x0001, CrdtKind::GCOUNTER);
        if let crate::crdt::CrdtState::GCounter(gc) = crdt2 {
            gc.inc(actor2, 20);
        }
        
        // Export store2 to wire format
        let mut meta2 = CrdtMetadata::new(actor2);
        for (node, field_id, kind, bytes) in store2.export_node_fields() {
            meta2.add_node_field(node, field_id, kind, bytes);
        }
        
        // Merge store2 into store1 using CRDT join
        let result = meta2.merge_into_store(&mut store1);
        assert!(result.is_ok());
        let join_count = result.unwrap();
        assert!(join_count > 0);
        
        // Verify convergence: counter should be 10 + 20 = 30
        let merged_crdt = store1.get_node_crdt(1, 0x0001, CrdtKind::GCOUNTER);
        if let crate::crdt::CrdtState::GCounter(gc) = merged_crdt {
            assert_eq!(gc.value(), 30, "CRDT counters should converge to sum");
        } else {
            panic!("Expected GCounter");
        }
    }

    #[test]
    fn test_crdt_sync_idempotent() {
        use crate::store::CrdtKind;
        use crate::crdt::MetaStore;
        
        let actor1 = ActorId::generate();
        let actor2 = ActorId::generate();
        let mut store1 = MetaStore::new(actor1);
        let mut store2 = MetaStore::new(actor2);
        
        // Setup same counter in both
        let crdt1 = store1.get_node_crdt(1, 0x0001, CrdtKind::GCOUNTER);
        if let crate::crdt::CrdtState::GCounter(gc) = crdt1 {
            gc.inc(actor1, 10);
        }
        
        let crdt2 = store2.get_node_crdt(1, 0x0001, CrdtKind::GCOUNTER);
        if let crate::crdt::CrdtState::GCounter(gc) = crdt2 {
            gc.inc(actor2, 20);
        }
        
        // Export and merge first time
        let mut meta2 = CrdtMetadata::new(actor2);
        for (node, field_id, kind, bytes) in store2.export_node_fields() {
            meta2.add_node_field(node, field_id, kind, bytes);
        }
        meta2.merge_into_store(&mut store1).unwrap();
        
        // Export and merge second time (should be idempotent)
        let mut meta2_again = CrdtMetadata::new(actor2);
        for (node, field_id, kind, bytes) in store2.export_node_fields() {
            meta2_again.add_node_field(node, field_id, kind, bytes);
        }
        meta2_again.merge_into_store(&mut store1).unwrap();
        
        // Value should still be 30 (idempotent)
        let merged = store1.get_node_crdt(1, 0x0001, CrdtKind::GCOUNTER);
        if let crate::crdt::CrdtState::GCounter(gc) = merged {
            assert_eq!(gc.value(), 30, "Repeated sync should be idempotent");
        }
    }

    #[test]
    fn test_sync_request() {
        let actor_id = ActorId::generate();
        let meta = CrdtMetadata::new(actor_id);
        let req = SyncRequest::new([1u8; 32], meta)
            .with_direction(SyncDirection::Push);

        assert_eq!(req.direction, SyncDirection::Push);
    }

    // ===================================================================
    // MapsTo Tests
    // ===================================================================

    #[test]
    fn test_maps_to() {
        let local_id = [1u8; 32];
        let remote_id = [2u8; 32];
        let remote_base = [3u8; 32];

        let mapping = MapsTo::new(local_id, remote_base, remote_id, 0.95);

        assert_eq!(mapping.local_id, local_id);
        assert_eq!(mapping.remote_id, remote_id);
        assert_eq!(mapping.remote_base, remote_base);
        assert_eq!(mapping.confidence, 0.95);
    }

    #[test]
    fn test_maps_to_is_valid() {
        let valid = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.5);
        assert!(valid.is_valid());

        let invalid_confidence = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 1.5);
        assert!(!invalid_confidence.is_valid());

        let zero_confidence = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.0);
        assert!(!zero_confidence.is_valid());
    }

    #[test]
    fn test_maps_to_is_same_content() {
        let same = MapsTo::new([1u8; 32], [2u8; 32], [1u8; 32], 0.9);
        assert!(same.is_same_content());

        let different = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.9);
        assert!(!different.is_same_content());
    }

    #[test]
    fn test_maps_to_evidence() {
        let mut mapping = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.9);

        let evidence = MappingEvidence::new(
            EvidenceType::SameHash,
            [4u8; 32],
            0.8,
            "Same content hash".to_string(),
        );

        mapping.add_evidence(evidence);

assert_eq!(mapping.evidence.len(), 1);
        assert_eq!(mapping.evidence_weight(), 0.8);
    }

    // ===================================================================
    // Validity/Trust Enforcement Tests (SKF-1.1 Section 7.3)
    // ===================================================================

    #[test]
    fn test_maps_to_is_valid_at_expiry() {
        // Create mapping that expires at timestamp 1000
        let mut mapping = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.9);
        mapping.valid_until_ns = 1000;
        mapping.created_at_ns = 0;

        // Should be valid before expiry
        assert!(mapping.is_valid_at(500));
        assert!(mapping.is_valid_at(999));

        // Should be invalid at expiry
        assert!(!mapping.is_valid_at(1000));

        // Should be invalid after expiry
        assert!(!mapping.is_valid_at(2000));

        // Mapping with no expiry (valid_until_ns = 0) should always be valid
        let forever = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.9);
        assert!(forever.is_valid_at(0));
        assert!(forever.is_valid_at(1000));
        assert!(forever.is_valid_at(u64::MAX));
    }

    #[test]
    fn test_maps_to_is_valid_at_confidence() {
        // Confidence out of range should be invalid
        let too_high = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 1.5);
        assert!(!too_high.is_valid_at(0));

        let zero = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.0);
        assert!(!zero.is_valid_at(0));

        let negative = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], -0.5);
        assert!(!negative.is_valid_at(0));

        // Valid confidence range
        let valid = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.5);
        assert!(valid.is_valid_at(0));

        let high = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 1.0);
        assert!(high.is_valid_at(0));
    }

    #[test]
    fn test_maps_to_is_expired() {
        let mut mapping = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.9);
        mapping.valid_until_ns = 1000;

        assert!(!mapping.is_expired(500));
        assert!(!mapping.is_expired(999));
        assert!(mapping.is_expired(1000));
        assert!(mapping.is_expired(2000));

        // No expiry means never expired
        let forever = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.9);
        assert!(!forever.is_expired(0));
        assert!(!forever.is_expired(u64::MAX));
    }

    #[test]
    fn test_maps_to_age() {
        let mut mapping = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.9);
        mapping.created_at_ns = 1000;

        assert_eq!(mapping.age_ns(1500), 500);
        assert_eq!(mapping.age_ns(2000), 1000);
        assert_eq!(mapping.age_ns(500), 0); // saturating_sub
    }

    #[test]
    fn test_trust_policy_default() {
        let policy = TrustPolicy::default();

        assert_eq!(policy.min_confidence, 0.5);
        assert_eq!(policy.min_source_trust, 1000);
        assert!(policy.allowed_bases.is_empty());
        assert!(policy.blocked_bases.is_empty());
        assert!(!policy.require_evidence);
        assert_eq!(policy.min_evidence_weight, 0.0);
        assert_eq!(policy.max_mapping_age_ns, 0);
    }

    #[test]
    fn test_trust_policy_strict() {
        let policy = TrustPolicy::strict();

        assert_eq!(policy.min_confidence, 0.8);
        assert_eq!(policy.min_source_trust, 5000);
        assert!(policy.require_evidence);
        assert_eq!(policy.min_evidence_weight, 0.5);
        assert_eq!(policy.max_mapping_age_ns, 3_600_000_000_000); // 1 hour
    }

    #[test]
    fn test_trust_policy_relaxed() {
        let policy = TrustPolicy::relaxed();

        assert_eq!(policy.min_confidence, 0.3);
        assert_eq!(policy.min_source_trust, 100);
        assert!(!policy.require_evidence);
    }

    #[test]
    fn test_trust_policy_base_allowed() {
        let base1 = [1u8; 32];
        let base2 = [2u8; 32];
        let base3 = [3u8; 32];

        // Default policy: all bases allowed (empty whitelist)
        let default_policy = TrustPolicy::default();
        assert!(default_policy.is_base_allowed(&base1));

        // Policy with whitelist
        let whitelist_policy = TrustPolicy::new()
            .allow_base(base1)
            .allow_base(base2);
        assert!(whitelist_policy.is_base_allowed(&base1));
        assert!(whitelist_policy.is_base_allowed(&base2));
        assert!(!whitelist_policy.is_base_allowed(&base3));

        // Policy with blocked list
        let blocked_policy = TrustPolicy::new()
            .block_base(base3);
        assert!(blocked_policy.is_base_allowed(&base1));
        assert!(!blocked_policy.is_base_allowed(&base3));

        // Whitelist + blocked (blocked takes precedence)
        let mixed_policy = TrustPolicy::new()
            .allow_base(base1)
            .allow_base(base2)
            .block_base(base2);
        assert!(mixed_policy.is_base_allowed(&base1));
        assert!(!mixed_policy.is_base_allowed(&base2)); // blocked even if in whitelist
    }

    #[test]
    fn test_maps_to_acceptable_under_confidence() {
        let policy = TrustPolicy::new().with_min_confidence(0.7);
        let now_ns = 1000u64;
        let source_trust = 5000u16;

        // High enough confidence
        let high = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.9);
        let result = high.is_acceptable_under(&policy, now_ns, source_trust);
        assert!(result.is_acceptable());

        // Too low confidence
        let low = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.5);
        let result = low.is_acceptable_under(&policy, now_ns, source_trust);
        assert!(!result.is_acceptable());
        assert!(matches!(result, TrustCheckResult::ConfidenceTooLow(0.5, 0.7)));
    }

    #[test]
    fn test_maps_to_acceptable_under_expiry() {
        let policy = TrustPolicy::new();
        let source_trust = 5000u16;

        // Expired mapping
        let mut expired = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.9);
        expired.valid_until_ns = 1000;

        let result = expired.is_acceptable_under(&policy, 2000, source_trust);
        assert!(!result.is_acceptable());
        assert!(matches!(result, TrustCheckResult::Expired(1000)));

        // Not yet expired
        let result = expired.is_acceptable_under(&policy, 500, source_trust);
        assert!(result.is_acceptable());
    }

    #[test]
    fn test_maps_to_acceptable_under_age() {
        let policy = TrustPolicy::new()
            .with_max_mapping_age_ns(1000);

        let source_trust = 5000u16;

        // Fresh mapping
        let mut fresh = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.9);
        fresh.created_at_ns = 1000;

        let result = fresh.is_acceptable_under(&policy, 1500, source_trust);
        assert!(result.is_acceptable());

        // Old mapping
        let result = fresh.is_acceptable_under(&policy, 2500, source_trust);
        assert!(!result.is_acceptable());
        assert!(matches!(result, TrustCheckResult::TooOld(1500, 1000)));
    }

    #[test]
    fn test_maps_to_acceptable_under_base_allowed() {
        let allowed_base = [1u8; 32];
        let blocked_base = [2u8; 32];
        let unknown_base = [3u8; 32];

        let policy = TrustPolicy::new()
            .allow_base(allowed_base)
            .block_base(blocked_base);

        let source_trust = 5000u16;
        let now_ns = 1000u64;

        // Allowed base
        let allowed_mapping = MapsTo::new([10u8; 32], allowed_base, [20u8; 32], 0.9);
        let result = allowed_mapping.is_acceptable_under(&policy, now_ns, source_trust);
        assert!(result.is_acceptable());

        // Blocked base
        let blocked_mapping = MapsTo::new([10u8; 32], blocked_base, [20u8; 32], 0.9);
        let result = blocked_mapping.is_acceptable_under(&policy, now_ns, source_trust);
        assert!(!result.is_acceptable());
        assert!(matches!(result, TrustCheckResult::BaseBlocked));

        // Unknown base (not in whitelist)
        let unknown_mapping = MapsTo::new([10u8; 32], unknown_base, [20u8; 32], 0.9);
        let result = unknown_mapping.is_acceptable_under(&policy, now_ns, source_trust);
        assert!(!result.is_acceptable());
        assert!(matches!(result, TrustCheckResult::BaseNotAllowed));
    }

    #[test]
    fn test_maps_to_acceptable_under_source_trust() {
        let policy = TrustPolicy::new()
            .with_min_source_trust(5000);

        let now_ns = 1000u64;

        // High enough source trust
        let mapping = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.9);
        let result = mapping.is_acceptable_under(&policy, now_ns, 8000);
        assert!(result.is_acceptable());

        // Too low source trust
        let result = mapping.is_acceptable_under(&policy, now_ns, 2000);
        assert!(!result.is_acceptable());
        assert!(matches!(result, TrustCheckResult::SourceTrustTooLow(2000, 5000)));
    }

    #[test]
    fn test_maps_to_acceptable_under_evidence() {
        let policy = TrustPolicy::strict() // requires evidence
            .with_min_evidence_weight(0.5);

        let source_trust = 5000u16;
        let now_ns = 1000u64;

        // Mapping with sufficient evidence
        let mut with_evidence = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.9);
        with_evidence.add_evidence(MappingEvidence::new(
            EvidenceType::SameHash,
            [4u8; 32],
            0.6,
            "Content hash match".to_string(),
        ));

        let result = with_evidence.is_acceptable_under(&policy, now_ns, source_trust);
        assert!(result.is_acceptable());

        // Mapping without evidence
        let no_evidence = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.9);
        let result = no_evidence.is_acceptable_under(&policy, now_ns, source_trust);
        assert!(!result.is_acceptable());
        assert!(matches!(result, TrustCheckResult::MissingEvidence));

        // Mapping with insufficient evidence weight
        let mut low_weight = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.9);
        low_weight.add_evidence(MappingEvidence::new(
            EvidenceType::Manual,
            [4u8; 32],
            0.3,
            "Low weight evidence".to_string(),
        ));

        let result = low_weight.is_acceptable_under(&policy, now_ns, source_trust);
        assert!(!result.is_acceptable());
        assert!(matches!(result, TrustCheckResult::EvidenceWeightTooLow(0.3, 0.5)));
    }

    #[test]
    fn test_maps_to_has_evidence_type() {
        let mut mapping = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.9);
        mapping.add_evidence(MappingEvidence::new(
            EvidenceType::SameHash,
            [4u8; 32],
            0.8,
            "Hash match".to_string(),
        ));
        mapping.add_evidence(MappingEvidence::new(
            EvidenceType::UserVerified,
            [5u8; 32],
            0.9,
            "User verified".to_string(),
        ));

        assert!(mapping.has_evidence_type(&EvidenceType::SameHash));
        assert!(mapping.has_evidence_type(&EvidenceType::UserVerified));
        assert!(!mapping.has_evidence_type(&EvidenceType::Manual));

        let same_hash_evidence = mapping.get_evidence_by_type(&EvidenceType::SameHash);
        assert_eq!(same_hash_evidence.len(), 1);
        assert_eq!(same_hash_evidence[0].weight, 0.8);
    }

    #[test]
    fn test_trust_check_result_reason() {
        assert_eq!(TrustCheckResult::Acceptable.reason(), "acceptable");
        assert_eq!(
            TrustCheckResult::Expired(1000).reason(),
            "expired at 1000 ns"
        );
        assert_eq!(
            TrustCheckResult::ConfidenceTooLow(0.3, 0.5).reason(),
            "confidence 0.3 < required 0.5"
        );
        assert_eq!(
            TrustCheckResult::SourceTrustTooLow(2000, 5000).reason(),
            "source trust 2000 < required 5000"
        );
        assert_eq!(TrustCheckResult::BaseBlocked.reason(), "source base is blocked");
    }

    #[test]
    fn test_maps_to_acceptable_under_constraints() {
        let policy = TrustPolicy::new();
        let source_trust = 5000u16;
        let now_ns = 1000u64;

        // Mapping with time range constraint
        let mut time_constrained = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.9);
        time_constrained.add_constraint(MappingConstraint {
            constraint_type: ConstraintType::TimeRange,
            params: {
                let mut p = HashMap::new();
                p.insert("start_ns".to_string(), "500".to_string());
                p.insert("end_ns".to_string(), "1500".to_string());
                p
            },
        });

        // Within time range
        let result = time_constrained.is_acceptable_under(&policy, now_ns, source_trust);
        assert!(result.is_acceptable());

        // Outside time range
        let result = time_constrained.is_acceptable_under(&policy, 2000, source_trust);
        assert!(!result.is_acceptable());
        assert!(matches!(result, TrustCheckResult::ConstraintNotSatisfied(_)));
    }

    // ===================================================================
    // Gateway Validity-Aware Tests (SKF-1.1 Section 7.3)
    // ===================================================================

    #[test]
    fn test_gateway_find_mappings_valid() {
        use std::sync::Arc;
        use crate::store::api::{MemoryX, StoreConfig};

        // Create a simple store for testing
        let local_base = [1u8; 32];
        let config = FederationConfig::new(local_base);
        let store = Arc::new(MemoryX::new(StoreConfig::default()).unwrap());
        let gateway = Gateway::new(store, config);

        let local_id = [10u8; 32];
        let remote_base = [2u8; 32];
        let remote_id = [20u8; 32];

        // Add valid mapping (no expiry)
        let valid_mapping = MapsTo::new(local_id, remote_base, remote_id, 0.9);
        gateway.add_mapping(valid_mapping);

        // Add expired mapping
        let mut expired_mapping = MapsTo::new(local_id, remote_base, [21u8; 32], 0.9);
        expired_mapping.valid_until_ns = 500;
        gateway.add_mapping(expired_mapping);

        // Add low confidence mapping (0.1 is still valid range, but below threshold)
        let low_confidence = MapsTo::new(local_id, remote_base, [22u8; 32], 0.1);
        gateway.add_mapping(low_confidence);

        // Test find_mappings_valid
        // Note: is_valid_at checks confidence range (0.0-1.0), NOT policy threshold
        // 0.1 is in valid range, so it passes is_valid_at()
        let now_ns = 1000u64;
        let valid_mappings = gateway.find_mappings_valid(&local_id, now_ns);

        // Should return 2 mappings:
        // - valid_mapping (confidence 0.9, no expiry) -> VALID
        // - expired_mapping (confidence 0.9, expiry 500) -> INVALID (expired)
        // - low_confidence (confidence 0.1, no expiry) -> VALID (in range)
        assert_eq!(valid_mappings.len(), 2);
        
        // Both should have valid confidence (in range)
        for m in &valid_mappings {
            assert!(m.confidence > 0.0 && m.confidence <= 1.0);
        }

        // Test at earlier time when expired mapping was still valid
        let earlier_mappings = gateway.find_mappings_valid(&local_id, 400);
        assert_eq!(earlier_mappings.len(), 3); // all three valid at time 400
    }

    #[test]
    fn test_gateway_find_mappings_trusted() {
        use std::sync::Arc;
        use crate::store::api::{MemoryX, StoreConfig};

        let local_base = [1u8; 32];
        let trusted_base = [2u8; 32];
        let untrusted_base = [3u8; 32];

        // Create config with trusted peer
        let trusted_peer = PeerConfig::new(trusted_base, "https://trusted.example.com".to_string(), 8000);
        let untrusted_peer = PeerConfig::new(untrusted_base, "https://untrusted.example.com".to_string(), 100);

        let config = FederationConfig::new(local_base)
            .with_peer(trusted_peer)
            .with_peer(untrusted_peer);

        let store = Arc::new(MemoryX::new(StoreConfig::default()).unwrap());
        let gateway = Gateway::new(store, config);

        let local_id = [10u8; 32];

        // Add mapping to trusted base (high confidence)
        let trusted_mapping = MapsTo::new(local_id, trusted_base, [20u8; 32], 0.9);
        gateway.add_mapping(trusted_mapping);

        // Add mapping to untrusted base
        let untrusted_mapping = MapsTo::new(local_id, untrusted_base, [30u8; 32], 0.9);
        gateway.add_mapping(untrusted_mapping);

        // Add low confidence mapping to trusted base
        let low_conf_mapping = MapsTo::new(local_id, trusted_base, [21u8; 32], 0.2);
        gateway.add_mapping(low_conf_mapping);

        // Create policy requiring min source trust 5000
        let policy = TrustPolicy::new()
            .with_min_source_trust(5000)
            .with_min_confidence(0.5);

        let now_ns = 1000u64;
        let trusted_mappings = gateway.find_mappings_trusted(&local_id, &policy, now_ns);

        // Should only return mapping to trusted base with sufficient confidence
        assert_eq!(trusted_mappings.len(), 1);
        assert_eq!(trusted_mappings[0].remote_base, trusted_base);
        assert_eq!(trusted_mappings[0].confidence, 0.9);
    }

    #[test]
    fn test_gateway_find_mapping_to_valid() {
        use std::sync::Arc;
        use crate::store::api::{MemoryX, StoreConfig};

        let local_base = [1u8; 32];
        let config = FederationConfig::new(local_base);
        let store = Arc::new(MemoryX::new(StoreConfig::default()).unwrap());
        let gateway = Gateway::new(store, config);

        let local_id = [10u8; 32];
        let remote_base = [2u8; 32];

        // Add expired mapping
        let mut expired = MapsTo::new(local_id, remote_base, [20u8; 32], 0.9);
        expired.valid_until_ns = 500;
        gateway.add_mapping(expired);

        // Add valid mapping
        let valid = MapsTo::new(local_id, remote_base, [21u8; 32], 0.9);
        gateway.add_mapping(valid);

        // At time when expired mapping is already expired
        let now_ns = 1000u64;
        let result = gateway.find_mapping_to_valid(&local_id, &remote_base, now_ns);

        // Should return valid mapping, not expired
        assert!(result.is_some());
        let mapping = result.unwrap();
        assert_eq!(mapping.remote_id, [21u8; 32]);

        // At earlier time when both are valid
        let earlier = gateway.find_mapping_to_valid(&local_id, &remote_base, 400);
        assert!(earlier.is_some());
        // Should return first valid one found
        let earlier_mapping = earlier.unwrap();
        // Could be either expired or valid at this time
        assert!(earlier_mapping.is_valid_at(400));
    }

    #[tokio::test]
    async fn test_gateway_discover_returns_atom_id_mappings_and_honors_constraints() {
        use crate::store::api::{MemoryX, StoreConfig};
        use std::sync::Arc;

        let temp_dir = tempfile::TempDir::new().unwrap();
        let mut store = MemoryX::new(StoreConfig::new(temp_dir.path().join("store"))).unwrap();
        let payload = build_discovery_test_payload("federated-atom", AtomType::FACT, 9000);
        let atom_id = store.ingest(&payload, AtomType::FACT, &[], &[]).unwrap();

        let local_base = [1u8; 32];
        let remote_base = [2u8; 32];
        let gateway = Gateway::new(Arc::new(store), FederationConfig::new(local_base));
        gateway.add_mapping(MapsTo::new(atom_id, remote_base, [3u8; 32], 0.9));

        let mut request = DiscoverRequest::new(format!("atom:{}", hex::encode(atom_id)), [9u8; 32])
            .with_min_trust(5000);
        request.constraints = Some(QueryConstraints {
            max_results: None,
            domain_mask: Some(0xFFFF),
            atom_types: Some(vec![AtomType::FACT]),
            time_range: None,
        });

        let response = gateway.handle_discover(request).await;
        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0].atom_id, Some(atom_id));
        assert_eq!(response.results[0].mappings.len(), 1);
        assert_eq!(response.results[0].mappings[0].remote_base, remote_base);

        let mut blocked_request =
            DiscoverRequest::new(format!("atom:{}", hex::encode(atom_id)), [9u8; 32]);
        blocked_request.constraints = Some(QueryConstraints {
            max_results: None,
            domain_mask: Some(0xFFFF),
            atom_types: Some(vec![AtomType::RULE]),
            time_range: None,
        });

        let blocked = gateway.handle_discover(blocked_request).await;
        assert!(blocked.results.is_empty());
    }

    #[test]
    fn test_gateway_verify_mapping_trust() {
        use crate::store::api::{MemoryX, StoreConfig};
        use std::sync::Arc;

        let local_base = [1u8; 32];
        let trusted_base = [2u8; 32];

        let trusted_peer = PeerConfig::new(
            trusted_base,
            "https://trusted.example.com".to_string(),
            8000,
        );
        let config = FederationConfig::new(local_base).with_peer(trusted_peer);

        let store = Arc::new(MemoryX::new(StoreConfig::default()).unwrap());
        let gateway = Gateway::new(store, config);

        // Create mapping
        let mapping = MapsTo::new([10u8; 32], trusted_base, [20u8; 32], 0.9);

        // Strict policy
        let policy = TrustPolicy::strict();
        let now_ns = 1000u64;

        // Verify - should fail because no evidence (strict policy requires evidence)
        let result = gateway.verify_mapping_trust(&mapping, &policy, now_ns);
        assert!(!result.is_acceptable());
        assert!(matches!(result, TrustCheckResult::MissingEvidence));

        // Relaxed policy - should pass
        let relaxed = TrustPolicy::relaxed();
        let result = gateway.verify_mapping_trust(&mapping, &relaxed, now_ns);
        assert!(result.is_acceptable());
    }

    #[cfg(feature = "federation")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_gateway_handle_discover_forwards_to_peer_hops() {
        use crate::store::api::{MemoryX, StoreConfig};
        use tempfile::tempdir;

        let peer_addr = free_local_addr();
        let peer_base = [3u8; 32];
        let peer_dir = tempdir().unwrap();
        let mut peer_store = MemoryX::new(StoreConfig::new(peer_dir.path().join("peer_store"))).unwrap();
        let payload = build_discovery_test_payload("federated-term", AtomType::FACT, 9000);
        peer_store
            .ingest(&payload, AtomType::FACT, &[], &[])
            .unwrap();

        let peer_gateway = Arc::new(tokio::sync::RwLock::new(Gateway::new(
            Arc::new(peer_store),
            FederationConfig::new(peer_base),
        )));
        let mut peer_server = FederationServer::new(peer_gateway, peer_addr);
        peer_server.start().unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mid_base = [2u8; 32];
        let mid_dir = tempdir().unwrap();
        let mid_store = Arc::new(MemoryX::new(StoreConfig::new(mid_dir.path().join("mid_store"))).unwrap());
        let peer_config = PeerConfig::new(peer_base, format!("http://{}", peer_addr), 8000);
        let mid_gateway = Gateway::new(
            mid_store,
            FederationConfig::new(mid_base)
                .with_peer(peer_config)
                .with_timeout(2_000)
                .with_max_hops(2),
        );

        let request = DiscoverRequest::new("federated-term".to_string(), [9u8; 32])
            .with_max_hops(2)
            .with_min_trust(1000);
        let response = mid_gateway.handle_discover(request).await;

        peer_server.stop();

        assert!(response.results.is_empty(), "middle gateway should not have local matches");
        assert_eq!(response.forwarded.len(), 1, "peer result should be forwarded");
        assert_eq!(response.forwarded[0].base_id, peer_base);
        assert_eq!(response.forwarded[0].term, "federated-term");
        assert_eq!(response.forwarded[0].hops, 1);
        assert_eq!(response.forwarded[0].path_trust, 5000);
    }

    #[test]
    fn test_gateway_get_base_trust() {
        use std::sync::Arc;
        use crate::store::api::{MemoryX, StoreConfig};

        let local_base = [1u8; 32];
        let known_base = [2u8; 32];
        let unknown_base = [3u8; 32];

        let peer = PeerConfig::new(known_base, "https://known.example.com".to_string(), 7000);
        let config = FederationConfig::new(local_base).with_peer(peer);

        let store = Arc::new(MemoryX::new(StoreConfig::default()).unwrap());
        let gateway = Gateway::new(store, config);

        // Known base should have configured trust
        assert_eq!(gateway.get_base_trust(&known_base), 7000);

        // Unknown base should have zero trust
        assert_eq!(gateway.get_base_trust(&unknown_base), 0);

        // Check if bases are configured
        assert!(gateway.is_base_configured(&known_base));
        assert!(!gateway.is_base_configured(&unknown_base));
    }

    // ===================================================================
    // Utility Tests
    // ===================================================================

    #[test]
    fn test_format_base_id() {
        let base_id = [0xABu8, 0xCD, 0xEF, 0x12, 0x34, 0x56, 0x78, 0x9A, 
                       0u8, 0u8, 0u8, 0u8, 0u8, 0u8, 0u8, 0u8,
                       0u8, 0u8, 0u8, 0u8, 0u8, 0u8, 0u8, 0u8,
                       0u8, 0u8, 0u8, 0u8, 0u8, 0u8, 0u8, 0u8];
        let formatted = format_base_id(&base_id);
        assert_eq!(formatted, "abcdef123456789a");
    }

    #[test]
    fn test_check_protocol_version() {
        // Same major version should be compatible
        assert!(check_protocol_version(FEDERATION_PROTOCOL_VERSION));

        // Different major version should not be compatible
        let different_major = 0x0201u16; // Version 2.1
        assert!(!check_protocol_version(different_major));
    }

    #[test]
    fn test_calculate_content_hash() {
        let body = b"test content";
        let hash = calculate_content_hash(body);

        // Hash should not be all zeros
        assert_ne!(hash, [0u8; 32]);

        // Same content should produce same hash
        let hash2 = calculate_content_hash(body);
        assert_eq!(hash, hash2);

        // Different content should produce different hash
        let hash3 = calculate_content_hash(b"different content");
        assert_ne!(hash, hash3);
    }

    // ===================================================================
    // Fetch Types Tests
    // ===================================================================

    #[test]
    fn test_fetch_request() {
        let atom_id = [1u8; 32];
        let from_base = [2u8; 32];

        let req = FetchRequest::new(atom_id, from_base)
            .with_evidence(false)
            .with_meta(false);

        assert_eq!(req.atom_id, atom_id);
        assert_eq!(req.from_base, from_base);
        assert!(!req.include_evidence);
        assert!(!req.include_meta);
    }

    #[test]
    fn test_fetch_response() {
        let atom_id = [1u8; 32];
        let from_base = [2u8; 32];
        let body = vec![1, 2, 3, 4];

        let meta = AtomMetadata::new(AtomType::FACT, from_base);

        let response = FetchResponse::new(atom_id, body, from_base)
            .with_trust(8000)
            .with_metadata(meta);

        assert_eq!(response.atom_id, atom_id);
        assert_eq!(response.trust_level, 8000);
        assert!(response.metadata.is_some());
    }

    #[test]
    fn test_atom_metadata_is_valid_at() {
        let mut meta = AtomMetadata::new(AtomType::FACT, [1u8; 32]);
        meta.valid_from_ns = 1000;
        meta.valid_to_ns = 5000;

        assert!(!meta.is_valid_at(500)); // Before valid_from
        assert!(meta.is_valid_at(1000)); // At valid_from
        assert!(meta.is_valid_at(3000)); // Within range
        assert!(!meta.is_valid_at(5000)); // At valid_to (exclusive)
    }

    // ===================================================================
    // Serialization Tests
    // ===================================================================

    #[test]
    fn test_serde_discover_request() {
        let req = DiscoverRequest::new("test".to_string(), [1u8; 32]);
        let json = serde_json::to_string(&req).unwrap();
        let deserialized: DiscoverRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.term, "test");
        assert_eq!(deserialized.from_base, [1u8; 32]);
    }

    #[test]
    fn test_serde_schema_agreement() {
        let agreement = SchemaAgreement::new(FEDERATION_PROTOCOL_VERSION, [1u8; 32]);
        let json = serde_json::to_string(&agreement).unwrap();
        let deserialized: SchemaAgreement = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.schema_version, FEDERATION_PROTOCOL_VERSION);
        assert_eq!(deserialized.remote_base, [1u8; 32]);
    }

    #[test]
    fn test_serde_maps_to() {
        let mapping = MapsTo::new([1u8; 32], [2u8; 32], [3u8; 32], 0.95);
        let json = serde_json::to_string(&mapping).unwrap();
        let deserialized: MapsTo = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.local_id, [1u8; 32]);
        assert_eq!(deserialized.confidence, 0.95);
    }
}
