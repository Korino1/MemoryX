//! Advanced Cost Calculator for MemoryX SKF-1.1 Query Engine
//!
//! This module implements the cost function from SKF-1.1 Section 5.1:
//! ```text
//! cost = wN * nodes + wE * edges + wIO * io_bytes + wC * hard_conflicts
//!        + wS * soft_conflicts + wT * trust_penalty + wA * age_penalty
//! ```
//!
//! # Components
//!
//! - **AtomCost**: Cost calculation for individual atoms
//! - **AnswerGraphCost**: Total cost calculation for AnswerGraph structures
//! - **BenefitCostRatio**: Optimization metric for set cover algorithms
//!
//! # Safety Invariants
//!
//! - All cost calculations use saturating arithmetic to prevent overflow
//! - Trust penalties are bounded to avoid division by zero
//! - Age calculations handle edge cases (now_ns = 0, age_ns > now_ns)

use crate::store::api::{AgNode, AnswerGraph, CostWeights, GapId};
use crate::vm::AtomView;

// ============================================================================
// Cost Components
// ============================================================================

/// Individual cost component breakdown for debugging and analysis
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostBreakdown {
    /// Cost from nodes (wN * node_count)
    pub node_cost: f64,
    /// Cost from edges (wE * edge_count)
    pub edge_cost: f64,
    /// Cost from I/O bytes (wIO * io_bytes)
    pub io_cost: f64,
    /// Cost from hard conflicts (wC * hard_conflicts)
    pub hard_conflict_cost: f64,
    /// Cost from soft conflicts (wS * soft_conflicts)
    pub soft_conflict_cost: f64,
    /// Cost from trust penalty (wT * trust_penalty)
    pub trust_cost: f64,
    /// Cost from age penalty (wA * age_penalty)
    pub age_cost: f64,
    /// Cost from domain penalty (wD * domain_penalty)
    pub domain_cost: f64,
}

impl CostBreakdown {
    /// Create a new empty cost breakdown
    #[inline]
    pub const fn new() -> Self {
        CostBreakdown {
            node_cost: 0.0,
            edge_cost: 0.0,
            io_cost: 0.0,
            hard_conflict_cost: 0.0,
            soft_conflict_cost: 0.0,
            trust_cost: 0.0,
            age_cost: 0.0,
            domain_cost: 0.0,
        }
    }

    /// Calculate total cost from all components
    #[inline]
    pub fn total(&self) -> f64 {
        self.node_cost
            + self.edge_cost
            + self.io_cost
            + self.hard_conflict_cost
            + self.soft_conflict_cost
            + self.trust_cost
            + self.age_cost
            + self.domain_cost
    }

    /// Get the dominant cost component (largest contributor)
    pub fn dominant_component(&self) -> (&'static str, f64) {
        let components = [
            ("nodes", self.node_cost),
            ("edges", self.edge_cost),
            ("io", self.io_cost),
            ("hard_conflicts", self.hard_conflict_cost),
            ("soft_conflicts", self.soft_conflict_cost),
            ("trust", self.trust_cost),
            ("age", self.age_cost),
            ("domain", self.domain_cost),
        ];

        components
            .iter()
            .copied()
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(("none", 0.0))
    }

    /// Check if any hard conflicts exist
    #[inline]
    pub fn has_hard_conflicts(&self) -> bool {
        self.hard_conflict_cost > 0.0
    }

    /// Check if cost exceeds threshold
    #[inline]
    pub fn exceeds_threshold(&self, threshold: f64) -> bool {
        self.total() > threshold
    }
}

impl Default for CostBreakdown {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Atom Cost Calculator
// ============================================================================

/// Cost calculator for individual atoms
///
/// Calculates the cost contribution of a single atom based on its
/// metadata, I/O requirements, and quality metrics.
#[derive(Debug, Clone)]
pub struct AtomCostCalculator<'a> {
    weights: &'a CostWeights,
    now_ns: u64,
}

impl<'a> AtomCostCalculator<'a> {
    /// Create a new atom cost calculator
    #[inline]
    pub const fn new(weights: &'a CostWeights, now_ns: u64) -> Self {
        AtomCostCalculator { weights, now_ns }
    }

    /// Calculate cost for an atom view
    ///
    /// # Cost Components
    /// - Base node cost (wN)
    /// - I/O cost based on metadata size (wIO * meta.len())
    /// - Trust penalty (lower trust = higher cost)
    /// - Age penalty (older atoms = higher cost)
    /// - Domain penalty (missing domain = higher cost)
    pub fn calculate(&self, atom: &AtomView) -> f64 {
        let breakdown = self.calculate_breakdown(atom);
        breakdown.total()
    }

