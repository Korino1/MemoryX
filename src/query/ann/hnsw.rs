//! HNSW (Hierarchical Navigable Small World) graph for ANN search
//!
//! Provides fast approximate nearest neighbor search using multi-layer graph navigation.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use super::embedding::cosine_similarity;
use crate::store::NodeNum;

/// Neighbor entry in the HNSW graph
#[derive(Clone, Debug)]
pub struct Neighbor {
    pub node_id: NodeNum,
    pub similarity: f32,
}

impl PartialEq for Neighbor {
    fn eq(&self, other: &Self) -> bool {
        self.node_id == other.node_id
    }
}

impl Eq for Neighbor {}

impl PartialOrd for Neighbor {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Neighbor {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse order for min-heap behavior (we want lowest similarity at top)
        other
            .similarity
            .partial_cmp(&self.similarity)
            .unwrap_or(Ordering::Equal)
    }
}

/// Connection layer in the HNSW graph
#[derive(Clone, Debug)]
pub struct Layer {
    pub connections: Vec<NodeNum>,
}

/// Node in the HNSW graph
#[derive(Clone, Debug)]
pub struct HnswNode {
    pub node_id: NodeNum,
    pub embedding: Vec<f32>,
    pub layers: Vec<Layer>,
    pub level: usize,
}

/// Hierarchical Navigable Small World graph for ANN search.
///
/// Parameters:
/// - M: max connections per node (default 16)
/// - ef_construction: search size during construction (default 200)
/// - num_layers: number of layers in hierarchy (calculated)
pub struct HnswGraph {
    nodes: Vec<HnswNode>,
    node_map: Vec<Option<usize>>, // node_id -> index in nodes
    m: usize,
    ef_construction: usize,
    num_layers: usize,
}

impl HnswGraph {
    /// Create a new HNSW graph with given parameters.
    pub fn new(m: usize, ef_construction: usize) -> Self {
        HnswGraph {
            nodes: Vec::new(),
            node_map: Vec::with_capacity(1024),
            m,
            ef_construction,
            num_layers: 0,
        }
    }

    /// Calculate maximum layer for a new node using a deterministic geometric distribution.
    fn random_level(node_id: NodeNum) -> usize {
        const MAX_HNSW_LEVEL: usize = 15;

        let mut level = 0;
        let mut sample = node_hash(node_id);
        while level < MAX_HNSW_LEVEL && (sample & 1) == 0 {
            level += 1;
            sample >>= 1;
        }
        level
    }

    /// Insert a node with its embedding into the graph.
    pub fn insert(&mut self, node_id: NodeNum, embedding: &[f32]) {
        let level = Self::random_level(node_id);
        let emb = embedding.to_vec();

        // Ensure node_map is large enough
        let idx = node_id as usize;
        while self.node_map.len() <= idx {
            self.node_map.push(None);
        }

        let layers: Vec<Layer> = (0..=level)
            .map(|_| Layer {
                connections: Vec::new(),
            })
            .collect();

        let node = HnswNode {
            node_id,
            embedding: emb,
            layers,
            level,
        };

        self.node_map[idx] = Some(self.nodes.len());
        self.nodes.push(node);

        if level + 1 > self.num_layers {
            self.num_layers = level + 1;
        }

        // Connect to nearest nodes in each layer
        self.connect_neighbors(node_id, level);
    }

    /// Connect a new node to its nearest neighbors.
    fn connect_neighbors(&mut self, node_id: NodeNum, level: usize) {
        let current_idx = match self.node_map.get(node_id as usize) {
            Some(Some(idx)) => *idx,
            _ => return,
        };

        // Collect current node info
        let (current_level, current_embedding) = {
            let node = &self.nodes[current_idx];
            (node.level, node.embedding.clone())
        };
        let top_layer = std::cmp::min(level, current_level);

        // Collect neighbor info first to avoid borrowing issues
        let mut neighbor_infos: Vec<(usize, Vec<f32>, NodeNum, usize)> = Vec::new();
        for (i, node) in self.nodes.iter().enumerate() {
            if i == current_idx {
                continue;
            }
            if node.level >= top_layer {
                neighbor_infos.push((i, node.embedding.clone(), node.node_id, node.level));
            }
        }

        for layer_idx in 0..=top_layer {
            // Find nearest neighbors in this layer
            let mut candidates: Vec<(f32, usize)> = Vec::new();
            for (i, emb, _, _) in &neighbor_infos {
                if *i != current_idx && layer_idx <= self.nodes[*i].level {
                    let sim = cosine_similarity(emb, &current_embedding);
                    candidates.push((sim, *i));
                }
            }

            // Sort by similarity descending
            candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

            // Collect neighbor connections to apply
            let neighbors_to_connect = std::cmp::min(self.m, candidates.len());
            let mut updates: Vec<((usize, usize), NodeNum)> = Vec::new();

            for &(sim, neighbor_idx) in &candidates[..neighbors_to_connect] {
                let _sim = sim;
                let neighbor_id = self.nodes[neighbor_idx].node_id;
                updates.push(((current_idx, layer_idx), neighbor_id));
            }

            // Apply current node connections
            if let Some(current) = self.nodes.get_mut(current_idx) {
                for &((_, l), neighbor_id) in &updates {
                    if l < current.layers.len() {
                        if !current.layers[l].connections.contains(&neighbor_id) {
                            current.layers[l].connections.push(neighbor_id);
                        }
                        if current.layers[l].connections.len() > self.m {
                            current.layers[l].connections.sort();
                            current.layers[l].connections.dedup();
                            current.layers[l].connections.truncate(self.m);
                        }
                    }
                }
            }

            // Apply reciprocal connections
            for &((_, _), neighbor_id) in &updates {
                if let Some(Some(neighbor_idx)) = self.node_map.get(neighbor_id as usize)
                    && layer_idx <= self.nodes[*neighbor_idx].level
                    && let Some(neighbor) = self.nodes.get_mut(*neighbor_idx)
                {
                    if !neighbor.layers[layer_idx].connections.contains(&node_id) {
                        neighbor.layers[layer_idx].connections.push(node_id);
                    }
                    if neighbor.layers[layer_idx].connections.len() > self.m {
                        neighbor.layers[layer_idx].connections.sort();
                        neighbor.layers[layer_idx].connections.dedup();
                        neighbor.layers[layer_idx].connections.truncate(self.m);
                    }
                }
            }
        }
    }

