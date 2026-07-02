//! Stable query contract for MCP and external API callers.
//!
//! This layer describes what the caller asks for before the request is lowered
//! into internal solver structures such as `GoalSpec`.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;

use crate::store::{DomainMask, Intent};

/// Public, serializable query request accepted by higher-level APIs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryContract {
    pub intent: ContractIntent,
    pub targets: Vec<EntityPattern>,
    pub relations: Vec<RelationRequirement>,
    pub constraints: Vec<Constraint>,
    pub quantifiers: Vec<QuantifiedCondition>,
    pub temporal_scope: TemporalScope,
    pub context_scope: ContextScope,
    pub source_policy: SourcePolicy,
    pub evidence_policy: EvidencePolicy,
    pub freshness_policy: FreshnessPolicy,
    pub ambiguity_policy: AmbiguityPolicy,
    pub conflict_policy: ConflictPolicy,
    pub completeness_policy: CompletenessPolicy,
    pub output_contract: OutputContract,
    pub budgets: QueryBudgets,
}

impl QueryContract {
    pub fn new(intent: ContractIntent) -> Self {
        Self {
            intent,
            targets: Vec::new(),
            relations: Vec::new(),
            constraints: Vec::new(),
            quantifiers: Vec::new(),
            temporal_scope: TemporalScope::default(),
            context_scope: ContextScope::default(),
            source_policy: SourcePolicy::default(),
            evidence_policy: EvidencePolicy::default(),
            freshness_policy: FreshnessPolicy::default(),
            ambiguity_policy: AmbiguityPolicy::default(),
            conflict_policy: ConflictPolicy::default(),
            completeness_policy: CompletenessPolicy::default(),
            output_contract: OutputContract::default(),
            budgets: QueryBudgets::default(),
        }
    }

    pub fn with_target(mut self, target: EntityPattern) -> Self {
        self.targets.push(target);
        self
    }

    pub fn with_constraint(mut self, constraint: Constraint) -> Self {
        self.constraints.push(constraint);
        self
    }

    pub fn with_relation(mut self, relation: RelationRequirement) -> Self {
        self.relations.push(relation);
        self
    }

    pub fn required_constraints(&self) -> impl Iterator<Item = &Constraint> {
        self.constraints
            .iter()
            .filter(|constraint| constraint.strength == ConstraintStrength::Must)
    }

    pub fn forbidden_constraints(&self) -> impl Iterator<Item = &Constraint> {
        self.constraints
            .iter()
            .filter(|constraint| constraint.strength == ConstraintStrength::MustNot)
    }

    pub fn should_constraints(&self) -> impl Iterator<Item = &Constraint> {
        self.constraints
            .iter()
            .filter(|constraint| matches!(constraint.strength, ConstraintStrength::Should { .. }))
    }

    pub fn validate(&self) -> Result<(), QueryContractError> {
        if self.targets.is_empty() && self.relations.is_empty() && self.constraints.is_empty() {
            return Err(QueryContractError::EmptyContract);
        }

        let mut constraint_ids = HashSet::with_capacity(self.constraints.len());
        for constraint in &self.constraints {
            if !constraint_ids.insert(constraint.id.clone()) {
                return Err(QueryContractError::DuplicateConstraintId(
                    constraint.id.clone(),
                ));
            }

            if let ConstraintStrength::Should { weight } = constraint.strength
                && !(0.0..=1.0).contains(&weight)
            {
                return Err(QueryContractError::InvalidShouldWeight {
                    id: constraint.id.clone(),
                    weight,
                });
            }
        }

        for quantifier in &self.quantifiers {
            for constraint_id in &quantifier.constraint_ids {
                if !constraint_ids.contains(constraint_id) {
                    return Err(QueryContractError::UnknownConstraintReference(
                        constraint_id.clone(),
                    ));
                }
            }
        }

        if self.budgets.max_atoms == 0 {
            return Err(QueryContractError::InvalidBudget("max_atoms"));
        }

        if self.budgets.max_iterations == 0 {
            return Err(QueryContractError::InvalidBudget("max_iterations"));
        }

        Ok(())
    }
}

/// Intent vocabulary used at API boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractIntent {
    Lookup,
    Define,
    Explain,
    Compare,
    Derive,
    Verify,
    Plan,
}

impl From<ContractIntent> for Intent {
    fn from(value: ContractIntent) -> Self {
        match value {
            ContractIntent::Lookup => Intent::LOOKUP,
            ContractIntent::Define => Intent::DEFINE,
            ContractIntent::Explain => Intent::EXPLAIN,
            ContractIntent::Compare => Intent::COMPARE,
            ContractIntent::Derive => Intent::DERIVE,
            ContractIntent::Verify => Intent::VERIFY,
            ContractIntent::Plan => Intent::PLAN,
        }
    }
}

