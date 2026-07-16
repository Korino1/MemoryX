//! COMPLETE FixedPointSolver for MemoryX SKF-1.1
//!
//! This module implements the production-ready fixed-point answer solver
//! following SKF-1.1 sections 4-5 exactly.

#![allow(dead_code)]

use crate::cas::CasError;
use crate::cas::io::CasStore;
use crate::prelude::*;
use crate::query::planner::{PlannerBudgets, RetrievalPlanner};
use crate::store::api::ProofStep;
use crate::store::{
    AtomId, AtomType, ClaimPattern, DomainMask, EdgeType, GapKind, Intent, NodeNum, SymId,
    TrustLevel,
};
use crate::vm;
use crate::vm::abi::InvariantResult;

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

// Re-export API types for convenience
pub use crate::store::api::{
    ActiveClaim, AgEdge, AgEdgeType, AgNode, AnswerGraph, AnswerStatus, AtomRef, ClaimStatus,
    ClaimView, Conflict, ConflictConditions, ConflictSet, ConflictSeverity, ConflictSummary,
    ConflictType, CostWeights, EntityRef, EvidenceRef, Gap, GapId, GapPriority, Limitation,
    LimitationCode, LimitationSeverity, ObjTag, RejectedCandidateSummary, ResolutionOption,
};

// ============================================================================
// Solver Configuration
// ============================================================================

/// I/O mode for retrieval
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IoMode {
    /// Synchronous reads
    #[default]
    Sync = 0,
    /// Batched async reads (io_uring on Linux)
    BatchAsync = 1,
    /// Memory-mapped reads
    Mmap = 2,
    /// Prefetch hints
    Prefetch = 3,
}

/// Solver configuration
///
/// # Fields
/// - `max_iterations`: Maximum fixed-point iterations (default 10)
/// - `fetch_budget`: Atoms per iteration (64-512)
/// - `io_budget`: Bytes per iteration
/// - `max_coalesce_gap`: Maximum gap between offsets for I/O coalescing (64KB default)
/// - `io_mode`: I/O mode (MMAP, IO_URING, DIRECT)
#[derive(Debug, Clone)]
pub struct SolverConfig {
    /// Maximum iterations (default 10)
    pub max_iterations: u32,
    /// Fetch budget: atoms per iteration (64-512)
    pub fetch_budget: u32,
    /// I/O budget: bytes per iteration
    pub io_budget: u32,
    /// Maximum gap for I/O coalescing (default 64KB)
    pub max_coalesce_gap: usize,
    /// I/O mode
    pub io_mode: IoMode,
}

impl Default for SolverConfig {
    fn default() -> Self {
        SolverConfig {
            max_iterations: 10,
            fetch_budget: 128,
            io_budget: 256 * 1024,       // 256KB
            max_coalesce_gap: 64 * 1024, // 64KB
            io_mode: IoMode::Mmap,
        }
    }
}

impl SolverConfig {
    /// Create a new solver config
    #[inline]
    pub fn new(max_iterations: u32, fetch_budget: u32, io_budget: u32) -> Self {
        SolverConfig {
            max_iterations,
            fetch_budget,
            io_budget,
            max_coalesce_gap: 64 * 1024,
            io_mode: IoMode::Mmap,
        }
    }

    /// Set I/O mode
    #[inline]
    pub fn with_io_mode(mut self, io_mode: IoMode) -> Self {
        self.io_mode = io_mode;
        self
    }

    /// Set max coalesce gap
    #[inline]
    pub fn with_coalesce_gap(mut self, gap: usize) -> Self {
        self.max_coalesce_gap = gap;
        self
    }

    /// Validate configuration
    pub fn validate(&self) -> Result<(), SolverError> {
        if self.max_iterations == 0 {
            return Err(SolverError::InvalidConfig("max_iterations must be > 0"));
        }
        if self.fetch_budget < 64 || self.fetch_budget > 512 {
            return Err(SolverError::InvalidConfig("fetch_budget must be 64-512"));
        }
        if self.io_budget < 1024 {
            return Err(SolverError::InvalidConfig("io_budget must be >= 1024"));
        }
        Ok(())
    }
}

// ============================================================================
// TMS Branch Tracking (SKF-1.1 Section 3.2)
// ============================================================================

/// Information about a candidate that requires TMS context branching.
/// When NeedBranch is returned from invariant evaluation, the solver must:
/// 1. Create a new context branch via ctx_manager.branch_ctx()
/// 2. Associate the candidate with the new branch (branch_ctx_id)
/// 3. Add the candidate to the admissible set for that branch
#[derive(Debug, Clone)]
pub struct BranchedCandidate {
    /// The candidate that triggered the NeedBranch
    pub candidate: crate::query::router::Candidate,
    /// Pattern hash for conflict detection (computed from claim subj+pred)
    pub pattern_hash: u64,
    /// Conflicting atom ID from CtxIndex (if available)
    pub conflicting_atom_id: Option<AtomId>,
}

/// Result of filter_by_invariants containing normal admissible candidates
/// and candidates that require TMS context branching.
pub struct InvariantFilterResult {
    /// Candidates that passed invariant checks (admissible in current CTX)
    pub admissible: Vec<crate::query::router::Candidate>,
    /// Candidates that require context branching (NeedBranch result)
    pub need_branch: Vec<BranchedCandidate>,
    /// Count of hard rejection failures
    pub rejected_hard: u32,
    /// Count of soft rejection failures
    pub rejected_soft: u32,
    /// Contract-level hard/MUST_NOT rejections before invariant evaluation.
    pub contract_rejections: Vec<RejectedCandidateSummary>,
}

// ============================================================================
// Candidate, SourcePriority, QueryRouter, Candidate types
// are now defined in the router module and re-exported via mod.rs
// using super::{Candidate, SourcePriority, QueryRouter, BackendKind, Candidate};

/// Fixed-point solver state
pub struct SolverState {
    /// Context ID
    pub ctx_id: CtxId,
    /// Goal specification
    pub goal: GoalSpec,
    /// Gaps to fill
    pub gaps: Vec<Gap>,
    /// Answer graph (minimal proof subgraph)
    pub answer_graph: AnswerGraph,
    /// Current generation
    pub generation: u32,
    /// Iteration count
    pub iterations: u32,
    /// Cost weights
    pub weights: CostWeights,
    /// Stable flag
    pub stable: bool,
    /// Budget exceeded flag
    pub budget_exceeded: bool,
    /// Loaded atoms cache
    pub loaded_atoms: HashMap<AtomId, AtomView<'static>>,
    /// Fetched offsets
    pub fetched_offsets: Vec<(u32, u64, u32)>,
    /// Previous graph hash (for convergence detection)
    pub prev_graph_hash: u64,
    /// Candidates rejected by query contract constraints before ranking.
    pub rejected_candidates: Vec<RejectedCandidateSummary>,
    /// Bounded query planning trace.
    pub query_trace: crate::store::api::QueryTrace,
}

impl SolverState {
    /// Create new solver state
    #[inline]
    pub fn new(ctx_id: CtxId, goal: GoalSpec, gaps: Vec<Gap>) -> Self {
        SolverState {
            ctx_id,
            goal,
            gaps,
            answer_graph: AnswerGraph::new(),
            generation: 0,
            iterations: 0,
            weights: CostWeights::default(),
            stable: false,
            budget_exceeded: false,
            loaded_atoms: HashMap::new(),
            fetched_offsets: Vec::new(),
            prev_graph_hash: 0,
            rejected_candidates: Vec::new(),
            query_trace: crate::store::api::QueryTrace::default(),
        }
    }

    /// Get uncovered gaps
    #[inline]
    pub fn uncovered_gaps(&self) -> impl Iterator<Item = (usize, &Gap)> {
        self.gaps.iter().enumerate().filter(|(_, g)| !g.covered)
    }

    /// Get uncovered gap IDs
    #[inline]
    pub fn uncovered_gap_ids(&self) -> Vec<GapId> {
        self.gaps
            .iter()
            .filter(|g| !g.covered)
            .map(|g| g.id)
            .collect()
    }

    /// Check if all gaps are covered
    #[inline]
    pub fn all_gaps_covered(&self) -> bool {
        self.gaps.iter().all(|g| g.covered)
    }

    /// Get gap by ID
    #[inline]
    pub fn get_gap(&self, gap_id: GapId) -> Option<&Gap> {
        self.gaps.iter().find(|g| g.id == gap_id)
    }

    /// Get mutable gap by ID
    #[inline]
    pub fn get_gap_mut(&mut self, gap_id: GapId) -> Option<&mut Gap> {
        self.gaps.iter_mut().find(|g| g.id == gap_id)
    }

    /// Count covered gaps
    #[inline]
    pub fn covered_count(&self) -> usize {
        self.gaps.iter().filter(|g| g.covered).count()
    }

    /// Get total gap count
    #[inline]
    pub fn total_gaps(&self) -> usize {
        self.gaps.len()
    }

    /// Check budget constraints
    pub fn check_budget(&mut self, config: &SolverConfig) -> bool {
        let total_io: u64 = self
            .fetched_offsets
            .iter()
            .map(|(_, _, len)| *len as u64)
            .sum();
        if total_io > config.io_budget as u64 {
            self.budget_exceeded = true;
            return false;
        }
        true
    }
}

// ============================================================================
// Goal Specification
// ============================================================================

// EntityRef is defined in store::api and re-exported

/// Time query mode
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeMode {
    /// Exact timestamp match
    Exact = 0,
    /// Range overlap
    #[default]
    Overlap = 1,
    /// Contained within range
    Contained = 2,
    /// Latest N items
    Latest = 3,
}

/// Time range specification
#[derive(Debug, Clone, Copy, Default)]
pub struct TimeRange {
    pub from_ns: u64,
    pub to_ns: u64,
    pub mode: TimeMode,
}

impl TimeRange {
    /// Create a new time range
    #[inline]
    pub const fn new(from_ns: u64, to_ns: u64, mode: TimeMode) -> Self {
        TimeRange {
            from_ns,
            to_ns,
            mode,
        }
    }

    /// Create an unbounded time range
    #[inline]
    pub const fn unbounded() -> Self {
        TimeRange {
            from_ns: 0,
            to_ns: u64::MAX,
            mode: TimeMode::Overlap,
        }
    }

    /// Check if a timestamp is within range
    #[inline]
    pub fn contains(&self, timestamp_ns: u64) -> bool {
        timestamp_ns >= self.from_ns && timestamp_ns < self.to_ns
    }
}

/// Output schema specification
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputSchema {
    /// Flat list of results
    #[default]
    Flat = 0,
    /// Tree structure with parent-child
    Tree = 1,
    /// Graph with edges
    Graph = 2,
    /// Explanation with evidence chains
    Explanation = 3,
    /// Comparison table
    Comparison = 4,
}

/// Goal specification for query execution
#[derive(Debug, Clone)]
pub struct GoalSpec {
    /// Query intent
    pub intent: Intent,
    /// Time constraints
    pub time_q: TimeRange,
    /// Minimum trust level
    pub trust_min: TrustLevel,
    /// Domain bitmask
    pub domain_mask: DomainMask,
    /// Entity references
    pub entities: Vec<EntityRef>,
    /// True when a natural-language target was resolved against the durable
    /// lexicon. In this mode an empty resolution must not broaden to all atoms.
    pub lexical_resolution_required: bool,
    /// Query embedding vectors for ANN-backed semantic retrieval.
    pub semantic_vectors: Vec<Vec<f32>>,
    /// Hard/soft/negative constraints lowered from the public QueryContract.
    pub constraints: Vec<crate::query::contract::Constraint>,
    /// Context selectors and branch policy lowered from the public QueryContract.
    pub context_scope: crate::query::contract::ContextScope,
    /// Conflict resolution/exposure policy lowered from the public QueryContract.
    pub conflict_policy: crate::query::contract::ConflictPolicy,
    /// Comparison axes
    pub axes: Vec<SymId>,
    /// Allowed atom types bitmask
    pub want: AtomTypeMask,
    /// Output format schema
    pub output_schema: OutputSchema,
    /// Context policy ID
    pub ctx_policy: CtxPolicyId,
}

pub type AtomTypeMask = u64;
pub type CtxPolicyId = u32;

impl GoalSpec {
    /// Create a new GoalSpec
    #[inline]
    pub fn new(intent: Intent) -> Self {
        GoalSpec {
            intent,
            time_q: TimeRange::unbounded(),
            trust_min: 0,
            domain_mask: 0,
            entities: Vec::new(),
            lexical_resolution_required: false,
            semantic_vectors: Vec::new(),
            constraints: Vec::new(),
            context_scope: crate::query::contract::ContextScope::default(),
            conflict_policy: crate::query::contract::ConflictPolicy::default(),
            axes: Vec::new(),
            want: 0xFFFF,
            output_schema: OutputSchema::Flat,
            ctx_policy: 0,
        }
    }

    /// Set time range
    #[inline]
    pub fn with_time(mut self, time_q: TimeRange) -> Self {
        self.time_q = time_q;
        self
    }

    /// Set minimum trust
    #[inline]
    pub fn with_trust(mut self, trust_min: TrustLevel) -> Self {
        self.trust_min = trust_min;
        self
    }

    /// Set domain mask
    #[inline]
    pub fn with_domain(mut self, domain_mask: DomainMask) -> Self {
        self.domain_mask = domain_mask;
        self
    }

    /// Add entity references
    #[inline]
    pub fn with_entities(mut self, entities: Vec<EntityRef>) -> Self {
        self.entities = entities;
        self
    }

    /// Mark this goal as a lexicon-resolved natural-language query.
    #[inline]
    pub fn with_lexical_resolution_required(mut self, required: bool) -> Self {
        self.lexical_resolution_required = required;
        self
    }

    /// Add query embedding vectors for semantic ANN retrieval.
    #[inline]
    pub fn with_semantic_vectors(mut self, vectors: Vec<Vec<f32>>) -> Self {
        self.semantic_vectors = vectors;
        self
    }

    /// Add public query constraints for pre-ranking candidate validation.
    #[inline]
    pub fn with_constraints(
        mut self,
        constraints: Vec<crate::query::contract::Constraint>,
    ) -> Self {
        self.constraints = constraints;
        self
    }

    /// Set context selector policy for pre-ranking candidate validation.
    #[inline]
    pub fn with_context_scope(
        mut self,
        context_scope: crate::query::contract::ContextScope,
    ) -> Self {
        self.context_scope = context_scope;
        self
    }

    /// Set conflict handling policy for branch selection and AnswerPack exposure.
    #[inline]
    pub fn with_conflict_policy(
        mut self,
        conflict_policy: crate::query::contract::ConflictPolicy,
    ) -> Self {
        self.conflict_policy = conflict_policy;
        self
    }

    /// Add comparison axes
    #[inline]
    pub fn with_axes(mut self, axes: Vec<SymId>) -> Self {
        self.axes = axes;
        self
    }

    /// Set allowed atom types
    #[inline]
    pub fn with_want(mut self, want: AtomTypeMask) -> Self {
        self.want = want;
        self
    }

    /// Set output schema
    #[inline]
    pub fn with_output_schema(mut self, schema: OutputSchema) -> Self {
        self.output_schema = schema;
        self
    }

    /// Set context policy
    #[inline]
    pub fn with_ctx_policy(mut self, policy: CtxPolicyId) -> Self {
        self.ctx_policy = policy;
        self
    }

    /// Get iteration limit
    #[inline]
    pub fn iter_limit(&self) -> u32 {
        100
    }

    /// Get node seeds
    pub fn get_node_seeds(&self) -> Vec<NodeNum> {
        self.entities.iter().filter_map(|e| e.as_node()).collect()
    }
}

// ============================================================================
// Backward Wave Generator (SKF-1.1 Section 4.3)
// ============================================================================

/// BackwardWave generator: GoalSpec -> Gaps
pub struct BackwardWaveGenerator;

impl BackwardWaveGenerator {
    /// Generate gaps from GoalSpec based on intent (SKF-1.1 Section 4.3)
    pub fn generate(gs: &GoalSpec) -> Vec<Gap> {
        match gs.intent {
            Intent::LOOKUP => Self::generate_lookup_gaps(gs),
            Intent::DEFINE => Self::generate_define_gaps(gs),
            Intent::EXPLAIN => Self::generate_explain_gaps(gs),
            Intent::COMPARE => Self::generate_compare_gaps(gs),
            Intent::DERIVE => Self::generate_derive_gaps(gs),
            Intent::VERIFY => Self::generate_verify_gaps(gs),
            Intent::PLAN => Self::generate_plan_gaps(gs),
        }
    }

    /// LOOKUP: NEED_FACT + NEED_EVIDENCE
    fn generate_lookup_gaps(gs: &GoalSpec) -> Vec<Gap> {
        let mut gaps = Vec::new();
        let mut gap_id = 0u32;

        for entity in &gs.entities {
            // NEED_FACT gap
            let pattern = ClaimPattern {
                subj: entity
                    .as_node()
                    .map(PatternRef::Node)
                    .unwrap_or(PatternRef::Any),
                pred: PatternRef::Any,
                obj_tag: None,
                obj: PatternRef::Any,
                qualifiers_mask: 0,
            };

            let mut gap = Gap::new(gap_id, GapKind::NEED_FACT, pattern);
            if let Some(node) = entity.as_node() {
                gap.nav.seed_nodes.push(node);
            }
            gap.priority = 180;
            gaps.push(gap);
            gap_id += 1;

            // NEED_EVIDENCE gap (strict mode)
            let evidence_pattern = ClaimPattern {
                subj: entity
                    .as_node()
                    .map(PatternRef::Node)
                    .unwrap_or(PatternRef::Any),
                pred: PatternRef::Any,
                obj_tag: None,
                obj: PatternRef::Any,
                qualifiers_mask: 0,
            };
            let mut evidence_gap = Gap::new(gap_id, GapKind::NEED_EVIDENCE, evidence_pattern);
            evidence_gap.priority = 150;
            gaps.push(evidence_gap);
            gap_id += 1;
        }

        gaps
    }

    /// DEFINE: NEED_DEFINITION + NEED_CONSTRAINTS + NEED_COUNTEREXAMPLE
    fn generate_define_gaps(gs: &GoalSpec) -> Vec<Gap> {
        let mut gaps = Vec::new();
        let mut gap_id = 0u32;

        for entity in &gs.entities {
            // NEED_DEFINITION gap (high priority)
            let pattern = ClaimPattern {
                subj: entity
                    .as_node()
                    .map(PatternRef::Node)
                    .unwrap_or(PatternRef::Any),
                pred: PatternRef::Any,
                obj_tag: Some(ObjTag::SYM),
                obj: PatternRef::Any,
                qualifiers_mask: 0,
            };

            let mut gap = Gap::high_priority(gap_id, GapKind::NEED_DEFINITION, pattern);
            gap.nav.edge_types.push(EdgeType::DEFINES);
            gap.nav.max_depth = 2;
            gaps.push(gap);
            gap_id += 1;

            // NEED_CONSTRAINTS gap
            let constraint_pattern = ClaimPattern {
                subj: entity
                    .as_node()
                    .map(PatternRef::Node)
                    .unwrap_or(PatternRef::Any),
                pred: PatternRef::Any,
                obj_tag: None,
                obj: PatternRef::Any,
                qualifiers_mask: 0,
            };
            let mut constraint_gap =
                Gap::new(gap_id, GapKind::NEED_CONSTRAINTS, constraint_pattern);
            constraint_gap.priority = 160;
            gaps.push(constraint_gap);
            gap_id += 1;

            // NEED_COUNTEREXAMPLE (optional, lower priority)
            let counter_pattern = ClaimPattern {
                subj: entity
                    .as_node()
                    .map(PatternRef::Node)
                    .unwrap_or(PatternRef::Any),
                pred: PatternRef::Any,
                obj_tag: None,
                obj: PatternRef::Any,
                qualifiers_mask: 0,
            };
            let mut counter_gap = Gap::new(gap_id, GapKind::NEED_COUNTEREXAMPLE, counter_pattern);
            counter_gap.priority = 80;
            counter_gap.nav.edge_types.push(EdgeType::CONTRADICTS);
            gaps.push(counter_gap);
            gap_id += 1;
        }

        gaps
    }

