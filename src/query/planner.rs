//! Deterministic retrieval action planner.
//!
//! The planner orders gap retrieval work by expected utility. It does not
//! validate truth and does not replace the fixed-point solver.

use serde::{Deserialize, Serialize};

use crate::query::solver::{Gap, GoalSpec};
use crate::store::api::RetrievalActionTrace;
use crate::store::{GapKind, TrustLevel};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalAction {
    pub gap_id: u32,
    pub utility: f32,
    pub expected_gap_coverage: f32,
    pub evidence_quality: f32,
    pub constraint_selectivity: f32,
    pub execution_cost: f32,
    pub reason: String,
}

impl RetrievalAction {
    pub fn to_trace(&self, selected: bool) -> RetrievalActionTrace {
        RetrievalActionTrace {
            gap_id: self.gap_id,
            utility: self.utility,
            selected,
            reason: self.reason.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannerBudgets {
    pub max_actions: usize,
    pub max_io_bytes: u32,
}

impl Default for PlannerBudgets {
    fn default() -> Self {
        Self {
            max_actions: usize::MAX,
            max_io_bytes: u32::MAX,
        }
    }
}

pub struct RetrievalPlanner;

impl RetrievalPlanner {
    pub fn plan(gaps: &[Gap], goal: &GoalSpec, budgets: PlannerBudgets) -> Vec<RetrievalAction> {
        let mut planned: Vec<_> = gaps
            .iter()
            .filter(|gap| !gap.covered)
            .map(|gap| Self::score_gap(gap, goal))
            .collect();

        planned.sort_by(|left, right| {
            right
                .utility
                .partial_cmp(&left.utility)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.gap_id.cmp(&right.gap_id))
        });

        let mut selected = Vec::new();
        let mut io_used = 0u32;
        for action in planned {
            if selected.len() >= budgets.max_actions {
                break;
            }
            let action_cost = action.execution_cost.ceil() as u32;
            if io_used.saturating_add(action_cost) > budgets.max_io_bytes {
                continue;
            }
            io_used = io_used.saturating_add(action_cost);
            selected.push(action);
        }

        selected
    }

    fn score_gap(gap: &Gap, goal: &GoalSpec) -> RetrievalAction {
        let expected_gap_coverage = (gap.priority as f32).max(1.0) / 255.0;
        let evidence_quality = evidence_quality(gap.kind);
        let constraint_selectivity = 1.0 + (goal.constraints.len() as f32 * 0.05).min(1.0);
        let execution_cost = execution_cost(gap.kind, goal.trust_min);
        let utility =
            (expected_gap_coverage * evidence_quality * constraint_selectivity) / execution_cost;

        RetrievalAction {
            gap_id: gap.id,
            utility,
            expected_gap_coverage,
            evidence_quality,
            constraint_selectivity,
            execution_cost,
            reason: format!("{:?}", gap.kind),
        }
    }
}

fn evidence_quality(kind: GapKind) -> f32 {
    match kind {
        GapKind::NEED_EVIDENCE => 1.0,
        GapKind::NEED_COUNTEREXAMPLE => 0.95,
        GapKind::NEED_FACT => 0.9,
        GapKind::NEED_DEFINITION => 0.8,
        GapKind::NEED_CAUSAL_CHAIN => 0.75,
        GapKind::NEED_CONSTRAINTS => 0.85,
        GapKind::NEED_COMPARISON_AXIS => 0.7,
        GapKind::NEED_PROCEDURE => 0.65,
    }
}

fn execution_cost(kind: GapKind, trust_min: TrustLevel) -> f32 {
    let base = match kind {
        GapKind::NEED_FACT | GapKind::NEED_DEFINITION => 1.0,
        GapKind::NEED_EVIDENCE | GapKind::NEED_CONSTRAINTS => 1.2,
        GapKind::NEED_COUNTEREXAMPLE | GapKind::NEED_COMPARISON_AXIS => 1.5,
        GapKind::NEED_CAUSAL_CHAIN | GapKind::NEED_PROCEDURE => 2.0,
    };
    base + (trust_min as f32 / 10_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ClaimPattern, Intent};

    #[test]
    fn planner_orders_actions_by_deterministic_utility() {
        let gaps = vec![
            Gap::new(0, GapKind::NEED_PROCEDURE, ClaimPattern::default()).with_priority(50),
            Gap::new(1, GapKind::NEED_EVIDENCE, ClaimPattern::default()).with_priority(200),
            Gap::new(2, GapKind::NEED_FACT, ClaimPattern::default()).with_priority(200),
        ];
        let goal = GoalSpec::new(Intent::LOOKUP);

        let actions = RetrievalPlanner::plan(&gaps, &goal, PlannerBudgets::default());

        assert_eq!(actions[0].gap_id, 2);
        assert_eq!(actions[1].gap_id, 1);
        assert_eq!(actions[2].gap_id, 0);
    }

    #[test]
    fn planner_respects_action_budget() {
        let gaps = vec![
            Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default()).with_priority(100),
            Gap::new(1, GapKind::NEED_EVIDENCE, ClaimPattern::default()).with_priority(200),
        ];
        let goal = GoalSpec::new(Intent::LOOKUP);

        let actions = RetrievalPlanner::plan(
            &gaps,
            &goal,
            PlannerBudgets {
                max_actions: 1,
                max_io_bytes: u32::MAX,
            },
        );

        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].gap_id, 1);
    }
}
