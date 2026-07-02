//! Embedding index: NodeNum → Vec<f32> mapping with persistence
//!
//! Stores embedding vectors for each atom, used by ANN search.
//! Binary format: [magic u32][dimension u32][count u32][entries...]
//! Each entry: [node_num u64][dimension u32][f32 * dimension]

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use crate::store::NodeNum;

/// Magic number for embeddings file: "EMB1" = 0x454D4231
const EMB_MAGIC: u32 = 0x454D4231;

/// Embedding storage index mapping NodeNum to embedding vector.
///
/// Enforces dimension consistency — all embeddings must have
/// the same dimension once set.
#[derive(Clone)]
pub struct EmbeddingIndex {
    embeddings: HashMap<NodeNum, Vec<f32>>,
    dimension: Option<usize>,
    capacity: usize,
}

impl EmbeddingIndex {
    /// Create a new embedding index with given capacity hint.
    pub fn new(capacity: usize) -> Self {
        EmbeddingIndex {
            embeddings: HashMap::with_capacity(capacity),
            dimension: None,
            capacity,
        }
    }

    /// Add an embedding vector for a node.
    ///
    /// Returns false if the vector dimension doesn't match existing embeddings.
    pub fn add_embedding(&mut self, node_num: NodeNum, vec: &[f32]) -> bool {
        if vec.is_empty() {
            return false;
        }

        // Check dimension consistency
        if let Some(dim) = self.dimension {
            if vec.len() != dim {
                return false;
            }
        } else {
            self.dimension = Some(vec.len());
        }

        self.embeddings.insert(node_num, vec.to_vec());
        true
    }

    /// Get embedding for a node.
    pub fn get_embedding(&self, node_num: NodeNum) -> Option<&[f32]> {
        self.embeddings.get(&node_num).map(|v| v.as_slice())
    }

    /// Get the embedding dimension (None if empty).
    pub fn dimension(&self) -> Option<usize> {
        self.dimension
    }

    /// Get the number of stored embeddings.
    pub fn len(&self) -> usize {
        self.embeddings.len()
    }

    /// Check if the index is empty.
    pub fn is_empty(&self) -> bool {
        self.embeddings.is_empty()
    }

    /// Iterate over all (node_num, embedding) pairs.
    pub fn iter(&self) -> impl Iterator<Item = (NodeNum, &[f32])> {
        self.embeddings.iter().map(|(&k, v)| (k, v.as_slice()))
    }

    /// Search for k nearest neighbors to query vector.
    ///
    /// Uses cosine similarity to find the most similar embeddings.
    /// Returns results sorted by similarity in descending order.
    ///
    /// # Arguments
    /// - `query`: Query embedding vector
    /// - `k`: Number of neighbors to return
    ///
    /// # Returns
    /// - `Vec<(NodeNum, f32)>`: Top-k neighbors with similarity scores
    ///   Sorted by similarity descending (highest similarity first)
    ///
    /// # Performance
    /// - O(N) brute-force search through all embeddings
    /// - For large datasets, consider using HnswGraph instead
    pub fn search(&self, query: &[f32], k: u32) -> Vec<(NodeNum, f32)> {
        if self.embeddings.is_empty() || k == 0 {
            return Vec::new();
        }

        // Compute similarity for all embeddings
        let mut results: Vec<(f32, NodeNum)> = self
            .embeddings
            .iter()
            .map(|(&node_num, embedding)| {
                let sim = super::cosine_similarity(query, embedding);
                (sim, node_num)
            })
            .collect();

        // Sort by similarity descending
        results.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        // Return top-k results
        results
            .into_iter()
            .take(k as usize)
            .map(|(sim, id)| (id, sim))
            .collect()
    }

    /// Save to binary file.
    ///
    /// Format:
    /// [magic u32][version u16][flags u16][dimension u32][count u32]
    /// [node_num u64][data_len u32][f32 * data_len] * count
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        let dimension = self.dimension.unwrap_or(0);
        let count = self.embeddings.len() as u32;

        // Write header
        file.write_all(&EMB_MAGIC.to_le_bytes())?;
        file.write_all(&1u16.to_le_bytes())?; // Version
        file.write_all(&0u16.to_le_bytes())?; // Flags
        file.write_all(&(dimension as u32).to_le_bytes())?;
        file.write_all(&count.to_le_bytes())?;

        // Write entries sorted by node_num
        let mut entries: Vec<_> = self.embeddings.iter().collect();
        entries.sort_by_key(|(k, _)| *k);

        for (&node_num, vec) in entries {
            file.write_all(&node_num.to_le_bytes())?;
            file.write_all(&(vec.len() as u32).to_le_bytes())?;
            for &val in vec {
                file.write_all(&val.to_le_bytes())?;
            }
        }

        file.flush()?;
        Ok(())
    }

    /// Load from binary file.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();

        // magic[4] + version[2] + flags[2] + dimension[4] + count[4] = 16 byte header
        let mut header_buf = [0u8; 16];
        file.read_exact(&mut header_buf)?;

        let magic =
            u32::from_le_bytes([header_buf[0], header_buf[1], header_buf[2], header_buf[3]]);
        if magic != EMB_MAGIC {
            return Err(std::io::Error::other("Invalid embeddings file magic"));
        }

        let _dimension =
            u32::from_le_bytes([header_buf[8], header_buf[9], header_buf[10], header_buf[11]])
                as usize;
        let count = u32::from_le_bytes([
            header_buf[12],
            header_buf[13],
            header_buf[14],
            header_buf[15],
        ]) as usize;

        let mut index = Self::new(count);

        // Read entries
        let data_size = file_len - 16;
        let mut data = vec![0u8; data_size as usize];
        file.read_exact(&mut data)?;

        let mut offset = 0usize;
        for _ in 0..count {
            if offset + 12 > data.len() {
                break; // node_num[8] + data_len[4]
            }
            let node_num = u64::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
                data[offset + 4],
                data[offset + 5],
                data[offset + 6],
                data[offset + 7],
            ]);
            offset += 8;

            let data_len = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as usize;
            offset += 4;

            if offset + data_len * 4 > data.len() {
                break;
            }

            let mut vec = Vec::with_capacity(data_len);
            for _ in 0..data_len {
                let val = f32::from_le_bytes([
                    data[offset],
                    data[offset + 1],
                    data[offset + 2],
                    data[offset + 3],
                ]);
                vec.push(val);
                offset += 4;
            }

            index.add_embedding(node_num, &vec);
        }

        Ok(index)
    }
}
