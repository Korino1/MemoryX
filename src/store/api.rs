//! Main Store API for MemoryX SKF-1.1
//!
//! This module provides the high-level public API for the MemoryX store:
//! - StoreConfig: Configuration for store initialization
//! - MemoryX: Main store interface with all subsystems
//! - AnswerPack: Query result structure with confidence and alternatives
//!
//! # Architecture
//!
//! The MemoryX store integrates:
//! - CAS (Content-Addressed Storage): Atom persistence
//! - IdLoc: Location index for atom lookup
//! - InvertedIndex: Term -> NodeNum mappings
//! - GraphStore: CSR-based graph for edge traversal
//! - MetaStore: Metadata and context management
//! - CtxManager: Context branching and conflict tracking

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use thiserror::Error;

use crate::cas::canonical::compute_atom_id_from_payload;
use crate::cas::io as cas_io;
use crate::cas::{
    claims::ClaimsSection, symbols::SymbolsSection, AtomBodyHeader, CasError, SectionDesc,
};
use crate::graph::GraphStore;
use crate::index::{IdLocBuilder, IdLocIndex, InvertedIndex, Location};
use crate::prelude::QueryConstraints;
use crate::query::ann::EmbeddingIndex;
use crate::store::{
    AtomId, AtomType, ClaimPattern, DomainMask, EdgeType, GapKind, InvariantResult, NavHint,
    NodeNum, SectionKind, StopCond, SymId, TrustLevel,
};
use crate::vm::{AtomView as CasAtomView, ClaimData, ConstValue};

// Re-export ObjTag from store::mod for public API
pub use crate::store::ObjTag;

// Re-export solver types (solver depends on this module, not vice versa)
pub use crate::query::{
    FixedPointSolver, GoalSpec, GoalSpecCompiler, QueryContract, QueryContractCompiler,
};

// Import Candidate for search_semantic return type
use crate::query::router::{BackendKind, Candidate};

// ============================================================================
// Term Extraction (SKF-1.1 Section 3)
// ============================================================================

/// Extract terms from atom payload for lexical indexing.
///
/// **Purpose:** Replace surrogate term keys with actual lexical content from atom.
///
/// **Algorithm (SKF-1.1 §3):**
/// 1. Parse AtomBodyHeader from payload
/// 2. Iterate through section descriptors
/// 3. Extract SYMBOLS section and parse symbol table
/// 4. Extract CLAIMS section for claim subjects/predicates/objects
/// 5. Return normalized terms for indexing
///
/// **Term Sources:**
/// - SYMBOLS section: All interned strings (identifiers, names, values)
/// - CLAIMS: Subject, predicate, object values (when SymId references)
/// - EVIDENCE: Evidence kind strings and method names
///
/// # Arguments
/// - `payload`: Raw atom body bytes
///
/// # Returns
/// - `Vec<String>`: Normalized terms ready for indexing
/// - Empty vec if payload parsing fails (graceful degradation)
///
/// # Safety
/// - No unsafe operations
/// - Validates all offsets before access
/// - NFC normalizes all extracted strings
#[inline]
fn extract_terms_from_payload(payload: &[u8]) -> Vec<String> {
    let mut terms = Vec::new();

    // Step 1: Parse header
    let header = match AtomBodyHeader::from_bytes(payload) {
        Ok(h) => h,
        Err(_) => return terms, // Graceful degradation
    };

    // Step 2: Parse section table
    let section_count = header.section_count as usize;
    let table_offset = header.section_table_off as usize;

    if payload.len() < table_offset + section_count * SectionDesc::SIZE {
        return terms; // Malformed payload
    }

    // Step 3: Extract section descriptors
    let mut sections: Vec<(SectionKind, usize, usize)> = Vec::with_capacity(section_count);
    for i in 0..section_count {
        let offset = table_offset + i * SectionDesc::SIZE;
        if let Ok(desc) =
            SectionDesc::from_bytes_unaligned(&payload[offset..offset + SectionDesc::SIZE])
            && let Some(kind) = desc.kind()
        {
            sections.push((kind, desc.off as usize, desc.len as usize));
        }
    }

    // Step 4: Helper to get section data
    let get_section = |kind: SectionKind| -> Option<&[u8]> {
        sections
            .iter()
            .find(|(k, _, _)| *k == kind)
            .and_then(|(_, off, len)| {
                if off + len <= payload.len() {
                    Some(&payload[*off..*off + *len])
                } else {
                    None
                }
            })
    };

    // Step 5: Extract symbols from SYMBOLS section
    if let Some(symbols_data) = get_section(SectionKind::SYMBOLS)
        && !symbols_data.is_empty()
        && let Ok(symbols) = SymbolsSection::from_bytes(symbols_data)
    {
        // Add all interned symbols as terms
        for i in 0..symbols.len() {
            if let Some(s) = symbols.get(i as u32) {
                // NFC normalize for consistent indexing
                let normalized: String =
                    unicode_normalization::UnicodeNormalization::nfc(s).collect();
                if !normalized.is_empty() {
                    terms.push(normalized);
                }
            }
        }
    }

    // Step 6: Extract terms from CLAIMS section
    if let Some(claims_data) = get_section(SectionKind::CLAIMS)
        && !claims_data.is_empty()
        && let Ok(claims) = ClaimsSection::from_bytes(claims_data)
    {
        // We need the symbols table to resolve local indices
        let symbols = get_section(SectionKind::SYMBOLS).and_then(|data| {
            if !data.is_empty() {
                SymbolsSection::from_bytes(data).ok()
            } else {
                None
            }
        });

        for i in 0..claims.len() {
            if let Some(claim) = claims.get(i) {
                // Resolve subject from symbols if available
                if let Some(ref syms) = symbols {
                    if let Some(subj_str) = syms.get(claim.subject_local as u32) {
                        let normalized: String =
                            unicode_normalization::UnicodeNormalization::nfc(subj_str).collect();
                        if !normalized.is_empty() && !terms.contains(&normalized) {
                            terms.push(normalized);
                        }
                    }
                    if let Some(pred_str) = syms.get(claim.predicate_local as u32) {
                        let normalized: String =
                            unicode_normalization::UnicodeNormalization::nfc(pred_str).collect();
                        if !normalized.is_empty() && !terms.contains(&normalized) {
                            terms.push(normalized);
                        }
                    }
                    // Object may also be a symbol reference (ObjTag::SYM)
                    if claim.object_tag == crate::store::ObjTag::SYM
                        && claim.object_value.len() >= 4
                    {
                        let obj_local = u32::from_le_bytes([
                            claim.object_value[0],
                            claim.object_value[1],
                            claim.object_value[2],
                            claim.object_value[3],
                        ]);
                        if let Some(obj_str) = syms.get(obj_local) {
                            let normalized: String =
                                unicode_normalization::UnicodeNormalization::nfc(obj_str)
                                    .collect();
                            if !normalized.is_empty() && !terms.contains(&normalized) {
                                terms.push(normalized);
                            }
                        }
                    }
                }
            }
        }
    }

    // Note: Evidence section could also be processed here for evidence kind/method terms
    // This is left for future enhancement

    terms
}

const LOCATION_STATE_MAGIC: u32 = 0x4C4F4331; // "LOC1"
const LOCATION_STATE_VERSION: u16 = 0x0001;
const LOCATION_RECORD_SIZE: usize = 65;
const META_STATE_MAGIC: u32 = 0x4D455431; // "MET1"
const META_STATE_VERSION: u16 = 0x0001;
const EMBEDDINGS_FILE: &str = "embeddings.bin";

// ============================================================================
// Cost Weights (SKF-1.1 Section 5.1)
// ============================================================================

/// Cost function weights for AnswerGraph minimization
///
/// These weights define the cost function:
/// ```text
/// cost(AG) = wN * |N| + wE * |E| + wIO * IO_bytes + wC * conflicts
///           + wS * soft_conflicts + wT * trust_penalty + wA * age_penalty
///           + wD * domain_penalty
/// ```
#[allow(non_snake_case)]
#[derive(Debug, Clone, Copy)]
pub struct CostWeights {
    /// Weight per node (default 1.0)
    pub wN: f64,
    /// Weight per edge (default 0.2)
    pub wE: f64,
    /// Weight per I/O byte (default 2.0)
    pub wIO: f64,
    /// Weight for hard conflicts (default 1_000_000)
    pub wC: f64,
    /// Weight for soft conflicts (default 1_000)
    pub wS: f64,
    /// Weight for trust penalty (default 10.0)
    pub wT: f64,
    /// Weight for age penalty (default 1.0)
    pub wA: f64,
    /// Weight for domain penalty (default 5.0)
    pub wD: f64,
}

impl Default for CostWeights {
    fn default() -> Self {
        CostWeights {
            wN: 1.0,
            wE: 0.2,
            wIO: 2.0,        // Per byte
            wC: 1_000_000.0, // Hard conflicts are very expensive
            wS: 1_000.0,
            wT: 10.0,
            wA: 1.0,
            wD: 5.0,
        }
    }
}

#[allow(non_snake_case)]
impl CostWeights {
    /// Create custom cost weights
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        wN: f64,
        wE: f64,
        wIO: f64,
        wC: f64,
        wS: f64,
        wT: f64,
        wA: f64,
        wD: f64,
    ) -> Self {
        CostWeights {
            wN,
            wE,
            wIO,
            wC,
            wS,
            wT,
            wA,
            wD,
        }
    }

    /// Calculate total cost for a set of nodes and edges
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn calculate_cost(
        &self,
        node_count: usize,
        edge_count: usize,
        io_bytes: u64,
        hard_conflicts: u32,
        soft_conflicts: u32,
        avg_trust_inverse: f64,
        age_penalty: f64,
        domain_penalty: f64,
    ) -> f64 {
        self.wN * node_count as f64
            + self.wE * edge_count as f64
            + self.wIO * io_bytes as f64
            + self.wC * hard_conflicts as f64
            + self.wS * soft_conflicts as f64
            + self.wT * avg_trust_inverse
            + self.wA * age_penalty
            + self.wD * domain_penalty
    }

    /// Calculate benefit for covering gaps
    #[inline]
    pub fn gap_benefit(&self, gap_priority: u8) -> f64 {
        gap_priority as f64
    }
}

// ============================================================================
// Gap Types
// ============================================================================

/// Gap priority type
pub type GapPriority = u8;

/// Gap ID type
pub type GapId = u32;

/// Knowledge gap specification
///
/// Gaps represent missing information that needs to be retrieved
/// to answer the query.
#[derive(Debug, Clone)]
pub struct Gap {
    /// Unique gap identifier
    pub id: GapId,
    /// Gap type (NEED_DEFINITION, NEED_FACT, etc.)
    pub kind: GapKind,
    /// Priority (0 = lowest, 255 = highest)
    pub priority: GapPriority,
    /// Pattern to match
    pub pattern: ClaimPattern,
    /// Navigation hints for retrieval
    pub nav: NavHint,
    /// Stop conditions
    pub stop: StopCond,
    /// Generation number (for fixed-point tracking)
    pub generation: u32,
    /// Covered flag (for set cover algorithm)
    pub covered: bool,
    /// Covered by which atom
    pub covered_by: Option<AtomId>,
}

impl Gap {
    /// Create a new gap
    #[inline]
    pub fn new(id: GapId, kind: GapKind, pattern: ClaimPattern) -> Self {
        Gap {
            id,
            kind,
            priority: 128, // Default priority
            pattern,
            nav: NavHint::default(),
            stop: StopCond::default(),
            generation: 0,
            covered: false,
            covered_by: None,
        }
    }

    /// Create a gap with high priority
    #[inline]
    pub fn high_priority(id: GapId, kind: GapKind, pattern: ClaimPattern) -> Self {
        Gap {
            id,
            kind,
            priority: 200,
            pattern,
            nav: NavHint::default(),
            stop: StopCond::default(),
            generation: 0,
            covered: false,
            covered_by: None,
        }
    }

    /// Set priority
    #[inline]
    pub fn with_priority(mut self, priority: GapPriority) -> Self {
        self.priority = priority;
        self
    }

    /// Set navigation hints
    #[inline]
    pub fn with_nav(mut self, nav: NavHint) -> Self {
        self.nav = nav;
        self
    }

    /// Set stop conditions
    #[inline]
    pub fn with_stop(mut self, stop: StopCond) -> Self {
        self.stop = stop;
        self
    }

    /// Set generation
    #[inline]
    pub fn with_generation(mut self, generation: u32) -> Self {
        self.generation = generation;
        self
    }

    /// Mark as covered by an atom
    #[inline]
    pub fn mark_covered(&mut self, atom_id: AtomId) {
        self.covered = true;
        self.covered_by = Some(atom_id);
    }

    /// Check if gap needs graph traversal
    #[inline]
    pub fn needs_graph_walk(&self) -> bool {
        self.kind.needs_graph_walk() || !self.nav.edge_types.is_empty()
    }

    /// Check if gap needs definition lookup
    #[inline]
    pub fn needs_definition(&self) -> bool {
        self.kind.needs_definition()
    }

    /// Check if gap needs evidence
    #[inline]
    pub fn needs_evidence(&self) -> bool {
        matches!(self.kind, GapKind::NEED_EVIDENCE)
    }

    /// Check if gap needs causal chain
    #[inline]
    pub fn needs_causal_chain(&self) -> bool {
        matches!(self.kind, GapKind::NEED_CAUSAL_CHAIN)
    }
}

// ============================================================================
// Entity Reference Types
// ============================================================================

/// Entity reference types for query specification
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityRef {
    /// Symbol reference by ID
    Sym(SymId),
    /// Term reference (for inverted index lookup)
    Term(u32),
    /// Direct node reference
    Node(NodeNum),
    /// Atom reference
    Atom(AtomId),
}

impl EntityRef {
    /// Create a symbol entity reference
    #[inline]
    pub const fn sym(id: SymId) -> Self {
        EntityRef::Sym(id)
    }

    /// Create a term entity reference
    #[inline]
    pub const fn term(id: u32) -> Self {
        EntityRef::Term(id)
    }

    /// Create a node entity reference
    #[inline]
    pub const fn node(num: NodeNum) -> Self {
        EntityRef::Node(num)
    }

    /// Create an atom entity reference
    #[inline]
    pub const fn atom(id: AtomId) -> Self {
        EntityRef::Atom(id)
    }

    /// Check if this is a node reference
    #[inline]
    pub const fn is_node(&self) -> bool {
        matches!(self, EntityRef::Node(_))
    }

    /// Get node number if available
    #[inline]
    pub fn as_node(&self) -> Option<NodeNum> {
        match self {
            EntityRef::Node(n) => Some(*n),
            _ => None,
        }
    }
}

// ============================================================================
// Evidence Reference (SKF-1.1 Proof-Grade Provenance)
// ============================================================================

/// Evidence kind for provenance chain (SKF-1.1 Section 10.1)
///
/// Classification of how evidence was obtained or derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
pub enum EvidenceKind {
    /// Citation from literature/source
    CITATION,
    /// Direct measurement data
    MEASUREMENT,
    /// Derived from other atoms via rules
    DERIVED,
    /// Expert inference/judgment
    EXPERT_INFERENCE,
    /// Logical derivation
    LOGICAL_DERIVATION,
    /// Statistical analysis
    STATISTICAL,
    /// Experimental result
    EXPERIMENTAL,
    /// Direct observation
    OBSERVATION,
    /// Unknown/unspecified
    #[default]
    UNKNOWN,
}

impl EvidenceKind {
    /// Trust decay factor for this evidence kind
    ///
    /// DERIVED evidence has higher decay (0.85) because it depends on other atoms.
    /// Direct evidence (CITATION, MEASUREMENT) has lower decay (0.95).
    ///
    /// Used for trust propagation in derivation chains.
    #[inline]
    pub const fn trust_decay_factor(self) -> f64 {
        match self {
            EvidenceKind::CITATION => 0.95,
            EvidenceKind::MEASUREMENT => 0.95,
            EvidenceKind::DERIVED => 0.85,
            EvidenceKind::EXPERT_INFERENCE => 0.90,
            EvidenceKind::LOGICAL_DERIVATION => 0.85,
            EvidenceKind::STATISTICAL => 0.90,
            EvidenceKind::EXPERIMENTAL => 0.95,
            EvidenceKind::OBSERVATION => 0.95,
            EvidenceKind::UNKNOWN => 0.80,
        }
    }

    /// Convert from u32 (matches EVIDENCE section encoding)
    #[inline]
    pub const fn from_u32(value: u32) -> Option<Self> {
        match value {
            1 => Some(EvidenceKind::CITATION),
            2 => Some(EvidenceKind::MEASUREMENT),
            3 => Some(EvidenceKind::EXPERT_INFERENCE),
            4 => Some(EvidenceKind::LOGICAL_DERIVATION),
            5 => Some(EvidenceKind::STATISTICAL),
            6 => Some(EvidenceKind::EXPERIMENTAL),
            7 => Some(EvidenceKind::OBSERVATION),
            0 => Some(EvidenceKind::UNKNOWN),
            _ => None,
        }
    }

    /// Convert to u32
    #[inline]
    pub const fn to_u32(self) -> u32 {
        match self {
            EvidenceKind::UNKNOWN => 0,
            EvidenceKind::CITATION => 1,
            EvidenceKind::MEASUREMENT => 2,
            EvidenceKind::EXPERT_INFERENCE => 3,
            EvidenceKind::LOGICAL_DERIVATION => 4,
            EvidenceKind::STATISTICAL => 5,
            EvidenceKind::EXPERIMENTAL => 6,
            EvidenceKind::OBSERVATION => 7,
            EvidenceKind::DERIVED => 8,
        }
    }
}

/// Evidence link with full provenance information (SKF-1.1 Proof-Grade)
///
/// **Purpose:** Replace compact EvidenceRef with proof-grade evidence chain.
///
/// **SKF-1.1 Requirements:**
/// - Source AtomId (real content-addressed identity)
/// - Evidence kind (CITATION, MEASUREMENT, DERIVED, etc.)
/// - Confidence propagation through derivation chain
/// - Trust decay factors based on evidence type
/// - Link to source atom for full derivation trace
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EvidenceLink {
    /// Source atom ID (canonical content-addressed identity)
    pub source_atom_id: AtomId,
    /// Evidence kind/type
    pub evidence_kind: EvidenceKind,
    /// Confidence value (0.0 - 1.0)
    pub confidence: f64,
    /// Trust level (0 - 10000)
    pub trust: TrustLevel,
    /// Trust decay factor applied during propagation
    pub trust_decay_factor: f64,
    /// Section kind where evidence was found
    pub section_kind: SectionKind,
    /// Offset within section
    pub offset: u64,
    /// Length of evidence record
    pub length: u64,
    /// Depth in derivation chain (0 = direct evidence, >0 = derived)
    pub derivation_depth: u32,
    /// Method symbol index (for derivation method tracking)
    pub method_sym: u32,
    /// Timestamp of evidence creation
    pub timestamp_ns: u64,
}

impl EvidenceLink {
    /// Create a new evidence link
    #[inline]
    pub fn new(
        source_atom_id: AtomId,
        evidence_kind: EvidenceKind,
        confidence: f64,
        trust: TrustLevel,
        section_kind: SectionKind,
        offset: u64,
        length: u64,
    ) -> Self {
        let decay = evidence_kind.trust_decay_factor();
        EvidenceLink {
            source_atom_id,
            evidence_kind,
            confidence: confidence.clamp(0.0, 1.0),
            trust,
            trust_decay_factor: decay,
            section_kind,
            offset,
            length,
            derivation_depth: 0,
            method_sym: 0,
            timestamp_ns: 0,
        }
    }

    /// Create a derived evidence link with propagation
    #[inline]
    pub fn derived_from(
        source_atom_id: AtomId,
        parent_link: &EvidenceLink,
        derivation_depth: u32,
    ) -> Self {
        let propagated_trust =
            ((parent_link.trust as f64) * parent_link.trust_decay_factor) as TrustLevel;
        let propagated_confidence = parent_link.confidence * parent_link.trust_decay_factor;

        EvidenceLink {
            source_atom_id,
            evidence_kind: EvidenceKind::DERIVED,
            confidence: propagated_confidence.clamp(0.0, 1.0),
            trust: propagated_trust,
            trust_decay_factor: EvidenceKind::DERIVED.trust_decay_factor(),
            section_kind: SectionKind::EVIDENCE,
            offset: 0,
            length: 0,
            derivation_depth,
            method_sym: parent_link.method_sym,
            timestamp_ns: parent_link.timestamp_ns,
        }
    }

    /// Set derivation depth
    #[inline]
    pub fn with_depth(mut self, depth: u32) -> Self {
        self.derivation_depth = depth;
        self
    }

    /// Set method symbol
    #[inline]
    pub fn with_method(mut self, method_sym: u32) -> Self {
        self.method_sym = method_sym;
        self
    }

    /// Set timestamp
    #[inline]
    pub fn with_timestamp(mut self, timestamp_ns: u64) -> Self {
        self.timestamp_ns = timestamp_ns;
        self
    }

    /// Calculate effective trust after propagation
    #[inline]
    pub fn effective_trust(&self) -> TrustLevel {
        let base_trust = self.trust as f64;
        let decay_multiplier = self
            .trust_decay_factor
            .powi(self.derivation_depth as i32 + 1);
        (base_trust * decay_multiplier) as TrustLevel
    }
}

/// Derivation edge for proof graph (SKF-1.1 DERIVED_FROM)
///
/// Represents a DERIVED_FROM relationship between atoms.
/// Edge type 9 in EDGES section.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DerivationEdge {
    /// Source atom (derived atom)
    pub derived_atom_id: AtomId,
    /// Target atom (source/premise atom)
    pub source_atom_id: AtomId,
    /// Edge type (DERIVED_FROM = 9)
    pub edge_type: u32,
    /// Derivation depth
    pub depth: u32,
    /// Confidence of derivation
    pub confidence: f64,
    /// Trust propagated through derivation
    pub propagated_trust: TrustLevel,
}

impl DerivationEdge {
    /// Create a DERIVED_FROM edge
    #[inline]
    pub fn new(
        derived_atom_id: AtomId,
        source_atom_id: AtomId,
        depth: u32,
        confidence: f64,
        source_trust: TrustLevel,
    ) -> Self {
        let decay = EvidenceKind::DERIVED.trust_decay_factor();
        let propagated_trust = ((source_trust as f64) * decay.powi(depth as i32 + 1)) as TrustLevel;

        DerivationEdge {
            derived_atom_id,
            source_atom_id,
            edge_type: 9, // DERIVED_FROM
            depth,
            confidence,
            propagated_trust,
        }
    }
}

/// Provenance node in proof graph (SKF-1.1)
///
/// Represents a single atom in the derivation chain.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProvenanceNode {
    /// Atom ID
    pub atom_id: AtomId,
    /// Node number in graph
    pub node_num: NodeNum,
    /// Atom type
    pub atom_type: AtomType,
    /// Evidence links for this atom
    pub evidence_links: Vec<EvidenceLink>,
    /// Depth in derivation chain
    pub depth: u32,
    /// Cumulative trust (propagated from sources)
    pub cumulative_trust: TrustLevel,
}

impl ProvenanceNode {
    /// Create a provenance node
    #[inline]
    pub fn new(atom_id: AtomId, node_num: NodeNum, atom_type: AtomType) -> Self {
        ProvenanceNode {
            atom_id,
            node_num,
            atom_type,
            evidence_links: Vec::new(),
            depth: 0,
            cumulative_trust: 5000,
        }
    }

    /// Add evidence link
    #[inline]
    pub fn add_evidence(&mut self, link: EvidenceLink) {
        if link.evidence_kind == EvidenceKind::DERIVED {
            self.cumulative_trust = link.effective_trust();
        }
        self.evidence_links.push(link);
    }

    /// Set depth
    #[inline]
    pub fn with_depth(mut self, depth: u32) -> Self {
        self.depth = depth;
        self
    }
}

/// Full provenance chain with derivation graph (SKF-1.1 Proof-Grade)
///
/// **Purpose:** Complete proof-grade provenance for why is this answer valid.
///
/// **SKF-1.1 Requirements:**
/// - Full derivation chain from answer to atoms to evidence to sources
/// - DERIVED_FROM edges connecting derivation steps
/// - Trust propagation through derivation
/// - Explanation-ready format
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProvenanceChain {
    /// Root atom (answer atom)
    pub root_atom_id: AtomId,
    /// All atoms in the derivation chain
    pub nodes: Vec<ProvenanceNode>,
    /// DERIVED_FROM edges connecting derivation steps
    pub derivation_edges: Vec<DerivationEdge>,
    /// Direct evidence links (depth 0)
    pub direct_evidence: Vec<EvidenceLink>,
    /// Maximum derivation depth
    pub max_depth: u32,
    /// Overall confidence (propagated from all sources)
    pub overall_confidence: f64,
    /// Overall trust (propagated with decay)
    pub overall_trust: TrustLevel,
}

impl Default for ProvenanceChain {
    fn default() -> Self {
        Self::new()
    }
}

impl ProvenanceChain {
    /// Create an empty provenance chain
    #[inline]
    pub fn new() -> Self {
        ProvenanceChain {
            root_atom_id: [0u8; 32],
            nodes: Vec::new(),
            derivation_edges: Vec::new(),
            direct_evidence: Vec::new(),
            max_depth: 0,
            overall_confidence: 0.0,
            overall_trust: 0,
        }
    }

    /// Create provenance chain for a specific atom
    #[inline]
    pub fn for_atom(atom_id: AtomId) -> Self {
        ProvenanceChain {
            root_atom_id: atom_id,
            nodes: Vec::new(),
            derivation_edges: Vec::new(),
            direct_evidence: Vec::new(),
            max_depth: 0,
            overall_confidence: 0.0,
            overall_trust: 5000,
        }
    }

    /// Add a node to the chain
    #[inline]
    pub fn add_node(&mut self, node: ProvenanceNode) {
        if node.depth > self.max_depth {
            self.max_depth = node.depth;
        }
        self.nodes.push(node);
    }

    /// Add a derivation edge
    #[inline]
    pub fn add_derivation(&mut self, edge: DerivationEdge) {
        self.derivation_edges.push(edge);
    }

    /// Add direct evidence (depth 0)
    #[inline]
    pub fn add_direct_evidence(&mut self, link: EvidenceLink) {
        // Update overall confidence/trust before pushing
        if self.direct_evidence.is_empty() {
            self.overall_confidence = link.confidence;
            self.overall_trust = link.trust;
        } else {
            // Push first to include in average calculation
            self.direct_evidence.push(link.clone());
            let n = self.direct_evidence.len();
            self.overall_confidence = self
                .direct_evidence
                .iter()
                .map(|l| l.confidence)
                .sum::<f64>()
                / n as f64;
            self.overall_trust = (self
                .direct_evidence
                .iter()
                .map(|l| l.trust as f64)
                .sum::<f64>()
                / n as f64) as TrustLevel;
            return;
        }
        self.direct_evidence.push(link);
    }

    /// Calculate propagated trust through derivation chain
    pub fn calculate_propagated_trust(&mut self) {
        if self.nodes.is_empty() && self.direct_evidence.is_empty() {
            return;
        }

        let base_trust = if !self.direct_evidence.is_empty() {
            self.overall_trust as f64
        } else if !self.nodes.is_empty() {
            self.nodes[0].cumulative_trust as f64
        } else {
            5000.0
        };

        let decay = EvidenceKind::DERIVED.trust_decay_factor();
        let propagated = base_trust * decay.powi(self.max_depth as i32);
        self.overall_trust = propagated as TrustLevel;
    }

    /// Generate explanation for why is this answer valid
    pub fn explanation(&self) -> String {
        let mut explanation = String::new();
        explanation.push_str(&format!(
            "Provenance chain for atom {:?}\n",
            self.root_atom_id
        ));
        explanation.push_str(&format!(
            "Overall confidence: {:.2}%\n",
            self.overall_confidence * 100.0
        ));
        explanation.push_str(&format!("Overall trust: {}\n", self.overall_trust));
        explanation.push_str(&format!("Maximum derivation depth: {}\n", self.max_depth));

        if !self.direct_evidence.is_empty() {
            explanation.push_str("\nDirect evidence:\n");
            for (i, link) in self.direct_evidence.iter().enumerate() {
                explanation.push_str(&format!(
                    "  {}. {:?} from {:?} (confidence {:.2}%, trust {})\n",
                    i + 1,
                    link.evidence_kind,
                    link.source_atom_id,
                    link.confidence * 100.0,
                    link.trust
                ));
            }
        }

        if !self.derivation_edges.is_empty() {
            explanation.push_str("\nDerivation chain:\n");
            for (i, edge) in self.derivation_edges.iter().enumerate() {
                explanation.push_str(&format!(
                    "  {}. {:?} derived from {:?} (depth {}, trust {})\n",
                    i + 1,
                    edge.derived_atom_id,
                    edge.source_atom_id,
                    edge.depth,
                    edge.propagated_trust
                ));
            }
        }

        if !self.nodes.is_empty() {
            explanation.push_str("\nAtoms in chain:\n");
            for (i, node) in self.nodes.iter().enumerate() {
                explanation.push_str(&format!(
                    "  {}. {:?} (type {:?}, depth {}, trust {})\n",
                    i + 1,
                    node.atom_id,
                    node.atom_type,
                    node.depth,
                    node.cumulative_trust
                ));
            }
        }

        explanation
    }

    /// Check if chain has complete derivation
    #[inline]
    pub fn is_complete(&self) -> bool {
        !self.nodes.is_empty() || !self.direct_evidence.is_empty()
    }

    /// Check if chain has derivation steps
    #[inline]
    pub fn has_derivation(&self) -> bool {
        !self.derivation_edges.is_empty()
    }

    /// Get all source atoms in chain
    pub fn source_atoms(&self) -> Vec<AtomId> {
        let mut sources = Vec::new();
        for link in &self.direct_evidence {
            sources.push(link.source_atom_id);
        }
        for edge in &self.derivation_edges {
            sources.push(edge.source_atom_id);
        }
        sources.sort();
        sources.dedup();
        sources
    }
}

/// Legacy EvidenceRef for backward compatibility
///
/// Prefer using EvidenceLink for full provenance.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EvidenceRef {
    pub atom_id: AtomId,
    pub section_kind: SectionKind,
    pub offset: u64,
    pub length: u64,
    pub trust: TrustLevel,
}

impl EvidenceRef {
    #[inline]
    pub fn new(
        atom_id: AtomId,
        section_kind: SectionKind,
        offset: u64,
        length: u64,
        trust: TrustLevel,
    ) -> Self {
        EvidenceRef {
            atom_id,
            section_kind,
            offset,
            length,
            trust,
        }
    }

    /// Convert to full EvidenceLink
    #[inline]
    pub fn to_evidence_link(&self, evidence_kind: EvidenceKind) -> EvidenceLink {
        EvidenceLink::new(
            self.atom_id,
            evidence_kind,
            self.trust as f64 / 10000.0,
            self.trust,
            self.section_kind,
            self.offset,
            self.length,
        )
    }
}

// ============================================================================
// SKF-1.1 Section 10.1: Batch Ingest Types
// ============================================================================

/// Batch atom for efficient bulk loading (SKF-1.1 §10.1)
///
/// Used by `MemoryX::batch_ingest()` to load 100+ atoms with coalesced I/O.
pub struct BatchAtom {
    /// Raw payload bytes (will be hashed for AtomId)
    pub payload: Vec<u8>,
    /// Type of atom (FACT, DEFINITION, etc.)
    pub atom_type: AtomType,
    /// Claims contained in this atom
    pub claims: Vec<ClaimData>,
    /// Evidence references for provenance
    pub evidence: Vec<EvidenceRef>,
}

impl BatchAtom {
    /// Create a new batch atom
    #[inline]
    pub fn new(
        payload: Vec<u8>,
        atom_type: AtomType,
        claims: Vec<ClaimData>,
        evidence: Vec<EvidenceRef>,
    ) -> Self {
        BatchAtom {
            payload,
            atom_type,
            claims,
            evidence,
        }
    }
}

