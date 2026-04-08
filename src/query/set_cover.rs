//! Set Cover Algorithm for MemoryX SKF-1.1 Query Engine
//!
//! This module implements a weighted set cover algorithm for building minimal AnswerGraphs.
//! The algorithm uses a greedy cost-benefit ratio approach to select atoms that cover gaps
//! with minimal total cost.
//!
//! # Algorithm
//!
//! The greedy weighted set cover algorithm:
//! 1. While uncovered gaps exist:
//! 2. For each candidate, calculate: benefit/cost ratio
//! 3. Select candidate with best ratio
//! 4. Mark covered gaps
//! 5. Repeat until all gaps covered or no useful candidates remain
//!
//! # Cost Function
//!
//! ```text
//! cost(atom) = wN * 1 + wE * edges + wIO * io_bytes + wT * (100 - trust)
//! ```
//!
//! Where:
//! - wN: Weight per node (default 1.0)
//! - wE: Weight per edge (default 0.2)
//! - wIO: Weight per I/O byte (default 2.0)
//! - wT: Weight for trust penalty (default 10.0)
//! - trust: Normalized trust level (0-100)

#![allow(dead_code)]

use crate::store::api::{CostWeights, Gap, GapId};
use crate::store::{AtomId, AtomType, TrustLevel};
use std::collections::HashSet;

/// A candidate atom that can potentially cover gaps
#[derive(Debug, Clone)]
pub struct AtomCandidate {
    /// Unique atom identifier (BLAKE3-256 hash)
    pub atom_id: AtomId,
    /// Atom type (FACT, DEFINITION, RULE, etc.)
    pub atom_type: AtomType,
    /// Trust level (0-10000, normalized to 0-100 for cost calculation)
    pub trust: TrustLevel,
    /// Number of edges this atom has
    pub edge_count: u32,
    /// Estimated I/O bytes to retrieve this atom
    pub io_bytes: u32,
    /// Set of gap IDs this atom can cover
    pub covers_gaps: Vec<GapId>,
}

impl AtomCandidate {
    /// Create a new atom candidate
    #[inline]
    pub fn new(atom_id: AtomId, atom_type: AtomType) -> Self {
        AtomCandidate {
            atom_id,
            atom_type,
            trust: 5000, // Default trust (50%)
            edge_count: 0,
            io_bytes: 256, // Default estimate
            covers_gaps: Vec::new(),
        }
    }

    /// Create a candidate with specified properties
    #[inline]
    pub fn with_trust(mut self, trust: TrustLevel) -> Self {
        self.trust = trust;
        self
    }

    /// Set edge count
    #[inline]
    pub fn with_edges(mut self, edge_count: u32) -> Self {
        self.edge_count = edge_count;
        self
    }

    /// Set I/O bytes estimate
    #[inline]
    pub fn with_io_bytes(mut self, io_bytes: u32) -> Self {
        self.io_bytes = io_bytes;
        self
    }

    /// Set covered gaps
    #[inline]
    pub fn with_covers_gaps(mut self, gaps: Vec<GapId>) -> Self {
        self.covers_gaps = gaps;
        self
    }

    /// Add a gap that this candidate covers
    #[inline]
    pub fn add_gap(&mut self, gap_id: GapId) {
        self.covers_gaps.push(gap_id);
    }

    /// Calculate cost according to the cost function:
    /// cost(atom) = wN * 1 + wE * edges + wIO * io_bytes + wT * (100 - trust)
    #[inline]
    pub fn calculate_cost(&self, weights: &CostWeights) -> f64 {
        // Normalize trust from 0-10000 to 0-100
        let trust_normalized = (self.trust as f64) / 100.0;
        let trust_penalty = 100.0 - trust_normalized;

        weights.wN * 1.0
            + weights.wE * (self.edge_count as f64)
            + weights.wIO * (self.io_bytes as f64)
            + weights.wT * trust_penalty
    }

    /// Calculate benefit from covering gaps
    #[inline]
    pub fn calculate_benefit(&self, gaps: &[Gap], covered_gaps: &HashSet<GapId>) -> f64 {
        self.covers_gaps
            .iter()
            .filter(|&&gap_id| {
                // Only count gaps that are not yet covered
                !covered_gaps.contains(&gap_id)
            })
            .filter_map(|&gap_id| {
                // Find the gap in the gaps list
                gaps.iter().find(|g| g.id == gap_id)
            })
            .map(|gap| gap.priority as f64)
            .sum()
    }

