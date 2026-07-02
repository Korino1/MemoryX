//! Deterministic natural-language compiler for query contracts.
//!
//! This is the safe baseline: no LLM is required and no entity IDs are
//! fabricated from plain text. LLM-based parsing can be layered on top later,
//! but must produce the same public `QueryContract`.

use super::contract::{
    AmbiguityPolicy, CompletenessPolicy, ConflictPolicy, Constraint, ConstraintOperator,
    ConstraintTarget, ConstraintValue, ContractIntent, EntityPattern, EvidencePolicy, OutputFormat,
    QueryContract, QueryContractError, SourcePolicy,
};
use super::solver::GoalSpec;

pub struct QueryContractCompiler;

impl QueryContractCompiler {
    pub fn compile_contract(query: &str) -> QueryContract {
        let normalized = normalize_query(query);
        let mut contract = QueryContract::new(classify_intent(&normalized))
            .with_target(EntityPattern::label(query.trim()));

        apply_explicit_term_targets(&normalized, &mut contract);
        apply_broad_lookup_compat_target(&mut contract);
        apply_requirement_rules(&normalized, &mut contract);
        apply_negative_rules(&normalized, &mut contract);
        apply_priority_rules(&normalized, &mut contract);
        apply_policy_rules(&normalized, &mut contract);

        contract
    }

    pub fn compile_goal(query: &str) -> Result<GoalSpec, QueryContractError> {
        Self::compile_contract(query).to_goal_spec()
    }
}

