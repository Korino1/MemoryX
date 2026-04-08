//! Approximate Nearest Neighbors (ANN) Backend for MemoryX SKF-1.1
//!
//! Provides vector-based semantic search using cosine similarity
//! over atom embeddings stored in `index/embeddings.bin`.
//!
//! # Architecture
//! - EmbeddingIndex: NodeNum → Vec<f32> mapping with persistence
//! - HnswGraph: Hierarchical Navigable Small World for fast ANN search
//! - AnnBackend: Integration with Query Router as final fallback

#![allow(dead_code)]

mod embedding;
mod index;
mod hnsw;
mod backend;

pub use embedding::cosine_similarity;
pub use index::EmbeddingIndex;
pub use hnsw::HnswGraph;
pub use backend::AnnBackend;

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0f32, 2.0, 3.0];
        let b = vec![1.0f32, 2.0, 3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-6, "Identical vectors should have similarity 1.0, got {}", sim);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![0.0f32, 1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-6, "Orthogonal vectors should have similarity 0.0, got {}", sim);
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0f32, 2.0, 3.0];
        let b = vec![-1.0f32, -2.0, -3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - (-1.0)).abs() < 1e-6, "Opposite vectors should have similarity -1.0, got {}", sim);
    }

    #[test]
    fn test_cosine_similarity_different_lengths() {
        let a = vec![1.0f32, 2.0, 3.0];
        let b = vec![1.0f32, 2.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 0.0).abs() < 1e-6, "Different length vectors should have similarity 0.0, got {}", sim);
    }

    #[test]
    fn test_cosine_similarity_empty() {
        let a: Vec<f32> = vec![];
        let b: Vec<f32> = vec![];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 0.0).abs() < 1e-6, "Empty vectors should have similarity 0.0, got {}", sim);
    }

    #[test]
    fn test_embedding_index_add_get() {
        let mut idx = EmbeddingIndex::new(128);
        assert!(idx.add_embedding(1, &[0.1, 0.2, 0.3]));
        assert!(idx.add_embedding(2, &[0.4, 0.5, 0.6]));

        let emb1 = idx.get_embedding(1).unwrap();
        assert_eq!(emb1, &[0.1, 0.2, 0.3]);

        assert!(idx.get_embedding(99).is_none());
    }

    #[test]
    fn test_embedding_index_dimension_consistency() {
        let mut idx = EmbeddingIndex::new(128);
        assert!(idx.add_embedding(1, &[0.1, 0.2, 0.3]));
        assert_eq!(idx.dimension(), Some(3));

        // Different dimension should fail
        assert!(!idx.add_embedding(2, &[0.1, 0.2]));
    }

    #[test]
    fn test_embedding_index_persistence() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("embeddings.bin");

        let mut idx = EmbeddingIndex::new(128);
        idx.add_embedding(10, &[0.1, 0.2, 0.3, 0.4]);
        idx.add_embedding(20, &[0.5, 0.6, 0.7, 0.8]);
        idx.save(&path).unwrap();

        let idx2 = EmbeddingIndex::load(&path).unwrap();
        assert_eq!(idx2.dimension(), Some(4));
        assert_eq!(idx2.len(), 2);

        let emb1 = idx2.get_embedding(10).unwrap();
        assert_eq!(emb1, &[0.1, 0.2, 0.3, 0.4]);

        let emb2 = idx2.get_embedding(20).unwrap();
        assert_eq!(emb2, &[0.5, 0.6, 0.7, 0.8]);
    }

    #[test]
    fn test_hnsw_graph_insert_search() {
        let mut hnsw = HnswGraph::new(16, 200);
        hnsw.insert(0, &[1.0, 0.0, 0.0]);
        hnsw.insert(1, &[0.0, 1.0, 0.0]);
        hnsw.insert(2, &[0.0, 0.0, 1.0]);
        hnsw.insert(3, &[0.9, 0.1, 0.0]); // Close to 0

        let results = hnsw.search(&[0.95, 0.05, 0.0], 2);
        assert!(!results.is_empty());
        // Node 0 or 3 should be closest
        let top_id = results[0].0;
        assert!(top_id == 0 || top_id == 3, "Expected node 0 or 3, got {}", top_id);
    }

    #[test]
    fn test_ann_backend() {
        let mut backend = AnnBackend::new(128);
        backend.add_embedding(10, &[1.0, 0.0, 0.0, 0.0]);
        backend.add_embedding(20, &[0.0, 1.0, 0.0, 0.0]);
        backend.add_embedding(30, &[0.0, 0.0, 1.0, 0.0]);

        let results = backend.search(&[0.9, 0.1, 0.0, 0.0], 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 10); // Closest to [0.9, 0.1, 0, 0] is node 10
    }
}
