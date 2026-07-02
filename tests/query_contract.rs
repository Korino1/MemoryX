use memoryx::query::contract::{
    Constraint, ConstraintOperator, ConstraintTarget, ConstraintValue, ContractIntent,
    EntityPattern, QueryContract,
};
use memoryx::store::api::{MemoryX, StoreConfig};

#[test]
fn query_contract_accepts_must_must_not_should() {
    let contract = QueryContract::new(ContractIntent::Lookup)
        .with_target(EntityPattern::label("term:1"))
        .with_constraint(Constraint::must(
            "requires_provenance",
            ConstraintTarget::Evidence,
            ConstraintOperator::Exists,
            ConstraintValue::Bool(true),
        ))
        .with_constraint(Constraint::must_not(
            "not_chunk_only",
            ConstraintTarget::Custom("answer_model".to_string()),
            ConstraintOperator::Eq,
            ConstraintValue::Text("chunk_only".to_string()),
        ))
        .with_constraint(Constraint::should(
            "prefer_project_scope",
            ConstraintTarget::Context,
            ConstraintOperator::Contains,
            ConstraintValue::Text("project".to_string()),
            0.5,
        ));

    contract.validate().unwrap();
    assert_eq!(contract.required_constraints().count(), 1);
    assert_eq!(contract.forbidden_constraints().count(), 1);
    assert_eq!(contract.should_constraints().count(), 1);
}

#[test]
fn unsupported_empty_contract_is_rejected_before_execution() {
    let contract = QueryContract::new(ContractIntent::Verify);
    let temp = tempfile::tempdir().unwrap();
    let store = MemoryX::new(StoreConfig::new(temp.path().join("memoryx"))).unwrap();

    let err = store.answer_contract(contract, 0).unwrap_err();
    assert!(err.to_string().contains("query contract is empty"));
}