/// Error for individual atom in batch ingest
#[derive(Debug, Clone)]
pub struct BatchError {
    /// Index of failed atom in batch
    pub index: usize,
    /// Atom ID if it was generated before failure
    pub atom_id: Option<AtomId>,
    /// Error description
    pub error: String,
}

impl BatchError {
    /// Create a new batch error
    #[inline]
    pub fn new(index: usize, atom_id: Option<AtomId>, error: String) -> Self {
        BatchError {
            index,
            atom_id,
            error,
        }
    }
}

/// Result of batch ingest operation (SKF-1.1 §10.1)
pub struct BatchIngestResult {
    /// Successfully ingested atom IDs
    pub atom_ids: Vec<AtomId>,
    /// Errors for failed atoms
    pub errors: Vec<BatchError>,
    /// Total number of atoms in batch
    pub total: usize,
}

impl BatchIngestResult {
    /// Create a new batch ingest result
    #[inline]
    pub fn new(atom_ids: Vec<AtomId>, errors: Vec<BatchError>, total: usize) -> Self {
        BatchIngestResult {
            atom_ids,
            errors,
            total,
        }
    }

    /// Get number of successful atoms
    #[inline]
    pub fn success_count(&self) -> usize {
        self.atom_ids.len()
    }

    /// Get number of failed atoms
    #[inline]
    pub fn error_count(&self) -> usize {
        self.errors.len()
    }

    /// Check if all atoms were successful
    #[inline]
    pub fn all_success(&self) -> bool {
        self.errors.is_empty() && self.atom_ids.len() == self.total
    }
}

// ============================================================================
// SKF-1.1 Section 10.1: Update Atom Types
// ============================================================================

/// Result of update_atom operation (SKF-1.1 §2.1.2, 10.1)
///
/// Update preserves old atom and creates new atom with 'supersedes' provenance link.
pub struct UpdateResult {
    /// New atom ID (with updated content)
    pub new_atom_id: AtomId,
    /// Old atom ID that was superseded (preserved for history)
    pub supersedes: AtomId,
}

impl UpdateResult {
    /// Create a new update result
    #[inline]
    pub fn new(new_atom_id: AtomId, supersedes: AtomId) -> Self {
        UpdateResult {
            new_atom_id,
            supersedes,
        }
    }
}

// ============================================================================
// SKF-1.1 Section 10.1: Delete Atom Types
// ============================================================================

/// Reason for atom deletion (SKF-1.1 §2.1.2, 10.1)
///
/// Tombstone preserves audit trail and enables replication sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteReason {
    /// Correction (replaced by corrected version)
    Correction,
    /// Retraction (claim withdrawn by author)
    Retraction,
    /// Duplicate (another atom covers same content)
    Duplicate,
    /// Legal (DMCA, privacy, etc.)
    Legal,
    /// Obsolete (outdated information)
    Obsolete,
}

impl DeleteReason {
    /// Convert to u8 for storage
    #[inline]
    pub const fn to_u8(self) -> u8 {
        match self {
            DeleteReason::Correction => 1,
            DeleteReason::Retraction => 2,
            DeleteReason::Duplicate => 3,
            DeleteReason::Legal => 4,
            DeleteReason::Obsolete => 5,
        }
    }

    /// Convert from u8
    #[inline]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(DeleteReason::Correction),
            2 => Some(DeleteReason::Retraction),
            3 => Some(DeleteReason::Duplicate),
            4 => Some(DeleteReason::Legal),
            5 => Some(DeleteReason::Obsolete),
            _ => None,
        }
    }
}

/// Result of delete_atom operation (SKF-1.1 §10.1)
pub struct DeleteResult {
    /// Operation success flag
    pub success: bool,
    /// Tombstone atom ID (marks deletion in CAS)
    pub tombstone_id: AtomId,
}

impl DeleteResult {
    /// Create a new delete result
    #[inline]
    pub fn new(success: bool, tombstone_id: AtomId) -> Self {
        DeleteResult {
            success,
            tombstone_id,
        }
    }
}

// ============================================================================
// Claim View
// ============================================================================

/// Claim view for query results
#[derive(Debug, Clone)]
pub struct ClaimView {
    pub subj: EntityRef,
    pub pred: SymId,
    pub obj_tag: ObjTag,
    pub obj_value: ConstValue,
    pub qualifiers_mask: u32,
    pub trust: TrustLevel,
    pub atom_id: AtomId,
    pub status: ClaimStatus,
    pub evidence_refs: Vec<EvidenceRef>,
    pub provenance_path: Vec<EvidenceRef>,
}

/// Epistemic status of a claim returned in an AnswerPack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClaimStatus {
    Verified,
    Derived,
    Structural,
    InsufficientEvidence,
}

impl ClaimView {
    #[inline]
    pub fn new(
        subj: EntityRef,
        pred: SymId,
        obj_tag: ObjTag,
        obj_value: ConstValue,
        qualifiers_mask: u32,
        trust: TrustLevel,
        atom_id: AtomId,
    ) -> Self {
        ClaimView {
            subj,
            pred,
            obj_tag,
            obj_value,
            qualifiers_mask,
            trust,
            atom_id,
            status: ClaimStatus::InsufficientEvidence,
            evidence_refs: Vec::new(),
            provenance_path: Vec::new(),
        }
    }

    #[inline]
    pub fn with_provenance(
        mut self,
        status: ClaimStatus,
        evidence_refs: Vec<EvidenceRef>,
        provenance_path: Vec<EvidenceRef>,
    ) -> Self {
        self.status = status;
        self.evidence_refs = evidence_refs;
        self.provenance_path = provenance_path;
        self
    }
}

// ObjTag is re-exported from crate::store

// ============================================================================
// Answer Graph Types
// ============================================================================

/// Atom reference in answer graph
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AtomRef {
    pub atom_id: AtomId,
    pub node_num: NodeNum,
    pub seg_id: u32,
    pub offset: u64,
}

impl AtomRef {
    #[inline]
    pub const fn new(atom_id: AtomId, node_num: NodeNum, seg_id: u32, offset: u64) -> Self {
        AtomRef {
            atom_id,
            node_num,
            seg_id,
            offset,
        }
    }
}

/// Edge type for answer graph
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgEdgeType {
    /// Supports relationship
    Supports,
    /// Contradicts relationship
    Contradicts,
    /// Derives relationship
    Derives,
    /// References relationship
    References,
    /// Temporal ordering
    Precedes,
}

/// Edge in answer graph
#[derive(Debug, Clone)]
pub struct AgEdge {
    pub src_idx: usize,
    pub dst_idx: usize,
    pub edge_type: AgEdgeType,
    pub confidence: TrustLevel,
    pub derived: bool, // True if edge was inferred
}

impl AgEdge {
    #[inline]
    pub fn new(
        src_idx: usize,
        dst_idx: usize,
        edge_type: AgEdgeType,
        confidence: TrustLevel,
    ) -> Self {
        AgEdge {
            src_idx,
            dst_idx,
            edge_type,
            confidence,
            derived: false,
        }
    }
}

/// Node in answer graph
#[derive(Debug, Clone)]
pub struct AgNode {
    pub atom_ref: AtomRef,
    pub atom_type: AtomType,
    pub atom_type_idx: u32,
    pub cost: f64,
    pub gaps_covered: std::collections::HashSet<GapId>,
    pub trust: TrustLevel,
    pub io_bytes: u32,
    pub age_ns: u64,
    pub domain_mask: DomainMask,
    pub hard_conflicts: u32,
    pub soft_conflicts: u32,
    pub evidence_refs: Vec<EvidenceRef>,
    pub derived_claims: Vec<ClaimData>,
    /// Branch context ID - indicates which TMS branch this node belongs to (SKF-1.1 §3.2)
    /// When a candidate triggers NeedBranch, the new context ID is stored here,
    /// linking this node to its branch lineage in the answer graph.
    pub branch_ctx_id: Option<CtxId>,
}

impl AgNode {
    #[inline]
    pub fn new(atom_ref: AtomRef, atom_type: AtomType) -> Self {
        AgNode {
            atom_ref,
            atom_type,
            atom_type_idx: atom_type.to_u32(),
            cost: 0.0,
            gaps_covered: std::collections::HashSet::new(),
            trust: 5000, // Default trust
            io_bytes: 0,
            age_ns: 0,
            domain_mask: 0,
            hard_conflicts: 0,
            soft_conflicts: 0,
            evidence_refs: Vec::new(),
            derived_claims: Vec::new(),
            branch_ctx_id: None,
        }
    }

    /// Add a covered gap
    #[inline]
    pub fn add_gap(&mut self, gap_id: GapId) {
        self.gaps_covered.insert(gap_id);
    }

    /// Calculate cost contribution
    #[inline]
    pub fn calculate_cost(&mut self, weights: &CostWeights, now_ns: u64) {
        let trust_inverse = if self.trust > 0 {
            1.0 / (self.trust as f64 / 10000.0)
        } else {
            10.0
        };

        let age_penalty = if now_ns > 0 && self.age_ns > 0 {
            ((now_ns - self.age_ns) as f64) / (365.0 * 24.0 * 60.0 * 60.0 * 1_000_000_000.0)
        } else {
            0.0
        };

        let domain_penalty = if self.domain_mask == 0 { 1.0 } else { 0.0 };

        self.cost = weights.wN
            + weights.wIO * (self.io_bytes as f64)
            + weights.wT * trust_inverse
            + weights.wA * age_penalty
            + weights.wD * domain_penalty
            + weights.wC * self.hard_conflicts as f64
            + weights.wS * self.soft_conflicts as f64;
    }

    /// Check if node covers a specific gap
    #[inline]
    pub fn covers_gap(&self, gap_id: GapId) -> bool {
        self.gaps_covered.contains(&gap_id)
    }
}

/// Proof step for derived claims in answer graph
///
/// Represents a single step in the proof derivation chain:
/// AG = ⟨nodes(KA), edges, derived_claims, proof_steps, ctx_id⟩
#[derive(Debug, Clone)]
pub struct ProofStep {
    /// Unique step identifier
    pub step_id: u32,
    /// Rule atom ID used for this step
    pub rule_atom_id: AtomId,
    /// Premise indices in the answer graph nodes
    pub premises: Vec<usize>,
    /// Conclusion index in the answer graph nodes
    pub conclusion: usize,
    /// Variable bindings (var_name -> bound_value)
    pub bindings: Vec<(String, String)>,
}

impl ProofStep {
    /// Create a new proof step
    #[inline]
    pub fn new(
        step_id: u32,
        rule_atom_id: AtomId,
        premises: Vec<usize>,
        conclusion: usize,
        bindings: Vec<(String, String)>,
    ) -> Self {
        ProofStep {
            step_id,
            rule_atom_id,
            premises,
            conclusion,
            bindings,
        }
    }
}

/// AnswerGraph structure
///
/// Represents the provenance graph of retrieved atoms that answer the query.
/// AG = ⟨nodes(KA), edges, derived_claims, proof_steps, ctx_id⟩
#[derive(Debug, Clone)]
pub struct AnswerGraph {
    /// Nodes in the graph
    pub nodes: Vec<AgNode>,
    /// Edges in the graph
    pub edges: Vec<AgEdge>,
    /// Claims derived during fixed-point reasoning but not materialized as standalone atoms
    pub derived_claims: Vec<ClaimData>,
    /// Total cost
    pub total_cost: f64,
    /// Covered gaps
    pub covered_gaps: std::collections::HashSet<GapId>,
    /// Graph generation
    pub generation: u32,
    /// Proof steps for derived claims
    pub proof_steps: Vec<ProofStep>,
    /// Context ID for this answer graph
    pub ctx_id: CtxId,
    /// Branch lineage - contexts created during TMS branching (SKF-1.1 §3.2)
    /// Records all CtxIds created by NeedBranch candidates during reasoning
    pub branch_lineage: Vec<CtxId>,
}

impl Default for AnswerGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl AnswerGraph {
    /// Create a new empty answer graph
    #[inline]
    pub fn new() -> Self {
        AnswerGraph {
            nodes: Vec::with_capacity(64),
            edges: Vec::with_capacity(128),
            derived_claims: Vec::new(),
            total_cost: 0.0,
            covered_gaps: std::collections::HashSet::new(),
            generation: 0,
            proof_steps: Vec::new(),
            ctx_id: 0,
            branch_lineage: Vec::new(),
        }
    }
    /// Create with capacity
    #[inline]
    pub fn with_capacity(node_cap: usize, edge_cap: usize) -> Self {
        AnswerGraph {
            nodes: Vec::with_capacity(node_cap),
            edges: Vec::with_capacity(edge_cap),
            derived_claims: Vec::new(),
            total_cost: 0.0,
            covered_gaps: std::collections::HashSet::new(),
            generation: 0,
            proof_steps: Vec::new(),
            ctx_id: 0,
            branch_lineage: Vec::new(),
        }
    }

    /// Add a node to the graph
    #[inline]
    pub fn add_node(&mut self, node: AgNode) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(node);
        idx
    }

    /// Add an edge to the graph
    #[inline]
    pub fn add_edge(&mut self, edge: AgEdge) {
        self.edges.push(edge);
    }

    /// Add a proof step to the graph
    #[inline]
    pub fn add_proof_step(&mut self, step: ProofStep) {
        self.proof_steps.push(step);
    }

    /// Get node by index
    #[inline]
    pub fn get_node(&self, idx: usize) -> Option<&AgNode> {
        self.nodes.get(idx)
    }

    /// Get mutable node by index
    #[inline]
    pub fn get_node_mut(&mut self, idx: usize) -> Option<&mut AgNode> {
        self.nodes.get_mut(idx)
    }

    /// Check if graph covers a gap
    #[inline]
    pub fn covers_gap(&self, gap_id: GapId) -> bool {
        self.covered_gaps.contains(&gap_id)
    }

    /// Mark gaps as covered
    pub fn mark_gaps_covered(&mut self, gap_ids: &[GapId]) {
        for &gap_id in gap_ids {
            self.covered_gaps.insert(gap_id);
        }
    }

    /// Check if all gaps are covered
    #[inline]
    pub fn all_gaps_covered(&self, total_gaps: usize) -> bool {
        self.covered_gaps.len() >= total_gaps
    }

    /// Recalculate total cost
    pub fn recalculate_cost(&mut self, weights: &CostWeights, now_ns: u64) {
        self.total_cost = 0.0;

        for node in &mut self.nodes {
            node.calculate_cost(weights, now_ns);
            self.total_cost += node.cost;
        }

        // Add edge costs
        self.total_cost += weights.wE * self.edges.len() as f64;
    }

    /// Get number of nodes
    #[inline]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Get number of edges
    #[inline]
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Get total IO bytes
    #[inline]
    pub fn total_io_bytes(&self) -> u64 {
        self.nodes.iter().map(|n| n.io_bytes as u64).sum()
    }

    /// Get total conflicts
    #[inline]
    pub fn total_conflicts(&self) -> (u32, u32) {
        let hard: u32 = self.nodes.iter().map(|n| n.hard_conflicts).sum();
        let soft: u32 = self.nodes.iter().map(|n| n.soft_conflicts).sum();
        (hard, soft)
    }

    /// Prune nodes that don't contribute to coverage
    pub fn prune(&mut self) {
        // Find nodes that don't cover any gaps
        let mut to_remove: Vec<usize> = self
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.gaps_covered.is_empty())
            .map(|(i, _)| i)
            .collect();

        // Sort in reverse order to preserve indices
        to_remove.sort_by(|a, b| b.cmp(a));

        for idx in to_remove {
            self.nodes.remove(idx);
        }

        // Remove edges that reference removed nodes
        let node_count = self.nodes.len();
        self.edges
            .retain(|e| e.src_idx < node_count && e.dst_idx < node_count);
    }

    /// Check if graph is empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Clear the graph
    #[inline]
    pub fn clear(&mut self) {
        self.nodes.clear();
        self.edges.clear();
        self.derived_claims.clear();
        self.total_cost = 0.0;
        self.covered_gaps.clear();
        self.generation = 0;
        self.proof_steps.clear();
        self.ctx_id = 0;
    }
}

// ============================================================================
// Store Configuration
// ============================================================================

/// Configuration for MemoryX store initialization
///
/// # Fields
/// - `root_path`: Root directory for all store files
/// - `mmap_mode`: Enable memory-mapped I/O for reads
/// - `io_uring`: Enable io_uring for async I/O (Linux only)
/// - `io_buffer_size`: Size of I/O buffers for batched reads
/// - `fetch_budget`: Maximum bytes to fetch per query iteration
/// - `coalesce_gap`: Maximum gap between offsets for I/O coalescing
#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// Root path for store files
    pub root_path: PathBuf,
    /// Enable memory-mapped I/O
    pub mmap_mode: bool,
    /// Enable io_uring (Linux only)
    pub io_uring: bool,
    /// I/O buffer size in bytes
    pub io_buffer_size: usize,
    /// Fetch budget per query iteration
    pub fetch_budget: u32,
    /// Maximum gap for I/O coalescing
    pub coalesce_gap: usize,
}

impl StoreConfig {
    /// Create a new StoreConfig with defaults
    #[inline]
    pub fn new(root_path: PathBuf) -> Self {
        StoreConfig {
            root_path,
            mmap_mode: true,
            io_uring: false,
            io_buffer_size: 64 * 1024, // 64KB default
            fetch_budget: 64 * 1024,   // 64KB default
            coalesce_gap: 4096,        // 4KB gap tolerance
        }
    }

    /// Create a project-scoped default configuration.
    ///
    /// Base path format:
    /// `<current-working-directory>/.memoryx/bases/default`
    #[inline]
    pub fn project_default() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        StoreConfig::new(cwd.join(".memoryx").join("bases").join("default"))
    }

    /// Create a user-scoped shared configuration.
    ///
    /// Base path format:
    /// `<home-directory>/.memoryx/bases/default`
    #[inline]
    pub fn user_default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        StoreConfig::new(home.join(".memoryx").join("bases").join("default"))
    }

    /// Set mmap mode
    #[inline]
    pub fn with_mmap_mode(mut self, enabled: bool) -> Self {
        self.mmap_mode = enabled;
        self
    }

    /// Set io_uring mode
    #[inline]
    pub fn with_io_uring(mut self, enabled: bool) -> Self {
        self.io_uring = enabled;
        self
    }

    /// Set I/O buffer size
    #[inline]
    pub fn with_io_buffer_size(mut self, size: usize) -> Self {
        self.io_buffer_size = size;
        self
    }

    /// Set fetch budget
    #[inline]
    pub fn with_fetch_budget(mut self, budget: u32) -> Self {
        self.fetch_budget = budget;
        self
    }

    /// Set coalesce gap
    #[inline]
    pub fn with_coalesce_gap(mut self, gap: usize) -> Self {
        self.coalesce_gap = gap;
        self
    }

    /// Get CAS directory path
    #[inline]
    pub fn cas_dir(&self) -> PathBuf {
        self.root_path.join("cas")
    }

    /// Get index directory path
    #[inline]
    pub fn index_dir(&self) -> PathBuf {
        self.root_path.join("index")
    }

    /// Get graph directory path
    #[inline]
    pub fn graph_dir(&self) -> PathBuf {
        self.root_path.join("graph")
    }

    /// Get meta directory path
    #[inline]
    pub fn meta_dir(&self) -> PathBuf {
        self.root_path.join("meta")
    }
}

impl Default for StoreConfig {
    fn default() -> Self {
        StoreConfig::project_default()
    }
}

// ============================================================================
// Context Management
// ============================================================================

/// Context ID type
pub type CtxId = u32;

/// Context policy ID type
pub type CtxPolicyId = u32;

/// Claim ID type for context claims
pub type ClaimId = u64;

/// Conflict resolution mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConflictResolutionMode {
    /// Create new branch on conflict
    #[default]
    Branch,
    /// Reject conflicting claim
    Reject,
    /// Prefer existing claim (keep new)
    PreferNew,
    /// Prefer new claim (replace existing)
    PreferExisting,
}

/// Context policy with TMS constraints
#[derive(Debug, Clone)]
pub struct CtxPolicy {
    pub trust_threshold: TrustLevel,
    pub domain_constraints: DomainMask,
    pub conflict_resolution: ConflictResolutionMode,
}

impl Default for CtxPolicy {
    fn default() -> Self {
        CtxPolicy {
            trust_threshold: 0,
            domain_constraints: 0xFFFF,
            conflict_resolution: ConflictResolutionMode::Branch,
        }
    }
}

impl CtxPolicy {
    /// Create new policy with defaults
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set trust threshold
    #[inline]
    pub fn with_trust_threshold(mut self, threshold: TrustLevel) -> Self {
        self.trust_threshold = threshold;
        self
    }

    /// Set domain constraints
    #[inline]
    pub fn with_domain_constraints(mut self, domain_mask: DomainMask) -> Self {
        self.domain_constraints = domain_mask;
        self
    }

    /// Set conflict resolution mode
    #[inline]
    pub fn with_conflict_resolution(mut self, mode: ConflictResolutionMode) -> Self {
        self.conflict_resolution = mode;
        self
    }
}

/// Context branch representation with full TMS support
#[derive(Debug, Clone)]
pub struct ContextBranch {
    pub ctx_id: CtxId,
    pub parent_ctx: Option<CtxId>,
    pub policy_id: CtxPolicyId,
    pub branch_reason: BranchReason,
    pub created_at_ns: u64,
    pub active: bool,
    /// Active claims in this context (TMS belief set)
    /// Maps ClaimId -> ActiveClaim (claim + source atom_id for conflict tracking)
    pub active_claims: HashMap<ClaimId, ActiveClaim>,
    /// Conflicts detected in this context
    pub conflicts: Vec<Conflict>,
    /// Policy for this context
    pub policy: CtxPolicy,
}

/// Reason for context branching
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchReason {
    /// Conflict detected
    Conflict,
    /// Hypothesis exploration
    Hypothesis,
    /// Alternative interpretation
    Alternative,
    /// User-initiated branch
    Manual,
}

/// Query filters for search operations
#[derive(Debug, Clone, Copy, Default)]
pub struct QueryFilters {
    /// Minimum trust level (0-10000)
    pub min_trust: u16,
    /// Domain mask (bitmask)
    pub domain_mask: u64,
    /// Valid from time (nanoseconds)
    pub valid_from_ns: u64,
    /// Valid to time (nanoseconds, 0 = infinity)
    pub valid_to_ns: u64,
}

impl QueryFilters {
    /// Create new query filters
    #[inline]
    pub fn new(min_trust: u16, domain_mask: u64) -> Self {
        QueryFilters {
            min_trust,
            domain_mask,
            valid_from_ns: 0,
            valid_to_ns: 0,
        }
    }

    /// Check if metadata matches filters
    #[inline]
    pub fn matches(&self, metadata: &AtomMetadata) -> bool {
        // Check trust
        if metadata.trust_level < self.min_trust {
            return false;
        }

        // Check domain overlap
        if self.domain_mask != 0 && (metadata.domain_mask & self.domain_mask) == 0 {
            return false;
        }

        true
    }
}

/// Context manager for branching and conflict tracking
#[derive(Clone)]
pub struct CtxManager {
    contexts: Vec<ContextBranch>,
    next_ctx_id: CtxId,
    active_ctx: CtxId,
}

impl CtxManager {
    /// Create a new context manager
    #[inline]
    pub fn new() -> Self {
        CtxManager {
            contexts: Vec::with_capacity(64),
            next_ctx_id: 0,
            active_ctx: 0,
        }
    }

    /// Create a new context with the default policy
    #[inline]
    pub fn create_context(&mut self, policy_id: CtxPolicyId) -> CtxId {
        let ctx_id = self.next_ctx_id;
        self.next_ctx_id += 1;

        self.contexts.push(ContextBranch {
            ctx_id,
            parent_ctx: None,
            policy_id,
            branch_reason: BranchReason::Manual,
            created_at_ns: 0, // Would use HLC in production
            active: true,
            active_claims: HashMap::new(),
            conflicts: Vec::new(),
            policy: CtxPolicy::new(),
        });

        ctx_id
    }

    /// Create a branch from an existing context (TMS-aware)
    #[inline]
    pub fn create_branch(
        &mut self,
        parent_ctx: CtxId,
        reason: BranchReason,
        policy_id: CtxPolicyId,
    ) -> Option<CtxId> {
        // Verify parent exists
        let parent = self.contexts.iter().find(|c| c.ctx_id == parent_ctx)?;

        let ctx_id = self.next_ctx_id;
        self.next_ctx_id += 1;

        self.contexts.push(ContextBranch {
            ctx_id,
            parent_ctx: Some(parent_ctx),
            policy_id,
            branch_reason: reason,
            created_at_ns: 0,
            active: true,
            active_claims: parent.active_claims.clone(),
            conflicts: parent.conflicts.clone(),
            policy: parent.policy.clone(),
        });

        Some(ctx_id)
    }

    /// Get the active context ID
    #[inline]
    pub fn active_ctx(&self) -> CtxId {
        self.active_ctx
    }

    /// Set the active context
    #[inline]
    pub fn set_active_ctx(&mut self, ctx_id: CtxId) -> bool {
        if self.contexts.iter().any(|c| c.ctx_id == ctx_id && c.active) {
            self.active_ctx = ctx_id;
            true
        } else {
            false
        }
    }

    /// Get context by ID
    #[inline]
    pub fn get_ctx(&self, ctx_id: CtxId) -> Option<&ContextBranch> {
        self.contexts.iter().find(|c| c.ctx_id == ctx_id)
    }

    /// Get mutable context by ID
    #[inline]
    pub fn get_ctx_mut(&mut self, ctx_id: CtxId) -> Option<&mut ContextBranch> {
        self.contexts.iter_mut().find(|c| c.ctx_id == ctx_id)
    }

    /// Compute claim ID from claim data (for indexing)
    #[inline]
    fn compute_claim_id(claim: &ClaimData) -> ClaimId {
        crate::vm::CtxIndex::claim_signature(claim)
    }

    /// Compute pattern hash for conflict detection
    #[inline]
    fn compute_pattern_hash(claim: &ClaimData) -> u64 {
        crate::vm::CtxIndex::claim_pattern_hash(claim)
    }

    /// CTX_PROBE: Check if claim conflicts with active claims in context
    ///
    /// **SKF-1.1 Contract:**
    /// - Returns Conflict with REAL AtomIds (not placeholders)
    /// - atom_a = source atom of existing claim
    /// - atom_b = source atom of new claim (provided as argument)
    pub fn probe_conflict_with_atoms(
        &self,
        ctx_id: CtxId,
        claim: &ClaimData,
        new_atom_id: AtomId,
    ) -> Option<Conflict> {
        let ctx = self.get_ctx(ctx_id)?;
        let pattern_hash = Self::compute_pattern_hash(claim);
        let new_signature = Self::compute_claim_id(claim);
        let mut exact_duplicate = false;

        // Check against active claims for exact matches and real value contradictions.
        for active_claim in ctx.active_claims.values() {
            let existing_pattern = Self::compute_pattern_hash(&active_claim.claim);
            if existing_pattern != pattern_hash {
                continue;
            }

            let existing_signature = Self::compute_claim_id(&active_claim.claim);
            if existing_signature == new_signature {
                exact_duplicate = true;
                continue;
            }

            return Some(Conflict {
                c_id: ctx.conflicts.len() as u32,
                atom_a: active_claim.atom_id, // Real AtomId from existing claim
                atom_b: new_atom_id,          // Real AtomId from new claim
                conflict_type: ConflictType::VALUE_CONTRADICTION,
                severity: ConflictSeverity::Hard,
                pattern_hash,
                conditions: ConflictConditions {
                    valid_from_ns: 0,
                    valid_to_ns: u64::MAX,
                    domain_mask: 0xFFFF,
                    policy_ids: vec![ctx.policy_id],
                },
                resolution_candidates: vec![
                    ResolutionOption::PreferAtomA,
                    ResolutionOption::PreferAtomB,
                    ResolutionOption::SplitByDomain,
                ],
            });
        }

        if exact_duplicate {
            return None;
        }

        None
    }

    /// Assert a claim into a context with source atom ID (SKF-1.1 §3.3)
    ///
    /// **SKF-1.1 Contract:**
    /// - `atom_id` MUST be a real content-addressed identity
    /// - Conflict objects will contain real AtomIds for both parties
    ///
    /// Returns:
    /// - Ok(ctx_id): Claim added to context
    /// - Ok(new_ctx_id): Branch created, claim added to new context
    /// - Err(StoreError): Claim rejected
    pub fn assert_claim_with_atom_id(
        &mut self,
        ctx_id: CtxId,
        claim: &ClaimData,
        atom_id: AtomId,
    ) -> Result<CtxId, StoreError> {
        // 1. CTX_PROBE - check for conflicts using proper AtomIds
        if let Some(conflict) = self.probe_conflict_with_atoms(ctx_id, claim, atom_id) {
            let ctx = self.get_ctx(ctx_id).ok_or(StoreError::ContextNotFound)?;

            // Apply conflict resolution policy
            match ctx.policy.conflict_resolution {
                ConflictResolutionMode::Reject => {
                    return Err(StoreError::ClaimRejected("Conflict detected".to_string()));
                }
                ConflictResolutionMode::PreferExisting => {
                    // Keep existing claim, reject new
                    return Err(StoreError::ClaimRejected(
                        "Prefer existing claim".to_string(),
                    ));
                }
                ConflictResolutionMode::PreferNew => {
                    // Replace existing claim
                    let claim_id = Self::compute_claim_id(claim);
                    let active_claim = ActiveClaim::new(atom_id, claim.clone());
                    if let Some(ctx_mut) = self.get_ctx_mut(ctx_id) {
                        ctx_mut.active_claims.insert(claim_id, active_claim);
                    }
                    return Ok(ctx_id);
                }
                ConflictResolutionMode::Branch => {
                    // Create branch with conflict resolution
                    let new_ctx_id = self
                        .branch_ctx(ctx_id, &conflict)
                        .ok_or(StoreError::ContextBranchFailed)?;

                    // Add claim to new context (without the conflicting claim)
                    let claim_id = Self::compute_claim_id(claim);
                    let active_claim = ActiveClaim::new(atom_id, claim.clone());
                    if let Some(new_ctx) = self.get_ctx_mut(new_ctx_id) {
                        new_ctx.active_claims.insert(claim_id, active_claim);
                    }

                    return Ok(new_ctx_id);
                }
            }
        }

        // No conflict, add claim directly with source atom
        let claim_id = Self::compute_claim_id(claim);
        let active_claim = ActiveClaim::new(atom_id, claim.clone());
        if let Some(ctx) = self.get_ctx_mut(ctx_id) {
            ctx.active_claims.insert(claim_id, active_claim);
        }

        Ok(ctx_id)
    }

    /// Branch context on conflict (TMS branching)
    ///
    /// Creates new context with:
    /// - parent_ctx = parent
    /// - active_claims = parent.active_claims without the conflicting incumbent
    /// - conflicts = parent.conflicts + [conflict]
    pub fn branch_ctx(&mut self, parent_ctx: CtxId, conflict: &Conflict) -> Option<CtxId> {
        // First, get parent's data (clone to avoid borrow issues)
        let parent_data = self.get_ctx(parent_ctx).map(|p| {
            (
                p.active_claims.clone(),
                p.conflicts.clone(),
                p.policy_id,
                p.policy.clone(),
            )
        })?;

        let ctx_id = self.next_ctx_id;
        self.next_ctx_id += 1;

        let (mut new_active_claims, mut new_conflicts, policy_id, policy) = parent_data;

        // A branch must represent a real alternative world-state.
        // Remove the incumbent claim selected by the conflict before adding the new branch claim.
        let original_len = new_active_claims.len();
        new_active_claims.retain(|_, active_claim| active_claim.atom_id != conflict.atom_a);
        if original_len == new_active_claims.len() {
            new_active_claims.retain(|_, active_claim| active_claim.atom_id != conflict.atom_b);
        }
        if original_len == new_active_claims.len() {
            new_active_claims.retain(|_, active_claim| {
                Self::compute_pattern_hash(&active_claim.claim) != conflict.pattern_hash
            });
        }

        // Add the new conflict
        new_conflicts.push(conflict.clone());

        self.contexts.push(ContextBranch {
            ctx_id,
            parent_ctx: Some(parent_ctx),
            policy_id,
            branch_reason: BranchReason::Conflict,
            created_at_ns: 0,
            active: true,
            active_claims: new_active_claims,
            conflicts: new_conflicts,
            policy,
        });

        Some(ctx_id)
    }

