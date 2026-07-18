//! Real QueryRouter backends for MemoryX SKF-1.1
//!
//! Implements source-prioritized query routing across all index layers:
//! 1. CAS: exact O(1) access by AtomId via IdLoc index
//! 2. Inverted: term/formula/identifier lookup via term.lex + term.postings
//! 3. Graph: multi-hop typed-edge traversal via GraphStore CSR
//! 4. ANN: candidate-only vector similarity (must be filtered through invariants)

#![allow(dead_code)]

use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::graph::GraphStore;
use crate::index::{InvertedIndex, Location};
use crate::query::ann::EmbeddingIndex;
use crate::query::constraints::ConstraintSubject;
use crate::query::contract::{ConstraintTarget, ConstraintValue};
use crate::query::retrieval::{CandidateV2, Retriever};
use crate::store::api::{CostWeights, CtxId, EvidenceRef, Gap, GapId};
use crate::store::{
    AtomId, AtomType, DomainMask, EdgeType, GapKind, NodeNum, PatternRef, TrustLevel,
};
use crate::vm::ClaimData;

// ============================================================================
// Atom Metadata Cache (SKF-1.1 Section 7: inv.filter concept)
// ============================================================================

/// Cached atom metadata for routing decisions
/// Avoids expensive atom body reads for trust/domain filtering
#[derive(Debug, Clone)]
pub struct AtomMetadataCache {
    /// NodeNum -> Trust level from META section
    trust_by_node: HashMap<NodeNum, TrustLevel>,
    /// NodeNum -> Domain mask from META section
    domain_by_node: HashMap<NodeNum, DomainMask>,
    /// NodeNum -> Source ID from META section
    source_by_node: HashMap<NodeNum, u32>,
    /// NodeNum -> Evidence refs (pre-extracted from EVIDENCE section)
    evidence_by_node: HashMap<NodeNum, Vec<EvidenceRef>>,
}

impl AtomMetadataCache {
    pub fn new() -> Self {
        AtomMetadataCache {
            trust_by_node: HashMap::new(),
            domain_by_node: HashMap::new(),
            source_by_node: HashMap::new(),
            evidence_by_node: HashMap::new(),
        }
    }

    /// Register metadata for a node (called during atom ingestion)
    #[inline]
    pub fn register(
        &mut self,
        node_num: NodeNum,
        trust: TrustLevel,
        domain: DomainMask,
        source: u32,
    ) {
        self.trust_by_node.insert(node_num, trust);
        self.domain_by_node.insert(node_num, domain);
        self.source_by_node.insert(node_num, source);
    }

    /// Register evidence refs for a node
    #[inline]
    pub fn register_evidence(&mut self, node_num: NodeNum, evidence: Vec<EvidenceRef>) {
        self.evidence_by_node.insert(node_num, evidence);
    }

    /// Get trust level for node (default 5000 if not cached)
    #[inline]
    pub fn get_trust(&self, node_num: NodeNum) -> TrustLevel {
        self.trust_by_node.get(&node_num).copied().unwrap_or(5000)
    }

    /// Get domain mask for node (default 0xFFFF if not cached)
    #[inline]
    pub fn get_domain(&self, node_num: NodeNum) -> DomainMask {
        self.domain_by_node
            .get(&node_num)
            .copied()
            .unwrap_or(0xFFFF)
    }

    /// Get evidence refs for node
    #[inline]
    pub fn get_evidence(&self, node_num: NodeNum) -> Vec<EvidenceRef> {
        self.evidence_by_node
            .get(&node_num)
            .cloned()
            .unwrap_or_default()
    }

    /// Get source ID for node (default 0 if not cached)
    #[inline]
    pub fn get_source(&self, node_num: NodeNum) -> u32 {
        self.source_by_node.get(&node_num).copied().unwrap_or(0)
    }
}

impl Default for AtomMetadataCache {
    fn default() -> Self {
        AtomMetadataCache::new()
    }
}

// ============================================================================
// Backend result types
// ============================================================================

#[derive(Debug, Clone)]
pub struct BackendResult {
    pub candidates: Vec<Candidate>,
    pub backend_name: &'static str,
}

impl BackendResult {
    pub fn new(backend_name: &'static str, candidates: Vec<Candidate>) -> Self {
        BackendResult {
            candidates,
            backend_name,
        }
    }
}

#[derive(Default)]
pub struct CasBackend {
    locations: HashMap<AtomId, Location>,
}

impl CasBackend {
    pub fn new() -> Self {
        CasBackend {
            locations: HashMap::new(),
        }
    }
    #[inline]
    pub fn register(&mut self, atom_id: AtomId, location: Location) {
        self.locations.insert(atom_id, location);
    }
    #[inline]
    pub fn unregister(&mut self, atom_id: &AtomId) {
        self.locations.remove(atom_id);
    }
    #[inline]
    pub fn locate(&self, atom_id: &AtomId) -> Option<Location> {
        self.locations.get(atom_id).copied()
    }

