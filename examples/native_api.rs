//! Native API для MemoryX с полным доступом ко всем функциям
//!
//! Этот модуль предоставляет прямой доступ к возможностям MemoryX без ограничений MCP:
//! - Batch ingest с chunking по `batch_size`
//! - Parallel batch query с bounded concurrency
//! - Прямой mmap доступ
//! - Graph walk с итераторами
//! - CRDT операции
//! - Инварианты и VM bytecode

use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

use memoryx::cas::{
    claims::{ClaimRecord, ClaimsSection},
    evidence::EvidenceSection,
    invariants::InvariantsSection,
    meta::{MetaField, MetaFieldKind, MetaSection, MetaValue},
    symbols::SymbolsSection,
};
use memoryx::prelude::*;
use memoryx::store::api::{AnswerPack, MemoryX, StoreConfig};

/// Native API для полного доступа к MemoryX
pub struct MemoryXNative {
    store: Arc<RwLock<MemoryX>>,
    config: NativeConfig,
}

/// Конфигурация Native API
#[derive(Debug, Clone)]
pub struct NativeConfig {
    /// Использовать mmap для чтения
    pub mmap_mode: bool,
    /// Максимальный размер чанка для batch ingest
    pub batch_size: usize,
    /// Максимальное число параллельных worker-ов для batch_query
    pub query_parallelism: usize,
}

impl Default for NativeConfig {
    fn default() -> Self {
        let query_parallelism = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);

        NativeConfig {
            mmap_mode: true,
            batch_size: 100,
            query_parallelism,
        }
    }
}

impl MemoryXNative {
    /// Создать новый Native API
    pub async fn new<P: AsRef<Path>>(
        data_dir: P,
        config: NativeConfig,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let store_config =
            StoreConfig::new(data_dir.as_ref().to_path_buf()).with_mmap_mode(config.mmap_mode);

        let store = MemoryX::new(store_config)?;

        Ok(MemoryXNative {
            store: Arc::new(RwLock::new(store)),
            config,
        })
    }

    // ========================================================================
    // Batch Operations (недоступно в MCP)
    // ========================================================================

    /// Batch ingest - загрузка сотен атомов за раз
    pub async fn batch_ingest(
        &self,
        atoms: Vec<BatchAtom>,
    ) -> Result<BatchIngestResult, Box<dyn std::error::Error + Send + Sync>> {
        let total = atoms.len();
        let mut results = Vec::with_capacity(total);
        let mut errors = Vec::new();
        let batch_size = self.config.batch_size.max(1);
        let mut iter = atoms.into_iter().enumerate();

        loop {
            let mut processed = 0usize;
            {
                let mut store = self.store.write().await;
                while processed < batch_size {
                    let Some((idx, atom)) = iter.next() else {
                        break;
                    };

                    match store.ingest(&atom.payload, atom.atom_type, &atom.claims, &atom.evidence)
                    {
                        Ok(atom_id) => {
                            results.push(atom_id);
                        }
                        Err(e) => {
                            errors.push(BatchError {
                                index: idx,
                                error: e.to_string(),
                            });
                        }
                    }
                    processed += 1;
                }
            }

            if processed == 0 {
                break;
            }
        }

        Ok(BatchIngestResult {
            atom_ids: results,
            errors,
            total,
        })
    }

    /// Batch query - множественные запросы с bounded parallel execution
    pub async fn batch_query(
        &self,
        queries: Vec<String>,
        ctx_id: Option<CtxId>,
    ) -> Result<Vec<AnswerPack>, Box<dyn std::error::Error + Send + Sync>> {
        let ctx = ctx_id.unwrap_or(0);
        let total = queries.len();

        if total == 0 {
            return Ok(Vec::new());
        }

        let parallelism = self.config.query_parallelism.max(1);
        if parallelism == 1 || total == 1 {
            let store = self.store.read().await;
            let mut results = Vec::with_capacity(total);
            for query in queries {
                results.push(store.answer(&query, ctx)?);
            }
            return Ok(results);
        }

        let mut results: Vec<Option<AnswerPack>> =
            std::iter::repeat_with(|| None).take(total).collect();
        let mut pending = queries.into_iter().enumerate();

        loop {
            let mut handles = Vec::with_capacity(parallelism);
            for _ in 0..parallelism {
                let Some((index, query)) = pending.next() else {
                    break;
                };

                let store = Arc::clone(&self.store);
                handles.push(tokio::task::spawn_blocking(
                    move || -> Result<(usize, AnswerPack), String> {
                        let store = store.blocking_read();
                        store
                            .answer(&query, ctx)
                            .map(|answer| (index, answer))
                            .map_err(|error| error.to_string())
                    },
                ));
            }

            if handles.is_empty() {
                break;
            }

            for handle in handles {
                let worker_result = handle.await.map_err(|error| {
                    std::io::Error::other(format!("batch_query worker panicked: {error}"))
                })?;
                let (index, answer) = worker_result.map_err(std::io::Error::other)?;
                results[index] = Some(answer);
            }
        }

        Ok(results
            .into_iter()
            .map(|result| result.expect("batch_query worker must fill every result slot"))
            .collect())
    }

