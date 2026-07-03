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
    BITPACK_BLOCK_SIZE, BitPackBlock, BitPackKind, CsrLayer, CsrView, DELTA_COMPACTION_RATIO,
    DELTA_MAGIC, DELTA_PREFIX, DELTA_SUFFIX, DeltaHeader, DeltaLayer, EDGE_ATTR_SIZE, EdgeAttr,
    EdgeListEntry, GRAPH_MAGIC, GRAPH_VERSION, GraphBuilder, GraphManifest, GraphStore,
    MANIFEST_FILE, MAX_DELTA_LAYERS, MAX_INLINE_DELTAS, MergedNeighborIter, MmapTriple,
    NeighborIter,
};

// Tests are in store.rs