    /// Search for k nearest neighbors to query vector.
    /// Returns Vec<(node_id, similarity)> sorted by similarity descending.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(NodeNum, f32)> {
        if self.nodes.is_empty() {
            return Vec::new();
        }

        // Start from top layer of first node
        let mut current_idx = 0;
        let top_layer = self.num_layers.saturating_sub(1);

        // Greedy descent through layers
        for layer in (0..=top_layer).rev() {
            let mut found_improvement = true;
            while found_improvement {
                found_improvement = false;
                if let Some(current_node) = self.nodes.get(current_idx) {
                    if layer >= current_node.layers.len() {
                        continue;
                    }

                    let current_sim = cosine_similarity(query, &current_node.embedding);

                    for &neighbor_id in &current_node.layers[layer].connections {
                        if let Some(Some(neighbor_idx)) = self.node_map.get(neighbor_id as usize)
                            && let Some(neighbor) = self.nodes.get(*neighbor_idx)
                        {
                            let neighbor_sim = cosine_similarity(query, &neighbor.embedding);
                            if neighbor_sim > current_sim {
                                current_idx = *neighbor_idx;
                                found_improvement = true;
                                break;
                            }
                        }
                    }
                }
            }
        }

        // Final search in layer 0 with EF expansion
        let ef = std::cmp::max(self.ef_construction, k);
        let mut visited = std::collections::HashSet::with_capacity(ef);
        let mut candidates: BinaryHeap<Neighbor> = BinaryHeap::new();
        let mut result_list: Vec<(f32, NodeNum)> = Vec::new();

        // Start from current_idx
        visited.insert(current_idx);
        let current_sim = cosine_similarity(query, &self.nodes[current_idx].embedding);
        candidates.push(Neighbor {
            node_id: self.nodes[current_idx].node_id,
            similarity: current_sim,
        });

        while !candidates.is_empty() {
            let best = candidates.pop().unwrap();
            let best_idx = self
                .node_map
                .get(best.node_id as usize)
                .and_then(|x| *x)
                .unwrap_or(current_idx);

            if let Some(worst) = result_list.last()
                && best.similarity < worst.0
                && result_list.len() >= ef
            {
                break;
            }

            result_list.push((best.similarity, best.node_id));
            result_list.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            if result_list.len() > ef {
                result_list.truncate(ef);
            }

            // Explore neighbors
            if let Some(node) = self.nodes.get(best_idx)
                && !node.layers.is_empty()
            {
                for &neighbor_id in &node.layers[0].connections {
                    if !visited.contains(&(neighbor_id as usize)) {
                        visited.insert(neighbor_id as usize);
                        if let Some(Some(neighbor_idx)) = self.node_map.get(neighbor_id as usize)
                            && let Some(neighbor) = self.nodes.get(*neighbor_idx)
                        {
                            let sim = cosine_similarity(query, &neighbor.embedding);
                            candidates.push(Neighbor {
                                node_id: neighbor_id,
                                similarity: sim,
                            });
                        }
                    }
                }
            }
        }

        // Return top k
        result_list.truncate(std::cmp::min(k, result_list.len()));
        result_list.into_iter().map(|(sim, id)| (id, sim)).collect()
    }

    /// Get number of nodes in the graph.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Check if graph is empty.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// Simple deterministic hash based on input value.
fn node_hash(value: u64) -> u64 {
    // FNV-1a hash
    const FNV_OFFSET: u64 = 14695981039346656037;
    const FNV_PRIME: u64 = 1099511628211;

    let mut hash = FNV_OFFSET ^ value;
    hash = hash.wrapping_mul(FNV_PRIME);
    hash = hash.wrapping_add(123456789);
    hash = hash.wrapping_mul(FNV_PRIME);
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_random_level_varies_by_node_id() {
        let levels: HashSet<_> = (0..128u64)
            .map(|node_id| HnswGraph::random_level(node_id as NodeNum))
            .collect();
        assert!(levels.len() > 1);
        assert!(levels.iter().any(|&level| level > 0));
    }

    #[test]
    fn test_insert_builds_multiple_layers_for_diverse_node_ids() {
        let mut graph = HnswGraph::new(4, 16);
        for node_id in 0..128u64 {
            graph.insert(node_id as NodeNum, &[node_id as f32, 1.0, 0.5]);
        }

        assert_eq!(graph.len(), 128);
        assert!(graph.num_layers > 1);
        assert!(graph.nodes.iter().any(|node| node.level > 0));
    }
}
