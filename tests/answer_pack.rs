use memoryx::query::contract::{ContractIntent, EntityPattern, QueryContract};
use memoryx::store::api::{MemoryX, StoreConfig};

#[test]
fn same_snapshot_and_contract_return_same_logical_empty_result() {
    let temp = tempfile::tempdir().unwrap();
    let store = MemoryX::new(StoreConfig::new(temp.path().join("memoryx"))).unwrap();
    let contract =
        QueryContract::new(ContractIntent::Lookup).with_target(EntityPattern::label("term:1"));

    let first = store.answer_contract(contract.clone(), 0).unwrap();
    let second = store.answer_contract(contract, 0).unwrap();

    assert_eq!(
        first.snapshot.cas_atom_count,
        second.snapshot.cas_atom_count
    );
    assert_eq!(
        first.snapshot.graph_node_count,
        second.snapshot.graph_node_count
    );
    assert_eq!(
        first.snapshot.graph_edge_count,
        second.snapshot.graph_edge_count
    );
    assert_eq!(
        first.snapshot.index_generation,
        second.snapshot.index_generation
    );
    assert_eq!(
        first.snapshot.solver_version,
        second.snapshot.solver_version
    );
    assert_eq!(first.status, second.status);
    assert_eq!(first.coverage_report, second.coverage_report);
}

#[test]
fn answer_pack_exposes_insufficient_or_empty_evidence_without_fabricating_claims() {
    let temp = tempfile::tempdir().unwrap();
    let store = MemoryX::new(StoreConfig::new(temp.path().join("memoryx"))).unwrap();
    let contract = QueryContract::new(ContractIntent::Explain)
        .with_target(EntityPattern::label("unknown unsupported fact"));

    let answer = store.answer_contract(contract, 0).unwrap();

    assert!(answer.claims.is_empty());
    assert!(answer.evidence.is_empty());
    assert_eq!(answer.snapshot.context_id, answer.selected_ctx);
}
