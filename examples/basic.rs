//! MemoryX SKF-1.1 Basic Usage Example
//!
//! This example demonstrates:
//! - Store creation with configuration
//! - Atom ingestion with claims and evidence
//! - Query and answer retrieval
//! - Context branching for hypothesis exploration

use memoryx::cas::{
    claims::{ClaimRecord, ClaimsSection},
    evidence::EvidenceSection,
    invariants::InvariantsSection,
    meta::{MetaField, MetaFieldKind, MetaSection, MetaValue},
    symbols::SymbolsSection,
};
use memoryx::prelude::*;
use memoryx::store::api::{AnswerPack, CtxId, MemoryX, StoreConfig, StoreError};
use memoryx::vm::ClaimData;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== MemoryX SKF-1.1 Basic Example ===\n");

    // =========================================================================
    // 1. Store Creation
    // =========================================================================

    println!("1. Creating MemoryX store...");

    let config = StoreConfig::new(PathBuf::from("./memoryx_example_data"))
        .with_mmap_mode(true)
        .with_io_uring(false) // io_uring only on Linux
        .with_io_buffer_size(64 * 1024)
        .with_fetch_budget(128 * 1024)
        .with_coalesce_gap(4096);

    let mut store = MemoryX::new(config)?;
    println!("   Store created successfully!\n");

    // =========================================================================
    // 2. Atom Ingestion
    // =========================================================================

    println!("2. Ingesting atoms...");

    // Create claims (subject-predicate-object triples)
    let claims1 = vec![
        ClaimData {
            subj: 1, // Rust (SymId)
            pred: 2, // is_a (SymId)
            obj_tag: 3,
            obj_val: 4, // programming_language (SymId)
            qualifiers_mask: 0,
        },
        ClaimData {
            subj: 1, // Rust
            pred: 5, // has_property (SymId)
            obj_tag: 3,
            obj_val: 6, // fast (SymId)
            qualifiers_mask: 0,
        },
    ];

    let payload1 = build_atom_payload(
        AtomType::FACT,
        &[
            "rust",
            "systems",
            "programming",
            "language",
            "fast",
            "thread",
            "safety",
        ],
        &claims1,
        5000,
        0xFFFF,
    );

    // Ingest first atom
    let atom_id1 = store.ingest(&payload1, AtomType::FACT, &claims1, &[])?;
    println!("   Ingested atom 1: {}...", hex::encode(&atom_id1[..8]));

    // Second atom: Definition
    let claims2 = vec![ClaimData {
        subj: 7, // systems_programming_language
        pred: 2, // is_a
        obj_tag: 3,
        obj_val: 8, // programming_language
        qualifiers_mask: 0,
    }];

    let payload2 = build_atom_payload(
        AtomType::DEFINITION,
        &[
            "systems",
            "programming",
            "language",
            "system",
            "software",
            "drivers",
            "embedded",
        ],
        &claims2,
        5000,
        0xFFFF,
    );

    let atom_id2 = store.ingest(&payload2, AtomType::DEFINITION, &claims2, &[])?;
    println!("   Ingested atom 2: {}...", hex::encode(&atom_id2[..8]));

    // Third atom: Fact about memory safety
    let claims3 = vec![ClaimData {
        subj: 1, // Rust
        pred: 9, // guarantees
        obj_tag: 3,
        obj_val: 10, // memory_safety
        qualifiers_mask: 0,
    }];

    let payload3 = build_atom_payload(
        AtomType::FACT,
        &[
            "rust",
            "memory",
            "safety",
            "ownership",
            "borrowing",
            "garbage",
            "collection",
        ],
        &claims3,
        5000,
        0xFFFF,
    );

    let atom_id3 = store.ingest(&payload3, AtomType::FACT, &claims3, &[])?;
    println!("   Ingested atom 3: {}...", hex::encode(&atom_id3[..8]));

    println!("   Total atoms ingested: 3\n");

    // =========================================================================
    // 3. Atom Retrieval
    // =========================================================================

    println!("3. Retrieving atom by ID...");

    let retrieved = store.get_atom(&atom_id1)?;
    println!("   Retrieved atom type: {:?}", retrieved.atom_type);
    println!("   Trust level: {}\n", retrieved.trust_level);

    // =========================================================================
    // 4. Query and Answer
    // =========================================================================

    println!("4. Running queries...");

    // Query 1: Lookup
    let answer1 = store.answer("What is Rust?", 0)?;
    print_answer_summary(&answer1, "Query: What is Rust?");

    // Query 2: Definition
    let answer2 = store.answer("Define systems programming language", 0)?;
    print_answer_summary(&answer2, "Query: Define systems programming language");

    // Query 3: Explanation
    let answer3 = store.answer("Why is Rust memory safe?", 0)?;
    print_answer_summary(&answer3, "Query: Why is Rust memory safe?");

    println!();

    // =========================================================================
    // 5. Context Branching
    // =========================================================================

    println!("5. Context branching demonstration...");

    // Create initial context
    let ctx_main = store.create_context(0)?;
    println!("   Created main context: {}", ctx_main);

    // Create a hypothesis branch using a concrete claim and source atom.
    let branch_claim = ClaimData {
        subj: 1, // Rust
        pred: 2, // is_a
        obj_tag: 3,
        obj_val: 10, // memory_safety
        qualifiers_mask: 0,
    };

    let ctx_hypothesis = create_hypothesis_branch(&mut store, ctx_main, atom_id1, &branch_claim)?;
    println!("   Created hypothesis branch: {:?}", ctx_hypothesis);

    // List conflicts (would be populated in real scenario)
    let conflicts = store.list_conflicts(ctx_main);
    println!("   Conflicts in main context: {}", conflicts.len());

    println!();

    // =========================================================================
    // 6. Alternative Answers
    // =========================================================================

    println!("6. Demonstrating alternative answers...");

    let mut answer = store.answer("Compare Rust and C++", 0)?;

    // Create an alternate answer (simulating context branch result)
    let alternate = AnswerPack::new(1);
    answer = answer.add_alternate(alternate);

    println!("   Main answer confidence: {:.2}", answer.confidence);
    println!("   Number of alternates: {}", answer.alternates.len());
    println!("   Best answer confidence: {:.2}", answer.best().confidence);

    println!();
    println!("=== Example Complete ===");

    Ok(())
}