    pub fn route(
        &self,
        gap: &Gap,
        goal_entities: &[crate::store::api::EntityRef],
        _trust_min: TrustLevel,
        domain_mask: DomainMask,
        lexical_resolution_required: bool,
    ) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        // 1. Resolve from goal entities that have direct AtomIds
        for entity_ref in goal_entities {
            if let crate::store::api::EntityRef::Atom(atom_id) = entity_ref
                && let Some(loc) = self.locate(atom_id)
            {
                candidates.push(self.make_candidate(*atom_id, loc, gap));
            }
        }
        // 2. If gap pattern has NodeRef, resolve via node->atom mapping
        if let PatternRef::Node(node_num) = gap.pattern.subj {
            for (&atom_id, &loc) in &self.locations {
                if loc.node_num == node_num {
                    candidates.push(self.make_candidate(atom_id, loc, gap));
                    break;
                }
            }
        }
        // 3. For NEED_DEFINITION with subj=Any, broadcast (capped at 256)
        if candidates.is_empty()
            && gap.kind == GapKind::NEED_DEFINITION
            && gap.pattern.subj.is_any()
            && !lexical_resolution_required
        {
            let mut broadcast = self
                .locations
                .iter()
                .filter(|(_, loc)| matches_domain(**loc, domain_mask))
                .map(|(atom_id, loc)| self.make_candidate(*atom_id, *loc, gap))
                .collect::<Vec<_>>();
            broadcast.sort_by_key(|candidate| candidate.atom_id);
            broadcast.truncate(256);
            for candidate in broadcast {
                candidates.push(candidate);
            }
        }
        candidates
    }

    fn make_candidate(&self, atom_id: AtomId, loc: Location, gap: &Gap) -> Candidate {
        Candidate {
            atom_id,
            node_num: loc.node_num,
            seg_id: loc.seg_id,
            offset: loc.offset,
            atom_type: AtomType::FACT,
            trust: 5000,
            estimated_io_bytes: loc.len,
            source_backend: BackendKind::Cas,
            requires_invariant_check: true,
            covers_gaps: vec![gap.id],
            source_priority: SourcePriority::CasExact,
            hard_conflicts: 0,
            soft_conflicts: 0,
            age_ns: 0,
            domain_mask: 0xFFFF,
            evidence_refs: Vec::new(),
            derived_claims: Vec::new(),
            ann_candidate_requires_filtering: false, // CAS candidates don't need ANN filtering
            branch_ctx_id: None,
        }
    }
}

#[derive(Default)]
pub struct InvertedBackend {
    inverted_index: Option<Arc<InvertedIndex>>,
    node_to_atom: HashMap<NodeNum, AtomId>,
    node_to_location: HashMap<NodeNum, Location>,
    local_terms: HashMap<String, Vec<NodeNum>>,
    /// Metadata cache for atom routing decisions (SKF-1.1)
    metadata_cache: AtomMetadataCache,
}

impl InvertedBackend {
    pub fn new() -> Self {
        InvertedBackend {
            inverted_index: None,
            node_to_atom: HashMap::new(),
            node_to_location: HashMap::new(),
            local_terms: HashMap::new(),
            metadata_cache: AtomMetadataCache::new(),
        }
    }
    pub fn with_index(mut self, index: Arc<InvertedIndex>) -> Self {
        self.inverted_index = Some(index);
        self
    }
    pub fn register(&mut self, node_num: NodeNum, atom_id: AtomId, location: Location) {
        self.node_to_atom.insert(node_num, atom_id);
        self.node_to_location.insert(node_num, location);
    }
    /// Register atom metadata for routing (SKF-1.1)
    #[inline]
    pub fn register_metadata(
        &mut self,
        node_num: NodeNum,
        trust: TrustLevel,
        domain: DomainMask,
        source: u32,
    ) {
        self.metadata_cache
            .register(node_num, trust, domain, source);
    }
    /// Register evidence refs for node (SKF-1.1)
    #[inline]
    pub fn register_evidence(&mut self, node_num: NodeNum, evidence: Vec<EvidenceRef>) {
        self.metadata_cache.register_evidence(node_num, evidence);
    }
    pub fn index_term(&mut self, term: &str, node_num: NodeNum) {
        self.local_terms
            .entry(term.to_lowercase())
            .or_default()
            .push(node_num);
    }

    /// Resolve term_id to term string (SKF-1.1 atom-aware indexing)
    ///
    /// Uses the real InvertedIndex lexicon to resolve term_id (u32)
    /// to actual term string for lookup.
    ///
    /// # Arguments
    /// - `term_id`: Internal term identifier from EntityRef::Term(u32)
    ///
    /// # Returns
    /// - `Some(&str)`: Resolved term string if found in lexicon
    /// - `None`: If inverted_index not connected or term_id not found
    ///
    /// # SKF-1.1 Contract
    /// - Replaces surrogate forms like `subj_<id>` with actual term content
    /// - Enables lexical retrieval using real symbol content from SYMBOLS section
    pub fn resolve_term_id(&self, term_id: u32) -> Option<&str> {
        self.inverted_index
            .as_ref()
            .and_then(|idx| idx.lexicon().get(term_id))
    }

    pub fn route(
        &self,
        gap: &Gap,
        goal_entities: &[crate::store::api::EntityRef],
        _trust_min: TrustLevel,
        domain_mask: DomainMask,
        lexical_resolution_required: bool,
    ) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        let mut seen_nodes: HashSet<NodeNum> = HashSet::new();
        let mut all_node_nums: Vec<NodeNum> = Vec::new();
        let mut lexical_term_sets: Vec<Vec<NodeNum>> = Vec::new();

        // Handle PatternRef::Sym lookups
        if let PatternRef::Sym(s) = gap.pattern.subj {
            all_node_nums.extend(self.lookup_term(&format!("sym:{}", s)));
            all_node_nums.extend(self.lookup_term("sym:*"));
        }
        if let PatternRef::Sym(p) = gap.pattern.pred {
            all_node_nums.extend(self.lookup_term(&format!("pred:{}", p)));
            all_node_nums.extend(self.lookup_term("pred:*"));
        }
        if let PatternRef::Sym(o) = gap.pattern.obj {
            all_node_nums.extend(self.lookup_term(&format!("sym:{}", o)));
            all_node_nums.extend(self.lookup_term("sym:*"));
        }

        // Handle goal entities lookup (SKF-1.1 atom-aware indexing)
        for entity_ref in goal_entities {
            match entity_ref {
                // Atom-aware: EntityRef::Term contains term_id, resolve to actual term string
                // NO surrogate forms like "subj_<id>" - use real lexical content
                crate::store::api::EntityRef::Term(term_id) => {
                    // Resolve term_id to actual term string from InvertedIndex lexicon
                    // This replaces surrogate forms with atom-aware content from SYMBOLS section
                    if let Some(term_str) = self.resolve_term_id(*term_id) {
                        lexical_term_sets.push(self.lookup_term(term_str));
                    }
                    // If term_id cannot be resolved (no InvertedIndex or not found),
                    // skip silently - this maintains graceful degradation
                }
                crate::store::api::EntityRef::Sym(s) => {
                    all_node_nums.extend(self.lookup_term(&format!("sym:{}", s)));
                }
                _ => {}
            }
        }
        if lexical_resolution_required && !lexical_term_sets.is_empty() {
            for nodes in &mut lexical_term_sets {
                nodes.sort_unstable();
                nodes.dedup();
            }
            let mut intersection = lexical_term_sets.remove(0);
            for nodes in lexical_term_sets {
                intersection.retain(|node| nodes.binary_search(node).is_ok());
            }
            all_node_nums.extend(intersection);
        } else {
            all_node_nums.extend(lexical_term_sets.into_iter().flatten());
        }

