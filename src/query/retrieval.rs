//! Common retrieval contracts for all MemoryX candidate channels.
//!
//! Retrievers only propose candidates. They do not validate truth, hard
//! constraints, conflicts, or final answer completeness.

use serde::{Deserialize, Serialize};

use crate::query::ConstraintEvaluator;
use crate::query::contract::{ConstraintId, ConstraintStatus};
use crate::query::router::{BackendKind, Candidate};
use crate::query::solver::{Gap, GoalSpec};
use crate::store::{AtomId, NodeNum};

/// Stable reference to a knowledge object returned by a retriever.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeObjectRef {
    Atom(AtomId),
    Node(NodeNum),
}

/// Compact set of matched constraint IDs.
pub type ConstraintBitSet = Vec<ConstraintId>;

/// Why a candidate was retrieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalReason {
    ExactAtom,
    Lexical,
    GraphWalk,
    Semantic,
    Mixed,
}

impl RetrievalReason {
    pub fn from_backend(backend: BackendKind) -> Self {
        match backend {
            BackendKind::Cas => RetrievalReason::ExactAtom,
            BackendKind::Inverted => RetrievalReason::Lexical,
            BackendKind::Graph => RetrievalReason::GraphWalk,
            BackendKind::Ann => RetrievalReason::Semantic,
        }
    }
}

/// Candidate view used by federated/pluggable retrieval channels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateV2 {
    pub object: KnowledgeObjectRef,
    pub matched_constraints: ConstraintBitSet,
    pub retrieval_reason: RetrievalReason,
    pub estimated_gain: f32,
    pub estimated_cost: f32,
    pub source_backend: String,
    pub requires_validation: bool,
    pub legacy_atom_id: Option<AtomId>,
}

impl CandidateV2 {
    pub fn from_candidate(candidate: &Candidate, goal: &GoalSpec) -> Self {
        let matched_constraints = goal
            .constraints
            .iter()
            .filter_map(|constraint| {
                let result = ConstraintEvaluator::evaluate_constraint(constraint, candidate);
                (result.status == ConstraintStatus::Satisfied).then(|| constraint.id.clone())
            })
            .collect();

        let estimated_gain = candidate.covers_gaps.len() as f32;
        let estimated_cost = candidate.estimated_io_bytes.max(1) as f32;

        Self {
            object: KnowledgeObjectRef::Atom(candidate.atom_id),
            matched_constraints,
            retrieval_reason: RetrievalReason::from_backend(candidate.source_backend),
            estimated_gain,
            estimated_cost,
            source_backend: candidate.source_backend.as_str().to_owned(),
            requires_validation: candidate.requires_invariant_check
                || candidate.ann_candidate_requires_filtering,
            legacy_atom_id: Some(candidate.atom_id),
        }
    }
}

/// Common retriever trait. Implementations must not return final answers.
pub trait Retriever {
    fn retrieve(&self, gap: &Gap, goal: &GoalSpec) -> Vec<CandidateV2>;
}
