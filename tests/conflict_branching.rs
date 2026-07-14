use memoryx::store::api::{BranchReason, MemoryX, StoreConfig};

#[test]
fn branch_context_creates_visible_child_context() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = MemoryX::new(StoreConfig::new(temp.path().join("memoryx"))).unwrap();
    let root = store.create_context(0).expect("root must persist");
    let branch = store
        .branch_ctx(root, BranchReason::Conflict, 100)
        .expect("branch state must persist")
        .expect("branch must be created");

    let contexts = store.list_contexts();
    let child = contexts
        .iter()
        .find(|ctx| ctx.ctx_id == branch)
        .expect("child context must be listed");

    assert_eq!(child.parent_ctx, Some(root));
    assert!(matches!(child.branch_reason, BranchReason::Conflict));
}