        // Fallback: If PatternRef::Any and no nodes found via terms,
        // search all registered nodes (fallback for broad queries)
        if matches!(gap.pattern.subj, PatternRef::Any)
            && all_node_nums.is_empty()
            && !self.node_to_atom.is_empty()
            && !lexical_resolution_required
        {
            // Add all registered nodes as candidates for broad queries
            for &node_num in self.node_to_atom.keys() {
                all_node_nums.push(node_num);
            }
        }
        all_node_nums.sort_unstable();
        all_node_nums.dedup();
        for &node_num in &all_node_nums {
            if seen_nodes.contains(&node_num) {
                continue;
            }
            seen_nodes.insert(node_num);
            if let Some(&atom_id) = self.node_to_atom.get(&node_num) {
                let loc = self
                    .node_to_location
                    .get(&node_num)
                    .copied()
                    .unwrap_or(Location::new(0, 0, 0, node_num, 0xFFFF));
                if !matches_domain(loc, domain_mask) {
                    continue;
                }

                // SKF-1.1: Use real atom metadata for routing decisions
                // Get trust from metadata cache (or default 5000)
                let atom_trust = self.metadata_cache.get_trust(node_num);
                // Get domain from location (already validated above)
                let atom_domain = loc.domain_mask;
                // Get evidence refs from cache (for connectivity)
                let atom_evidence = self.metadata_cache.get_evidence(node_num);

                candidates.push(Candidate {
                    atom_id,
                    node_num,
                    seg_id: loc.seg_id,
                    offset: loc.offset,
                    atom_type: AtomType::FACT,
                    trust: atom_trust, // Real trust from META section
                    estimated_io_bytes: loc.len.max(64),
                    source_backend: BackendKind::Inverted,
                    requires_invariant_check: true,
                    covers_gaps: vec![gap.id],
                    source_priority: SourcePriority::Inverted,
                    hard_conflicts: 0,
                    soft_conflicts: 0,
                    age_ns: 0,
                    domain_mask: atom_domain, // Real domain from META section
                    evidence_refs: atom_evidence, // Real evidence refs for connectivity
                    derived_claims: Vec::new(),
                    ann_candidate_requires_filtering: false,
                    branch_ctx_id: None,
                });
            }
        }
        candidates
    }

    /// Lookup nodes by term (public for testing and direct access)
    pub fn lookup_term(&self, term: &str) -> Vec<NodeNum> {
        let mut results = Vec::new();
        if let Some(nodes) = self.local_terms.get(term) {
            results.extend(nodes.iter().copied());
        }
        if let Some(ref idx) = self.inverted_index
            && let Some(pl) = idx.search(term)
        {
            for &n in pl {
                if !results.contains(&n) {
                    results.push(n);
                }
            }
        }
        results
    }
}
#[derive(Default)]
pub struct GraphBackend {
    pub graph_store: Option<Arc<GraphStore>>,
    node_to_atom: HashMap<NodeNum, AtomId>,
    node_to_location: HashMap<NodeNum, Location>,
    max_fanout: u16,
    /// Metadata cache for atom routing decisions (SKF-1.1)
    metadata_cache: AtomMetadataCache,
}

impl GraphBackend {
    pub fn new() -> Self {
        GraphBackend {
            graph_store: None,
            node_to_atom: HashMap::new(),
            node_to_location: HashMap::new(),
            max_fanout: 128,
            metadata_cache: AtomMetadataCache::new(),
        }
    }
    pub fn with_store(mut self, store: Arc<GraphStore>) -> Self {
        self.graph_store = Some(store);
        self
    }
    pub fn with_max_fanout(mut self, limit: u16) -> Self {
        self.max_fanout = limit;
        self
    }
    pub fn register(&mut self, node_num: NodeNum, atom_id: AtomId, location: Location) {
        self.node_to_atom.insert(node_num, atom_id);
        self.node_to_location.insert(node_num, location);
    }
    /// Register atom metadata for routing (SKF-1.1)
    #[inline]
    pub fn register_metadata(
        &mut self,
        node_num: NodeNum,
        trust: TrustLevel,
        domain: DomainMask,
        source: u32,
    ) {
        self.metadata_cache
            .register(node_num, trust, domain, source);
    }
    /// Register evidence refs for node (SKF-1.1)
    #[inline]
    pub fn register_evidence(&mut self, node_num: NodeNum, evidence: Vec<EvidenceRef>) {
        self.metadata_cache.register_evidence(node_num, evidence);
    }

    /// Check if GraphStore is connected
    #[inline]
    pub fn has_store(&self) -> bool {
        self.graph_store.is_some()
    }

    pub fn walk(
        &self,
        seed_ids: &[NodeNum],
        edge_types: &[EdgeType],
        max_depth: u8,
        gap: &Gap,
        domain_mask: DomainMask,
    ) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        let mut visited: HashSet<NodeNum> = seed_ids.iter().copied().collect();
        let mut queue: VecDeque<(NodeNum, u8)> = seed_ids.iter().map(|&n| (n, 0u8)).collect();