/// Print summary of an answer pack
fn print_answer_summary(answer: &AnswerPack, query: &str) {
    println!("   {}", query);
    println!("   ─────────────────────────────────────");
    println!("   Confidence: {:.2}%", answer.confidence * 100.0);
    println!("   Claims: {}", answer.claims.len());
    println!("   Evidence: {}", answer.evidence.len());
    println!("   Graph nodes: {}", answer.graph.node_count());
    println!("   Graph edges: {}", answer.graph.edge_count());

    if !answer.limitations.is_empty() {
        println!("   Limitations:");
        for lim in &answer.limitations {
            println!("     - [{:?}] {}", lim.severity, lim.code);
        }
    }

    if answer.has_critical_limitations() {
        println!("   ⚠️  WARNING: Critical limitations present!");
    }
    println!();
}

/// Create a hypothesis branch demonstrating context forking
fn create_hypothesis_branch(
    store: &mut MemoryX,
    parent_ctx: CtxId,
    source_atom_id: [u8; 32],
    claim: &ClaimData,
) -> Result<CtxId, StoreError> {
    // Set active context before asserting the concrete claim.
    store.set_active_context(parent_ctx)?;

    // Use the current public API: a claim plus its source atom ID.
    let new_ctx = store.assert_claim_with_atom_id(parent_ctx, claim, source_atom_id)?;

    if new_ctx != parent_ctx {
        println!(
            "   Context branched due to conflict: {} -> {}",
            parent_ctx, new_ctx
        );
    }

    Ok(new_ctx)
}