    /// Calculate cost breakdown for detailed analysis
    pub fn calculate_breakdown(&self, atom: &AtomView) -> CostBreakdown {
        let mut breakdown = CostBreakdown::new();

        // Base node cost (count = 1 for single atom)
        breakdown.node_cost = self.weights.wN;

        // I/O cost based on metadata size + estimated overhead
        let io_bytes = atom.meta.len() as f64 + self.estimate_claims_size(atom);
        breakdown.io_cost = self.weights.wIO * io_bytes;

        // Trust penalty (inverse relationship)
        breakdown.trust_cost = self.calculate_trust_penalty(atom.trust_level);

        // Age penalty
        breakdown.age_cost = self.calculate_age_penalty(atom.valid_from_ns);

        // Domain penalty (if no domain specified)
        breakdown.domain_cost = self.calculate_domain_penalty(atom.domain_mask);

        breakdown
    }

    /// Calculate trust penalty
    ///
    /// Trust penalty is inversely proportional to trust level.
    /// Lower trust = higher penalty.
    #[inline]
    pub fn calculate_trust_penalty(&self, trust_level: u16) -> f64 {
        // Trust level is 0-10000, normalize to 0-1
        let normalized_trust = (trust_level as f64 / 10000.0).clamp(0.01, 1.0);
        // Inverse: lower trust = higher penalty
        self.weights.wT * (1.0 / normalized_trust - 1.0)
    }

    /// Calculate age penalty
    ///
    /// Age penalty increases linearly with age in years.
    /// Uses `now_ns` as reference time.
    #[inline]
    pub fn calculate_age_penalty(&self, valid_from_ns: u64) -> f64 {
        if self.now_ns == 0 || valid_from_ns == 0 || valid_from_ns > self.now_ns {
            return 0.0;
        }

        let age_ns = self.now_ns.saturating_sub(valid_from_ns);
        let age_years = age_ns as f64 / (365.0 * 24.0 * 60.0 * 60.0 * 1_000_000_000.0);

        self.weights.wA * age_years
    }

    /// Calculate domain penalty
    ///
    /// Atoms without domain classification incur a penalty.
    #[inline]
    pub fn calculate_domain_penalty(&self, domain_mask: u64) -> f64 {
        if domain_mask == 0 {
            self.weights.wD
        } else {
            0.0
        }
    }

    /// Estimate claims size for I/O cost calculation
    #[inline]
    fn estimate_claims_size(&self, atom: &AtomView) -> f64 {
        // Each claim is approximately 32 bytes (ClaimData size)
        atom.claims.len() as f64 * 32.0
    }

    /// Calculate effective trust score (0.0 - 1.0)
    #[inline]
    pub fn effective_trust(&self, trust_level: u16) -> f64 {
        (trust_level as f64 / 10000.0).clamp(0.0, 1.0)
    }
}

// ============================================================================
// Answer Graph Cost Calculator
// ============================================================================

/// Cost calculator for AnswerGraph structures
///
/// Calculates the total cost of an answer graph including all nodes,
/// edges, and their associated penalties.
#[derive(Debug, Clone)]
pub struct AnswerGraphCostCalculator<'a> {
    weights: &'a CostWeights,
    now_ns: u64,
}

impl<'a> AnswerGraphCostCalculator<'a> {
    /// Create a new answer graph cost calculator
    #[inline]
    pub const fn new(weights: &'a CostWeights, now_ns: u64) -> Self {
        AnswerGraphCostCalculator { weights, now_ns }
    }

    /// Calculate total cost for an answer graph
    ///
    /// # Cost Components
    /// - Node costs: sum of all node costs
    /// - Edge costs: wE * edge_count
    /// - Conflict costs: sum of hard/soft conflicts
    pub fn calculate(&self, ag: &AnswerGraph) -> f64 {
        let breakdown = self.calculate_breakdown(ag);
        breakdown.total()
    }