    /// Calculate cost-benefit ratio
    /// Returns (ratio, benefit, cost) where ratio = benefit / cost
    #[inline]
    pub fn cost_benefit_ratio(
        &self,
        gaps: &[Gap],
        covered_gaps: &HashSet<GapId>,
        weights: &CostWeights,
    ) -> (f64, f64, f64) {
        let benefit = self.calculate_benefit(gaps, covered_gaps);
        let cost = self.calculate_cost(weights);

        let ratio = if cost > 0.0 {
            benefit / cost
        } else {
            f64::INFINITY
        };

        (ratio, benefit, cost)
    }
}

/// Result of the set cover algorithm
#[derive(Debug, Clone)]
pub struct SetCoverResult {
    /// Selected atom IDs that cover all gaps
    pub selected_atoms: Vec<AtomId>,
    /// Total cost of the selected atoms
    pub total_cost: f64,
    /// Coverage ratio (0.0 - 1.0)
    pub coverage_ratio: f64,
    /// Number of gaps covered
    pub gaps_covered: usize,
    /// Total number of gaps
    pub total_gaps: usize,
    /// Indices of selected candidates (for reference)
    pub selected_indices: Vec<usize>,
}

impl SetCoverResult {
    /// Create a new empty result
    #[inline]
    pub fn new(total_gaps: usize) -> Self {
        SetCoverResult {
            selected_atoms: Vec::new(),
            total_cost: 0.0,
            coverage_ratio: 0.0,
            gaps_covered: 0,
            total_gaps,
            selected_indices: Vec::new(),
        }
    }

    /// Check if all gaps were covered
    #[inline]
    pub fn is_complete(&self) -> bool {
        self.gaps_covered >= self.total_gaps
    }

    /// Get number of selected atoms
    #[inline]
    pub fn atom_count(&self) -> usize {
        self.selected_atoms.len()
    }
}

/// Set Cover Solver using greedy weighted set cover algorithm
///
/// This solver implements a greedy algorithm that iteratively selects
/// the atom with the best cost-benefit ratio until all gaps are covered
/// or no more useful candidates remain.
pub struct SetCoverSolver;

impl SetCoverSolver {
    /// Solve the weighted set cover problem
    ///
    /// # Arguments
    ///
    /// * `gaps` - The gaps that need to be covered
    /// * `candidates` - Atoms that can potentially cover gaps
    /// * `weights` - Cost weights for the cost function
    ///
    /// # Returns
    ///
    /// A `SetCoverResult` containing the selected atoms, total cost, and coverage ratio
    ///
    /// # Algorithm
    ///
    /// 1. Initialize uncovered gaps set
    /// 2. While uncovered gaps exist:
    ///    a. For each candidate, calculate marginal benefit / cost
    ///    b. Select candidate with maximum benefit/cost ratio
    ///    c. Add candidate to solution, mark gaps as covered
    /// 3. Return selected atoms and statistics
    pub fn solve(
        gaps: &[Gap],
        candidates: &[AtomCandidate],
        weights: &CostWeights,
    ) -> SetCoverResult {
        let total_gaps = gaps.len();

        if total_gaps == 0 {
            // No gaps to cover - empty solution
            return SetCoverResult::new(0);
        }

        if candidates.is_empty() {
            // No candidates available
            return SetCoverResult::new(total_gaps);
        }

        // Track covered gaps
        let mut covered_gaps: HashSet<GapId> = HashSet::new();
        let mut selected_atoms: Vec<AtomId> = Vec::new();
        let mut selected_indices: Vec<usize> = Vec::new();
        let mut total_cost: f64 = 0.0;
        let mut available_candidates: Vec<usize> = (0..candidates.len()).collect();

        // Greedy selection loop
        while covered_gaps.len() < total_gaps && !available_candidates.is_empty() {
            let mut best_idx: Option<usize> = None;
            let mut best_ratio = f64::NEG_INFINITY;
            let mut best_cost = 0.0;

            // Find candidate with best cost-benefit ratio
            for &idx in &available_candidates {
                let candidate = &candidates[idx];

                let (ratio, benefit, cost) =
                    candidate.cost_benefit_ratio(gaps, &covered_gaps, weights);

                // Only consider candidates that provide positive benefit
                if benefit > 0.0 && ratio > best_ratio {
                    best_ratio = ratio;
                    best_cost = cost;
                    best_idx = Some(idx);
                }
            }

            // If no candidate provides benefit, stop
            let idx = match best_idx {
                Some(idx) => idx,
                None => break,
            };

            let candidate = &candidates[idx];

            // Add to solution
            selected_atoms.push(candidate.atom_id);
            selected_indices.push(idx);
            total_cost += best_cost;

            // Mark gaps as covered
            for &gap_id in &candidate.covers_gaps {
                covered_gaps.insert(gap_id);
            }

            // Remove from available candidates
            available_candidates.retain(|&i| i != idx);
        }

        // Calculate coverage ratio
        let gaps_covered = covered_gaps.len();
        let coverage_ratio = if total_gaps > 0 {
            gaps_covered as f64 / total_gaps as f64
        } else {
            1.0
        };

        SetCoverResult {
            selected_atoms,
            total_cost,
            coverage_ratio,
            gaps_covered,
            total_gaps,
            selected_indices,
        }
    }

