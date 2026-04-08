//! ANN Backend for Query Router integration
//!
//! Provides vector-based search as fallback when traditional
//! inverted/graph lookups don't return sufficient results.

use crate::store::NodeNum;
use super::index::EmbeddingIndex;

/// ANN Backend for semantic search.
///
/// Used as final fallback in the query router when CAS exact,
/// inverted index, and graph search don't return sufficient results.
pub struct AnnBackend {
    index: EmbeddingIndex,
    capacity: usize,
}

impl AnnBackend {
    /// Create a new ANN backend with given capacity.
    pub fn new(capacity: usize) -> Self {
        AnnBackend {
            index: EmbeddingIndex::new(capacity),
            capacity,
        }
    }

    /// Add an embedding vector for a node.
    pub fn add_embedding(&mut self, node_num: NodeNum, vec: &[f32]) {
        self.index.add_embedding(node_num, vec);
    }

    /// Search for k nearest neighbors to query vector.
    /// Returns Vec<(node_id, cosine_similarity)> sorted by similarity descending.
    pub fn search(&self, query: &[f32], k: u32) -> Vec<(NodeNum, f32)> {
        if self.index.is_empty() {
            return Vec::new();
        }

        // Brute-force search through all embeddings
        let mut results: Vec<(f32, NodeNum)> = Vec::new();
        for (node_num, embedding) in self.index.iter() {
            let sim = super::embedding::cosine_similarity(query, embedding);
            results.push((sim, node_num));
        }

        // Sort by similarity descending
        results.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        // Return top-k
        results.truncate(k as usize);
        results.into_iter().map(|(sim, id)| (id, sim)).collect()
    }

    /// Save embeddings to disk.
    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        let dir = path.to_path_buf();
        std::fs::create_dir_all(&dir)?;
        let file_path = dir.join("embeddings.bin");
        self.index.save(&file_path)
    }

    /// Load embeddings from disk.
    pub fn load(path: &std::path::Path) -> std::io::Result<Self> {
        let file_path = path.join("embeddings.bin");
        let index = EmbeddingIndex::load(&file_path)?;
        let cap = index.len().next_power_of_two();
        Ok(AnnBackend {
            index,
            capacity: cap,
        })
    }

    /// Get the number of stored embeddings.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Check if the backend is empty.
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Get the embedding dimension.
    pub fn dimension(&self) -> Option<usize> {
        self.index.dimension()
    }
}