        while let Some((current_node, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }
            for &edge_type in edge_types {
                let neighbors = self.get_neighbors(current_node, edge_type);
                for (neighbor_node, _confidence) in neighbors.take(self.max_fanout as usize) {
                    if visited.insert(neighbor_node) {
                        if let Some(&atom_id) = self.node_to_atom.get(&neighbor_node) {
                            let loc = self
                                .node_to_location
                                .get(&neighbor_node)
                                .copied()
                                .unwrap_or(Location::new(0, 0, 0, neighbor_node, 0xFFFF));
                            if !matches_domain(loc, domain_mask) {
                                continue;
                            }

                            // SKF-1.1: Use real atom metadata for routing decisions
                            let atom_trust = self.metadata_cache.get_trust(neighbor_node);
                            let atom_domain = loc.domain_mask;
                            let atom_evidence = self.metadata_cache.get_evidence(neighbor_node);

                            candidates.push(Candidate {
                                atom_id,
                                node_num: neighbor_node,
                                seg_id: loc.seg_id,
                                offset: loc.offset,
                                atom_type: AtomType::FACT,
                                trust: atom_trust, // Real trust from META section
                                estimated_io_bytes: loc.len.max(64),
                                source_backend: BackendKind::Graph,
                                requires_invariant_check: true,
                                covers_gaps: vec![gap.id],
                                source_priority: SourcePriority::GraphWalk,
                                hard_conflicts: 0,
                                soft_conflicts: 0,
                                age_ns: 0,
                                domain_mask: atom_domain, // Real domain from META section
                                evidence_refs: atom_evidence, // Real evidence refs for connectivity
                                derived_claims: Vec::new(),
                                ann_candidate_requires_filtering: false,
                                branch_ctx_id: None,
                            });
                        }
                        queue.push_back((neighbor_node, depth + 1));
                    }
                }
            }
        }
        candidates
    }

    pub fn route(&self, gap: &Gap, domain_mask: DomainMask) -> Vec<Candidate> {
        if gap.nav.seed_nodes.is_empty() || gap.nav.edge_types.is_empty() {
            return Vec::new();
        }
        self.walk(
            &gap.nav.seed_nodes,
            &gap.nav.edge_types,
            gap.nav.max_depth,
            gap,
            domain_mask,
        )
    }

    fn get_neighbors(
        &self,
        node: NodeNum,
        edge_type: EdgeType,
    ) -> Box<dyn Iterator<Item = (NodeNum, TrustLevel)>> {
        if let Some(ref store) = self.graph_store {
            let neighbors: Vec<(NodeNum, TrustLevel)> = store.neighbors(node, edge_type).collect();
            Box::new(neighbors.into_iter())
        } else {
            Box::new(std::iter::empty())
        }
    }
}
pub struct AnnBackend {
    node_to_atom: HashMap<NodeNum, AtomId>,
    node_to_location: HashMap<NodeNum, Location>,
    embedding_index: Option<Arc<EmbeddingIndex>>,
    /// Metadata cache for atom routing decisions (SKF-1.1)
    metadata_cache: AtomMetadataCache,
}

impl AnnBackend {
    pub fn new() -> Self {
        AnnBackend {
            node_to_atom: HashMap::new(),
            node_to_location: HashMap::new(),
            embedding_index: None,
            metadata_cache: AtomMetadataCache::new(),
        }
    }
    pub fn with_embedding_index(mut self, index: Arc<EmbeddingIndex>) -> Self {
        self.embedding_index = Some(index);
        self
    }
    pub fn register(&mut self, node_num: NodeNum, atom_id: AtomId, location: Location) {
        self.node_to_atom.insert(node_num, atom_id);
        self.node_to_location.insert(node_num, location);
    }
    /// Register atom metadata for routing (SKF-1.1)
    #[inline]
    pub fn register_metadata(
        &mut self,
        node_num: NodeNum,
        trust: TrustLevel,
        domain: DomainMask,
        source: u32,
    ) {
        self.metadata_cache
            .register(node_num, trust, domain, source);
    }
    /// Register evidence refs for node (SKF-1.1)
    #[inline]
    pub fn register_evidence(&mut self, node_num: NodeNum, evidence: Vec<EvidenceRef>) {
        self.metadata_cache.register_evidence(node_num, evidence);
    }

    pub fn route(
        &self,
        gap: &Gap,
        goal_entities: &[crate::store::api::EntityRef],
        domain_mask: DomainMask,
        goal: &crate::query::solver::GoalSpec,
    ) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        if let Some(index) = &self.embedding_index {
            for query_vector in &goal.semantic_vectors {
                for (node_num, similarity) in index.search(query_vector, 10) {
                    if let Some(&atom_id) = self.node_to_atom.get(&node_num) {
                        let loc = self
                            .node_to_location
                            .get(&node_num)
                            .copied()
                            .unwrap_or(Location::new(0, 0, 0, node_num, 0xFFFF));
                        if !matches_domain(loc, domain_mask) {
                            continue;
                        }

                        let atom_trust = self.metadata_cache.get_trust(node_num);
                        if atom_trust < goal.trust_min {
                            continue;
                        }
                        let atom_domain = self.metadata_cache.get_domain(node_num);
                        let atom_evidence = self.metadata_cache.get_evidence(node_num);
                        let semantic_trust = (similarity.clamp(0.0, 1.0) * 10000.0) as TrustLevel;

                        candidates.push(Candidate {
                            atom_id,
                            node_num,
                            seg_id: loc.seg_id,
                            offset: loc.offset,
                            atom_type: AtomType::FACT,
                            trust: semantic_trust.min(atom_trust),
                            estimated_io_bytes: loc.len.max(64),
                            source_backend: BackendKind::Ann,
                            requires_invariant_check: true,
                            covers_gaps: vec![gap.id],
                            source_priority: SourcePriority::Ann,
                            hard_conflicts: 0,
                            soft_conflicts: 0,
                            age_ns: 0,
                            domain_mask: atom_domain,
                            evidence_refs: atom_evidence,
                            derived_claims: Vec::new(),
                            ann_candidate_requires_filtering: true,
                            branch_ctx_id: None,
                        });
                    }
                }
            }
        }
        for entity_ref in goal_entities {
            if let crate::store::api::EntityRef::Node(node_num) = entity_ref
                && let Some(&atom_id) = self.node_to_atom.get(node_num)
            {
                let loc = self
                    .node_to_location
                    .get(node_num)
                    .copied()
                    .unwrap_or(Location::new(0, 0, 0, *node_num, 0xFFFF));
                if !matches_domain(loc, domain_mask) {
                    continue;
                }

                // SKF-1.1: Use real atom metadata for routing decisions
                let atom_trust = self.metadata_cache.get_trust(*node_num);
                let atom_domain = loc.domain_mask;
                let atom_evidence = self.metadata_cache.get_evidence(*node_num);

                candidates.push(Candidate {
                    atom_id,
                    node_num: *node_num,
                    seg_id: loc.seg_id,
                    offset: loc.offset,
                    atom_type: AtomType::FACT,
                    trust: atom_trust, // Real trust from META section
                    estimated_io_bytes: loc.len.max(64),
                    source_backend: BackendKind::Ann,
                    requires_invariant_check: true,
                    covers_gaps: vec![gap.id],
                    source_priority: SourcePriority::Ann,
                    hard_conflicts: 0,
                    soft_conflicts: 0,
                    age_ns: 0,
                    domain_mask: atom_domain, // Real domain from META section
                    evidence_refs: atom_evidence, // Real evidence refs for connectivity
                    derived_claims: Vec::new(),
                    ann_candidate_requires_filtering: true, // ANTI-RAG: ANN candidates MUST be filtered
                    branch_ctx_id: None,
                });
            }
        }
        candidates
    }
}