    /// Solve with a maximum cost budget
    ///
    /// Stops when the total cost exceeds the budget, even if not all gaps are covered.
    pub fn solve_with_budget(
        gaps: &[Gap],
        candidates: &[AtomCandidate],
        weights: &CostWeights,
        max_cost: f64,
    ) -> SetCoverResult {
        let total_gaps = gaps.len();

        if total_gaps == 0 {
            return SetCoverResult::new(0);
        }

        if candidates.is_empty() {
            return SetCoverResult::new(total_gaps);
        }

        let mut covered_gaps: HashSet<GapId> = HashSet::new();
        let mut selected_atoms: Vec<AtomId> = Vec::new();
        let mut selected_indices: Vec<usize> = Vec::new();
        let mut total_cost: f64 = 0.0;
        let mut available_candidates: Vec<usize> = (0..candidates.len()).collect();

        while covered_gaps.len() < total_gaps && !available_candidates.is_empty() {
            // Check budget
            if total_cost >= max_cost {
                break;
            }

            let mut best_idx: Option<usize> = None;
            let mut best_ratio = f64::NEG_INFINITY;
            let mut best_cost = 0.0;

            for &idx in &available_candidates {
                let candidate = &candidates[idx];
                let (ratio, benefit, cost) =
                    candidate.cost_benefit_ratio(gaps, &covered_gaps, weights);

                // Check if adding this candidate would exceed budget
                if total_cost + cost > max_cost && benefit > 0.0 {
                    // Skip if it would exceed budget
                    continue;
                }

                if benefit > 0.0 && ratio > best_ratio {
                    best_ratio = ratio;
                    best_cost = cost;
                    best_idx = Some(idx);
                }
            }

            let idx = match best_idx {
                Some(idx) => idx,
                None => break,
            };

            let candidate = &candidates[idx];

            selected_atoms.push(candidate.atom_id);
            selected_indices.push(idx);
            total_cost += best_cost;

            for &gap_id in &candidate.covers_gaps {
                covered_gaps.insert(gap_id);
            }

            available_candidates.retain(|&i| i != idx);
        }

        let gaps_covered = covered_gaps.len();
        let coverage_ratio = if total_gaps > 0 {
            gaps_covered as f64 / total_gaps as f64
        } else {
            1.0
        };

        SetCoverResult {
            selected_atoms,
            total_cost,
            coverage_ratio,
            gaps_covered,
            total_gaps,
            selected_indices,
        }
    }

    /// Get the gaps covered by a set of selected candidates
    pub fn get_covered_gaps(
        candidates: &[AtomCandidate],
        selected_indices: &[usize],
    ) -> HashSet<GapId> {
        let mut covered = HashSet::new();
        for &idx in selected_indices {
            if let Some(candidate) = candidates.get(idx) {
                for &gap_id in &candidate.covers_gaps {
                    covered.insert(gap_id);
                }
            }
        }
        covered
    }

