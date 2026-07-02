//! Deterministic retrieval action planner.
//!
//! The planner orders gap retrieval work by expected utility. It does not
//! validate truth and does not replace the fixed-point solver.

use serde::{Deserialize, Serialize};

use crate::query::solver::{Gap, GoalSpec};
use crate::store::api::RetrievalActionTrace;
use crate::store::{GapKind, TrustLevel};

pub type ShardId = [u8; 32];

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederatedPayloadContract {
    pub returns_claims: bool,
    pub returns_provenance: bool,
    pub returns_metadata: bool,
    pub returns_ready_text: bool,
}

impl FederatedPayloadContract {
    pub const fn memoryx_claim_payload() -> Self {
        Self {
            returns_claims: true,
            returns_provenance: true,
            returns_metadata: true,
            returns_ready_text: false,
        }
    }

    pub const fn is_memoryx_source_payload(&self) -> bool {
        self.returns_claims
            && self.returns_provenance
            && self.returns_metadata
            && !self.returns_ready_text
    }
}

impl Default for FederatedPayloadContract {
    fn default() -> Self {
        Self::memoryx_claim_payload()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShardDescriptor {
    pub shard_id: ShardId,
    pub endpoint: String,
    pub trust_level: TrustLevel,
    pub domain_mask: u64,
    pub supported_gap_kinds: Vec<u8>,
    pub payload_contract: FederatedPayloadContract,
}

impl ShardDescriptor {
    pub fn new(shard_id: ShardId, endpoint: impl Into<String>, trust_level: TrustLevel) -> Self {
        Self {
            shard_id,
            endpoint: endpoint.into(),
            trust_level,
            domain_mask: u64::MAX,
            supported_gap_kinds: Vec::new(),
            payload_contract: FederatedPayloadContract::default(),
        }
    }

    pub fn with_domain_mask(mut self, domain_mask: u64) -> Self {
        self.domain_mask = domain_mask;
        self
    }

    pub fn with_gap_kinds(mut self, kinds: impl IntoIterator<Item = GapKind>) -> Self {
        self.supported_gap_kinds = kinds.into_iter().map(GapKind::to_u8).collect();
        self
    }

    pub fn with_payload_contract(mut self, contract: FederatedPayloadContract) -> Self {
        self.payload_contract = contract;
        self
    }

    pub fn supports_gap(&self, gap: &Gap, goal: &GoalSpec) -> bool {
        let trust_ok = self.trust_level >= goal.trust_min;
        let domain_ok = goal.domain_mask == 0 || (self.domain_mask & goal.domain_mask) != 0;
        let kind_ok = self.supported_gap_kinds.is_empty()
            || self.supported_gap_kinds.contains(&gap.kind.to_u8());
        trust_ok && domain_ok && kind_ok && self.payload_contract.is_memoryx_source_payload()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShardRetrievalAction {
    pub action: RetrievalAction,
    pub shard_id: ShardId,
    pub endpoint: String,
    pub shard_trust: TrustLevel,
    pub federated_utility: f32,
    pub payload_contract: FederatedPayloadContract,
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

    pub fn plan_federated(
        gaps: &[Gap],
        goal: &GoalSpec,
        budgets: PlannerBudgets,
        shards: &[ShardDescriptor],
    ) -> Vec<ShardRetrievalAction> {
        let local_actions = Self::plan(gaps, goal, PlannerBudgets::default());
        let mut planned = Vec::new();

        for action in local_actions {
            let Some(gap) = gaps.iter().find(|gap| gap.id == action.gap_id) else {
                continue;
            };

            for shard in shards.iter().filter(|shard| shard.supports_gap(gap, goal)) {
                let trust_factor = (shard.trust_level as f32 / 10_000.0).clamp(0.0, 1.0);
                let domain_factor = if goal.domain_mask != 0
                    && (shard.domain_mask & goal.domain_mask) == goal.domain_mask
                {
                    1.05
                } else {
                    1.0
                };
                planned.push(ShardRetrievalAction {
                    federated_utility: action.utility * (0.5 + trust_factor) * domain_factor,
                    action: action.clone(),
                    shard_id: shard.shard_id,
                    endpoint: shard.endpoint.clone(),
                    shard_trust: shard.trust_level,
                    payload_contract: shard.payload_contract.clone(),
                });
            }
        }

        planned.sort_by(|left, right| {
            right
                .federated_utility
                .partial_cmp(&left.federated_utility)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.action.gap_id.cmp(&right.action.gap_id))
                .then_with(|| left.shard_id.cmp(&right.shard_id))
        });

        let mut selected = Vec::new();
        let mut io_used = 0u32;
        for action in planned {
            if selected.len() >= budgets.max_actions {
                break;
            }
            let action_cost = action.action.execution_cost.ceil() as u32;
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

    #[test]
    fn federated_planner_routes_gaps_to_best_compatible_shard() {
        let gaps =
            vec![Gap::new(7, GapKind::NEED_EVIDENCE, ClaimPattern::default()).with_priority(200)];
        let mut goal = GoalSpec::new(Intent::LOOKUP);
        goal.trust_min = 5000;
        goal.domain_mask = 0b0010;
        let weak_shard = ShardDescriptor::new([1u8; 32], "memoryx://weak", 6000)
            .with_domain_mask(0b0010)
            .with_gap_kinds([GapKind::NEED_EVIDENCE]);
        let strong_shard = ShardDescriptor::new([2u8; 32], "memoryx://strong", 9000)
            .with_domain_mask(0b0010)
            .with_gap_kinds([GapKind::NEED_EVIDENCE]);

        let actions = RetrievalPlanner::plan_federated(
            &gaps,
            &goal,
            PlannerBudgets {
                max_actions: 2,
                max_io_bytes: u32::MAX,
            },
            &[weak_shard, strong_shard],
        );

        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].shard_id, [2u8; 32]);
        assert_eq!(actions[0].action.gap_id, 7);
        assert!(actions[0].payload_contract.is_memoryx_source_payload());
    }

    #[test]
    fn federated_planner_rejects_ready_text_shards() {
        let gaps =
            vec![Gap::new(0, GapKind::NEED_FACT, ClaimPattern::default()).with_priority(200)];
        let goal = GoalSpec::new(Intent::LOOKUP);
        let ready_text_contract = FederatedPayloadContract {
            returns_claims: false,
            returns_provenance: false,
            returns_metadata: false,
            returns_ready_text: true,
        };
        let rag_like_shard = ShardDescriptor::new([9u8; 32], "memoryx://text", 10_000)
            .with_gap_kinds([GapKind::NEED_FACT])
            .with_payload_contract(ready_text_contract);

        let actions = RetrievalPlanner::plan_federated(
            &gaps,
            &goal,
            PlannerBudgets::default(),
            &[rag_like_shard],
        );

        assert!(actions.is_empty());
    }
}