/// Entity selector that can name a concrete atom/node or a symbolic target.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntityPattern {
    pub id: Option<String>,
    pub label: Option<String>,
    pub entity_type: Option<String>,
    pub aliases: Vec<String>,
    pub domain_mask: Option<DomainMask>,
}

impl EntityPattern {
    pub fn label(label: impl Into<String>) -> Self {
        Self {
            id: None,
            label: Some(label.into()),
            entity_type: None,
            aliases: Vec::new(),
            domain_mask: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RelationRequirement {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub strength: ConstraintStrength,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Constraint {
    pub id: ConstraintId,
    pub strength: ConstraintStrength,
    pub target: ConstraintTarget,
    pub operator: ConstraintOperator,
    pub value: ConstraintValue,
    pub description: Option<String>,
}

impl Constraint {
    pub fn must(
        id: impl Into<ConstraintId>,
        target: ConstraintTarget,
        operator: ConstraintOperator,
        value: ConstraintValue,
    ) -> Self {
        Self {
            id: id.into(),
            strength: ConstraintStrength::Must,
            target,
            operator,
            value,
            description: None,
        }
    }

    pub fn must_not(
        id: impl Into<ConstraintId>,
        target: ConstraintTarget,
        operator: ConstraintOperator,
        value: ConstraintValue,
    ) -> Self {
        Self {
            id: id.into(),
            strength: ConstraintStrength::MustNot,
            target,
            operator,
            value,
            description: None,
        }
    }

    pub fn should(
        id: impl Into<ConstraintId>,
        target: ConstraintTarget,
        operator: ConstraintOperator,
        value: ConstraintValue,
        weight: f32,
    ) -> Self {
        Self {
            id: id.into(),
            strength: ConstraintStrength::Should { weight },
            target,
            operator,
            value,
            description: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConstraintId(pub String);

impl From<&str> for ConstraintId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for ConstraintId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintStrength {
    Must,
    MustNot,
    Should { weight: f32 },
}

impl Eq for ConstraintStrength {}

impl std::hash::Hash for ConstraintStrength {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            ConstraintStrength::Must => 0_u8.hash(state),
            ConstraintStrength::MustNot => 1_u8.hash(state),
            ConstraintStrength::Should { weight } => {
                2_u8.hash(state);
                weight.to_bits().hash(state);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintTarget {
    Entity,
    EntityType,
    Predicate,
    Relation,
    Source,
    Evidence,
    Time,
    Context,
    Domain,
    NumericMetric,
    Text,
    Custom(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintOperator {
    Eq,
    Ne,
    Contains,
    Exists,
    Matches,
    Before,
    After,
    During,
    Within,
    Gte,
    Lte,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintValue {
    None,
    Bool(bool),
    Text(String),
    Number(f64),
    TimeRange(TimeRangeSpec),
    Ref(String),
    List(Vec<ConstraintValue>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct QuantifiedCondition {
    pub quantifier: Quantifier,
    pub constraint_ids: Vec<ConstraintId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quantifier {
    All,
    Any,
    AtLeast(u32),
    Exactly(u32),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TemporalScope {
    pub time_range: Option<TimeRangeSpec>,
    pub require_current: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeRangeSpec {
    pub from_unix_ns: Option<u64>,
    pub to_unix_ns: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextScope {
    pub policy_id: Option<u32>,
    pub branch_ids: Vec<String>,
    pub include_conflicting_branches: bool,
}

impl Default for ContextScope {
    fn default() -> Self {
        Self {
            policy_id: None,
            branch_ids: Vec::new(),
            include_conflicting_branches: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePolicy {
    pub allowed_sources: Vec<String>,
    pub forbidden_sources: Vec<String>,
    pub require_provenance: bool,
    pub allow_federated_sources: bool,
}

impl Default for SourcePolicy {
    fn default() -> Self {
        Self {
            allowed_sources: Vec::new(),
            forbidden_sources: Vec::new(),
            require_provenance: true,
            allow_federated_sources: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidencePolicy {
    pub min_evidence_items: u32,
    pub require_direct_evidence: bool,
    pub allow_inferred_claims: bool,
    pub include_rejected_candidates: bool,
}

impl Default for EvidencePolicy {
    fn default() -> Self {
        Self {
            min_evidence_items: 1,
            require_direct_evidence: false,
            allow_inferred_claims: true,
            include_rejected_candidates: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshnessPolicy {
    pub max_age_unix_ns: Option<u64>,
    pub stale_behavior: StaleBehavior,
}

impl Default for FreshnessPolicy {
    fn default() -> Self {
        Self {
            max_age_unix_ns: None,
            stale_behavior: StaleBehavior::MarkStale,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleBehavior {
    Allow,
    MarkStale,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AmbiguityPolicy {
    pub allow_ambiguous_targets: bool,
    pub require_disambiguation_notes: bool,
}

impl Default for AmbiguityPolicy {
    fn default() -> Self {
        Self {
            allow_ambiguous_targets: true,
            require_disambiguation_notes: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictPolicy {
    pub include_conflicts: bool,
    pub fail_on_hard_conflict: bool,
    pub prefer_latest_branch: bool,
}

impl Default for ConflictPolicy {
    fn default() -> Self {
        Self {
            include_conflicts: true,
            fail_on_hard_conflict: false,
            prefer_latest_branch: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletenessPolicy {
    pub require_minimal_proof_subgraph: bool,
    pub expose_unknowns: bool,
    pub expose_unsatisfied_constraints: bool,
}

impl Default for CompletenessPolicy {
    fn default() -> Self {
        Self {
            require_minimal_proof_subgraph: true,
            expose_unknowns: true,
            expose_unsatisfied_constraints: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputContract {
    pub format: OutputFormat,
    pub include_answer_graph: bool,
    pub include_confidence: bool,
    pub include_provenance: bool,
    pub include_execution_trace: bool,
    pub max_items: u32,
}

impl Default for OutputContract {
    fn default() -> Self {
        Self {
            format: OutputFormat::StructuredJson,
            include_answer_graph: true,
            include_confidence: true,
            include_provenance: true,
            include_execution_trace: false,
            max_items: 64,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    StructuredJson,
    TextSummary,
    EvidenceTable,
    MinimalGraph,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryBudgets {
    pub max_iterations: u32,
    pub max_atoms: u32,
    pub max_edges: u32,
    pub max_io_bytes: u64,
    pub max_time_ms: u64,
    pub max_federated_calls: u32,
}

impl Default for QueryBudgets {
    fn default() -> Self {
        Self {
            max_iterations: 100,
            max_atoms: 4096,
            max_edges: 8192,
            max_io_bytes: 64 * 1024 * 1024,
            max_time_ms: 30_000,
            max_federated_calls: 16,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum QueryContractError {
    EmptyContract,
    DuplicateConstraintId(ConstraintId),
    UnknownConstraintReference(ConstraintId),
    InvalidShouldWeight { id: ConstraintId, weight: f32 },
    InvalidBudget(&'static str),
}

impl fmt::Display for QueryContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryContractError::EmptyContract => write!(f, "query contract is empty"),
            QueryContractError::DuplicateConstraintId(id) => {
                write!(f, "duplicate constraint id: {}", id.0)
            }
            QueryContractError::UnknownConstraintReference(id) => {
                write!(f, "unknown constraint reference: {}", id.0)
            }
            QueryContractError::InvalidShouldWeight { id, weight } => write!(
                f,
                "invalid should constraint weight for {}: {}",
                id.0, weight
            ),
            QueryContractError::InvalidBudget(field) => write!(f, "invalid query budget: {field}"),
        }
    }
}

impl std::error::Error for QueryContractError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_roundtrips_through_json() {
        let contract = QueryContract::new(ContractIntent::Explain)
            .with_target(EntityPattern::label("MemoryX MCP"))
            .with_constraint(Constraint::must(
                "has_provenance",
                ConstraintTarget::Evidence,
                ConstraintOperator::Exists,
                ConstraintValue::Bool(true),
            ))
            .with_constraint(Constraint::must_not(
                "avoid_chunk_only_answer",
                ConstraintTarget::Custom("answer_model".to_owned()),
                ConstraintOperator::Eq,
                ConstraintValue::Text("chunk_only".to_owned()),
            ))
            .with_constraint(Constraint::should(
                "prefer_project_scope",
                ConstraintTarget::Context,
                ConstraintOperator::Contains,
                ConstraintValue::Text("project".to_owned()),
                0.75,
            ));

        contract.validate().unwrap();

        let encoded = serde_json::to_string(&contract).unwrap();
        let decoded: QueryContract = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, contract);
        assert_eq!(decoded.required_constraints().count(), 1);
        assert_eq!(decoded.forbidden_constraints().count(), 1);
        assert_eq!(decoded.should_constraints().count(), 1);
    }

    #[test]
    fn validation_rejects_unknown_quantifier_reference() {
        let mut contract =
            QueryContract::new(ContractIntent::Verify).with_target(EntityPattern::label("claim"));
        contract.quantifiers.push(QuantifiedCondition {
            quantifier: Quantifier::All,
            constraint_ids: vec![ConstraintId::from("missing")],
        });

        assert!(matches!(
            contract.validate(),
            Err(QueryContractError::UnknownConstraintReference(id)) if id.0 == "missing"
        ));
    }

    #[test]
    fn validation_rejects_invalid_should_weight() {
        let contract =
            QueryContract::new(ContractIntent::Lookup).with_constraint(Constraint::should(
                "too_heavy",
                ConstraintTarget::Text,
                ConstraintOperator::Contains,
                ConstraintValue::Text("x".to_owned()),
                1.5,
            ));

        assert!(matches!(
            contract.validate(),
            Err(QueryContractError::InvalidShouldWeight { id, .. }) if id.0 == "too_heavy"
        ));
    }
}
