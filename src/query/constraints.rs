//! Deterministic constraint evaluation for query contracts.
//!
//! The evaluator works through a small `ConstraintSubject` trait so the query
//! router, solver candidates, MCP records, and tests can all use the same
//! policy without forcing a large refactor.

use std::collections::{HashMap, HashSet};

use super::contract::{
    Constraint, ConstraintId, ConstraintOperator, ConstraintResult, ConstraintStatus,
    ConstraintStrength, ConstraintTarget, ConstraintValue, QueryContract,
};

pub trait ConstraintSubject {
    fn value_for(&self, target: &ConstraintTarget) -> Option<ConstraintValue>;

    fn evidence_refs_for(&self, _constraint: &Constraint) -> Vec<String> {
        Vec::new()
    }

    fn candidate_ref(&self) -> Option<String> {
        None
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConstraintFacts {
    values: HashMap<ConstraintTarget, ConstraintValue>,
    evidence_refs: HashMap<ConstraintId, Vec<String>>,
    candidate_ref: Option<String>,
}

impl ConstraintFacts {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_value(mut self, target: ConstraintTarget, value: ConstraintValue) -> Self {
        self.values.insert(target, value);
        self
    }

    pub fn with_candidate_ref(mut self, candidate_ref: impl Into<String>) -> Self {
        self.candidate_ref = Some(candidate_ref.into());
        self
    }

    pub fn with_evidence_ref(
        mut self,
        constraint_id: impl Into<ConstraintId>,
        evidence_ref: impl Into<String>,
    ) -> Self {
        self.evidence_refs
            .entry(constraint_id.into())
            .or_default()
            .push(evidence_ref.into());
        self
    }
}

impl ConstraintSubject for ConstraintFacts {
    fn value_for(&self, target: &ConstraintTarget) -> Option<ConstraintValue> {
        self.values.get(target).cloned()
    }

    fn evidence_refs_for(&self, constraint: &Constraint) -> Vec<String> {
        self.evidence_refs
            .get(&constraint.id)
            .cloned()
            .unwrap_or_default()
    }

    fn candidate_ref(&self) -> Option<String> {
        self.candidate_ref.clone()
    }
}

pub struct ConstraintEvaluator;

impl ConstraintEvaluator {
    pub fn evaluate_contract<S: ConstraintSubject>(
        contract: &QueryContract,
        subject: &S,
    ) -> Vec<ConstraintResult> {
        contract
            .constraints
            .iter()
            .map(|constraint| Self::evaluate_constraint(constraint, subject))
            .collect()
    }

    pub fn evaluate_constraint<S: ConstraintSubject>(
        constraint: &Constraint,
        subject: &S,
    ) -> ConstraintResult {
        let Some(actual) = subject.value_for(&constraint.target) else {
            return attach_subject_refs(ConstraintResult::unknown(constraint.id.clone()), subject);
        };

        let matched = compare_values(&actual, &constraint.operator, &constraint.value);
        let satisfied = match constraint.strength {
            ConstraintStrength::MustNot => !matched,
            ConstraintStrength::Must | ConstraintStrength::Should { .. } => matched,
        };
        let status = if satisfied {
            ConstraintStatus::Satisfied
        } else {
            ConstraintStatus::Violated
        };

        let mut result = ConstraintResult {
            constraint_id: constraint.id.clone(),
            status,
            reason: (!satisfied).then(|| {
                format!(
                    "constraint {:?} expected {:?}, got {:?}",
                    constraint.operator, constraint.value, actual
                )
            }),
            candidate_ref: None,
            evidence_refs: subject.evidence_refs_for(constraint),
        };

        if let Some(candidate_ref) = subject.candidate_ref() {
            result.candidate_ref = Some(candidate_ref);
        }

        result
    }

    pub fn hard_constraints_satisfied(results: &[ConstraintResult]) -> bool {
        results.iter().all(|result| {
            !matches!(
                result.status,
                ConstraintStatus::Violated | ConstraintStatus::BlockedByPolicy
            )
        })
    }
}

fn attach_subject_refs<S: ConstraintSubject>(
    mut result: ConstraintResult,
    subject: &S,
) -> ConstraintResult {
    if let Some(candidate_ref) = subject.candidate_ref() {
        result.candidate_ref = Some(candidate_ref);
    }
    result
}

fn compare_values(
    actual: &ConstraintValue,
    operator: &ConstraintOperator,
    expected: &ConstraintValue,
) -> bool {
    match operator {
        ConstraintOperator::Exists => !matches!(actual, ConstraintValue::None),
        ConstraintOperator::Eq => values_equal(actual, expected),
        ConstraintOperator::Ne => !values_equal(actual, expected),
        ConstraintOperator::Contains => contains_value(actual, expected),
        ConstraintOperator::Matches => contains_value(actual, expected),
        ConstraintOperator::Gte => numeric_pair(actual, expected).is_some_and(|(a, b)| a >= b),
        ConstraintOperator::Lte => numeric_pair(actual, expected).is_some_and(|(a, b)| a <= b),
        ConstraintOperator::Before => temporal_before(actual, expected),
        ConstraintOperator::After => temporal_after(actual, expected),
        ConstraintOperator::During | ConstraintOperator::Within => {
            temporal_within(actual, expected)
        }
    }
}

fn values_equal(left: &ConstraintValue, right: &ConstraintValue) -> bool {
    match (left, right) {
        (ConstraintValue::Text(a), ConstraintValue::Text(b)) => a.eq_ignore_ascii_case(b),
        _ => left == right,
    }
}

fn contains_value(actual: &ConstraintValue, expected: &ConstraintValue) -> bool {
    match (actual, expected) {
        (ConstraintValue::Text(actual), ConstraintValue::Text(expected)) => {
            actual.to_lowercase().contains(&expected.to_lowercase())
        }
        (ConstraintValue::List(items), ConstraintValue::List(expected_items)) => {
            let actual_text = text_set(items);
            expected_items
                .iter()
                .filter_map(value_text)
                .all(|item| actual_text.contains(&item.to_lowercase()))
        }
        (ConstraintValue::List(items), expected) => {
            items.iter().any(|item| values_equal(item, expected))
        }
        _ => values_equal(actual, expected),
    }
}

fn numeric_pair(actual: &ConstraintValue, expected: &ConstraintValue) -> Option<(f64, f64)> {
    match (actual, expected) {
        (ConstraintValue::Number(a), ConstraintValue::Number(b)) => Some((*a, *b)),
        _ => None,
    }
}

fn temporal_before(actual: &ConstraintValue, expected: &ConstraintValue) -> bool {
    numeric_pair(actual, expected).is_some_and(|(timestamp, boundary)| timestamp < boundary)
}

fn temporal_after(actual: &ConstraintValue, expected: &ConstraintValue) -> bool {
    numeric_pair(actual, expected).is_some_and(|(timestamp, boundary)| timestamp >= boundary)
}

fn temporal_within(actual: &ConstraintValue, expected: &ConstraintValue) -> bool {
    match (actual, expected) {
        (ConstraintValue::Number(timestamp), ConstraintValue::TimeRange(range)) => {
            let from = range.from_unix_ns.unwrap_or(0) as f64;
            let to = range.to_unix_ns.unwrap_or(u64::MAX) as f64;
            *timestamp >= from && *timestamp < to
        }
        _ => false,
    }
}

fn text_set(values: &[ConstraintValue]) -> HashSet<String> {
    values
        .iter()
        .filter_map(value_text)
        .map(|value| value.to_lowercase())
        .collect()
}

fn value_text(value: &ConstraintValue) -> Option<&str> {
    match value {
        ConstraintValue::Text(text) => Some(text),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::contract::{ContractIntent, QueryContract};

    #[test]
    fn evaluates_positive_and_negative_constraints() {
        let contract = QueryContract::new(ContractIntent::Lookup)
            .with_constraint(Constraint::must(
                "supports_mcp",
                ConstraintTarget::Custom("supports".to_owned()),
                ConstraintOperator::Contains,
                ConstraintValue::Text("mcp".to_owned()),
            ))
            .with_constraint(Constraint::must_not(
                "requires_postgresql",
                ConstraintTarget::Custom("requires".to_owned()),
                ConstraintOperator::Eq,
                ConstraintValue::Text("postgresql".to_owned()),
            ));

        let facts = ConstraintFacts::new()
            .with_candidate_ref("candidate:memoryx")
            .with_value(
                ConstraintTarget::Custom("supports".to_owned()),
                ConstraintValue::List(vec![
                    ConstraintValue::Text("mcp".to_owned()),
                    ConstraintValue::Text("conflicts".to_owned()),
                ]),
            )
            .with_value(
                ConstraintTarget::Custom("requires".to_owned()),
                ConstraintValue::Text("sqlite".to_owned()),
            );

        let results = ConstraintEvaluator::evaluate_contract(&contract, &facts);

        assert_eq!(results[0].status, ConstraintStatus::Satisfied);
        assert_eq!(results[1].status, ConstraintStatus::Satisfied);
        assert_eq!(
            results[0].candidate_ref.as_deref(),
            Some("candidate:memoryx")
        );
    }

    #[test]
    fn missing_subject_value_returns_unknown() {
        let constraint = Constraint::must(
            "supports_mcp",
            ConstraintTarget::Custom("supports".to_owned()),
            ConstraintOperator::Contains,
            ConstraintValue::Text("mcp".to_owned()),
        );
        let facts = ConstraintFacts::new();

        let result = ConstraintEvaluator::evaluate_constraint(&constraint, &facts);

        assert_eq!(result.status, ConstraintStatus::Unknown);
    }

    #[test]
    fn numeric_comparison_is_supported() {
        let constraint = Constraint::must(
            "trust_min",
            ConstraintTarget::NumericMetric,
            ConstraintOperator::Gte,
            ConstraintValue::Number(0.7),
        );
        let facts = ConstraintFacts::new().with_value(
            ConstraintTarget::NumericMetric,
            ConstraintValue::Number(0.8),
        );

        let result = ConstraintEvaluator::evaluate_constraint(&constraint, &facts);

        assert_eq!(result.status, ConstraintStatus::Satisfied);
    }

    #[test]
    fn temporal_operators_are_deterministic() {
        let facts = ConstraintFacts::new().with_value(
            ConstraintTarget::Time,
            ConstraintValue::Number(1_700_000_000.0),
        );

        let before = Constraint::must(
            "before_cutoff",
            ConstraintTarget::Time,
            ConstraintOperator::Before,
            ConstraintValue::Number(1_800_000_000.0),
        );
        let after = Constraint::must(
            "after_cutoff",
            ConstraintTarget::Time,
            ConstraintOperator::After,
            ConstraintValue::Number(1_600_000_000.0),
        );
        let during = Constraint::must(
            "during_window",
            ConstraintTarget::Time,
            ConstraintOperator::During,
            ConstraintValue::TimeRange(crate::query::contract::TimeRangeSpec {
                from_unix_ns: Some(1_600_000_000),
                to_unix_ns: Some(1_800_000_000),
            }),
        );

        assert_eq!(
            ConstraintEvaluator::evaluate_constraint(&before, &facts).status,
            ConstraintStatus::Satisfied
        );
        assert_eq!(
            ConstraintEvaluator::evaluate_constraint(&after, &facts).status,
            ConstraintStatus::Satisfied
        );
        assert_eq!(
            ConstraintEvaluator::evaluate_constraint(&during, &facts).status,
            ConstraintStatus::Satisfied
        );
    }
}