    /// Calculate detailed cost breakdown for an answer graph
    pub fn calculate_breakdown(&self, ag: &AnswerGraph) -> CostBreakdown {
        let mut breakdown = CostBreakdown::new();

        // Node costs: sum of individual node contributions
        for node in &ag.nodes {
            let node_cost = self.calculate_node_cost(node);
            breakdown.node_cost += node_cost.node_cost;
            breakdown.io_cost += node_cost.io_cost;
            breakdown.trust_cost += node_cost.trust_cost;
            breakdown.age_cost += node_cost.age_cost;
            breakdown.domain_cost += node_cost.domain_cost;
            breakdown.hard_conflict_cost += node_cost.hard_conflict_cost;
            breakdown.soft_conflict_cost += node_cost.soft_conflict_cost;
        }

        // Edge costs
        breakdown.edge_cost = self.weights.wE * ag.edges.len() as f64;

        breakdown
    }

    /// Calculate cost for a single AG node
    fn calculate_node_cost(&self, node: &AgNode) -> CostBreakdown {
        let mut breakdown = CostBreakdown::new();

        // Base node cost
        breakdown.node_cost = self.weights.wN;

        // I/O cost
        breakdown.io_cost = self.weights.wIO * (node.io_bytes as f64);

        // Trust penalty
        breakdown.trust_cost = self.calculate_trust_penalty(node.trust);

        // Age penalty
        breakdown.age_cost = self.calculate_age_penalty(node.age_ns);

        // Domain penalty
        breakdown.domain_cost = self.calculate_domain_penalty(node.domain_mask);

        // Conflict costs
        breakdown.hard_conflict_cost = self.weights.wC * node.hard_conflicts as f64;
        breakdown.soft_conflict_cost = self.weights.wS * node.soft_conflicts as f64;

        breakdown
    }

    /// Calculate trust penalty for a trust level
    #[inline]
    fn calculate_trust_penalty(&self, trust_level: u16) -> f64 {
        let normalized_trust = (trust_level as f64 / 10000.0).clamp(0.01, 1.0);
        self.weights.wT * (1.0 / normalized_trust - 1.0)
    }

    /// Calculate age penalty
    #[inline]
    fn calculate_age_penalty(&self, age_ns: u64) -> f64 {
        if self.now_ns == 0 || age_ns == 0 || age_ns > self.now_ns {
            return 0.0;
        }

        let age_delta = self.now_ns.saturating_sub(age_ns);
        let age_years = age_delta as f64 / (365.0 * 24.0 * 60.0 * 60.0 * 1_000_000_000.0);

        self.weights.wA * age_years
    }

    /// Calculate domain penalty
    #[inline]
    fn calculate_domain_penalty(&self, domain_mask: u64) -> f64 {
        if domain_mask == 0 {
            self.weights.wD
        } else {
            0.0
        }
    }

    /// Calculate average cost per node
    pub fn average_cost_per_node(&self, ag: &AnswerGraph) -> f64 {
        if ag.nodes.is_empty() {
            0.0
        } else {
            self.calculate(ag) / ag.nodes.len() as f64
        }
    }

    /// Calculate cost efficiency (coverage per unit cost)
    pub fn cost_efficiency(&self, ag: &AnswerGraph) -> f64 {
        let total_cost = self.calculate(ag);
        if total_cost <= 0.0 {
            return f64::INFINITY;
        }
        ag.covered_gaps.len() as f64 / total_cost
    }
}

// ============================================================================
// Main Cost Calculator
// ============================================================================

/// Main cost calculator combining all cost calculation functions
///
/// This is the primary interface for cost calculation in the query engine.
/// It provides methods for calculating atom costs, answer graph costs,
/// and benefit/cost ratios for set cover optimization.
#[derive(Debug, Clone)]
pub struct CostCalculator {
    weights: CostWeights,
    now_ns: u64,
}

impl CostCalculator {
    /// Create a new cost calculator with default weights
    #[inline]
    pub fn new(weights: CostWeights) -> Self {
        CostCalculator { weights, now_ns: 0 }
    }

    /// Create with current timestamp for age calculations
    #[inline]
    pub fn with_timestamp(mut self, now_ns: u64) -> Self {
        self.now_ns = now_ns;
        self
    }

    /// Get the cost weights
    #[inline]
    pub fn weights(&self) -> &CostWeights {
        &self.weights
    }

    /// Get mutable access to weights
    #[inline]
    pub fn weights_mut(&mut self) -> &mut CostWeights {
        &mut self.weights
    }

    /// Update weights
    #[inline]
    pub fn set_weights(&mut self, weights: CostWeights) {
        self.weights = weights;
    }