    // ========================================================================
    // Graph Operations (полный доступ)
    // ========================================================================

    /// Graph walk (возвращает Vec)
    pub async fn graph_walk(
        &self,
        seed_nodes: Vec<NodeNum>,
        edge_types: Vec<EdgeType>,
        max_depth: u8,
    ) -> Result<Vec<(NodeNum, NodeNum, EdgeType)>, Box<dyn std::error::Error + Send + Sync>> {
        let store = self.store.read().await;
        Ok(store.graph_walk(&seed_nodes, &edge_types, max_depth, None))
    }

    /// subgraph extraction для конкретного query
    pub async fn extract_subgraph(
        &self,
        question: &str,
        ctx_id: Option<CtxId>,
    ) -> Result<Subgraph, Box<dyn std::error::Error + Send + Sync>> {
        let answer = self
            .store
            .read()
            .await
            .answer(question, ctx_id.unwrap_or(0))?;

        Ok(Subgraph {
            nodes: answer.graph.nodes.clone(),
            edges: answer.graph.edges.clone(),
        })
    }

    // ========================================================================
    // Context Operations
    // ========================================================================

    /// Создать новый контекст
    pub async fn create_context(&self, policy_id: CtxPolicyId) -> Result<CtxId, StoreError> {
        let mut store = self.store.write().await;
        store.create_context(policy_id)
    }

    /// Получить активный контекст
    pub async fn active_context(&self) -> CtxId {
        let store = self.store.read().await;
        store.active_context()
    }

    /// List conflicts в контексте
    pub async fn list_conflicts(&self, ctx_id: CtxId) -> Vec<Conflict> {
        let store = self.store.read().await;
        store.list_conflicts(ctx_id)
    }
}

// ============================================================================
// Batch Types
// ============================================================================

#[derive(Debug, Clone)]
pub struct BatchAtom {
    pub payload: Vec<u8>,
    pub atom_type: AtomType,
    pub claims: Vec<ClaimData>,
    pub evidence: Vec<EvidenceRef>,
}

#[derive(Debug)]
pub struct BatchIngestResult {
    pub atom_ids: Vec<AtomId>,
    pub errors: Vec<BatchError>,
    pub total: usize,
}

#[derive(Debug)]
pub struct BatchError {
    pub index: usize,
    pub error: String,
}

// ============================================================================
// Subgraph Types
// ============================================================================

#[derive(Debug, Clone)]
pub struct Subgraph {
    pub nodes: Vec<AgNode>,
    pub edges: Vec<AgEdge>,
}

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
        claims_section.add_claim(
            ClaimRecord::from_scalar(
                claim.subj,
                claim.pred as u32,
                ObjTag::from_u8(claim.obj_tag).unwrap_or(ObjTag::U64),
                claim.obj_val,
            )
            .expect("example claim must be scalar"),
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_batch_ingest() {
        let dir = tempdir().unwrap();
        let api = MemoryXNative::new(dir.path(), NativeConfig::default())
            .await
            .unwrap();

        let atoms = vec![
            BatchAtom {
                payload: build_atom_payload(
                    AtomType::FACT,
                    &["test", "atom", "one"],
                    &[],
                    5000,
                    0xFFFF,
                ),
                atom_type: AtomType::FACT,
                claims: vec![],
                evidence: vec![],
            },
            BatchAtom {
                payload: build_atom_payload(
                    AtomType::DEFINITION,
                    &["test", "atom", "two"],
                    &[],
                    5000,
                    0xFFFF,
                ),
                atom_type: AtomType::DEFINITION,
                claims: vec![],
                evidence: vec![],
            },
        ];

        let result = api.batch_ingest(atoms).await.unwrap();

        assert_eq!(result.total, 2);
        assert_eq!(result.atom_ids.len(), 2);
        assert!(result.errors.is_empty());
    }

    #[tokio::test]
    async fn test_batch_query_parallel() {
        let dir = tempdir().unwrap();
        let config = NativeConfig {
            mmap_mode: false,
            batch_size: 1,
            query_parallelism: 2,
        };
        let api = MemoryXNative::new(dir.path(), config).await.unwrap();

        let atoms = vec![
            BatchAtom {
                payload: build_atom_payload(
                    AtomType::FACT,
                    &["rust", "systems", "programming"],
                    &[],
                    5000,
                    0xFFFF,
                ),
                atom_type: AtomType::FACT,
                claims: vec![],
                evidence: vec![],
            },
            BatchAtom {
                payload: build_atom_payload(
                    AtomType::DEFINITION,
                    &["memory", "safety", "ownership"],
                    &[],
                    5000,
                    0xFFFF,
                ),
                atom_type: AtomType::DEFINITION,
                claims: vec![],
                evidence: vec![],
            },
        ];
        api.batch_ingest(atoms).await.unwrap();

        let results = api
            .batch_query(vec!["rust".to_string(), "ownership".to_string()], None)
            .await
            .unwrap();

        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|answer| !answer.graph.is_empty()));
    }

    #[tokio::test]
    async fn test_native_config() {
        let config = NativeConfig {
            mmap_mode: false,
            batch_size: 50,
            query_parallelism: 8,
        };

        assert!(!config.mmap_mode);
        assert_eq!(config.batch_size, 50);
        assert_eq!(config.query_parallelism, 8);
    }
}