    /// Calculate the total cost of a solution
    pub fn calculate_solution_cost(
        candidates: &[AtomCandidate],
        selected_indices: &[usize],
        weights: &CostWeights,
    ) -> f64 {
        selected_indices
            .iter()
            .filter_map(|&idx| candidates.get(idx))
            .map(|c| c.calculate_cost(weights))
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::api::CostWeights;
    use crate::store::{AtomType, ClaimPattern, GapKind};

    /// Helper function to create a test gap
    fn create_gap(id: GapId, kind: GapKind, priority: u8) -> Gap {
        Gap::new(id, kind, ClaimPattern::default()).with_priority(priority)
    }

    /// Helper function to create a test atom ID
    fn create_atom_id(seed: u8) -> AtomId {
        let mut id = [0u8; 32];
        id[0] = seed;
        id
    }

    #[test]
    fn test_basic_coverage() {
        // Create 3 gaps
        let gaps = vec![
            create_gap(0, GapKind::NEED_FACT, 100),
            create_gap(1, GapKind::NEED_DEFINITION, 150),
            create_gap(2, GapKind::NEED_EVIDENCE, 80),
        ];

        // Create candidates that cover each gap
        let candidates = vec![
            AtomCandidate::new(create_atom_id(1), AtomType::FACT)
                .with_covers_gaps(vec![0])
                .with_trust(8000),
            AtomCandidate::new(create_atom_id(2), AtomType::DEFINITION)
                .with_covers_gaps(vec![1])
                .with_trust(9000),
            AtomCandidate::new(create_atom_id(3), AtomType::FACT)
                .with_covers_gaps(vec![2])
                .with_trust(7000),
        ];

        let weights = CostWeights::default();
        let result = SetCoverSolver::solve(&gaps, &candidates, &weights);

        assert_eq!(result.gaps_covered, 3);
        assert_eq!(result.total_gaps, 3);
        assert_eq!(result.coverage_ratio, 1.0);
        assert!(result.is_complete());
        assert_eq!(result.atom_count(), 3);
    }

    #[test]
    fn test_multiple_gaps_covered_by_one_atom() {
        // Create 4 gaps
        let gaps = vec![
            create_gap(0, GapKind::NEED_FACT, 100),
            create_gap(1, GapKind::NEED_FACT, 100),
            create_gap(2, GapKind::NEED_DEFINITION, 150),
            create_gap(3, GapKind::NEED_EVIDENCE, 80),
        ];

        // Create candidates where one covers multiple gaps
        let candidates = vec![
            AtomCandidate::new(create_atom_id(1), AtomType::DATASET)
                .with_covers_gaps(vec![0, 1]) // Covers 2 gaps
                .with_trust(7500)
                .with_io_bytes(512),
            AtomCandidate::new(create_atom_id(2), AtomType::DEFINITION)
                .with_covers_gaps(vec![2])
                .with_trust(9000),
            AtomCandidate::new(create_atom_id(3), AtomType::FACT)
                .with_covers_gaps(vec![3])
                .with_trust(7000),
        ];

        let weights = CostWeights::default();
        let result = SetCoverSolver::solve(&gaps, &candidates, &weights);

        assert_eq!(result.gaps_covered, 4);
        assert_eq!(result.coverage_ratio, 1.0);
        assert!(result.is_complete());
        // Should select the multi-gap candidate plus 2 others = 3 atoms
        assert_eq!(result.atom_count(), 3);
    }

    #[test]
    fn test_cost_benefit_ordering() {
        // Create 2 gaps with different priorities
        let gaps = vec![
            create_gap(0, GapKind::NEED_DEFINITION, 200), // High priority
            create_gap(1, GapKind::NEED_FACT, 50),        // Low priority
        ];

        // Create candidates with different costs
        // Candidate 1: High cost but covers high priority gap
        // Candidate 2: Low cost but covers low priority gap
        // Candidate 3: Medium cost, covers both (should be selected first)
        let candidates = vec![
            AtomCandidate::new(create_atom_id(1), AtomType::DEFINITION)
                .with_covers_gaps(vec![0])
                .with_trust(5000)
                .with_io_bytes(1000), // High I/O cost
            AtomCandidate::new(create_atom_id(2), AtomType::FACT)
                .with_covers_gaps(vec![1])
                .with_trust(5000)
                .with_io_bytes(100), // Low I/O cost
            AtomCandidate::new(create_atom_id(3), AtomType::DATASET)
                .with_covers_gaps(vec![0, 1]) // Covers both!
                .with_trust(8000)
                .with_io_bytes(500), // Medium I/O cost
        ];

        let weights = CostWeights::default();
        let result = SetCoverSolver::solve(&gaps, &candidates, &weights);

        assert_eq!(result.gaps_covered, 2);
        assert!(result.is_complete());
        // Should select candidate 3 (covers both) as first choice
        assert_eq!(result.selected_atoms[0], create_atom_id(3));
        // Should only need 1 atom since candidate 3 covers all gaps
        assert_eq!(result.atom_count(), 1);
    }

    #[test]
    fn test_no_candidates() {
        let gaps = vec![
            create_gap(0, GapKind::NEED_FACT, 100),
            create_gap(1, GapKind::NEED_DEFINITION, 150),
        ];

        let candidates: Vec<AtomCandidate> = vec![];
        let weights = CostWeights::default();
        let result = SetCoverSolver::solve(&gaps, &candidates, &weights);

        assert_eq!(result.gaps_covered, 0);
        assert_eq!(result.coverage_ratio, 0.0);
        assert!(!result.is_complete());
        assert_eq!(result.atom_count(), 0);
    }

    #[test]
    fn test_all_gaps_already_covered() {
        // Test when one candidate covers all gaps
        let gaps = vec![
            create_gap(0, GapKind::NEED_FACT, 100),
            create_gap(1, GapKind::NEED_DEFINITION, 150),
        ];

        let candidates = vec![AtomCandidate::new(create_atom_id(1), AtomType::DATASET)
            .with_covers_gaps(vec![0, 1]) // Covers all gaps
            .with_trust(9000)];

        let weights = CostWeights::default();
        let result = SetCoverSolver::solve(&gaps, &candidates, &weights);

        assert_eq!(result.gaps_covered, 2);
        assert_eq!(result.coverage_ratio, 1.0);
        assert!(result.is_complete());
        assert_eq!(result.atom_count(), 1);
    }

    #[test]
    fn test_cost_calculation() {
        let candidate = AtomCandidate::new(create_atom_id(1), AtomType::FACT)
            .with_edges(5)
            .with_io_bytes(256)
            .with_trust(8000); // 80% trust

        let weights = CostWeights::default();
        let cost = candidate.calculate_cost(&weights);

        // Expected: wN * 1 + wE * edges + wIO * io_bytes + wT * (100 - trust)
        // = 1.0 * 1 + 0.2 * 5 + 2.0 * 256 + 10.0 * (100 - 80)
        // = 1.0 + 1.0 + 512.0 + 200.0
        // = 714.0
        let expected = 1.0 + 1.0 + 512.0 + 200.0;
        assert!(
            (cost - expected).abs() < 0.001,
            "Expected {}, got {}",
            expected,
            cost
        );
    }

    #[test]
    fn test_benefit_calculation() {
        let gaps = vec![
            create_gap(0, GapKind::NEED_FACT, 100),
            create_gap(1, GapKind::NEED_DEFINITION, 200),
        ];

        let candidate =
            AtomCandidate::new(create_atom_id(1), AtomType::FACT).with_covers_gaps(vec![0, 1]);

        let covered = HashSet::new();
        let benefit = candidate.calculate_benefit(&gaps, &covered);

        assert_eq!(benefit, 300.0); // 100 + 200
    }

    #[test]
    fn test_marginal_benefit() {
        let gaps = vec![
            create_gap(0, GapKind::NEED_FACT, 100),
            create_gap(1, GapKind::NEED_DEFINITION, 200),
        ];

        let candidate =
            AtomCandidate::new(create_atom_id(1), AtomType::FACT).with_covers_gaps(vec![0, 1]);

        // Gap 0 is already covered
        let mut covered = HashSet::new();
        covered.insert(0);

        let benefit = candidate.calculate_benefit(&gaps, &covered);

        // Only gap 1 provides marginal benefit
        assert_eq!(benefit, 200.0);
    }

    #[test]
    fn test_budget_constraint() {
        let gaps = vec![
            create_gap(0, GapKind::NEED_FACT, 100),
            create_gap(1, GapKind::NEED_DEFINITION, 200),
            create_gap(2, GapKind::NEED_EVIDENCE, 150),
        ];

        // Expensive candidate that covers all gaps
        let candidates = vec![
            AtomCandidate::new(create_atom_id(1), AtomType::DATASET)
                .with_covers_gaps(vec![0, 1, 2])
                .with_trust(5000)
                .with_io_bytes(10000), // Very expensive
        ];

        let weights = CostWeights::default();
        // Set a very low budget
        let result = SetCoverSolver::solve_with_budget(&gaps, &candidates, &weights, 100.0);

        // Should not be able to afford the expensive candidate
        assert_eq!(result.gaps_covered, 0);
        assert!(!result.is_complete());
    }

    #[test]
    fn test_trust_penalty() {
        // Two candidates covering the same gap, different trust
        let gaps = vec![create_gap(0, GapKind::NEED_FACT, 100)];

        let candidates = vec![
            AtomCandidate::new(create_atom_id(1), AtomType::FACT)
                .with_covers_gaps(vec![0])
                .with_trust(10000), // Maximum trust (no penalty)
            AtomCandidate::new(create_atom_id(2), AtomType::FACT)
                .with_covers_gaps(vec![0])
                .with_trust(5000), // Medium trust
        ];

        let weights = CostWeights::default();
        let result = SetCoverSolver::solve(&gaps, &candidates, &weights);

        // Should select the high-trust candidate (lower cost due to trust penalty)
        assert_eq!(result.selected_atoms.len(), 1);
        assert_eq!(result.selected_atoms[0], create_atom_id(1));
    }

    #[test]
    fn test_partial_coverage() {
        // More gaps than candidates can cover
        let gaps = vec![
            create_gap(0, GapKind::NEED_FACT, 100),
            create_gap(1, GapKind::NEED_DEFINITION, 200),
            create_gap(2, GapKind::NEED_EVIDENCE, 150),
        ];

        // Only covers 2 gaps
        let candidates = vec![
            AtomCandidate::new(create_atom_id(1), AtomType::FACT)
                .with_covers_gaps(vec![0])
                .with_trust(8000),
            AtomCandidate::new(create_atom_id(2), AtomType::DEFINITION)
                .with_covers_gaps(vec![1])
                .with_trust(8000),
        ];

        let weights = CostWeights::default();
        let result = SetCoverSolver::solve(&gaps, &candidates, &weights);

        assert_eq!(result.gaps_covered, 2);
        assert_eq!(result.total_gaps, 3);
        assert_eq!(result.coverage_ratio, 2.0 / 3.0);
        assert!(!result.is_complete());
    }

    #[test]
    fn test_empty_gaps() {
        let gaps: Vec<Gap> = vec![];
        let candidates =
            vec![AtomCandidate::new(create_atom_id(1), AtomType::FACT).with_covers_gaps(vec![0])];

        let weights = CostWeights::default();
        let result = SetCoverSolver::solve(&gaps, &candidates, &weights);

        assert_eq!(result.gaps_covered, 0);
        assert_eq!(result.total_gaps, 0);
        assert_eq!(result.coverage_ratio, 0.0);
        assert!(result.is_complete()); // Empty is considered complete
        assert_eq!(result.atom_count(), 0);
    }

    #[test]
    fn test_candidate_with_no_gaps() {
        let gaps = vec![create_gap(0, GapKind::NEED_FACT, 100)];

        let candidates = vec![
            AtomCandidate::new(create_atom_id(1), AtomType::FACT).with_covers_gaps(vec![]), // Covers no gaps
            AtomCandidate::new(create_atom_id(2), AtomType::FACT).with_covers_gaps(vec![0]),
        ];

        let weights = CostWeights::default();
        let result = SetCoverSolver::solve(&gaps, &candidates, &weights);

        // Should only select candidate 2
        assert_eq!(result.gaps_covered, 1);
        assert_eq!(result.atom_count(), 1);
        assert_eq!(result.selected_atoms[0], create_atom_id(2));
    }

    #[test]
    fn test_covered_gaps_helper() {
        let candidates = vec![
            AtomCandidate::new(create_atom_id(1), AtomType::FACT).with_covers_gaps(vec![0, 1]),
            AtomCandidate::new(create_atom_id(2), AtomType::DEFINITION).with_covers_gaps(vec![2]),
        ];

        let covered = SetCoverSolver::get_covered_gaps(&candidates, &[0, 1]);

        assert_eq!(covered.len(), 3);
        assert!(covered.contains(&0));
        assert!(covered.contains(&1));
        assert!(covered.contains(&2));
    }

    #[test]
    fn test_solution_cost_calculation() {
        let candidates = vec![
            AtomCandidate::new(create_atom_id(1), AtomType::FACT)
                .with_io_bytes(100)
                .with_trust(10000),
            AtomCandidate::new(create_atom_id(2), AtomType::DEFINITION)
                .with_io_bytes(200)
                .with_trust(10000),
        ];

        let weights = CostWeights::default();
        let cost = SetCoverSolver::calculate_solution_cost(&candidates, &[0, 1], &weights);

        // cost = (wN * 1 + wIO * 100 + wT * 0) + (wN * 1 + wIO * 200 + wT * 0)
        // = (1.0 + 200.0 + 0) + (1.0 + 400.0 + 0)
        // = 201.0 + 401.0
        // = 602.0
        let expected = 602.0;
        assert!(
            (cost - expected).abs() < 0.001,
            "Expected {}, got {}",
            expected,
            cost
        );
    }
}