    /// Calculate cost for an individual atom
    ///
    /// # Example
    /// ```
    /// use memoryx::query::CostCalculator;
    /// use memoryx::store::api::CostWeights;
    ///
    /// let calculator = CostCalculator::new(CostWeights::default());
    /// // let cost = calculator.atom_cost(&atom_view);
    /// ```
    pub fn atom_cost(&self, atom: &AtomView) -> f64 {
        let calculator = AtomCostCalculator::new(&self.weights, self.now_ns);
        calculator.calculate(atom)
    }

    /// Calculate detailed cost breakdown for an atom
    pub fn atom_cost_breakdown(&self, atom: &AtomView) -> CostBreakdown {
        let calculator = AtomCostCalculator::new(&self.weights, self.now_ns);
        calculator.calculate_breakdown(atom)
    }

    /// Calculate total cost for an AnswerGraph
    ///
    /// # Example
    /// ```
    /// use memoryx::query::CostCalculator;
    /// use memoryx::store::api::{CostWeights, AnswerGraph};
    ///
    /// let calculator = CostCalculator::new(CostWeights::default());
    /// // let cost = calculator.ag_cost(&answer_graph);
    /// ```
    pub fn ag_cost(&self, ag: &AnswerGraph) -> f64 {
        let calculator = AnswerGraphCostCalculator::new(&self.weights, self.now_ns);
        calculator.calculate(ag)
    }

    /// Calculate detailed cost breakdown for an AnswerGraph
    pub fn ag_cost_breakdown(&self, ag: &AnswerGraph) -> CostBreakdown {
        let calculator = AnswerGraphCostCalculator::new(&self.weights, self.now_ns);
        calculator.calculate_breakdown(ag)
    }

    /// Calculate benefit/cost ratio for set cover optimization
    ///
    /// This ratio helps prioritize which atoms to include in the answer graph.
    /// Higher ratio = better value (more benefit per unit cost).
    ///
    /// # Arguments
    /// - `atom`: The atom to evaluate
    /// - `gaps_covered`: List of gap IDs this atom covers
    /// - `gap_priorities`: Optional slice of gap priorities for benefit calculation
    ///
    /// # Returns
    /// - `f64`: Benefit/cost ratio (higher is better)
    /// - Returns `f64::INFINITY` if cost is zero
    ///
    /// # Example
    /// ```
    /// use memoryx::query::CostCalculator;
    /// use memoryx::store::api::{CostWeights, GapId};
    ///
    /// let calculator = CostCalculator::new(CostWeights::default());
    /// let gaps_covered: Vec<GapId> = vec![0, 1, 2];
    /// // let ratio = calculator.benefit_cost_ratio(&atom_view, &gaps_covered, Some(&[100, 150, 200]));
    /// ```
    pub fn benefit_cost_ratio(
        &self,
        atom: &AtomView,
        gaps_covered: &[GapId],
        gap_priorities: Option<&[u8]>,
    ) -> f64 {
        let cost = self.atom_cost(atom);
        let benefit = self.calculate_benefit(gaps_covered, gap_priorities);

        if cost <= 0.0 {
            f64::INFINITY
        } else {
            benefit / cost
        }
    }

    /// Calculate benefit/cost ratio for an AgNode (used in set cover)
    ///
    /// This variant uses the pre-computed cost stored in the node.
    pub fn node_benefit_cost_ratio(&self, node: &AgNode, gap_priorities: Option<&[u8]>) -> f64 {
        let gaps: Vec<GapId> = node.gaps_covered.iter().copied().collect();
        let cost = node.cost;
        let benefit = self.calculate_benefit(&gaps, gap_priorities);

        if cost <= 0.0 {
            f64::INFINITY
        } else {
            benefit / cost
        }
    }

    /// Calculate marginal benefit/cost ratio for set cover
    ///
    /// Only counts benefit from gaps not already covered.
    pub fn marginal_benefit_cost_ratio(
        &self,
        atom: &AtomView,
        gaps_covered: &[GapId],
        already_covered: &[GapId],
        gap_priorities: Option<&[u8]>,
    ) -> f64 {
        // Filter to only uncovered gaps
        let already_covered_set: std::collections::HashSet<GapId> =
            already_covered.iter().copied().collect();
        let marginal_gaps: Vec<GapId> = gaps_covered
            .iter()
            .copied()
            .filter(|g| !already_covered_set.contains(g))
            .collect();

        if marginal_gaps.is_empty() {
            return 0.0;
        }

        let cost = self.atom_cost(atom);
        let benefit = self.calculate_benefit(&marginal_gaps, gap_priorities);

        if cost <= 0.0 {
            f64::INFINITY
        } else {
            benefit / cost
        }
    }