fn normalize_query(query: &str) -> String {
    query
        .to_lowercase()
        .replace([';', ',', '.', ':'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn classify_intent(normalized: &str) -> ContractIntent {
    if contains_any(
        normalized,
        &["compare", "versus", " vs ", "сравни", "сравнить"],
    ) {
        return ContractIntent::Compare;
    }

    if contains_any(
        normalized,
        &["verify", "check", "validate", "confirm", "prove", "проверь"],
    ) {
        return ContractIntent::Verify;
    }

    if contains_any(
        normalized,
        &[
            "explain",
            "why",
            "how",
            "reason",
            "cause",
            "объясни",
            "почему",
        ],
    ) {
        return ContractIntent::Explain;
    }

    if contains_any(normalized, &["define", "definition", "meaning", "определи"]) {
        return ContractIntent::Define;
    }

    if contains_any(normalized, &["plan", "steps", "procedure", "план"]) {
        return ContractIntent::Plan;
    }

    if contains_any(normalized, &["derive", "infer", "therefore", "выведи"]) {
        return ContractIntent::Derive;
    }

    ContractIntent::Lookup
}

fn apply_requirement_rules(normalized: &str, contract: &mut QueryContract) {
    if contains_any(normalized, &["rust", "раст"]) {
        contract.constraints.push(Constraint::must(
            "implementation_language_rust",
            ConstraintTarget::Custom("implementation_language".to_owned()),
            ConstraintOperator::Eq,
            ConstraintValue::Text("rust".to_owned()),
        ));
    }

    if contains_any(
        normalized,
        &["local", "локаль", "без облака", "offline", "on device"],
    ) {
        contract.constraints.push(Constraint::must(
            "deployment_local",
            ConstraintTarget::Custom("deployment".to_owned()),
            ConstraintOperator::Eq,
            ConstraintValue::Text("local".to_owned()),
        ));
    }

    if contains_any(normalized, &["conflict", "conflicts", "противореч", "ветв"]) {
        contract.constraints.push(Constraint::must(
            "supports_conflicts",
            ConstraintTarget::Custom("supports".to_owned()),
            ConstraintOperator::Contains,
            ConstraintValue::Text("conflicts".to_owned()),
        ));
        contract.conflict_policy.include_conflicts = true;
    }

    if contains_any(normalized, &["mcp"]) {
        contract.constraints.push(Constraint::must(
            "supports_mcp",
            ConstraintTarget::Custom("supports".to_owned()),
            ConstraintOperator::Contains,
            ConstraintValue::Text("mcp".to_owned()),
        ));
    }
}

fn apply_explicit_term_targets(normalized: &str, contract: &mut QueryContract) {
    for token in normalized.split_whitespace() {
        if let Ok(term_id) = token.parse::<u32>() {
            contract.targets.push(EntityPattern {
                id: Some(format!("term:{term_id}")),
                label: Some(token.to_owned()),
                entity_type: Some("term_id".to_owned()),
                aliases: Vec::new(),
                domain_mask: None,
            });
        }
    }
}

fn apply_broad_lookup_compat_target(contract: &mut QueryContract) {
    let has_explicit_target = contract
        .targets
        .iter()
        .any(|target| target.id.as_deref().is_some_and(|id| id.contains(':')));

    if contract.intent == ContractIntent::Lookup && !has_explicit_target {
        contract.targets.push(EntityPattern {
            id: Some("term:0".to_owned()),
            label: Some("compat_broad_lookup".to_owned()),
            entity_type: Some("compatibility_term_seed".to_owned()),
            aliases: Vec::new(),
            domain_mask: None,
        });
    }
}

fn apply_negative_rules(normalized: &str, contract: &mut QueryContract) {
    let mentions_postgresql = contains_any(normalized, &["postgresql", "postgres"]);
    let negates_postgresql = contains_any(
        normalized,
        &[
            "not postgresql",
            "not postgres",
            "without postgresql",
            "without postgres",
            "не использующую postgresql",
            "не использующую postgres",
            "без postgresql",
            "без postgres",
        ],
    );

    if mentions_postgresql && negates_postgresql {
        contract.constraints.push(Constraint::must_not(
            "requires_postgresql",
            ConstraintTarget::Custom("requires".to_owned()),
            ConstraintOperator::Eq,
            ConstraintValue::Text("postgresql".to_owned()),
        ));
    }
}

fn apply_priority_rules(normalized: &str, contract: &mut QueryContract) {
    if contains_any(normalized, &["windows", "win32", "win64"]) {
        contract.constraints.push(Constraint::should(
            "supports_windows",
            ConstraintTarget::Custom("supports".to_owned()),
            ConstraintOperator::Contains,
            ConstraintValue::Text("windows".to_owned()),
            0.9,
        ));
    }

    if contains_any(
        normalized,
        &["provenance", "origin", "citation", "происхожд", "источник"],
    ) {
        contract.constraints.push(Constraint::should(
            "exposes_provenance",
            ConstraintTarget::Custom("exposes".to_owned()),
            ConstraintOperator::Contains,
            ConstraintValue::Text("provenance".to_owned()),
            1.0,
        ));
        contract.source_policy.require_provenance = true;
        contract.evidence_policy.require_direct_evidence = true;
        contract.output_contract.include_provenance = true;
    }
}

fn apply_policy_rules(normalized: &str, contract: &mut QueryContract) {
    if contains_any(normalized, &["json", "structured", "структур"]) {
        contract.output_contract.format = OutputFormat::StructuredJson;
    }

    if contains_any(normalized, &["graph", "граф"]) {
        contract.output_contract.include_answer_graph = true;
    }

    if contains_any(normalized, &["trace", "трасс"]) {
        contract.output_contract.include_execution_trace = true;
    }

    contract.source_policy = SourcePolicy {
        require_provenance: contract.source_policy.require_provenance,
        ..contract.source_policy.clone()
    };
    contract.evidence_policy = EvidencePolicy {
        include_rejected_candidates: true,
        ..contract.evidence_policy.clone()
    };
    contract.ambiguity_policy = AmbiguityPolicy {
        require_disambiguation_notes: true,
        ..contract.ambiguity_policy.clone()
    };
    contract.conflict_policy = ConflictPolicy {
        include_conflicts: contract.conflict_policy.include_conflicts,
        ..contract.conflict_policy.clone()
    };
    contract.completeness_policy = CompletenessPolicy {
        require_minimal_proof_subgraph: true,
        expose_unknowns: true,
        expose_unsatisfied_constraints: true,
    };
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::contract::ConstraintStrength;

    #[test]
    fn concept_example_compiles_to_contract_constraints() {
        let contract = QueryContractCompiler::compile_contract(
            "Найди локальную базу знаний на Rust, работающую без облака, \
             поддерживающую противоречия и MCP, но не использующую PostgreSQL; \
             приоритет — Windows и возможность проверить происхождение ответа.",
        );

        contract.validate().unwrap();

        let must_ids = contract
            .required_constraints()
            .map(|constraint| constraint.id.0.as_str())
            .collect::<Vec<_>>();
        let must_not_ids = contract
            .forbidden_constraints()
            .map(|constraint| constraint.id.0.as_str())
            .collect::<Vec<_>>();
        let should_ids = contract
            .should_constraints()
            .map(|constraint| constraint.id.0.as_str())
            .collect::<Vec<_>>();

        assert_eq!(contract.intent, ContractIntent::Lookup);
        assert!(must_ids.contains(&"implementation_language_rust"));
        assert!(must_ids.contains(&"deployment_local"));
        assert!(must_ids.contains(&"supports_conflicts"));
        assert!(must_ids.contains(&"supports_mcp"));
        assert_eq!(must_not_ids, vec!["requires_postgresql"]);
        assert!(should_ids.contains(&"supports_windows"));
        assert!(should_ids.contains(&"exposes_provenance"));
        assert!(contract.source_policy.require_provenance);
        assert!(contract.evidence_policy.require_direct_evidence);
        assert!(contract.conflict_policy.include_conflicts);
    }

    #[test]
    fn compiler_does_not_fabricate_internal_entity_ids() {
        let goal = QueryContractCompiler::compile_goal("Explain MemoryX MCP").unwrap();

        assert!(goal.entities.is_empty());
    }

    #[test]
    fn compiler_preserves_explicit_numeric_term_ids() {
        let goal = QueryContractCompiler::compile_goal("find 100").unwrap();

        assert_eq!(goal.entities, vec![crate::query::EntityRef::Term(100)]);
    }

    #[test]
    fn compiler_preserves_broad_lookup_compat_seed() {
        let goal = QueryContractCompiler::compile_goal("find what").unwrap();

        assert_eq!(goal.entities, vec![crate::query::EntityRef::Term(0)]);
    }

    #[test]
    fn soft_constraints_have_valid_weights() {
        let contract = QueryContractCompiler::compile_contract(
            "Find a local Rust knowledge base with Windows and provenance priority.",
        );

        for constraint in contract.should_constraints() {
            match constraint.strength {
                ConstraintStrength::Should { weight } => assert!((0.0..=1.0).contains(&weight)),
                _ => unreachable!("should_constraints returned non-should item"),
            }
        }
    }
}