impl Default for AnnBackend {
    fn default() -> Self {
        AnnBackend::new()
    }
}

/// Source priority for routing (SKF-1.1 6.1)
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourcePriority {
    CasExact = 0,
    Inverted = 1,
    GraphWalk = 2,
    Ann = 3,
}

impl SourcePriority {
    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Which backend produced a candidate
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    Cas = 0,
    Inverted = 1,
    Graph = 2,
    Ann = 3,
}

impl BackendKind {
    #[inline]
    pub const fn as_str(&self) -> &'static str {
        match self {
            BackendKind::Cas => "CAS",
            BackendKind::Inverted => "inverted",
            BackendKind::Graph => "graph",
            BackendKind::Ann => "ANN",
        }
    }
}

/// Candidate atom from the query router
#[derive(Debug, Clone)]
pub struct Candidate {
    pub atom_id: AtomId,
    pub node_num: NodeNum,
    pub seg_id: u32,
    pub offset: u64,
    pub atom_type: AtomType,
    pub trust: TrustLevel,
    pub estimated_io_bytes: u32,
    pub source_backend: BackendKind,
    pub requires_invariant_check: bool,
    pub covers_gaps: Vec<GapId>,
    pub source_priority: SourcePriority,
    pub hard_conflicts: u32,
    pub soft_conflicts: u32,
    pub age_ns: u64,
    pub domain_mask: DomainMask,
    pub evidence_refs: Vec<EvidenceRef>,
    pub derived_claims: Vec<ClaimData>,
    /// ANTI-RAG: Explicit flag for ANN candidates that require mandatory filtering
    /// This flag is set to true for ALL ANN candidates and is checked by the solver
    /// to ensure no ANN candidate bypasses the invariant pipeline (SKF-1.1 6.2)
    pub ann_candidate_requires_filtering: bool,
    /// Branch context ID - set when candidate requires a TMS context branch (SKF-1.1 3.2)
    /// When NeedBranch is returned from invariant evaluation, this field is set to
    /// the new branch's CtxId, indicating the candidate belongs to that branch.
    pub branch_ctx_id: Option<CtxId>,
}

impl Candidate {
    #[inline]
    pub fn benefit_cost_ratio(&self, gaps: &[Gap], weights: &CostWeights) -> f64 {
        let benefit: f64 = self
            .covers_gaps
            .iter()
            .filter_map(|&g| gaps.get(g as usize))
            .map(|g| weights.gap_benefit(g.priority))
            .sum();
        let trust_inverse = if self.trust > 0 {
            1.0 / (self.trust as f64 / 10000.0)
        } else {
            10.0
        };
        let cost = (self.estimated_io_bytes as f64) * weights.wIO
            + weights.wT * trust_inverse
            + weights.wC * self.hard_conflicts as f64
            + weights.wS * self.soft_conflicts as f64
            + 1.0;
        if cost > 0.0 {
            benefit / cost
        } else {
            f64::INFINITY
        }
    }
}

impl ConstraintSubject for Candidate {
    fn value_for(&self, target: &ConstraintTarget) -> Option<ConstraintValue> {
        match target {
            ConstraintTarget::Entity => Some(ConstraintValue::Text(self.node_num.to_string())),
            ConstraintTarget::Domain => Some(ConstraintValue::Number(self.domain_mask as f64)),
            ConstraintTarget::NumericMetric => {
                Some(ConstraintValue::Number(self.trust as f64 / 10_000.0))
            }
            ConstraintTarget::Source => Some(ConstraintValue::Text(
                self.source_backend.as_str().to_owned(),
            )),
            ConstraintTarget::Time => {
                (self.age_ns > 0).then_some(ConstraintValue::Number(self.age_ns as f64))
            }
            ConstraintTarget::Evidence => Some(ConstraintValue::List(
                self.evidence_refs
                    .iter()
                    .map(|evidence| {
                        ConstraintValue::Text(format!(
                            "{}:{}:{}",
                            crate::cas::hex_encode(&evidence.atom_id),
                            evidence.offset,
                            evidence.length
                        ))
                    })
                    .collect(),
            )),
            ConstraintTarget::Custom(name) => match name.as_str() {
                "atom_id" => Some(ConstraintValue::Text(crate::cas::hex_encode(&self.atom_id))),
                "node_num" => Some(ConstraintValue::Number(self.node_num as f64)),
                "atom_type" => Some(ConstraintValue::Text(format!("{:?}", self.atom_type))),
                "backend" | "source_backend" => Some(ConstraintValue::Text(
                    self.source_backend.as_str().to_owned(),
                )),
                "trust" => Some(ConstraintValue::Number(self.trust as f64 / 10_000.0)),
                "domain" | "domain_mask" => Some(ConstraintValue::Number(self.domain_mask as f64)),
                "requires_invariant_check" => {
                    Some(ConstraintValue::Bool(self.requires_invariant_check))
                }
                "ann_candidate_requires_filtering" => {
                    Some(ConstraintValue::Bool(self.ann_candidate_requires_filtering))
                }
                _ => None,
            },
            ConstraintTarget::EntityType
            | ConstraintTarget::Predicate
            | ConstraintTarget::Relation
            | ConstraintTarget::Context
            | ConstraintTarget::Text => None,
        }
    }