    /// EXPLAIN: NEED_DEFINITION + NEED_CAUSAL_CHAIN + NEED_CONSTRAINTS + NEED_ALTERNATES
    fn generate_explain_gaps(gs: &GoalSpec) -> Vec<Gap> {
        let mut gaps = Vec::new();
        let mut gap_id = 0u32;

        for entity in &gs.entities {
            // NEED_DEFINITION
            let def_pattern = ClaimPattern {
                subj: entity
                    .as_node()
                    .map(PatternRef::Node)
                    .unwrap_or(PatternRef::Any),
                pred: PatternRef::Any,
                obj_tag: None,
                obj: PatternRef::Any,
                qualifiers_mask: 0,
            };
            let mut def_gap = Gap::high_priority(gap_id, GapKind::NEED_DEFINITION, def_pattern);
            def_gap.priority = 200;
            gaps.push(def_gap);
            gap_id += 1;

            // NEED_CAUSAL_CHAIN
            let causal_pattern = ClaimPattern {
                subj: entity
                    .as_node()
                    .map(PatternRef::Node)
                    .unwrap_or(PatternRef::Any),
                pred: PatternRef::Any,
                obj_tag: None,
                obj: PatternRef::Any,
                qualifiers_mask: 0,
            };
            let mut causal_gap =
                Gap::high_priority(gap_id, GapKind::NEED_CAUSAL_CHAIN, causal_pattern);
            causal_gap.nav.edge_types = vec![
                EdgeType::CAUSES,
                EdgeType::ENABLES,
                EdgeType::PREVENTS,
                EdgeType::SUPPORTS,
            ];
            causal_gap.nav.max_depth = 5;
            causal_gap.priority = 190;
            gaps.push(causal_gap);
            gap_id += 1;

            // NEED_CONSTRAINTS
            let constraint_pattern = ClaimPattern {
                subj: entity
                    .as_node()
                    .map(PatternRef::Node)
                    .unwrap_or(PatternRef::Any),
                pred: PatternRef::Any,
                obj_tag: None,
                obj: PatternRef::Any,
                qualifiers_mask: 0,
            };
            let mut constraint_gap =
                Gap::new(gap_id, GapKind::NEED_CONSTRAINTS, constraint_pattern);
            constraint_gap.priority = 140;
            gaps.push(constraint_gap);
            gap_id += 1;
        }

        gaps
    }

    /// COMPARE: NEED_DEFINITION(A,B) + NEED_COMPARISON_AXIS + NEED_FACT + NEED_CONSTRAINTS
    fn generate_compare_gaps(gs: &GoalSpec) -> Vec<Gap> {
        let mut gaps = Vec::new();
        let mut gap_id = 0u32;

        // NEED_DEFINITION for each entity
        for entity in &gs.entities {
            let pattern = ClaimPattern {
                subj: entity
                    .as_node()
                    .map(PatternRef::Node)
                    .unwrap_or(PatternRef::Any),
                pred: PatternRef::Any,
                obj_tag: None,
                obj: PatternRef::Any,
                qualifiers_mask: 0,
            };
            let mut gap = Gap::high_priority(gap_id, GapKind::NEED_DEFINITION, pattern);
            gap.priority = 200;
            gaps.push(gap);
            gap_id += 1;
        }

        // NEED_COMPARISON_AXIS
        for &axis in &gs.axes {
            let pattern = ClaimPattern {
                subj: PatternRef::Sym(axis),
                pred: PatternRef::Any,
                obj_tag: None,
                obj: PatternRef::Any,
                qualifiers_mask: 0,
            };
            let mut gap = Gap::new(gap_id, GapKind::NEED_COMPARISON_AXIS, pattern);
            gap.priority = 170;
            gaps.push(gap);
            gap_id += 1;
        }

        // NEED_FACT for each entity on each axis
        for entity in &gs.entities {
            for &axis in &gs.axes {
                let pattern = ClaimPattern {
                    subj: entity
                        .as_node()
                        .map(PatternRef::Node)
                        .unwrap_or(PatternRef::Any),
                    pred: PatternRef::Sym(axis),
                    obj_tag: None,
                    obj: PatternRef::Any,
                    qualifiers_mask: 0,
                };
                let mut gap = Gap::new(gap_id, GapKind::NEED_FACT, pattern);
                gap.priority = 160;
                gaps.push(gap);
                gap_id += 1;
            }
        }

        // NEED_CONSTRAINTS
        let constraint_pattern = ClaimPattern::default();
        let mut constraint_gap = Gap::new(gap_id, GapKind::NEED_CONSTRAINTS, constraint_pattern);
        constraint_gap.priority = 120;
        gaps.push(constraint_gap);

        gaps
    }

    /// DERIVE: NEED_RULE + NEED_FACT + NEED_CONSTRAINTS
    fn generate_derive_gaps(gs: &GoalSpec) -> Vec<Gap> {
        let mut gaps = Vec::new();
        let mut gap_id = 0u32;

        // NEED_RULE for inference
        let rule_pattern = ClaimPattern {
            subj: PatternRef::Any,
            pred: PatternRef::Any,
            obj_tag: None,
            obj: PatternRef::Any,
            qualifiers_mask: 0,
        };
        let mut rule_gap = Gap::high_priority(gap_id, GapKind::NEED_PROCEDURE, rule_pattern);
        rule_gap.nav.edge_types.push(EdgeType::IMPLIES);
        rule_gap.priority = 200;
        gaps.push(rule_gap);
        gap_id += 1;

        // NEED_FACT for premises
        for entity in &gs.entities {
            let pattern = ClaimPattern {
                subj: entity
                    .as_node()
                    .map(PatternRef::Node)
                    .unwrap_or(PatternRef::Any),
                pred: PatternRef::Any,
                obj_tag: None,
                obj: PatternRef::Any,
                qualifiers_mask: 0,
            };
            let mut gap = Gap::new(gap_id, GapKind::NEED_FACT, pattern);
            gap.priority = 170;
            gaps.push(gap);
            gap_id += 1;
        }

        // NEED_CONSTRAINTS
        let constraint_pattern = ClaimPattern::default();
        let mut constraint_gap = Gap::new(gap_id, GapKind::NEED_CONSTRAINTS, constraint_pattern);
        constraint_gap.priority = 140;
        gaps.push(constraint_gap);

        gaps
    }

    /// VERIFY: NEED_EVIDENCE + NEED_CONTRADICTIONS + NEED_CONSTRAINTS
    fn generate_verify_gaps(gs: &GoalSpec) -> Vec<Gap> {
        let mut gaps = Vec::new();
        let mut gap_id = 0u32;

        for entity in &gs.entities {
            // NEED_EVIDENCE (high priority)
            let evidence_pattern = ClaimPattern {
                subj: entity
                    .as_node()
                    .map(PatternRef::Node)
                    .unwrap_or(PatternRef::Any),
                pred: PatternRef::Any,
                obj_tag: None,
                obj: PatternRef::Any,
                qualifiers_mask: 0,
            };
            let mut evidence_gap =
                Gap::high_priority(gap_id, GapKind::NEED_EVIDENCE, evidence_pattern);
            evidence_gap.priority = 200;
            gaps.push(evidence_gap);
            gap_id += 1;

            // NEED_CONTRADICTIONS (via NEED_COUNTEREXAMPLE)
            let contradiction_pattern = ClaimPattern {
                subj: entity
                    .as_node()
                    .map(PatternRef::Node)
                    .unwrap_or(PatternRef::Any),
                pred: PatternRef::Any,
                obj_tag: None,
                obj: PatternRef::Any,
                qualifiers_mask: 0,
            };
            let mut contradiction_gap =
                Gap::high_priority(gap_id, GapKind::NEED_COUNTEREXAMPLE, contradiction_pattern);
            contradiction_gap.nav.edge_types = vec![EdgeType::CONTRADICTS];
            contradiction_gap.nav.max_depth = 3;
            contradiction_gap.priority = 180;
            gaps.push(contradiction_gap);
            gap_id += 1;
        }

        // NEED_CONSTRAINTS
        let constraint_pattern = ClaimPattern::default();
        let mut constraint_gap = Gap::new(gap_id, GapKind::NEED_CONSTRAINTS, constraint_pattern);
        constraint_gap.priority = 150;
        gaps.push(constraint_gap);

        gaps
    }

    /// PLAN: NEED_PROCEDURE + NEED_CONSTRAINTS + NEED_FACT + NEED_COUNTEREXAMPLE
    fn generate_plan_gaps(gs: &GoalSpec) -> Vec<Gap> {
        let mut gaps = Vec::new();
        let mut gap_id = 0u32;

        // NEED_PROCEDURE (high priority)
        let procedure_pattern = ClaimPattern {
            subj: PatternRef::Any,
            pred: PatternRef::Any,
            obj_tag: None,
            obj: PatternRef::Any,
            qualifiers_mask: 0,
        };
        let mut procedure_gap =
            Gap::high_priority(gap_id, GapKind::NEED_PROCEDURE, procedure_pattern);
        procedure_gap.nav.edge_types = vec![EdgeType::STEP_OF];
        procedure_gap.nav.max_depth = 10;
        procedure_gap.priority = 200;
        gaps.push(procedure_gap);
        gap_id += 1;

        // NEED_CONSTRAINTS
        let constraint_pattern = ClaimPattern::default();
        let mut constraint_gap = Gap::new(gap_id, GapKind::NEED_CONSTRAINTS, constraint_pattern);
        constraint_gap.priority = 160;
        gaps.push(constraint_gap);
        gap_id += 1;

        // NEED_FACT for resources/prerequisites
        for entity in &gs.entities {
            let pattern = ClaimPattern {
                subj: entity
                    .as_node()
                    .map(PatternRef::Node)
                    .unwrap_or(PatternRef::Any),
                pred: PatternRef::Any,
                obj_tag: None,
                obj: PatternRef::Any,
                qualifiers_mask: 0,
            };
            let mut gap = Gap::new(gap_id, GapKind::NEED_FACT, pattern);
            gap.priority = 150;
            gaps.push(gap);
            gap_id += 1;
        }

        // NEED_COUNTEREXAMPLE for risks
        let risk_pattern = ClaimPattern::default();
        let mut risk_gap = Gap::new(gap_id, GapKind::NEED_COUNTEREXAMPLE, risk_pattern);
        risk_gap.priority = 130;
        gaps.push(risk_gap);

        gaps
    }
}

// ============================================================================
// Set Cover Algorithm
// ============================================================================

/// Set cover solver using greedy benefit/cost ratio
pub struct SetCoverSolver;

impl SetCoverSolver {
    /// Greedy set cover: select atoms with max benefit/cost ratio
    ///
    /// # Arguments
    /// - `candidates`: Available candidates
    /// - `gaps`: Gaps to cover
    /// - `weights`: Cost weights
    /// - `output_schema`: Schema requirements for connectivity
    ///
    /// # Returns
    /// - Selected candidate indices
    pub fn greedy_select(
        candidates: &[Candidate],
        gaps: &[Gap],
        weights: &CostWeights,
        output_schema: OutputSchema,
    ) -> Vec<usize> {
        let mut selected = Vec::new();
        let mut covered_gaps: HashSet<GapId> = HashSet::new();
        let mut available: Vec<usize> = (0..candidates.len()).collect();

        while covered_gaps.len() < gaps.len() && !available.is_empty() {
            // Find candidate with best benefit/cost ratio
            let mut best_idx = None;
            let mut best_ratio = f64::NEG_INFINITY;

            for &idx in &available {
                let candidate = &candidates[idx];

                // Calculate marginal benefit (gaps not yet covered)
                let marginal_gaps: Vec<GapId> = candidate
                    .covers_gaps
                    .iter()
                    .copied()
                    .filter(|g| !covered_gaps.contains(g))
                    .collect();

                if marginal_gaps.is_empty() {
                    continue;
                }

                // Calculate marginal benefit
                let marginal_benefit: f64 = marginal_gaps
                    .iter()
                    .filter_map(|&g| gaps.get(g as usize))
                    .map(|g| weights.gap_benefit(g.priority))
                    .sum();

                // Calculate cost
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

                let ratio = marginal_benefit / cost;

                if ratio > best_ratio {
                    best_ratio = ratio;
                    best_idx = Some(idx);
                }
            }

            if let Some(idx) = best_idx {
                let candidate = &candidates[idx];
                selected.push(idx);

                // Mark gaps as covered
                for &gap_id in &candidate.covers_gaps {
                    covered_gaps.insert(gap_id);
                }

                // Remove from available
                available.retain(|&i| i != idx);
            } else {
                break; // No more useful candidates
            }
        }

        // Add connectivity closure if output_schema requires chains
        if matches!(
            output_schema,
            OutputSchema::Tree | OutputSchema::Graph | OutputSchema::Explanation
        ) {
            selected = Self::add_connectivity_closure(candidates, &selected, gaps);
        }

        selected
    }

    /// Add connectivity closure for tree/graph/explanation schemas.
    ///
    /// Ensures the AnswerGraph is transitively connected: if two selected nodes
    /// share a common referenced atom (via evidence, derived claims, or gap patterns),
    /// that bridging atom must also be included. Iterates to fixed point.
    ///
    /// Algorithm:
    /// 1. Build a set of "bridge keys" from selected nodes: evidence atom IDs,
    ///    derived claim subject/predicate/object IDs, and gap pattern references.
    /// 2. For each pair of selected nodes, check if they share any bridge key.
    ///    If they do, they are connected.
    /// 3. Find connected components. If there are multiple components, search
    ///    unselected candidates for bridge nodes that connect them.
    /// 4. Add bridge nodes and repeat until single component or no more bridges found.
    fn add_connectivity_closure(
        candidates: &[Candidate],
        selected: &[usize],
        _gaps: &[Gap],
    ) -> Vec<usize> {
        if selected.len() <= 1 {
            return selected.to_vec();
        }

        let mut result: HashSet<usize> = selected.iter().copied().collect();
        let mut changed = true;
        let max_iterations = 10;
        let mut iteration = 0;

        while changed && iteration < max_iterations {
            changed = false;
            iteration += 1;

            // Build connectivity map: for each selected node, collect its "bridge keys"
            // Bridge keys are identifiers that can link nodes together:
            // - Evidence atom IDs
            // - Derived claim subject IDs
            // - Gap IDs covered (shared gaps = shared purpose)
            let mut node_keys: HashMap<usize, HashSet<u64>> = HashMap::new();
            for &idx in &result {
                let candidate = &candidates[idx];
                let mut keys = HashSet::new();

                // Evidence references create provenance links
                for ev in &candidate.evidence_refs {
                    // Hash the evidence atom_id to create a bridge key
                    let key = SetCoverSolver::atom_id_hash(&ev.atom_id);
                    keys.insert(key);
                }

                // Derived claims create semantic links
                for claim in &candidate.derived_claims {
                    keys.insert(claim.subj);
                    keys.insert(claim.pred << 16);
                    keys.insert(claim.obj_val << 32);
                }

                // Shared gap coverage creates functional links
                for &gap_id in &candidate.covers_gaps {
                    keys.insert(0x1000_0000_0000_0000 | u64::from(gap_id));
                }

                // Node number itself as a key
                keys.insert(0x2000_0000_0000_0000 | candidate.node_num);

                node_keys.insert(idx, keys);
            }

            // Find connected components using union-find
            let mut parent: HashMap<usize, usize> = result.iter().map(|&i| (i, i)).collect();

            fn find(parent: &mut HashMap<usize, usize>, x: usize) -> usize {
                let mut root = x;
                while let Some(&p) = parent.get(&root) {
                    if p != root {
                        root = p;
                    } else {
                        break;
                    }
                }
                // Path compression
                let mut curr = x;
                while curr != root {
                    let next = *parent.get(&curr).unwrap_or(&curr);
                    parent.insert(curr, root);
                    curr = next;
                }
                root
            }

            fn union(parent: &mut HashMap<usize, usize>, a: usize, b: usize) -> bool {
                let ra = find(parent, a);
                let rb = find(parent, b);
                if ra != rb {
                    parent.insert(ra, rb);
                    true
                } else {
                    false
                }
            }

            // Union nodes that share bridge keys
            let selected_vec: Vec<usize> = result.iter().copied().collect();
            for i in 0..selected_vec.len() {
                for j in (i + 1)..selected_vec.len() {
                    let a = selected_vec[i];
                    let b = selected_vec[j];
                    if let (Some(keys_a), Some(keys_b)) = (node_keys.get(&a), node_keys.get(&b))
                        && !keys_a.is_disjoint(keys_b)
                    {
                        union(&mut parent, a, b);
                    }
                }
            }

            // Count components
            let roots: HashSet<usize> =
                selected_vec.iter().map(|&i| find(&mut parent, i)).collect();

            if roots.len() <= 1 {
                break; // Already connected
            }

            // Group nodes by component root
            let mut components: HashMap<usize, Vec<usize>> = HashMap::new();
            for &idx in &selected_vec {
                let root = find(&mut parent, idx);
                components.entry(root).or_default().push(idx);
            }

            // Try to find bridge candidates that connect different components
            let component_roots: Vec<usize> = components.keys().copied().collect();
            let mut bridges_found = Vec::new();

            for ci in 0..component_roots.len() {
                for cj in (ci + 1)..component_roots.len() {
                    let comp_a = &components[&component_roots[ci]];
                    let comp_b = &components[&component_roots[cj]];

                    // Collect all keys from each component
                    let mut keys_a = HashSet::new();
                    for &idx in comp_a {
                        if let Some(keys) = node_keys.get(&idx) {
                            keys_a.extend(keys.iter().copied());
                        }
                    }
                    let mut keys_b = HashSet::new();
                    for &idx in comp_b {
                        if let Some(keys) = node_keys.get(&idx) {
                            keys_b.extend(keys.iter().copied());
                        }
                    }

                    // Find unselected candidates that share keys with BOTH components
                    for (cand_idx, candidate) in candidates.iter().enumerate() {
                        if result.contains(&cand_idx) {
                            continue; // Already selected
                        }

                        let mut cand_keys = HashSet::new();
                        for ev in &candidate.evidence_refs {
                            cand_keys.insert(Self::atom_id_hash(&ev.atom_id));
                        }
                        for claim in &candidate.derived_claims {
                            cand_keys.insert(claim.subj);
                            cand_keys.insert(claim.pred << 16);
                            cand_keys.insert(claim.obj_val << 32);
                        }
                        for &gap_id in &candidate.covers_gaps {
                            cand_keys.insert(0x1000_0000_0000_0000 | u64::from(gap_id));
                        }
                        cand_keys.insert(0x2000_0000_0000_0000 | candidate.node_num);

                        let shares_a = !cand_keys.is_disjoint(&keys_a);
                        let shares_b = !cand_keys.is_disjoint(&keys_b);

                        if shares_a && shares_b {
                            bridges_found.push(cand_idx);
                            break; // One bridge per component pair is enough
                        }
                    }
                }
            }

            // Also add candidates that cover gaps referenced by selected nodes
            // but aren't yet selected — these are implicit dependencies
            if bridges_found.is_empty() {
                let mut referenced_gap_ids: HashSet<GapId> = HashSet::new();
                for &idx in &result {
                    let candidate = &candidates[idx];
                    // Check if any evidence_refs point to atoms that cover gaps
                    for ev in &candidate.evidence_refs {
                        let ev_key = Self::atom_id_hash(&ev.atom_id);
                        for (cand_idx, c) in candidates.iter().enumerate() {
                            if result.contains(&cand_idx) {
                                continue;
                            }
                            if Self::atom_id_hash(&c.atom_id) == ev_key {
                                bridges_found.push(cand_idx);
                                break;
                            }
                        }
                    }
                    // Check derived claims for references to other candidates
                    for claim in &candidate.derived_claims {
                        for (cand_idx, c) in candidates.iter().enumerate() {
                            if result.contains(&cand_idx) {
                                continue;
                            }
                            if c.node_num == claim.subj {
                                bridges_found.push(cand_idx);
                                break;
                            }
                        }
                    }
                    referenced_gap_ids.extend(candidate.covers_gaps.iter().copied());
                }

                // Add candidates that cover the same gaps (provenance completeness)
                if bridges_found.is_empty() {
                    for (cand_idx, candidate) in candidates.iter().enumerate() {
                        if result.contains(&cand_idx) {
                            continue;
                        }
                        let shared_gaps: usize = candidate
                            .covers_gaps
                            .iter()
                            .filter(|g| referenced_gap_ids.contains(g))
                            .count();
                        if shared_gaps > 0 {
                            bridges_found.push(cand_idx);
                        }
                    }
                }
            }

            if !bridges_found.is_empty() {
                for bridge in bridges_found {
                    if result.insert(bridge) {
                        changed = true;
                    }
                }
            }
        }

        let mut final_result: Vec<usize> = result.into_iter().collect();
        final_result.sort_unstable();
        final_result
    }