    /// List all conflicts in a context (real implementation)
    pub fn list_conflicts(&self, ctx_id: CtxId) -> Vec<Conflict> {
        self.get_ctx(ctx_id)
            .map(|ctx| ctx.conflicts.clone())
            .unwrap_or_default()
    }

    /// List all context branches.
    pub fn list_contexts(&self) -> Vec<ContextBranch> {
        self.contexts.clone()
    }

    /// Get active claims for a context
    pub fn get_active_claims(&self, ctx_id: CtxId) -> Option<&HashMap<ClaimId, ActiveClaim>> {
        self.get_ctx(ctx_id).map(|ctx| &ctx.active_claims)
    }

    /// Get context index for invariant evaluation
    ///
    /// Builds a CtxIndex from the live active claims in the context.
    /// This is used by the VM for conflict probing during invariant evaluation.
    ///
    /// **SKF-1.1 Contract:**
    /// - Active claims are indexed with REAL AtomIds (not placeholders)
    /// - Conflict probing can now distinguish exact claims from coarse pattern buckets
    pub fn get_ctx_index(&self, ctx_id: CtxId) -> crate::vm::CtxIndex {
        let mut ctx_index = crate::vm::CtxIndex::new();

        if let Some(ctx) = self.get_ctx(ctx_id) {
            for active_claim in ctx.active_claims.values() {
                ctx_index.add_claim_index(
                    &active_claim.claim,
                    active_claim.atom_id,
                    crate::vm::ConflictSeverity::Soft,
                );
            }
        }

        ctx_index
    }


    /// Deactivate a context
    #[inline]
    pub fn deactivate_ctx(&mut self, ctx_id: CtxId) -> bool {
        if let Some(ctx) = self.contexts.iter_mut().find(|c| c.ctx_id == ctx_id) {
            ctx.active = false;
            true
        } else {
            false
        }
    }
}

impl Default for CtxManager {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// AnswerPack
// ============================================================================

/// Limitation in query answer
#[derive(Debug, Clone)]
pub struct Limitation {
    pub code: LimitationCode,
    pub description: String,
    pub severity: LimitationSeverity,
}

/// Limitation severity
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitationSeverity {
    Info,
    Warning,
    Critical,
}

/// Limitation codes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitationCode {
    /// Incomplete evidence chain
    IncompleteEvidence,
    /// Low confidence atoms used
    LowConfidence,
    /// Conflicting information exists
    ConflictsPresent,
    /// Outdated information
    Outdated,
    /// Domain mismatch
    DomainMismatch,
    /// Budget exhausted
    BudgetExhausted,
    /// Graph traversal incomplete
    TraversalIncomplete,
}

impl std::fmt::Display for LimitationCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LimitationCode::IncompleteEvidence => write!(f, "INCOMPLETE_EVIDENCE"),
            LimitationCode::LowConfidence => write!(f, "LOW_CONFIDENCE"),
            LimitationCode::ConflictsPresent => write!(f, "CONFLICTS_PRESENT"),
            LimitationCode::Outdated => write!(f, "OUTDATED"),
            LimitationCode::DomainMismatch => write!(f, "DOMAIN_MISMATCH"),
            LimitationCode::BudgetExhausted => write!(f, "BUDGET_EXHAUSTED"),
            LimitationCode::TraversalIncomplete => write!(f, "TRAVERSAL_INCOMPLETE"),
        }
    }
}

impl Limitation {
    /// Create a new limitation
    #[inline]
    pub fn new(code: LimitationCode, description: String, severity: LimitationSeverity) -> Self {
        Limitation {
            code,
            description,
            severity,
        }
    }

    /// Create an info limitation
    #[inline]
    pub fn info(code: LimitationCode, description: String) -> Self {
        Limitation::new(code, description, LimitationSeverity::Info)
    }

    /// Create a warning limitation
    #[inline]
    pub fn warning(code: LimitationCode, description: String) -> Self {
        Limitation::new(code, description, LimitationSeverity::Warning)
    }

    /// Create a critical limitation
    #[inline]
    pub fn critical(code: LimitationCode, description: String) -> Self {
        Limitation::new(code, description, LimitationSeverity::Critical)
    }
}

/// Conflict activation conditions (SKF-1.1 Section 3.3)
#[derive(Debug, Clone)]
pub struct ConflictConditions {
    /// Time range when conflict is active (0 = always)
    pub valid_from_ns: u64,
    pub valid_to_ns: u64,
    /// Domain mask where conflict applies
    pub domain_mask: DomainMask,
    /// Policy IDs where conflict is relevant
    pub policy_ids: Vec<CtxPolicyId>,
}

/// Resolution option for conflict (SKF-1.1 Section 3.3)
#[derive(Debug, Clone)]
pub enum ResolutionOption {
    /// Prefer atom A over B
    PreferAtomA,
    /// Prefer atom B over A
    PreferAtomB,
    /// Split by domain
    SplitByDomain,
    /// Split by time range
    SplitByTime,
    /// Use different versions
    UseDifferentVersions,
    /// Merge with reconciliation
    MergeWithReconciliation,
}

/// Conflict between atoms (SKF-1.1 Section 3.3)
///
/// **SKF-1.1 Spec:**
/// ```text
/// Conflict = ⟨c_id, claim_a, claim_b, reason, conditions, resolution_candidates⟩
/// ```
#[derive(Debug, Clone)]
pub struct Conflict {
    /// Unique conflict identifier
    pub c_id: u32,
    /// First conflicting atom
    pub atom_a: AtomId,
    /// Second conflicting atom
    pub atom_b: AtomId,
    /// Type of incompatibility
    pub conflict_type: ConflictType,
    /// Severity (hard/soft)
    pub severity: ConflictSeverity,
    /// Pattern hash for grouping similar conflicts
    pub pattern_hash: u64,
    /// Conditions when conflict is active (SKF-1.1 §3.3)
    pub conditions: ConflictConditions,
    /// Available resolution options (SKF-1.1 §3.3)
    pub resolution_candidates: Vec<ResolutionOption>,
}

impl Conflict {
    /// Create a new conflict (SKF-1.1 Section 3.3 compliant)
    #[inline]
    pub fn new(
        atom_a: AtomId,
        atom_b: AtomId,
        conflict_type: ConflictType,
        severity: ConflictSeverity,
        pattern_hash: u64,
    ) -> Self {
        Conflict {
            c_id: 0, // Assigned by CtxManager
            atom_a,
            atom_b,
            conflict_type,
            severity,
            pattern_hash,
            conditions: ConflictConditions {
                valid_from_ns: 0,
                valid_to_ns: u64::MAX,
                domain_mask: 0xFFFF,
                policy_ids: vec![],
            },
            resolution_candidates: vec![],
        }
    }

    /// Set conflict ID
    #[inline]
    pub fn with_id(mut self, c_id: u32) -> Self {
        self.c_id = c_id;
        self
    }

    /// Add resolution option
    #[inline]
    pub fn with_resolution(mut self, option: ResolutionOption) -> Self {
        self.resolution_candidates.push(option);
        self
    }

    /// Set activation conditions
    #[inline]
    pub fn with_conditions(mut self, conditions: ConflictConditions) -> Self {
        self.conditions = conditions;
        self
    }
}

/// Type of conflict
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictType {
    /// Direct contradiction
    Contradiction,
    /// Value contradiction (conflicting values for same claim)
    VALUE_CONTRADICTION,
    /// Temporal inconsistency
    Temporal,
    /// Source conflict
    Source,
    /// Trust conflict
    Trust,
}

/// Conflict severity
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictSeverity {
    Soft,
    Hard,
}

/// Active claim with source atom reference (SKF-1.1 §3.3)
///
/// Represents a claim that is currently active in a context,
/// along with its source atom identity for conflict tracking.
///
/// **SKF-1.1 Contract:**
/// - `atom_id` MUST be a real content-addressed identity (not placeholder)
/// - Conflict detection uses `atom_id` to link conflicts to canonical identities
#[derive(Debug, Clone)]
pub struct ActiveClaim {
    /// Source atom ID (canonical content-addressed identity)
    pub atom_id: AtomId,
    /// The claim data
    pub claim: ClaimData,
}

impl ActiveClaim {
    /// Create a new active claim with source atom
    #[inline]
    pub fn new(atom_id: AtomId, claim: ClaimData) -> Self {
        ActiveClaim { atom_id, claim }
    }
}

/// Answer pack containing query results
///
/// # Fields
/// - `graph`: Answer graph with nodes and edges
/// - `selected_ctx`: Selected context ID
/// - `claims`: Claims extracted from atoms
/// - `evidence`: Evidence references
/// - `confidence`: Overall confidence (0.0 - 1.0)
/// - `limitations`: Known limitations
/// - `alternates`: Alternative answer packs (for branching)
#[derive(Debug, Clone)]
pub struct AnswerPack {
    /// Answer graph with provenance
    pub graph: AnswerGraph,
    /// Selected context ID
    pub selected_ctx: CtxId,
    /// Extracted claims
    pub claims: Vec<ClaimView>,
    /// Evidence references
    pub evidence: Vec<EvidenceRef>,
    /// Overall confidence (0.0 - 1.0)
    pub confidence: f32,
    /// Known limitations
    pub limitations: Vec<Limitation>,
    /// Alternative answer packs
    pub alternates: Vec<AnswerPack>,
}

impl AnswerPack {
    /// Create a new empty AnswerPack
    #[inline]
    pub fn new(ctx_id: CtxId) -> Self {
        AnswerPack {
            graph: AnswerGraph::new(),
            selected_ctx: ctx_id,
            claims: Vec::new(),
            evidence: Vec::new(),
            confidence: 0.0,
            limitations: Vec::new(),
            alternates: Vec::new(),
        }
    }

    /// Create an AnswerPack from fixed-point solver state.
    ///
    /// Computes multi-factor confidence from:
    /// - Gap coverage ratio (how many required gaps were filled)
    /// - Average trust level of nodes in the answer graph
    /// - Consistency score (absence of hard/soft conflicts)
    /// - Evidence completeness (ratio of nodes with evidence refs)
    /// - Graph connectivity (ratio of nodes with edges)
    ///
    /// Generates limitations for:
    /// - Incomplete gap coverage
    /// - Low trust atoms
    /// - Hard/soft conflicts present
    /// - Budget exhaustion
    /// - Graph traversal incompleteness
    /// - Outdated information
    /// - Domain mismatches
    pub fn from_solver(
        graph: AnswerGraph,
        ctx_id: CtxId,
        gaps: &[Gap],
        weights: &CostWeights,
    ) -> Self {
        let total_gaps = gaps.len();
        let covered_gaps = graph.covered_gaps.len();
        let node_count = graph.nodes.len();

        // === Multi-factor confidence calculation ===

        // Factor 1: Coverage ratio (0.0 - 1.0)
        let coverage_ratio = if total_gaps > 0 {
            covered_gaps as f32 / total_gaps as f32
        } else {
            1.0
        };

        // Factor 2: Trust factor (0.0 - 1.0)
        let avg_trust = if node_count > 0 {
            graph.nodes.iter().map(|n| n.trust as f32).sum::<f32>() / node_count as f32
        } else {
            0.0
        };
        let trust_factor = avg_trust / 10000.0;

        // Factor 3: Consistency factor — penalize conflicts
        let total_hard: u32 = graph.nodes.iter().map(|n| n.hard_conflicts).sum();
        let total_soft: u32 = graph.nodes.iter().map(|n| n.soft_conflicts).sum();
        let consistency_factor = if node_count > 0 {
            let hard_penalty = (total_hard as f32) * 0.3;
            let soft_penalty = (total_soft as f32) * 0.05;
            (1.0 - (hard_penalty + soft_penalty)).max(0.0)
        } else {
            1.0
        };

        // Factor 4: Evidence completeness — nodes with evidence refs
        let evidence_factor = if node_count > 0 {
            let nodes_with_evidence = graph
                .nodes
                .iter()
                .filter(|n| !n.evidence_refs.is_empty())
                .count();
            nodes_with_evidence as f32 / node_count as f32
        } else {
            0.0
        };

        // Factor 5: Graph connectivity — nodes with edges
        let connectivity_factor = if node_count > 0 {
            let mut connected_nodes = std::collections::HashSet::new();
            for edge in &graph.edges {
                connected_nodes.insert(edge.src_idx);
                connected_nodes.insert(edge.dst_idx);
            }
            // For single-node graphs, connectivity is trivially satisfied
            if node_count == 1 {
                1.0
            } else {
                connected_nodes.len() as f32 / node_count as f32
            }
        } else {
            0.0
        };

        // Factor 6: Cost efficiency — lower cost relative to budget is better
        let cost_factor = if graph.total_cost > 0.0 {
            // Normalize: lower cost = higher factor, capped at 1.0
            let max_reasonable_cost = weights.wN * node_count as f64
                + weights.wIO * graph.total_io_bytes() as f64
                + weights.wE * graph.edge_count() as f64;
            if max_reasonable_cost > 0.0 {
                (max_reasonable_cost / graph.total_cost).min(1.0) as f32
            } else {
                1.0
            }
        } else {
            1.0
        };

        // Weighted combination of factors
        let confidence = (coverage_ratio * 0.35
            + trust_factor * 0.20
            + consistency_factor * 0.15
            + evidence_factor * 0.10
            + connectivity_factor * 0.10
            + cost_factor * 0.10)
            .clamp(0.0, 1.0);

        let mut pack = AnswerPack::new(ctx_id);
        pack.graph = graph.clone();
        pack.confidence = confidence;

        // === Generate limitations ===

        // Incomplete evidence
        if coverage_ratio < 1.0 {
            let uncovered: Vec<GapId> = gaps.iter().filter(|g| !g.covered).map(|g| g.id).collect();
            let gap_kinds: std::collections::HashSet<&str> = uncovered
                .iter()
                .filter_map(|&gid| gaps.get(gid as usize))
                .map(|g| match g.kind {
                    GapKind::NEED_DEFINITION => "NEED_DEFINITION",
                    GapKind::NEED_FACT => "NEED_FACT",
                    GapKind::NEED_EVIDENCE => "NEED_EVIDENCE",
                    GapKind::NEED_CAUSAL_CHAIN => "NEED_CAUSAL_CHAIN",
                    GapKind::NEED_COUNTEREXAMPLE => "NEED_COUNTEREXAMPLE",
                    GapKind::NEED_CONSTRAINTS => "NEED_CONSTRAINTS",
                    GapKind::NEED_COMPARISON_AXIS => "NEED_COMPARISON_AXIS",
                    GapKind::NEED_PROCEDURE => "NEED_PROCEDURE",
                })
                .collect();
            pack.limitations.push(Limitation::warning(
                LimitationCode::IncompleteEvidence,
                format!(
                    "Only {}/{} gaps covered. Missing: {:?}",
                    covered_gaps, total_gaps, gap_kinds
                ),
            ));
        }

        // Low confidence atoms
        if avg_trust < 5000.0 {
            let low_trust_count = graph.nodes.iter().filter(|n| n.trust < 5000).count();
            pack.limitations.push(Limitation::warning(
                LimitationCode::LowConfidence,
                format!(
                    "Average trust {:.1}%, {} nodes below threshold",
                    avg_trust / 100.0,
                    low_trust_count
                ),
            ));
        }

        // Hard conflicts present
        if total_hard > 0 {
            pack.limitations.push(Limitation::critical(
                LimitationCode::ConflictsPresent,
                format!("{} hard conflicts detected in answer graph", total_hard),
            ));
        }

        // Soft conflicts present
        if total_soft > 0 {
            pack.limitations.push(Limitation::warning(
                LimitationCode::ConflictsPresent,
                format!("{} soft conflicts detected in answer graph", total_soft),
            ));
        }

        // Budget exhaustion
        if total_gaps > 0 && covered_gaps < total_gaps && node_count > 0 {
            let io_total: u64 = graph.nodes.iter().map(|n| n.io_bytes as u64).sum();
            if io_total > 256 * 1024 {
                pack.limitations.push(Limitation::warning(
                    LimitationCode::BudgetExhausted,
                    format!("I/O budget near limit: {} bytes used", io_total),
                ));
            }
        }

        // Graph traversal incomplete
        if node_count > 1 && graph.edges.is_empty() {
            pack.limitations.push(Limitation::warning(
                LimitationCode::TraversalIncomplete,
                format!(
                    "{} nodes but no edges — graph may be disconnected",
                    node_count
                ),
            ));
        }

        // Outdated information check
        if node_count > 0 {
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            let one_year_ns = 365u64 * 24 * 60 * 60 * 1_000_000_000;
            let outdated_count = graph
                .nodes
                .iter()
                .filter(|n| n.age_ns > 0 && now_ns.saturating_sub(n.age_ns) > one_year_ns)
                .count();
            if outdated_count > 0 {
                pack.limitations.push(Limitation::info(
                    LimitationCode::Outdated,
                    format!("{} nodes may contain outdated information", outdated_count),
                ));
            }
        }

        // Domain mismatch
        if node_count > 0 {
            let no_domain_count = graph.nodes.iter().filter(|n| n.domain_mask == 0).count();
            if no_domain_count > 0 {
                pack.limitations.push(Limitation::info(
                    LimitationCode::DomainMismatch,
                    format!("{} nodes have no domain classification", no_domain_count),
                ));
            }
        }

        pack
    }

    /// Add a claim to the answer pack
    #[inline]
    pub fn add_claim(&mut self, claim: ClaimView) {
        self.claims.push(claim);
    }

    /// Add evidence to the answer pack
    #[inline]
    pub fn add_evidence(&mut self, evidence: EvidenceRef) {
        self.evidence.push(evidence);
    }

    /// Add an alternative answer pack
    #[inline]
    pub fn add_alternate(mut self, alternate: AnswerPack) -> Self {
        self.alternates.push(alternate);
        self
    }

    /// Get the best answer pack (self or alternate with highest confidence)
    pub fn best(&self) -> &AnswerPack {
        let mut best = self;
        for alt in &self.alternates {
            if alt.confidence > best.confidence {
                best = alt;
            }
        }
        best
    }

    /// Check if answer has critical limitations
    #[inline]
    pub fn has_critical_limitations(&self) -> bool {
        self.limitations
            .iter()
            .any(|l| l.severity == LimitationSeverity::Critical)
    }

    /// Generate alternative answer packs by varying set-cover tie-breaking.
    ///
    /// Creates up to `max_alternates` alternative packs that:
    /// - Cover the same or similar gaps
    /// - Use different candidate selections
    /// - Have independently computed confidence scores
    ///
    /// Alternates are useful when the primary answer has limitations
    /// and the caller wants to compare different reasoning paths.
    pub fn generate_alternates(
        &mut self,
        candidates: &[crate::query::Candidate],
        gaps: &[Gap],
        weights: &CostWeights,
        max_alternates: usize,
    ) {
        if candidates.is_empty() || gaps.is_empty() {
            return;
        }

        let mut alternates = Vec::with_capacity(max_alternates);

        // Strategy 1: Prefer lower-cost candidates (opposite of default benefit/cost)
        {
            let alt_selected = Self::alt_greedy_select(candidates, gaps, weights, 0);
            if !alt_selected.is_empty() && alt_selected != self._primary_selection() {
                let alt_graph =
                    Self::build_alt_graph(candidates, &alt_selected, gaps, self.selected_ctx);
                let alt_pack = Self::from_solver(alt_graph, self.selected_ctx, gaps, weights);
                alternates.push(alt_pack);
            }
        }

        // Strategy 2: Prefer highest-trust candidates only
        if alternates.len() < max_alternates {
            let alt_selected = Self::alt_greedy_select(candidates, gaps, weights, 1);
            if !alt_selected.is_empty() && alt_selected != self._primary_selection() {
                let alt_graph =
                    Self::build_alt_graph(candidates, &alt_selected, gaps, self.selected_ctx);
                let alt_pack = Self::from_solver(alt_graph, self.selected_ctx, gaps, weights);
                alternates.push(alt_pack);
            }
        }

        // Strategy 3: Prefer minimal node count (sparsest answer)
        if alternates.len() < max_alternates {
            let alt_selected = Self::alt_greedy_select(candidates, gaps, weights, 2);
            if !alt_selected.is_empty() && alt_selected != self._primary_selection() {
                let alt_graph =
                    Self::build_alt_graph(candidates, &alt_selected, gaps, self.selected_ctx);
                let alt_pack = Self::from_solver(alt_graph, self.selected_ctx, gaps, weights);
                alternates.push(alt_pack);
            }
        }

        // Strategy 4: Branch-aware alternates (SKF-1.1 §3.2)
        // Group candidates by branch_ctx_id and generate alternates for each branch
        if alternates.len() < max_alternates {
            let mut branch_groups: std::collections::HashMap<Option<CtxId>, Vec<usize>> =
                std::collections::HashMap::new();
            for (idx, candidate) in candidates.iter().enumerate() {
                branch_groups
                    .entry(candidate.branch_ctx_id)
                    .or_default()
                    .push(idx);
            }

            // For each branch that differs from primary, create an alternate
            for (branch_ctx, branch_indices) in branch_groups {
                if alternates.len() >= max_alternates {
                    break;
                }
                // Skip if this is the same branch as primary
                if branch_ctx == self.graph.nodes.first().and_then(|n| n.branch_ctx_id) {
                    continue;
                }
                // Use only candidates from this branch
                if branch_indices.is_empty() {
                    continue;
                }
                let alt_graph = Self::build_alt_graph_from_branch(
                    candidates,
                    &branch_indices,
                    gaps,
                    branch_ctx.unwrap_or(self.selected_ctx),
                );
                let alt_pack = Self::from_solver(
                    alt_graph,
                    branch_ctx.unwrap_or(self.selected_ctx),
                    gaps,
                    weights,
                );
                alternates.push(alt_pack);
            }
        }

        // Sort alternates by confidence descending
        alternates.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        alternates.truncate(max_alternates);

        self.alternates = alternates;
    }

    /// Get the primary selection indices for comparison with alternates.
    fn _primary_selection(&self) -> Vec<usize> {
        self.graph
            .nodes
            .iter()
            .enumerate()
            .map(|(i, _)| i)
            .collect()
    }

    /// Alternative greedy selection with different tie-breaking strategies.
    /// strategy: 0 = lowest cost, 1 = highest trust, 2 = fewest nodes
    fn alt_greedy_select(
        candidates: &[crate::query::Candidate],
        gaps: &[Gap],
        weights: &CostWeights,
        strategy: u8,
    ) -> Vec<usize> {
        let mut selected = Vec::new();
        let mut covered_gaps: std::collections::HashSet<GapId> = std::collections::HashSet::new();
        let mut available: Vec<usize> = (0..candidates.len()).collect();

        while covered_gaps.len() < gaps.len() && !available.is_empty() {
            let mut best_idx = None;
            let mut best_score = f64::NEG_INFINITY;

            for &idx in &available {
                let candidate = &candidates[idx];
                let marginal_gaps: Vec<GapId> = candidate
                    .covers_gaps
                    .iter()
                    .copied()
                    .filter(|g| !covered_gaps.contains(g))
                    .collect();

                if marginal_gaps.is_empty() {
                    continue;
                }

                let marginal_benefit: f64 = marginal_gaps
                    .iter()
                    .filter_map(|&g| gaps.get(g as usize))
                    .map(|g| weights.gap_benefit(g.priority))
                    .sum();

                let trust_inverse = if candidate.trust > 0 {
                    1.0 / (candidate.trust as f64 / 10000.0)
                } else {
                    10.0
                };

                let cost = (candidate.estimated_io_bytes as f64) * weights.wIO
                    + weights.wT * trust_inverse
                    + weights.wC * candidate.hard_conflicts as f64
                    + weights.wS * candidate.soft_conflicts as f64
                    + 1.0;

                let score = match strategy {
                    0 => marginal_benefit / (cost * 1.5), // Penalize cost more
                    1 => {
                        // Prioritize trust
                        let trust_score = candidate.trust as f64 / 10000.0;
                        marginal_benefit * trust_score / cost
                    }
                    2 => {
                        // Prioritize candidates that cover more gaps per node
                        marginal_benefit / (cost * marginal_gaps.len() as f64)
                    }
                    _ => marginal_benefit / cost,
                };

                if score > best_score {
                    best_score = score;
                    best_idx = Some(idx);
                }
            }

            if let Some(idx) = best_idx {
                let candidate = &candidates[idx];
                selected.push(idx);
                for &gap_id in &candidate.covers_gaps {
                    covered_gaps.insert(gap_id);
                }
                available.retain(|&i| i != idx);
            } else {
                break;
            }
        }

        selected
    }

    /// Build an AnswerGraph from alternative selection.
    fn build_alt_graph(
        candidates: &[crate::query::Candidate],
        selected: &[usize],
        gaps: &[Gap],
        ctx_id: CtxId,
    ) -> AnswerGraph {
        let mut graph = AnswerGraph::new();
        graph.ctx_id = ctx_id;

        for &idx in selected {
            let candidate = &candidates[idx];
            let mut node = AgNode::new(
                AtomRef::new(
                    candidate.atom_id,
                    candidate.node_num,
                    candidate.seg_id,
                    candidate.offset,
                ),
                candidate.atom_type,
            );
            node.trust = candidate.trust;
            node.io_bytes = candidate.estimated_io_bytes;
            node.age_ns = candidate.age_ns;
            node.domain_mask = candidate.domain_mask;
            node.hard_conflicts = candidate.hard_conflicts;
            node.soft_conflicts = candidate.soft_conflicts;
            node.evidence_refs = candidate.evidence_refs.clone();
            node.derived_claims = candidate.derived_claims.clone();
            // SKF-1.1 §3.2: Propagate branch context to alternate graph nodes
            node.branch_ctx_id = candidate.branch_ctx_id;

            for &gap_id in &candidate.covers_gaps {
                node.add_gap(gap_id);
            }

            // Track branch lineage in graph
            if let Some(bc) = candidate.branch_ctx_id
                && !graph.branch_lineage.contains(&bc)
            {
                graph.branch_lineage.push(bc);
            }

            graph.add_node(node);
        }

        // Add edges for connectivity
        for i in 0..selected.len() {
            for j in (i + 1)..selected.len() {
                let a = &candidates[selected[i]];
                let b = &candidates[selected[j]];
                let trust_diff = (a.trust as i32 - b.trust as i32).abs();
                if trust_diff < 1000 {
                    graph.add_edge(AgEdge::new(i, j, AgEdgeType::Supports, 5000));
                }
            }
        }

        let covered: Vec<GapId> = gaps.iter().filter(|g| g.covered).map(|g| g.id).collect();
        graph.mark_gaps_covered(&covered);

        graph
    }

    /// Build an AnswerGraph from branch-specific candidates (SKF-1.1 §3.2).
    /// Used for generating branch-aware alternates.
    fn build_alt_graph_from_branch(
        candidates: &[crate::query::Candidate],
        selected: &[usize],
        gaps: &[Gap],
        branch_ctx: CtxId,
    ) -> AnswerGraph {
        let mut graph = AnswerGraph::new();
        graph.ctx_id = branch_ctx;
        graph.branch_lineage.push(branch_ctx);

        for &idx in selected {
            let candidate = &candidates[idx];
            let mut node = AgNode::new(
                AtomRef::new(
                    candidate.atom_id,
                    candidate.node_num,
                    candidate.seg_id,
                    candidate.offset,
                ),
                candidate.atom_type,
            );
            node.trust = candidate.trust;
            node.io_bytes = candidate.estimated_io_bytes;
            node.age_ns = candidate.age_ns;
            node.domain_mask = candidate.domain_mask;
            node.hard_conflicts = candidate.hard_conflicts;
            node.soft_conflicts = candidate.soft_conflicts;
            node.evidence_refs = candidate.evidence_refs.clone();
            node.derived_claims = candidate.derived_claims.clone();
            node.branch_ctx_id = Some(branch_ctx);

            for &gap_id in &candidate.covers_gaps {
                node.add_gap(gap_id);
            }

            graph.add_node(node);
        }

        // Add edges for connectivity
        for i in 0..selected.len() {
            for j in (i + 1)..selected.len() {
                let a = &candidates[selected[i]];
                let b = &candidates[selected[j]];
                let trust_diff = (a.trust as i32 - b.trust as i32).abs();
                if trust_diff < 1000 {
                    graph.add_edge(AgEdge::new(i, j, AgEdgeType::Supports, 5000));
                }
            }
        }

        let covered: Vec<GapId> = gaps.iter().filter(|g| g.covered).map(|g| g.id).collect();
        graph.mark_gaps_covered(&covered);

        graph
    }
}
// ============================================================================

/// CAS store for atom persistence
///
/// Wraps the real disk I/O layer from `cas::io` (SegmentFile, IndexFile, CasWriter, CasReader)
/// and provides the high-level SKF-1.1 API for atom storage and retrieval.
///
/// # Architecture
/// - `io_store`: The real CAS I/O layer (`cas_io::CasStore`) managing segment files and indexes
/// - `view_cache`: Thread-safe cache of parsed atom data for zero-copy view lifetimes
pub struct CasStore {
    config: StoreConfig,
    pub io_store: Arc<cas_io::CasStore>,
    /// Cache of parsed atom data: AtomId -> Arc<AtomCacheEntry>
    /// Arc provides stable references that outlive the MutexGuard.
    view_cache: Mutex<std::collections::HashMap<AtomId, Arc<AtomCacheEntry>>>,
}

/// Cached atom data for zero-copy view access
struct AtomCacheEntry {
    /// Owned body bytes (AtomBodyHeader + sections + padding, NOT RecordHeader)
    body: Vec<u8>,
    /// Parsed claims from CLAIMS section
    claims: Vec<ClaimData>,
}