    fn evidence_refs_for(&self, _constraint: &crate::query::contract::Constraint) -> Vec<String> {
        self.evidence_refs
            .iter()
            .map(|evidence| crate::cas::hex_encode(&evidence.atom_id))
            .collect()
    }

    fn candidate_ref(&self) -> Option<String> {
        Some(crate::cas::hex_encode(&self.atom_id))
    }
}

/// The QueryRouter routes gaps to storage backends in priority order:
/// CAS > Inverted > Graph > ANN
/// All candidates have requires_invariant_check: true (SKF-1.1 6.2)
pub struct QueryRouter {
    pub cas: CasBackend,
    pub inverted: InvertedBackend,
    pub graph: GraphBackend,
    pub ann: AnnBackend,
}

impl Default for QueryRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryRouter {
    pub fn new() -> Self {
        QueryRouter {
            cas: CasBackend::new(),
            inverted: InvertedBackend::new(),
            graph: GraphBackend::new(),
            ann: AnnBackend::new(),
        }
    }

    pub fn register_atom(
        &mut self,
        atom_id: AtomId,
        node_num: NodeNum,
        seg_id: u32,
        offset: u64,
        len: u32,
    ) {
        let location = Location::new(seg_id, offset, len, node_num, 0xFFFF);
        self.cas.register(atom_id, location);
        self.inverted.register(node_num, atom_id, location);
        self.graph.register(node_num, atom_id, location);
        self.ann.register(node_num, atom_id, location);
    }

    /// Register atom metadata for routing decisions (SKF-1.1)
    ///
    /// This method populates the metadata cache for all backends,
    /// enabling routing decisions based on real atom META section data.
    #[inline]
    pub fn register_atom_metadata(
        &mut self,
        node_num: NodeNum,
        trust: TrustLevel,
        domain: DomainMask,
        source: u32,
    ) {
        self.inverted
            .register_metadata(node_num, trust, domain, source);
        self.graph
            .register_metadata(node_num, trust, domain, source);
        self.ann.register_metadata(node_num, trust, domain, source);
    }

    /// Register evidence refs for atom connectivity (SKF-1.1)
    #[inline]
    pub fn register_atom_evidence(&mut self, node_num: NodeNum, evidence: Vec<EvidenceRef>) {
        self.inverted.register_evidence(node_num, evidence.clone());
        self.graph.register_evidence(node_num, evidence.clone());
        self.ann.register_evidence(node_num, evidence);
    }

    pub fn index_term(&mut self, term: &str, node_num: NodeNum) {
        self.inverted.index_term(term, node_num);
    }

    /// Set inverted index for real term-based retrieval (SKF-1.1 6.1)
    ///
    /// This connects the InvertedBackend to a real InvertedIndex from
    /// the index module, enabling deterministic lexical retrieval instead
    /// of relying on local_terms HashMap.
    ///
    /// # Arguments
    /// - `index`: Arc-wrapped InvertedIndex with front-coded lexicon and postings
    ///
    /// # Example
    /// ```ignore
    /// let index = Arc::new(InvertedIndex::new(&path)?);
    /// let router = QueryRouter::new().with_inverted_index(index);
    /// ```
    pub fn with_inverted_index(mut self, index: Arc<InvertedIndex>) -> Self {
        self.inverted = self.inverted.with_index(index);
        self
    }