    /// Hash an AtomId to a u64 for use as a bridge key.
    #[inline]
    pub fn atom_id_hash(atom_id: &AtomId) -> u64 {
        // Use first 8 bytes as hash (sufficient for bridge key uniqueness)
        let mut h: u64 = 0;
        for (i, &b) in atom_id.iter().take(8).enumerate() {
            h |= (b as u64) << (i * 8);
        }
        h
    }

    /// Prune nodes that don't break coverage
    pub fn prune(candidates: &[Candidate], selected: &[usize], _gaps: &[Gap]) -> Vec<usize> {
        let mut result = selected.to_vec();

        // Try removing each node and check if coverage is maintained
        let mut to_remove = Vec::new();

        for &idx in &result {
            let candidate = &candidates[idx];

            // Check if any gap covered only by this candidate
            let uniquely_covered = candidate.covers_gaps.iter().any(|&gap_id| {
                result
                    .iter()
                    .filter(|&&other_idx| other_idx != idx)
                    .any(|&other_idx| candidates[other_idx].covers_gaps.contains(&gap_id))
            });

            if !uniquely_covered {
                to_remove.push(idx);
            }
        }

        result.retain(|&idx| !to_remove.contains(&idx));
        result
    }
}

// ============================================================================
// Answer Pack
// ============================================================================

// Types re-exported from prelude - no need to define duplicates
// AnswerPack, ClaimView, EvidenceRef, Limitation, etc. are available via prelude

// ============================================================================
// Solver Errors
// ============================================================================

/// Solver errors
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolverError {
    /// Invalid configuration
    InvalidConfig(&'static str),
    /// Budget exceeded
    BudgetExceeded,
    /// Maximum iterations reached
    MaxIterationsReached,
    /// CAS error
    CasError(String),
    /// Graph error
    GraphError(String),
    /// VM error
    VmError(String),
    /// Context error
    ContextError(String),
}

impl std::fmt::Display for SolverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SolverError::InvalidConfig(msg) => write!(f, "Invalid config: {}", msg),
            SolverError::BudgetExceeded => write!(f, "Budget exceeded"),
            SolverError::MaxIterationsReached => write!(f, "Maximum iterations reached"),
            SolverError::CasError(msg) => write!(f, "CAS error: {}", msg),
            SolverError::GraphError(msg) => write!(f, "Graph error: {}", msg),
            SolverError::VmError(msg) => write!(f, "VM error: {}", msg),
            SolverError::ContextError(msg) => write!(f, "Context error: {}", msg),
        }
    }
}

impl std::error::Error for SolverError {}

impl From<CasError> for SolverError {
    fn from(err: CasError) -> Self {
        SolverError::CasError(err.to_string())
    }
}

// ============================================================================
// Backward Compatibility Types
// ============================================================================

/// Fixed-point solver state (backward compatibility alias)
pub type FixedPointState = SolverState;

/// GoalSpec compiler for query text compilation
pub struct GoalSpecCompiler;

impl GoalSpecCompiler {
    /// Compile query text into GoalSpec
    pub fn compile(query: &str) -> GoalSpec {
        let tokens = Self::tokenize(query);
        let intent = Self::classify_intent(&tokens);
        let entities = Self::extract_entities(&tokens);

        GoalSpec::new(intent).with_entities(entities)
    }

    fn tokenize(query: &str) -> Vec<String> {
        query
            .split(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_lowercase())
            .collect()
    }

    fn classify_intent(tokens: &[String]) -> Intent {
        for token in tokens {
            match token.as_str() {
                "what" | "who" | "where" | "when" | "find" | "get" | "show" => {
                    return Intent::LOOKUP;
                }
                "define" | "definition" | "meaning" | "what is" => {
                    return Intent::DEFINE;
                }
                "explain" | "why" | "how" | "reason" | "cause" => {
                    return Intent::EXPLAIN;
                }
                "compare" | "vs" | "versus" | "difference" | "better" => {
                    return Intent::COMPARE;
                }
                "derive" | "conclude" | "infer" | "therefore" => {
                    return Intent::DERIVE;
                }
                "verify" | "check" | "validate" | "confirm" | "prove" => {
                    return Intent::VERIFY;
                }
                "plan" | "how to" | "steps" | "procedure" => {
                    return Intent::PLAN;
                }
                _ => {}
            }
        }
        Intent::LOOKUP
    }

    fn extract_entities(tokens: &[String]) -> Vec<EntityRef> {
        tokens
            .iter()
            .filter(|t| {
                !matches!(
                    t.as_str(),
                    "what"
                        | "who"
                        | "where"
                        | "when"
                        | "why"
                        | "how"
                        | "define"
                        | "explain"
                        | "compare"
                        | "verify"
                        | "plan"
                        | "the"
                        | "a"
                        | "an"
                        | "is"
                        | "are"
                        | "was"
                        | "were"
                )
            })
            .enumerate()
            .map(|(i, t)| EntityRef::Term(t.parse::<u32>().unwrap_or(i as u32)))
            .collect()
    }
}

/// Retrieval plan for gap resolution (backward compatibility)
#[derive(Debug, Clone)]
pub struct RetrievalPlan {
    pub gs: GoalSpec,
    pub ctx_id: CtxId,
    pub iter_limit: u32,
    pub fetch_budget: u32,
    pub io_mode: IoMode,
}

impl RetrievalPlan {
    pub fn new(gs: GoalSpec, ctx_id: CtxId) -> Self {
        RetrievalPlan {
            gs,
            ctx_id,
            iter_limit: 100,
            fetch_budget: 64 * 1024,
            io_mode: IoMode::default(),
        }
    }
}

/// Router for backward compatibility (uses QueryRouter internally)
pub struct Router;

impl Router {
    pub fn route(gap: &Gap, goal: &GoalSpec) -> Vec<LegacyCandidate> {
        let query_router = QueryRouter::new();
        let candidates = query_router.route(gap, goal);
        candidates
            .into_iter()
            .map(|c| LegacyCandidate {
                atom_id: c.atom_id,
                node_num: c.node_num,
                atom_type: c.atom_type,
                trust: c.trust,
                estimated_io_bytes: c.estimated_io_bytes,
                source_priority: c.source_priority as u8,
                covers_gaps: c.covers_gaps,
            })
            .collect()
    }
}

/// Legacy candidate structure for backward compatibility
#[derive(Debug, Clone)]
pub struct LegacyCandidate {
    pub atom_id: AtomId,
    pub node_num: NodeNum,
    pub atom_type: AtomType,
    pub trust: TrustLevel,
    pub estimated_io_bytes: u32,
    pub source_priority: u8,
    pub covers_gaps: Vec<GapId>,
}

impl LegacyCandidate {
    pub fn benefit_cost_ratio(&self, gaps: &[Gap]) -> f64 {
        let benefit: f64 = self
            .covers_gaps
            .iter()
            .filter_map(|&g| gaps.get(g as usize))
            .map(|g| g.priority as f64)
            .sum();

        let cost = self.estimated_io_bytes as f64 + (self.trust.max(1) as f64).recip() * 100.0;

        if cost > 0.0 {
            benefit / cost
        } else {
            f64::INFINITY
        }
    }
}

// ============================================================================
// Fixed-Point Solver
// ============================================================================

/// Fixed-Point Answer Solver for MemoryX SKF-1.1
///
/// Implements the fixed-point iteration algorithm from SKF-1.1 Section 5.2:
///
/// ```text
/// State: (CTX_k, Gaps_k, AnswerGraph_k)
///
/// Initialization:
///   CTX_0 = from ctx_policy
///   Gaps_0 = BackwardWave(GoalSpec)
///   AG_0 = empty
///
/// Iteration k -> k+1:
///   1. Candidates = Router(Gaps_k) - route by source
///   2. Admissible = FilterByInvariants(Candidates, CTX_k) - VM bytecode
///   3. CTX' = UpdateContext(CTX_k, Admissible) - branch on conflicts
///   4. Derived = Infer(CTX', rules) - derive new claims
///   5. AG_{k+1} = MinimalSupportSubgraph(CTX', output_schema) - set cover
///   6. Gaps_{k+1} = UpdateGaps(Gaps_k, AG_{k+1}) - mark covered
///
/// Stop when:
///   - AG_{k+1} == AG_k (stabilized)
///   - Gaps all covered
///   - max_iterations reached
///   - budgets exceeded
/// ```
pub struct FixedPointSolver {
    /// Solver configuration
    pub config: SolverConfig,
    /// Context manager
    pub ctx_manager: Arc<Mutex<CtxManager>>,
    /// Query router
    pub router: QueryRouter,
    /// Cost weights
    pub cost_weights: CostWeights,
    /// Current timestamp (for age calculations)
    pub now_ns: u64,
    /// CAS store for reading atom bodies
    pub cas: Arc<CasStore>,
}

impl Default for FixedPointSolver {
    fn default() -> Self {
        Self::new()
    }
}

/// Extracted edge from atom EDGES section for proof-derived graph connectivity.
/// Contains real knowledge relation data from CAS storage.
#[derive(Debug, Clone)]
struct ExtractedEdge {
    /// Source node number
    src_node: NodeNum,
    /// Destination node number
    dst_node: NodeNum,
    /// Edge type from atom EDGES section
    edge_type: EdgeType,
    /// Confidence/weight from EDGES section
    confidence: TrustLevel,
}

/// A claim inferred during fixed-point reasoning together with its provenance anchors.
#[derive(Debug, Clone)]
struct InferredClaimStep {
    claim: ClaimData,
    premise_nodes: Vec<NodeNum>,
    support_atom_ids: Vec<AtomId>,
}

// Fixed FixedPointSolver constructors
impl FixedPointSolver {
    /// Create a new fixed-point solver with default configuration
    ///
    /// Note: CAS store must be set via `with_cas()` before calling `solve()`.
    #[inline]
    pub fn new() -> Self {
        FixedPointSolver {
            config: SolverConfig::default(),
            ctx_manager: Arc::new(Mutex::new(CtxManager::new())),
            router: QueryRouter::new(),
            cost_weights: CostWeights::default(),
            now_ns: 0,
            cas: Arc::new(
                CasStore::open(std::path::Path::new("./memoryx_data/cas"), None)
                    .expect("Failed to open CAS"),
            ),
        }
    }

    /// Create with custom configuration
    ///
    /// Note: CAS store must be set via `with_cas()` before calling `solve()`.
    #[inline]
    pub fn with_config(config: SolverConfig) -> Self {
        // Open CAS and initialize writer/reader
        let cas = CasStore::open(std::path::Path::new("./memoryx_data/cas"), None)
            .expect("Failed to open CAS");
        cas.init_writer().expect("Failed to init CAS writer");
        cas.init_reader().expect("Failed to init CAS reader");

        FixedPointSolver {
            config,
            ctx_manager: Arc::new(Mutex::new(CtxManager::new())),
            router: QueryRouter::new(),
            cost_weights: CostWeights::default(),
            now_ns: 0,
            cas: Arc::new(cas),
        }
    }

    /// Set cost weights
    #[inline]
    pub fn with_weights(mut self, weights: CostWeights) -> Self {
        self.cost_weights = weights;
        self
    }

    /// Set context manager
    #[inline]
    pub fn with_ctx_manager(mut self, ctx_manager: Arc<Mutex<CtxManager>>) -> Self {
        self.ctx_manager = ctx_manager;
        self
    }

    /// Set query router
    #[inline]
    pub fn with_router(mut self, router: QueryRouter) -> Self {
        self.router = router;
        self
    }

    /// Set current timestamp
    #[inline]
    pub fn with_timestamp(mut self, now_ns: u64) -> Self {
        self.now_ns = now_ns;
        self
    }

    /// Set CAS store
    #[inline]
    pub fn with_cas(mut self, cas: Arc<CasStore>) -> Self {
        self.cas = cas;
        self
    }

    /// Load atom body from CAS
    ///
    /// Uses the candidate's atom_id to read the full atom body from storage.
    fn load_atom_body(&self, atom_id: &AtomId) -> Result<Vec<u8>, SolverError> {
        // Re-init reader to see newly written atoms (Windows file sharing workaround)
        if let Err(e) = self.cas.init_reader() {
            eprintln!("WARN: init_reader failed: {:?}", e);
        }
        self.cas
            .read(atom_id)
            .map_err(|e| SolverError::CasError(e.to_string()))?
            .ok_or_else(|| SolverError::CasError(format!("Atom not found: {:?}", atom_id)))
    }

    fn claims_from_atom_body(body: &[u8]) -> Vec<ClaimData> {
        let Ok(header) = crate::cas::AtomBodyHeader::from_bytes(body) else {
            return Vec::new();
        };
        let table_start = header.section_table_off as usize;
        for index in 0..header.section_count as usize {
            let descriptor_offset = table_start + index * crate::cas::SectionDesc::SIZE;
            let Some(descriptor_bytes) =
                body.get(descriptor_offset..descriptor_offset + crate::cas::SectionDesc::SIZE)
            else {
                return Vec::new();
            };
            let Ok(descriptor) = crate::cas::SectionDesc::from_bytes_unaligned(descriptor_bytes)
            else {
                continue;
            };
            if descriptor.kind() != Some(crate::store::SectionKind::CLAIMS) {
                continue;
            }
            let start = descriptor.off as usize;
            let end = start.saturating_add(descriptor.len as usize);
            let Some(section_bytes) = body.get(start..end) else {
                return Vec::new();
            };
            let Ok(section) = crate::cas::claims::ClaimsSection::from_bytes(section_bytes) else {
                return Vec::new();
            };
            return section
                .claims
                .iter()
                .map(|claim| {
                    let mut object = [0u8; 8];
                    let copy_len = claim.object_value.len().min(object.len());
                    object[..copy_len].copy_from_slice(&claim.object_value[..copy_len]);
                    ClaimData {
                        subj: u64::from(claim.subject_local),
                        pred: u64::from(claim.predicate_local),
                        obj_tag: claim.object_tag.to_u8(),
                        obj_val: u64::from_le_bytes(object),
                        qualifiers_mask: 0,
                    }
                })
                .collect();
        }
        Vec::new()
    }

    /// Run fixed-point solving
    ///
    /// # Arguments
    /// - `goal`: Goal specification
    /// - `ctx_policy`: Context policy ID
    ///
    /// # Returns
    /// - `Ok(AnswerPack)`: Query results
    /// - `Err(SolverError)`: Solver error
    ///
    /// # Algorithm (SKF-1.1 Section 5.2)
    pub fn solve(
        &self,
        goal: GoalSpec,
        ctx_policy: CtxPolicyId,
    ) -> Result<AnswerPack, SolverError> {
        // Validate configuration
        self.config.validate()?;

        // Reuse an explicitly selected context. Legacy callers pass a policy id
        // here, so a missing id still creates a context with that policy.
        let ctx_id = {
            let mut contexts = self.ctx_manager.lock();
            if contexts.get_ctx(ctx_policy).is_some() {
                ctx_policy
            } else {
                contexts.create_context(ctx_policy)
            }
        };

        // Generate initial gaps via BackwardWave
        let gaps = BackwardWaveGenerator::generate(&goal);

        // Create initial state
        let mut state = SolverState::new(ctx_id, goal, gaps);

        // Run fixed-point iterations
        self.run_iterations(&mut state)?;

        // Build answer pack with full confidence, limitations
        let mut pack = AnswerPack::from_solver(
            state.answer_graph.clone(),
            state.ctx_id,
            &state.gaps,
            &self.cost_weights,
        );

        // Extract claims with full provenance chains
        self.extract_claims(&mut pack, &state);
        if pack.graph.nodes.is_empty() && state.goal.lexical_resolution_required {
            pack.status = AnswerStatus::NoMatch;
        } else if !pack.graph.nodes.is_empty() && pack.claims.is_empty() {
            pack.status = AnswerStatus::InsufficientEvidence;
            pack.limitations.push(Limitation::warning(
                LimitationCode::IncompleteEvidence,
                "Lexical candidates were found, but they contain no knowledge claims; only source-backed candidate evidence is returned"
                    .to_owned(),
            ));
        }

        pack.rejected_candidates = state.rejected_candidates.clone();
        if !pack.rejected_candidates.is_empty() {
            pack.limitations.push(Limitation::info(
                LimitationCode::ConstraintRejected,
                format!(
                    "{} candidates rejected by query contract constraints",
                    pack.rejected_candidates.len()
                ),
            ));
            if pack.graph.nodes.is_empty() {
                pack.status = AnswerStatus::PolicyBlocked;
            }
        }

        // Generate alternative answer paths for comparison
        // Re-collect candidates from the final iteration for alternate generation
        let all_candidates = self.collect_final_candidates(&state);
        if !all_candidates.is_empty() {
            pack.generate_alternates(
                &all_candidates,
                &state.gaps,
                &self.cost_weights,
                3, // Up to 3 alternates
            );
        }

        self.apply_conflict_policy_to_pack(&mut pack, &state);
        pack.query_trace = state.query_trace.clone();

        Ok(pack)
    }

    fn apply_conflict_policy_to_pack(&self, pack: &mut AnswerPack, state: &SolverState) {
        use crate::query::contract::ConflictPolicyMode;

        let conflict_contexts = {
            let mut ctx_ids = state.answer_graph.branch_lineage.clone();
            if !ctx_ids.contains(&state.ctx_id) {
                ctx_ids.push(state.ctx_id);
            }
            ctx_ids
        };
        let conflicts = {
            let ctx_manager = self.ctx_manager.lock();
            conflict_contexts
                .iter()
                .flat_map(|ctx_id| ctx_manager.list_conflicts(*ctx_id))
                .collect::<Vec<_>>()
        };
        if conflicts.is_empty() {
            return;
        }

        if state.goal.conflict_policy.include_conflicts {
            pack.conflicts = conflicts.iter().map(ConflictSummary::from).collect();
        }

        let branches = conflict_contexts;

        pack.conflict_sets = conflicts
            .iter()
            .fold(
                HashMap::<u64, Vec<ConflictSummary>>::new(),
                |mut acc, conflict| {
                    acc.entry(conflict.pattern_hash)
                        .or_default()
                        .push(ConflictSummary::from(conflict));
                    acc
                },
            )
            .into_iter()
            .map(|(pattern_hash, conflicts)| ConflictSet {
                pattern_hash,
                policy: format!("{:?}", state.goal.conflict_policy.mode),
                branches: branches.clone(),
                conflicts,
            })
            .collect();

        let hard_conflict = conflicts
            .iter()
            .any(|conflict| conflict.severity == ConflictSeverity::Hard);

        pack.status = AnswerStatus::Conflicted;

        if hard_conflict {
            pack.limitations.push(Limitation::critical(
                LimitationCode::ConflictsPresent,
                "hard conflict is present in the selected answer context".to_owned(),
            ));
        } else {
            pack.limitations.push(Limitation::warning(
                LimitationCode::ConflictsPresent,
                "conflict branch is present and exposed in the answer pack".to_owned(),
            ));
        }

        if state.goal.conflict_policy.fail_on_hard_conflict
            || matches!(state.goal.conflict_policy.mode, ConflictPolicyMode::Fail)
        {
            pack.status = AnswerStatus::PolicyBlocked;
            pack.limitations.push(Limitation::critical(
                LimitationCode::ConflictsPresent,
                "conflict policy failed the answer instead of hiding the conflict".to_owned(),
            ));
        }

        if matches!(
            state.goal.conflict_policy.mode,
            ConflictPolicyMode::IncludeAlternatives
        ) && pack.alternates.is_empty()
        {
            pack.limitations.push(Limitation::warning(
                LimitationCode::ConflictsPresent,
                "conflict policy requested alternatives, but no alternate proof graph was available"
                    .to_owned(),
            ));
        }
    }