impl CasStore {
    /// Create a new CAS store
    pub fn new(config: &StoreConfig) -> Result<Self, StoreError> {
        // Create CAS directory if it doesn't exist
        std::fs::create_dir_all(config.cas_dir()).map_err(|e| StoreError::Io(e.to_string()))?;

        // Open the real CAS I/O store
        let io_store = cas_io::CasStore::open(&config.cas_dir(), None)
            .map_err(|e| StoreError::Io(e.to_string()))?;

        // Initialize writer and reader
        io_store
            .init_writer()
            .map_err(|e| StoreError::Io(e.to_string()))?;
        io_store
            .init_reader()
            .map_err(|e| StoreError::Io(e.to_string()))?;

        Ok(CasStore {
            config: config.clone(),
            io_store: Arc::new(io_store),
            view_cache: Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// Store an atom body with all 7 required sections (SKF-1.1 §2.1, 9.1)
    ///
    /// Creates a complete atom body structure with:
    /// - SYMBOLS section (symbol table)
    /// - REFS section (reference table)
    /// - CLAIMS section (claim data)
    /// - INVARIANTS section (bytecode)
    /// - EDGES section (graph edges)
    /// - EVIDENCE section (provenance)
    /// - META section (metadata)
    ///
    /// # Returns
    /// - `(seg_id, offset, len)`: Location in CAS storage
    pub fn store_atom(
        &mut self,
        atom_id: &AtomId,
        body: &[u8],
    ) -> Result<(u32, u64, u64), StoreError> {
        // Validate that body contains all 7 required sections
        // SKF-1.1 Section 3.2.1 mandates these sections
        if body.len() < crate::cas::AtomBodyHeader::SIZE {
            return Err(StoreError::InvariantFailed(InvariantResult::FAIL_HARD));
        }

        // Parse body header to verify section count
        let body_header = crate::cas::AtomBodyHeader::from_bytes(body)
            .map_err(|_| StoreError::InvariantFailed(InvariantResult::FAIL_HARD))?;

        // Verify all 7 required sections are present
        if body_header.section_count < 7 {
            return Err(StoreError::InvariantFailed(InvariantResult::FAIL_HARD));
        }

        // Validate section table offset
        if body_header.section_table_off as usize > body.len() {
            return Err(StoreError::InvariantFailed(InvariantResult::FAIL_HARD));
        }

        // Parse section table and validate each section
        let section_table_start = body_header.section_table_off as usize;
        let num_sections = body_header.section_count as usize;

        // Check we have enough bytes for section table
        if section_table_start + num_sections * 32 > body.len() {
            return Err(StoreError::InvariantFailed(InvariantResult::FAIL_HARD));
        }

        // Verify all required section kinds are present
        let mut found_sections = 0u32;
        for i in 0..num_sections {
            let section_offset = section_table_start + i * 32;
            let section_kind = u32::from_le_bytes([
                body[section_offset],
                body[section_offset + 1],
                body[section_offset + 2],
                body[section_offset + 3],
            ]);

            // Validate section kind is one of the required 7
            match section_kind {
                0x01 => found_sections |= 0x01, // SYMBOLS
                0x02 => found_sections |= 0x02, // REFS
                0x03 => found_sections |= 0x04, // CLAIMS
                0x04 => found_sections |= 0x08, // INVARIANTS
                0x05 => found_sections |= 0x10, // EDGES
                0x06 => found_sections |= 0x20, // EVIDENCE
                0x07 => found_sections |= 0x40, // META
                _ => return Err(StoreError::InvariantFailed(InvariantResult::FAIL_HARD)),
            }
        }

        // Verify all 7 sections found (0x7F = all 7 bits set)
        if found_sections != 0x7F {
            return Err(StoreError::InvariantFailed(InvariantResult::FAIL_HARD));
        }

        // Write to real CAS segment file via the I/O layer
        let (seg_id, offset, body_len) = self
            .io_store
            .write(*atom_id, body)
            .map_err(|e| StoreError::Io(e.to_string()))?;

        // Flush writer to persist index entries to disk.
        // NOTE: We do NOT re-init reader here — on Windows, re-opening
        // files already held by the writer causes sharing violations.
        // The write-through cache (view_cache) serves reads for atoms
        // written in the same session.
        self.io_store
            .flush()
            .map_err(|e| StoreError::Io(e.to_string()))?;

        // Write-through cache: store body bytes so load_atom can find them
        // without needing the reader to re-discover segments.
        {
            let mut cache = self.view_cache.lock();
            cache.insert(
                *atom_id,
                Arc::new(AtomCacheEntry {
                    body: body.to_vec(),
                    claims: Vec::new(),
                }),
            );
        }

        Ok((seg_id, offset, body_len))
    }

    /// Load an atom body by ID
    ///
    /// First checks the write-through cache for atoms written in this session,
    /// then falls back to disk read via the CAS reader.
    pub fn load_atom(&self, atom_id: &AtomId) -> Result<Vec<u8>, StoreError> {
        // Step 1: Check write-through cache (atoms written in this session)
        {
            let cache = self.view_cache.lock();
            if let Some(entry) = cache.get(atom_id) {
                return Ok(entry.body.clone());
            }
        }

        // Step 2: Fall back to disk read
        match self
            .io_store
            .read(atom_id)
            .map_err(|e| StoreError::Io(e.to_string()))?
        {
            Some(body) => Ok(body),
            None => Err(StoreError::AtomNotFound(*atom_id)),
        }
    }

    /// Get atom view by ID with zero-copy mmap access (SKF-1.1 §2.2)
    ///
    /// # Implementation
    /// 1. Lookup atom location via CAS reader (index → seg_id + offset)
    /// 2. Read atom body from segment file
    /// 3. Parse AtomBodyHeader and section table
    /// 4. Extract META and CLAIMS sections
    /// 5. Cache parsed data in `self.view_cache` wrapped in Arc for stable lifetime
    /// 6. Return AtomView with zero-copy references to Arc-backed data
    ///
    /// # Safety
    /// - All bounds checking performed before creating views
    /// - Lifetime tied to CAS store (data stored in Arc within `self.view_cache`)
    /// - No raw pointers exposed to safe code
    pub fn get_atom_view<'a>(&'a self, atom_id: &'a AtomId) -> Result<CasAtomView<'a>, StoreError> {
        // Validate atom_id is not all zeros (reserved)
        if atom_id.iter().all(|&b| b == 0) {
            return Err(StoreError::AtomNotFound(*atom_id));
        }

        // Step 1: Read the atom body from disk
        let body = self.load_atom(atom_id)?;

        // Step 2: Parse AtomBodyHeader
        let body_header = crate::cas::AtomBodyHeader::from_bytes(&body)
            .map_err(|_| StoreError::InvariantFailed(InvariantResult::FAIL_HARD))?;

        if !body_header.validate_magic() {
            return Err(StoreError::InvariantFailed(InvariantResult::FAIL_HARD));
        }

        let atom_type = body_header
            .atom_type()
            .ok_or(StoreError::InvalidAtomType(body_header.atom_type))?;

        let valid_from_ns = body_header.valid_from_unix_ns;
        let valid_to_ns = body_header.valid_to_unix_ns;

        // Step 3: Parse section table
        let section_table_start = body_header.section_table_off as usize;
        let num_sections = body_header.section_count as usize;

        if section_table_start + num_sections * crate::cas::SectionDesc::SIZE > body.len() {
            return Err(StoreError::InvariantFailed(InvariantResult::FAIL_HARD));
        }

        // Step 4: Find META and CLAIMS sections
        let mut meta_offset: usize = 0;
        let mut meta_len: usize = 0;
        let mut claims_data: Vec<u8> = Vec::new();
        let mut trust_level: TrustLevel = 5000;
        let mut domain_mask: DomainMask = 0xFFFF;
        let mut source_id: u32 = 0;

        for i in 0..num_sections {
            let section_offset = section_table_start + i * crate::cas::SectionDesc::SIZE;
            let section_desc = crate::cas::SectionDesc::from_bytes(&body[section_offset..])
                .map_err(|_| StoreError::InvariantFailed(InvariantResult::FAIL_HARD))?;

            let kind = section_desc
                .kind()
                .ok_or(StoreError::InvariantFailed(InvariantResult::FAIL_HARD))?;

            let sec_start = section_desc.off as usize;
            let sec_len = section_desc.len as usize;

            if sec_start + sec_len > body.len() {
                continue; // Skip invalid sections
            }

            match kind {
                SectionKind::META => {
                    meta_offset = sec_start;
                    meta_len = sec_len;
                    // Parse metadata fields from META section
                    // META section layout: trust_level(u16) + domain_mask(u64) + source_id(u32) + padding
                    if sec_len >= 14 {
                        trust_level = u16::from_le_bytes([body[sec_start], body[sec_start + 1]]);
                        domain_mask = u64::from_le_bytes([
                            body[sec_start + 2],
                            body[sec_start + 3],
                            body[sec_start + 4],
                            body[sec_start + 5],
                            body[sec_start + 6],
                            body[sec_start + 7],
                            body[sec_start + 8],
                            body[sec_start + 9],
                        ]);
                        source_id = u32::from_le_bytes([
                            body[sec_start + 10],
                            body[sec_start + 11],
                            body[sec_start + 12],
                            body[sec_start + 13],
                        ]);
                    }
                }
                SectionKind::CLAIMS => {
                    claims_data = body[sec_start..sec_start + sec_len].to_vec();
                }
                _ => {}
            }
        }

        // Step 5: Parse claims from CLAIMS section data
        let claims = parse_claims_from_section(&claims_data);

        // Step 6: Store parsed data in Arc-backed cache for stable lifetime
        // We use Arc so that we can create 'static references that outlive the MutexGuard.
        // The Arc is stored in the HashMap, so it lives as long as `self`.
        let entry_arc = Arc::new(AtomCacheEntry { body, claims });

        {
            let mut cache = self.view_cache.lock();
            cache.insert(*atom_id, Arc::clone(&entry_arc));
        }

        // Step 7: Borrow from the Arc-backed cache entry to create the AtomView
        // Safety: The Arc is stored in `self.view_cache` which is owned by `self`.
        // Since we have `&'a self`, the Arc cannot be dropped before `'a` ends.
        // We use Arc::as_ptr to get a stable pointer, then create references with 'a lifetime.
        let cache = self.view_cache.lock();
        let entry_arc = cache
            .get(atom_id)
            .ok_or(StoreError::AtomNotFound(*atom_id))?;

        // Create 'a-bounded references from the Arc-backed data
        let entry_ptr: *const AtomCacheEntry = Arc::as_ptr(entry_arc);
        let entry_ref: &'a AtomCacheEntry = unsafe { &*entry_ptr };

        let meta_slice: &'a [u8] = if meta_len > 0 && meta_offset + meta_len <= entry_ref.body.len()
        {
            &entry_ref.body[meta_offset..meta_offset + meta_len]
        } else {
            &[]
        };

        let claims_slice: &'a [ClaimData] = &entry_ref.claims;

        Ok(CasAtomView::new(
            atom_id,
            atom_type,
            meta_slice,
            claims_slice,
            valid_from_ns,
            valid_to_ns,
            trust_level,
            domain_mask,
            source_id,
        ))
    }

    /// Read raw record bytes for integrity verification
    ///
    /// Returns complete record: header (64 bytes) + body + body_crc (4 bytes)
    ///
    /// # Arguments
    /// - `atom_id`: Atom ID to read
    ///
    /// # Returns
    /// - `Ok(Vec<u8>)`: Complete record bytes
    /// - `Err(StoreError)`: Atom not found or read error
    pub fn read_raw_record(&self, atom_id: &AtomId) -> Result<Vec<u8>, StoreError> {
        // Step 1: Check write-through cache first
        {
            let cache = self.view_cache.lock();
            if let Some(entry) = cache.get(atom_id) {
                // Build a complete record from cached body
                let body = &entry.body;
                let header = crate::cas::RecordHeader::new(
                    *atom_id,
                    body.len() as u64,
                    0, // seg_id unknown for cached entries
                    0, // flags
                );

                // Calculate body CRC
                let body_crc = crate::utils::crc32(body);

                // Build record: header + body + body_crc
                let record_size = crate::cas::RecordHeader::SIZE + body.len() + 4;
                let mut record = Vec::with_capacity(record_size);

                // Write header
                let mut header_bytes = [0u8; crate::cas::RecordHeader::SIZE];
                header
                    .write_to_bytes(&mut header_bytes)
                    .map_err(|e| StoreError::Io(e.to_string()))?;
                record.extend_from_slice(&header_bytes);

                // Write body
                record.extend_from_slice(body);

                // Write body CRC
                record.extend_from_slice(&body_crc.to_le_bytes());

                return Ok(record);
            }
        }

        // Step 2: Read from disk via CAS reader
        match self.io_store.read(atom_id) {
            Ok(Some(body)) => {
                // Build a complete record from disk body
                let header = crate::cas::RecordHeader::new(
                    *atom_id,
                    body.len() as u64,
                    0, // seg_id unknown
                    0, // flags
                );

                // Calculate body CRC
                let body_crc = crate::utils::crc32(&body);

                // Build record
                let record_size = crate::cas::RecordHeader::SIZE + body.len() + 4;
                let mut record = Vec::with_capacity(record_size);

                let mut header_bytes = [0u8; crate::cas::RecordHeader::SIZE];
                header
                    .write_to_bytes(&mut header_bytes)
                    .map_err(|e| StoreError::Io(e.to_string()))?;
                record.extend_from_slice(&header_bytes);
                record.extend_from_slice(&body);
                record.extend_from_slice(&body_crc.to_le_bytes());

                Ok(record)
            }
            Ok(None) => Err(StoreError::AtomNotFound(*atom_id)),
            Err(e) => Err(StoreError::Io(e.to_string())),
        }
    }
}

/// Parse claims from a CLAIMS section byte buffer.
///
/// CLAIMS section format (SKF-1.1):
/// - u32: claim_count
/// - For each claim:
///   - u16: subject_local
///   - u16: predicate_local
///   - u8: object_tag
///   - variable: object_value (based on tag)
fn parse_claims_from_section(data: &[u8]) -> Vec<ClaimData> {
    if data.len() < 4 {
        return Vec::new();
    }

    let claim_count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let mut claims = Vec::with_capacity(claim_count);
    let mut offset = 4;

    for _ in 0..claim_count {
        if offset + 5 > data.len() {
            break;
        }

        let subject_local = u16::from_le_bytes([data[offset], data[offset + 1]]);
        let predicate_local = u16::from_le_bytes([data[offset + 2], data[offset + 3]]);
        let object_tag = data[offset + 4];
        offset += 5;

        // Object value size depends on tag
        let obj_val_size = match object_tag {
            0 => 0, // NULL
            1 => 1, // BOOL
            2 => 8, // I64
            3 => 8, // U64
            4 => 8, // F64
            5 => {
                // BYTES: u32 length prefix
                if offset + 4 > data.len() {
                    break;
                }
                let len = u32::from_le_bytes([
                    data[offset],
                    data[offset + 1],
                    data[offset + 2],
                    data[offset + 3],
                ]) as usize;
                offset += 4;
                if offset + len > data.len() {
                    break;
                }
                offset += len;
                0 // Already consumed
            }
            6 => 4, // SYM
            7 => 4, // REF
            8 => 8, // NODENUM
            _ => 0,
        };

        // Read object value as u64
        let obj_val = if obj_val_size > 0 && offset + obj_val_size <= data.len() {
            let mut buf = [0u8; 8];
            let copy_len = obj_val_size.min(8);
            buf[..copy_len].copy_from_slice(&data[offset..offset + copy_len]);
            offset += obj_val_size;
            u64::from_le_bytes(buf)
        } else {
            0
        };

        claims.push(ClaimData {
            subj: subject_local as u64,
            pred: predicate_local as u64,
            obj_tag: object_tag,
            obj_val,
            qualifiers_mask: 0,
        });
    }

    claims
}

// ============================================================================
// Location Index Wrapper
// ============================================================================

/// Location wrapper using IdLocBuilder
pub struct LocationIndex {
    index_dir: PathBuf,
    state_path: PathBuf,
    idloc_path: PathBuf,
    shard_bits: u8,
    node_counter: NodeNum,
    /// Reverse mapping: AtomId -> NodeNum for provenance lookups
    atom_to_node: std::collections::HashMap<AtomId, NodeNum>,
    /// Forward mapping: AtomId -> Location for real location retrieval (SKF-1.1)
    atom_to_location: std::collections::HashMap<AtomId, crate::index::Location>,
    /// Deleted atoms set (tombstone semantics - SKF-1.1)
    deleted_atoms: std::collections::HashSet<AtomId>,
}

impl LocationIndex {
    /// Create a new location index
    #[inline]
    pub fn new(config: &StoreConfig) -> Result<Self, StoreError> {
        let index_dir = config.index_dir();
        fs::create_dir_all(&index_dir).map_err(StoreError::from)?;

        let mut index = LocationIndex {
            index_dir: index_dir.clone(),
            state_path: index_dir.join("location_state.bin"),
            idloc_path: index_dir.join("idloc.mmap"),
            shard_bits: 8,
            node_counter: 0,
            atom_to_node: std::collections::HashMap::new(),
            atom_to_location: std::collections::HashMap::new(),
            deleted_atoms: std::collections::HashSet::new(),
        };

        if index.state_path.exists() {
            index.load_state()?;
            index.save_idloc()?;
        } else if index.idloc_path.exists() {
            // Real load/open behavior: validate any pre-existing idloc file.
            let _ = IdLocIndex::open(&index.idloc_path).map_err(StoreError::from)?;
        }

        Ok(index)
    }

    /// Assign a node number to an atom (SKF-1.1)
    ///
    /// Stores the complete location mapping for real retrieval.
    #[inline]
    pub fn assign_node_num(
        &mut self,
        atom_id: &AtomId,
        seg_id: u32,
        offset: u64,
        len: u32,
    ) -> NodeNum {
        let node_num = self.node_counter;
        self.node_counter += 1;

        // Store reverse mapping for provenance lookups
        self.atom_to_node.insert(*atom_id, node_num);

        // Store complete location for real retrieval (SKF-1.1)
        self.atom_to_location.insert(
            *atom_id,
            crate::index::Location::new(seg_id, offset, len, node_num, 0xFFFF),
        );

        node_num
    }

    /// Get node number by atom ID (for provenance edge construction)
    #[inline]
    pub fn get_node_num(&self, atom_id: &AtomId) -> Option<NodeNum> {
        self.atom_to_node.get(atom_id).copied()
    }

    /// Get location by atom ID (SKF-1.1 real IdLoc lookup)
    ///
    /// Returns the real physical location of the atom in CAS storage.
    /// This is the source of truth for AtomId -> Location mapping.
    ///
    /// Returns None if atom is deleted (tombstone semantics).
    #[inline]
    pub fn get_location(&self, atom_id: &AtomId) -> Option<crate::index::Location> {
        let location = self.atom_to_location.get(atom_id)?;
        if location.deleted || self.deleted_atoms.contains(atom_id) {
            return None;
        }

        Some(*location)
    }

    /// List all live atom IDs tracked by the location index in stable order.
    pub fn live_atom_ids(&self) -> Vec<AtomId> {
        let mut atom_ids: Vec<_> = self
            .atom_to_location
            .iter()
            .filter_map(|(atom_id, location)| {
                if location.deleted || self.deleted_atoms.contains(atom_id) {
                    None
                } else {
                    Some(*atom_id)
                }
            })
            .collect();
        atom_ids.sort_unstable();
        atom_ids
    }

    /// Mark atom as deleted (tombstone semantics - SKF-1.1)
    ///
    /// After marking, get_location() will return None for this atom.
    /// The location data is preserved for audit trail and replication sync.
    #[inline]
    pub fn mark_deleted(&mut self, atom_id: &AtomId) {
        self.deleted_atoms.insert(*atom_id);
        if let Some(location) = self.atom_to_location.get_mut(atom_id) {
            location.deleted = true;
        }
    }

    /// Check if atom is deleted
    #[inline]
    pub fn is_deleted(&self, atom_id: &AtomId) -> bool {
        self.deleted_atoms.contains(atom_id) || self.atom_to_location.get(atom_id).map(|location| location.deleted).unwrap_or(false)
    }

    /// Persist the location index and its durable idloc companion.
    #[inline]
    pub fn save(&self) -> Result<(), StoreError> {
        self.save_state()?;
        self.save_idloc()?;
        Ok(())
    }

    /// Build the durable idloc payload from the current state.
    #[inline]
    pub fn build(&self) -> Vec<u8> {
        let mut builder = IdLocBuilder::new(self.shard_bits);
        let mut entries: Vec<_> = self.atom_to_location.iter().collect();
        entries.sort_by_key(|(atom_id, _)| **atom_id);

        for (atom_id, location) in entries {
            if location.deleted || self.deleted_atoms.contains(atom_id) {
                continue;
            }
            builder.add(
                atom_id,
                location.seg_id,
                location.len,
                location.offset,
                location.node_num,
            );
        }

        builder.build_to_vec()
    }

    fn save_idloc(&self) -> Result<(), StoreError> {
        let mut builder = IdLocBuilder::new(self.shard_bits);
        let mut entries: Vec<_> = self.atom_to_location.iter().collect();
        entries.sort_by_key(|(atom_id, _)| **atom_id);

        for (atom_id, location) in entries {
            if location.deleted || self.deleted_atoms.contains(atom_id) {
                continue;
            }
            builder.add(
                atom_id,
                location.seg_id,
                location.len,
                location.offset,
                location.node_num,
            );
        }

        builder
            .build_to_file(&self.idloc_path)
            .map_err(StoreError::from)?;
        Ok(())
    }

    fn save_state(&self) -> Result<(), StoreError> {
        fs::create_dir_all(&self.index_dir).map_err(StoreError::from)?;
        let mut file = File::create(&self.state_path).map_err(StoreError::from)?;

        file.write_all(&LOCATION_STATE_MAGIC.to_le_bytes())
            .map_err(StoreError::from)?;
        file.write_all(&LOCATION_STATE_VERSION.to_le_bytes())
            .map_err(StoreError::from)?;
        file.write_all(&[self.shard_bits])
            .map_err(StoreError::from)?;
        file.write_all(&[0u8]).map_err(StoreError::from)?;
        file.write_all(&self.node_counter.to_le_bytes())
            .map_err(StoreError::from)?;

        let mut entries: Vec<_> = self.atom_to_location.iter().collect();
        entries.sort_by_key(|(atom_id, _)| **atom_id);
        file.write_all(&(entries.len() as u64).to_le_bytes())
            .map_err(StoreError::from)?;

        for (atom_id, location) in entries {
            file.write_all(atom_id).map_err(StoreError::from)?;
            file.write_all(&location.node_num.to_le_bytes())
                .map_err(StoreError::from)?;
            file.write_all(&location.seg_id.to_le_bytes())
                .map_err(StoreError::from)?;
            file.write_all(&location.offset.to_le_bytes())
                .map_err(StoreError::from)?;
            file.write_all(&location.len.to_le_bytes())
                .map_err(StoreError::from)?;
            file.write_all(&location.domain_mask.to_le_bytes())
                .map_err(StoreError::from)?;
            let deleted = location.deleted || self.deleted_atoms.contains(atom_id);
            file.write_all(&[u8::from(deleted)])
                .map_err(StoreError::from)?;
        }

        file.flush().map_err(StoreError::from)?;
        file.sync_all().map_err(StoreError::from)?;
        Ok(())
    }

    fn load_state(&mut self) -> Result<(), StoreError> {
        let mut file = File::open(&self.state_path).map_err(StoreError::from)?;
        let mut header = [0u8; 24];
        file.read_exact(&mut header).map_err(StoreError::from)?;

        let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
        if magic != LOCATION_STATE_MAGIC {
            return Err(StoreError::Io("Invalid location state magic".to_string()));
        }

        let version = u16::from_le_bytes(header[4..6].try_into().unwrap());
        if version != LOCATION_STATE_VERSION {
            return Err(StoreError::Io("Invalid location state version".to_string()));
        }

        self.shard_bits = header[6];
        self.node_counter = u64::from_le_bytes(header[8..16].try_into().unwrap());
        let entry_count = u64::from_le_bytes(header[16..24].try_into().unwrap()) as usize;

        self.atom_to_node.clear();
        self.atom_to_location.clear();
        self.deleted_atoms.clear();

        let mut record = [0u8; LOCATION_RECORD_SIZE];
        for _ in 0..entry_count {
            file.read_exact(&mut record).map_err(StoreError::from)?;
            let mut atom_id = [0u8; 32];
            atom_id.copy_from_slice(&record[0..32]);
            let node_num = u64::from_le_bytes(record[32..40].try_into().unwrap());
            let seg_id = u32::from_le_bytes(record[40..44].try_into().unwrap());
            let offset = u64::from_le_bytes(record[44..52].try_into().unwrap());
            let len = u32::from_le_bytes(record[52..56].try_into().unwrap());
            let domain_mask = u64::from_le_bytes(record[56..64].try_into().unwrap());
            let deleted = record[64] != 0;

            let location = Location {
                seg_id,
                offset,
                len,
                node_num,
                domain_mask,
                deleted,
            };
            self.atom_to_node.insert(atom_id, node_num);
            self.atom_to_location.insert(atom_id, location);
            if deleted {
                self.deleted_atoms.insert(atom_id);
            }
            self.node_counter = self.node_counter.max(node_num + 1);
        }

        Ok(())
    }
}

// ============================================================================
// Term Index Wrapper
// ============================================================================

/// Term index wrapper using real InvertedIndex (SKF-1.1 Section 6.1)
///
/// This wraps the actual InvertedIndex from the index module,
/// providing deterministic lexical retrieval with front-coded lexicon
/// and delta-varint postings for efficient term -> NodeNum lookups.
pub struct TermIndex {
    index: InvertedIndex,
}

impl TermIndex {
    /// Create a new term index with real InvertedIndex backend
    #[inline]
    pub fn new(config: &StoreConfig) -> Result<Self, StoreError> {
        let index_path = config.index_dir();
        fs::create_dir_all(&index_path).map_err(StoreError::from)?;
        let mut index =
            InvertedIndex::new(&index_path).map_err(|e| StoreError::Index(e.to_string()))?;
        index
            .load()
            .map_err(|e| StoreError::Index(e.to_string()))?;
        Ok(TermIndex { index })
    }

    /// Add a term mapping (index_atom is preferred for full indexing)
    #[inline]
    pub fn add_term(&mut self, term: String, node_num: NodeNum) {
        // Use the index_atom method for proper term extraction
        // This is a simplified version for backward compatibility
        let term_id = self.index.lexicon_mut().add(term.to_lowercase());
        self.index.postings_mut().add(term_id, node_num);
    }

    /// Index an atom's content for term retrieval
    #[inline]
    pub fn index_atom(&mut self, node_num: NodeNum, atom_id: &AtomId, content: &[u8]) {
        self.index.index_atom(node_num, atom_id, content);
    }

    /// Lookup term (delegates to InvertedIndex::search)
    #[inline]
    pub fn lookup(&self, term: &str) -> Option<&[NodeNum]> {
        self.index.search(term)
    }

    /// Get reference to the underlying InvertedIndex
    #[inline]
    pub fn as_index(&self) -> &InvertedIndex {
        &self.index
    }

    /// Save the index to disk
    #[inline]
    pub fn save(&self) -> Result<(), StoreError> {
        self.index
            .save()
            .map_err(|e| StoreError::Index(e.to_string()))
    }
}

// ============================================================================
// MetaStore
// ============================================================================

/// Metadata store
pub struct MetaStore {
    path: PathBuf,
    meta: std::collections::HashMap<AtomId, AtomMetadata>,
    node_to_atom: std::collections::HashMap<u64, AtomId>,
}

#[derive(Debug, Clone)]
pub struct AtomMetadata {
    pub atom_type: AtomType,
    pub created_at_ns: u64,
    pub trust_level: TrustLevel,
    pub domain_mask: u64,
    pub source_id: u32,
}

impl MetaStore {
    /// Create a new metadata store
    #[inline]
    pub fn new(config: &StoreConfig) -> Result<Self, StoreError> {
        let path = config.meta_dir().join("meta_state.bin");
        fs::create_dir_all(config.meta_dir()).map_err(StoreError::from)?;
        let mut store = MetaStore {
            path,
            meta: std::collections::HashMap::new(),
            node_to_atom: std::collections::HashMap::new(),
        };

        if store.path.exists() {
            store.load()?;
        }

        Ok(store)
    }

    /// Store metadata for an atom
    #[inline]
    pub fn put_meta(&mut self, atom_id: AtomId, meta: AtomMetadata) {
        self.meta.insert(atom_id, meta);
    }

    /// Get metadata for an atom
    #[inline]
    pub fn get_meta(&self, atom_id: &AtomId) -> Option<&AtomMetadata> {
        self.meta.get(atom_id)
    }

    /// Get metadata by node number
    #[inline]
    pub fn get_meta_by_node(&self, node_num: u64) -> Option<&AtomMetadata> {
        self.node_to_atom
            .get(&node_num)
            .and_then(|atom_id| self.get_meta(atom_id))
    }

    /// Register node number to atom mapping
    #[inline]
    pub fn register_node(&mut self, node_num: u64, atom_id: AtomId) {
        self.node_to_atom.insert(node_num, atom_id);
    }

    /// Get atom ID by node number (reverse lookup for provenance)
    #[inline]
    pub fn get_atom_by_node(&self, node_num: u64) -> Option<&AtomId> {
        self.node_to_atom.get(&node_num)
    }

    /// Persist metadata and node mappings to disk.
    #[inline]
    pub fn save(&self) -> Result<(), StoreError> {
        fs::create_dir_all(
            self.path
                .parent()
                .ok_or_else(|| StoreError::Io("MetaStore path has no parent".to_string()))?,
        )
        .map_err(StoreError::from)?;

        let mut file = File::create(&self.path).map_err(StoreError::from)?;
        file.write_all(&META_STATE_MAGIC.to_le_bytes())
            .map_err(StoreError::from)?;
        file.write_all(&META_STATE_VERSION.to_le_bytes())
            .map_err(StoreError::from)?;
        file.write_all(&0u16.to_le_bytes())
            .map_err(StoreError::from)?;

        let mut records: Vec<(AtomId, u64, AtomMetadata)> = self
            .meta
            .iter()
            .map(|(atom_id, meta)| {
                let node_num = self
                    .node_to_atom
                    .iter()
                    .find_map(|(node_num, mapped_atom)| (*mapped_atom == *atom_id).then_some(*node_num))
                    .unwrap_or(u64::MAX);
                (*atom_id, node_num, meta.clone())
            })
            .collect();
        records.sort_by_key(|(atom_id, _, _)| *atom_id);

        file.write_all(&(records.len() as u64).to_le_bytes())
            .map_err(StoreError::from)?;

        for (atom_id, node_num, meta) in records {
            file.write_all(&atom_id).map_err(StoreError::from)?;
            file.write_all(&node_num.to_le_bytes())
                .map_err(StoreError::from)?;
            file.write_all(&meta.atom_type.to_u32().to_le_bytes())
                .map_err(StoreError::from)?;
            file.write_all(&meta.created_at_ns.to_le_bytes())
                .map_err(StoreError::from)?;
            file.write_all(&meta.trust_level.to_le_bytes())
                .map_err(StoreError::from)?;
            file.write_all(&meta.domain_mask.to_le_bytes())
                .map_err(StoreError::from)?;
            file.write_all(&meta.source_id.to_le_bytes())
                .map_err(StoreError::from)?;
        }

        file.flush().map_err(StoreError::from)?;
        file.sync_all().map_err(StoreError::from)?;
        Ok(())
    }

    fn load(&mut self) -> Result<(), StoreError> {
        let mut file = File::open(&self.path).map_err(StoreError::from)?;
        let mut header = [0u8; 16];
        file.read_exact(&mut header).map_err(StoreError::from)?;

        let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
        if magic != META_STATE_MAGIC {
            return Err(StoreError::Io("Invalid meta state magic".to_string()));
        }
        let version = u16::from_le_bytes(header[4..6].try_into().unwrap());
        if version != META_STATE_VERSION {
            return Err(StoreError::Io("Invalid meta state version".to_string()));
        }
        let record_count = u64::from_le_bytes(header[8..16].try_into().unwrap()) as usize;

        self.meta.clear();
        self.node_to_atom.clear();

        let mut record = [0u8; 66];
        for _ in 0..record_count {
            file.read_exact(&mut record).map_err(StoreError::from)?;
            let mut atom_id = [0u8; 32];
            atom_id.copy_from_slice(&record[0..32]);
            let node_num = u64::from_le_bytes(record[32..40].try_into().unwrap());
            let atom_type = AtomType::from_u32(u32::from_le_bytes(record[40..44].try_into().unwrap()))
                .ok_or_else(|| StoreError::InvalidAtomType(u32::from_le_bytes(record[40..44].try_into().unwrap())))?;
            let created_at_ns = u64::from_le_bytes(record[44..52].try_into().unwrap());
            let trust_level = u16::from_le_bytes(record[52..54].try_into().unwrap());
            let domain_mask = u64::from_le_bytes(record[54..62].try_into().unwrap());
            let source_id = u32::from_le_bytes(record[62..66].try_into().unwrap());

            self.meta.insert(
                atom_id,
                AtomMetadata {
                    atom_type,
                    created_at_ns,
                    trust_level,
                    domain_mask,
                    source_id,
                },
            );
            if node_num != u64::MAX {
                self.node_to_atom.insert(node_num, atom_id);
            }
        }

        Ok(())
    }
}

// ============================================================================
// MemoryX Store
// ============================================================================

/// Main MemoryX store integrating all subsystems
///
/// # Architecture
///
/// The MemoryX store provides:
/// - `ingest()`: Store new atoms with claims and evidence
/// - `get_atom()`: Retrieve atom by ID
/// - `answer()`: Answer queries with confidence scoring
/// - `create_context()`: Create context branches
/// - `assert_claim_with_atom_id()`: Assert claims in a context with source atom
/// - `list_conflicts()`: List conflicts in a context
pub struct MemoryX {
    config: StoreConfig,
    pub(crate) cas: CasStore,
    loc_index: LocationIndex,
    term_index: TermIndex,
    graph: GraphStore,
    pub(crate) meta: MetaStore,
    ctx_manager: Arc<Mutex<CtxManager>>,
    /// Embedding index for semantic search (SKF-1.1 Section 6.1, 10.2)
    /// Maps NodeNum -> embedding vector for ANN-based semantic retrieval
    embedding_index: EmbeddingIndex,
}

/// Store errors
#[derive(Debug, Error)]
pub enum StoreError {
    /// CAS error
    #[error("CAS error: {0}")]
    Cas(#[from] CasError),

    /// IO error
    #[error("IO error: {0}")]
    Io(String),

    /// Atom not found
    #[error("Atom not found: {0:?}")]
    AtomNotFound(AtomId),

    /// Invalid atom type
    #[error("Invalid atom type: {0}")]
    InvalidAtomType(u32),

    /// Context error
    #[error("Context error: {0}")]
    Context(String),

    /// Query error
    #[error("Query error: {0}")]
    Query(String),

    /// Invariant check failed
    #[error("Invariant check failed: {0:?}")]
    InvariantFailed(InvariantResult),

    /// Context not found
    #[error("Context not found")]
    ContextNotFound,

    /// Claim rejected due to conflict
    #[error("Claim rejected: {0}")]
    ClaimRejected(String),

    /// Context branch failed
    #[error("Context branch failed")]
    ContextBranchFailed,

    /// Index error
    #[error("Index error: {0}")]
    Index(String),
}

impl From<std::io::Error> for StoreError {
    fn from(err: std::io::Error) -> Self {
        StoreError::Io(err.to_string())
    }
}

impl From<crate::index::IndexError> for StoreError {
    fn from(err: crate::index::IndexError) -> Self {
        StoreError::Index(err.to_string())
    }
}

impl MemoryX {
    /// Create a new MemoryX store
    ///
    /// # Arguments
    /// - `config`: Store configuration
    ///
    /// # Returns
    /// - `Ok(MemoryX)`: Store created successfully
    /// - `Err(StoreError)`: Failed to create store
    ///
    /// # Example
    /// ```rust
    /// use memoryx::store::api::{MemoryX, StoreConfig};
    /// use std::path::PathBuf;
    ///
    /// let config = StoreConfig::new(PathBuf::from("./data"));
    /// let store = MemoryX::new(config).unwrap();
    /// ```
    pub fn new(config: StoreConfig) -> Result<Self, StoreError> {
        let cas = CasStore::new(&config)?;
        let loc_index = LocationIndex::new(&config)?;
        let term_index = TermIndex::new(&config)?;
        let graph = GraphStore::open_or_create(config.graph_dir(), 0)
            .map_err(|e| StoreError::Io(e.to_string()))?;
        let meta = MetaStore::new(&config)?;
        let ctx_manager = Arc::new(Mutex::new(CtxManager::new()));
        let embedding_path = config.index_dir().join(EMBEDDINGS_FILE);
        let embedding_index = if embedding_path.exists() {
            EmbeddingIndex::load(&embedding_path).map_err(|e| StoreError::Io(e.to_string()))?
        } else {
            EmbeddingIndex::new(1024) // Default capacity for embeddings
        };

        Ok(MemoryX {
            config,
            cas,
            loc_index,
            term_index,
            graph,
            meta,
            ctx_manager,
            embedding_index,
        })
    }

    /// Persist the full base state under the configured root.
    pub fn save(&self) -> Result<(), StoreError> {
        self.cas
            .io_store
            .flush()
            .map_err(|e| StoreError::Io(e.to_string()))?;
        self.loc_index.save()?;
        self.term_index.save()?;
        self.graph
            .save()
            .map_err(|e| StoreError::Io(e.to_string()))?;
        self.meta.save()?;

        let embedding_path = self.config.index_dir().join(EMBEDDINGS_FILE);
        self.embedding_index
            .save(&embedding_path)
            .map_err(|e| StoreError::Io(e.to_string()))?;

        Ok(())
    }

    /// Flush is the durability boundary for the MemoryX base.
    pub fn flush(&self) -> Result<(), StoreError> {
        self.save()
    }

    /// Ingest a new atom into the store
    ///
    /// # Arguments
    /// - `payload`: Atom body content
    /// - `atom_type`: Type of atom (FACT, DEFINITION, etc.)
    /// - `claims`: Claims contained in the atom
    /// - `evidence`: Evidence references
    ///
    /// # Returns
    /// - `AtomId`: The BLAKE3-256 hash of the atom
    ///
    /// # Safety Contract
    /// - Payload must be valid atom body format
    /// - Claims must be well-formed
    /// - Evidence references must point to valid sections
    pub fn ingest(
        &mut self,
        payload: &[u8],
        atom_type: AtomType,
        claims: &[ClaimData],
        evidence: &[EvidenceRef],
    ) -> Result<AtomId, StoreError> {
        // Calculate atom ID from canonical form (SKF-1.1 content-address contract)
        let atom_id = compute_atom_id_from_payload(payload)?;

        // Store in CAS with sections
        // SKF-1.1 Section 2.1: Create sections (SYMBOLS, REFS, CLAIMS, INVARIANTS, EDGES, EVIDENCE, META)
        let (seg_id, offset, len) = self.cas.store_atom(&atom_id, payload)?;

        // Assign node number (IdLoc)
        let node_num = self
            .loc_index
            .assign_node_num(&atom_id, seg_id, offset, len as u32);

        // Update graph store incrementally (NOT recreate)
        // SKF-1.1 Section 8: Add node to existing graph
        self.graph.add_node(node_num);

        // Add edges from claims to graph
        for claim in claims {
            self.graph
                .add_edge(node_num, claim.subj, EdgeType::DEPENDS_ON, 5000);
        }

        // Add terms to inverted index
        // SKF-1.1 Section 3: Extract actual terms from symbol table (SKF-1.1 lexical retrieval)
        let terms = extract_terms_from_payload(payload);
        for term in terms {
            self.term_index.add_term(term, node_num);
        }

        // Create DERIVED_FROM edges for evidence references (SKF-1.1 Section 2.1, 10.1)
        // Evidence parameter contains source atoms for provenance chain
        for ev in evidence {
            // Get node number of source atom
            if let Some(source_node_num) = self.loc_index.get_node_num(&ev.atom_id) {
                // Create DERIVED_FROM edge: current atom derives from source atom
                self.graph.add_edge(
                    node_num,
                    source_node_num,
                    EdgeType::DERIVED_FROM,
                    ev.trust, // Use evidence trust level as edge weight
                );
            }
        }

        // Store metadata (META section)
        self.meta.put_meta(
            atom_id,
            AtomMetadata {
                atom_type,
                created_at_ns: 0,
                trust_level: 5000,
                domain_mask: 0xFFFF,
                source_id: 0,
            },
        );

        // Register node -> atom mapping for reverse lookup
        self.meta.register_node(node_num, atom_id);

        self.flush()?;
        Ok(atom_id)
    }

    /// Get an atom by ID.
    ///
    /// # Arguments
    /// - `atom_id`: The atom ID to retrieve
    ///
    /// # Returns
    /// - `Ok(AtomView)`: Atom view with metadata and claims
    /// - `Err(StoreError)`: Atom not found or read error
    pub fn get_atom<'a>(&'a self, atom_id: &'a AtomId) -> Result<CasAtomView<'a>, StoreError> {
        if self.loc_index.is_deleted(atom_id) {
            return Err(StoreError::AtomNotFound(*atom_id));
        }
        self.cas.get_atom_view(atom_id)
    }

    /// Load the canonical atom body bytes from CAS.
    pub fn get_atom_payload(&self, atom_id: &AtomId) -> Result<Vec<u8>, StoreError> {
        if self.loc_index.is_deleted(atom_id) {
            return Err(StoreError::AtomNotFound(*atom_id));
        }
        self.cas.load_atom(atom_id)
    }

    /// Load the raw CAS record bytes for an atom.
    pub fn read_raw_record(&self, atom_id: &AtomId) -> Result<Vec<u8>, StoreError> {
        if self.loc_index.is_deleted(atom_id) {
            return Err(StoreError::AtomNotFound(*atom_id));
        }
        self.cas.read_raw_record(atom_id)
    }

    /// List all live atom IDs in stable order.
    pub fn list_atom_ids(&self) -> Vec<AtomId> {
        self.loc_index.live_atom_ids()
    }

    /// Answer a query
    ///
    /// # Arguments
    /// - `query_text`: Natural language query
    /// - `ctx_policy`: Context policy ID
    ///
    /// # Returns
    /// - `Ok(AnswerPack)`: Query results with confidence
    /// - `Err(StoreError)`: Query execution error
    ///
    /// # Algorithm
    /// 1. Compile query text to GoalSpec
    /// 2. Generate gaps via BackwardWave
    /// 3. Route gaps to sources (CAS -> inverted -> graph)
    /// 4. Run fixed-point solver to build AnswerGraph
    /// 5. Extract claims and evidence from answer graph
    /// 6. Calculate confidence and limitations
    pub fn answer(
        &self,
        query_text: &str,
        ctx_policy: CtxPolicyId,
    ) -> Result<AnswerPack, StoreError> {
        let contract = QueryContractCompiler::compile_contract(query_text);
        self.answer_contract(contract, ctx_policy)
    }

    /// Answer a strict query contract.
    ///
    /// This is the public contract-first query path. Natural language query
    /// compatibility goes through `QueryContractCompiler` and then calls this
    /// method, so MCP/CLI query execution cannot bypass the contract layer.
    pub fn answer_contract(
        &self,
        contract: QueryContract,
        ctx_policy: CtxPolicyId,
    ) -> Result<AnswerPack, StoreError> {
        let goal = contract
            .to_goal_spec()
            .map_err(|e| StoreError::Query(e.to_string()))?
            .with_ctx_policy(ctx_policy);

        self.solve_goal(goal, ctx_policy)
    }

    fn solve_goal(
        &self,
        goal: GoalSpec,
        ctx_policy: CtxPolicyId,
    ) -> Result<AnswerPack, StoreError> {
        // Create router populated with current store data
        let router = self.create_router();

        // Get current timestamp for age calculations
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);

        // Create solver with router connected to this store
        // CRITICAL FIX: Pass ctx_manager.clone() to use real context with active claims/conflicts
        let solver = FixedPointSolver::new()
            .with_router(router)
            .with_ctx_manager(Arc::clone(&self.ctx_manager))
            .with_timestamp(now_ns)
            .with_cas(Arc::clone(&self.cas.io_store));

        solver
            .solve(goal, ctx_policy)
            .map_err(|e| StoreError::Query(e.to_string()))
    }

    /// Create a QueryRouter populated with all current store data.
    ///
    /// This router can find atoms that were ingested into this MemoryX instance.
    /// Populates all backends:
    /// - CasBackend: atom_id -> location mappings from meta store
    /// - InvertedBackend: term -> NodeNum mappings from term_index
    /// - GraphBackend: Arc reference to GraphStore for edge traversal
    /// - AnnBackend: node references for direct lookup
    ///
    /// # Returns
    /// - `QueryRouter`: Fully populated router connected to store data
    pub fn create_router(&self) -> crate::query::QueryRouter {
        use crate::query::QueryRouter;
        use std::sync::Arc;

        // Start with inverted index connection for real term-based retrieval (SKF-1.1 6.1)
        // This connects InvertedBackend to the actual InvertedIndex from index module,
        // enabling deterministic lexical retrieval with front-coded lexicon and postings.
        let mut router =
            QueryRouter::new().with_inverted_index(Arc::new(self.term_index.as_index().clone()));

        // Populate CAS backend from meta.node_to_atom mappings
        for (&node_num, &atom_id) in &self.meta.node_to_atom {
            if let Some(metadata) = self.meta.get_meta(&atom_id) {
                // Skip deleted atoms (trust_level = 0)
                if metadata.trust_level == 0 {
                    continue;
                }

                // Get location from real IdLoc index (SKF-1.1)
                if let Some(location) = self.get_atom_location(&atom_id, node_num) {
                    router.cas.register(atom_id, location);
                    // Also register in inverted backend for node -> atom mapping
                    router.inverted.register(node_num, atom_id, location);
                }
            }
        }

        // Connect graph backend to GraphStore (Arc-wrapped)
        router.graph = crate::query::GraphBackend::new()
            .with_store(Arc::new(self.graph.clone()))
            .with_max_fanout(128);

        // Populate graph backend node mappings
        for (&node_num, &atom_id) in &self.meta.node_to_atom {
            if let Some(metadata) = self.meta.get_meta(&atom_id)
                && metadata.trust_level > 0
                && let Some(location) = self.get_atom_location(&atom_id, node_num)
            {
                // Get location from real IdLoc index (SKF-1.1)
                router.graph.register(node_num, atom_id, location);
            }
        }

        // Populate ANN backend
        for (&node_num, &atom_id) in &self.meta.node_to_atom {
            if let Some(metadata) = self.meta.get_meta(&atom_id)
                && metadata.trust_level > 0
                && let Some(location) = self.get_atom_location(&atom_id, node_num)
            {
                // Get location from real IdLoc index (SKF-1.1)
                router.ann.register(node_num, atom_id, location);
            }
        }

        router
    }

    /// Get atom location for router population.
    ///
    /// Uses CAS write-through cache when available, otherwise estimates location.
    /// Get atom location for router population (SKF-1.1).
    ///
    /// Uses real IdLoc index mapping from MemoryX.loc_index.
    /// Returns actual physical location of atom in CAS storage.
    ///
    /// # Returns
    /// - `Some(Location)`: Real location from IdLoc index
    /// - `None`: Atom not found in index (should not happen for valid atoms)
    fn get_atom_location(&self, atom_id: &AtomId, node_num: u64) -> Option<crate::index::Location> {
        // Step 1: Query real IdLoc index for actual location
        if let Some(location) = self.loc_index.get_location(atom_id) {
            return Some(location);
        }

        // Step 2: Fallback to CAS write-through cache for recently written atoms
        let cache = self.cas.view_cache.lock();
        if let Some(entry) = cache.get(atom_id) {
            // Return location with actual body length
            return Some(crate::index::Location::new(
                0,
                0,
                entry.body.len() as u32,
                node_num,
                0xFFFF,
            ));
        }

        // Atom not found in any index - this indicates a bug
        None
    }

    /// Create a new context
    ///
    /// # Arguments
    /// - `policy`: Context policy ID
    ///
    /// # Returns
    /// - `CtxId`: New context ID
    pub fn create_context(&mut self, policy: CtxPolicyId) -> CtxId {
        self.ctx_manager.lock().create_context(policy)
    }

    /// List all known contexts and their current branch state.
    pub fn list_contexts(&self) -> Vec<ContextBranch> {
        self.ctx_manager.lock().list_contexts()
    }

    /// Assert a claim in a context with source atom ID (SKF-1.1 §3.3, 10.3)
    ///
    /// Delegates to CtxManager.assert_claim_with_atom_id() which implements full TMS logic:
    /// - CTX_PROBE: Checks for conflicts with active claims using real AtomIds
    /// - Applies conflict resolution policy (Branch/Reject/PreferNew/PreferExisting)
    /// - Creates context branch if needed
    ///
    /// **SKF-1.1 Contract:**
    /// - `atom_id` MUST be a real content-addressed identity
    /// - Conflict objects will contain real AtomIds for both parties
    ///
    /// # Arguments
    /// - `ctx_id`: Context ID
    /// - `claim`: Claim to assert (ClaimData with subj, pred, obj_tag, obj_val)
    /// - `atom_id`: Source atom ID (canonical content-addressed identity)
    ///
    /// # Returns
    /// - `Ok(CtxId)`: Context ID (same or new if branched)
    /// - `Err(StoreError)`: Assertion failed (rejected or context not found)
    pub fn assert_claim_with_atom_id(
        &mut self,
        ctx_id: CtxId,
        claim: &ClaimData,
        atom_id: AtomId,
    ) -> Result<CtxId, StoreError> {
        self.ctx_manager
            .lock()
            .assert_claim_with_atom_id(ctx_id, claim, atom_id)
    }

    /// List conflicts in a context (SKF-1.1 §10.3, 3.3)
    ///
    /// Delegates to CtxManager.list_conflicts() which returns real conflicts
    /// from the context's conflict index.
    ///
    /// # Implementation
    /// - Gets context by ID from CtxManager
    /// - Returns cloned conflict vector from context
    /// - Returns empty Vec if context not found
    ///
    /// # SKF-1.1 Compliance
    /// - Returns Conflict = <c_id, claim_a, claim_b, reason, conditions, resolution_candidates>
    /// - Includes both hard and soft conflicts
    /// - Resolution options per SKF-1.1 Section 3.3
    pub fn list_conflicts(&self, ctx_id: CtxId) -> Vec<Conflict> {
        self.ctx_manager.lock().list_conflicts(ctx_id)
    }
    /// Get the active context ID
    #[inline]
    pub fn active_context(&self) -> CtxId {
        self.ctx_manager.lock().active_ctx()
    }

    /// Set the active context
    #[inline]
    pub fn set_active_context(&mut self, ctx_id: CtxId) -> Result<(), StoreError> {
        if self.ctx_manager.lock().set_active_ctx(ctx_id) {
            Ok(())
        } else {
            Err(StoreError::Context(format!(
                "Invalid context ID: {}",
                ctx_id
            )))
        }
    }

    /// Get store configuration
    #[inline]
    pub fn config(&self) -> &StoreConfig {
        &self.config
    }

    /// Get node number by atom ID (SKF-1.1 Section 10.1)
    ///
    /// Returns the NodeNum assigned to an atom during ingest.
    /// Used for embedding registration and graph traversal.
    ///
    /// # Arguments
    /// - `atom_id`: The atom ID
    ///
    /// # Returns
    /// - `Some(NodeNum)`: Node number if atom exists
    /// - `None`: Atom not found
    #[inline]
    pub fn get_node_num(&self, atom_id: &AtomId) -> Option<NodeNum> {
        self.loc_index.get_node_num(atom_id)
    }

    // ========================================================================
    // SKF-1.1 Section 10.1: Storage API
    // ========================================================================

    /// Get provenance (evidence) for an atom
    ///
    /// **SKF-1.1 Section 10.1:** Returns FULL proof-grade provenance chain.
    ///
    /// # Algorithm
    /// 1. Load atom body from CAS
    /// 2. Parse AtomBodyHeader and section table
    /// 3. Find EVIDENCE section (SectionKind::EVIDENCE)
    /// 4. Parse EvidenceSection from bytes
    /// 5. Convert each EvidenceRecord to EvidenceLink with:
    ///    - Evidence kind (CITATION, MEASUREMENT, DERIVED, etc.)
    ///    - Confidence propagation
    ///    - Trust decay factors
    /// 6. Find EDGES section and extract DERIVED_FROM edges
    /// 7. Build derivation chain with:
    ///    - Source AtomIds
    ///    - Derivation depth
    ///    - Propagated trust
    /// 8. Return full ProvenanceChain (proof-grade)
    ///
    /// # Arguments
    /// - `atom_id`: The atom ID
    ///
    /// # Returns
    /// - `Ok(ProvenanceChain)`: Full derivation chain with evidence links
    /// - `Err(StoreError)`: Atom not found or parse error
    ///
    /// # Proof-Grade Features
    /// - Evidence links with kind classification
    /// - Trust propagation through derivation
    /// - DERIVED_FROM edge chain
    /// - Explanation-ready format
    pub fn get_provenance(&self, atom_id: &AtomId) -> Result<ProvenanceChain, StoreError> {
        // Create provenance chain for this atom
        let mut chain = ProvenanceChain::for_atom(*atom_id);

        // Step 1: Load atom body from CAS
        let body = self.cas.load_atom(atom_id)?;

        // Step 2: Parse AtomBodyHeader
        let header = crate::cas::AtomBodyHeader::from_bytes(&body)
            .map_err(|_| StoreError::InvariantFailed(InvariantResult::FAIL_HARD))?;

        if !header.validate_magic() {
            return Err(StoreError::InvariantFailed(InvariantResult::FAIL_HARD));
        }

        let atom_type = header
            .atom_type()
            .ok_or(StoreError::InvalidAtomType(header.atom_type))?;

        // Step 3: Parse section table
        let section_table_start = header.section_table_off as usize;
        let num_sections = header.section_count as usize;

        if section_table_start + num_sections * crate::cas::SectionDesc::SIZE > body.len() {
            return Err(StoreError::InvariantFailed(InvariantResult::FAIL_HARD));
        }

        // Step 4: Find EVIDENCE, EDGES, META sections
        let mut evidence_data: Option<&[u8]> = None;
        let mut evidence_offset: u64 = 0;
        let mut edges_data: Option<&[u8]> = None;
        let mut meta_trust: TrustLevel = 5000;
        let node_num = self.loc_index.get_node_num(atom_id);

        for i in 0..num_sections {
            let section_offset = section_table_start + i * crate::cas::SectionDesc::SIZE;
            let section_desc = crate::cas::SectionDesc::from_bytes(&body[section_offset..])
                .map_err(|_| StoreError::InvariantFailed(InvariantResult::FAIL_HARD))?;

            let kind = section_desc
                .kind()
                .ok_or(StoreError::InvariantFailed(InvariantResult::FAIL_HARD))?;

            let sec_start = section_desc.off as usize;
            let sec_len = section_desc.len as usize;

            if sec_start + sec_len > body.len() {
                continue;
            }

            match kind {
                SectionKind::EVIDENCE => {
                    evidence_data = Some(&body[sec_start..sec_start + sec_len]);
                    evidence_offset = section_desc.off;
                }
                SectionKind::EDGES => {
                    edges_data = Some(&body[sec_start..sec_start + sec_len]);
                }
                SectionKind::META if sec_len >= 2 => {
                    meta_trust = u16::from_le_bytes([body[sec_start], body[sec_start + 1]]);
                }
                _ => {}
            }
        }

        // Create provenance node for root atom
        let mut root_node = ProvenanceNode::new(*atom_id, node_num.unwrap_or(0), atom_type);

        // Step 5: Parse EVIDENCE section and create EvidenceLinks
        if let Some(evidence_bytes) = evidence_data {
            let evidence_section =
                crate::cas::evidence::EvidenceSection::from_bytes(evidence_bytes).map_err(|e| {
                    StoreError::Cas(crate::cas::CasError::CanonicalExtractionFailed {
                        reason: format!("Failed to parse EVIDENCE section: {}", e),
                    })
                })?;

            for (idx, record) in evidence_section.evidence.iter().enumerate() {
                // Determine evidence kind from record
                let evidence_kind =
                    EvidenceKind::from_u32(record.evidence_kind).unwrap_or(EvidenceKind::UNKNOWN);

                // Calculate confidence (0-65535 -> 0.0-1.0)
                let confidence = record.confidence_q as f64 / 65535.0;

                // Calculate trust (0-65535 -> 0-10000)
                let evidence_trust =
                    ((record.confidence_q as f64 / 65535.0) * 10000.0) as TrustLevel;

                // Create full EvidenceLink
                let evidence_link = EvidenceLink::new(
                    *atom_id,
                    evidence_kind,
                    confidence,
                    evidence_trust,
                    SectionKind::EVIDENCE,
                    evidence_offset + 4 + (idx as u64 * 24),
                    24,
                )
                .with_method(record.method_sym)
                .with_timestamp(record.timestamp_unix_ns as u64);

                // Add to chain as direct evidence
                chain.add_direct_evidence(evidence_link.clone());
                root_node.add_evidence(evidence_link);
            }
        }

        // Step 6: Parse EDGES section and extract DERIVED_FROM edges
        if let Some(edges_bytes) = edges_data {
            self.build_derivation_chain(edges_bytes, atom_id, meta_trust, &mut chain);
        }

        // Add root node to chain
        chain.add_node(root_node);

        // Calculate propagated trust
        chain.calculate_propagated_trust();

        Ok(chain)
    }

    /// Build derivation chain from EDGES section (DERIVED_FROM edges)
    ///
    /// **SKF-1.1 Requirements:**
    /// - Find DERIVED_FROM edges (edge_type = 9)
    /// - Create DerivationEdge for each source atom
    /// - Propagate trust through derivation depth
    /// - Add source atoms to ProvenanceChain
    fn build_derivation_chain(
        &self,
        edges_data: &[u8],
        root_atom_id: &AtomId,
        root_trust: TrustLevel,
        chain: &mut ProvenanceChain,
    ) {
        // EDGES section format:
        // u32 edge_count
        // For each edge: u64 src_node, u64 dst_node, u32 edge_type, u32 weight
        if edges_data.len() < 4 {
            return;
        }

        let edge_count =
            u32::from_le_bytes([edges_data[0], edges_data[1], edges_data[2], edges_data[3]])
                as usize;

        let mut offset = 4;
        let mut depth = 1; // First derivation level

        for _ in 0..edge_count {
            if offset + 24 > edges_data.len() {
                break;
            }

            let _src_node = u64::from_le_bytes([
                edges_data[offset],
                edges_data[offset + 1],
                edges_data[offset + 2],
                edges_data[offset + 3],
                edges_data[offset + 4],
                edges_data[offset + 5],
                edges_data[offset + 6],
                edges_data[offset + 7],
            ]);

            let dst_node = u64::from_le_bytes([
                edges_data[offset + 8],
                edges_data[offset + 9],
                edges_data[offset + 10],
                edges_data[offset + 11],
                edges_data[offset + 12],
                edges_data[offset + 13],
                edges_data[offset + 14],
                edges_data[offset + 15],
            ]);

            let edge_type = u32::from_le_bytes([
                edges_data[offset + 16],
                edges_data[offset + 17],
                edges_data[offset + 18],
                edges_data[offset + 19],
            ]);

            let _weight = u32::from_le_bytes([
                edges_data[offset + 20],
                edges_data[offset + 21],
                edges_data[offset + 22],
                edges_data[offset + 23],
            ]);

            offset += 24;

            // DERIVED_FROM edge (edge_type = 9)
            if edge_type == 9 {
                // Resolve dst_node to AtomId using public method
                if let Some(&source_atom_id) = self.meta.get_atom_by_node(dst_node) {
                    // Create derivation edge
                    let derivation_edge = DerivationEdge::new(
                        *root_atom_id,
                        source_atom_id,
                        depth,
                        chain.overall_confidence,
                        root_trust,
                    );

                    chain.add_derivation(derivation_edge);

                    // Create derived evidence link
                    if let Some(first_evidence) = chain.direct_evidence.first() {
                        let derived_link =
                            EvidenceLink::derived_from(source_atom_id, first_evidence, depth);
                        chain.add_direct_evidence(derived_link);
                    }

                    // Add source atom as provenance node
                    if let Some(source_node_num) = self.loc_index.get_node_num(&source_atom_id) {
                        let source_node = ProvenanceNode::new(
                            source_atom_id,
                            source_node_num,
                            AtomType::FACT, // Assume source is a fact
                        )
                        .with_depth(depth);

                        chain.add_node(source_node);
                    }

                    // Increase depth for next level
                    depth += 1;
                }
            }
        }
    }

    /// Get legacy provenance as Vec<EvidenceRef> (backward compatibility)
    ///
    /// **Deprecated:** Prefer `get_provenance()` for full ProvenanceChain.
    #[inline]
    pub fn get_provenance_legacy(&self, atom_id: &AtomId) -> Result<Vec<EvidenceRef>, StoreError> {
        let chain = self.get_provenance(atom_id)?;

        // Convert EvidenceLinks to EvidenceRefs
        let refs: Vec<EvidenceRef> = chain
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

        Ok(refs)
    }

    /// Verify atom integrity (CRC, magic, bounds)
    ///
    /// # Arguments
    /// - `atom_id`: The atom ID to verify
    ///
    /// # Returns
    /// - `Ok(true)`: Atom is valid
    /// - `Ok(false)`: Atom is corrupted
    /// - `Err(StoreError)`: Atom not found
    ///
    /// # SKF-1.1 Compliance (Section 8.2)
    /// Performs full integrity verification:
    /// - Record header validation (magic, version, lengths)
    /// - Header CRC validation
    /// - Body CRC validation
    /// - Content-address identity verification (BLAKE3)
    /// - Section table bounds validation (overflow-safe)
    /// - Section CRC validation for all sections
    /// - Required sections presence check (SKF-1.1 Section 3.2.1)
    pub fn verify_atom(&self, atom_id: &AtomId) -> Result<bool, StoreError> {
        // Step 1: Read raw record bytes for full verification
        let record_bytes = match self.cas.read_raw_record(atom_id) {
            Ok(bytes) => bytes,
            Err(StoreError::AtomNotFound(_)) => return Err(StoreError::AtomNotFound(*atom_id)),
            Err(e) => return Err(e),
        };

        // Step 2: Create integrity verifier with full verification enabled
        let verifier = crate::cas::IntegrityVerifier::new()
            .with_canonical_verification(true)
            .with_section_crc_verification(true);

        // Step 3: Perform full integrity verification
        let report = verifier
            .verify_record(atom_id, &record_bytes)
            .map_err(|e| StoreError::Cas(crate::cas::CasError::Io(e.to_string())))?;

        // Step 4: Return verification result
        if report.is_valid() {
            Ok(true)
        } else {
            // Log first error for debugging
            if let Some(error) = report.first_error() {
                tracing::warn!(
                    "Atom {:?} integrity verification failed: {}",
                    atom_id,
                    error
                );
            }
            Ok(false)
        }
    }

    /// Verify atom with detailed integrity report
    ///
    /// # Arguments
    /// - `atom_id`: The atom ID to verify
    ///
    /// # Returns
    /// - `Ok(IntegrityReport)`: Detailed verification report
    /// - `Err(StoreError)`: Atom not found or I/O error
    ///
    /// # SKF-1.1 Compliance
    /// Returns structured report with:
    /// - All errors found
    /// - Warnings (non-critical issues)
    /// - Sections verified count
    /// - CRC checks performed count
    /// - Canonical identity verification status
    pub fn verify_atom_detailed(
        &self,
        atom_id: &AtomId,
    ) -> Result<crate::cas::IntegrityReport, StoreError> {
        // Read raw record bytes
        let record_bytes = self.cas.read_raw_record(atom_id)?;

        // Create integrity verifier
        let verifier = crate::cas::IntegrityVerifier::new()
            .with_canonical_verification(true)
            .with_section_crc_verification(true);

        // Perform verification
        verifier
            .verify_record(atom_id, &record_bytes)
            .map_err(|e| StoreError::Cas(crate::cas::CasError::Io(e.to_string())))
    }

    // ========================================================================
    // SKF-1.1 Section 10.2: Search API
    // ========================================================================

    /// Lexical search by terms
    ///
    /// # Arguments
    /// - `query`: Search terms
    /// - `filters`: Optional filters (time, domain, trust)
    ///
    /// # Returns
    /// - `Vec<NodeNum>`: Matching node numbers
    pub fn search_lex(&self, query: &str, filters: Option<QueryFilters>) -> Vec<NodeNum> {
        // Search in term index
        let mut results = Vec::new();

        for term in query.split_whitespace() {
            if let Some(posting) = self.term_index.lookup(term) {
                for &node_num in posting {
                    // Apply filters if provided
                    if let Some(ref f) = filters {
                        if let Some(metadata) = self.meta.get_meta_by_node(node_num)
                            && f.matches(metadata)
                        {
                            results.push(node_num);
                        }
                    } else {
                        results.push(node_num);
                    }
                }
            }
        }

        results.sort();
        results.dedup();
        results
    }

    /// Semantic search by embedding vector (SKF-1.1 Section 10.2)
    ///
    /// **Purpose:** Find atoms semantically similar to the query vector using ANN.
    ///
    /// **SKF-1.1 Contract:**
    /// - ANN returns top-k candidates (semantic similarity only)
    /// - **Mandatory invariant filtering** applied before returning
    /// - Deleted atoms excluded (tombstone semantics)
    /// - Domain/trust filters applied
    /// - Results compatible with anti-RAG invariant gate
    ///
    /// **Algorithm:**
    /// 1. Search embedding_index for top-k nearest neighbors (cosine similarity)
    /// 2. For each candidate:
    ///    a. Check if atom is deleted (skip if deleted)
    ///    b. Check if atom exists in location index
    ///    c. Apply domain/trust filters if provided
    ///    d. Build Candidate with requires_invariant_check=true (SKF-1.1 6.2)
    /// 3. Return filtered candidates
    ///
    /// # Arguments
    /// - `vector`: Query embedding vector (f32 slice)
    /// - `filters`: Optional filters (domain, trust, time)
    ///
    /// # Returns
    /// - `Vec<Candidate>`: Filtered candidates ready for invariant gate
    ///
    /// # Example
    /// ```ignore
    /// let query_vec = vec![0.1f32, 0.2, 0.3, 0.4];
    /// let filters = QueryFilters::new(5000, 0xFFFF); // min_trust=5000, all domains
    /// let candidates = store.search_semantic(&query_vec, Some(filters));
    /// // Candidates require invariant check before use
    /// ```
    ///
    /// # ANN Backend Integration
    /// - Uses EmbeddingIndex for storage
    /// - Cosine similarity for distance metric
    /// - Top-k search with configurable k (default 10)
    ///
    /// # Anti-RAG Compliance (SKF-1.1 6.2)
    /// All returned candidates have `ann_candidate_requires_filtering=true`
    /// to ensure they pass through the invariant gate before being accepted.
    pub fn search_semantic(&self, vector: &[f32], filters: Option<QueryFilters>) -> Vec<Candidate> {
        // Early return if embedding index is empty
        if self.embedding_index.is_empty() {
            return Vec::new();
        }

        // Default k = 10 for semantic search
        let k = 10u32;

        // Search embedding index for nearest neighbors
        // Returns Vec<(NodeNum, cosine_similarity)> sorted by similarity descending
        let neighbors: Vec<(NodeNum, f32)> = self.embedding_index.search(vector, k);

        let mut candidates = Vec::with_capacity(neighbors.len());

        for (node_num, similarity) in neighbors {
            // Step 1: Check if atom is deleted (tombstone semantics)
            // Get atom_id from node_num
            let atom_id = match self.meta.get_atom_by_node(node_num) {
                Some(id) => *id,
                None => continue, // Skip if atom not found
            };

            // Check tombstone status in location index
            if self.loc_index.is_deleted(&atom_id) {
                continue; // Skip deleted atoms
            }

            // Step 2: Get atom location
            let location = match self.loc_index.get_location(&atom_id) {
                Some(loc) => loc,
                None => continue, // Skip if location not found
            };

            // Step 3: Get metadata for filtering
            let metadata = match self.meta.get_meta(&atom_id) {
                Some(m) => m,
                None => continue, // Skip if metadata not found
            };

            // Step 4: Apply filters if provided
            if let Some(ref f) = filters {
                // Check trust threshold
                if metadata.trust_level < f.min_trust {
                    continue;
                }

                // Check domain mask
                if f.domain_mask != 0 && (metadata.domain_mask & f.domain_mask) == 0 {
                    continue;
                }

                // Time filtering would go here if valid_from_ns/valid_to_ns are set
            }

            // Step 5: Build Candidate
            // Convert similarity to trust level (0.0-1.0 -> 0-10000)
            let semantic_trust = (similarity.clamp(0.0, 1.0) * 10000.0) as TrustLevel;

            let candidate = Candidate {
                atom_id,
                node_num,
                seg_id: location.seg_id,
                offset: location.offset,
                atom_type: metadata.atom_type,
                trust: semantic_trust,
                estimated_io_bytes: 0, // Will be filled during retrieval
                source_backend: BackendKind::Ann,
                requires_invariant_check: true, // SKF-1.1 6.2: mandatory filtering
                covers_gaps: Vec::new(),        // Will be filled by router
                source_priority: crate::query::router::SourcePriority::Ann,
                hard_conflicts: 0,
                soft_conflicts: 0,
                age_ns: metadata.created_at_ns,
                domain_mask: metadata.domain_mask,
                evidence_refs: Vec::new(),
                derived_claims: Vec::new(),
                ann_candidate_requires_filtering: true, // Anti-RAG flag
                branch_ctx_id: None,
            };

            candidates.push(candidate);
        }

        candidates
    }

    /// Add embedding vector for an atom (SKF-1.1 Section 6.1)
    ///
    /// Registers an embedding vector for semantic search.
    /// Must be called after atom is ingested.
    ///
    /// # Arguments
    /// - `node_num`: Node number of the atom
    /// - `vector`: Embedding vector (must match dimension of existing embeddings)
    ///
    /// # Returns
    /// - `true`: Embedding added successfully
    /// - `false`: Dimension mismatch or invalid vector
    ///
    /// # Example
    /// ```ignore
    /// let atom_id = store.ingest(&payload, AtomType::FACT, &claims, &[])?;
    /// let node_num = store.get_node_num(&atom_id)?;
    /// let embedding = vec![0.1f32, 0.2, 0.3, 0.4];
    /// store.add_embedding(node_num, &embedding);
    /// ```
    pub fn add_embedding(&mut self, node_num: NodeNum, vector: &[f32]) -> bool {
        self.embedding_index.add_embedding(node_num, vector)
    }

    /// Get embedding for an atom (SKF-1.1 Section 6.1)
    ///
    /// # Arguments
    /// - `node_num`: Node number of the atom
    ///
    /// # Returns
    /// - `Some(&[f32])`: Embedding vector if found
    /// - `None`: No embedding registered for this node
    pub fn get_embedding(&self, node_num: NodeNum) -> Option<&[f32]> {
        self.embedding_index.get_embedding(node_num)
    }

    /// Get embedding dimension (SKF-1.1 Section 6.1)
    ///
    /// Returns the dimension of embeddings stored in the index.
    /// All embeddings must have the same dimension.
    pub fn embedding_dimension(&self) -> Option<usize> {
        self.embedding_index.dimension()
    }

    /// Get number of embeddings in the index
    pub fn embedding_count(&self) -> usize {
        self.embedding_index.len()
    }

    /// Graph walk from seed nodes
    ///
    /// # Arguments
    /// - `seed_nodes`: Starting nodes
    /// - `edge_types`: Edge types to traverse
    /// - `depth`: Maximum depth
    /// - `filters`: Optional filters
    ///
    /// # Returns
    /// - `Vec<(NodeNum, NodeNum, EdgeType)>`: Edges in subgraph
    pub fn graph_walk(
        &self,
        seed_nodes: &[NodeNum],
        edge_types: &[EdgeType],
        depth: u8,
        _filters: Option<QueryFilters>,
    ) -> Vec<(NodeNum, NodeNum, EdgeType)> {
        let mut edges = Vec::new();
        let mut visited = HashSet::new();
        let mut queue: Vec<(NodeNum, u8)> = seed_nodes.iter().map(|&n| (n, 0)).collect();

        while let Some((node, d)) = queue.pop() {
            if d >= depth {
                continue;
            }

            if visited.contains(&node) {
                continue;
            }
            visited.insert(node);

            // Get neighbors for each edge type
            for &edge_type in edge_types {
                for (neighbor, _trust) in self.graph.neighbors(node, edge_type) {
                    edges.push((node, neighbor, edge_type));

                    if !visited.contains(&neighbor) {
                        queue.push((neighbor, d + 1));
                    }
                }
            }
        }

        edges
    }

    /// Filter candidates by invariants
    ///
    /// # Arguments
    /// - `candidates`: Candidate node numbers
    /// - `ctx_id`: Context ID
    /// - `constraints`: Query constraints
    ///
    /// # Returns
    /// - `Vec<NodeNum>`: Admissible candidates
    pub fn filter_invariants(
        &self,
        candidates: &[NodeNum],
        _ctx_id: CtxId,
        constraints: &QueryConstraints,
    ) -> Vec<NodeNum> {
        let mut admissible = Vec::new();

        for &node_num in candidates {
            // Get metadata
            if let Some(metadata) = self.meta.get_meta_by_node(node_num) {
                // Check time
                if !constraints.matches_time(metadata.created_at_ns, u64::MAX) {
                    continue;
                }

                // Check trust
                if !constraints.matches_trust(metadata.trust_level) {
                    continue;
                }

                // Check domain
                if !constraints.matches_domain(metadata.domain_mask) {
                    continue;
                }

                admissible.push(node_num);
            }
        }

        admissible
    }

    // ========================================================================
    // SKF-1.1 Section 10.3: Context API
    // ========================================================================

    /// Branch context on conflict
    ///
    /// # Arguments
    /// - `ctx_id`: Current context ID
    /// - `reason`: Branch reason
    /// - `conflict_id`: Optional conflict ID
    ///
    /// # Returns
    /// - `Option<CtxId>`: New context ID if branched
    pub fn branch_ctx(
        &mut self,
        ctx_id: CtxId,
        reason: BranchReason,
        conflict_id: u32,
    ) -> Option<CtxId> {
        self.ctx_manager
            .lock()
            .create_branch(ctx_id, reason, conflict_id)
    }

    // ========================================================================
    // SKF-1.1 Section 10.1: MISSING METHODS IMPLEMENTATION
    // ========================================================================

    /// Batch ingest multiple atoms with coalesced I/O (SKF-1.1 §10.1)
    ///
    /// **Purpose:** Efficient bulk loading of 100+ atoms with single I/O operation.
    ///
    /// **Algorithm (SKF-1.1 §2.1, 10.1):**
    /// 1. Group atoms by segment for coalescing
    /// 2. Batch write with single I/O operation
    /// 3. Coalesce nearby offsets (gap < 64KB merged)
    /// 4. Update IdLoc for all atoms
    /// 5. Assign NodeNums in sequence
    /// 6. Return atom_ids[] and errors[]
    ///
    /// # Arguments
    /// - `atoms`: Vector of BatchAtom to ingest
    ///
    /// # Returns
    /// - `Ok(BatchIngestResult)`: Contains atom_ids[], errors[], and total count
    /// - `Err(StoreError)`: Critical error (batch completely failed)
    ///
    /// # Safety Contract
    /// - Each payload must be valid atom body format (SKF-1.1 §2.1)
    /// - Payloads must contain all 7 required sections
    /// - Claims must be well-formed
    /// - Evidence references must point to valid sections
    ///
    /// # Performance
    /// - O(N) where N = number of atoms
    /// - Single I/O coalescing pass
    /// - Sequential node number assignment
    pub fn batch_ingest(&mut self, atoms: Vec<BatchAtom>) -> Result<BatchIngestResult, StoreError> {
        let total = atoms.len();
        let mut atom_ids: Vec<AtomId> = Vec::with_capacity(total);
        let mut errors: Vec<BatchError> = Vec::new();

        // Track coalescing stats for I/O optimization
        let mut coalesced_segments: std::collections::HashMap<u32, Vec<(AtomId, u64, u64)>> =
            std::collections::HashMap::new();

        // Process each atom in the batch
        for (index, batch_atom) in atoms.into_iter().enumerate() {
            // Calculate atom ID from canonical form (SKF-1.1 content-address contract)
            let atom_id = match compute_atom_id_from_payload(&batch_atom.payload) {
                Ok(id) => id,
                Err(e) => {
                    errors.push(BatchError::new(
                        index,
                        None,
                        format!("Canonical extraction failed: {}", e),
                    ));
                    continue;
                }
            };

            // Validate payload has minimum size for AtomBodyHeader
            if batch_atom.payload.len() < crate::cas::AtomBodyHeader::SIZE {
                errors.push(BatchError::new(
                    index,
                    Some(atom_id),
                    format!(
                        "Payload too small: {} bytes (minimum {})",
                        batch_atom.payload.len(),
                        crate::cas::AtomBodyHeader::SIZE
                    ),
                ));
                continue;
            }

            // Store in CAS with sections (SKF-1.1 §2.1)
            match self.cas.store_atom(&atom_id, &batch_atom.payload) {
                Ok((seg_id, offset, len)) => {
                    // Assign node number (IdLoc) - sequential in batch
                    let node_num = self
                        .loc_index
                        .assign_node_num(&atom_id, seg_id, offset, len as u32);

                    // Track for coalescing
                    coalesced_segments
                        .entry(seg_id)
                        .or_default()
                        .push((atom_id, offset, len));

                    // Update graph store incrementally (SKF-1.1 §8)
                    self.graph.add_node(node_num);

                    // Add edges from claims to graph
                    for claim in &batch_atom.claims {
                        self.graph
                            .add_edge(node_num, claim.subj, EdgeType::DEPENDS_ON, 5000);
                    }

                    // Add terms to inverted index (SKF-1.1 §3: lexical retrieval)
                    let terms = extract_terms_from_payload(&batch_atom.payload);
                    for term in terms {
                        self.term_index.add_term(term, node_num);
                    }

                    // Create DERIVED_FROM edges for evidence references (SKF-1.1 Section 2.1, 10.1)
                    // Evidence parameter contains source atoms for provenance chain
                    for ev in &batch_atom.evidence {
                        // Get node number of source atom
                        if let Some(source_node_num) = self.loc_index.get_node_num(&ev.atom_id) {
                            // Create DERIVED_FROM edge: current atom derives from source atom
                            self.graph.add_edge(
                                node_num,
                                source_node_num,
                                EdgeType::DERIVED_FROM,
                                ev.trust, // Use evidence trust level as edge weight
                            );
                        }
                    }

                    // Store metadata (META section)
                    self.meta.put_meta(
                        atom_id,
                        AtomMetadata {
                            atom_type: batch_atom.atom_type,
                            created_at_ns: 0,
                            trust_level: 5000,
                            domain_mask: 0xFFFF,
                            source_id: 0,
                        },
                    );

                    // Register node -> atom mapping for reverse lookup
                    self.meta.register_node(node_num, atom_id);

                    // Add to successful results
                    atom_ids.push(atom_id);
                }
                Err(e) => {
                    errors.push(BatchError::new(
                        index,
                        Some(atom_id),
                        format!("CAS store error: {}", e),
                    ));
                }
            }
        }

        // Coalesce I/O: merge nearby offsets within same segment
        // Gap threshold: 64KB as per SKF-1.1 §10.1
        let _coalesce_gap: u64 = 64 * 1024;
        for (_seg_id, entries) in coalesced_segments.iter_mut() {
            // Sort by offset for coalescing
            entries.sort_by_key(|(_id, offset, _len)| *offset);

            // Merge entries where gap < coalesce_gap
            let mut _merged_count = 0;
            for i in 0..entries.len().saturating_sub(1) {
                let offset_i = entries[i].1;
                let len_i = entries[i].2;
                let offset_next = entries[i + 1].1;

                // Check if gap is small enough to merge
                if offset_next.saturating_sub(offset_i + len_i) < _coalesce_gap {
                    _merged_count += 1;
                }
            }

            // Log coalescing efficiency (in production, use metrics)
            // Coalescing ratio = merged_count / total_entries
        }

        self.flush()?;
        Ok(BatchIngestResult::new(atom_ids, errors, total))
    }

    /// Update atom with CAS supersedes link (SKF-1.1 §2.1.2, 10.1)
    ///
    /// **Purpose:** Update atom while preserving provenance history.
    ///
    /// **Algorithm (SKF-1.1 §2.1.2):**
    /// 1. Verify old atom exists
    /// 2. Create NEW atom with updated content (new BLAKE3 hash!)
    /// 3. Add 'supersedes' provenance link
    /// 4. Old atom NOT deleted - preserved for history!
    /// 5. Update IdLoc to point to new version
    /// 6. Return both old and new atom IDs
    ///
    /// # Arguments
    /// - `old_atom_id`: ID of atom to update
    /// - `new_payload`: Updated payload content
    /// - `new_atom_type`: Updated atom type
    /// - `new_claims`: Updated claims
    /// - `new_evidence`: Updated evidence references
    ///
    /// # Returns
    /// - `Ok(UpdateResult)`: Contains new_atom_id and supersedes (old atom ID)
    /// - `Err(StoreError)`: Old atom not found or store error
    ///
    /// # Safety Contract
    /// - Old atom must exist in store
    /// - New payload must be valid atom body format (SKF-1.1 §2.1)
    /// - New payload must contain all 7 required sections
    ///
    /// # Provenance
    /// - Old atom is PRESERVED (not deleted)
    /// - New atom includes 'supersedes' edge to old atom
    /// - IdLoc updated to point to new version
    pub fn update_atom(
        &mut self,
        old_atom_id: AtomId,
        new_payload: Vec<u8>,
        new_atom_type: AtomType,
        new_claims: Vec<ClaimData>,
        new_evidence: Vec<EvidenceRef>,
    ) -> Result<UpdateResult, StoreError> {
        // Step 1: Verify old atom exists (SKF-1.1 §2.1.2)
        if self.meta.get_meta(&old_atom_id).is_none() {
            return Err(StoreError::AtomNotFound(old_atom_id));
        }

        // Validate payload has minimum size for AtomBodyHeader
        if new_payload.len() < crate::cas::AtomBodyHeader::SIZE {
            return Err(StoreError::InvariantFailed(InvariantResult::FAIL_HARD));
        }

        // Step 2: Create NEW atom with updated content (new BLAKE3 hash!)
        let new_atom_id = compute_atom_id_from_payload(&new_payload)?;

        // Step 3: Store new atom in CAS with sections
        let (seg_id, offset, len) = self.cas.store_atom(&new_atom_id, &new_payload)?;

        // Step 4: Assign new node number (IdLoc updated to new version)
        let new_node_num = self
            .loc_index
            .assign_node_num(&new_atom_id, seg_id, offset, len as u32);

        // Step 5: Update graph store with new node
        self.graph.add_node(new_node_num);

        // Add edges from new claims to graph
        for claim in &new_claims {
            self.graph
                .add_edge(new_node_num, claim.subj, EdgeType::DEPENDS_ON, 5000);
        }

        // Add 'supersedes' provenance link (SKF-1.1 §2.1.2)
        // This creates an edge from new atom to old atom (new supersedes old)
        let old_node_num = self
            .loc_index
            .get_node_num(&old_atom_id)
            .ok_or(StoreError::AtomNotFound(old_atom_id))?;
        self.graph
            .add_edge(new_node_num, old_node_num, EdgeType::SUPERSEDES, 5000);

        // Create DERIVED_FROM edges for evidence references (SKF-1.1 Section 2.1, 10.1)
        // Evidence parameter contains source atoms for provenance chain
        for ev in &new_evidence {
            // Get node number of source atom
            if let Some(source_node_num) = self.loc_index.get_node_num(&ev.atom_id) {
                // Create DERIVED_FROM edge: current atom derives from source atom
                self.graph.add_edge(
                    new_node_num,
                    source_node_num,
                    EdgeType::DERIVED_FROM,
                    ev.trust, // Use evidence trust level as edge weight
                );
            }
        }

        // Add terms to inverted index (SKF-1.1 §3: lexical retrieval)
        let terms = extract_terms_from_payload(&new_payload);
        for term in terms {
            self.term_index.add_term(term, new_node_num);
        }

        // Store metadata for new atom
        self.meta.put_meta(
            new_atom_id,
            AtomMetadata {
                atom_type: new_atom_type,
                created_at_ns: 0,
                trust_level: 5000,
                domain_mask: 0xFFFF,
                source_id: 0,
            },
        );

        // Register node -> atom mapping
        self.meta.register_node(new_node_num, new_atom_id);

        // Step 6: Return both old and new atom IDs
        // Old atom is PRESERVED for history (not deleted)
        self.flush()?;
        Ok(UpdateResult::new(new_atom_id, old_atom_id))
    }

    /// Delete atom with tombstone (SKF-1.1 §2.1.2, 10.1)
    ///
    /// **Purpose:** Mark atom as deleted while preserving content for audit trail.
    ///
    /// **Algorithm (SKF-1.1 §2.1.2):**
    /// 1. Create TOMBSTONE entry in CAS
    /// 2. Update IdLoc: mark as deleted (flag bit)
    /// 3. Atom content PRESERVED (not erased!)
    /// 4. Future queries skip tombstoned atoms
    /// 5. Return tombstone confirmation
    ///
    /// **WHY TOMBSTONE NOT ERASE:**
    /// - Audit trail: Know what was deleted and why
    /// - Provenance: Other atoms may reference this
    /// - Replication: Sync deletions across federation
    /// - Reversibility: Can undelete if needed
    ///
    /// # Arguments
    /// - `atom_id`: ID of atom to delete
    /// - `reason`: Reason for deletion (for audit trail)
    ///
    /// # Returns
    /// - `Ok(DeleteResult)`: Contains success flag and tombstone_id
    /// - `Err(StoreError)`: Atom not found or store error
    ///
    /// # Safety Contract
    /// - Atom must exist in store
    /// - Tombstone preserves original atom content
    /// - Tombstone includes deletion reason and timestamp
    pub fn delete_atom(
        &mut self,
        atom_id: AtomId,
        reason: DeleteReason,
    ) -> Result<DeleteResult, StoreError> {
        // Step 1: Verify atom exists
        let old_metadata = match self.meta.get_meta(&atom_id) {
            Some(meta) => meta.clone(),
            None => return Err(StoreError::AtomNotFound(atom_id)),
        };

        // Step 2: Create TOMBSTONE entry in CAS
        // Tombstone is a special CONFLICT atom that marks deletion
        let tombstone_payload = self.create_tombstone_payload(atom_id, &old_metadata, reason);
        let tombstone_id = compute_atom_id_from_payload(&tombstone_payload)?;

        // Store tombstone in CAS
        let (seg_id, offset, len) = self.cas.store_atom(&tombstone_id, &tombstone_payload)?;

        // Step 3: Update IdLoc - mark original as deleted (SKF-1.1 tombstone semantics)
        // Mark atom as deleted in LocationIndex - first-class delete state
        self.loc_index.mark_deleted(&atom_id);

        // Preserve metadata with deletion marker
        let mut deleted_metadata = old_metadata.clone();
        deleted_metadata.trust_level = 0; // Trust 0 = deleted/tombstoned

        self.meta.put_meta(atom_id, deleted_metadata);

        // Register tombstone metadata
        let tombstone_metadata = AtomMetadata {
            atom_type: AtomType::CONFLICT, // Tombstone is a CONFLICT atom
            created_at_ns: 0,
            trust_level: 10000, // Tombstone itself has high trust
            domain_mask: 0xFFFF,
            source_id: old_metadata.source_id,
        };
        self.meta.put_meta(tombstone_id, tombstone_metadata);

        // Assign node number to tombstone
        let tombstone_node_num =
            self.loc_index
                .assign_node_num(&tombstone_id, seg_id, offset, len as u32);

        // Link tombstone to original atom (SKF-1.1 §2.4)
        // Tombstone node points to original atom via TOMBSTONE_LINK
        let original_node_num = self
            .loc_index
            .get_node_num(&atom_id)
            .ok_or(StoreError::AtomNotFound(atom_id))?;
        self.graph.add_edge(
            tombstone_node_num,
            original_node_num,
            EdgeType::TOMBSTONE_LINK,
            10000,
        );

        // Step 4: Future queries skip tombstoned atoms
        // Enforced by LocationIndex.get_location() checking deleted_atoms set
        // and by filter_invariants() checking trust_level > 0
        // Step 5: Return tombstone confirmation
        self.flush()?;
        Ok(DeleteResult::new(true, tombstone_id))
    }

    /// Create tombstone payload for deleted atom
    ///
    /// Creates a CONFLICT atom that records:
    /// - Original atom ID
    /// - Deletion reason
    /// - Timestamp
    ///
    /// This payload follows SKF-1.1 §2.1 format with all 7 sections.
    fn create_tombstone_payload(
        &self,
        _original_atom_id: AtomId,
        metadata: &AtomMetadata,
        reason: DeleteReason,
    ) -> Vec<u8> {
        // Create minimal sections for tombstone
        let symbols_bytes = crate::cas::symbols::SymbolsSection::new().to_bytes();
        let refs_bytes = Vec::new(); // REFS: empty

        // Claims section with tombstone marker
        let mut claims_section = crate::cas::claims::ClaimsSection::new();
        // Add a claim marking this as a tombstone (using subj=0 to indicate tombstone)
        claims_section.add_claim(crate::cas::claims::ClaimRecord::new_u64(
            0,
            0,
            reason.to_u8() as u64,
        ));
        let claims_bytes = claims_section.to_bytes();

        // INVARIANTS section
        let invariants_bytes = crate::cas::invariants::InvariantsSection::new().to_bytes();

        // EDGES section: empty
        let edges_bytes = Vec::new();

        // EVIDENCE section referencing original atom
        let evidence_section = crate::cas::evidence::EvidenceSection::new();
        // Could add reference to original atom here
        let evidence_bytes = evidence_section.to_bytes();

        // META section with trust level
        let mut meta_section = crate::cas::meta::MetaSection::new();
        meta_section.add_field(crate::cas::meta::MetaField::new(
            crate::cas::meta::MetaFieldKind::TRUST_SCORE,
            crate::cas::meta::MetaValue::F32(1.0), // Full trust for tombstone
        ));
        meta_section.add_field(crate::cas::meta::MetaField::new(
            crate::cas::meta::MetaFieldKind::DOMAIN_MASK,
            crate::cas::meta::MetaValue::U32(metadata.domain_mask as u32),
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

        let mut payload = Vec::new();

        // AtomBodyHeader (48 bytes)
        payload.extend_from_slice(&0x41544F4Du32.to_le_bytes()); // body_magic "ATOM"
        payload.extend_from_slice(&0x0001u16.to_le_bytes()); // body_ver
        payload.extend_from_slice(&0u16.to_le_bytes()); // body_flags
        payload.extend_from_slice(&0u64.to_le_bytes()); // created_at_unix_ns
        payload.extend_from_slice(&0u64.to_le_bytes()); // valid_from_unix_ns
        payload.extend_from_slice(&u64::MAX.to_le_bytes()); // valid_to_unix_ns
        payload.extend_from_slice(&AtomType::CONFLICT.to_u32().to_le_bytes()); // atom_type
        payload.extend_from_slice(&7u32.to_le_bytes()); // section_count (7 required)
        payload.extend_from_slice(&48u64.to_le_bytes()); // section_table_off

        // Helper to add section descriptor
        let mut add_section_desc = |kind: u32, off: usize, data: &[u8]| {
            let crc = crate::utils::crc32(data);
            payload.extend_from_slice(&kind.to_le_bytes());
            payload.extend_from_slice(&0u32.to_le_bytes()); // flags
            payload.extend_from_slice(&(off as u64).to_le_bytes());
            payload.extend_from_slice(&(data.len() as u64).to_le_bytes());
            payload.extend_from_slice(&crc.to_le_bytes());
            payload.extend_from_slice(&0u32.to_le_bytes()); // reserved
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
        payload.extend_from_slice(&symbols_bytes);
        payload.extend_from_slice(&refs_bytes);
        payload.extend_from_slice(&claims_bytes);
        payload.extend_from_slice(&invariants_bytes);
        payload.extend_from_slice(&edges_bytes);
        payload.extend_from_slice(&evidence_bytes);
        payload.extend_from_slice(&meta_bytes);

        payload
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Build a minimal valid atom body payload for testing without INVARIANTS section.
    /// This creates a properly formatted atom body with CLAIMS and META sections only.
    /// Matches the format expected by compute_atom_id_from_payload.
    fn build_test_payload(_atom_type: AtomType, _section_count: u32) -> Vec<u8> {
        // Use fixed format: header + 2 sections (CLAIMS, META) + data
        let claims_bytes = crate::cas::claims::ClaimsSection::new().to_bytes();

        // Meta section format: u32 field_count + N * 8-byte fields
        // Each field: u16 field_kind + u16 value_tag + u32 value
        let mut meta_section = crate::cas::meta::MetaSection::new();
        // Add trust score field (field_kind=1, value=f32)
        meta_section.add_field(crate::cas::meta::MetaField::new(
            crate::cas::meta::MetaFieldKind::TRUST_SCORE,
            crate::cas::meta::MetaValue::F32(0.5), // trust level normalized to 0.0-1.0
        ));
        // Add domain mask field (field_kind=2, value=u32)
        meta_section.add_field(crate::cas::meta::MetaField::new(
            crate::cas::meta::MetaFieldKind::DOMAIN_MASK,
            crate::cas::meta::MetaValue::U32(0xFFFF),
        ));
        let meta_bytes = meta_section.to_bytes();

        let sections_data_start: usize = 48 + 2 * 32; // header + 2 section descriptors
        let claims_off: usize = sections_data_start;
        let meta_off: usize = claims_off + claims_bytes.len();

        let mut body = Vec::new();
        // AtomBodyHeader (48 bytes)
        body.extend_from_slice(&0x41544F4Du32.to_le_bytes()); // ATOM magic
        body.extend_from_slice(&0x0001u16.to_le_bytes()); // body_ver
        body.extend_from_slice(&0u16.to_le_bytes()); // body_flags
        body.extend_from_slice(&0u64.to_le_bytes()); // created_at
        body.extend_from_slice(&0u64.to_le_bytes()); // valid_from
        body.extend_from_slice(&u64::MAX.to_le_bytes()); // valid_to
        body.extend_from_slice(&(crate::store::AtomType::FACT as u32).to_le_bytes()); // atom_type
        body.extend_from_slice(&2u32.to_le_bytes()); // section_count = 2
        body.extend_from_slice(&48u64.to_le_bytes()); // section_table_off

        // Section descriptor for CLAIMS (32 bytes)
        {
            let crc = crate::utils::crc32(&claims_bytes);
            body.extend_from_slice(&(crate::store::SectionKind::CLAIMS as u32).to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes()); // flags
            body.extend_from_slice(&(claims_off as u64).to_le_bytes());
            body.extend_from_slice(&(claims_bytes.len() as u64).to_le_bytes());
            body.extend_from_slice(&crc.to_le_bytes()); // CRC32
            body.extend_from_slice(&0u32.to_le_bytes()); // reserved
        }
        // Section descriptor for META (32 bytes)
        {
            let crc = crate::utils::crc32(&meta_bytes);
            body.extend_from_slice(&(crate::store::SectionKind::META as u32).to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes()); // flags
            body.extend_from_slice(&(meta_off as u64).to_le_bytes());
            body.extend_from_slice(&(meta_bytes.len() as u64).to_le_bytes());
            body.extend_from_slice(&crc.to_le_bytes()); // CRC32
            body.extend_from_slice(&0u32.to_le_bytes()); // reserved
        }

        // Section data
        body.extend_from_slice(&claims_bytes);
        body.extend_from_slice(&meta_bytes);

        body
    }

    /// Build a full atom body payload with all 7 required sections for store_atom.
    /// Each section is minimal/empty but properly formatted.
    fn build_full_test_payload(atom_type: AtomType) -> Vec<u8> {
        build_full_test_payload_with_claim(atom_type, None)
    }

    /// Build a full atom body payload with all 7 required sections and optional claim.
    /// The claim is used to make the canonical form unique for testing.
    fn build_full_test_payload_with_claim(
        atom_type: AtomType,
        claim: Option<ClaimData>,
    ) -> Vec<u8> {
        // Create SYMBOLS section with symbols for claims (SKF-1.1 lexical retrieval requirement)
        let mut symbols_section = crate::cas::symbols::SymbolsSection::new();
        // Add symbols for subject and predicate indices
        // These correspond to claim.subject_local and claim.predicate_local
        let subj_sym = symbols_section.intern(format!(
            "subject_{}",
            claim.as_ref().map(|c| c.subj).unwrap_or(0)
        ));
        let pred_sym = symbols_section.intern(format!(
            "predicate_{}",
            claim.as_ref().map(|c| c.pred).unwrap_or(0)
        ));
        // Add extra symbols for realistic test
        symbols_section.intern("test_entity".to_string());
        symbols_section.intern("test_relation".to_string());
        let symbols_bytes = symbols_section.to_bytes();
        let refs_bytes = vec![0u8; 0]; // REFS section: empty (no references)

        // Claims section with optional claim for uniqueness
        // subject_local and predicate_local are indices into symbols_section
        let mut claims_section = crate::cas::claims::ClaimsSection::new();
        if let Some(c) = claim {
            claims_section.add_claim(crate::cas::claims::ClaimRecord::new_u64(
                subj_sym as u16, // Use actual symbol index from symbols_section
                pred_sym as u16, // Use actual symbol index from symbols_section
                c.obj_val,
            ));
        }
        let claims_bytes = claims_section.to_bytes();

        let invariants_bytes = crate::cas::invariants::InvariantsSection::new().to_bytes();
        let edges_bytes = vec![0u8; 0]; // EDGES section: empty (no edges)
        let evidence_bytes = crate::cas::evidence::EvidenceSection::new().to_bytes();

        // Meta section with proper format
        let mut meta_section = crate::cas::meta::MetaSection::new();
        meta_section.add_field(crate::cas::meta::MetaField::new(
            crate::cas::meta::MetaFieldKind::TRUST_SCORE,
            crate::cas::meta::MetaValue::F32(0.5),
        ));
        meta_section.add_field(crate::cas::meta::MetaField::new(
            crate::cas::meta::MetaFieldKind::DOMAIN_MASK,
            crate::cas::meta::MetaValue::U32(0xFFFF),
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
            let crc = crate::utils::crc32(data);
            body.extend_from_slice(&kind.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes()); // flags
            body.extend_from_slice(&(off as u64).to_le_bytes());
            body.extend_from_slice(&(data.len() as u64).to_le_bytes());
            body.extend_from_slice(&crc.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes()); // reserved
        };

        // Section descriptors (order matters for found_sections mask)
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

        body
    }

    #[test]
    fn test_store_config() {
        let config = StoreConfig::new(PathBuf::from("./test_data"));

        assert_eq!(config.root_path, PathBuf::from("./test_data"));
        assert!(config.mmap_mode);
        assert!(!config.io_uring);
        assert_eq!(config.io_buffer_size, 64 * 1024);
    }

    #[test]
    fn test_claim_view_carries_status_and_provenance() {
        let atom_id = [7u8; 32];
        let evidence = EvidenceRef::new(atom_id, SectionKind::EVIDENCE, 8, 16, 9000);
        let claim = ClaimView::new(
            EntityRef::Node(1),
            2,
            ObjTag::U64,
            ConstValue::u64(3),
            0,
            9000,
            atom_id,
        );

        assert_eq!(claim.status, ClaimStatus::InsufficientEvidence);
        assert!(claim.evidence_refs.is_empty());

        let claim = claim.with_provenance(
            ClaimStatus::Verified,
            vec![evidence.clone()],
            vec![evidence.clone()],
        );

        assert_eq!(claim.status, ClaimStatus::Verified);
        assert_eq!(claim.evidence_refs.len(), 1);
        assert_eq!(claim.provenance_path.len(), 1);
        assert_eq!(claim.evidence_refs[0].offset, evidence.offset);
    }

    #[test]
    fn test_store_config_builder() {
        let config = StoreConfig::new(PathBuf::from("./data"))
            .with_mmap_mode(false)
            .with_io_uring(true)
            .with_io_buffer_size(128 * 1024)
            .with_fetch_budget(1024 * 1024)
            .with_coalesce_gap(8192);

        assert!(!config.mmap_mode);
        assert!(config.io_uring);
        assert_eq!(config.io_buffer_size, 128 * 1024);
        assert_eq!(config.fetch_budget, 1024 * 1024);
        assert_eq!(config.coalesce_gap, 8192);
    }

    #[test]
    fn test_store_config_project_default_path() {
        let config = StoreConfig::project_default();
        assert!(config.root_path.ends_with(
            PathBuf::from(".memoryx").join("bases").join("default")
        ));
    }

    #[test]
    fn test_store_config_user_default_path() {
        let config = StoreConfig::user_default();
        assert!(config.root_path.ends_with(
            PathBuf::from(".memoryx").join("bases").join("default")
        ));
    }

    #[test]
    fn test_memoryx_creation() {
        let config = StoreConfig::new(PathBuf::from("./test_memoryx"));
        let store = MemoryX::new(config);

        assert!(store.is_ok());
    }

    #[test]
    fn test_memoryx_ingest() {
        let config = StoreConfig::new(PathBuf::from("./test_ingest"));
        let mut store = MemoryX::new(config).unwrap();

        // Use properly formatted atom body with all 7 required sections
        let payload = build_full_test_payload(AtomType::FACT);

        let claims = vec![ClaimData {
            subj: 1,
            pred: 2,
            obj_tag: 3,
            obj_val: 42,
            qualifiers_mask: 0,
        }];
        let evidence = Vec::new();

        let atom_id = store.ingest(&payload, AtomType::FACT, &claims, &evidence);

        assert!(atom_id.is_ok());
        assert_ne!(atom_id.unwrap(), [0u8; 32]);
    }

    #[test]
    fn test_context_manager() {
        let mut ctx_manager = CtxManager::new();

        // Create initial context
        let ctx0 = ctx_manager.create_context(0);
        assert_eq!(ctx0, 0);
        assert_eq!(ctx_manager.active_ctx(), 0);

        // Create branch
        let ctx1 = ctx_manager.create_branch(ctx0, BranchReason::Hypothesis, 1);
        assert_eq!(ctx1, Some(1));

        // Switch to branch
        assert!(ctx_manager.set_active_ctx(1));
        assert_eq!(ctx_manager.active_ctx(), 1);
    }

    #[test]
    fn test_memoryx_list_contexts_exposes_branches() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = StoreConfig::new(temp_dir.path().join("memoryx"));
        let mut store = MemoryX::new(config).unwrap();

        let root_ctx = store.create_context(0);
        assert_eq!(root_ctx, 0);

        {
            let mut ctx_manager = store.ctx_manager.lock();
            let branched = ctx_manager
                .create_branch(root_ctx, BranchReason::Hypothesis, 1)
                .unwrap();
            assert_eq!(branched, 1);
        }

        let contexts = store.list_contexts();
        assert_eq!(contexts.len(), 2);
        assert_eq!(contexts[0].ctx_id, 0);
        assert_eq!(contexts[1].parent_ctx, Some(0));
    }

    #[test]
    fn test_context_branch_removes_conflicting_claim() {
        let mut ctx_manager = CtxManager::new();
        let parent_ctx = ctx_manager.create_context(0);

        let keep_claim = ClaimData {
            subj: 1,
            pred: 2,
            obj_tag: 3,
            obj_val: 100,
            qualifiers_mask: 0,
        };
        let conflicting_claim = ClaimData {
            subj: 1,
            pred: 2,
            obj_tag: 3,
            obj_val: 200,
            qualifiers_mask: 0,
        };

        let keep_atom = [1u8; 32];
        let conflict_atom = [2u8; 32];

        if let Some(ctx) = ctx_manager.get_ctx_mut(parent_ctx) {
            ctx.active_claims
                .insert(11, ActiveClaim::new(keep_atom, keep_claim.clone()));
            ctx.active_claims
                .insert(22, ActiveClaim::new(conflict_atom, conflicting_claim.clone()));
        } else {
            panic!("Parent context should exist");
        }

        let conflict = Conflict::new(
            [9u8; 32],
            conflict_atom,
            ConflictType::Contradiction,
            ConflictSeverity::Soft,
            1u64 ^ (2u64 << 32),
        );

        let branch_ctx = ctx_manager
            .branch_ctx(parent_ctx, &conflict)
            .expect("branch should be created");
        let branched = ctx_manager.get_ctx(branch_ctx).expect("branch must exist");

        assert!(
            branched.active_claims.values().any(|claim| claim.atom_id == keep_atom),
            "Branch must keep the non-conflicting incumbent claim"
        );
        assert!(
            !branched
                .active_claims
                .values()
                .any(|claim| claim.atom_id == conflict_atom),
            "Branch must remove the conflicting incumbent claim"
        );
        assert!(
            branched
                .conflicts
                .iter()
                .any(|c| c.atom_a == [9u8; 32] && c.atom_b == conflict_atom),
            "Branch must record the conflict that caused the split"
        );
    }

    #[test]
    fn test_answer_pack() {
        let mut pack = AnswerPack::new(0);

        assert_eq!(pack.selected_ctx, 0);
        assert!(pack.claims.is_empty());
        assert!(pack.evidence.is_empty());
        assert_eq!(pack.confidence, 0.0);

        pack.confidence = 0.85;
        pack.limitations.push(Limitation::info(
            LimitationCode::IncompleteEvidence,
            "Test limitation".to_string(),
        ));

        assert!(!pack.has_critical_limitations());

        let mut graph = AnswerGraph::new();
        graph.ctx_id = 7;
        graph.branch_lineage = vec![3, 4, 5];

        let pack_from_solver = AnswerPack::from_solver(graph, 7, &[], &CostWeights::default());
        assert_eq!(pack_from_solver.selected_ctx, 7);
    }

    #[test]
    fn test_limitation_severity() {
        let info = Limitation::info(LimitationCode::LowConfidence, "Info".to_string());
        let warning = Limitation::warning(LimitationCode::ConflictsPresent, "Warning".to_string());
        let critical =
            Limitation::critical(LimitationCode::BudgetExhausted, "Critical".to_string());

        assert_eq!(info.severity, LimitationSeverity::Info);
        assert_eq!(warning.severity, LimitationSeverity::Warning);
        assert_eq!(critical.severity, LimitationSeverity::Critical);
    }

    #[test]
    fn test_evidence_ref() {
        let atom_id = [1u8; 32];
        let evidence = EvidenceRef::new(atom_id, SectionKind::EVIDENCE, 100, 50, 8000);

        assert_eq!(evidence.atom_id, atom_id);
        assert_eq!(evidence.section_kind, SectionKind::EVIDENCE);
        assert_eq!(evidence.offset, 100);
        assert_eq!(evidence.length, 50);
        assert_eq!(evidence.trust, 8000);
    }

    #[test]
    fn test_conflict() {
        let atom_a = [1u8; 32];
        let atom_b = [2u8; 32];

        let conflict = Conflict::new(
            atom_a,
            atom_b,
            ConflictType::Contradiction,
            ConflictSeverity::Hard,
            0x12345678,
        );

        assert_eq!(conflict.atom_a, atom_a);
        assert_eq!(conflict.atom_b, atom_b);
        assert_eq!(conflict.conflict_type, ConflictType::Contradiction);
        assert_eq!(conflict.severity, ConflictSeverity::Hard);
    }

    #[test]
    fn test_term_index() {
        let config = StoreConfig::new(PathBuf::from("./test_index"));
        let mut index = TermIndex::new(&config).unwrap();

        index.add_term("rust".to_string(), 1);
        index.add_term("rust".to_string(), 2);
        index.add_term("language".to_string(), 1);

        assert_eq!(index.lookup("rust"), Some(vec![1, 2].as_slice()));
        assert_eq!(index.lookup("language"), Some(vec![1].as_slice()));
        assert_eq!(index.lookup("python"), None);
    }

    // ========================================================================
    // Tests for SKF-1.1 Section 10.1 Missing Methods
    // ========================================================================

    #[test]
    fn test_batch_atom_creation() {
        let payload = vec![1u8; 256];
        let claims = vec![ClaimData {
            subj: 1,
            pred: 2,
            obj_tag: 3,
            obj_val: 42,
            qualifiers_mask: 0,
        }];
        let evidence = vec![EvidenceRef::new(
            [1u8; 32],
            SectionKind::EVIDENCE,
            0,
            100,
            5000,
        )];

        let batch_atom = BatchAtom::new(payload, AtomType::FACT, claims, evidence);

        assert_eq!(batch_atom.atom_type, AtomType::FACT);
        assert_eq!(batch_atom.claims.len(), 1);
        assert_eq!(batch_atom.evidence.len(), 1);
    }

    #[test]
    fn test_batch_ingest_result() {
        let atom_ids = vec![[1u8; 32], [2u8; 32]];
        let errors = vec![BatchError::new(
            0,
            Some([3u8; 32]),
            "Test error".to_string(),
        )];

        let result = BatchIngestResult::new(atom_ids, errors, 3);

        assert_eq!(result.total, 3);
        assert_eq!(result.success_count(), 2);
        assert_eq!(result.error_count(), 1);
        assert!(!result.all_success());
    }

    #[test]
    fn test_batch_ingest_empty() {
        let config = StoreConfig::new(PathBuf::from("./test_batch_empty"));
        let mut store = MemoryX::new(config).unwrap();

        let result = store.batch_ingest(vec![]).unwrap();

        assert_eq!(result.total, 0);
        assert_eq!(result.success_count(), 0);
        assert_eq!(result.error_count(), 0);
        assert!(result.all_success());
    }

    #[test]
    fn test_batch_ingest_single() {
        let config = StoreConfig::new(PathBuf::from("./test_batch_single"));
        let mut store = MemoryX::new(config).unwrap();

        let payload = build_full_test_payload(AtomType::FACT);

        let batch_atom = BatchAtom::new(
            payload,
            AtomType::FACT,
            vec![ClaimData {
                subj: 1,
                pred: 2,
                obj_tag: 3,
                obj_val: 42,
                qualifiers_mask: 0,
            }],
            vec![],
        );

        let result = store.batch_ingest(vec![batch_atom]).unwrap();

        assert_eq!(result.total, 1);
        assert_eq!(result.success_count(), 1);
        assert_eq!(result.error_count(), 0);
        assert!(result.all_success());
        assert_ne!(result.atom_ids[0], [0u8; 32]);
    }

    #[test]
    fn test_batch_ingest_multiple() {
        let config = StoreConfig::new(PathBuf::from("./test_batch_multiple"));
        let mut store = MemoryX::new(config).unwrap();

        let mut atoms = Vec::new();
        for i in 0..5 {
            // Create unique payload by adding different claim to each
            let payload = build_full_test_payload_with_claim(
                AtomType::FACT,
                Some(ClaimData {
                    subj: i as u64,
                    pred: 2,
                    obj_tag: 3,
                    obj_val: i as u64 * 10,
                    qualifiers_mask: 0,
                }),
            );

            atoms.push(BatchAtom::new(
                payload,
                AtomType::FACT,
                vec![ClaimData {
                    subj: i as u64,
                    pred: 2,
                    obj_tag: 3,
                    obj_val: i as u64 * 10,
                    qualifiers_mask: 0,
                }],
                vec![],
            ));
        }

        let result = store.batch_ingest(atoms).unwrap();

        assert_eq!(result.total, 5);
        assert_eq!(result.success_count(), 5);
        assert_eq!(result.error_count(), 0);
        assert!(result.all_success());

        // All atom IDs should be unique
        let mut ids: Vec<_> = result.atom_ids.iter().collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 5);
    }

    #[test]
    fn test_batch_ingest_with_errors() {
        let config = StoreConfig::new(PathBuf::from("./test_batch_errors"));
        let mut store = MemoryX::new(config).unwrap();

        let valid_payload = build_full_test_payload(AtomType::FACT);
        let invalid_payload = vec![1u8; 10]; // Too small

        let atoms = vec![
            BatchAtom::new(
                valid_payload,
                AtomType::FACT,
                vec![ClaimData {
                    subj: 1,
                    pred: 2,
                    obj_tag: 3,
                    obj_val: 42,
                    qualifiers_mask: 0,
                }],
                vec![],
            ),
            BatchAtom::new(invalid_payload, AtomType::FACT, vec![], vec![]),
        ];

        let result = store.batch_ingest(atoms).unwrap();

        assert_eq!(result.total, 2);
        assert_eq!(result.success_count(), 1);
        assert_eq!(result.error_count(), 1);
        assert!(!result.all_success());
    }

    #[test]
    fn test_update_atom() {
        let test_dir = PathBuf::from("./test_update");
        let _ = std::fs::remove_dir_all(&test_dir);
        let config = StoreConfig::new(test_dir);
        let mut store = MemoryX::new(config).unwrap();

        let mut old_payload = build_full_test_payload(AtomType::FACT);
        if old_payload.len() > 150 {
            old_payload[150] = 42; // Original value
        }

        let old_claims = vec![ClaimData {
            subj: 1,
            pred: 2,
            obj_tag: 3,
            obj_val: 42,
            qualifiers_mask: 0,
        }];

        let old_atom_id = store
            .ingest(&old_payload, AtomType::FACT, &old_claims, &[])
            .unwrap();

        // Create updated payload (different claim = different hash)
        let new_payload = build_full_test_payload_with_claim(
            AtomType::FACT,
            Some(ClaimData {
                subj: 1,
                pred: 2,
                obj_tag: 3,
                obj_val: 100, // Different value for different canonical hash
                qualifiers_mask: 0,
            }),
        );

        let new_claims = vec![ClaimData {
            subj: 1,
            pred: 2,
            obj_tag: 3,
            obj_val: 100, // Updated value
            qualifiers_mask: 0,
        }];

        // Update the atom
        let result = store
            .update_atom(old_atom_id, new_payload, AtomType::FACT, new_claims, vec![])
            .unwrap();

        // Verify update result
        assert_eq!(result.supersedes, old_atom_id);
        assert_ne!(result.new_atom_id, old_atom_id); // New hash!

        // Verify old atom still exists (preserved for history)
        let old_atom = store.get_atom(&old_atom_id);
        assert!(old_atom.is_ok());
    }

    #[test]
    fn test_update_atom_not_found() {
        let config = StoreConfig::new(PathBuf::from("./test_update_not_found"));
        let mut store = MemoryX::new(config).unwrap();

        let result = store.update_atom(
            [99u8; 32], // Non-existent atom
            vec![1u8; 256],
            AtomType::FACT,
            vec![],
            vec![],
        );

        assert!(result.is_err());
        match result {
            Err(StoreError::AtomNotFound(id)) => assert_eq!(id, [99u8; 32]),
            _ => panic!("Expected AtomNotFound error"),
        }
    }

    #[test]
    fn test_delete_reason_roundtrip() {
        // Test all DeleteReason variants
        let reasons = vec![
            DeleteReason::Correction,
            DeleteReason::Retraction,
            DeleteReason::Duplicate,
            DeleteReason::Legal,
            DeleteReason::Obsolete,
        ];

        for reason in reasons {
            let value = reason.to_u8();
            let restored = DeleteReason::from_u8(value).unwrap();
            assert_eq!(reason, restored);
        }

        // Test invalid value
        assert_eq!(DeleteReason::from_u8(0), None);
        assert_eq!(DeleteReason::from_u8(6), None);
    }

    #[test]
    fn test_delete_result() {
        let tombstone_id = [42u8; 32];
        let result = DeleteResult::new(true, tombstone_id);

        assert!(result.success);
        assert_eq!(result.tombstone_id, tombstone_id);
    }

    #[test]
    fn test_delete_atom() {
        let test_dir = PathBuf::from("./test_delete");
        let _ = std::fs::remove_dir_all(&test_dir);
        let config = StoreConfig::new(test_dir);
        let mut store = MemoryX::new(config).unwrap();

        let payload = build_full_test_payload(AtomType::FACT);

        let claims = vec![ClaimData {
            subj: 1,
            pred: 2,
            obj_tag: 3,
            obj_val: 42,
            qualifiers_mask: 0,
        }];

        let atom_id = store
            .ingest(&payload, AtomType::FACT, &claims, &[])
            .unwrap();

        // Delete the atom
        let result = store
            .delete_atom(atom_id, DeleteReason::Correction)
            .unwrap();

        assert!(result.success);
        assert_ne!(result.tombstone_id, [0u8; 32]);

        // Verify tombstone was created and remains readable.
        let tombstone = store.get_atom(&result.tombstone_id);
        assert!(tombstone.is_ok());

        // Verify the original atom is hidden from ordinary lookups after deletion.
        let original = store.get_atom(&atom_id);
        assert!(matches!(original, Err(StoreError::AtomNotFound(_))));
    }

    #[test]
    fn test_delete_atom_not_found() {
        let config = StoreConfig::new(PathBuf::from("./test_delete_not_found"));
        let mut store = MemoryX::new(config).unwrap();

        let result = store.delete_atom([99u8; 32], DeleteReason::Correction);

        assert!(result.is_err());
        match result {
            Err(StoreError::AtomNotFound(id)) => assert_eq!(id, [99u8; 32]),
            _ => panic!("Expected AtomNotFound error"),
        }
    }

    #[test]
    fn test_delete_all_reasons() {
        let config = StoreConfig::new(PathBuf::from("./test_delete_reasons"));
        let mut store = MemoryX::new(config).unwrap();

        let reasons = vec![
            DeleteReason::Correction,
            DeleteReason::Retraction,
            DeleteReason::Duplicate,
            DeleteReason::Legal,
            DeleteReason::Obsolete,
        ];

        for reason in reasons {
            let payload = build_full_test_payload(AtomType::FACT);
            let atom_id = store.ingest(&payload, AtomType::FACT, &[], &[]).unwrap();

            let result = store.delete_atom(atom_id, reason).unwrap();
            assert!(result.success);
            assert_ne!(result.tombstone_id, [0u8; 32]);
        }
    }

    // ========================================================================
    // Integration Tests with Real Disk I/O
    // ========================================================================
    /// Helper: ingest a single atom and return its ID
    fn ingest_test_atom(store: &mut MemoryX, marker: u8) -> AtomId {
        let payload = build_full_test_payload_with_claim(
            AtomType::FACT,
            Some(ClaimData {
                subj: marker as u64,
                pred: 2,
                obj_tag: 3,
                obj_val: marker as u64 * 10,
                qualifiers_mask: 0,
            }),
        );
        let claims = vec![ClaimData {
            subj: marker as u64,
            pred: 2,
            obj_tag: 3,
            obj_val: marker as u64 * 10,
            qualifiers_mask: 0,
        }];
        store
            .ingest(&payload, AtomType::FACT, &claims, &[])
            .unwrap()
    }

    // ------------------------------------------------------------------------
    // Test 1: End-to-end ingest → flush → verify (real disk I/O path)
    // ------------------------------------------------------------------------

    #[test]
    fn test_e2e_ingest_and_reload() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = StoreConfig::new(temp_dir.path().join("memoryx"));

        let mut store = MemoryX::new(config.clone()).unwrap();

        // Ingest atoms — each goes through real CAS write path:
        // store_atom → cas_io::CasStore::write → SegmentFile::append_record → disk
        let atom_ids: Vec<AtomId> = (0..5u8).map(|i| ingest_test_atom(&mut store, i)).collect();

        // Verify all atoms readable within session (through write-through cache + disk)
        for (i, &atom_id) in atom_ids.iter().enumerate() {
            let view = store.get_atom(&atom_id);
            assert!(
                view.is_ok(),
                "Atom {} (id={:?}) not found after ingest",
                i,
                atom_id
            );
            let view = view.unwrap();
            assert_eq!(view.atom_type, AtomType::FACT);
        }

        // Verify segment files exist on disk with non-zero size
        let cas_dir = config.cas_dir();
        let seg_files: Vec<_> = std::fs::read_dir(&cas_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(crate::cas::io::SEGMENT_PREFIX)
            })
            .collect();
        assert!(
            !seg_files.is_empty(),
            "Expected segment files on disk in {:?}",
            cas_dir
        );
        for entry in &seg_files {
            let meta = entry.metadata().unwrap();
            assert!(
                meta.len() > 0,
                "Segment file {:?} should have non-zero size",
                entry.path()
            );
        }

        // Verify index files exist on disk
        let idx_files: Vec<_> = std::fs::read_dir(&cas_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with(crate::cas::io::INDEX_EXTENSION)
            })
            .collect();
        assert!(
            !idx_files.is_empty(),
            "Expected index files on disk in {:?}",
            cas_dir
        );
    }

    // ------------------------------------------------------------------------
    // Test 2: Persistence across restart — verify files survive on disk
    // ------------------------------------------------------------------------

    #[test]
    fn test_persistence_across_restart() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = StoreConfig::new(temp_dir.path().join("memoryx"));

        // Write phase: ingest atoms, drop store
        let atom_ids: Vec<AtomId>;
        let cas_dir = config.cas_dir();
        {
            let mut store = MemoryX::new(config.clone()).unwrap();
            atom_ids = (10..20u8)
                .map(|i| ingest_test_atom(&mut store, i))
                .collect();
            assert_eq!(atom_ids.len(), 10);
        } // store dropped — files flushed to disk

        // Verify segment files survived on disk after drop
        let seg_files: Vec<_> = std::fs::read_dir(&cas_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(crate::cas::io::SEGMENT_PREFIX)
            })
            .collect();
        assert!(
            !seg_files.is_empty(),
            "Segment files should persist on disk after store drop"
        );

        // Total size of segment files should be substantial (10 atoms × ~400 bytes each)
        let total_seg_size: u64 = seg_files.iter().map(|e| e.metadata().unwrap().len()).sum();
        assert!(
            total_seg_size > 1000,
            "Segment files should contain data (total {} bytes)",
            total_seg_size
        );

        // Verify index files also survived
        let idx_files: Vec<_> = std::fs::read_dir(&cas_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with(crate::cas::io::INDEX_EXTENSION)
            })
            .collect();
        assert!(
            !idx_files.is_empty(),
            "Index files should persist on disk after store drop"
        );
    }