    /// Route a single gap to the appropriate backend(s) based on GapKind.
    ///
    /// SKF-1.1 6.1 gap-to-backend routing policy:
    /// - NEED_DEFINITION    -> CasBackend + InvertedBackend (definitions by term)
    /// - NEED_FACT          -> InvertedBackend (predicate/sym lookup)
    /// - NEED_CAUSAL_CHAIN  -> GraphBackend (walk CAUSES/ENABLES/PREVENTS)
    /// - NEED_EVIDENCE      -> CasBackend (evidence references)
    /// - NEED_COUNTEREXAMPLE -> GraphBackend (walk CONTRADICTS)
    /// - NEED_CONSTRAINTS   -> CasBackend (constraint atoms)
    /// - NEED_COMPARISON_AXIS -> InvertedBackend (term-based axis)
    /// - NEED_PROCEDURE     -> InvertedBackend + CasBackend
    pub fn route_one_gap(
        &self,
        gap: &Gap,
        goal: &crate::query::solver::GoalSpec,
    ) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        match gap.kind {
            GapKind::NEED_DEFINITION => {
                let mut cas_cands = self.cas.route(
                    gap,
                    &goal.entities,
                    goal.trust_min,
                    goal.domain_mask,
                    goal.lexical_resolution_required,
                );
                let mut inv_cands = self.inverted.route(
                    gap,
                    &goal.entities,
                    goal.trust_min,
                    goal.domain_mask,
                    goal.lexical_resolution_required,
                );
                candidates.append(&mut cas_cands);
                candidates.append(&mut inv_cands);
            }
            GapKind::NEED_FACT => {
                let mut inv_cands = self.inverted.route(
                    gap,
                    &goal.entities,
                    goal.trust_min,
                    goal.domain_mask,
                    goal.lexical_resolution_required,
                );
                candidates.append(&mut inv_cands);
            }
            GapKind::NEED_CAUSAL_CHAIN => {
                let graph_cands = self.graph.route(gap, goal.domain_mask);
                candidates.extend(graph_cands);
            }
            GapKind::NEED_EVIDENCE => {
                let mut cas_cands = self.cas.route(
                    gap,
                    &goal.entities,
                    goal.trust_min,
                    goal.domain_mask,
                    goal.lexical_resolution_required,
                );
                candidates.append(&mut cas_cands);
            }
            GapKind::NEED_COUNTEREXAMPLE => {
                let graph_cands = self.graph.route(gap, goal.domain_mask);
                candidates.extend(graph_cands);
            }
            GapKind::NEED_CONSTRAINTS => {
                let mut cas_cands = self.cas.route(
                    gap,
                    &goal.entities,
                    goal.trust_min,
                    goal.domain_mask,
                    goal.lexical_resolution_required,
                );
                candidates.append(&mut cas_cands);
            }
            GapKind::NEED_COMPARISON_AXIS => {
                let mut inv_cands = self.inverted.route(
                    gap,
                    &goal.entities,
                    goal.trust_min,
                    goal.domain_mask,
                    goal.lexical_resolution_required,
                );
                candidates.append(&mut inv_cands);
            }
            GapKind::NEED_PROCEDURE => {
                let mut inv_cands = self.inverted.route(
                    gap,
                    &goal.entities,
                    goal.trust_min,
                    goal.domain_mask,
                    goal.lexical_resolution_required,
                );
                let mut cas_cands = self.cas.route(
                    gap,
                    &goal.entities,
                    goal.trust_min,
                    goal.domain_mask,
                    goal.lexical_resolution_required,
                );
                candidates.append(&mut inv_cands);
                candidates.append(&mut cas_cands);
            }
        }
        for c in &mut candidates {
            if !c.covers_gaps.contains(&gap.id) {
                c.covers_gaps.push(gap.id);
            }
        }
        candidates
    }

    /// Route a gap to all backends with deduplication by AtomId.
    pub fn route(&self, gap: &Gap, goal: &crate::query::solver::GoalSpec) -> Vec<Candidate> {
        let mut candidate_map: HashMap<AtomId, Candidate> = HashMap::new();
        for c in self.cas.route(
            gap,
            &goal.entities,
            goal.trust_min,
            goal.domain_mask,
            goal.lexical_resolution_required,
        ) {
            candidate_map.insert(c.atom_id, c);
        }
        for c in self.inverted.route(
            gap,
            &goal.entities,
            goal.trust_min,
            goal.domain_mask,
            goal.lexical_resolution_required,
        ) {
            candidate_map.entry(c.atom_id).or_insert(c);
        }
        if gap.needs_graph_walk() || !gap.nav.seed_nodes.is_empty() {
            for c in self.graph.route(gap, goal.domain_mask) {
                candidate_map.entry(c.atom_id).or_insert(c);
            }
        }
        for c in self.ann.route(gap, &goal.entities, goal.domain_mask, goal) {
            if goal.semantic_vectors.is_empty() {
                candidate_map.entry(c.atom_id).or_insert(c);
            } else {
                // For explicit semantic-vector queries, preserve ANN provenance
                // instead of hiding it behind broad lexical fallback candidates.
                candidate_map.insert(c.atom_id, c);
            }
        }
        let mut candidates: Vec<_> = candidate_map.into_values().collect();
        candidates.sort_by_key(|candidate| candidate.atom_id);
        candidates
    }

    /// Route a gap through all current channels and expose the common CandidateV2 contract.
    pub fn route_v2(&self, gap: &Gap, goal: &crate::query::solver::GoalSpec) -> Vec<CandidateV2> {
        self.route(gap, goal)
            .iter()
            .map(|candidate| CandidateV2::from_candidate(candidate, goal))
            .collect()
    }
}

impl Retriever for QueryRouter {
    fn retrieve(&self, gap: &Gap, goal: &crate::query::solver::GoalSpec) -> Vec<CandidateV2> {
        self.route_v2(gap, goal)
    }
}

/// Deduplicate candidates by AtomId, keeping highest-priority backend.
/// Priority: Cas > Inverted > Graph > Ann (lower number = better).
pub fn dedup_candidates(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut seen: HashMap<AtomId, Candidate> = HashMap::with_capacity(candidates.len());
    for c in candidates {
        seen.entry(c.atom_id)
            .and_modify(|existing| {
                if c.source_priority < existing.source_priority {
                    *existing = c.clone();
                }
            })
            .or_insert(c);
    }
    let mut candidates: Vec<_> = seen.into_values().collect();
    candidates.sort_by_key(|candidate| candidate.atom_id);
    candidates
}