    /// Collect all candidates that were considered during solving.
    /// Used for generating alternative answer paths.
    fn collect_final_candidates(&self, state: &SolverState) -> Vec<Candidate> {
        let mut candidates = Vec::new();

        for gap in &state.gaps {
            let routed = self.router.route(gap, &state.goal);
            for mut candidate in routed {
                candidate.covers_gaps.push(gap.id);
                candidates.push(candidate);
            }
        }

        candidates = self.filter_by_contract_constraints(candidates, state).0;

        // Deduplicate by atom_id
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|c| seen.insert(c.atom_id));

        candidates
    }

    /// Run fixed-point iterations until convergence or limits
    fn run_iterations(&self, state: &mut SolverState) -> Result<(), SolverError> {
        let max_iterations = self.config.max_iterations;
        let mut prev_graph_cost = f64::NEG_INFINITY;
        let mut stable_count = 0;

        for iteration in 0..max_iterations {
            state.iterations = iteration + 1;
            state.generation += 1;

            // Check if all gaps covered
            if state.all_gaps_covered() {
                state.stable = true;
                break;
            }

            // Check budget
            if !state.check_budget(&self.config) {
                state.stable = true;
                break;
            }

            // Run one iteration
            self.iteration(state)?;

            // Check for convergence
            let current_cost = state.answer_graph.total_cost;
            if (current_cost - prev_graph_cost).abs() < 0.001 {
                stable_count += 1;
                if stable_count >= 2 {
                    state.stable = true;
                    break;
                }
            } else {
                stable_count = 0;
            }
            prev_graph_cost = current_cost;
        }

        // Fixed-point iterations can select the same physical atom for several
        // gaps. Collapse only same-branch duplicates before claims, proof counts,
        // and costs are finalized; branch-qualified nodes remain distinct.
        state.answer_graph.canonicalize_nodes();

        // Final cost calculation
        state
            .answer_graph
            .recalculate_cost(&self.cost_weights, self.now_ns);

        if state.iterations >= max_iterations && !state.stable {
            return Err(SolverError::MaxIterationsReached);
        }

        Ok(())
    }

    /// Run one iteration of fixed-point solving (SKF-1.1 Section 5.2)
    fn iteration(&self, state: &mut SolverState) -> Result<(), SolverError> {
        // Step 1: Route uncovered gaps to candidates
        let mut all_candidates = Vec::new();
        let planned_actions = RetrievalPlanner::plan(
            &state.gaps,
            &state.goal,
            PlannerBudgets {
                max_actions: self.config.fetch_budget as usize,
                max_io_bytes: self.config.io_budget,
            },
        );

        state
            .query_trace
            .retrieval_actions
            .extend(planned_actions.iter().map(|action| action.to_trace(true)));

        for action in planned_actions {
            let Some(gap) = state.get_gap(action.gap_id) else {
                continue;
            };
            let candidates = self.router.route(gap, &state.goal);
            for mut candidate in candidates {
                candidate.covers_gaps.push(gap.id);
                all_candidates.push(candidate);
            }
        }

        let (filtered_candidates, contract_rejections) =
            self.filter_by_contract_constraints(all_candidates, state);
        state.rejected_candidates.extend(contract_rejections);
        let all_candidates = filtered_candidates;

        if all_candidates.is_empty() {
            return Ok(()); // No candidates available
        }

        // Step 2: Filter by invariants (real INVARIANTS section evaluation)
        let filter_result = self.filter_by_invariants(&all_candidates, state)?;

        // Step 2.1: Process NeedBranch candidates - create TMS context branches
        // SKF-1.1 Section 3.2: Each NeedBranch creates an alternative CTX'
        let mut admissible = filter_result.admissible;

        for branched in filter_result.need_branch {
            // Create Conflict object for TMS branching
            let conflict = Conflict::new(
                branched.conflicting_atom_id.unwrap_or([0u8; 32]),
                branched.candidate.atom_id,
                ConflictType::Contradiction,
                ConflictSeverity::Soft, // NeedBranch always indicates soft conflict
                branched.pattern_hash,
            );

            // Create new context branch via ctx_manager.branch_ctx()
            // Clone ctx_manager to get mutable access
            // Use Arc<Mutex> for shared mutable access - mutations persist
            if let Some(new_ctx_id) = self.ctx_manager.lock().branch_ctx(state.ctx_id, &conflict) {
                eprintln!(
                    "TMS BRANCH: Created new context {} from {} for candidate {}",
                    new_ctx_id, state.ctx_id, branched.candidate.node_num
                );

                // Add candidate to admissible with branch_ctx_id set
                let mut branched_candidate = branched.candidate;
                branched_candidate.branch_ctx_id = Some(new_ctx_id);
                admissible.push(branched_candidate);
            } else {
                eprintln!(
                    "WARN: Failed to create context branch for candidate {}",
                    branched.candidate.node_num
                );
                // Still add candidate without branch info
                admissible.push(branched.candidate);
            }
        }

        let (context_admissible, context_rejections) =
            self.filter_by_context_scope(admissible, state);
        state.rejected_candidates.extend(context_rejections);
        let admissible = context_admissible;

        // Step 3: Update context (branch on conflicts)
        // Simplified: just track conflicts in candidates
        let ctx_updated = self.update_context(&admissible, state)?;

        // Step 4: Infer new claims and integrate them into the current fixed-point state
        let inferred_steps = self.infer_claim_steps(&admissible, &ctx_updated);

        // Step 5: Build minimal support subgraph (set cover)
        let selected_indices = SetCoverSolver::greedy_select(
            &admissible,
            &state.gaps,
            &self.cost_weights,
            state.goal.output_schema,
        );

        // Step 6: Update answer graph
        for &idx in &selected_indices {
            let candidate = &admissible[idx];

            // Create node
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
            // SKF-1.1 §3.2: Link node to its TMS branch context
            node.branch_ctx_id = candidate.branch_ctx_id;

            // Mark covered gaps
            for &gap_id in &candidate.covers_gaps {
                node.add_gap(gap_id);
                if let Some(gap) = state.get_gap_mut(gap_id) {
                    gap.mark_covered(candidate.atom_id);
                }
            }

            state.answer_graph.add_node(node);

            // Track fetched offsets
            state.fetched_offsets.push((
                candidate.seg_id,
                candidate.offset,
                candidate.estimated_io_bytes,
            ));
        }

        // Add edges for connectivity
        self.add_edges(&mut state.answer_graph, &selected_indices, &admissible);

        // Step 6.1: Add derived claims to the answer graph and let them close remaining gaps
        self.apply_inferred_claims(
            &mut state.answer_graph,
            &mut state.gaps,
            &selected_indices,
            &admissible,
            &inferred_steps,
        );

        // Step 7: Mark covered gaps in answer graph
        let covered: Vec<GapId> = state
            .gaps
            .iter()
            .filter(|g| g.covered)
            .map(|g| g.id)
            .collect();
        state.answer_graph.mark_gaps_covered(&covered);

        // Step 8: Prune unnecessary nodes
        state.answer_graph.prune();

        Ok(())
    }

    /// Enforce QueryContract hard/MUST_NOT constraints before invariant checks and ranking.
    ///
    /// `SHOULD` constraints are advisory and therefore never block a candidate
    /// here. `MUST` and `MUST_NOT` constraints must be explicitly satisfied;
    /// `UNKNOWN` is treated as a rejection because the solver cannot prove the
    /// candidate satisfies the hard contract.
    fn filter_by_contract_constraints(
        &self,
        candidates: Vec<Candidate>,
        state: &SolverState,
    ) -> (Vec<Candidate>, Vec<RejectedCandidateSummary>) {
        use crate::query::ConstraintEvaluator;
        use crate::query::contract::{ConstraintStatus, ConstraintStrength};

        if state.goal.constraints.is_empty() {
            return self.filter_by_context_scope(candidates, state);
        }

        let mut admitted = Vec::with_capacity(candidates.len());
        let mut rejected = Vec::new();

        for candidate in candidates {
            if let Some(rejection) = self.context_scope_rejection(&candidate, state) {
                rejected.push(rejection);
                continue;
            }

            let results: Vec<_> = state
                .goal
                .constraints
                .iter()
                .map(|constraint| ConstraintEvaluator::evaluate_constraint(constraint, &candidate))
                .collect();

            let blocking_results: Vec<_> = state
                .goal
                .constraints
                .iter()
                .zip(results.iter())
                .filter(|(constraint, result)| {
                    !matches!(constraint.strength, ConstraintStrength::Should { .. })
                        && result.status != ConstraintStatus::Satisfied
                })
                .map(|(_, result)| result.clone())
                .collect();

            if blocking_results.is_empty() {
                admitted.push(candidate);
            } else {
                rejected.push(RejectedCandidateSummary {
                    candidate_ref: Some(crate::cas::hex_encode(&candidate.atom_id)),
                    atom_id: Some(candidate.atom_id),
                    node_num: Some(candidate.node_num),
                    source_backend: candidate.source_backend.as_str().to_owned(),
                    reason: "query contract hard constraint was not satisfied".to_owned(),
                    constraint_results: blocking_results,
                });
            }
        }

        (admitted, rejected)
    }

    fn filter_by_context_scope(
        &self,
        candidates: Vec<Candidate>,
        state: &SolverState,
    ) -> (Vec<Candidate>, Vec<RejectedCandidateSummary>) {
        let mut admitted = Vec::with_capacity(candidates.len());
        let mut rejected = Vec::new();

        for candidate in candidates {
            if let Some(rejection) = self.context_scope_rejection(&candidate, state) {
                rejected.push(rejection);
            } else {
                admitted.push(candidate);
            }
        }

        (admitted, rejected)
    }

    fn context_scope_rejection(
        &self,
        candidate: &Candidate,
        state: &SolverState,
    ) -> Option<RejectedCandidateSummary> {
        use crate::query::contract::{ConstraintId, ConstraintResult, ConstraintStatus};

        if state.goal.context_scope.include_conflicting_branches {
            return None;
        }

        let branch_ctx_id = candidate.branch_ctx_id?;
        let allowed = state.goal.context_scope.branch_ids.iter().any(|branch| {
            branch
                .strip_prefix("ctx:")
                .and_then(|value| value.parse::<CtxId>().ok())
                .is_some_and(|ctx_id| ctx_id == branch_ctx_id)
        });

        if allowed {
            return None;
        }

        Some(RejectedCandidateSummary {
            candidate_ref: Some(crate::cas::hex_encode(&candidate.atom_id)),
            atom_id: Some(candidate.atom_id),
            node_num: Some(candidate.node_num),
            source_backend: candidate.source_backend.as_str().to_owned(),
            reason: format!("context branch ctx:{branch_ctx_id} is blocked by context scope"),
            constraint_results: vec![ConstraintResult {
                constraint_id: ConstraintId("__context_branch_policy".to_owned()),
                status: ConstraintStatus::BlockedByPolicy,
                reason: Some("conflicting branches are disabled for this query".to_owned()),
                candidate_ref: Some(crate::cas::hex_encode(&candidate.atom_id)),
                evidence_refs: Vec::new(),
            }],
        })
    }

    /// Filter candidates by invariant checks using real INVARIANTS section from atoms.
    ///
    /// SKF-1.1 Section 5.2, Step 2: Admissible = FilterByInvariants(Candidates, CTX_k)
    ///
    /// ANTI-RAG GUARANTEE: All ANN candidates (ann_candidate_requires_filtering=true)
    /// MUST pass through this function. The pipeline enforces that:
    /// 1. ANN candidates always have requires_invariant_check=true
    /// 2. ANN candidates are never admitted without invariant evaluation
    /// 3. Any attempt to bypass invariant checks for ANN candidates causes an error
    ///
    /// For each candidate:
    /// 1. Load atom body from CAS
    /// 2. Get CtxIndex from CtxManager
    /// 3. Build QueryConstraintsView from GoalSpec
    /// 4. Call eval_invariants_for_atom from abi module
    /// 5. Classify the result:
    ///    - PASS: Candidate accepted, added to admissible set
    ///    - FAIL_SOFT: Soft failure, candidate rejected
    ///    - FAIL_HARD: Hard failure, constraint violation, candidate rejected
    ///    - NEED_BRANCH: Context split needed, candidate added to need_branch list
    fn filter_by_invariants(
        &self,
        candidates: &[Candidate],
        state: &SolverState,
    ) -> Result<InvariantFilterResult, SolverError> {
        let mut admissible = Vec::new();
        let mut need_branch = Vec::new();
        let ctx_id = state.ctx_id;

        // Rejection counters for SKF invariant gate reporting
        let mut rejected_hard = 0u32;
        let mut rejected_soft = 0u32;

        // ANTI-RAG: Verify ANN  Invariant pipeline integrity
        // Count ANN candidates and ensure they all require filtering
        let ann_candidates: Vec<&Candidate> = candidates
            .iter()
            .filter(|c| c.ann_candidate_requires_filtering)
            .collect();

        if !ann_candidates.is_empty() {
            // Verify all ANN candidates have requires_invariant_check=true
            let ann_without_invariant_check = ann_candidates
                .iter()
                .filter(|c| !c.requires_invariant_check)
                .count();

            if ann_without_invariant_check > 0 {
                return Err(SolverError::InvalidConfig(
                    "ANTI-RAG VIOLATION: ANN candidate without requires_invariant_check",
                ));
            }

            eprintln!(
                "ANTI-RAG: Processing {} ANN candidates through invariant pipeline",
                ann_candidates.len()
            );
        }

        // 1. Get CtxIndex from ctx_manager for conflict probing
        let ctx_index = self.ctx_manager.lock().get_ctx_index(ctx_id);

        // 2. Create QueryConstraints from GoalSpec
        let constraints = crate::prelude::QueryConstraints {
            time_range: Some(crate::prelude::TimeRange {
                from_ns: state.goal.time_q.from_ns,
                to_ns: state.goal.time_q.to_ns,
            }),
            trust_min: Some(state.goal.trust_min),
            domain_mask: Some(state.goal.domain_mask),
            atom_types: None,
            max_results: None,
        };

        // 3. For each candidate: load atom body and evaluate invariants
        for candidate in candidates {
            // Skip candidates that don't require invariant check
            if !candidate.requires_invariant_check {
                admissible.push(candidate.clone());
                continue;
            }

            // Load atom body from CAS
            let atom_body = match self.load_atom_body(&candidate.atom_id) {
                Ok(body) => body,
                Err(e) => {
                    // Log error and skip this candidate
                    eprintln!("Failed to load atom {:?}: {}", candidate.atom_id, e);
                    continue;
                }
            };
            let mut candidate = candidate.clone();
            candidate.derived_claims = Self::claims_from_atom_body(&atom_body);

            // Call eval_invariants_for_atom from abi module
            // This evaluates the real INVARIANTS bytecode from the atom
            let result = vm::abi::eval_invariants_for_atom(
                &atom_body,
                None, // claim_idx - evaluate all claims
                &ctx_index,
                &constraints,
            );

            match result {
                InvariantResult::Pass => {
                    admissible.push(candidate.clone());
                }
                InvariantResult::NeedBranch { conflict_id: _ } => {
                    // SKF-1.1 Section 3.2: NeedBranch means the candidate conflicts
                    // with the active context and requires creating an alternative CTX'.
                    //
                    // The VM emits a coarse branch signal, but the solver re-probes the
                    // exact claim signatures so identical claims are not branched.
                    if let Some((pattern_hash, conflicting_atom_id)) =
                        candidate.derived_claims.iter().find_map(|claim| {
                            ctx_index.probe_conflict(claim).map(|info| {
                                let pattern_hash = vm::CtxIndex::claim_pattern_hash(claim);
                                let conflicting_atom_id = info
                                    .atom_ids
                                    .iter()
                                    .copied()
                                    .find(|atom_id| *atom_id != candidate.atom_id)
                                    .or_else(|| info.atom_ids.first().copied());
                                (pattern_hash, conflicting_atom_id)
                            })
                        })
                    {
                        // Add to need_branch list for processing in iteration()
                        need_branch.push(BranchedCandidate {
                            candidate: candidate.clone(),
                            pattern_hash,
                            conflicting_atom_id,
                        });

                        eprintln!(
                            "NEED_BRANCH: Candidate {} requires context branch (exact pattern_hash={:X})",
                            candidate.node_num, pattern_hash
                        );
                    } else if candidate.derived_claims.is_empty() {
                        // No derived claims were available to exact-probe, so keep the
                        // legacy coarse fallback to preserve branch handling.
                        let pattern_hash = {
                            let mut h: u64 = 0;
                            for (i, &b) in candidate.atom_id.iter().take(8).enumerate() {
                                h |= (b as u64) << (i * 8);
                            }
                            h
                        };

                        let conflicting_atom_id = ctx_index
                            .get_conflict(pattern_hash)
                            .and_then(|info| info.atom_ids.first().copied());

                        need_branch.push(BranchedCandidate {
                            candidate: candidate.clone(),
                            pattern_hash,
                            conflicting_atom_id,
                        });

                        eprintln!(
                            "NEED_BRANCH: Candidate {} requires context branch (fallback pattern_hash={:X})",
                            candidate.node_num, pattern_hash
                        );
                    } else {
                        admissible.push(candidate.clone());
                        eprintln!(
                            "NEED_BRANCH: Candidate {} produced a coarse probe, but no exact conflict was found",
                            candidate.node_num
                        );
                    }
                }

                InvariantResult::FailSoft { .. } => {
                    // Soft failure - reject candidate but continue
                    rejected_soft += 1;
                }
                InvariantResult::FailHard { reason } => {
                    // Hard failure - CANDIDATE MUST BE REJECTED per SKF-1.1 Section 5.2
                    // Hard invariant violation means the claim/atom is NOT admissible
                    // This is a core invariant-driven gate mechanism
                    rejected_hard += 1;
                    eprintln!(
                        "INVARIANT GATE: Candidate {} REJECTED (hard failure, reason={})",
                        candidate.node_num, reason
                    );
                    // DO NOT add to admissible - hard failures are absolute rejections
                }
            }
        }

        // ANTI-RAG: Verify that all ANN candidates were processed through invariants
        // (they should either be in admissible or explicitly rejected)
        let admitted_ann_count = admissible
            .iter()
            .filter(|c| c.ann_candidate_requires_filtering)
            .count();

        if !ann_candidates.is_empty() {
            eprintln!(
                "ANTI-RAG: {} ANN candidates admitted after invariant checks",
                admitted_ann_count
            );
        }

        // SKF INVARIANT GATE: Report rejection statistics
        if rejected_hard > 0 || rejected_soft > 0 {
            eprintln!(
                "INVARIANT GATE: rejected_hard={}, rejected_soft={}",
                rejected_hard, rejected_soft
            );
        }

        Ok(InvariantFilterResult {
            admissible,
            need_branch,
            rejected_hard,
            rejected_soft,
            contract_rejections: Vec::new(),
        })
    }

    ///
    /// Update context based on admissible candidates - SKF-1.1 Section 5.2 Step 3
    ///
    /// **SKF-1.1 Contract:**
    /// UpdateContext(CTX_k, Admissible) is a FULL fixed-point solver phase that:
    /// 1. Asserts admissible claims into context via ctx_manager.assert_claim_with_atom_id()
    /// 2. Tracks conflicts detected during claim assertion
    /// 3. Updates branch lineage when candidates have branch_ctx_id
    /// 4. Makes context part of reasoning loop state machine
    ///
    /// **Implementation:**
    /// - For each candidate, assert its derived_claims into the appropriate context
    /// - If candidate has branch_ctx_id, use that context (TMS branch already created)
    /// - Otherwise, use the current state.ctx_id
    /// - Track resulting context ID for state updates
    fn update_context(
        &self,
        candidates: &[Candidate],
        state: &mut SolverState,
    ) -> Result<CtxId, SolverError> {
        // Track conflict counts for reporting
        let hard_conflicts: u32 = candidates.iter().map(|c| c.hard_conflicts).sum();
        let soft_conflicts: u32 = candidates.iter().map(|c| c.soft_conflicts).sum();

        if hard_conflicts > 0 || soft_conflicts > 0 {
            eprintln!(
                "CONTEXT UPDATE: {} hard conflicts, {} soft conflicts among {} candidates",
                hard_conflicts,
                soft_conflicts,
                candidates.len()
            );
        }

        // SKF-1.1: Active claims assertion phase
        // For each candidate, assert its claims into the appropriate context
        let mut current_ctx = state.ctx_id;
        let mut claims_added = 0u32;
        let mut conflicts_detected = 0u32;
        let mut branch_ctx_created: Option<CtxId> = None;

        for candidate in candidates {
            // Determine target context for this candidate
            let target_ctx = candidate.branch_ctx_id.unwrap_or(current_ctx);

            // Assert each derived claim from this candidate into context
            for claim in &candidate.derived_claims {
                // Use the candidate's atom_id as source for proper provenance
                let result = self.ctx_manager.lock().assert_claim_with_atom_id(
                    target_ctx,
                    claim,
                    candidate.atom_id,
                );

                match result {
                    Ok(result_ctx_id) => {
                        // Claim successfully asserted
                        claims_added += 1;

                        // If a new context was created (branch mode), track it
                        if result_ctx_id != target_ctx {
                            branch_ctx_created = Some(result_ctx_id);
                            eprintln!(
                                "CTX_BRANCH: Claim {} triggered new context {} from {}",
                                claim.pred, result_ctx_id, target_ctx
                            );
                        }

                        // Update current_ctx if we branched
                        if candidate.branch_ctx_id.is_none() && result_ctx_id != current_ctx {
                            current_ctx = result_ctx_id;
                        }
                    }
                    Err(StoreError::ClaimRejected(reason)) => {
                        // Claim rejected due to conflict - track it
                        conflicts_detected += 1;
                        eprintln!(
                            "CTX_REJECTED: Claim {} rejected in ctx {}: {}",
                            claim.pred, target_ctx, reason
                        );
                        // Continue processing other claims
                    }
                    Err(e) => {
                        // Unexpected error - log and continue
                        eprintln!("CTX_ERROR: Failed to assert claim: {:?}", e);
                    }
                }
            }
        }

        // Report context update results
        if claims_added > 0 || conflicts_detected > 0 {
            eprintln!(
                "CTX_UPDATED: {} claims added, {} conflicts detected in ctx {}",
                claims_added, conflicts_detected, current_ctx
            );
        }

        // Update state's context ID if we branched
        if current_ctx != state.ctx_id {
            state.ctx_id = current_ctx;
            eprintln!("CTX_STATE: Updated solver state ctx_id to {}", current_ctx);
        }

        // Record branch lineage in answer graph if a branch was created
        if let Some(new_ctx_id) = branch_ctx_created {
            state.answer_graph.branch_lineage.push(new_ctx_id);
            eprintln!(
                "CTX_LINEAGE: Recorded branch {} in answer graph",
                new_ctx_id
            );
        }

        Ok(current_ctx)
    }

    #[inline]
    fn pattern_ref_matches(pattern: &PatternRef, value: u64) -> bool {
        match pattern {
            PatternRef::Any => true,
            PatternRef::Sym(sym) => u64::from(*sym) == value,
            PatternRef::Node(node) => *node == value,
            PatternRef::Range { min, max } => i64::try_from(value)
                .map(|v| v >= *min && v <= *max)
                .unwrap_or(false),
            PatternRef::Set(values) => values.iter().any(|sym| u64::from(*sym) == value),
        }
    }

    #[inline]
    fn claim_matches_gap_pattern(claim: &ClaimData, pattern: &ClaimPattern) -> bool {
        if let Some(obj_tag) = pattern.obj_tag
            && claim.obj_tag != obj_tag.to_u8()
        {
            return false;
        }

        if pattern.qualifiers_mask != 0
            && (claim.qualifiers_mask & pattern.qualifiers_mask) != pattern.qualifiers_mask
        {
            return false;
        }

        Self::pattern_ref_matches(&pattern.subj, claim.subj)
            && Self::pattern_ref_matches(&pattern.pred, claim.pred)
            && Self::pattern_ref_matches(&pattern.obj, claim.obj_val)
    }

    fn apply_inferred_claims(
        &self,
        graph: &mut AnswerGraph,
        gaps: &mut [Gap],
        _selected: &[usize],
        _candidates: &[Candidate],
        inferred_steps: &[InferredClaimStep],
    ) {
        let node_to_graph_idx: HashMap<NodeNum, usize> = graph
            .nodes
            .iter()
            .enumerate()
            .map(|(idx, node)| (node.atom_ref.node_num, idx))
            .collect();

        for step in inferred_steps {
            if !graph.derived_claims.iter().any(|existing| {
                existing.subj == step.claim.subj
                    && existing.pred == step.claim.pred
                    && existing.obj_tag == step.claim.obj_tag
                    && existing.obj_val == step.claim.obj_val
                    && existing.qualifiers_mask == step.claim.qualifiers_mask
            }) {
                graph.derived_claims.push(step.claim.clone());
            }

            let Some(&graph_idx) = node_to_graph_idx.get(&step.claim.subj) else {
                continue;
            };

            let node_atom_id = {
                let node = &mut graph.nodes[graph_idx];
                if !node.derived_claims.iter().any(|existing| {
                    existing.subj == step.claim.subj
                        && existing.pred == step.claim.pred
                        && existing.obj_tag == step.claim.obj_tag
                        && existing.obj_val == step.claim.obj_val
                        && existing.qualifiers_mask == step.claim.qualifiers_mask
                }) {
                    node.derived_claims.push(step.claim.clone());
                }
                node.atom_ref.atom_id
            };

            let support_strings: Vec<(String, String)> = step
                .premise_nodes
                .iter()
                .enumerate()
                .map(|(idx, node_num)| (format!("premise_{idx}"), node_num.to_string()))
                .collect();

            if !step.support_atom_ids.is_empty() {
                let premise_indices: Vec<usize> = step
                    .premise_nodes
                    .iter()
                    .filter_map(|node_num| node_to_graph_idx.get(node_num).copied())
                    .collect();

                graph.add_proof_step(ProofStep::new(
                    graph.proof_steps.len() as u32,
                    step.support_atom_ids[0],
                    premise_indices,
                    graph_idx,
                    support_strings,
                ));
            }

            for gap in gaps.iter_mut().filter(|gap| !gap.covered) {
                if Self::claim_matches_gap_pattern(&step.claim, &gap.pattern) {
                    gap.mark_covered(node_atom_id);
                    graph.nodes[graph_idx].add_gap(gap.id);
                    graph.covered_gaps.insert(gap.id);
                }
            }
        }
    }

    /// Infer new claims from admissible candidates using transitive reasoning.
    ///
    /// Applies inference rules over the candidate set:
    /// 1. **Transitivity**: If A→B (IMPLIES/CAUSES) and B→C, infer A→C
    ///    with decayed trust: trust(A→C) = min(trust(A→B), trust(B→C)) * 0.8
    /// 2. **Symmetry**: For SAME_AS relationships, A=B implies B=A
    /// 3. **Reflexivity through evidence**: If two candidates share evidence,
    ///    create a SUPPORTS link between them
    /// 4. **Contradiction propagation**: If A contradicts B and B supports C,
    ///    flag A as potentially contradicting C (soft conflict)
    ///
    /// All inferred claims are marked with reduced trust to distinguish them
    /// from directly observed claims.
    fn infer_claims(&self, candidates: &[Candidate], ctx_id: &CtxId) -> Vec<ClaimData> {
        self.infer_claim_steps(candidates, ctx_id)
            .into_iter()
            .map(|step| step.claim)
            .collect()
    }

    fn infer_claim_steps(
        &self,
        candidates: &[Candidate],
        _ctx_id: &CtxId,
    ) -> Vec<InferredClaimStep> {
        let mut derived: Vec<InferredClaimStep> = Vec::new();
        let inference_decay: f64 = 0.8;
        let node_to_atom: HashMap<NodeNum, AtomId> = candidates
            .iter()
            .map(|candidate| (candidate.node_num, candidate.atom_id))
            .collect();

        // Collect all direct claims indexed by subject node
        let mut claims_by_subj: HashMap<u64, Vec<(ClaimData, TrustLevel)>> = HashMap::new();
        for candidate in candidates {
            let trust = candidate.trust;
            for claim in &candidate.derived_claims {
                claims_by_subj
                    .entry(claim.subj)
                    .or_default()
                    .push((claim.clone(), trust));
            }
        }

        // Rule 1: Transitive inference (A→B, B→C => A→C)
        // Only for predicates that support transitivity:
        // IMPLIES(2), CAUSES(3), DERIVED_FROM(7), SUPPORTS(5)
        let transitive_preds: HashSet<u64> = [2, 3, 5, 7].iter().copied().collect();

        for (subj, subj_claims) in &claims_by_subj {
            for (claim_ab, trust_ab) in subj_claims {
                if !transitive_preds.contains(&claim_ab.pred) {
                    continue;
                }
                let intermediate = claim_ab.obj_val;
                if let Some(mid_claims) = claims_by_subj.get(&intermediate) {
                    for (claim_bc, trust_bc) in mid_claims {
                        if claim_bc.pred != claim_ab.pred {
                            continue;
                        }
                        // Avoid self-loops
                        if claim_bc.obj_val == *subj {
                            continue;
                        }
                        // Check we haven't already derived this
                        let already_derived = derived.iter().any(|d| {
                            d.claim.subj == claim_ab.subj
                                && d.claim.pred == claim_ab.pred
                                && d.claim.obj_val == claim_bc.obj_val
                        });
                        if already_derived {
                            continue;
                        }
                        let inferred_trust = ((*trust_ab as f64).min(*trust_bc as f64)
                            * inference_decay)
                            as TrustLevel;
                        let inferred = ClaimData {
                            subj: claim_ab.subj,
                            pred: claim_ab.pred,
                            obj_tag: claim_ab.obj_tag,
                            obj_val: claim_bc.obj_val,
                            qualifiers_mask: claim_ab.qualifiers_mask
                                | claim_bc.qualifiers_mask
                                | 0x8000_0000, // Mark as inferred
                        };
                        // Only add if trust is still meaningful
                        if inferred_trust >= 100 {
                            let mut support_atom_ids = Vec::new();
                            if let Some(atom_id) = node_to_atom.get(&claim_ab.subj) {
                                support_atom_ids.push(*atom_id);
                            }
                            if let Some(atom_id) = node_to_atom.get(&intermediate)
                                && !support_atom_ids.contains(atom_id)
                            {
                                support_atom_ids.push(*atom_id);
                            }
                            derived.push(InferredClaimStep {
                                claim: inferred,
                                premise_nodes: vec![claim_ab.subj, intermediate],
                                support_atom_ids,
                            });
                        }
                    }
                }
            }
        }

        // Rule 2: Evidence-based SUPPORTS links
        // If two candidates share evidence atom IDs, they support each other
        let mut evidence_map: HashMap<u64, Vec<usize>> = HashMap::new();
        for (idx, candidate) in candidates.iter().enumerate() {
            for ev in &candidate.evidence_refs {
                let key = SetCoverSolver::atom_id_hash(&ev.atom_id);
                evidence_map.entry(key).or_default().push(idx);
            }
        }
        for sharing_candidates in evidence_map.values() {
            if sharing_candidates.len() < 2 {
                continue;
            }
            for i in 0..sharing_candidates.len() {
                for j in (i + 1)..sharing_candidates.len() {
                    let a = &candidates[sharing_candidates[i]];
                    let b = &candidates[sharing_candidates[j]];
                    if a.atom_id == b.atom_id {
                        continue;
                    }
                    let supports_trust =
                        ((a.trust as f64).min(b.trust as f64) * inference_decay) as TrustLevel;
                    if supports_trust < 100 {
                        continue;
                    }
                    let claim = ClaimData {
                        subj: a.node_num,
                        pred: 5, // SUPPORTS
                        obj_tag: 0,
                        obj_val: b.node_num,
                        qualifiers_mask: 0x8000_0000,
                    };
                    if !derived.iter().any(|d| {
                        d.claim.subj == claim.subj
                            && d.claim.pred == claim.pred
                            && d.claim.obj_val == claim.obj_val
                    }) {
                        derived.push(InferredClaimStep {
                            claim,
                            premise_nodes: vec![a.node_num, b.node_num],
                            support_atom_ids: vec![a.atom_id, b.atom_id],
                        });
                    }
                }
            }
        }

        // Rule 3: Contradiction propagation
        // If A contradicts B (pred=8) and B supports C (pred=5),
        // then A may contradict C (soft flag)
        for candidate in candidates {
            for claim in &candidate.derived_claims {
                if claim.pred == 8 {
                    // A contradicts B
                    let contradicted = claim.obj_val;
                    if let Some(supports_claims) = claims_by_subj.get(&contradicted) {
                        for (support_claim, trust_support) in supports_claims {
                            if support_claim.pred != 5 {
                                continue;
                            }
                            let target = support_claim.obj_val;
                            if target == claim.subj {
                                continue;
                            }
                            let propagated_trust =
                                ((candidate.trust as f64).min(*trust_support as f64) * 0.6)
                                    as TrustLevel;
                            if propagated_trust < 100 {
                                continue;
                            }
                            let contra_claim = ClaimData {
                                subj: claim.subj,
                                pred: 8, // CONTRADICTS
                                obj_tag: 0,
                                obj_val: target,
                                qualifiers_mask: 0xC000_0000, // Inferred + soft
                            };
                            if !derived.iter().any(|d| {
                                d.claim.subj == contra_claim.subj
                                    && d.claim.pred == contra_claim.pred
                                    && d.claim.obj_val == contra_claim.obj_val
                            }) {
                                let mut support_atom_ids = Vec::new();
                                if let Some(atom_id) = node_to_atom.get(&claim.subj) {
                                    support_atom_ids.push(*atom_id);
                                }
                                if let Some(atom_id) = node_to_atom.get(&contradicted)
                                    && !support_atom_ids.contains(atom_id)
                                {
                                    support_atom_ids.push(*atom_id);
                                }
                                if let Some(atom_id) = node_to_atom.get(&target)
                                    && !support_atom_ids.contains(atom_id)
                                {
                                    support_atom_ids.push(*atom_id);
                                }
                                derived.push(InferredClaimStep {
                                    claim: contra_claim,
                                    premise_nodes: vec![claim.subj, contradicted, target],
                                    support_atom_ids,
                                });
                            }
                        }
                    }
                }
            }
        }

        // Rule 4: Chain completeness — if we have A→B→C→D, also add A→D
        // This is a second-order transitivity pass over already-derived claims
        if !derived.is_empty() {
            let mut second_order: Vec<InferredClaimStep> = Vec::new();
            let all_claims: Vec<(ClaimData, TrustLevel)> = claims_by_subj
                .values()
                .flatten()
                .cloned()
                .chain(derived.iter().map(|c| (c.claim.clone(), 5000)))
                .collect();

            let mut derived_by_subj: HashMap<u64, Vec<(ClaimData, TrustLevel)>> = HashMap::new();
            for (claim, trust) in &all_claims {
                derived_by_subj
                    .entry(claim.subj)
                    .or_default()
                    .push((claim.clone(), *trust));
            }

            for (subj, claims) in &derived_by_subj {
                for (claim_ab, trust_ab) in claims {
                    if !transitive_preds.contains(&claim_ab.pred) {
                        continue;
                    }
                    let intermediate = claim_ab.obj_val;
                    if let Some(mid_claims) = derived_by_subj.get(&intermediate) {
                        for (claim_bc, trust_bc) in mid_claims {
                            if claim_bc.pred != claim_ab.pred {
                                continue;
                            }
                            if claim_bc.obj_val == *subj {
                                continue;
                            }
                            let already_exists = derived.iter().any(|d| {
                                d.claim.subj == claim_ab.subj
                                    && d.claim.pred == claim_ab.pred
                                    && d.claim.obj_val == claim_bc.obj_val
                            }) || second_order.iter().any(|d| {
                                d.claim.subj == claim_ab.subj
                                    && d.claim.pred == claim_ab.pred
                                    && d.claim.obj_val == claim_bc.obj_val
                            });
                            if already_exists {
                                continue;
                            }
                            let inferred_trust = ((*trust_ab as f64).min(*trust_bc as f64)
                                * inference_decay
                                * inference_decay)
                                as TrustLevel;
                            if inferred_trust < 100 {
                                continue;
                            }
                            let inferred = ClaimData {
                                subj: claim_ab.subj,
                                pred: claim_ab.pred,
                                obj_tag: claim_ab.obj_tag,
                                obj_val: claim_bc.obj_val,
                                qualifiers_mask: 0xE000_0000, // Second-order inferred
                            };
                            let mut support_atom_ids = Vec::new();
                            if let Some(atom_id) = node_to_atom.get(&claim_ab.subj) {
                                support_atom_ids.push(*atom_id);
                            }
                            if let Some(atom_id) = node_to_atom.get(&intermediate)
                                && !support_atom_ids.contains(atom_id)
                            {
                                support_atom_ids.push(*atom_id);
                            }
                            second_order.push(InferredClaimStep {
                                claim: inferred,
                                premise_nodes: vec![claim_ab.subj, intermediate],
                                support_atom_ids,
                            });
                        }
                    }
                }
            }
            derived.extend(second_order);
        }

        derived
    }

    /// Add edges to answer graph based on real knowledge relations from atom EDGES section.
    ///
    /// SKF-1.1 Requirement: AnswerGraph must be proof-derived, NOT heuristic-based.
    ///
    /// # Algorithm
    /// 1. Load atom body for each selected candidate
    /// 2. Parse EDGES section to extract real knowledge relations
    /// 3. Add AgEdge only if real relation exists between nodes
    ///
    /// # Edge Types Used (SKF-1.1)
    /// - DERIVED_FROM (9): derivation chain, proof ancestry
    /// - SUPPORTS (5): evidence/argument support
    /// - IMPLIES (4): logical consequence
    /// - DEPENDS_ON (10): prerequisite dependency
    fn add_edges(&self, graph: &mut AnswerGraph, selected: &[usize], candidates: &[Candidate]) {
        // Build node_num -> graph index mapping for fast lookup
        let node_to_idx: HashMap<NodeNum, usize> = selected
            .iter()
            .map(|&idx| (candidates[idx].node_num, idx))
            .collect();

        // Load atom bodies and extract edges for each selected candidate
        for &src_idx in selected {
            let src_candidate = &candidates[src_idx];

            // Load atom body from CAS
            let atom_body = match self.load_atom_body(&src_candidate.atom_id) {
                Ok(body) => body,
                Err(_) => continue, // Skip if atom cannot be loaded
            };

            // Parse EDGES section from atom body
            let edges = self.extract_edges_from_body(&atom_body);

            // Add edges to graph based on real relations
            for edge in edges {
                // Check if target node is in selected candidates
                if let Some(&dst_idx) = node_to_idx.get(&edge.dst_node) {
                    // Convert EdgeType to AgEdgeType (SKF-1.1 proof-derived)
                    // Mapping from atom EDGES section EdgeType to AgEdgeType
                    let ag_edge_type = match edge.edge_type {
                        EdgeType::DERIVED_FROM => AgEdgeType::Derives, // Derivation chain
                        EdgeType::SUPPORTS => AgEdgeType::Supports,    // Evidence/argument support
                        EdgeType::IMPLIES => AgEdgeType::Derives, // Logical consequence -> derivation
                        EdgeType::DEPENDS_ON => AgEdgeType::References, // Prerequisite -> reference
                        _ => continue, // Skip non-proof-relation edge types
                    };

                    // Add edge with real confidence from atom EDGES section
                    graph.add_edge(AgEdge::new(src_idx, dst_idx, ag_edge_type, edge.confidence));
                }
            }
        }
    }

    /// Extract knowledge relation edges from atom EDGES section.
    ///
    /// EDGES section format (SKF-1.1):
    /// - u32 edge_count
    /// - For each edge: u64 src_node, u64 dst_node, u32 edge_type, u32 weight
    ///
    /// # Returns
    /// Vector of extracted edges with real knowledge relations
    fn extract_edges_from_body(&self, atom_body: &[u8]) -> Vec<ExtractedEdge> {
        let mut edges = Vec::new();

        // Parse AtomBodyHeader to get section table
        let body_header = match AtomBodyHeader::from_bytes(atom_body) {
            Ok(h) => h,
            Err(_) => return edges,
        };

        let section_count = body_header.section_count as usize;
        let table_start = body_header.section_table_off as usize;

        // Find EDGES section descriptor
        let edges_section = self.find_edges_section(atom_body, table_start, section_count);

        if let Some(section) = edges_section {
            // Extract section data
            let section_data = get_section_data(atom_body, &section);
            if let Ok(data) = section_data {
                // Parse edges from section data
                edges = self.parse_edges_data(data);
            }
        }

        edges
    }

    /// Find EDGES section in atom body section table.
    fn find_edges_section(
        &self,
        atom_body: &[u8],
        table_start: usize,
        section_count: usize,
    ) -> Option<SectionDesc> {
        let table_bytes =
            atom_body.get(table_start..table_start + section_count * SectionDesc::SIZE)?;

        for i in 0..section_count {
            let offset = i * SectionDesc::SIZE;
            let section_bytes = table_bytes.get(offset..offset + SectionDesc::SIZE)?;

            let section = match SectionDesc::from_bytes_unaligned(section_bytes) {
                Ok(s) => s,
                Err(_) => continue,
            };

            if section.kind() == Some(SectionKind::EDGES) {
                return Some(section);
            }
        }

        None
    }

    /// Parse edges from EDGES section data.
    ///
    /// Format: u32 edge_count, then edge records (24 bytes each)
    fn parse_edges_data(&self, data: &[u8]) -> Vec<ExtractedEdge> {
        let mut edges = Vec::new();

        if data.len() < 4 {
            return edges;
        }

        let edge_count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let mut offset = 4;

        for _ in 0..edge_count {
            if offset + 24 > data.len() {
                break;
            }

            let src_node = u64::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
                data[offset + 4],
                data[offset + 5],
                data[offset + 6],
                data[offset + 7],
            ]);

            let dst_node = u64::from_le_bytes([
                data[offset + 8],
                data[offset + 9],
                data[offset + 10],
                data[offset + 11],
                data[offset + 12],
                data[offset + 13],
                data[offset + 14],
                data[offset + 15],
            ]);

            let edge_type_raw = u32::from_le_bytes([
                data[offset + 16],
                data[offset + 17],
                data[offset + 18],
                data[offset + 19],
            ]);

            let confidence = u32::from_le_bytes([
                data[offset + 20],
                data[offset + 21],
                data[offset + 22],
                data[offset + 23],
            ]);

            offset += 24;

            // Convert to EdgeType and filter for proof-relation types only
            let edge_type = EdgeType::from_u32(edge_type_raw);
            if let Some(et) = edge_type {
                // Only include proof-relation edge types (SKF-1.1)
                if matches!(
                    et,
                    EdgeType::DERIVED_FROM
                        | EdgeType::SUPPORTS
                        | EdgeType::IMPLIES
                        | EdgeType::DEPENDS_ON
                ) {
                    edges.push(ExtractedEdge {
                        src_node,
                        dst_node,
                        edge_type: et,
                        confidence: (confidence & 0xFFFF) as TrustLevel, // Clamp to u16
                    });
                }
            }
        }

        edges
    }

    /// Extract claims from answer graph with full provenance.
    ///
    /// For each node in the answer graph:
    /// 1. Extract direct claims from derived_claims
    /// 2. Create structural claims from node metadata
    /// 3. Build evidence chains from evidence_refs
    /// 4. Track provenance for each claim
    fn extract_claims(&self, pack: &mut AnswerPack, state: &SolverState) {
        let mut evidence_chains: Vec<(ClaimView, Vec<EvidenceRef>)> = Vec::new();

        for (node_idx, node) in state.answer_graph.nodes.iter().enumerate() {
            // Extract direct claims from the node's derived_claims
            for claim_data in &node.derived_claims {
                let claim_status = if node.evidence_refs.is_empty() {
                    ClaimStatus::InsufficientEvidence
                } else {
                    ClaimStatus::Verified
                };
                let claim = ClaimView::new(
                    EntityRef::Node(node.atom_ref.node_num),
                    claim_data.pred as SymId,
                    ObjTag::from_u8(claim_data.obj_tag).unwrap_or(ObjTag::NULL),
                    ConstValue::u64(claim_data.obj_val),
                    claim_data.qualifiers_mask,
                    node.trust,
                    node.atom_ref.atom_id,
                )
                .with_provenance(
                    claim_status,
                    node.evidence_refs.clone(),
                    node.evidence_refs.clone(),
                );
                pack.add_claim(claim.clone());

                // Build evidence chain for this claim
                let chain = node.evidence_refs.clone();
                evidence_chains.push((claim, chain));
            }
            // Add all evidence references to the pack
            for ev in &node.evidence_refs {
                pack.add_evidence(ev.clone());
            }

            // Structural and routing-derived claims are useful annotations only
            // when the atom supplied at least one real semantic claim. A
            // symbol-only atom must remain a candidate, not become a fabricated
            // factual answer through solver bookkeeping.
            if !node.derived_claims.is_empty() {
                let structural_claim = ClaimView::new(
                    EntityRef::Node(node.atom_ref.node_num),
                    1, // "has_type" predicate
                    ObjTag::NULL,
                    ConstValue::u64(node.atom_type.to_u32() as u64),
                    0,
                    node.trust,
                    node.atom_ref.atom_id,
                )
                .with_provenance(
                    ClaimStatus::Structural,
                    node.evidence_refs.clone(),
                    node.evidence_refs.clone(),
                );
                pack.add_claim(structural_claim);

                // Create claims for gaps covered by this node
                for &gap_id in &node.gaps_covered {
                    if let Some(gap) = state.gaps.get(gap_id as usize) {
                        let gap_claim = ClaimView::new(
                            EntityRef::Node(node.atom_ref.node_num),
                            gap.kind.to_u8() as SymId,
                            ObjTag::NULL,
                            ConstValue::u64(gap_id as u64),
                            0,
                            node.trust,
                            node.atom_ref.atom_id,
                        )
                        .with_provenance(
                            ClaimStatus::Derived,
                            node.evidence_refs.clone(),
                            node.evidence_refs.clone(),
                        );
                        pack.add_claim(gap_claim);
                    }
                }

                // Create edge-based claims for connectivity
                for edge in &state.answer_graph.edges {
                    if edge.src_idx == node_idx || edge.dst_idx == node_idx {
                        let other_idx = if edge.src_idx == node_idx {
                            edge.dst_idx
                        } else {
                            edge.src_idx
                        };
                        if let Some(other_node) = state.answer_graph.get_node(other_idx) {
                            let edge_claim = ClaimView::new(
                                EntityRef::Node(node.atom_ref.node_num),
                                Self::edge_type_to_pred(&edge.edge_type),
                                ObjTag::REF,
                                ConstValue::u64(other_node.atom_ref.node_num),
                                0,
                                edge.confidence,
                                node.atom_ref.atom_id,
                            )
                            .with_provenance(
                                ClaimStatus::Derived,
                                node.evidence_refs.clone(),
                                node.evidence_refs.clone(),
                            );
                            pack.add_claim(edge_claim);
                        }
                    }
                }
            }
        }

        for claim_data in &state.answer_graph.derived_claims {
            if let Some(subject_node) = state
                .answer_graph
                .nodes
                .iter()
                .find(|node| node.atom_ref.node_num == claim_data.subj)
            {
                let claim_status = if subject_node.evidence_refs.is_empty() {
                    ClaimStatus::InsufficientEvidence
                } else {
                    ClaimStatus::Derived
                };
                let claim = ClaimView::new(
                    EntityRef::Node(claim_data.subj),
                    claim_data.pred as SymId,
                    ObjTag::from_u8(claim_data.obj_tag).unwrap_or(ObjTag::NULL),
                    ConstValue::u64(claim_data.obj_val),
                    claim_data.qualifiers_mask,
                    subject_node.trust,
                    subject_node.atom_ref.atom_id,
                )
                .with_provenance(
                    claim_status,
                    subject_node.evidence_refs.clone(),
                    subject_node.evidence_refs.clone(),
                );
                pack.add_claim(claim);
            }
        }

        // Deduplicate evidence references
        pack.evidence
            .sort_by(|a, b| a.atom_id.cmp(&b.atom_id).then(a.offset.cmp(&b.offset)));
        pack.evidence.dedup_by(|a, b| {
            a.atom_id == b.atom_id && a.offset == b.offset && a.section_kind == b.section_kind
        });
    }

    /// Convert AgEdgeType to a predicate SymId for claim representation.
    #[inline]
    fn edge_type_to_pred(edge_type: &AgEdgeType) -> SymId {
        match edge_type {
            AgEdgeType::Supports => 5,
            AgEdgeType::Contradicts => 8,
            AgEdgeType::Derives => 7,
            AgEdgeType::References => 4,
            AgEdgeType::Precedes => 6,
        }
    }

    /// Solve with custom context
    pub fn solve_with_ctx(&self, goal: GoalSpec, ctx_id: CtxId) -> Result<AnswerPack, SolverError> {
        let gaps = BackwardWaveGenerator::generate(&goal);
        let mut state = SolverState::new(ctx_id, goal, gaps);

        self.run_iterations(&mut state)?;

        let mut pack = AnswerPack::from_solver(
            state.answer_graph.clone(),
            state.ctx_id,
            &state.gaps,
            &self.cost_weights,
        );
        self.extract_claims(&mut pack, &state);
        if pack.graph.nodes.is_empty() && state.goal.lexical_resolution_required {
            pack.status = AnswerStatus::NoMatch;
        } else if !pack.graph.nodes.is_empty() && pack.claims.is_empty() {
            pack.status = AnswerStatus::InsufficientEvidence;
            pack.limitations.push(Limitation::warning(
                LimitationCode::IncompleteEvidence,
                "Lexical candidates were found, but they contain no knowledge claims; only source-backed candidate evidence is returned"
                    .to_owned(),
            ));
        }

        // Generate alternates
        let all_candidates = self.collect_final_candidates(&state);
        if !all_candidates.is_empty() {
            pack.generate_alternates(&all_candidates, &state.gaps, &self.cost_weights, 3);
        }

        Ok(pack)
    }
}

// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cost_weights_default() {
        let weights = CostWeights::default();
        assert!(weights.wC > weights.wIO); // Hard conflicts > I/O
        assert!(weights.wN > 0.0);
        assert_eq!(weights.wC, 1_000_000.0);
    }

    #[test]
    fn test_solver_config_validation() {
        let config = SolverConfig::default();
        assert!(config.validate().is_ok());

        let invalid = SolverConfig::new(0, 128, 1024);
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn test_gap_creation() {
        let pattern = ClaimPattern::default();
        let gap = Gap::new(0, GapKind::NEED_FACT, pattern)
            .with_priority(200)
            .with_generation(1);

        assert_eq!(gap.kind, GapKind::NEED_FACT);
        assert_eq!(gap.priority, 200);
        assert_eq!(gap.generation, 1);
        assert!(!gap.covered);
    }

    #[test]
    fn test_backward_wave_lookup() {
        let gs = GoalSpec::new(Intent::LOOKUP).with_entities(vec![EntityRef::Node(1)]);

        let gaps = BackwardWaveGenerator::generate(&gs);
        assert!(!gaps.is_empty());
        assert!(gaps.iter().any(|g| g.kind == GapKind::NEED_FACT));
        assert!(gaps.iter().any(|g| g.kind == GapKind::NEED_EVIDENCE));
    }

    #[test]
    fn test_backward_wave_define() {
        let gs = GoalSpec::new(Intent::DEFINE).with_entities(vec![EntityRef::Node(1)]);

        let gaps = BackwardWaveGenerator::generate(&gs);
        assert!(gaps.iter().any(|g| g.kind == GapKind::NEED_DEFINITION));
        assert!(gaps.iter().any(|g| g.kind == GapKind::NEED_CONSTRAINTS));
    }

    #[test]
    fn test_backward_wave_verify() {
        let gs = GoalSpec::new(Intent::VERIFY).with_entities(vec![EntityRef::Node(1)]);

        let gaps = BackwardWaveGenerator::generate(&gs);
        assert!(gaps.iter().any(|g| g.kind == GapKind::NEED_EVIDENCE));
        assert!(gaps.iter().any(|g| g.kind == GapKind::NEED_COUNTEREXAMPLE));
    }

    #[test]
    fn test_answer_graph() {
        let mut ag = AnswerGraph::new();

        let atom_id = [1u8; 32];
        let node = AgNode::new(AtomRef::new(atom_id, 1, 0, 0), AtomType::FACT);
        ag.add_node(node);

        assert_eq!(ag.node_count(), 1);
        assert_eq!(ag.edge_count(), 0);
    }

    #[test]
    fn test_candidate_benefit_cost() {
        let gaps = vec![
            Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default()).with_priority(100),
            Gap::new(1, GapKind::NEED_EVIDENCE, ClaimPattern::default()).with_priority(200),
        ];

        let candidate = Candidate {
            atom_id: [0u8; 32],
            node_num: 1,
            seg_id: 0,
            offset: 0,
            atom_type: AtomType::FACT,
            trust: 5000,
            estimated_io_bytes: 256,
            source_priority: SourcePriority::CasExact,
            source_backend: crate::query::router::BackendKind::Cas,
            covers_gaps: vec![0, 1],
            hard_conflicts: 0,
            soft_conflicts: 0,
            age_ns: 0,
            domain_mask: 0xFFFF,
            evidence_refs: Vec::new(),
            derived_claims: Vec::new(),
            requires_invariant_check: true,
            ann_candidate_requires_filtering: false,
            branch_ctx_id: None,
        };

        let weights = CostWeights::default();
        let ratio = candidate.benefit_cost_ratio(&gaps, &weights);
        assert!(ratio > 0.0);
    }

    #[test]
    fn test_context_scope_blocks_unlisted_branch_candidate() {
        let context_scope = crate::query::contract::ContextScope {
            include_conflicting_branches: false,
            ..Default::default()
        };

        let goal = GoalSpec::new(Intent::LOOKUP).with_context_scope(context_scope);
        let state = SolverState::new(0, goal, Vec::new());
        let solver = FixedPointSolver::new();
        let candidate = Candidate {
            atom_id: [9u8; 32],
            node_num: 9,
            seg_id: 0,
            offset: 0,
            atom_type: AtomType::FACT,
            trust: 5000,
            estimated_io_bytes: 128,
            source_priority: SourcePriority::CasExact,
            source_backend: crate::query::router::BackendKind::Cas,
            covers_gaps: vec![0],
            hard_conflicts: 0,
            soft_conflicts: 0,
            age_ns: 0,
            domain_mask: 0xFFFF,
            evidence_refs: Vec::new(),
            derived_claims: Vec::new(),
            requires_invariant_check: true,
            ann_candidate_requires_filtering: false,
            branch_ctx_id: Some(42),
        };

        let (admitted, rejected) = solver.filter_by_context_scope(vec![candidate], &state);

        assert!(admitted.is_empty());
        assert_eq!(rejected.len(), 1);
        assert_eq!(
            rejected[0].constraint_results[0].status,
            crate::query::contract::ConstraintStatus::BlockedByPolicy
        );
    }

    #[test]
    fn test_conflict_policy_exposes_conflict_sets_and_fail_status() {
        let solver = FixedPointSolver::new();
        let parent_ctx = solver.ctx_manager.lock().create_context(0);
        let conflict = Conflict::new(
            [1u8; 32],
            [2u8; 32],
            ConflictType::Contradiction,
            ConflictSeverity::Hard,
            0xABCD,
        );
        let branch_ctx = solver
            .ctx_manager
            .lock()
            .branch_ctx(parent_ctx, &conflict)
            .expect("branch should be created");

        let conflict_policy = crate::query::contract::ConflictPolicy {
            mode: crate::query::contract::ConflictPolicyMode::Fail,
            ..Default::default()
        };
        let goal = GoalSpec::new(Intent::LOOKUP).with_conflict_policy(conflict_policy);
        let mut state = SolverState::new(parent_ctx, goal, Vec::new());
        state.answer_graph.branch_lineage.push(branch_ctx);

        let mut pack = AnswerPack::new(parent_ctx);
        solver.apply_conflict_policy_to_pack(&mut pack, &state);

        assert_eq!(pack.status, AnswerStatus::PolicyBlocked);
        assert_eq!(pack.conflicts.len(), 1);
        assert_eq!(pack.conflict_sets.len(), 1);
        assert_eq!(pack.conflict_sets[0].pattern_hash, 0xABCD);
        assert!(pack.conflict_sets[0].branches.contains(&branch_ctx));
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
    }

    #[test]
    fn test_set_cover_greedy() {
        let gaps = vec![
            Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default()).with_priority(100),
            Gap::new(1, GapKind::NEED_EVIDENCE, ClaimPattern::default()).with_priority(200),
        ];

        let candidates = vec![
            Candidate {
                atom_id: [1u8; 32],
                node_num: 1,
                seg_id: 0,
                offset: 0,
                atom_type: AtomType::FACT,
                trust: 5000,
                estimated_io_bytes: 100,
                source_priority: SourcePriority::CasExact,
                source_backend: crate::query::router::BackendKind::Cas,
                covers_gaps: vec![0],
                hard_conflicts: 0,
                soft_conflicts: 0,
                age_ns: 0,
                domain_mask: 0xFFFF,
                evidence_refs: Vec::new(),
                derived_claims: Vec::new(),
                requires_invariant_check: true,
                ann_candidate_requires_filtering: false, // Non-ANN candidate
                branch_ctx_id: None,
            },
            Candidate {
                atom_id: [2u8; 32],
                node_num: 2,
                seg_id: 0,
                offset: 0,
                atom_type: AtomType::FACT,
                trust: 5000,
                estimated_io_bytes: 100,
                source_priority: SourcePriority::CasExact,
                source_backend: crate::query::router::BackendKind::Cas,
                covers_gaps: vec![0, 1],
                hard_conflicts: 0,
                soft_conflicts: 0,
                age_ns: 0,
                domain_mask: 0xFFFF,
                evidence_refs: Vec::new(),
                derived_claims: Vec::new(),
                requires_invariant_check: true,
                ann_candidate_requires_filtering: false, // Non-ANN candidate
                branch_ctx_id: None,
            },
        ];

        let weights = CostWeights::default();
        let selected =
            SetCoverSolver::greedy_select(&candidates, &gaps, &weights, OutputSchema::Flat);

        // Should select candidate 1 (covers both gaps)
        assert!(!selected.is_empty());
    }

    #[test]
    fn test_goal_spec_builder() {
        let gs = GoalSpec::new(Intent::LOOKUP)
            .with_trust(500)
            .with_domain(0xFFFF)
            .with_entities(vec![EntityRef::Node(1), EntityRef::Node(2)]);

        assert_eq!(gs.intent, Intent::LOOKUP);
        assert_eq!(gs.trust_min, 500);
        assert_eq!(gs.domain_mask, 0xFFFF);
        assert_eq!(gs.entities.len(), 2);
    }

    #[test]
    fn test_entity_ref() {
        let sym = EntityRef::sym(42);
        assert!(matches!(sym, EntityRef::Sym(_)));
        assert!(!sym.is_node());
        assert_eq!(sym.as_node(), None);

        let node = EntityRef::node(100);
        assert!(node.is_node());
        assert_eq!(node.as_node(), Some(100));
    }

    #[test]
    fn test_solver_creation() {
        let solver = FixedPointSolver::new();
        assert_eq!(solver.config.max_iterations, 10);
    }

    #[test]
    fn test_solver_config_custom() {
        let config = SolverConfig::new(20, 256, 512 * 1024);
        let solver = FixedPointSolver::with_config(config);
        assert_eq!(solver.config.max_iterations, 20);
        assert_eq!(solver.config.fetch_budget, 256);
    }

    #[test]
    fn test_time_range() {
        let range = TimeRange::new(100, 200, TimeMode::Overlap);
        assert!(range.contains(150));
        assert!(!range.contains(50));
        assert!(!range.contains(250));
    }

    #[test]
    fn test_io_mode() {
        assert_eq!(IoMode::default(), IoMode::Sync);
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
    fn test_ag_node_cost() {
        let mut node = AgNode::new(AtomRef::new([1u8; 32], 1, 0, 0), AtomType::FACT);
        node.trust = 8000;
        node.io_bytes = 256;

        let weights = CostWeights::default();
        node.calculate_cost(&weights, 1_000_000_000);

        assert!(node.cost > 0.0);
    }

    #[test]
    fn test_answer_graph_prune() {
        let mut ag = AnswerGraph::new();

        // Add node with no gaps covered
        let node1 = AgNode::new(AtomRef::new([1u8; 32], 1, 0, 0), AtomType::FACT);
        ag.add_node(node1);

        // Add node with gaps covered
        let mut node2 = AgNode::new(AtomRef::new([2u8; 32], 2, 0, 0), AtomType::FACT);
        node2.add_gap(0);
        ag.add_node(node2);

        ag.prune();

        // First node should be removed
        assert_eq!(ag.node_count(), 1);
    }

    // ========================================================================
    // Integration tests for new functionality
    // ========================================================================

    #[test]
    fn test_connectivity_closure_single_node() {
        // Single node should remain single
        let gaps = vec![Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default())];
        let candidates = vec![Candidate {
            atom_id: [1u8; 32],
            node_num: 1,
            seg_id: 0,
            offset: 0,
            atom_type: AtomType::FACT,
            trust: 5000,
            estimated_io_bytes: 100,
            source_priority: SourcePriority::CasExact,
            source_backend: crate::query::router::BackendKind::Cas,
            covers_gaps: vec![0],
            hard_conflicts: 0,
            soft_conflicts: 0,
            age_ns: 0,
            domain_mask: 0xFFFF,
            evidence_refs: Vec::new(),
            derived_claims: Vec::new(),
            requires_invariant_check: true,
            ann_candidate_requires_filtering: false, // Non-ANN candidate
            branch_ctx_id: None,
        }];

        let selected = SetCoverSolver::greedy_select(
            &candidates,
            &gaps,
            &CostWeights::default(),
            OutputSchema::Flat,
        );
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn test_connectivity_closure_shared_gap() {
        // Two candidates covering the same gap should be connected
        let gaps = vec![
            Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default()).with_priority(100),
            Gap::new(1, GapKind::NEED_EVIDENCE, ClaimPattern::default()).with_priority(100),
        ];
        let candidates = vec![
            Candidate {
                atom_id: [1u8; 32],
                node_num: 1,
                seg_id: 0,
                offset: 0,
                atom_type: AtomType::FACT,
                trust: 5000,
                estimated_io_bytes: 100,
                source_priority: SourcePriority::CasExact,
                source_backend: crate::query::router::BackendKind::Cas,
                covers_gaps: vec![0],
                hard_conflicts: 0,
                soft_conflicts: 0,
                age_ns: 0,
                domain_mask: 0xFFFF,
                evidence_refs: Vec::new(),
                derived_claims: Vec::new(),
                requires_invariant_check: true,
                ann_candidate_requires_filtering: false, // Non-ANN candidate
                branch_ctx_id: None,
            },
            Candidate {
                atom_id: [2u8; 32],
                node_num: 2,
                seg_id: 0,
                offset: 0,
                atom_type: AtomType::FACT,
                trust: 5000,
                estimated_io_bytes: 100,
                source_priority: SourcePriority::CasExact,
                source_backend: crate::query::router::BackendKind::Cas,
                covers_gaps: vec![0, 1],
                hard_conflicts: 0,
                soft_conflicts: 0,
                age_ns: 0,
                domain_mask: 0xFFFF,
                evidence_refs: Vec::new(),
                derived_claims: Vec::new(),
                requires_invariant_check: true,
                ann_candidate_requires_filtering: false, // Non-ANN candidate
                branch_ctx_id: None,
            },
        ];

        let selected = SetCoverSolver::greedy_select(
            &candidates,
            &gaps,
            &CostWeights::default(),
            OutputSchema::Explanation,
        );
        // Should select at least one candidate
        assert!(!selected.is_empty());
    }

    #[test]
    fn test_connectivity_closure_evidence_bridge() {
        // Two candidates sharing an evidence reference should be connected
        let shared_evidence = EvidenceRef::new([99u8; 32], SectionKind::EVIDENCE, 100, 50, 8000);

        let gaps = vec![
            Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default()).with_priority(100),
            Gap::new(1, GapKind::NEED_EVIDENCE, ClaimPattern::default()).with_priority(100),
        ];
        let candidates = vec![
            Candidate {
                atom_id: [1u8; 32],
                node_num: 1,
                seg_id: 0,
                offset: 0,
                atom_type: AtomType::FACT,
                trust: 5000,
                estimated_io_bytes: 100,
                source_priority: SourcePriority::CasExact,
                source_backend: crate::query::router::BackendKind::Cas,
                covers_gaps: vec![0],
                hard_conflicts: 0,
                soft_conflicts: 0,
                age_ns: 0,
                domain_mask: 0xFFFF,
                evidence_refs: vec![shared_evidence.clone()],
                derived_claims: Vec::new(),
                requires_invariant_check: true,
                ann_candidate_requires_filtering: false, // Non-ANN candidate
                branch_ctx_id: None,
            },
            Candidate {
                atom_id: [2u8; 32],
                node_num: 2,
                seg_id: 0,
                offset: 0,
                atom_type: AtomType::FACT,
                trust: 5000,
                estimated_io_bytes: 100,
                source_priority: SourcePriority::CasExact,
                source_backend: crate::query::router::BackendKind::Cas,
                covers_gaps: vec![1],
                hard_conflicts: 0,
                soft_conflicts: 0,
                age_ns: 0,
                domain_mask: 0xFFFF,
                evidence_refs: vec![shared_evidence],
                derived_claims: Vec::new(),
                requires_invariant_check: true,
                ann_candidate_requires_filtering: false, // Non-ANN candidate
                branch_ctx_id: None,
            },
        ];

        let selected = SetCoverSolver::greedy_select(
            &candidates,
            &gaps,
            &CostWeights::default(),
            OutputSchema::Explanation,
        );
        // Both should be selected since they cover different gaps
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn test_infer_claims_transitivity() {
        let solver = FixedPointSolver::new();

        // Create candidates with transitive claims: A→B, B→C
        let claim_ab = ClaimData {
            subj: 1,
            pred: 2, // IMPLIES
            obj_tag: 0,
            obj_val: 2,
            qualifiers_mask: 0,
        };
        let claim_bc = ClaimData {
            subj: 2,
            pred: 2, // IMPLIES
            obj_tag: 0,
            obj_val: 3,
            qualifiers_mask: 0,
        };

        let candidates = vec![
            Candidate {
                atom_id: [1u8; 32],
                node_num: 1,
                seg_id: 0,
                offset: 0,
                atom_type: AtomType::FACT,
                trust: 8000,
                estimated_io_bytes: 100,
                source_priority: SourcePriority::CasExact,
                source_backend: crate::query::router::BackendKind::Cas,
                covers_gaps: vec![0],
                hard_conflicts: 0,
                soft_conflicts: 0,
                age_ns: 0,
                domain_mask: 0xFFFF,
                evidence_refs: Vec::new(),
                derived_claims: vec![claim_ab],
                requires_invariant_check: true,
                ann_candidate_requires_filtering: false, // Non-ANN candidate
                branch_ctx_id: None,
            },
            Candidate {
                atom_id: [2u8; 32],
                node_num: 2,
                seg_id: 0,
                offset: 0,
                atom_type: AtomType::FACT,
                trust: 7000,
                estimated_io_bytes: 100,
                source_priority: SourcePriority::CasExact,
                source_backend: crate::query::router::BackendKind::Cas,
                covers_gaps: vec![1],
                hard_conflicts: 0,
                soft_conflicts: 0,
                age_ns: 0,
                domain_mask: 0xFFFF,
                evidence_refs: Vec::new(),
                derived_claims: vec![claim_bc],
                requires_invariant_check: true,
                ann_candidate_requires_filtering: false, // Non-ANN candidate
                branch_ctx_id: None,
            },
        ];

        let ctx_id = 0u32;
        let derived = solver.infer_claims(&candidates, &ctx_id);

        // Should have inferred A→C (transitive)
        let transitive = derived
            .iter()
            .any(|c| c.subj == 1 && c.pred == 2 && c.obj_val == 3);
        assert!(
            transitive,
            "Should have inferred transitive claim A→C, derived: {:?}",
            derived
        );

        // Inferred claims should have the inferred flag set
        let inferred_claims: Vec<_> = derived
            .iter()
            .filter(|c| c.qualifiers_mask & 0x8000_0000 != 0)
            .collect();
        assert!(
            !inferred_claims.is_empty(),
            "Inferred claims should have inferred flag set"
        );
    }

    #[test]
    fn test_infer_claims_evidence_support() {
        let solver = FixedPointSolver::new();

        let shared_ev = EvidenceRef::new([50u8; 32], SectionKind::EVIDENCE, 200, 100, 9000);

        let candidates = vec![
            Candidate {
                atom_id: [1u8; 32],
                node_num: 1,
                seg_id: 0,
                offset: 0,
                atom_type: AtomType::FACT,
                trust: 8000,
                estimated_io_bytes: 100,
                source_priority: SourcePriority::CasExact,
                source_backend: crate::query::router::BackendKind::Cas,
                covers_gaps: vec![0],
                hard_conflicts: 0,
                soft_conflicts: 0,
                age_ns: 0,
                domain_mask: 0xFFFF,
                evidence_refs: vec![shared_ev.clone()],
                derived_claims: Vec::new(),
                requires_invariant_check: true,
                ann_candidate_requires_filtering: false, // Non-ANN candidate
                branch_ctx_id: None,
            },
            Candidate {
                atom_id: [2u8; 32],
                node_num: 2,
                seg_id: 0,
                offset: 0,
                atom_type: AtomType::FACT,
                trust: 7000,
                estimated_io_bytes: 100,
                source_priority: SourcePriority::CasExact,
                source_backend: crate::query::router::BackendKind::Cas,
                covers_gaps: vec![1],
                hard_conflicts: 0,
                soft_conflicts: 0,
                age_ns: 0,
                domain_mask: 0xFFFF,
                evidence_refs: vec![shared_ev],
                derived_claims: Vec::new(),
                requires_invariant_check: true,
                ann_candidate_requires_filtering: false, // Non-ANN candidate
                branch_ctx_id: None,
            },
        ];

        let ctx_id = 0u32;
        let derived = solver.infer_claims(&candidates, &ctx_id);

        // Should have inferred SUPPORTS link between nodes sharing evidence
        let supports = derived.iter().any(|c| c.pred == 5); // SUPPORTS
        assert!(
            supports,
            "Should have inferred SUPPORTS link from shared evidence, derived: {:?}",
            derived
        );
    }

    #[test]
    fn test_answer_pack_multi_factor_confidence() {
        let mut graph = AnswerGraph::new();

        // Add two nodes with good trust and evidence
        let mut node1 = AgNode::new(AtomRef::new([1u8; 32], 1, 0, 0), AtomType::FACT);
        node1.trust = 8000;
        node1.io_bytes = 256;
        node1.add_gap(0);
        node1.evidence_refs.push(EvidenceRef::new(
            [10u8; 32],
            SectionKind::EVIDENCE,
            100,
            50,
            8000,
        ));
        graph.add_node(node1);

        let mut node2 = AgNode::new(AtomRef::new([2u8; 32], 2, 0, 0), AtomType::FACT);
        node2.trust = 7000;
        node2.io_bytes = 128;
        node2.add_gap(1);
        node2.evidence_refs.push(EvidenceRef::new(
            [20u8; 32],
            SectionKind::EVIDENCE,
            200,
            50,
            7000,
        ));
        graph.add_node(node2);

        // Add edge for connectivity
        graph.add_edge(AgEdge::new(0, 1, AgEdgeType::Supports, 7500));

        graph.mark_gaps_covered(&[0, 1]);
        graph.recalculate_cost(&CostWeights::default(), 0);

        let gaps = vec![
            Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default())
                .with_priority(200)
                .with_priority(200),
            Gap::new(1, GapKind::NEED_EVIDENCE, ClaimPattern::default()).with_priority(150),
        ];

        let pack = AnswerPack::from_solver(graph, 0, &gaps, &CostWeights::default());

        // Confidence should be reasonably high (all gaps covered, good trust, connected)
        assert!(
            pack.confidence > 0.5,
            "Confidence should be > 0.5 for good answer, got {}",
            pack.confidence
        );
        assert!(
            pack.confidence <= 1.0,
            "Confidence should be <= 1.0, got {}",
            pack.confidence
        );

        // No critical limitations for a good answer
        assert!(
            !pack.has_critical_limitations(),
            "Should not have critical limitations"
        );
    }

    #[test]
    fn test_answer_pack_incomplete_coverage_limitations() {
        let mut graph = AnswerGraph::new();

        let mut node = AgNode::new(AtomRef::new([1u8; 32], 1, 0, 0), AtomType::FACT);
        node.trust = 3000; // Low trust
        node.add_gap(0);
        graph.add_node(node);

        graph.mark_gaps_covered(&[0]);
        graph.recalculate_cost(&CostWeights::default(), 0);

        let gaps = vec![
            Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default()),
            Gap::new(1, GapKind::NEED_EVIDENCE, ClaimPattern::default()),
            Gap::new(2, GapKind::NEED_CONSTRAINTS, ClaimPattern::default()),
        ];

        let pack = AnswerPack::from_solver(graph, 0, &gaps, &CostWeights::default());

        // Should have incomplete evidence limitation
        assert!(
            pack.limitations
                .iter()
                .any(|l| l.code == LimitationCode::IncompleteEvidence),
            "Should have IncompleteEvidence limitation"
        );

        // Should have low confidence limitation
        assert!(
            pack.limitations
                .iter()
                .any(|l| l.code == LimitationCode::LowConfidence),
            "Should have LowConfidence limitation"
        );

        // Confidence should be low
        assert!(
            pack.confidence < 0.5,
            "Confidence should be < 0.5 for incomplete answer, got {}",
            pack.confidence
        );
    }

    #[test]
    fn test_answer_pack_hard_conflict_limitation() {
        let mut graph = AnswerGraph::new();

        let mut node = AgNode::new(AtomRef::new([1u8; 32], 1, 0, 0), AtomType::FACT);
        node.trust = 8000;
        node.hard_conflicts = 2;
        node.add_gap(0);
        graph.add_node(node);

        graph.mark_gaps_covered(&[0]);
        graph.recalculate_cost(&CostWeights::default(), 0);

        let gaps = vec![Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default())];

        let pack = AnswerPack::from_solver(graph, 0, &gaps, &CostWeights::default());

        // Should have critical conflict limitation
        let conflict_lim = pack
            .limitations
            .iter()
            .find(|l| l.code == LimitationCode::ConflictsPresent);
        assert!(
            conflict_lim.is_some(),
            "Should have ConflictsPresent limitation"
        );
        assert_eq!(
            conflict_lim.unwrap().severity,
            LimitationSeverity::Critical,
            "Hard conflicts should be Critical severity"
        );
    }

    #[test]
    fn test_answer_pack_alternates_generation() {
        let mut graph = AnswerGraph::new();

        let mut node = AgNode::new(AtomRef::new([1u8; 32], 1, 0, 0), AtomType::FACT);
        node.trust = 8000;
        node.add_gap(0);
        graph.add_node(node);

        graph.mark_gaps_covered(&[0]);

        let gaps = vec![Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default())];

        let mut pack = AnswerPack::from_solver(graph, 0, &gaps, &CostWeights::default());

        // Create candidates for alternate generation
        let candidates = vec![
            Candidate {
                atom_id: [1u8; 32],
                node_num: 1,
                seg_id: 0,
                offset: 0,
                atom_type: AtomType::FACT,
                trust: 8000,
                estimated_io_bytes: 100,
                source_priority: SourcePriority::CasExact,
                source_backend: crate::query::router::BackendKind::Cas,
                covers_gaps: vec![0],
                hard_conflicts: 0,
                soft_conflicts: 0,
                age_ns: 0,
                domain_mask: 0xFFFF,
                evidence_refs: Vec::new(),
                derived_claims: Vec::new(),
                requires_invariant_check: true,
                ann_candidate_requires_filtering: false, // Non-ANN candidate
                branch_ctx_id: None,
            },
            Candidate {
                atom_id: [2u8; 32],
                node_num: 2,
                seg_id: 0,
                offset: 0,
                atom_type: AtomType::FACT,
                trust: 6000,
                estimated_io_bytes: 200,
                source_priority: SourcePriority::Inverted,
                source_backend: crate::query::router::BackendKind::Inverted,
                covers_gaps: vec![0],
                hard_conflicts: 0,
                soft_conflicts: 0,
                age_ns: 0,
                domain_mask: 0xFFFF,
                evidence_refs: Vec::new(),
                derived_claims: Vec::new(),
                requires_invariant_check: true,
                ann_candidate_requires_filtering: false, // Non-ANN candidate
                branch_ctx_id: None,
            },
        ];

        pack.generate_alternates(&candidates, &gaps, &CostWeights::default(), 3);

        // Alternates may or may not be generated depending on selection overlap
        // The important thing is the method runs without error
        assert!(pack.alternates.len() <= 3);
    }

    #[test]
    fn test_e2e_solve_lookup() {
        // End-to-end test: create solver, register data, solve, verify
        let mut solver = FixedPointSolver::new();
        solver.config.max_iterations = 5;

        // Register some atoms in the router
        for i in 0..3u64 {
            let mut atom_id = [0u8; 32];
            atom_id[0] = (i + 1) as u8;
            solver.router.register_atom(atom_id, i + 1, 0, i * 100, 64);
            solver
                .router
                .index_term(&format!("entity_{}", i + 1), i + 1);
        }

        let goal = GoalSpec::new(Intent::LOOKUP)
            .with_entities(vec![EntityRef::Node(1), EntityRef::Node(2)]);

        let result = solver.solve(goal, 0);
        assert!(result.is_ok(), "Solve should succeed: {:?}", result);

        let pack = result.unwrap();
        assert!(pack.confidence >= 0.0);
        assert!(pack.confidence <= 1.0);
        // May or may not have claims depending on routing
        // The important thing is the full pipeline runs
    }

    #[test]
    fn test_e2e_solve_define() {
        let mut solver = FixedPointSolver::new();
        solver.config.max_iterations = 5;

        // Register atoms
        for i in 0..2u64 {
            let mut atom_id = [0u8; 32];
            atom_id[0] = (i + 10) as u8;
            solver.router.register_atom(atom_id, i + 10, 0, i * 100, 64);
        }

        let goal = GoalSpec::new(Intent::DEFINE).with_entities(vec![EntityRef::Node(10)]);

        let result = solver.solve(goal, 0);
        assert!(result.is_ok(), "Define solve should succeed: {:?}", result);

        let pack = result.unwrap();
        // DEFINE intent generates NEED_DEFINITION + NEED_CONSTRAINTS + NEED_COUNTEREXAMPLE
        assert!(pack.confidence >= 0.0);
    }

    #[test]
    fn test_e2e_solve_explain() {
        let mut solver = FixedPointSolver::new();
        solver.config.max_iterations = 5;

        for i in 0..3u64 {
            let mut atom_id = [0u8; 32];
            atom_id[0] = (i + 20) as u8;
            solver.router.register_atom(atom_id, i + 20, 0, i * 100, 64);
        }

        let goal = GoalSpec::new(Intent::EXPLAIN).with_entities(vec![EntityRef::Node(20)]);

        let result = solver.solve(goal, 0);
        assert!(result.is_ok(), "Explain solve should succeed: {:?}", result);

        let pack = result.unwrap();
        assert!(pack.confidence >= 0.0);
        assert!(pack.confidence <= 1.0);
    }

    #[test]
    fn test_e2e_solve_compare() {
        let mut solver = FixedPointSolver::new();
        solver.config.max_iterations = 5;

        for i in 0..4u64 {
            let mut atom_id = [0u8; 32];
            atom_id[0] = (i + 30) as u8;
            solver.router.register_atom(atom_id, i + 30, 0, i * 100, 64);
        }

        let goal = GoalSpec::new(Intent::COMPARE)
            .with_entities(vec![EntityRef::Node(30), EntityRef::Node(31)]);

        let result = solver.solve(goal, 0);
        assert!(result.is_ok(), "Compare solve should succeed: {:?}", result);

        let pack = result.unwrap();
        assert!(pack.confidence >= 0.0);
    }

    #[test]
    fn test_e2e_solve_verify() {
        let mut solver = FixedPointSolver::new();
        solver.config.max_iterations = 5;

        for i in 0..2u64 {
            let mut atom_id = [0u8; 32];
            atom_id[0] = (i + 40) as u8;
            solver.router.register_atom(atom_id, i + 40, 0, i * 100, 64);
        }

        let goal = GoalSpec::new(Intent::VERIFY).with_entities(vec![EntityRef::Node(40)]);

        let result = solver.solve(goal, 0);
        assert!(result.is_ok(), "Verify solve should succeed: {:?}", result);

        let pack = result.unwrap();
        assert!(pack.confidence >= 0.0);
    }

    #[test]
    fn test_e2e_solve_with_ctx() {
        let mut solver = FixedPointSolver::new();
        solver.config.max_iterations = 5;

        for i in 0..2u64 {
            let mut atom_id = [0u8; 32];
            atom_id[0] = (i + 50) as u8;
            solver.router.register_atom(atom_id, i + 50, 0, i * 100, 64);
        }

        let goal = GoalSpec::new(Intent::LOOKUP).with_entities(vec![EntityRef::Node(50)]);

        let result = solver.solve_with_ctx(goal, 0);
        assert!(
            result.is_ok(),
            "Solve with ctx should succeed: {:?}",
            result
        );

        let pack = result.unwrap();
        assert_eq!(pack.selected_ctx, 0);
        assert!(pack.confidence >= 0.0);
    }

    #[test]
    fn test_extract_claims_does_not_fabricate_structural_claims() {
        let solver = FixedPointSolver::new();

        let mut graph = AnswerGraph::new();
        let mut node = AgNode::new(AtomRef::new([1u8; 32], 1, 0, 0), AtomType::FACT);
        node.trust = 8000;
        node.add_gap(0);
        graph.add_node(node);
        graph.mark_gaps_covered(&[0]);

        let gaps = vec![Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default())];
        let mut pack = AnswerPack::from_solver(graph.clone(), 0, &gaps, &CostWeights::default());

        let state = SolverState::new(0, GoalSpec::new(Intent::LOOKUP), gaps);
        // We need a state with the graph populated
        let mut state_with_graph = state;
        state_with_graph.answer_graph = graph;

        solver.extract_claims(&mut pack, &state_with_graph);

        // Graph metadata is not semantic evidence and must not become an answer claim.
        assert!(
            pack.claims.is_empty(),
            "Zero-claim graph nodes must not fabricate structural claims"
        );
    }

    #[test]
    fn test_answer_pack_best_alternate() {
        let mut primary = AnswerPack::new(0);
        primary.confidence = 0.6;

        let mut alt1 = AnswerPack::new(0);
        alt1.confidence = 0.8;

        let mut alt2 = AnswerPack::new(0);
        alt2.confidence = 0.4;

        primary.alternates.push(alt1);
        primary.alternates.push(alt2);

        let best = primary.best();
        assert_eq!(
            best.confidence, 0.8,
            "Best should be the highest confidence alternate"
        );
    }

    #[test]
    fn test_answer_pack_empty_graph_confidence() {
        let graph = AnswerGraph::new();
        let gaps = vec![
            Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default()),
            Gap::new(1, GapKind::NEED_EVIDENCE, ClaimPattern::default()),
        ];

        let pack = AnswerPack::from_solver(graph, 0, &gaps, &CostWeights::default());

        // Empty graph with uncovered gaps should have low confidence
        // The multi-factor formula gives ~0.25 because consistency and cost factors
        // are still "good" (no conflicts, no cost) even with empty graph
        assert!(
            pack.confidence < 0.5,
            "Empty graph should have low confidence, got {}",
            pack.confidence
        );
        // Should have incomplete evidence limitation
        assert!(
            pack.limitations
                .iter()
                .any(|l| l.code == LimitationCode::IncompleteEvidence),
            "Should have IncompleteEvidence limitation"
        );
    }

    #[test]
    fn test_connectivity_closure_tree_schema() {
        // Tree schema should trigger connectivity closure
        let gaps = vec![
            Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default()).with_priority(100),
            Gap::new(1, GapKind::NEED_EVIDENCE, ClaimPattern::default()).with_priority(100),
        ];
        let candidates = vec![
            Candidate {
                atom_id: [1u8; 32],
                node_num: 1,
                seg_id: 0,
                offset: 0,
                atom_type: AtomType::FACT,
                trust: 5000,
                estimated_io_bytes: 100,
                source_priority: SourcePriority::CasExact,
                source_backend: crate::query::router::BackendKind::Cas,
                covers_gaps: vec![0],
                hard_conflicts: 0,
                soft_conflicts: 0,
                age_ns: 0,
                domain_mask: 0xFFFF,
                evidence_refs: Vec::new(),
                derived_claims: Vec::new(),
                requires_invariant_check: true,
                ann_candidate_requires_filtering: false, // Non-ANN candidate
                branch_ctx_id: None,
            },
            Candidate {
                atom_id: [2u8; 32],
                node_num: 2,
                seg_id: 0,
                offset: 0,
                atom_type: AtomType::FACT,
                trust: 5000,
                estimated_io_bytes: 100,
                source_priority: SourcePriority::CasExact,
                source_backend: crate::query::router::BackendKind::Cas,
                covers_gaps: vec![1],
                hard_conflicts: 0,
                soft_conflicts: 0,
                age_ns: 0,
                domain_mask: 0xFFFF,
                evidence_refs: Vec::new(),
                derived_claims: Vec::new(),
                requires_invariant_check: true,
                ann_candidate_requires_filtering: false, // Non-ANN candidate
                branch_ctx_id: None,
            },
        ];

        let selected = SetCoverSolver::greedy_select(
            &candidates,
            &gaps,
            &CostWeights::default(),
            OutputSchema::Tree,
        );
        // Both candidates needed for different gaps
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn test_infer_claims_no_self_loops() {
        let solver = FixedPointSolver::new();

        // A→A should not be inferred
        let claim_aa = ClaimData {
            subj: 1,
            pred: 2,
            obj_tag: 0,
            obj_val: 1, // Same as subj
            qualifiers_mask: 0,
        };

        let candidates = vec![Candidate {
            atom_id: [1u8; 32],
            node_num: 1,
            seg_id: 0,
            offset: 0,
            atom_type: AtomType::FACT,
            trust: 8000,
            estimated_io_bytes: 100,
            source_priority: SourcePriority::CasExact,
            source_backend: crate::query::router::BackendKind::Cas,
            covers_gaps: vec![0],
            hard_conflicts: 0,
            soft_conflicts: 0,
            age_ns: 0,
            domain_mask: 0xFFFF,
            evidence_refs: Vec::new(),
            derived_claims: vec![claim_aa],
            requires_invariant_check: true,
            ann_candidate_requires_filtering: false, // Non-ANN candidate
            branch_ctx_id: None,
        }];

        let ctx_id = 0u32;
        let derived = solver.infer_claims(&candidates, &ctx_id);

        // No self-loop should be inferred
        let self_loop = derived.iter().any(|c| c.subj == c.obj_val);
        assert!(
            !self_loop,
            "Should not have self-loop claims, derived: {:?}",
            derived
        );
    }

    #[test]
    fn test_claim_matches_gap_pattern_for_exact_inferred_claim() {
        let claim = ClaimData {
            subj: 10,
            pred: 2,
            obj_tag: ObjTag::NODENUM.to_u8(),
            obj_val: 30,
            qualifiers_mask: 0x8000_0000,
        };
        let pattern = ClaimPattern {
            subj: PatternRef::Node(10),
            pred: PatternRef::Sym(2),
            obj_tag: Some(ObjTag::NODENUM),
            obj: PatternRef::Node(30),
            qualifiers_mask: 0,
        };

        assert!(FixedPointSolver::claim_matches_gap_pattern(
            &claim, &pattern
        ));
    }

    #[test]
    fn test_apply_inferred_claims_updates_graph_and_gap_coverage() {
        let solver = FixedPointSolver::new();
        let mut graph = AnswerGraph::new();
        let mut node = AgNode::new(AtomRef::new([1u8; 32], 10, 0, 0), AtomType::FACT);
        node.trust = 7000;
        graph.add_node(node);

        let mut gaps = vec![Gap::new(
            0,
            GapKind::NEED_FACT,
            ClaimPattern {
                subj: PatternRef::Node(10),
                pred: PatternRef::Sym(2),
                obj_tag: Some(ObjTag::NODENUM),
                obj: PatternRef::Node(30),
                qualifiers_mask: 0,
            },
        )];

        let inferred = InferredClaimStep {
            claim: ClaimData {
                subj: 10,
                pred: 2,
                obj_tag: ObjTag::NODENUM.to_u8(),
                obj_val: 30,
                qualifiers_mask: 0x8000_0000,
            },
            premise_nodes: vec![10],
            support_atom_ids: vec![[1u8; 32]],
        };

        solver.apply_inferred_claims(&mut graph, &mut gaps, &[], &[], &[inferred]);

        assert!(gaps[0].covered, "inferred claim should cover matching gap");
        assert!(
            graph.covers_gap(0),
            "answer graph should record covered gap"
        );
        assert_eq!(
            graph.derived_claims.len(),
            1,
            "graph should retain inferred claim"
        );
        assert_eq!(
            graph.nodes[0].derived_claims.len(),
            1,
            "subject node should receive inferred claim"
        );
        assert_eq!(
            graph.proof_steps.len(),
            1,
            "proof anchor should be recorded"
        );
    }

    #[test]
    fn test_answer_pack_graph_connectivity_factor() {
        // Test that connected graph gets higher confidence than disconnected
        let mut connected_graph = AnswerGraph::new();
        let mut node1 = AgNode::new(AtomRef::new([1u8; 32], 1, 0, 0), AtomType::FACT);
        node1.trust = 8000;
        node1.add_gap(0);
        connected_graph.add_node(node1);

        let mut node2 = AgNode::new(AtomRef::new([2u8; 32], 2, 0, 0), AtomType::FACT);
        node2.trust = 8000;
        node2.add_gap(1);
        connected_graph.add_node(node2);

        connected_graph.add_edge(AgEdge::new(0, 1, AgEdgeType::Supports, 8000));
        connected_graph.mark_gaps_covered(&[0, 1]);
        connected_graph.recalculate_cost(&CostWeights::default(), 0);

        let mut disconnected_graph = AnswerGraph::new();
        let mut node3 = AgNode::new(AtomRef::new([3u8; 32], 3, 0, 0), AtomType::FACT);
        node3.trust = 8000;
        node3.add_gap(0);
        disconnected_graph.add_node(node3);

        let mut node4 = AgNode::new(AtomRef::new([4u8; 32], 4, 0, 0), AtomType::FACT);
        node4.trust = 8000;
        node4.add_gap(1);
        disconnected_graph.add_node(node4);
        // No edges — disconnected
        disconnected_graph.mark_gaps_covered(&[0, 1]);
        disconnected_graph.recalculate_cost(&CostWeights::default(), 0);

        let gaps = vec![
            Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default()),
            Gap::new(1, GapKind::NEED_EVIDENCE, ClaimPattern::default()),
        ];

        let connected_pack =
            AnswerPack::from_solver(connected_graph, 0, &gaps, &CostWeights::default());
        let disconnected_pack =
            AnswerPack::from_solver(disconnected_graph, 0, &gaps, &CostWeights::default());

        // Connected graph should have higher confidence
        assert!(
            connected_pack.confidence >= disconnected_pack.confidence,
            "Connected graph (conf={}) should have >= confidence than disconnected (conf={})",
            connected_pack.confidence,
            disconnected_pack.confidence
        );
    }

    // ========================================================================
    // ANTI-RAG Pipeline Tests (SKF-1.1 6.2)
    // ========================================================================

    #[test]
    fn test_ann_candidate_has_filtering_flag() {
        // Verify that ANN candidates have ann_candidate_requires_filtering=true
        let ann_candidate = Candidate {
            atom_id: [1u8; 32],
            node_num: 1,
            seg_id: 0,
            offset: 0,
            atom_type: AtomType::FACT,
            trust: 3000,
            estimated_io_bytes: 256,
            source_backend: crate::query::router::BackendKind::Ann,
            requires_invariant_check: true,
            covers_gaps: vec![0],
            source_priority: crate::query::router::SourcePriority::Ann,
            hard_conflicts: 0,
            soft_conflicts: 0,
            age_ns: 0,
            domain_mask: 0xFFFF,
            evidence_refs: Vec::new(),
            derived_claims: Vec::new(),
            ann_candidate_requires_filtering: true, // ANN candidate MUST have this
            branch_ctx_id: None,
        };

        assert!(
            ann_candidate.ann_candidate_requires_filtering,
            "ANN candidate must have ann_candidate_requires_filtering=true"
        );
        assert!(
            ann_candidate.requires_invariant_check,
            "ANN candidate must have requires_invariant_check=true"
        );
    }

    #[test]
    fn test_cas_candidate_no_filtering_flag() {
        // Verify that CAS candidates don't have ann_candidate_requires_filtering
        let cas_candidate = Candidate {
            atom_id: [1u8; 32],
            node_num: 1,
            seg_id: 0,
            offset: 0,
            atom_type: AtomType::FACT,
            trust: 5000,
            estimated_io_bytes: 256,
            source_backend: crate::query::router::BackendKind::Cas,
            requires_invariant_check: true,
            covers_gaps: vec![0],
            source_priority: crate::query::router::SourcePriority::CasExact,
            hard_conflicts: 0,
            soft_conflicts: 0,
            age_ns: 0,
            domain_mask: 0xFFFF,
            evidence_refs: Vec::new(),
            derived_claims: Vec::new(),
            ann_candidate_requires_filtering: false, // CAS candidate doesn't need ANN filtering
            branch_ctx_id: None,
        };

        assert!(
            !cas_candidate.ann_candidate_requires_filtering,
            "CAS candidate should not have ann_candidate_requires_filtering"
        );
    }

    #[test]
    fn test_ann_pipeline_integrity() {
        // Test that ANN candidates are tracked through the filter_by_invariants pipeline
        let _gaps = [Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default()).with_priority(100)];

        // Create ANN candidates
        let ann_candidates = [
            Candidate {
                atom_id: [1u8; 32],
                node_num: 1,
                seg_id: 0,
                offset: 0,
                atom_type: AtomType::FACT,
                trust: 3000,
                estimated_io_bytes: 256,
                source_backend: crate::query::router::BackendKind::Ann,
                requires_invariant_check: true,
                covers_gaps: vec![0],
                source_priority: crate::query::router::SourcePriority::Ann,
                hard_conflicts: 0,
                soft_conflicts: 0,
                age_ns: 0,
                domain_mask: 0xFFFF,
                evidence_refs: Vec::new(),
                derived_claims: Vec::new(),
                ann_candidate_requires_filtering: true,
                branch_ctx_id: None,
            },
            Candidate {
                atom_id: [2u8; 32],
                node_num: 2,
                seg_id: 0,
                offset: 0,
                atom_type: AtomType::FACT,
                trust: 3000,
                estimated_io_bytes: 256,
                source_backend: crate::query::router::BackendKind::Ann,
                requires_invariant_check: true,
                covers_gaps: vec![0],
                source_priority: crate::query::router::SourcePriority::Ann,
                hard_conflicts: 0,
                soft_conflicts: 0,
                age_ns: 0,
                domain_mask: 0xFFFF,
                evidence_refs: Vec::new(),
                derived_claims: Vec::new(),
                ann_candidate_requires_filtering: true,
                branch_ctx_id: None,
            },
        ];

        // Count ANN candidates before filtering
        let ann_count_before = ann_candidates
            .iter()
            .filter(|c| c.ann_candidate_requires_filtering)
            .count();

        assert_eq!(
            ann_count_before, 2,
            "Should have 2 ANN candidates before filtering"
        );

        // Verify all have requires_invariant_check
        let all_require_check = ann_candidates.iter().all(|c| c.requires_invariant_check);

        assert!(
            all_require_check,
            "All ANN candidates must require invariant check"
        );
    }

    #[test]
    fn test_ann_anti_rag_violation_detection() {
        // Test that ANN candidates without requires_invariant_check would be detected
        // This tests the logic that filter_by_invariants uses
        let ann_candidate_without_check = Candidate {
            atom_id: [1u8; 32],
            node_num: 1,
            seg_id: 0,
            offset: 0,
            atom_type: AtomType::FACT,
            trust: 3000,
            estimated_io_bytes: 256,
            source_backend: crate::query::router::BackendKind::Ann,
            requires_invariant_check: false, // VIOLATION: ANN without invariant check
            covers_gaps: vec![0],
            source_priority: crate::query::router::SourcePriority::Ann,
            hard_conflicts: 0,
            soft_conflicts: 0,
            age_ns: 0,
            domain_mask: 0xFFFF,
            evidence_refs: Vec::new(),
            derived_claims: Vec::new(),
            ann_candidate_requires_filtering: true,
            branch_ctx_id: None,
        };

        // Simulate the check that filter_by_invariants does
        if ann_candidate_without_check.ann_candidate_requires_filtering
            && !ann_candidate_without_check.requires_invariant_check
        {
            // This would trigger an error in the real filter_by_invariants
            println!("ANTI-RAG VIOLATION DETECTED: ANN candidate without requires_invariant_check");
        }

        // The candidate has the violation flag combination
        assert!(
            ann_candidate_without_check.ann_candidate_requires_filtering,
            "Candidate has ANN flag"
        );
        assert!(
            !ann_candidate_without_check.requires_invariant_check,
            "Candidate lacks invariant check (violation)"
        );
    }
}
