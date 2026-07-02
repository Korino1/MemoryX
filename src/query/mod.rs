//! Query Engine for MemoryX SKF-1.1
//!
//! This module implements the query compilation, goal specification,
//! gap generation, and fixed-point answer solving system.
//!
//! # Architecture
//!
//! 1. **IntentClassifier**: Query string -> Intent classification
//! 2. **GoalSpec**: Compiled query representation with intent, constraints, entities
//! 3. **GapGenerator**: Intent-based gap template instantiation
//! 4. **BackwardWave**: Generate gaps from GoalSpec
//! 5. **ForwardWave**: Forward inference from candidates
//! 6. **SetCover**: Minimal covering algorithm for AnswerGraph
//! 7. **FixedPointSolver**: Iterative solver that builds AnswerGraph
//! 8. **CostCalculator**: Advanced cost calculation for optimization

#![allow(dead_code)]

// Stable public query contract for MCP and external API calls
pub mod contract;
pub use contract::*;

// Deterministic natural-language compiler for QueryContract
pub mod compiler;
pub use compiler::QueryContractCompiler;

// Deterministic constraint evaluation
pub mod constraints;
pub use constraints::{ConstraintEvaluator, ConstraintFacts, ConstraintSubject};

// Common retriever contracts for candidate-producing channels
pub mod retrieval;
pub use retrieval::{
    CandidateV2, ConstraintBitSet, KnowledgeObjectRef, RetrievalReason, Retriever,
};

// Re-export all types from solver module
pub use solver::*;

// Re-export intent classifier
pub mod intent;
pub use intent::{IntentClassification, IntentClassifier, StructuredQuery};

// Re-export gap generator
pub mod gap_generator;
pub use gap_generator::GapGenerator;

// Set Cover solver module
pub mod set_cover;
pub use set_cover::{AtomCandidate, SetCoverResult, SetCoverSolver};

// Deterministic retrieval planner
pub mod planner;
pub use planner::{PlannerBudgets, RetrievalAction, RetrievalPlanner};

// Cost calculation module
pub mod cost;
pub use cost::{AnswerGraphCostCalculator, AtomCostCalculator, CostBreakdown, CostCalculator};

// Submodule for the complete solver implementation (must come BEFORE router)
mod solver;

// Router module (uses types solver exports via super::)
pub mod router;
// Re-export all types from router module
pub use router::*;

// ANN (Approximate Nearest Neighbors) module
pub mod ann;
// Re-export main ANN types
pub use ann::{AnnBackend, EmbeddingIndex, HnswGraph, cosine_similarity};