// ============================================================================
// Example usage
// ============================================================================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use std::env;

    let data_dir = env::args()
        .nth(1)
        .unwrap_or_else(|| "./memoryx_data".to_string());

    println!("MemoryX Native API Example");
    println!("==========================");
    println!("Data directory: {}", data_dir);
    println!();

    // Initialize Native API
    let config = NativeConfig::default();
    let api = MemoryXNative::new(&data_dir, config).await?;

    println!("✓ MemoryX store initialized");
    println!();

    // Example: Batch ingest
    println!("Example 1: Batch Ingest");
    println!("-----------------------");

    let atoms = vec![
        BatchAtom {
            payload: build_atom_payload(
                AtomType::DEFINITION,
                &["rust", "systems", "programming", "language"],
                &[],
                5000,
                0xFFFF,
            ),
            atom_type: AtomType::DEFINITION,
            claims: vec![],
            evidence: vec![],
        },
        BatchAtom {
            payload: build_atom_payload(
                AtomType::FACT,
                &["rust", "memory", "safety", "garbage", "collection"],
                &[],
                5000,
                0xFFFF,
            ),
            atom_type: AtomType::FACT,
            claims: vec![],
            evidence: vec![],
        },
        BatchAtom {
            payload: build_atom_payload(
                AtomType::FACT,
                &["rust", "ownership", "borrowing"],
                &[],
                5000,
                0xFFFF,
            ),
            atom_type: AtomType::FACT,
            claims: vec![],
            evidence: vec![],
        },
    ];

    let result = api.batch_ingest(atoms).await?;
    println!(
        "Ingested {} atoms, {} errors",
        result.atom_ids.len(),
        result.errors.len()
    );
    println!();

    // Example: Parallel batch query
    println!("Example 2: Parallel Batch Query");
    println!("--------------------------------");

    let answers = api
        .batch_query(vec!["rust".to_string(), "memory safety".to_string()], None)
        .await?;

    println!("Batch query returned {} answers", answers.len());
    for (idx, answer) in answers.iter().enumerate() {
        println!(
            "  [{}] ctx={} confidence={:.3} nodes={} claims={}",
            idx,
            answer.selected_ctx,
            answer.confidence,
            answer.graph.nodes.len(),
            answer.claims.len()
        );
    }
    println!();

    // Example: Graph walk
    println!("Example 3: Graph Walk");
    println!("---------------------");

    let edges = api
        .graph_walk(vec![0, 1], vec![EdgeType::DEFINES, EdgeType::SUPPORTS], 2)
        .await?;

    println!("Found {} edges", edges.len());
    for (src, dst, edge_type) in edges.iter().take(5) {
        println!("  {} -> {} ({:?})", src, dst, edge_type);
    }
    println!();

    // Example: Context management
    println!("Example 4: Context Management");
    println!("-----------------------------");

    let ctx_id = api.create_context(0).await?;
    println!("Created context: {}", ctx_id);

    let active = api.active_context().await;
    println!("Active context: {}", active);
    println!();

    println!("✓ All examples completed successfully!");

    Ok(())
}