    /// Calculate benefit from covering gaps
    ///
    /// Benefit is the sum of priorities of covered gaps.
    /// If priorities not provided, assumes uniform priority of 100.
    fn calculate_benefit(&self, gaps_covered: &[GapId], gap_priorities: Option<&[u8]>) -> f64 {
        match gap_priorities {
            Some(priorities) => gaps_covered
                .iter()
                .filter_map(|&gap_id| priorities.get(gap_id as usize))
                .map(|&p| p as f64)
                .sum(),
            None => gaps_covered.len() as f64 * 100.0,
        }
    }

    /// Calculate normalized cost (0.0 to 1.0 scale)
    ///
    /// Useful for comparing costs across different graph sizes.
    pub fn normalized_cost(&self, ag: &AnswerGraph, max_expected_cost: f64) -> f64 {
        let cost = self.ag_cost(ag);
        (cost / max_expected_cost).clamp(0.0, 1.0)
    }

    /// Check if answer graph is within budget
    #[inline]
    pub fn within_budget(&self, ag: &AnswerGraph, budget: f64) -> bool {
        self.ag_cost(ag) <= budget
    }

    /// Calculate cost difference between two answer graphs
    #[inline]
    pub fn cost_difference(&self, ag1: &AnswerGraph, ag2: &AnswerGraph) -> f64 {
        (self.ag_cost(ag1) - self.ag_cost(ag2)).abs()
    }

    /// Estimate cost savings from removing a node
    pub fn estimate_savings(&self, ag: &AnswerGraph, node_idx: usize) -> f64 {
        if node_idx >= ag.nodes.len() {
            return 0.0;
        }

        let node = &ag.nodes[node_idx];
        let calculator = AnswerGraphCostCalculator::new(&self.weights, self.now_ns);
        let node_cost = calculator.calculate_node_cost(node);

        // Also count edges that would be removed
        let edge_count = ag
            .edges
            .iter()
            .filter(|e| e.src_idx == node_idx || e.dst_idx == node_idx)
            .count();
        let edge_savings = self.weights.wE * edge_count as f64;

        node_cost.total() + edge_savings
    }
}

impl Default for CostCalculator {
    fn default() -> Self {
        Self::new(CostWeights::default())
    }
}

// ============================================================================
// Cost Comparison Utilities
// ============================================================================

/// Compare two answer graphs by cost
pub fn compare_by_cost(
    ag1: &AnswerGraph,
    ag2: &AnswerGraph,
    weights: &CostWeights,
    now_ns: u64,
) -> std::cmp::Ordering {
    let calculator = CostCalculator::new(*weights).with_timestamp(now_ns);
    let cost1 = calculator.ag_cost(ag1);
    let cost2 = calculator.ag_cost(ag2);

    cost1
        .partial_cmp(&cost2)
        .unwrap_or(std::cmp::Ordering::Equal)
}

/// Find the lowest cost graph from a collection
pub fn find_lowest_cost<'a>(
    graphs: &'a [AnswerGraph],
    weights: &CostWeights,
    now_ns: u64,
) -> Option<&'a AnswerGraph> {
    let calculator = CostCalculator::new(*weights).with_timestamp(now_ns);

    graphs.iter().min_by(|a, b| {
        let cost_a = calculator.ag_cost(a);
        let cost_b = calculator.ag_cost(b);
        cost_a
            .partial_cmp(&cost_b)
            .unwrap_or(std::cmp::Ordering::Equal)
    })
}