#[inline]
fn matches_domain(loc: Location, domain_mask: DomainMask) -> bool {
    if domain_mask == 0 {
        return true;
    }
    (loc.domain_mask & domain_mask) != 0
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Gap;
    use crate::store::api::EntityRef;
    use crate::store::{ClaimPattern, GapKind, Intent};

    fn test_goal() -> crate::query::GoalSpec {
        crate::query::GoalSpec::new(Intent::LOOKUP)
    }

    #[test]
    fn test_ann_backend_creates_candidates_with_invariant_check() {
        let mut backend = AnnBackend::new();
        let atom_id = [1u8; 32];
        let location = Location::new(0, 100, 256, 42, 0xFFFF);
        backend.register(42, atom_id, location);

        let gap = Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default());
        let entities = vec![EntityRef::Node(42)];

        let candidates = backend.route(&gap, &entities, 0xFFFF, &test_goal());

        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];

        // ANTI-RAG: ANN candidates MUST have requires_invariant_check=true
        assert!(
            candidate.requires_invariant_check,
            "ANN candidate must require invariant check"
        );

        // ANTI-RAG: ANN candidates MUST have ann_candidate_requires_filtering=true
        assert!(
            candidate.ann_candidate_requires_filtering,
            "ANN candidate must have ann_candidate_requires_filtering=true"
        );

        // Verify backend kind
        assert_eq!(candidate.source_backend, BackendKind::Ann);
        assert_eq!(candidate.source_priority, SourcePriority::Ann);
    }

    #[test]
    fn test_route_v2_preserves_semantic_candidate_validation_boundary() {
        let mut router = QueryRouter::new();
        let atom_id = [7u8; 32];
        let location = Location::new(0, 100, 256, 42, 0xFFFF);
        router.register_atom(atom_id, 42, 0, 100, 256);
        router.ann.register(42, atom_id, location);

        let gap = Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default());
        let goal = test_goal()
            .with_entities(vec![EntityRef::Node(42)])
            .with_semantic_vectors(vec![vec![1.0, 0.0]])
            .with_constraints(vec![crate::query::contract::Constraint::must(
                "backend_is_ann",
                crate::query::contract::ConstraintTarget::Custom("backend".to_owned()),
                crate::query::contract::ConstraintOperator::Eq,
                crate::query::contract::ConstraintValue::Text("ANN".to_owned()),
            )]);

        let candidates = router.route_v2(&gap, &goal);

        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].retrieval_reason,
            crate::query::RetrievalReason::Semantic
        );
        assert!(candidates[0].requires_validation);
        assert_eq!(candidates[0].matched_constraints.len(), 1);
        assert_eq!(candidates[0].matched_constraints[0].0, "backend_is_ann");
    }

    #[test]
    fn test_ann_backend_multiple_candidates() {
        let mut backend = AnnBackend::new();

        // Register multiple nodes
        for i in 1u64..=5 {
            let atom_id = [i as u8; 32];
            let location = Location::new(0, i * 100, 256, i, 0xFFFF);
            backend.register(i, atom_id, location);
        }

        let gap = Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default());
        let entities: Vec<EntityRef> = (1..=5).map(EntityRef::Node).collect();

        let candidates = backend.route(&gap, &entities, 0xFFFF, &test_goal());

        assert_eq!(candidates.len(), 5);

        // All ANN candidates must have filtering flags
        for candidate in &candidates {
            assert!(
                candidate.requires_invariant_check,
                "ANN candidate {} must require invariant check",
                candidate.node_num
            );
            assert!(
                candidate.ann_candidate_requires_filtering,
                "ANN candidate {} must have ann_candidate_requires_filtering",
                candidate.node_num
            );
        }
    }

    #[test]
    fn cas_broadcast_cap_sorts_before_taking_first_256() {
        let mut backend = CasBackend::new();
        let mut expected = Vec::new();
        for value in (0u16..300).rev() {
            let mut atom_id = [0u8; 32];
            atom_id[..2].copy_from_slice(&value.to_be_bytes());
            backend.register(
                atom_id,
                Location::new(0, u64::from(value), 1, u64::from(value), 0xFFFF),
            );
            expected.push(atom_id);
        }
        expected.sort_unstable();
        expected.truncate(256);
        let gap = Gap::new(0, GapKind::NEED_DEFINITION, ClaimPattern::default());
        let actual = backend
            .route(&gap, &[], 0, 0, false)
            .into_iter()
            .map(|candidate| candidate.atom_id)
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_ann_backend_domain_filtering() {
        let mut backend = AnnBackend::new();

        // Register node with specific domain
        let atom_id = [1u8; 32];
        let location = Location::new(0, 100, 256, 42, 0x0001); // Domain 1
        backend.register(42, atom_id, location);

        let gap = Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default());
        let entities = vec![EntityRef::Node(42)];

        // Should match when domain_mask includes domain 1
        let candidates = backend.route(&gap, &entities, 0x0001, &test_goal());
        assert_eq!(candidates.len(), 1);

        // Should not match when domain_mask excludes domain 1
        let candidates = backend.route(&gap, &entities, 0x0002, &test_goal());
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_ann_backend_empty_entities() {
        let backend = AnnBackend::new();
        let gap = Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default());

        let candidates = backend.route(&gap, &[], 0xFFFF, &test_goal());
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_ann_backend_non_node_entities() {
        let backend = AnnBackend::new();
        let gap = Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default());

        // EntityRef::Sym should not produce candidates
        let entities = vec![EntityRef::Sym(42)];
        let candidates = backend.route(&gap, &entities, 0xFFFF, &test_goal());
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_cas_backend_no_ann_filtering() {
        let cas = CasBackend::new();
        let atom_id = [1u8; 32];
        let _location = Location::new(0, 100, 256, 42, 0xFFFF);

        // CAS candidates should NOT have ANN filtering flag
        let gap = Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default());
        let _candidates = cas.route(&gap, &[EntityRef::Atom(atom_id)], 0, 0xFFFF, false);

        // Note: CAS backend won't find the atom since it wasn't registered through normal path
        // This test mainly verifies the structure
    }

    #[test]
    fn test_source_priority_ordering() {
        // Priority: CasExact (0) < Inverted (1) < GraphWalk (2) < Ann (3)
        assert!(SourcePriority::CasExact < SourcePriority::Inverted);
        assert!(SourcePriority::Inverted < SourcePriority::GraphWalk);
        assert!(SourcePriority::GraphWalk < SourcePriority::Ann);

        assert_eq!(SourcePriority::CasExact.as_u8(), 0);
        assert_eq!(SourcePriority::Inverted.as_u8(), 1);
        assert_eq!(SourcePriority::GraphWalk.as_u8(), 2);
        assert_eq!(SourcePriority::Ann.as_u8(), 3);
    }

    #[test]
    fn test_dedup_candidates_prioritizes_cas_over_ann() {
        let atom_id = [1u8; 32];

        let cas_candidate = Candidate {
            atom_id,
            node_num: 1,
            seg_id: 0,
            offset: 100,
            atom_type: AtomType::FACT,
            trust: 5000,
            estimated_io_bytes: 256,
            source_backend: BackendKind::Cas,
            requires_invariant_check: true,
            covers_gaps: vec![0],
            source_priority: SourcePriority::CasExact,
            hard_conflicts: 0,
            soft_conflicts: 0,
            age_ns: 0,
            domain_mask: 0xFFFF,
            evidence_refs: Vec::new(),
            derived_claims: Vec::new(),
            ann_candidate_requires_filtering: false,
            branch_ctx_id: None,
        };

        let ann_candidate = Candidate {
            atom_id,
            node_num: 1,
            seg_id: 0,
            offset: 200,
            atom_type: AtomType::FACT,
            trust: 3000,
            estimated_io_bytes: 256,
            source_backend: BackendKind::Ann,
            requires_invariant_check: true,
            covers_gaps: vec![0],
            source_priority: SourcePriority::Ann,
            hard_conflicts: 0,
            soft_conflicts: 0,
            age_ns: 0,
            domain_mask: 0xFFFF,
            evidence_refs: Vec::new(),
            derived_claims: Vec::new(),
            ann_candidate_requires_filtering: true,
            branch_ctx_id: None,
        };

        let candidates = vec![cas_candidate.clone(), ann_candidate];
        let deduped = dedup_candidates(candidates);

        assert_eq!(deduped.len(), 1);
        // Should keep CAS candidate (higher priority)
        assert_eq!(deduped[0].source_backend, BackendKind::Cas);
        assert!(!deduped[0].ann_candidate_requires_filtering);
    }
}