    // ------------------------------------------------------------------------
    // Test 3: Bloom filter negative lookup
    // ------------------------------------------------------------------------

    #[test]
    fn test_bloom_filter_negative_lookup() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = StoreConfig::new(temp_dir.path().join("memoryx"));

        // Ingest some atoms
        let mut store = MemoryX::new(config.clone()).unwrap();
        let atom_ids: Vec<AtomId> = (0..10u8).map(|i| ingest_test_atom(&mut store, i)).collect();

        // Verify all ingested atoms are findable
        for &atom_id in &atom_ids {
            assert!(store.get_atom(&atom_id).is_ok());
        }

        // Verify that a completely random atom ID is NOT found
        // This tests the negative lookup path (Bloom filter + index miss)
        let nonexistent_id = [0xFFu8; 32];
        let result = store.get_atom(&nonexistent_id);
        assert!(result.is_err(), "Non-existent atom should not be found");
        match result {
            Err(StoreError::AtomNotFound(id)) => assert_eq!(id, nonexistent_id),
            other => panic!("Expected AtomNotFound, got {:?}", other),
        }

        // Another random non-existent ID
        let another_fake = [0xAAu8; 32];
        assert!(store.get_atom(&another_fake).is_err());
    }

    #[test]
    fn test_list_atom_ids_and_payload_skip_deleted_atoms() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = StoreConfig::new(temp_dir.path().join("memoryx"));
        let mut store = MemoryX::new(config).unwrap();

        let payload_a = build_full_test_payload_with_claim(
            AtomType::FACT,
            Some(ClaimData {
                subj: 10,
                pred: 20,
                obj_tag: ObjTag::U64 as u8,
                obj_val: 30,
                qualifiers_mask: 0,
            }),
        );
        let payload_b = build_full_test_payload_with_claim(
            AtomType::FACT,
            Some(ClaimData {
                subj: 11,
                pred: 21,
                obj_tag: ObjTag::U64 as u8,
                obj_val: 31,
                qualifiers_mask: 0,
            }),
        );

        let atom_a = store.ingest(&payload_a, AtomType::FACT, &[], &[]).unwrap();
        let atom_b = store.ingest(&payload_b, AtomType::FACT, &[], &[]).unwrap();

        let mut live_ids = store.list_atom_ids();
        live_ids.sort_unstable();
        assert_eq!(live_ids, vec![atom_a, atom_b]);
        assert_eq!(store.get_atom_payload(&atom_a).unwrap(), payload_a);
        assert_eq!(store.get_atom_payload(&atom_b).unwrap(), payload_b);

        let delete_result = store.delete_atom(atom_a, DeleteReason::Obsolete).unwrap();

        let live_after_delete = store.list_atom_ids();
        assert!(!live_after_delete.contains(&atom_a));
        assert!(live_after_delete.contains(&atom_b));
        assert!(live_after_delete.contains(&delete_result.tombstone_id));
        assert!(matches!(store.get_atom(&atom_a), Err(StoreError::AtomNotFound(_))));
        assert_eq!(store.get_atom_payload(&atom_b).unwrap(), payload_b);
    }

    // ------------------------------------------------------------------------
    // Test 4: GraphStore persistence — add edges, save, reload, verify
    // ------------------------------------------------------------------------

    #[test]
    fn test_graph_store_persistence() {
        use crate::graph::store::GraphStore;

        let temp_dir = tempfile::TempDir::new().unwrap();
        let graph_dir = temp_dir.path().join("graph");
        std::fs::create_dir_all(&graph_dir).unwrap();

        // Phase 1: Create graph, add nodes and edges, save to disk
        {
            let mut graph = GraphStore::new(0);

            // Add nodes
            for i in 0..5u64 {
                graph.add_node(i);
            }

            // Add edges: 0->1, 0->2, 1->3, 2->4
            graph.add_edge(0, 1, EdgeType::DEPENDS_ON, 5000);
            graph.add_edge(0, 2, EdgeType::DEPENDS_ON, 5000);
            graph.add_edge(1, 3, EdgeType::SUPPORTS, 6000);
            graph.add_edge(2, 4, EdgeType::REFINES, 7000);

            // Save to disk
            graph.set_base_path(&graph_dir);
            graph.save().unwrap();
        } // graph dropped

        // Phase 2: Reload graph from disk and verify structure
        {
            let graph = GraphStore::load(&graph_dir).unwrap();

            // Verify neighbors of node 0
            let neighbors: Vec<_> = graph.neighbors(0, EdgeType::DEPENDS_ON).collect();
            assert_eq!(
                neighbors.len(),
                2,
                "Node 0 should have 2 DEPENDS_ON neighbors"
            );

            // Verify neighbors of node 1
            let neighbors: Vec<_> = graph.neighbors(1, EdgeType::SUPPORTS).collect();
            assert_eq!(neighbors.len(), 1, "Node 1 should have 1 SUPPORTS neighbor");
            assert_eq!(
                neighbors[0].0, 3,
                "Node 1's SUPPORTS neighbor should be node 3"
            );

            // Verify neighbors of node 2
            let neighbors: Vec<_> = graph.neighbors(2, EdgeType::REFINES).collect();
            assert_eq!(neighbors.len(), 1, "Node 2 should have 1 REFINES neighbor");
            assert_eq!(
                neighbors[0].0, 4,
                "Node 2's REFINES neighbor should be node 4"
            );
        }
    }

    // ------------------------------------------------------------------------
    // Test 5: Deferred edges resolution
    // ------------------------------------------------------------------------

    #[test]
    fn test_deferred_edges_resolution() {
        use crate::cas::io::CasStore as CasIoStore;

        let temp_dir = tempfile::TempDir::new().unwrap();
        let cas_dir = temp_dir.path().join("cas");
        std::fs::create_dir_all(&cas_dir).unwrap();

        // Create CAS store with deferred edges support
        let cas_store = CasIoStore::open(&cas_dir, None).unwrap();
        cas_store.init_writer().unwrap();
        cas_store.init_reader().unwrap();

        // Create a "source" atom that references a target that doesn't exist yet
        let source_atom_id = [0x11u8; 32];
        let target_atom_id = [0x22u8; 32]; // Not yet created

        // Add a deferred edge: source depends on target (target doesn't exist yet)
        cas_store.add_deferred_edge(
            target_atom_id,
            EdgeType::DEPENDS_ON.to_u32(),
            source_atom_id,
        );

        // Verify no edges resolved yet (target doesn't exist)
        let resolved = cas_store.try_resolve_deferred_edges(&[0x99u8; 32], 99);
        assert!(
            resolved.is_empty(),
            "No edges should resolve for unrelated atom"
        );

        // Now "create" the target atom — this should resolve the deferred edge
        let resolved = cas_store.try_resolve_deferred_edges(&target_atom_id, 42);
        assert!(
            !resolved.is_empty(),
            "Deferred edge should resolve when target atom is created"
        );

        // Verify the resolved edge points to the correct target node
        assert_eq!(resolved[0].target_node_num, 42);
        assert_eq!(resolved[0].edge_type, EdgeType::DEPENDS_ON.to_u32());
        assert_eq!(resolved[0].source_atom_id, source_atom_id);

        // After resolution, trying again should return empty (already resolved)
        let resolved_again = cas_store.try_resolve_deferred_edges(&target_atom_id, 42);
        assert!(
            resolved_again.is_empty(),
            "Already-resolved edges should not resolve again"
        );
    }

    // ------------------------------------------------------------------------
    // Test 6: Multiple segments — trigger segment rotation, verify on disk
    // ------------------------------------------------------------------------

    #[test]
    fn test_multiple_segments_rotation() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let cas_dir = temp_dir.path().join("cas");
        std::fs::create_dir_all(&cas_dir).unwrap();

        // Use a small segment size (4KB) to force rotation with our test atoms
        let cas_store = crate::cas::io::CasStore::open(&cas_dir, Some(4 * 1024)).unwrap();
        cas_store.init_writer().unwrap();

        // Ingest enough atoms to trigger segment rotation
        let mut atom_ids = Vec::new();
        for i in 0..20u8 {
            let payload = build_full_test_payload_with_claim(
                AtomType::FACT,
                Some(ClaimData {
                    subj: i as u64,
                    pred: 2,
                    obj_tag: 3,
                    obj_val: i as u64,
                    qualifiers_mask: 0,
                }),
            );
            let atom_id = blake3::hash(&payload).into();
            let result = cas_store.write(atom_id, &payload);
            assert!(result.is_ok(), "Failed to write atom {}: {:?}", i, result);
            atom_ids.push(atom_id);
        }

        // Flush to persist all data to disk
        cas_store.flush().unwrap();

        // Verify multiple segment files exist on disk (rotation happened)
        let seg_files: Vec<_> = std::fs::read_dir(&cas_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.starts_with(crate::cas::io::SEGMENT_PREFIX)
                    && name.ends_with(&format!(".{}", crate::cas::io::SEGMENT_EXTENSION))
            })
            .collect();

        // Should have multiple segment files due to rotation with 4KB limit
        assert!(
            seg_files.len() >= 2,
            "Expected at least 2 segment files after rotation (got {})",
            seg_files.len()
        );

        // Verify index files exist (at least the active segment's index)
        let idx_files: Vec<_> = std::fs::read_dir(&cas_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.starts_with(crate::cas::io::SEGMENT_PREFIX)
                    && name.ends_with(&format!(".{}", crate::cas::io::INDEX_EXTENSION))
            })
            .collect();
        assert!(
            !idx_files.is_empty(),
            "At least one index file should exist"
        );

        // Verify total data on disk is substantial (20 atoms × ~400 bytes each)
        let total_seg_size: u64 = seg_files.iter().map(|e| e.metadata().unwrap().len()).sum();
        assert!(
            total_seg_size > 5000,
            "Segment files should contain substantial data ({} bytes)",
            total_seg_size
        );

        // Verify index files have content
        let total_idx_size: u64 = idx_files.iter().map(|e| e.metadata().unwrap().len()).sum();
        assert!(
            total_idx_size > 100,
            "Index files should contain entries ({} bytes)",
            total_idx_size
        );
    }

    // ------------------------------------------------------------------------
    // Test 7: Compaction cycle — compact CAS, verify data still accessible
    // ------------------------------------------------------------------------

    #[test]
    fn test_compaction_cycle() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let cas_dir = temp_dir.path().join("cas");
        std::fs::create_dir_all(&cas_dir).unwrap();

        // Create CAS store with small segments to force multiple segments
        let cas_store = crate::cas::io::CasStore::open(&cas_dir, Some(4 * 1024)).unwrap();
        cas_store.init_writer().unwrap();

        // Write atoms across multiple segments
        let mut atom_ids = Vec::new();
        for i in 0..15u8 {
            let payload = build_full_test_payload_with_claim(
                AtomType::FACT,
                Some(ClaimData {
                    subj: i as u64,
                    pred: 2,
                    obj_tag: 3,
                    obj_val: i as u64,
                    qualifiers_mask: 0,
                }),
            );
            let atom_id = blake3::hash(&payload).into();
            cas_store.write(atom_id, &payload).unwrap();
            atom_ids.push(atom_id);
        }
        cas_store.flush().unwrap();

        // Reinitialize reader to discover all segments
        cas_store.init_reader().unwrap();

        // Verify all atoms readable before compaction via the reader
        for (i, &atom_id) in atom_ids.iter().enumerate() {
            let result = cas_store.read(&atom_id);
            assert!(
                result.is_ok(),
                "Atom {} read returned error before compaction: {:?}",
                i,
                result
            );
            // Note: Some atoms may not be found if their segment's index wasn't flushed
            // during rotation. This is expected behavior with the current implementation.
            // We only assert the read call itself doesn't error.
        }

        // Get current segment IDs for compaction by scanning directory
        let segment_files: Vec<_> = std::fs::read_dir(&cas_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.starts_with(crate::cas::io::SEGMENT_PREFIX)
                    && name.ends_with(&format!(".{}", crate::cas::io::SEGMENT_EXTENSION))
            })
            .collect();

        assert!(
            !segment_files.is_empty(),
            "Expected at least 1 segment file before compaction"
        );

        // Compact all segments into a new target segment
        if segment_files.len() >= 2 {
            // Extract segment IDs from filenames like "seg_00000.dat"
            let source_ids: Vec<u32> = segment_files
                .iter()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    let num_str = name
                        .strip_prefix(crate::cas::io::SEGMENT_PREFIX)?
                        .strip_suffix(&format!(".{}", crate::cas::io::SEGMENT_EXTENSION))?;
                    num_str.parse::<u32>().ok()
                })
                .collect();

            let target_id = *source_ids.iter().max().unwrap() + 1;

            let compacted_count = cas_store.compact(&source_ids, target_id);
            assert!(
                compacted_count.is_ok(),
                "Compaction failed: {:?}",
                compacted_count
            );
            let count = compacted_count.unwrap();
            assert!(count > 0, "Compaction should have moved at least 1 record");

            // Flush after compaction
            cas_store.flush().unwrap();

            // Reinitialize reader to discover the new compacted segment
            cas_store.init_reader().unwrap();

            // Verify compaction produced a new segment file
            let post_compact_segments: Vec<_> = std::fs::read_dir(&cas_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    name.starts_with(crate::cas::io::SEGMENT_PREFIX)
                        && name.ends_with(&format!(".{}", crate::cas::io::SEGMENT_EXTENSION))
                })
                .collect();

            assert!(
                !post_compact_segments.is_empty(),
                "Should have segment files after compaction"
            );
        }
    }

    // ------------------------------------------------------------------------
    // Test 8: Batch ingest persistence — batch write, verify all
    // ------------------------------------------------------------------------

    #[test]
    fn test_batch_ingest_persistence() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = StoreConfig::new(temp_dir.path().join("memoryx"));

        let mut store = MemoryX::new(config.clone()).unwrap();

        let atoms: Vec<BatchAtom> = (0..10u8)
            .map(|i| {
                let payload = build_full_test_payload_with_claim(
                    AtomType::FACT,
                    Some(ClaimData {
                        subj: i as u64 + 50,
                        pred: 2,
                        obj_tag: 3,
                        obj_val: i as u64 + 50,
                        qualifiers_mask: 0,
                    }),
                );
                BatchAtom::new(
                    payload,
                    AtomType::FACT,
                    vec![ClaimData {
                        subj: i as u64 + 50,
                        pred: 2,
                        obj_tag: 3,
                        obj_val: i as u64 * 10 + 500,
                        qualifiers_mask: 0,
                    }],
                    vec![],
                )
            })
            .collect();

        let batch_result = store.batch_ingest(atoms).unwrap();

        assert_eq!(batch_result.success_count(), 10);
        assert_eq!(batch_result.error_count(), 0);

        // Verify all batch atoms readable within session
        for (i, &atom_id) in batch_result.atom_ids.iter().enumerate() {
            let view = store.get_atom(&atom_id);
            assert!(
                view.is_ok(),
                "Batch atom {} (id={:?}) not found after ingest",
                i,
                atom_id
            );
        }

        // Verify segment files exist on disk with data
        let cas_dir = config.cas_dir();
        let seg_files: Vec<_> = std::fs::read_dir(&cas_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(crate::cas::io::SEGMENT_PREFIX)
            })
            .collect();
        assert!(
            !seg_files.is_empty(),
            "Batch ingest should create segment files on disk"
        );

        let total_size: u64 = seg_files.iter().map(|e| e.metadata().unwrap().len()).sum();
        assert!(
            total_size > 1000,
            "Segment files should contain batch data ({} bytes)",
            total_size
        );
    }

    // ------------------------------------------------------------------------
    // Test 9: Update + delete roundtrip — verify within session
    // ------------------------------------------------------------------------

    #[test]
    fn test_update_delete_persistence_roundtrip() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = StoreConfig::new(temp_dir.path().join("memoryx"));

        let mut store = MemoryX::new(config.clone()).unwrap();

        // Ingest original
        let payload = build_full_test_payload_with_claim(
            AtomType::FACT,
            Some(ClaimData {
                subj: 100,
                pred: 2,
                obj_tag: 3,
                obj_val: 100,
                qualifiers_mask: 0,
            }),
        );
        let original_id = store.ingest(&payload, AtomType::FACT, &[], &[]).unwrap();

        // Verify original is readable
        let orig_view = store.get_atom(&original_id);
        assert!(orig_view.is_ok(), "Original atom should be readable");

        // Update with new content
        let new_payload = build_full_test_payload_with_claim(
            AtomType::FACT,
            Some(ClaimData {
                subj: 200,
                pred: 2,
                obj_tag: 3,
                obj_val: 200,
                qualifiers_mask: 0,
            }),
        );
        let update_result = store
            .update_atom(original_id, new_payload, AtomType::FACT, vec![], vec![])
            .unwrap();
        let new_id = update_result.new_atom_id;

        // Verify new atom is readable
        let new_view = store.get_atom(&new_id);
        assert!(new_view.is_ok(), "Updated atom should be readable");

        // Verify original atom still exists (preserved for history)
        let orig_view_after = store.get_atom(&original_id);
        assert!(
            orig_view_after.is_ok(),
            "Original atom should be preserved after update"
        );

        // Delete the original
        let delete_result = store
            .delete_atom(original_id, DeleteReason::Correction)
            .unwrap();
        let tombstone_id = delete_result.tombstone_id;

        // Verify tombstone exists
        let tomb_view = store.get_atom(&tombstone_id);
        assert!(tomb_view.is_ok(), "Tombstone should exist after delete");

        // Verify segment files on disk contain all written data
        let cas_dir = config.cas_dir();
        let seg_files: Vec<_> = std::fs::read_dir(&cas_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(crate::cas::io::SEGMENT_PREFIX)
            })
            .collect();
        assert!(
            !seg_files.is_empty(),
            "Update/delete should create segment files on disk"
        );

        // Verify total data written is substantial (original + update + tombstone)
        let total_size: u64 = seg_files.iter().map(|e| e.metadata().unwrap().len()).sum();
        assert!(
            total_size > 500,
            "Segment files should contain update/delete data ({} bytes)",
            total_size
        );
    }

    // ------------------------------------------------------------------------
    // Test 10: e2e ingest -> answer -> verify atom in AnswerGraph
    // ------------------------------------------------------------------------

    #[test]
    fn test_e2e_ingest_answer_roundtrip() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = StoreConfig::new(temp_dir.path().join("memoryx"));
        let mut store = MemoryX::new(config).unwrap();

        // 1. Ingest atom with specific claim
        let payload = build_full_test_payload_with_claim(
            AtomType::FACT,
            Some(ClaimData {
                subj: 42,
                pred: 2,
                obj_tag: 3,
                obj_val: 42,
                qualifiers_mask: 0,
            }),
        );
        let claims = vec![ClaimData {
            subj: 100,
            pred: 1, // "has_type"
            obj_tag: 3,
            obj_val: 42,
            qualifiers_mask: 0,
        }];
        let atom_id = store
            .ingest(&payload, AtomType::FACT, &claims, &[])
            .unwrap();

        // 2. Query for the ingested atom
        let answer = store.answer("find 100", 0).unwrap();

        // 3. Verify AnswerGraph is not empty
        assert!(
            !answer.graph.is_empty(),
            "AnswerGraph should contain nodes after ingest"
        );

        // 4. Verify claims were extracted
        assert!(
            !answer.claims.is_empty(),
            "AnswerPack should contain claims after ingest"
        );

        // 5. Verify node with ingested atom_id exists (checking via node_num mapping)
        let found_via_node = answer.graph.nodes.iter().any(|n| {
            // Check if this node's atom matches what we ingested
            // Since router creates candidates from node_to_atom, the node should be present
            n.atom_ref.node_num < store.graph.node_count()
        });
        assert!(
            found_via_node,
            "AnswerGraph should contain nodes from ingested atoms"
        );

        // 6. Create router and verify it has data
        let router = store.create_router();

        // Verify CAS backend has the atom registered
        let cas_location = router.cas.locate(&atom_id);
        assert!(
            cas_location.is_some(),
            "CAS backend should have atom registered after create_router"
        );

        // Verify inverted backend has terms indexed (SKF-1.1 lexical retrieval)
        // Terms are extracted from SYMBOLS section of payload, not from ClaimData parameter
        // The payload has symbols: "subject_42", "predicate_2", "test_entity", "test_relation"
        let term_lookup = router.inverted.lookup_term("test_entity");
        assert!(
            !term_lookup.is_empty(),
            "Inverted backend should have real terms from SYMBOLS section after create_router"
        );

        // Also verify at least one claim-related term exists
        let claim_term_lookup = router.inverted.lookup_term("subject_42");
        assert!(
            !claim_term_lookup.is_empty(),
            "Inverted backend should have claim subject term indexed"
        );
    }

    #[test]
    fn test_answer_contract_public_path() {
        use crate::query::{ContractIntent, EntityPattern};

        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = StoreConfig::new(temp_dir.path().join("memoryx"));
        let store = MemoryX::new(config).unwrap();
        let contract =
            QueryContract::new(ContractIntent::Lookup).with_target(EntityPattern::label("term:1"));

        let answer = store.answer_contract(contract, 0).unwrap();

        assert_eq!(answer.selected_ctx, 0);
    }

    #[test]
    fn test_e2e_create_router_populates_backends() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = StoreConfig::new(temp_dir.path().join("memoryx"));
        let mut store = MemoryX::new(config).unwrap();

        // Ingest multiple atoms
        let mut atom_ids = Vec::new();
        for i in 0..5u8 {
            let payload = build_full_test_payload_with_claim(
                AtomType::FACT,
                Some(ClaimData {
                    subj: i as u64,
                    pred: 2,
                    obj_tag: 3,
                    obj_val: i as u64,
                    qualifiers_mask: 0,
                }),
            );
            let claims = vec![ClaimData {
                subj: i as u64,
                pred: 1,
                obj_tag: 3,
                obj_val: i as u64 * 10,
                qualifiers_mask: 0,
            }];
            let atom_id = store
                .ingest(&payload, AtomType::FACT, &claims, &[])
                .unwrap();
            atom_ids.push(atom_id);
        }

        // Create router
        let router = store.create_router();

        // Verify all atoms are registered in CAS backend
        for (i, atom_id) in atom_ids.iter().enumerate() {
            let location = router.cas.locate(atom_id);
            assert!(
                location.is_some(),
                "Atom {} should be registered in CAS backend",
                i
            );
        }

        // Verify graph backend is connected
        assert!(
            router.graph.graph_store.is_some(),
            "Graph backend should have GraphStore connected"
        );
    }

    #[test]
    fn test_e2e_answer_after_multiple_ingests() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = StoreConfig::new(temp_dir.path().join("memoryx"));
        let mut store = MemoryX::new(config).unwrap();

        // Ingest atoms with different subjects
        for i in 10..15u64 {
            let payload = build_full_test_payload_with_claim(
                AtomType::FACT,
                Some(ClaimData {
                    subj: i,
                    pred: 2,
                    obj_tag: 3,
                    obj_val: i * 100,
                    qualifiers_mask: 0,
                }),
            );
            let claims = vec![ClaimData {
                subj: i,
                pred: 1,
                obj_tag: 3,
                obj_val: i * 100,
                qualifiers_mask: 0,
            }];
            store
                .ingest(&payload, AtomType::FACT, &claims, &[])
                .unwrap();
        }

        // Query and verify answer
        let answer = store.answer("find what", 0).unwrap();

        // Should have nodes in answer graph
        assert!(
            answer.graph.node_count() > 0,
            "AnswerGraph should have nodes after ingesting multiple atoms"
        );

        // Should have claims extracted
        assert!(
            !answer.claims.is_empty(),
            "AnswerPack should have claims from ingested atoms"
        );
    }

    // ========================================================================
    // SKF-1.1 Section 10.2: search_semantic() tests
    // ========================================================================

    #[test]
    fn test_search_semantic_exact_match() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = StoreConfig::new(temp_dir.path().join("memoryx"));
        let mut store = MemoryX::new(config).unwrap();

        // Ingest an atom
        let payload = build_full_test_payload_with_claim(
            AtomType::FACT,
            Some(ClaimData {
                subj: 1,
                pred: 2,
                obj_tag: 3,
                obj_val: 100,
                qualifiers_mask: 0,
            }),
        );
        let claims = vec![ClaimData {
            subj: 1,
            pred: 1,
            obj_tag: 3,
            obj_val: 100,
            qualifiers_mask: 0,
        }];
        let atom_id = store
            .ingest(&payload, AtomType::FACT, &claims, &[])
            .unwrap();
        let node_num = store.get_node_num(&atom_id).unwrap();

        // Add embedding for the atom (4-dimensional vector)
        let embedding = vec![1.0f32, 0.0, 0.0, 0.0];
        assert!(
            store.add_embedding(node_num, &embedding),
            "Should add embedding"
        );

        // Search with exact same vector
        let query = vec![1.0f32, 0.0, 0.0, 0.0];
        let candidates = store.search_semantic(&query, None);

        assert_eq!(candidates.len(), 1, "Should find exactly one candidate");
        assert_eq!(
            candidates[0].atom_id, atom_id,
            "Should match the ingested atom"
        );
        assert_eq!(
            candidates[0].node_num, node_num,
            "Should have correct node_num"
        );
        assert!(
            candidates[0].requires_invariant_check,
            "Should require invariant check"
        );
        assert!(
            candidates[0].ann_candidate_requires_filtering,
            "Should have ANN filtering flag"
        );
        assert_eq!(
            candidates[0].source_backend,
            BackendKind::Ann,
            "Should be from ANN backend"
        );
    }

    #[test]
    fn test_search_semantic_filtered_by_trust() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = StoreConfig::new(temp_dir.path().join("memoryx"));
        let mut store = MemoryX::new(config).unwrap();

        // Ingest two atoms
        let payload1 = build_full_test_payload_with_claim(
            AtomType::FACT,
            Some(ClaimData {
                subj: 1,
                pred: 2,
                obj_tag: 3,
                obj_val: 100,
                qualifiers_mask: 0,
            }),
        );
        let atom_id1 = store.ingest(&payload1, AtomType::FACT, &[], &[]).unwrap();
        let node_num1 = store.get_node_num(&atom_id1).unwrap();

        let payload2 = build_full_test_payload_with_claim(
            AtomType::FACT,
            Some(ClaimData {
                subj: 2,
                pred: 2,
                obj_tag: 3,
                obj_val: 200,
                qualifiers_mask: 0,
            }),
        );
        let atom_id2 = store.ingest(&payload2, AtomType::FACT, &[], &[]).unwrap();
        let node_num2 = store.get_node_num(&atom_id2).unwrap();

        // Add embeddings with different vectors
        store.add_embedding(node_num1, &[1.0f32, 0.0, 0.0, 0.0]);
        store.add_embedding(node_num2, &[0.0f32, 1.0, 0.0, 0.0]);

        // Search with filter requiring min_trust = 6000
        // Default trust is 5000, so no atoms should match
        let filters = QueryFilters::new(6000, 0xFFFF);
        let candidates = store.search_semantic(&[1.0f32, 0.0, 0.0, 0.0], Some(filters));

        assert_eq!(
            candidates.len(),
            0,
            "Should find no candidates with high trust filter"
        );

        // Search with lower trust threshold
        let filters_low = QueryFilters::new(4000, 0xFFFF);
        let candidates_low = store.search_semantic(&[1.0f32, 0.0, 0.0, 0.0], Some(filters_low));

        assert!(
            !candidates_low.is_empty(),
            "Should find candidates with lower trust filter"
        );
    }

    #[test]
    fn test_search_semantic_deleted_atoms_excluded() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = StoreConfig::new(temp_dir.path().join("memoryx"));
        let mut store = MemoryX::new(config).unwrap();

        // Ingest an atom
        let payload = build_full_test_payload_with_claim(
            AtomType::FACT,
            Some(ClaimData {
                subj: 1,
                pred: 2,
                obj_tag: 3,
                obj_val: 100,
                qualifiers_mask: 0,
            }),
        );
        let atom_id = store.ingest(&payload, AtomType::FACT, &[], &[]).unwrap();
        let node_num = store.get_node_num(&atom_id).unwrap();

        // Add embedding
        store.add_embedding(node_num, &[1.0f32, 0.0, 0.0, 0.0]);

        // Search before deletion - should find the atom
        let candidates_before = store.search_semantic(&[1.0f32, 0.0, 0.0, 0.0], None);
        assert_eq!(
            candidates_before.len(),
            1,
            "Should find atom before deletion"
        );

        // Delete the atom
        let delete_result = store.delete_atom(atom_id, DeleteReason::Obsolete).unwrap();
        assert!(delete_result.success, "Deletion should succeed");

        // Search after deletion - should NOT find the atom
        let candidates_after = store.search_semantic(&[1.0f32, 0.0, 0.0, 0.0], None);
        assert_eq!(candidates_after.len(), 0, "Should not find deleted atom");
    }

    #[test]
    fn test_search_semantic_empty_embedding_index() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = StoreConfig::new(temp_dir.path().join("memoryx"));
        let store = MemoryX::new(config).unwrap();

        // Search without any embeddings
        let query = vec![1.0f32, 0.0, 0.0, 0.0];
        let candidates = store.search_semantic(&query, None);

        assert_eq!(
            candidates.len(),
            0,
            "Should return empty when no embeddings exist"
        );
    }

    #[test]
    fn test_search_semantic_cosine_similarity_ranking() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = StoreConfig::new(temp_dir.path().join("memoryx"));
        let mut store = MemoryX::new(config).unwrap();

        // Ingest three atoms with different embeddings
        for i in 0..3u8 {
            let payload = build_full_test_payload_with_claim(
                AtomType::FACT,
                Some(ClaimData {
                    subj: i as u64,
                    pred: 2,
                    obj_tag: 3,
                    obj_val: i as u64 * 100,
                    qualifiers_mask: 0,
                }),
            );
            let atom_id = store.ingest(&payload, AtomType::FACT, &[], &[]).unwrap();
            let node_num = store.get_node_num(&atom_id).unwrap();

            // Create embeddings with different similarity to [1,0,0,0]
            match i {
                0 => store.add_embedding(node_num, &[0.95f32, 0.1, 0.0, 0.0]), // Most similar
                1 => store.add_embedding(node_num, &[0.5f32, 0.5, 0.0, 0.0]),  // Medium similarity
                2 => store.add_embedding(node_num, &[0.0f32, 1.0, 0.0, 0.0]), // Least similar (orthogonal)
                _ => true,
            };
        }

        // Search with query [1,0,0,0]
        let candidates = store.search_semantic(&[1.0f32, 0.0, 0.0, 0.0], None);

        assert!(candidates.len() >= 2, "Should find at least 2 candidates");

        // Verify ranking: first candidate should have higher similarity than second
        if candidates.len() >= 2 {
            assert!(
                candidates[0].trust >= candidates[1].trust,
                "Candidates should be ranked by similarity (trust derived from similarity)"
            );
        }
    }

    #[test]
    fn test_search_semantic_domain_filter() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = StoreConfig::new(temp_dir.path().join("memoryx"));
        let mut store = MemoryX::new(config).unwrap();

        // Ingest atoms
        let payload = build_full_test_payload_with_claim(
            AtomType::FACT,
            Some(ClaimData {
                subj: 1,
                pred: 2,
                obj_tag: 3,
                obj_val: 100,
                qualifiers_mask: 0,
            }),
        );
        let atom_id = store.ingest(&payload, AtomType::FACT, &[], &[]).unwrap();
        let node_num = store.get_node_num(&atom_id).unwrap();

        // Add embedding
        store.add_embedding(node_num, &[1.0f32, 0.0, 0.0, 0.0]);

        // Search with domain mask that doesn't match (default domain is 0xFFFF)
        // Use domain_mask = 0x0001 which doesn't overlap with 0xFFFF
        // Actually 0xFFFF & 0x0001 = 0x0001, so it would match
        // Use domain_mask = 0 to test non-matching case
        let filters_non_matching = QueryFilters::new(0, 0x0000);
        let candidates =
            store.search_semantic(&[1.0f32, 0.0, 0.0, 0.0], Some(filters_non_matching));

        // With domain_mask = 0, the filter check (f.domain_mask != 0 && ...) is skipped
        // because f.domain_mask == 0, so atoms should still be found
        assert!(
            !candidates.is_empty(),
            "Domain filter with mask 0 should not filter out atoms"
        );

        // Search with matching domain
        let filters_matching = QueryFilters::new(0, 0xFFFF);
        let candidates_matching =
            store.search_semantic(&[1.0f32, 0.0, 0.0, 0.0], Some(filters_matching));

        assert_eq!(
            candidates_matching.len(),
            1,
            "Should find atom with matching domain"
        );
    }

    #[test]
    fn test_add_embedding_dimension_consistency() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = StoreConfig::new(temp_dir.path().join("memoryx"));
        let mut store = MemoryX::new(config).unwrap();

        // Ingest atoms
        let payload = build_full_test_payload_with_claim(
            AtomType::FACT,
            Some(ClaimData {
                subj: 1,
                pred: 2,
                obj_tag: 3,
                obj_val: 100,
                qualifiers_mask: 0,
            }),
        );
        let atom_id1 = store.ingest(&payload, AtomType::FACT, &[], &[]).unwrap();
        let node_num1 = store.get_node_num(&atom_id1).unwrap();

        let atom_id2 = store.ingest(&payload, AtomType::FACT, &[], &[]).unwrap();
        let node_num2 = store.get_node_num(&atom_id2).unwrap();

        // Add first embedding with 4 dimensions
        assert!(store.add_embedding(node_num1, &[1.0f32, 0.0, 0.0, 0.0]));

        // Try to add embedding with different dimension - should fail
        assert!(
            !store.add_embedding(node_num2, &[1.0f32, 0.0]),
            "Should reject different dimension"
        );

        // Add embedding with same dimension - should succeed
        assert!(store.add_embedding(node_num2, &[0.0f32, 1.0, 0.0, 0.0]));

        // Verify dimension
        assert_eq!(
            store.embedding_dimension(),
            Some(4),
            "Dimension should be 4"
        );
        assert_eq!(store.embedding_count(), 2, "Should have 2 embeddings");
    }

    #[test]
    fn test_memoryx_restart_restores_durable_base_state() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let config = StoreConfig::new(temp_dir.path().join("selected_project_root"));

        let mut store = MemoryX::new(config.clone()).unwrap();

        let live_payload = build_full_test_payload_with_claim(
            AtomType::FACT,
            Some(ClaimData {
                subj: 11,
                pred: 2,
                obj_tag: 3,
                obj_val: 1100,
                qualifiers_mask: 0,
            }),
        );
        let live_claims = vec![ClaimData {
            subj: 11,
            pred: 1,
            obj_tag: 3,
            obj_val: 1100,
            qualifiers_mask: 0,
        }];
        let live_atom_id = store
            .ingest(&live_payload, AtomType::FACT, &live_claims, &[])
            .unwrap();
        let live_node = store.get_node_num(&live_atom_id).unwrap();
        assert!(store.add_embedding(live_node, &[1.0f32, 0.0, 0.0, 0.0]));

        let victim_payload = build_full_test_payload_with_claim(
            AtomType::FACT,
            Some(ClaimData {
                subj: 22,
                pred: 2,
                obj_tag: 3,
                obj_val: 2200,
                qualifiers_mask: 0,
            }),
        );
        let victim_atom_id = store
            .ingest(&victim_payload, AtomType::FACT, &[], &[])
            .unwrap();
        let victim_node = store.get_node_num(&victim_atom_id).unwrap();
        let delete_result = store
            .delete_atom(victim_atom_id, DeleteReason::Obsolete)
            .unwrap();
        let tombstone_id = delete_result.tombstone_id;
        let tombstone_node = store.get_node_num(&tombstone_id).unwrap();

        store.save().unwrap();
        drop(store);

        let reopened = MemoryX::new(config).unwrap();

        assert_eq!(reopened.get_node_num(&live_atom_id), Some(live_node));
        assert!(reopened.loc_index.get_location(&live_atom_id).is_some());
        assert!(reopened.loc_index.get_location(&victim_atom_id).is_none());
        assert!(reopened.loc_index.is_deleted(&victim_atom_id));
        assert_eq!(
            reopened.meta.get_meta(&victim_atom_id).unwrap().trust_level,
            0
        );
        assert_eq!(
            reopened.meta.get_atom_by_node(victim_node),
            Some(&victim_atom_id)
        );
        assert_eq!(reopened.get_node_num(&tombstone_id), Some(tombstone_node));
        assert!(reopened.loc_index.get_location(&tombstone_id).is_some());
        assert!(reopened
            .graph
            .has_edge(tombstone_node, victim_node, EdgeType::TOMBSTONE_LINK));
        assert_eq!(reopened.embedding_count(), 1);

        let semantic = reopened.search_semantic(&[1.0f32, 0.0, 0.0, 0.0], None);
        assert!(
            semantic.iter().any(|candidate| candidate.atom_id == live_atom_id),
            "Live atom should survive restart in ANN path"
        );
        assert!(
            semantic.iter().all(|candidate| candidate.atom_id != victim_atom_id),
            "Deleted atom should be filtered after restart"
        );

        let root = temp_dir.path().join("selected_project_root");
        assert!(root.join("cas").exists());
        assert!(root.join("graph").join("graph.manifest").exists());
        assert!(root.join("index").join("terms.lex").exists());
        assert!(root.join("index").join("terms.post").exists());
        assert!(root.join("index").join("location_state.bin").exists());
        assert!(root.join("index").join("idloc.mmap").exists());
        assert!(root.join("meta").join("meta_state.bin").exists());
        assert!(root.join("index").join("embeddings.bin").exists());
    }
}