/// Calculate pareto frontier (graphs that are not dominated by others)
pub fn pareto_frontier<'a>(
    graphs: &'a [AnswerGraph],
    weights: &CostWeights,
    now_ns: u64,
) -> Vec<&'a AnswerGraph> {
    let calculator = CostCalculator::new(*weights).with_timestamp(now_ns);

    graphs
        .iter()
        .filter(|&g| {
            // A graph is on the pareto frontier if no other graph has
            // both lower cost and higher coverage
            let g_cost = calculator.ag_cost(g);
            let g_coverage = g.covered_gaps.len();

            !graphs.iter().any(|other| {
                if std::ptr::eq(g, other) {
                    return false;
                }
                let other_cost = calculator.ag_cost(other);
                let other_coverage = other.covered_gaps.len();

                other_cost <= g_cost
                    && other_coverage >= g_coverage
                    && (other_cost < g_cost || other_coverage > g_coverage)
            })
        })
        .collect()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{AtomType, TrustLevel};
    use crate::vm::ClaimData;

    fn create_test_atom_view(trust: TrustLevel, domain: u64, age_ns: u64) -> AtomView<'static> {
        // Create static data for the atom view
        let atom_id: &[u8; 32] = Box::leak(Box::new([1u8; 32]));
        let meta: &[u8] = Box::leak(vec![0u8; 64].into_boxed_slice());
        let claims: &[ClaimData] = Box::leak(Box::new([
            ClaimData {
                subj: 1,
                pred: 2,
                obj_tag: 3,
                obj_val: 42,
                qualifiers_mask: 0,
            },
            ClaimData {
                subj: 4,
                pred: 5,
                obj_tag: 6,
                obj_val: 100,
                qualifiers_mask: 1,
            },
        ]));

        AtomView::new(
            atom_id,
            AtomType::FACT,
            meta,
            claims,
            age_ns,
            age_ns + 1_000_000_000,
            trust,
            domain,
            1,
        )
    }

    fn create_test_ag_node(trust: u16, io_bytes: u32, conflicts: (u32, u32)) -> AgNode {
        let atom_ref = crate::store::api::AtomRef::new([1u8; 32], 0, 0, 0);
        let mut node = AgNode::new(atom_ref, AtomType::FACT);
        node.trust = trust;
        node.io_bytes = io_bytes;
        node.hard_conflicts = conflicts.0;
        node.soft_conflicts = conflicts.1;
        node.age_ns = 0;
        node.domain_mask = 0xFFFF;
        node
    }

    #[test]
    fn test_atom_cost_calculator() {
        let weights = CostWeights::default();
        let calculator = AtomCostCalculator::new(&weights, 1_000_000_000_000);

        let atom = create_test_atom_view(8000, 0xFFFF, 500_000_000_000);
        let cost = calculator.calculate(&atom);

        // Cost should be positive
        assert!(cost > 0.0, "Cost should be positive");

        // Higher trust should give lower cost
        let high_trust_atom = create_test_atom_view(9000, 0xFFFF, 500_000_000_000);
        let low_trust_atom = create_test_atom_view(1000, 0xFFFF, 500_000_000_000);

        let high_trust_cost = calculator.calculate(&high_trust_atom);
        let low_trust_cost = calculator.calculate(&low_trust_atom);

        assert!(
            low_trust_cost > high_trust_cost,
            "Low trust should have higher cost"
        );
    }

    #[test]
    fn test_trust_penalty_calculation() {
        let weights = CostWeights::default();
        let calculator = AtomCostCalculator::new(&weights, 0);

        // Maximum trust (10000) should have minimum penalty
        let max_penalty = calculator.calculate_trust_penalty(10000);
        assert!(
            (0.0..0.1).contains(&max_penalty),
            "Max trust should have near-zero penalty"
        );

        // Low trust should have high penalty
        let min_penalty = calculator.calculate_trust_penalty(100);
        assert!(
            min_penalty > max_penalty,
            "Low trust should have higher penalty"
        );
    }

    #[test]
    fn test_age_penalty_calculation() {
        let weights = CostWeights::default();
        let now_ns = 1_000_000_000_000_000u64; // Use larger value
        let calculator = AtomCostCalculator::new(&weights, now_ns);

        // Recent atom should have low age penalty
        let recent_penalty = calculator.calculate_age_penalty(now_ns - 1_000_000_000);
        assert!(recent_penalty >= 0.0, "Age penalty should be non-negative");

        // Old atom should have higher penalty (1 year ago)
        let old_penalty = calculator.calculate_age_penalty(now_ns - 365_000_000_000_000u64);
        assert!(
            old_penalty > recent_penalty,
            "Old atom should have higher penalty"
        );
    }

    #[test]
    fn test_domain_penalty() {
        let weights = CostWeights::default();
        let calculator = AtomCostCalculator::new(&weights, 0);

        // No domain should have penalty
        let no_domain = calculator.calculate_domain_penalty(0);
        assert_eq!(
            no_domain, weights.wD,
            "No domain should incur domain penalty"
        );

        // With domain should have no penalty
        let with_domain = calculator.calculate_domain_penalty(0xFFFF);
        assert_eq!(with_domain, 0.0, "With domain should have no penalty");
    }

    #[test]
    fn test_answer_graph_cost() {
        let weights = CostWeights::default();
        let calculator = AnswerGraphCostCalculator::new(&weights, 0);

        let mut ag = AnswerGraph::new();
        ag.add_node(create_test_ag_node(5000, 256, (0, 0)));
        ag.add_node(create_test_ag_node(6000, 256, (0, 0)));
        ag.add_edge(crate::store::api::AgEdge::new(
            0,
            1,
            crate::store::api::AgEdgeType::Supports,
            5000,
        ));

        let cost = calculator.calculate(&ag);

        // Should include node and edge costs
        assert!(
            cost >= 2.0 * weights.wN + weights.wE,
            "Cost should include nodes and edges"
        );
    }

    #[test]
    fn test_cost_breakdown() {
        let weights = CostWeights::default();
        let calculator = CostCalculator::new(weights);

        let atom = create_test_atom_view(5000, 0xFFFF, 0);
        let breakdown = calculator.atom_cost_breakdown(&atom);

        // All components should be non-negative
        assert!(breakdown.node_cost >= 0.0);
        assert!(breakdown.io_cost >= 0.0);
        assert!(breakdown.trust_cost >= 0.0);
        assert!(breakdown.age_cost >= 0.0);
        assert!(breakdown.domain_cost >= 0.0);

        // Total should equal sum of components
        let total = breakdown.total();
        let sum = breakdown.node_cost
            + breakdown.io_cost
            + breakdown.trust_cost
            + breakdown.age_cost
            + breakdown.domain_cost;
        assert!(
            (total - sum).abs() < 0.001,
            "Total should equal sum of components"
        );
    }

    #[test]
    fn test_benefit_cost_ratio() {
        let weights = CostWeights::default();
        let calculator = CostCalculator::new(weights);

        let atom = create_test_atom_view(8000, 0xFFFF, 0);
        let gaps_covered: Vec<GapId> = vec![0, 1, 2];
        let priorities: Vec<u8> = vec![100, 150, 200];

        let ratio = calculator.benefit_cost_ratio(&atom, &gaps_covered, Some(&priorities));

        // Should be positive finite value
        assert!(
            ratio > 0.0 && ratio.is_finite(),
            "Ratio should be positive and finite"
        );

        // More gaps should give higher ratio
        let more_gaps: Vec<GapId> = vec![0, 1, 2, 3, 4];
        let more_priorities: Vec<u8> = vec![100, 150, 200, 100, 100];
        let higher_ratio = calculator.benefit_cost_ratio(&atom, &more_gaps, Some(&more_priorities));

        assert!(higher_ratio > ratio, "More gaps should give higher ratio");
    }

    #[test]
    fn test_marginal_benefit_cost_ratio() {
        let weights = CostWeights::default();
        let calculator = CostCalculator::new(weights);

        let atom = create_test_atom_view(8000, 0xFFFF, 0);
        let gaps_covered: Vec<GapId> = vec![0, 1, 2];
        let already_covered: Vec<GapId> = vec![0];

        let marginal =
            calculator.marginal_benefit_cost_ratio(&atom, &gaps_covered, &already_covered, None);

        let full = calculator.benefit_cost_ratio(&atom, &gaps_covered, None);

        // Marginal should be less than full (since one gap already covered)
        assert!(
            marginal < full,
            "Marginal ratio should be less than full ratio"
        );
    }

    #[test]
    fn test_hard_conflict_cost() {
        let weights = CostWeights::default();
        let calculator = CostCalculator::new(weights);

        let mut ag1 = AnswerGraph::new();
        ag1.add_node(create_test_ag_node(5000, 256, (0, 0)));

        let mut ag2 = AnswerGraph::new();
        ag2.add_node(create_test_ag_node(5000, 256, (1, 0)));

        let cost1 = calculator.ag_cost(&ag1);
        let cost2 = calculator.ag_cost(&ag2);

        // Graph with hard conflict should have higher cost
        assert!(cost2 > cost1, "Hard conflict should increase cost");
        assert!(
            cost2 - cost1 >= weights.wC,
            "Cost difference should be at least wC"
        );
    }

    #[test]
    fn test_soft_conflict_cost() {
        let weights = CostWeights::default();
        let calculator = CostCalculator::new(weights);

        let mut ag1 = AnswerGraph::new();
        ag1.add_node(create_test_ag_node(5000, 256, (0, 0)));

        let mut ag2 = AnswerGraph::new();
        ag2.add_node(create_test_ag_node(5000, 256, (0, 1)));

        let cost1 = calculator.ag_cost(&ag1);
        let cost2 = calculator.ag_cost(&ag2);

        // Graph with soft conflict should have higher cost
        assert!(cost2 > cost1, "Soft conflict should increase cost");
        assert!(
            cost2 - cost1 >= weights.wS,
            "Cost difference should be at least wS"
        );
    }

    #[test]
    fn test_cost_efficiency() {
        let weights = CostWeights::default();
        let calculator = AnswerGraphCostCalculator::new(&weights, 0);

        let mut ag = AnswerGraph::new();
        ag.add_node(create_test_ag_node(5000, 256, (0, 0)));
        ag.add_node(create_test_ag_node(6000, 256, (0, 0)));

        // Mark some gaps as covered
        ag.mark_gaps_covered(&[0, 1, 2]);

        let efficiency = calculator.cost_efficiency(&ag);
        assert!(efficiency > 0.0, "Efficiency should be positive");
    }

    #[test]
    fn test_compare_by_cost() {
        let weights = CostWeights::default();

        let mut ag1 = AnswerGraph::new();
        ag1.add_node(create_test_ag_node(5000, 256, (0, 0)));

        let mut ag2 = AnswerGraph::new();
        ag2.add_node(create_test_ag_node(5000, 256, (0, 0)));
        ag2.add_node(create_test_ag_node(5000, 256, (0, 0)));

        let ordering = compare_by_cost(&ag1, &ag2, &weights, 0);
        assert_eq!(
            ordering,
            std::cmp::Ordering::Less,
            "Single node should be less than two nodes"
        );
    }

    #[test]
    fn test_find_lowest_cost() {
        let weights = CostWeights::default();

        let mut ag1 = AnswerGraph::new();
        ag1.add_node(create_test_ag_node(5000, 256, (0, 0)));

        let mut ag2 = AnswerGraph::new();
        ag2.add_node(create_test_ag_node(5000, 256, (0, 0)));
        ag2.add_node(create_test_ag_node(5000, 256, (0, 0)));

        let graphs = vec![ag1, ag2];
        let lowest = find_lowest_cost(&graphs, &weights, 0);

        assert!(lowest.is_some());
        assert_eq!(lowest.unwrap().node_count(), 1);
    }

    #[test]
    fn test_estimate_savings() {
        let weights = CostWeights::default();
        let calculator = CostCalculator::new(weights);

        let mut ag = AnswerGraph::new();
        ag.add_node(create_test_ag_node(5000, 256, (0, 0)));
        ag.add_node(create_test_ag_node(5000, 256, (0, 0)));
        ag.add_edge(crate::store::api::AgEdge::new(
            0,
            1,
            crate::store::api::AgEdgeType::Supports,
            5000,
        ));

        let savings = calculator.estimate_savings(&ag, 0);
        assert!(savings > 0.0, "Savings should be positive");
    }

    #[test]
    fn test_within_budget() {
        let weights = CostWeights::default();
        let calculator = CostCalculator::new(weights);

        let mut ag = AnswerGraph::new();
        ag.add_node(create_test_ag_node(5000, 256, (0, 0)));

        let total_cost = calculator.ag_cost(&ag);

        assert!(
            calculator.within_budget(&ag, total_cost * 2.0),
            "Should be within large budget"
        );
        assert!(
            !calculator.within_budget(&ag, total_cost / 2.0),
            "Should exceed small budget"
        );
    }

    #[test]
    fn test_pareto_frontier() {
        let weights = CostWeights::default();

        // Create graphs with different cost/coverage tradeoffs
        let mut ag1 = AnswerGraph::new(); // Low cost, low coverage
        ag1.add_node(create_test_ag_node(5000, 256, (0, 0)));
        ag1.mark_gaps_covered(&[0]);

        let mut ag2 = AnswerGraph::new(); // High cost, high coverage
        ag2.add_node(create_test_ag_node(5000, 256, (0, 0)));
        ag2.add_node(create_test_ag_node(5000, 256, (0, 0)));
        ag2.mark_gaps_covered(&[0, 1, 2]);

        let graphs = vec![ag1, ag2];
        let frontier = pareto_frontier(&graphs, &weights, 0);

        // Both should be on frontier (different tradeoffs)
        assert_eq!(frontier.len(), 2);
    }
}
