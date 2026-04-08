//! GraphStore with CSR (Compressed Sparse Row) format for MemoryX SKF-1.1.
//!
//! This module provides:
//! - CSR-based graph storage for efficient neighbor traversal
//! - Base graph + delta updates for incremental modifications
//! - Zero-copy mmap views for graph traversal
//! - Edge extraction and building utilities
//!
//! # File Formats
//!
//! ## graph.manifest
//! ```text
//! [GraphManifest]              - Graph metadata (96+ bytes)
//! [DeltaHeader * delta_count]  - Delta file references
//! ```
//!
//! ## edges_{edge_type}.offsets
//! ```text
//! [u64; node_count + 1]        - Row offsets into targets array
//! ```
//!
//! ## edges_{edge_type}.targets
//! ```text
//! [BitPackBlock * N]           - Delta-encoded, bit-packed destination nodes
//! ```
//!
//! ## edges_{edge_type}.attrs
//! ```text
//! [EdgeAttr; edge_count]       - 16 bytes per edge (confidence, flags, validity)
//! ```

// Re-export store module for public API
pub mod store;

// Re-export all public types from store
pub use store::{
    BitPackBlock, BitPackKind, CsrLayer, CsrView, DeltaHeader, DeltaLayer, EdgeAttr, EdgeListEntry,
    GraphBuilder, GraphManifest, GraphStore, MergedNeighborIter, MmapTriple, NeighborIter,
    BITPACK_BLOCK_SIZE, DELTA_COMPACTION_RATIO, DELTA_MAGIC, DELTA_PREFIX, DELTA_SUFFIX,
    EDGE_ATTR_SIZE, GRAPH_MAGIC, GRAPH_VERSION, MANIFEST_FILE, MAX_DELTA_LAYERS, MAX_INLINE_DELTAS,
};

// Tests are in store.rs
