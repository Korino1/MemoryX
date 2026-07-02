use memoryx::store::ObjTag;
use memoryx::store::api::{MemoryX, StoreConfig};

#[test]
fn real_atom_has_queryable_provenance_path() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = MemoryX::new(StoreConfig::new(temp.path().join("memoryx"))).unwrap();
    let entity = store.create_entity("GPU", "hardware").unwrap();
    let result = store
        .add_entity_claim(entity.entity_id, 7, ObjTag::U64, 4090, 0, Vec::new())
        .unwrap();

    let chain = store.get_provenance(&result.atom_id).unwrap();

    assert_eq!(chain.root_atom_id, result.atom_id);
    assert!(chain.overall_trust > 0);
}