/// Build a canonical SKF-1.1 atom body for the current ingest path.
fn build_atom_payload(
    atom_type: AtomType,
    text_symbols: &[&str],
    claims: &[ClaimData],
    trust_level: u16,
    domain_mask: u64,
) -> Vec<u8> {
    let mut symbols_section = SymbolsSection::new();
    for sym in text_symbols {
        symbols_section.intern((*sym).to_string());
    }
    for claim in claims {
        symbols_section.intern(format!("sym_{}", claim.subj));
        symbols_section.intern(format!("sym_{}", claim.pred));
        symbols_section.intern(format!("sym_{}", claim.obj_val));
    }
    let symbols_bytes = symbols_section.to_bytes();

    let refs_bytes = Vec::new();

    let mut claims_section = ClaimsSection::new();
    for claim in claims {
        claims_section.add_claim(ClaimRecord::new_u64(
            claim.subj as u16,
            claim.pred as u16,
            claim.obj_val,
        ));
    }
    let claims_bytes = claims_section.to_bytes();

    let invariants_bytes = InvariantsSection::new().to_bytes();
    let edges_bytes = Vec::new();
    let evidence_bytes = EvidenceSection::new().to_bytes();

    let mut meta_section = MetaSection::new();
    meta_section.add_field(MetaField::new(
        MetaFieldKind::TRUST_SCORE,
        MetaValue::F32(trust_level as f32 / 10000.0),
    ));
    meta_section.add_field(MetaField::new(
        MetaFieldKind::DOMAIN_MASK,
        MetaValue::U32(domain_mask as u32),
    ));
    let meta_bytes = meta_section.to_bytes();

    let sections_data_start: usize = 48 + 7 * 32;

    let mut current_off = sections_data_start;
    let symbols_off = current_off;
    current_off += symbols_bytes.len();

    let refs_off = current_off;
    current_off += refs_bytes.len();

    let claims_off = current_off;
    current_off += claims_bytes.len();

    let invariants_off = current_off;
    current_off += invariants_bytes.len();

    let edges_off = current_off;
    current_off += edges_bytes.len();

    let evidence_off = current_off;
    current_off += evidence_bytes.len();

    let meta_off = current_off;

    let mut body = Vec::new();
    body.extend_from_slice(&0x41544F4Du32.to_le_bytes());
    body.extend_from_slice(&0x0001u16.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&0u64.to_le_bytes());
    body.extend_from_slice(&0u64.to_le_bytes());
    body.extend_from_slice(&u64::MAX.to_le_bytes());
    body.extend_from_slice(&(atom_type as u32).to_le_bytes());
    body.extend_from_slice(&7u32.to_le_bytes());
    body.extend_from_slice(&48u64.to_le_bytes());

    let mut add_section_desc = |kind: u32, off: usize, data: &[u8]| {
        let crc = memoryx::utils::crc32(data);
        body.extend_from_slice(&kind.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&(off as u64).to_le_bytes());
        body.extend_from_slice(&(data.len() as u64).to_le_bytes());
        body.extend_from_slice(&crc.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
    };

    add_section_desc(0x01, symbols_off, &symbols_bytes);
    add_section_desc(0x02, refs_off, &refs_bytes);
    add_section_desc(0x03, claims_off, &claims_bytes);
    add_section_desc(0x04, invariants_off, &invariants_bytes);
    add_section_desc(0x05, edges_off, &edges_bytes);
    add_section_desc(0x06, evidence_off, &evidence_bytes);
    add_section_desc(0x07, meta_off, &meta_bytes);

    body.extend_from_slice(&symbols_bytes);
    body.extend_from_slice(&refs_bytes);
    body.extend_from_slice(&claims_bytes);
    body.extend_from_slice(&invariants_bytes);
    body.extend_from_slice(&edges_bytes);
    body.extend_from_slice(&evidence_bytes);
    body.extend_from_slice(&meta_bytes);

    body
}

// Helper module for hex encoding (simplified blake3 output display)
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        const CHARS: &[u8] = b"0123456789abcdef";
        let mut buf = String::with_capacity(bytes.len() * 2);
        for &byte in bytes.iter() {
            buf.push(CHARS[(byte >> 4) as usize] as char);
            buf.push(CHARS[(byte & 0x0F) as usize] as char);
        }
        buf
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_store_creation() {
        let dir = tempdir().unwrap();
        let config = StoreConfig::new(dir.path().join("test_store"));
        let store = MemoryX::new(config);
        assert!(store.is_ok());
    }

    #[test]
    fn test_atom_ingestion() {
        let dir = tempdir().unwrap();
        let config = StoreConfig::new(dir.path().join("test_ingest"));
        let mut store = MemoryX::new(config).unwrap();

        let claims = vec![ClaimData {
            subj: 1,
            pred: 2,
            obj_tag: 3,
            obj_val: 4,
            qualifiers_mask: 0,
        }];
        let payload = build_atom_payload(
            AtomType::FACT,
            &["test", "payload", "fact"],
            &claims,
            5000,
            0xFFFF,
        );

        let result = store.ingest(&payload, AtomType::FACT, &claims, &[]);
        assert!(result.is_ok());
        assert_ne!(result.unwrap(), [0u8; 32]);
    }

    #[test]
    fn test_query_answer() {
        let dir = tempdir().unwrap();
        let config = StoreConfig::new(dir.path().join("test_query"));
        let store = MemoryX::new(config).unwrap();

        let answer = store.answer("What is test?", 0);
        assert!(answer.is_ok());

        let answer = answer.unwrap();
        assert_eq!(answer.selected_ctx, 0);
        assert!(answer.confidence >= 0.0);
        assert!(answer.confidence <= 1.0);
    }

    #[test]
    fn test_context_branching() {
        let dir = tempdir().unwrap();
        let config = StoreConfig::new(dir.path().join("test_ctx"));
        let mut store = MemoryX::new(config).unwrap();

        let ctx0 = store.create_context(0).unwrap();
        assert_eq!(ctx0, 0);

        let ctx1 = store.create_context(1).unwrap();
        assert_eq!(ctx1, 1);

        assert!(store.set_active_context(ctx0).is_ok());
        assert_eq!(store.active_context(), ctx0);
    }

    #[test]
    fn test_answer_pack_limitations() {
        let mut pack = AnswerPack::new(0);

        // No limitations initially
        assert!(!pack.has_critical_limitations());

        // Add critical limitation
        pack.limitations.push(Limitation::critical(
            LimitationCode::BudgetExhausted,
            "Test".to_string(),
        ));

        assert!(pack.has_critical_limitations());
    }
}
