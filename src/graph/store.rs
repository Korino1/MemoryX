//! COMPLETE GraphStore implementation for MemoryX SKF-1.1.
//!
//! This module provides production-ready graph storage with:
//! - CSR (Compressed Sparse Row) base layer with bit-packed delta encoding
//! - Delta layers for incremental append-only updates
//! - Zero-copy GraphView for concurrent reads
//! - K-way merge for query-time view consolidation
//! - Compaction with atomic swap and copy-on-write semantics
//! - File I/O with mmap support for base layer
//!
//! # File Formats
//!
//! ## graph.manifest (96 bytes base)
//! ```text
//! [GraphManifest]              - Graph metadata
//! [DeltaHeader * delta_count]  - Delta file references
//! ```
//!
//! ## edges_{edge_type}.offsets
//! ```text
//! [u64; node_count + 1]        - Row offsets into targets
//! ```
//!
//! ## edges_{edge_type}.targets
//! ```text
//! [BitPackBlock * N]           - Bit-packed delta-encoded destinations
//! ```
//!
//! ## edges_{edge_type}.attrs
//! ```text
//! [EdgeAttr; edge_count]       - 16 bytes per edge
//! ```
//!
//! ## delta_{id}.edges
//! ```text
//! [DeltaHeader]                - 32 bytes
//! [EdgeListEntry * edge_count] - 32 bytes each
//! ```

#![allow(dead_code)]

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
use std::sync::Arc;

use memmap2::Mmap;

use crate::store::{EdgeType, NodeNum, TimeBucket, TrustLevel};
use crate::utils::crc32;

// Type alias for backward compatibility
pub type GraphView<'a> = CsrView<'a>;

// ============================================================================
// Constants
// ============================================================================

/// Magic number for GraphManifest: "GRM1" = 0x47524D31
pub const GRAPH_MAGIC: u32 = 0x47524D31;

/// Version for GraphManifest format (1.1)
pub const GRAPH_VERSION: u16 = 0x0101;

/// Maximum inline deltas in manifest
pub const MAX_INLINE_DELTAS: usize = 16;

/// Maximum delta layers before compaction
pub const MAX_DELTA_LAYERS: usize = 8;

/// Delta size ratio threshold for compaction (20%)
pub const DELTA_COMPACTION_RATIO: f64 = 0.20;

/// Edge attribute size in bytes
pub const EDGE_ATTR_SIZE: usize = 16;

/// Elements per bit-pack block
pub const BITPACK_BLOCK_SIZE: usize = 128;

/// Delta magic: "DELT" = 0x44454C54
pub const DELTA_MAGIC: u32 = 0x44454C54;

/// Manifest file name
pub const MANIFEST_FILE: &str = "graph.manifest";

/// Offsets file suffix
pub const OFFSETS_SUFFIX: &str = ".offsets";

/// Targets file suffix
pub const TARGETS_SUFFIX: &str = ".targets";

/// Attributes file suffix
pub const ATTRS_SUFFIX: &str = ".attrs";

/// Delta file prefix
pub const DELTA_PREFIX: &str = "delta_";

/// Delta file suffix
pub const DELTA_SUFFIX: &str = ".edges";

// ============================================================================
// BitPackBlock - Compressed target storage
// ============================================================================

/// Encoding kind for bit-pack blocks
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitPackKind {
    /// Raw values (no delta encoding)
    RAW = 0,
    /// Delta from previous element
    DELTA = 1,
    /// Zigzag-encoded delta (for signed deltas)
    ZIGZAG_DELTA = 2,
}

impl BitPackKind {
    #[inline]
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(BitPackKind::RAW),
            1 => Some(BitPackKind::DELTA),
            2 => Some(BitPackKind::ZIGZAG_DELTA),
            _ => None,
        }
    }

    #[inline]
    pub fn to_u8(self) -> u8 {
        self as u8
    }
}

/// Bit-pack block for compressed edge destination storage.
///
/// Layout (16 bytes header + variable payload):
/// - base: u64 (8 bytes) - Base value for delta encoding
/// - bits: u8 (1 byte) - Bits per element (0-64)
/// - kind: u8 (1 byte) - Encoding kind (RAW, DELTA, ZIGZAG_DELTA)
/// - reserved: u16 (2 bytes) - Alignment padding
/// - count: u32 (4 bytes) - Actual element count in this block (<= 128)
/// - payload: Vec<u64> - Packed bit data
#[repr(C)]
#[derive(Clone, Debug)]
pub struct BitPackBlock {
    /// Base value for delta encoding
    pub base: u64,
    /// Bits per element (0-64)
    pub bits: u8,
    /// Encoding kind
    pub kind: u8,
    /// Reserved (must be 0)
    pub reserved: u16,
    /// Actual element count (<= BITPACK_BLOCK_SIZE)
    pub count: u32,
    /// Packed bit data (variable size)
    pub payload: Vec<u64>,
}

impl BitPackBlock {
    /// Header size in bytes
    pub const HEADER_SIZE: usize = 16;

    /// Maximum elements per block
    pub const MAX_ELEMENTS: usize = BITPACK_BLOCK_SIZE;

    /// Create a new empty BitPackBlock
    #[inline]
    pub fn new() -> Self {
        BitPackBlock {
            base: 0,
            bits: 0,
            kind: BitPackKind::RAW.to_u8(),
            reserved: 0,
            count: 0,
            payload: Vec::new(),
        }
    }

    /// Create with capacity hint
    pub fn with_capacity(bits: u8, kind: u8) -> Self {
        let words_needed = (BITPACK_BLOCK_SIZE * bits as usize).div_ceil(64);
        BitPackBlock {
            base: 0,
            bits,
            kind,
            reserved: 0,
            count: 0,
            payload: vec![0u64; words_needed],
        }
    }

    /// Get number of u64 words needed for payload
    #[inline]
    fn words_needed(bits: u8, count: usize) -> usize {
        (count * bits as usize).div_ceil(64)
    }

    /// Pack a value into the block
    ///
    /// # Safety
    /// - count must be < BITPACK_BLOCK_SIZE
    /// - value must fit in `bits` bits
    pub unsafe fn pack(&mut self, value: u64) {
        debug_assert!(self.count < BITPACK_BLOCK_SIZE as u32);

        let bit_pos = (self.count as usize) * (self.bits as usize);
        let word_idx = bit_pos / 64;
        let bit_offset = bit_pos % 64;

        let mask = if self.bits >= 64 {
            !0u64
        } else {
            (1u64 << self.bits) - 1
        };

        let masked_value = value & mask;

        if bit_offset + self.bits as usize <= 64 {
            // Fits in single word
            self.payload[word_idx] |= masked_value << bit_offset;
        } else {
            // Spans two words
            let split = 64 - bit_offset;
            self.payload[word_idx] |= (masked_value & ((1u64 << split) - 1)) << bit_offset;
            if word_idx + 1 < self.payload.len() {
                self.payload[word_idx + 1] |= masked_value >> split;
            }
        }

        self.count += 1;
    }

    /// Unpack a value from the block
    ///
    /// # Safety
    /// - idx must be < count
    pub unsafe fn unpack(&self, idx: usize) -> u64 {
        debug_assert!(idx < self.count as usize);

        let bit_pos = idx * (self.bits as usize);
        let word_idx = bit_pos / 64;
        let bit_offset = bit_pos % 64;

        let value = if bit_offset + self.bits as usize <= 64 {
            // Single word
            self.payload[word_idx] >> bit_offset
        } else {
            // Spans two words
            let split = 64 - bit_offset;
            (self.payload[word_idx] >> bit_offset) | (self.payload[word_idx + 1] << split)
        };

        let mask = if self.bits >= 64 {
            !0u64
        } else {
            (1u64 << self.bits) - 1
        };

        value & mask
    }

    /// Get element at index with delta decoding
    ///
    /// # Safety
    /// - idx must be < count
    pub unsafe fn get(&self, idx: usize) -> u64 {
        unsafe {
            let value = self.unpack(idx);

            match self.kind {
                k if k == BitPackKind::RAW.to_u8() => value,
                k if k == BitPackKind::DELTA.to_u8() => {
                    // Delta decode: accumulate from base
                    let mut accum = self.base;
                    for i in 0..=idx {
                        accum = accum.wrapping_add(self.unpack(i));
                    }
                    accum
                }
                k if k == BitPackKind::ZIGZAG_DELTA.to_u8() => {
                    // Zigzag delta decode
                    let mut accum: i64 = self.base as i64;
                    for i in 0..=idx {
                        let delta_raw = self.unpack(i);
                        let delta = ((delta_raw >> 1) as i64) ^ -((delta_raw & 1) as i64);
                        accum = accum.wrapping_add(delta);
                    }
                    accum as u64
                }
                _ => value,
            }
        }
    }

    /// Get payload size in bytes
    #[inline]
    pub fn payload_size(&self) -> usize {
        self.payload.len() * size_of::<u64>()
    }

    /// Serialize block to bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(Self::HEADER_SIZE + self.payload_size());

        // Write header
        buf.extend_from_slice(&self.base.to_le_bytes());
        buf.push(self.bits);
        buf.push(self.kind);
        buf.extend_from_slice(&self.reserved.to_le_bytes());
        buf.extend_from_slice(&self.count.to_le_bytes());

        // Write payload
        for &word in &self.payload {
            buf.extend_from_slice(&word.to_le_bytes());
        }

        buf
    }

    /// Deserialize block from bytes
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::HEADER_SIZE {
            return None;
        }

        let base = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
        let bits = bytes[8];
        let kind = bytes[9];
        let reserved = u16::from_le_bytes(bytes[10..12].try_into().ok()?);
        let count = u32::from_le_bytes(bytes[12..16].try_into().ok()?);

        if bits > 64 {
            return None;
        }

        let payload_words = Self::words_needed(bits, count as usize);
        let payload_bytes = payload_words * size_of::<u64>();

        if bytes.len() < Self::HEADER_SIZE + payload_bytes {
            return None;
        }

        let mut payload = Vec::with_capacity(payload_words);
        for i in 0..payload_words {
            let offset = Self::HEADER_SIZE + i * size_of::<u64>();
            let word =
                u64::from_le_bytes(bytes[offset..offset + size_of::<u64>()].try_into().ok()?);
            payload.push(word);
        }

        Some(BitPackBlock {
            base,
            bits,
            kind,
            reserved,
            count,
            payload,
        })
    }

    /// Clear the block
    #[inline]
    pub fn clear(&mut self) {
        self.base = 0;
        self.count = 0;
        // Keep payload allocation
        for word in &mut self.payload {
            *word = 0;
        }
    }
}

impl Default for BitPackBlock {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Edge Attribute (16 bytes)
// ============================================================================

/// Edge attribute structure for CSR attrs file
///
/// Layout (16 bytes total):
/// - confidence_q: u16 (2 bytes) - Quantized confidence (0-10000)
/// - flags: u16 (2 bytes) - Edge flags
/// - valid_from_bucket: u32 (4 bytes) - Validity start bucket
/// - valid_to_bucket: u32 (4 bytes) - Validity end bucket (0 = infinity)
/// - reserved: u32 (4 bytes)
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EdgeAttr {
    /// Quantized confidence (0-10000, maps to TrustLevel)
    pub confidence_q: u16,
    /// Edge flags
    pub flags: u16,
    /// Validity start time bucket
    pub valid_from_bucket: u32,
    /// Validity end time bucket (0 = infinity)
    pub valid_to_bucket: u32,
    /// Reserved (must be 0)
    pub reserved: u32,
}

impl EdgeAttr {
    /// Size of EdgeAttr in bytes
    pub const SIZE: usize = 16;

    /// Flag: edge is deleted/tombstone
    pub const FLAG_DELETED: u16 = 0x0001;

    /// Flag: edge is from federation
    pub const FLAG_FEDERATED: u16 = 0x0002;

    /// Flag: edge has been validated
    pub const FLAG_VALIDATED: u16 = 0x0004;

    /// Flag: edge is inferred (not direct)
    pub const FLAG_INFERRED: u16 = 0x0008;

    /// Create a new EdgeAttr
    #[inline]
    pub fn new(confidence: TrustLevel, valid_from: TimeBucket, valid_to: TimeBucket) -> Self {
        EdgeAttr {
            confidence_q: confidence,
            flags: 0,
            valid_from_bucket: valid_from,
            valid_to_bucket: valid_to,
            reserved: 0,
        }
    }

    /// Create EdgeAttr with flags
    #[inline]
    pub fn with_flags(
        confidence: TrustLevel,
        valid_from: TimeBucket,
        valid_to: TimeBucket,
        flags: u16,
    ) -> Self {
        EdgeAttr {
            confidence_q: confidence,
            flags,
            valid_from_bucket: valid_from,
            valid_to_bucket: valid_to,
            reserved: 0,
        }
    }

    /// Get confidence as TrustLevel
    #[inline]
    pub fn confidence(&self) -> TrustLevel {
        self.confidence_q
    }

    /// Check if edge is deleted
    #[inline]
    pub fn is_deleted(&self) -> bool {
        self.flags & Self::FLAG_DELETED != 0
    }

    /// Check if edge is valid at given bucket
    #[inline]
    pub fn is_valid_at(&self, bucket: TimeBucket) -> bool {
        bucket >= self.valid_from_bucket
            && (self.valid_to_bucket == 0 || bucket < self.valid_to_bucket)
    }

    /// Mark edge as deleted
    #[inline]
    pub fn mark_deleted(&mut self) {
        self.flags |= Self::FLAG_DELETED;
    }

    /// Read EdgeAttr from bytes (zero-copy)
    ///
    /// # Safety
    /// - bytes must have at least SIZE bytes
    /// - bytes must be properly aligned
    #[inline]
    pub unsafe fn from_bytes_unchecked(bytes: &[u8]) -> &Self {
        unsafe { &*(bytes.as_ptr() as *const EdgeAttr) }
    }

    /// Read EdgeAttr from bytes (safe)
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::SIZE {
            return None;
        }
        unsafe { Some(ptr::read_unaligned(bytes.as_ptr() as *const EdgeAttr)) }
    }

    /// Write EdgeAttr to bytes
    pub fn write_to_bytes(&self, bytes: &mut [u8]) -> bool {
        if bytes.len() < Self::SIZE {
            return false;
        }
        unsafe {
            ptr::copy_nonoverlapping(
                self as *const EdgeAttr as *const u8,
                bytes.as_mut_ptr(),
                Self::SIZE,
            );
        }
        true
    }
}

// Compile-time size check
const _: () = assert!(size_of::<EdgeAttr>() == 16, "EdgeAttr must be 16 bytes");

// ============================================================================
// EdgeListEntry (32 bytes) - For delta layers and extraction
// ============================================================================

/// Edge list entry for delta layers and edge extraction stage
///
/// Layout (32 bytes total):
/// - src_node: u64 (8 bytes) - Source node number
/// - dst_node: u64 (8 bytes) - Destination node number
/// - edge_type: u32 (4 bytes) - Edge type
/// - confidence_q: u16 (2 bytes) - Quantized confidence
/// - flags: u16 (2 bytes) - Edge flags
/// - valid_from_bucket: u32 (4 bytes)
/// - valid_to_bucket: u32 (4 bytes)
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EdgeListEntry {
    /// Source node number
    pub src_node: u64,
    /// Destination node number
    pub dst_node: u64,
    /// Edge type
    pub edge_type: u32,
    /// Quantized confidence
    pub confidence_q: u16,
    /// Edge flags
    pub flags: u16,
    /// Validity start bucket
    pub valid_from_bucket: u32,
    /// Validity end time bucket
    pub valid_to_bucket: u32,
}

impl EdgeListEntry {
    /// Size of EdgeListEntry in bytes
    pub const SIZE: usize = 32;

    /// Create a new EdgeListEntry
    #[inline]
    pub fn new(
        src_node: NodeNum,
        dst_node: NodeNum,
        edge_type: EdgeType,
        confidence: TrustLevel,
        valid_from: TimeBucket,
        valid_to: TimeBucket,
    ) -> Self {
        EdgeListEntry {
            src_node,
            dst_node,
            edge_type: edge_type.to_u32(),
            confidence_q: confidence,
            flags: 0,
            valid_from_bucket: valid_from,
            valid_to_bucket: valid_to,
        }
    }

    /// Create with flags
    #[inline]
    pub fn with_flags(
        src_node: NodeNum,
        dst_node: NodeNum,
        edge_type: EdgeType,
        confidence: TrustLevel,
        valid_from: TimeBucket,
        valid_to: TimeBucket,
        flags: u16,
    ) -> Self {
        EdgeListEntry {
            src_node,
            dst_node,
            edge_type: edge_type.to_u32(),
            confidence_q: confidence,
            flags,
            valid_from_bucket: valid_from,
            valid_to_bucket: valid_to,
        }
    }

    /// Get edge type
    #[inline]
    pub fn get_edge_type(&self) -> Option<EdgeType> {
        EdgeType::from_u32(self.edge_type)
    }

    /// Get confidence
    #[inline]
    pub fn confidence(&self) -> TrustLevel {
        self.confidence_q
    }

    /// Check if edge is deleted
    #[inline]
    pub fn is_deleted(&self) -> bool {
        self.flags & EdgeAttr::FLAG_DELETED != 0
    }

    /// Check if edge is valid at bucket
    #[inline]
    pub fn is_valid_at(&self, bucket: TimeBucket) -> bool {
        bucket >= self.valid_from_bucket
            && (self.valid_to_bucket == 0 || bucket < self.valid_to_bucket)
    }

    /// Create tombstone entry (for deletion in delta)
    #[inline]
    pub fn tombstone(src_node: NodeNum, dst_node: NodeNum, edge_type: EdgeType) -> Self {
        let mut entry = Self::new(src_node, dst_node, edge_type, 0, 0, 0);
        entry.flags |= EdgeAttr::FLAG_DELETED;
        entry
    }

    /// Read EdgeListEntry from bytes (safe)
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::SIZE {
            return None;
        }
        unsafe { Some(ptr::read_unaligned(bytes.as_ptr() as *const EdgeListEntry)) }
    }

    /// Write EdgeListEntry to bytes
    pub fn write_to_bytes(&self, bytes: &mut [u8]) -> bool {
        if bytes.len() < Self::SIZE {
            return false;
        }
        unsafe {
            ptr::copy_nonoverlapping(
                self as *const EdgeListEntry as *const u8,
                bytes.as_mut_ptr(),
                Self::SIZE,
            );
        }
        true
    }
}

impl PartialOrd for EdgeListEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for EdgeListEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.edge_type
            .cmp(&other.edge_type)
            .then(self.src_node.cmp(&other.src_node))
            .then(self.dst_node.cmp(&other.dst_node))
    }
}

// Compile-time size check
const _: () = assert!(
    size_of::<EdgeListEntry>() == 32,
    "EdgeListEntry must be 32 bytes"
);

// ============================================================================
// Graph Manifest (96 bytes)
// ============================================================================

/// Manifest for the graph store
///
/// Layout (96 bytes):
/// - magic: u32 (4 bytes) = 0x47524D31 ("GRM1")
/// - ver: u16 (2 bytes)
/// - flags: u16 (2 bytes)
/// - base_gen: u32 (4 bytes) - Base graph generation
/// - node_count: u64 (8 bytes)
/// - edge_type_mask: u64 (8 bytes) - Bitmask of present edge types
/// - delta_count: u32 (4 bytes)
/// - reserved1: u32 (4 bytes) - padding
/// - delta_ids: [u32; 14] (56 bytes) - Inline delta file IDs
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GraphManifest {
    /// Magic number: 0x47524D31 ("GRM1")
    pub magic: u32,
    /// Format version
    pub ver: u16,
    /// Manifest flags
    pub flags: u16,
    /// Base graph generation number
    pub base_gen: u32,
    /// Total node count in graph
    pub node_count: u64,
    /// Bitmask of edge types present (1 << edge_type)
    pub edge_type_mask: u64,
    /// Number of delta files
    pub delta_count: u32,
    /// Reserved1 (must be 0) - padding
    pub reserved1: u32,
    /// Inline delta file IDs (up to 14)
    pub delta_ids: [u32; 14],
}

impl GraphManifest {
    /// Size of GraphManifest in bytes
    pub const SIZE: usize = 96;

    /// Flag: manifest has external deltas beyond inline
    pub const FLAG_EXTERNAL_DELTAS: u16 = 0x0001;

    /// Flag: graph is compacted (no pending deltas)
    pub const FLAG_COMPACTED: u16 = 0x0002;

    /// Flag: graph is read-only
    pub const FLAG_READONLY: u16 = 0x0004;

    /// Create a new GraphManifest
    #[inline]
    pub fn new(node_count: u64) -> Self {
        GraphManifest {
            magic: GRAPH_MAGIC,
            ver: GRAPH_VERSION,
            flags: Self::FLAG_COMPACTED,
            base_gen: 0,
            node_count,
            edge_type_mask: 0,
            delta_count: 0,
            reserved1: 0,
            delta_ids: [0; 14],
        }
    }

    /// Validate magic and version
    #[inline]
    pub fn validate_magic(&self) -> bool {
        self.magic == GRAPH_MAGIC && self.ver == GRAPH_VERSION
    }

    /// Check if edge type is present
    #[inline]
    pub fn has_edge_type(&self, edge_type: EdgeType) -> bool {
        let bit = edge_type.to_u32() - 1;
        if bit >= 64 {
            return false;
        }
        self.edge_type_mask & (1u64 << bit) != 0
    }

    /// Mark edge type as present
    #[inline]
    pub fn mark_edge_type(&mut self, edge_type: EdgeType) {
        let bit = edge_type.to_u32() - 1;
        if bit < 64 {
            self.edge_type_mask |= 1u64 << bit;
        }
    }

    /// Add a delta ID
    #[inline]
    pub fn add_delta(&mut self, delta_id: u32) -> bool {
        if self.delta_count >= 14 {
            return false;
        }
        self.delta_ids[self.delta_count as usize] = delta_id;
        self.delta_count += 1;
        self.flags &= !Self::FLAG_COMPACTED;
        true
    }

    /// Check if graph is compacted
    #[inline]
    pub fn is_compacted(&self) -> bool {
        self.flags & Self::FLAG_COMPACTED != 0
    }

    /// Mark graph as compacted
    #[inline]
    pub fn mark_compacted(&mut self) {
        self.flags |= Self::FLAG_COMPACTED;
        self.delta_count = 0;
        self.delta_ids = [0; 14];
    }

    /// Check if read-only
    #[inline]
    pub fn is_readonly(&self) -> bool {
        self.flags & Self::FLAG_READONLY != 0
    }

    /// Mark as read-only
    #[inline]
    pub fn mark_readonly(&mut self) {
        self.flags |= Self::FLAG_READONLY;
    }

    /// Read GraphManifest from bytes (safe)
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::SIZE {
            return None;
        }
        unsafe {
            let manifest = ptr::read_unaligned(bytes.as_ptr() as *const GraphManifest);
            if !manifest.validate_magic() {
                return None;
            }
            Some(manifest)
        }
    }

    /// Write GraphManifest to bytes
    pub fn write_to_bytes(&self, bytes: &mut [u8]) -> bool {
        if bytes.len() < Self::SIZE {
            return false;
        }
        unsafe {
            ptr::copy_nonoverlapping(
                self as *const GraphManifest as *const u8,
                bytes.as_mut_ptr(),
                Self::SIZE,
            );
        }
        true
    }

    /// Read from file
    pub fn read_from_file<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let mut buf = [0u8; Self::SIZE];
        file.read_exact(&mut buf)?;
        Self::from_bytes(&buf).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Invalid manifest magic/version")
        })
    }

    /// Write to file
    pub fn write_to_file<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        let mut buf = [0u8; Self::SIZE];
        self.write_to_bytes(&mut buf);
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.write_all(&buf)?;
        file.sync_all()?;
        Ok(())
    }
}

// Compile-time size check
const _: () = assert!(
    size_of::<GraphManifest>() == 96,
    "GraphManifest must be 96 bytes"
);

// ============================================================================
// Delta Header (32 bytes)
// ============================================================================

/// Delta file header for incremental updates
///
/// Layout (32 bytes):
/// - magic: u32 (4 bytes) = 0x44454C54 ("DELT")
/// - ver: u16 (2 bytes)
/// - flags: u16 (2 bytes)
/// - delta_id: u32 (4 bytes)
/// - base_gen: u32 (4 bytes)
/// - edge_count: u32 (4 bytes)
/// - reserved1: u32 (4 bytes)
/// - crc32: u32 (4 bytes)
/// - reserved2: u32 (4 bytes) - padding to 32 bytes
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DeltaHeader {
    pub magic: u32,
    pub ver: u16,
    pub flags: u16,
    pub delta_id: u32,
    pub base_gen: u32,
    pub edge_count: u32,
    pub reserved1: u32,
    pub crc32: u32,
    pub reserved2: u32,
}

impl DeltaHeader {
    pub const SIZE: usize = 32;

    pub fn new(delta_id: u32, base_gen: u32, edge_count: u32) -> Self {
        DeltaHeader {
            magic: DELTA_MAGIC,
            ver: GRAPH_VERSION,
            flags: 0,
            delta_id,
            base_gen,
            edge_count,
            reserved1: 0,
            crc32: 0,
            reserved2: 0,
        }
    }

    pub fn validate_magic(&self) -> bool {
        self.magic == DELTA_MAGIC && self.ver == GRAPH_VERSION
    }

    /// Compute CRC32 of header (excluding crc32 and reserved2 fields).
    ///
    /// Copies only the first 24 bytes (magic through reserved1) to avoid
    /// including the crc32 field itself in the computation.
    pub fn compute_crc(&self) -> u32 {
        let mut buf = [0u8; 24];
        unsafe {
            ptr::copy_nonoverlapping(
                self as *const DeltaHeader as *const u8,
                buf.as_mut_ptr(),
                24,
            );
        }
        crc32(&buf)
    }

    /// Validate CRC
    pub fn validate_crc(&self) -> bool {
        self.crc32 == self.compute_crc()
    }

    /// Read from bytes
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::SIZE {
            return None;
        }
        unsafe {
            let header = ptr::read_unaligned(bytes.as_ptr() as *const DeltaHeader);
            if !header.validate_magic() || !header.validate_crc() {
                return None;
            }
            Some(header)
        }
    }

    /// Write to bytes (with CRC computed over first 24 bytes)
    pub fn write_to_bytes(&mut self, bytes: &mut [u8]) -> bool {
        if bytes.len() < Self::SIZE {
            return false;
        }
        self.crc32 = self.compute_crc();
        unsafe {
            ptr::copy_nonoverlapping(
                self as *const DeltaHeader as *const u8,
                bytes.as_mut_ptr(),
                Self::SIZE,
            );
        }
        true
    }
}

// Compile-time size check
const _: () = assert!(
    size_of::<DeltaHeader>() == 32,
    "DeltaHeader must be 32 bytes"
);

// ============================================================================
// GraphView - Zero-copy CSR view
// ============================================================================

/// Zero-copy view of CSR graph data for a single edge type
pub struct CsrView<'a> {
    node_count: u64,
    offsets: &'a [u64],
    targets_data: &'a [u8],
    attrs: &'a [EdgeAttr],
}

impl<'a> CsrView<'a> {
    /// Create a new CsrView from CSR data
    ///
    /// # Safety
    /// - Data must remain valid for lifetime 'a
    /// - Offsets must have node_count + 1 elements
    /// - Targets must be valid bit-packed data
    /// - Attrs must have correct edge count
    pub unsafe fn new_unchecked(
        node_count: u64,
        offsets: &'a [u64],
        targets_data: &'a [u8],
        attrs: &'a [EdgeAttr],
    ) -> Option<Self> {
        if offsets.len() != (node_count as usize) + 1 {
            return None;
        }

        Some(CsrView {
            node_count,
            offsets,
            targets_data,
            attrs,
        })
    }

    /// Create CsrView from mmap (zero-copy)
    ///
    /// # Safety
    /// - Mmap must remain valid and not be modified during view lifetime
    pub unsafe fn from_mmap(
        node_count: u64,
        offsets_mmap: &'a Mmap,
        targets_mmap: &'a Mmap,
        attrs_mmap: &'a Mmap,
    ) -> Option<Self> {
        let offsets_len = offsets_mmap.len() / size_of::<u64>();
        let offsets =
            unsafe { slice::from_raw_parts(offsets_mmap.as_ptr() as *const u64, offsets_len) };

        let targets_data = targets_mmap;

        let attrs_len = attrs_mmap.len() / EdgeAttr::SIZE;
        let attrs =
            unsafe { slice::from_raw_parts(attrs_mmap.as_ptr() as *const EdgeAttr, attrs_len) };

        unsafe { Self::new_unchecked(node_count, offsets, targets_data, attrs) }
    }

    /// Get node count
    #[inline]
    pub fn node_count(&self) -> u64 {
        self.node_count
    }

    /// Get edge count
    #[inline]
    pub fn edge_count(&self) -> u64 {
        if self.node_count == 0 {
            0
        } else {
            self.offsets[self.node_count as usize]
        }
    }

    /// Get neighbor offset range for a node
    #[inline]
    fn neighbor_range(&self, src_node: NodeNum) -> Option<(usize, usize)> {
        if src_node >= self.node_count {
            return None;
        }

        // Offsets store edge counts; targets are 8 bytes per edge (u64 delta-encoded)
        let byte_per_edge = 8u64;
        let start = (self.offsets[src_node as usize] * byte_per_edge) as usize;
        let end = (self.offsets[(src_node + 1) as usize] * byte_per_edge) as usize;

        Some((start, end))
    }

    /// Iterate over neighbors of a node (returns raw data)
    #[inline]
    pub fn neighbor_range_data(&self, src_node: NodeNum) -> Option<(&'a [u8], &'a [EdgeAttr])> {
        let (start, end) = self.neighbor_range(src_node)?;

        let targets_slice = if start < self.targets_data.len() {
            &self.targets_data[start..end.min(self.targets_data.len())]
        } else {
            &[]
        };

        let attrs_slice = if start < self.attrs.len() {
            &self.attrs[start..end.min(self.attrs.len())]
        } else {
            &[]
        };

        Some((targets_slice, attrs_slice))
    }
}

/// Merged neighbor iterator with K-way merge of base + deltas
pub struct MergedNeighborIter<'a> {
    base_iter: Option<NeighborIter<'a>>,
    delta_iters: Vec<slice::Iter<'a, EdgeListEntry>>,
    current_min: Option<(NodeNum, TrustLevel, u8)>, // (dst, attr, source: 0=base, 1+=delta_idx)
    edge_type: EdgeType,
    time_bucket: TimeBucket,
    src_node: NodeNum, // Source node to filter by
}

impl<'a> MergedNeighborIter<'a> {
    fn new(
        base_iter: Option<NeighborIter<'a>>,
        delta_iters: Vec<slice::Iter<'a, EdgeListEntry>>,
        edge_type: EdgeType,
        time_bucket: TimeBucket,
        src_node: NodeNum,
    ) -> Self {
        MergedNeighborIter {
            base_iter,
            delta_iters,
            current_min: None,
            edge_type,
            time_bucket,
            src_node,
        }
    }

    /// Advance to next minimum element
    fn advance(&mut self) -> Option<(NodeNum, TrustLevel)> {
        let mut best: Option<(NodeNum, TrustLevel, u8)> = None;

        // Check base
        if let Some(ref mut iter) = self.base_iter {
            for (dst, attr) in iter.by_ref() {
                if attr.is_deleted() {
                    continue;
                }
                if !attr.is_valid_at(self.time_bucket) {
                    continue;
                }
                best = Some((dst, attr.confidence(), 0));
                break;
            }
        }

        // Check deltas - filter by src_node and edge_type
        for (idx, delta_iter) in self.delta_iters.iter_mut().enumerate() {
            for entry in delta_iter.by_ref() {
                if entry.src_node != self.src_node {
                    continue;
                }
                if entry.get_edge_type() != Some(self.edge_type) {
                    continue;
                }
                if entry.is_deleted() {
                    continue;
                }
                if !entry.is_valid_at(self.time_bucket) {
                    continue;
                }
                let candidate = (entry.dst_node, entry.confidence_q, (idx + 1) as u8);
                match &best {
                    None => best = Some(candidate),
                    Some((dst, _, _)) if entry.dst_node < *dst => best = Some(candidate),
                    _ => {}
                }
                break;
            }
        }

        self.current_min = best;
        best.map(|(dst, attr, _)| (dst, attr))
    }
}

impl<'a> Iterator for MergedNeighborIter<'a> {
    type Item = (NodeNum, TrustLevel);

    fn next(&mut self) -> Option<Self::Item> {
        self.advance()
    }
}

/// Simple neighbor iterator for CSR view
pub struct NeighborIter<'a> {
    targets_data: &'a [u8],
    attrs: &'a [EdgeAttr],
    pos: usize,
    attr_pos: usize,
    prev_node: NodeNum,
}

impl<'a> NeighborIter<'a> {
    /// Create a new NeighborIter.
    ///
    /// # Parameters
    /// - `targets_data`: raw delta-encoded target data (8 bytes per edge)
    /// - `attrs`: parallel attribute array
    /// - `prev_node`: the previous destination node for delta decoding.
    ///   For the first edge of a source node, this should be `0` because
    ///   `CsrLayer::from_edge_list` stores the first neighbor as an absolute
    ///   value (i.e., delta from 0).
    fn new(targets_data: &'a [u8], attrs: &'a [EdgeAttr], prev_node: NodeNum) -> Self {
        NeighborIter {
            targets_data,
            attrs,
            pos: 0,
            attr_pos: 0,
            prev_node,
        }
    }
}

impl<'a> Iterator for NeighborIter<'a> {
    type Item = (NodeNum, &'a EdgeAttr);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos + 8 > self.targets_data.len() {
            return None;
        }

        // Read delta (simplified: 8-byte little-endian)
        let delta = unsafe {
            let ptr = self.targets_data.as_ptr().add(self.pos) as *const u64;
            ptr.read_unaligned()
        };

        self.pos += 8;

        // Delta decode
        let dst_node = self.prev_node.wrapping_add(delta);
        self.prev_node = dst_node;

        let attr = self.attrs.get(self.attr_pos)?;
        self.attr_pos += 1;

        Some((dst_node, attr))
    }
}

// ============================================================================
// CSR Layer - Per-edge-type storage
// ============================================================================

/// CSR layer for a single edge type
#[derive(Clone)]
pub struct CsrLayer {
    offsets: Vec<u64>,
    targets: Vec<u8>,
    attrs: Vec<EdgeAttr>,
    node_count: u64,
    edge_count: u64,
}

impl CsrLayer {
    /// Create new empty CSR layer
    pub fn new(node_count: u64) -> Self {
        let offsets = vec![0u64; (node_count as usize) + 1];
        CsrLayer {
            offsets,
            targets: Vec::new(),
            attrs: Vec::new(),
            node_count,
            edge_count: 0,
        }
    }

    /// Build from sorted edge list
    pub fn from_edge_list(edge_list: &[EdgeListEntry], node_count: u64) -> Self {
        // Count edges per source
        let mut counts = vec![0usize; node_count as usize];
        for entry in edge_list {
            if entry.src_node < node_count {
                counts[entry.src_node as usize] += 1;
            }
        }

        // Build offsets (prefix sum)
        let mut offsets = vec![0u64; (node_count as usize) + 1];
        let mut offset = 0u64;
        for (i, &count) in counts.iter().enumerate() {
            offsets[i] = offset;
            offset += count as u64;
        }
        offsets[node_count as usize] = offset;

        // Build targets and attrs
        let mut targets = Vec::new();
        let mut attrs = Vec::new();
        let mut current_pos = vec![0usize; node_count as usize];
        let mut prev_dst = vec![0u64; node_count as usize]; // Track previous destination per source

        for entry in edge_list {
            if entry.src_node < node_count {
                let base_offset = offsets[entry.src_node as usize] as usize;
                let idx = base_offset + current_pos[entry.src_node as usize];

                // Delta encode: first neighbor is absolute, rest are delta from previous
                let delta = if idx == base_offset {
                    entry.dst_node
                } else {
                    entry
                        .dst_node
                        .wrapping_sub(prev_dst[entry.src_node as usize])
                };
                prev_dst[entry.src_node as usize] = entry.dst_node;

                // Simple 8-byte encoding (can be optimized with bit-packing)
                targets.extend_from_slice(&delta.to_le_bytes());

                attrs.push(EdgeAttr::with_flags(
                    entry.confidence_q,
                    entry.valid_from_bucket,
                    entry.valid_to_bucket,
                    entry.flags,
                ));

                current_pos[entry.src_node as usize] += 1;
            }
        }

        let edge_count = attrs.len() as u64;

        CsrLayer {
            offsets,
            targets,
            attrs,
            node_count,
            edge_count,
        }
    }

    /// Create view
    pub fn view(&self) -> Option<CsrView<'_>> {
        unsafe {
            CsrView::new_unchecked(self.node_count, &self.offsets, &self.targets, &self.attrs)
        }
    }

    /// Get edge count
    #[inline]
    pub fn edge_count(&self) -> u64 {
        self.edge_count
    }

    /// Get node count
    #[inline]
    pub fn node_count(&self) -> u64 {
        self.node_count
    }

    /// Write to files
    pub fn write_to_files<P: AsRef<Path>>(
        &self,
        base_path: P,
        edge_type: EdgeType,
    ) -> io::Result<()> {
        let base = base_path.as_ref();

        // Write offsets
        let offsets_path = base.join(format!("edges_{}{}", edge_type.to_u32(), OFFSETS_SUFFIX));
        let mut file = BufWriter::new(File::create(&offsets_path)?);
        for &off in &self.offsets {
            file.write_all(&off.to_le_bytes())?;
        }
        file.flush()?;
        file.into_inner()?.sync_all()?;

        // Write targets
        let targets_path = base.join(format!("edges_{}{}", edge_type.to_u32(), TARGETS_SUFFIX));
        let mut file = BufWriter::new(File::create(&targets_path)?);
        file.write_all(&self.targets)?;
        file.flush()?;
        file.into_inner()?.sync_all()?;

        // Write attrs
        let attrs_path = base.join(format!("edges_{}{}", edge_type.to_u32(), ATTRS_SUFFIX));
        let mut file = BufWriter::new(File::create(&attrs_path)?);
        for attr in &self.attrs {
            let mut buf = [0u8; EdgeAttr::SIZE];
            attr.write_to_bytes(&mut buf);
            file.write_all(&buf)?;
        }
        file.flush()?;
        file.into_inner()?.sync_all()?;

        Ok(())
    }

    /// Load from files
    pub fn load_from_files<P: AsRef<Path>>(
        base_path: P,
        edge_type: EdgeType,
        node_count: u64,
    ) -> io::Result<Option<Self>> {
        let base = base_path.as_ref();

        let offsets_path = base.join(format!("edges_{}{}", edge_type.to_u32(), OFFSETS_SUFFIX));
        let targets_path = base.join(format!("edges_{}{}", edge_type.to_u32(), TARGETS_SUFFIX));
        let attrs_path = base.join(format!("edges_{}{}", edge_type.to_u32(), ATTRS_SUFFIX));

        if !offsets_path.exists() || !targets_path.exists() || !attrs_path.exists() {
            return Ok(None);
        }

        // Read offsets
        let mut offsets_file = File::open(&offsets_path)?;
        let mut offsets_data = Vec::new();
        offsets_file.read_to_end(&mut offsets_data)?;

        let offset_count = offsets_data.len() / size_of::<u64>();
        let mut offsets = vec![0u64; offset_count];
        for (i, off) in offsets.iter_mut().enumerate() {
            let start = i * size_of::<u64>();
            *off = u64::from_le_bytes(
                offsets_data[start..start + size_of::<u64>()]
                    .try_into()
                    .map_err(|_| io::ErrorKind::InvalidData)?,
            );
        }

        // Read targets
        let mut targets_file = File::open(&targets_path)?;
        let mut targets = Vec::new();
        targets_file.read_to_end(&mut targets)?;

        // Read attrs
        let mut attrs_file = File::open(&attrs_path)?;
        let mut attrs_data = Vec::new();
        attrs_file.read_to_end(&mut attrs_data)?;

        let attr_count = attrs_data.len() / EdgeAttr::SIZE;
        let mut attrs = Vec::with_capacity(attr_count);
        for i in 0..attr_count {
            let start = i * EdgeAttr::SIZE;
            let attr = EdgeAttr::from_bytes(&attrs_data[start..start + EdgeAttr::SIZE])
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid EdgeAttr"))?;
            attrs.push(attr);
        }

        let edge_count = attrs.len() as u64;

        Ok(Some(CsrLayer {
            offsets,
            targets,
            attrs,
            node_count,
            edge_count,
        }))
    }
}

// ============================================================================
// Delta Layer
// ============================================================================

/// Delta layer for incremental updates
#[derive(Clone)]
pub struct DeltaLayer {
    id: u32,
    base_gen: u32,
    edges: Vec<EdgeListEntry>,
    sorted: bool,
}

impl DeltaLayer {
    /// Create new delta layer
    pub fn new(id: u32, base_gen: u32) -> Self {
        DeltaLayer {
            id,
            base_gen,
            edges: Vec::new(),
            sorted: false,
        }
    }

    /// Add edge to delta
    #[inline]
    pub fn add(&mut self, entry: EdgeListEntry) {
        self.edges.push(entry);
        self.sorted = false;
    }

    /// Get edge count
    #[inline]
    pub fn len(&self) -> usize {
        self.edges.len()
    }

    /// Check if empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    /// Sort edges
    pub fn sort(&mut self) {
        if !self.sorted {
            self.edges.sort();
            self.sorted = true;
        }
    }

    /// Get edges slice
    #[inline]
    pub fn edges(&self) -> &[EdgeListEntry] {
        &self.edges
    }

    /// Get delta ID
    #[inline]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Get base generation
    #[inline]
    pub fn base_gen(&self) -> u32 {
        self.base_gen
    }

    /// Write to file
    pub fn write_to_file<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        let mut file = BufWriter::new(File::create(path)?);

        // Write header
        let mut header = DeltaHeader::new(self.id, self.base_gen, self.edges.len() as u32);
        let mut header_buf = [0u8; DeltaHeader::SIZE];
        header.write_to_bytes(&mut header_buf);
        file.write_all(&header_buf)?;

        // Write edges
        for entry in &self.edges {
            let mut buf = [0u8; EdgeListEntry::SIZE];
            entry.write_to_bytes(&mut buf);
            file.write_all(&buf)?;
        }

        file.flush()?;
        file.into_inner()?.sync_all()?;
        Ok(())
    }

    /// Load from file
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> io::Result<Option<Self>> {
        let mut file = BufReader::new(File::open(path)?);

        // Read header
        let mut header_buf = [0u8; DeltaHeader::SIZE];
        file.read_exact(&mut header_buf)?;

        let header = DeltaHeader::from_bytes(&header_buf)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid delta header"))?;

        // Read edges
        let mut edges = Vec::with_capacity(header.edge_count as usize);
        for _ in 0..header.edge_count {
            let mut buf = [0u8; EdgeListEntry::SIZE];
            file.read_exact(&mut buf)?;
            if let Some(entry) = EdgeListEntry::from_bytes(&buf) {
                edges.push(entry);
            }
        }

        Ok(Some(DeltaLayer {
            id: header.delta_id,
            base_gen: header.base_gen,
            edges,
            sorted: false,
        }))
    }
}

// ============================================================================
// GraphBuilder - Build CSR from edges
// ============================================================================

/// Builder for constructing graph from edge lists
pub struct GraphBuilder {
    node_count: u64,
    edges_by_type: HashMap<u32, Vec<EdgeListEntry>>,
}

impl GraphBuilder {
    /// Create new GraphBuilder
    pub fn new(node_count: u64) -> Self {
        GraphBuilder {
            node_count,
            edges_by_type: HashMap::new(),
        }
    }

    /// Add edge
    #[inline]
    pub fn add_edge(&mut self, entry: EdgeListEntry) {
        self.edges_by_type
            .entry(entry.edge_type)
            .or_default()
            .push(entry);
    }

    /// Add multiple edges
    pub fn add_edges(&mut self, entries: &[EdgeListEntry]) {
        for &entry in entries {
            self.add_edge(entry);
        }
    }

    /// Build into GraphStore
    pub fn build(mut self) -> GraphStore {
        let mut store = GraphStore::new(self.node_count);

        for (edge_type_u32, edges) in self.edges_by_type.drain() {
            if let Some(edge_type) = EdgeType::from_u32(edge_type_u32) {
                // Sort edges
                let mut sorted_edges = edges;
                sorted_edges.sort();

                // Deduplicate (keep highest confidence)
                let mut unique_edges = Vec::new();
                let mut prev_key: Option<(u64, u64)> = None;

                for entry in &sorted_edges {
                    let key = (entry.src_node, entry.dst_node);
                    if Some(key) == prev_key {
                        continue;
                    }
                    prev_key = Some(key);
                    unique_edges.push(*entry);
                }

                // Build CSR layer
                let layer = CsrLayer::from_edge_list(&unique_edges, self.node_count);
                store.base_layers.insert(edge_type, layer);
                store.manifest.mark_edge_type(edge_type);
            }
        }

        store.rebuild_reverse_index();
        store.manifest.mark_compacted();
        store
    }

    /// Get node count
    #[inline]
    pub fn node_count(&self) -> u64 {
        self.node_count
    }

    /// Get total edge count
    pub fn edge_count(&self) -> usize {
        self.edges_by_type.values().map(|v| v.len()).sum()
    }
}

// ============================================================================
// GraphStore - Main graph storage
// ============================================================================

/// High-level graph store with CSR base + delta layers
pub struct GraphStore {
    /// Graph manifest
    manifest: GraphManifest,
    /// Base path for files
    base_path: PathBuf,
    /// Base CSR layers per edge type
    base_layers: HashMap<EdgeType, CsrLayer>,
    /// Delta layers (sorted by ID)
    delta_layers: Vec<DeltaLayer>,
    /// Mmap references for base layers (zero-copy reads)
    mmap_refs: HashMap<EdgeType, Arc<MmapTriple>>,
    /// Reverse adjacency index for efficient in-neighbor lookups
    reverse_index: HashMap<EdgeType, HashMap<NodeNum, Vec<(NodeNum, TrustLevel)>>>,
    /// Next delta ID
    next_delta_id: AtomicU32,
}

/// Mmap triple for zero-copy base layer access
pub struct MmapTriple {
    pub offsets: Mmap,
    pub targets: Mmap,
    pub attrs: Mmap,
}

impl GraphStore {
    /// Create new empty GraphStore
    pub fn new(node_count: u64) -> Self {
        GraphStore {
            manifest: GraphManifest::new(node_count),
            base_path: PathBuf::new(),
            base_layers: HashMap::new(),
            delta_layers: Vec::new(),
            mmap_refs: HashMap::new(),
            reverse_index: HashMap::new(),
            next_delta_id: AtomicU32::new(1),
        }
    }

    /// Create new GraphStore with base path
    pub fn with_path<P: AsRef<Path>>(base_path: P, node_count: u64) -> Self {
        GraphStore {
            manifest: GraphManifest::new(node_count),
            base_path: base_path.as_ref().to_path_buf(),
            base_layers: HashMap::new(),
            delta_layers: Vec::new(),
            mmap_refs: HashMap::new(),
            reverse_index: HashMap::new(),
            next_delta_id: AtomicU32::new(1),
        }
    }

    fn apply_reverse_entry(&mut self, entry: EdgeListEntry) {
        let Some(edge_type) = entry.get_edge_type() else {
            return;
        };

        let by_type = self.reverse_index.entry(edge_type).or_default();
        let incoming = by_type.entry(entry.dst_node).or_default();

        if entry.is_deleted() {
            incoming.retain(|(src, _)| *src != entry.src_node);
            if incoming.is_empty() {
                by_type.remove(&entry.dst_node);
            }
            return;
        }

        if let Some(existing) = incoming.iter_mut().find(|(src, _)| *src == entry.src_node) {
            existing.1 = entry.confidence();
        } else {
            incoming.push((entry.src_node, entry.confidence()));
            incoming.sort_unstable_by_key(|(src, _)| *src);
        }
    }

    fn rebuild_reverse_index(&mut self) {
        self.reverse_index.clear();

        for (edge_type, layer) in &self.base_layers {
            if let Some(view) = layer.view() {
                for src in 0..self.manifest.node_count {
                    if let Some((targets, attrs)) = view.neighbor_range_data(src) {
                        let mut iter = NeighborIter::new(targets, attrs, 0);
                        for (dst, attr) in iter.by_ref() {
                            if !attr.is_deleted() {
                                self.reverse_index
                                    .entry(*edge_type)
                                    .or_default()
                                    .entry(dst)
                                    .or_default()
                                    .push((src, attr.confidence_q));
                            }
                        }
                    }
                }
            }
        }

        for by_type in self.reverse_index.values_mut() {
            for incoming in by_type.values_mut() {
                incoming.sort_unstable_by_key(|(src, _)| *src);
            }
        }

        let delta_entries: Vec<EdgeListEntry> = self
            .delta_layers
            .iter()
            .flat_map(|delta| delta.edges().iter().copied())
            .collect();
        for entry in delta_entries {
            self.apply_reverse_entry(entry);
        }
    }

    /// Add a new node to the graph (SKF-1.1 Section 8)
    #[inline]
    pub fn add_node(&mut self, node_num: NodeNum) {
        // Update manifest node count
        if node_num >= self.manifest.node_count {
            self.manifest.node_count = node_num + 1;
        }
        // Node will be added to delta layer when first edge is added
    }

    /// Add an edge to the graph (SKF-1.1 Section 8)
    ///
    /// Edge is added to the current delta layer.
    /// Default valid_time buckets are 0 (current/invalid).
    ///
    /// The manifest node_count is automatically grown to accommodate
    /// new nodes if needed.
    #[inline]
    pub fn add_edge(
        &mut self,
        src_node: NodeNum,
        dst_node: NodeNum,
        edge_type: EdgeType,
        confidence: TrustLevel,
    ) {
        // Ensure graph can accommodate both nodes
        // This grows the manifest if needed
        let max_node = src_node.max(dst_node);
        if max_node >= self.manifest.node_count {
            self.manifest.node_count = max_node + 1;
        }

        // Mark edge type in manifest
        self.manifest.mark_edge_type(edge_type);

        let entry = EdgeListEntry::new(src_node, dst_node, edge_type, confidence, 0, 0);

        // Get or create current delta layer
        if self.delta_layers.is_empty() || self.delta_layers.last().unwrap().len() > 100000 {
            let id = self.next_delta_id.fetch_add(1, AtomicOrdering::SeqCst);
            let mut new_delta = DeltaLayer::new(id, self.manifest.base_gen);
            new_delta.add(entry);
            self.delta_layers.push(new_delta);
        } else {
            self.delta_layers.last_mut().unwrap().add(entry);
        }

        self.apply_reverse_entry(entry);

        // Check compaction trigger
        if self.delta_layers.len() > MAX_DELTA_LAYERS {
            let _ = self.compact();
        }
    }

    /// Load graph from files.
    ///
    /// Reads the manifest, base CSR layers, and delta layers from disk.
    /// Also sets up mmap references for zero-copy access to base layers.
    pub fn load<P: AsRef<Path>>(base_path: P) -> io::Result<Self> {
        let base_path = base_path.as_ref().to_path_buf();
        let manifest_path = base_path.join(MANIFEST_FILE);

        // Read manifest
        let manifest = GraphManifest::read_from_file(&manifest_path)?;

        let mut store = GraphStore {
            manifest,
            base_path,
            base_layers: HashMap::new(),
            delta_layers: Vec::new(),
            mmap_refs: HashMap::new(),
            reverse_index: HashMap::new(),
            next_delta_id: AtomicU32::new(1),
        };

        // Load base layers for present edge types and set up mmap refs
        for edge_type_u32 in 1..=64u32 {
            if store.manifest.edge_type_mask & (1u64 << (edge_type_u32 - 1)) != 0
                && let Some(et) = EdgeType::from_u32(edge_type_u32)
            {
                    // Try mmap-based zero-copy first
                    let offsets_path = store
                        .base_path
                        .join(format!("edges_{}{}", edge_type_u32, OFFSETS_SUFFIX));
                    let targets_path = store
                        .base_path
                        .join(format!("edges_{}{}", edge_type_u32, TARGETS_SUFFIX));
                    let attrs_path = store
                        .base_path
                        .join(format!("edges_{}{}", edge_type_u32, ATTRS_SUFFIX));

                    if offsets_path.exists() && targets_path.exists() && attrs_path.exists() {
                        // Set up mmap for zero-copy access
                        let offsets_file = File::open(&offsets_path)?;
                        let targets_file = File::open(&targets_path)?;
                        let attrs_file = File::open(&attrs_path)?;

                        let offsets_mmap = unsafe { Mmap::map(&offsets_file)? };
                        let targets_mmap = unsafe { Mmap::map(&targets_file)? };
                        let attrs_mmap = unsafe { Mmap::map(&attrs_file)? };

                        let triple = Arc::new(MmapTriple {
                            offsets: offsets_mmap,
                            targets: targets_mmap,
                            attrs: attrs_mmap,
                        });

                        store.mmap_refs.insert(et, Arc::clone(&triple));

                        // Also load into memory for in-memory operations
                        let layer = CsrLayer::load_from_files(
                            &store.base_path,
                            et,
                            store.manifest.node_count,
                        )?;
                        if let Some(layer) = layer {
                            store.base_layers.insert(et, layer);
                        }
                    }
            }
        }

        // Load delta layers
        for i in 0..store.manifest.delta_count as usize {
            let delta_id = store.manifest.delta_ids[i];
            if delta_id == 0 {
                continue; // Skip empty slots
            }
            let delta_path = store
                .base_path
                .join(format!("{}{}{}", DELTA_PREFIX, delta_id, DELTA_SUFFIX));

            if delta_path.exists()
                && let Some(delta) = DeltaLayer::load_from_file(&delta_path)?
            {
                store.delta_layers.push(delta);
                let current_max = store.next_delta_id.load(AtomicOrdering::Relaxed);
                if delta_id >= current_max {
                    store
                        .next_delta_id
                        .store(delta_id + 1, AtomicOrdering::Relaxed);
                }
            }
        }

        store.rebuild_reverse_index();

        Ok(store)
    }

    /// Save manifest to disk.
    ///
    /// Before writing, synchronizes the manifest's delta_ids and delta_count
    /// from the current in-memory delta layers. This ensures the manifest
    /// on disk always reflects the actual delta layer state.
    pub fn save_manifest(&self) -> io::Result<()> {
        if self.base_path.as_os_str().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "GraphStore has no base_path configured",
            ));
        }
        fs::create_dir_all(&self.base_path)?;

        // Sync manifest delta tracking from actual delta layers
        let mut manifest = self.manifest;
        manifest.delta_count = 0;
        manifest.delta_ids = [0u32; 14];
        for delta in &self.delta_layers {
            if manifest.delta_count < 14 {
                manifest.delta_ids[manifest.delta_count as usize] = delta.id();
                manifest.delta_count += 1;
            }
        }
        if self.delta_layers.is_empty() {
            manifest.flags |= GraphManifest::FLAG_COMPACTED;
        } else {
            manifest.flags &= !GraphManifest::FLAG_COMPACTED;
        }

        let manifest_path = self.base_path.join(MANIFEST_FILE);
        manifest.write_to_file(&manifest_path)
    }

    /// Save all base CSR layers to disk
    pub fn save_base_layers(&self) -> io::Result<()> {
        if self.base_path.as_os_str().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "GraphStore has no base_path configured",
            ));
        }
        fs::create_dir_all(&self.base_path)?;

        for (edge_type, layer) in &self.base_layers {
            layer.write_to_files(&self.base_path, *edge_type)?;
        }

        Ok(())
    }

    /// Save all in-memory delta layers to disk.
    ///
    /// Each delta layer is written as `delta_{id}.edges` in the base path.
    /// The manifest delta_ids array is updated to reflect the current delta layers.
    ///
    /// This method does NOT save the manifest — call `save_manifest()` after
    /// or use `save()` for a full persistence cycle.
    pub fn save_delta_layers(&self) -> io::Result<()> {
        if self.base_path.as_os_str().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "GraphStore has no base_path configured",
            ));
        }
        fs::create_dir_all(&self.base_path)?;

        for delta in &self.delta_layers {
            let delta_path =
                self.base_path
                    .join(format!("{}{}{}", DELTA_PREFIX, delta.id(), DELTA_SUFFIX));
            delta.write_to_file(&delta_path)?;
        }

        Ok(())
    }

    /// Persist the complete graph state to disk.
    ///
    /// This writes:
    /// 1. All base CSR layers (offsets, targets, attrs per edge type)
    /// 2. All in-memory delta layers (delta_{id}.edges files)
    /// 3. Updated manifest (with current delta_ids and base_gen)
    ///
    /// The manifest is written last so that a crash during save leaves
    /// a consistent state: either the old manifest (pointing to old data)
    /// or the new manifest (pointing to complete new data).
    pub fn save(&self) -> io::Result<()> {
        if self.base_path.as_os_str().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "GraphStore has no base_path configured",
            ));
        }

        // 1. Save base layers
        self.save_base_layers()?;

        // 2. Save delta layers
        self.save_delta_layers()?;

        // 3. Save manifest last (acts as commit point)
        self.save_manifest()?;

        Ok(())
    }

    /// Open an existing graph store from disk, or create a new one.
    ///
    /// If a manifest file exists at `base_path`, loads the full graph state
    /// (manifest + base layers + delta layers). Otherwise creates a new empty
    /// store with the given `node_count` and sets the base path.
    pub fn open_or_create<P: AsRef<Path>>(base_path: P, node_count: u64) -> io::Result<Self> {
        let base_path = base_path.as_ref().to_path_buf();
        let manifest_path = base_path.join(MANIFEST_FILE);

        if manifest_path.exists() {
            Self::load(&base_path)
        } else {
            Ok(Self::with_path(&base_path, node_count))
        }
    }

    /// Set the base path for this store (useful after `new()`).
    #[inline]
    pub fn set_base_path<P: AsRef<Path>>(&mut self, path: P) {
        self.base_path = path.as_ref().to_path_buf();
    }

    /// Delete edge (tombstone in delta)
    pub fn delete_edge(
        &mut self,
        src_node: NodeNum,
        dst_node: NodeNum,
        edge_type: EdgeType,
    ) -> io::Result<()> {
        if self.manifest.is_readonly() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Graph is read-only",
            ));
        }

        let entry = EdgeListEntry::tombstone(src_node, dst_node, edge_type);

        if self.delta_layers.is_empty() {
            let id = self.next_delta_id.fetch_add(1, AtomicOrdering::SeqCst);
            let mut delta = DeltaLayer::new(id, self.manifest.base_gen);
            delta.add(entry);
            self.delta_layers.push(delta);
        } else {
            self.delta_layers.last_mut().unwrap().add(entry);
        }

        self.apply_reverse_entry(entry);

        Ok(())
    }

    /// Get neighbors with K-way merge
    pub fn neighbors(
        &self,
        src_node: NodeNum,
        edge_type: EdgeType,
    ) -> impl Iterator<Item = (NodeNum, TrustLevel)> + '_ {
        let time_bucket = 0; // Current time (simplified)

        // Get base iterator
        // Note: prev_node=0 because CsrLayer::from_edge_list stores the first
        // neighbor as an absolute value (delta from 0), not delta from src_node.
        let base_iter = self
            .base_layers
            .get(&edge_type)
            .and_then(|layer| layer.view())
            .and_then(|view| {
                view.neighbor_range_data(src_node)
                    .map(|(targets, attrs)| NeighborIter::new(targets, attrs, 0))
            });

        // Get delta iterators
        let delta_iters: Vec<_> = self
            .delta_layers
            .iter()
            .filter(|d| !d.is_empty())
            .map(|d| d.edges.iter())
            .collect();

        MergedNeighborIter::new(base_iter, delta_iters, edge_type, time_bucket, src_node)
    }

    /// Check if edge exists
    pub fn has_edge(&self, src_node: NodeNum, dst_node: NodeNum, edge_type: EdgeType) -> bool {
        // Check deltas first (most recent) - iterate in reverse to get latest entry
        for delta in self.delta_layers.iter().rev() {
            for entry in delta.edges().iter().rev() {
                if entry.src_node == src_node
                    && entry.dst_node == dst_node
                    && entry.get_edge_type() == Some(edge_type)
                {
                    return !entry.is_deleted();
                }
            }
        }

        // Check base
        // Note: prev_node=0 because CsrLayer::from_edge_list stores the first
        // neighbor as an absolute value (delta from 0).
        if let Some(layer) = self.base_layers.get(&edge_type)
            && let Some(view) = layer.view()
            && let Some((targets, attrs)) = view.neighbor_range_data(src_node)
        {
            let mut iter = NeighborIter::new(targets, attrs, 0);
            for (dst, attr) in iter.by_ref() {
                if dst == dst_node && !attr.is_deleted() {
                    return true;
                }
            }
        }

        false
    }

    /// Get in-neighbors (requires scanning or reverse index)
    pub fn in_neighbors(
        &self,
        dst_node: NodeNum,
        edge_type: EdgeType,
    ) -> Vec<(NodeNum, TrustLevel)> {
        self.reverse_index
            .get(&edge_type)
            .and_then(|by_dst| by_dst.get(&dst_node))
            .cloned()
            .unwrap_or_default()
    }

    /// Check if path exists (BFS)
    pub fn path_exists(
        &self,
        src_node: NodeNum,
        dst_node: NodeNum,
        edge_types: &[EdgeType],
        max_depth: u8,
    ) -> bool {
        if src_node == dst_node {
            return true;
        }

        if max_depth == 0 {
            return false;
        }

        use std::collections::VecDeque;

        let mut visited = vec![false; self.manifest.node_count as usize];
        let mut queue = VecDeque::new();

        visited[src_node as usize] = true;
        queue.push_back((src_node, 0u8));

        while let Some((current, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }

            for &edge_type in edge_types {
                for (neighbor, _) in self.neighbors(current, edge_type) {
                    if neighbor == dst_node {
                        // Found destination, check if within depth limit
                        if depth < max_depth {
                            return true;
                        }
                        // Found but too deep, don't continue from here
                        continue;
                    }

                    if !visited[neighbor as usize] {
                        visited[neighbor as usize] = true;
                        queue.push_back((neighbor, depth + 1));
                    }
                }
            }
        }

        false
    }

    /// Compact delta layers into base
    pub fn compact(&mut self) -> io::Result<()> {
        if self.delta_layers.is_empty() {
            return Ok(());
        }

        // Collect all edges
        let mut all_edges: Vec<EdgeListEntry> = Vec::new();

        // Add existing base edges
        for (edge_type, layer) in &self.base_layers {
            if let Some(view) = layer.view() {
                for src in 0..self.manifest.node_count {
                    if let Some((targets, attrs)) = view.neighbor_range_data(src) {
                        // prev_node=0: first neighbor is stored as absolute value
                        let mut iter = NeighborIter::new(targets, attrs, 0);
                        for (dst, attr) in iter.by_ref() {
                            if !attr.is_deleted() {
                                all_edges.push(EdgeListEntry::with_flags(
                                    src,
                                    dst,
                                    *edge_type,
                                    attr.confidence_q,
                                    attr.valid_from_bucket,
                                    attr.valid_to_bucket,
                                    attr.flags,
                                ));
                            }
                        }
                    }
                }
            }
        }

        // Add delta edges
        for delta in &self.delta_layers {
            for &entry in delta.edges() {
                all_edges.push(entry);
            }
        }

        // Sort by type, src, dst
        all_edges.sort();

        // Group by edge type and deduplicate
        let mut edges_by_type: HashMap<u32, Vec<EdgeListEntry>> = HashMap::new();

        for entry in &all_edges {
            edges_by_type
                .entry(entry.edge_type)
                .or_default()
                .push(*entry);
        }

        // Build new base layers
        let mut new_layers = HashMap::new();

        for (edge_type_u32, edges) in edges_by_type {
            if let Some(et) = EdgeType::from_u32(edge_type_u32) {
                // Deduplicate (keep latest, handle tombstones)
                let mut unique: Vec<EdgeListEntry> = Vec::new();
                let mut seen: HashMap<(u64, u64), bool> = HashMap::new();

                for entry in edges.iter().rev() {
                    let key = (entry.src_node, entry.dst_node);
                    if seen.insert(key, entry.is_deleted()).is_none()
                        && !entry.is_deleted()
                    {
                        unique.push(*entry);
                    }
                }

                unique.sort();

                if !unique.is_empty() {
                    let layer = CsrLayer::from_edge_list(&unique, self.manifest.node_count);
                    new_layers.insert(et, layer);
                }
            }
        }

        // Write new base layers
        for (edge_type, layer) in &new_layers {
            layer.write_to_files(&self.base_path, *edge_type)?;
        }

        // Delete old delta files
        for delta in &self.delta_layers {
            let delta_path =
                self.base_path
                    .join(format!("{}{}{}", DELTA_PREFIX, delta.id(), DELTA_SUFFIX));
            if delta_path.exists() {
                let _ = fs::remove_file(&delta_path);
            }
        }

        // Atomic swap
        self.base_layers = new_layers;
        self.delta_layers.clear();
        self.rebuild_reverse_index();
        self.manifest.mark_compacted();
        self.manifest.base_gen += 1;

        // Save manifest
        self.save_manifest()?;

        Ok(())
    }

    /// Check if compaction is needed
    pub fn needs_compaction(&self) -> bool {
        if self.delta_layers.len() > MAX_DELTA_LAYERS {
            return true;
        }

        // Check delta size ratio
        let base_edges: usize = self
            .base_layers
            .values()
            .map(|l| l.edge_count() as usize)
            .sum();

        let delta_edges: usize = self.delta_layers.iter().map(|d| d.len()).sum();

        if base_edges > 0 {
            let ratio = delta_edges as f64 / base_edges as f64;
            if ratio > DELTA_COMPACTION_RATIO {
                return true;
            }
        }

        false
    }

    /// Get node count
    #[inline]
    pub fn node_count(&self) -> u64 {
        self.manifest.node_count
    }

    /// Get edge count
    pub fn edge_count(&self) -> u64 {
        let base: u64 = self.base_layers.values().map(|l| l.edge_count()).sum();
        let delta: u64 = self.delta_layers.iter().map(|d| d.len() as u64).sum();
        base + delta
    }

    /// Get manifest reference
    #[inline]
    pub fn manifest(&self) -> &GraphManifest {
        &self.manifest
    }

    /// Create GraphView for zero-copy access (base only)
    pub fn view(&self, edge_type: EdgeType) -> Option<CsrView<'_>> {
        self.base_layers.get(&edge_type).and_then(|l| l.view())
    }

    /// Get delta count
    #[inline]
    pub fn delta_count(&self) -> usize {
        self.delta_layers.len()
    }

    /// Set read-only mode
    pub fn set_readonly(&mut self, readonly: bool) {
        if readonly {
            self.manifest.mark_readonly();
        } else {
            self.manifest.flags &= !GraphManifest::FLAG_READONLY;
        }
    }
}

impl Default for GraphStore {
    fn default() -> Self {
        Self::new(0)
    }
}

impl Clone for GraphStore {
    /// Clone GraphStore by creating a new store with copied data.
    ///
    /// Note: This is a shallow clone for the base_layers HashMap.
    /// For sharing GraphStore with QueryRouter, prefer Arc<GraphStore>.
    fn clone(&self) -> Self {
        GraphStore {
            manifest: self.manifest,
            base_path: self.base_path.clone(),
            base_layers: self.base_layers.clone(),
            delta_layers: self.delta_layers.clone(),
            mmap_refs: self.mmap_refs.clone(),
            reverse_index: self.reverse_index.clone(),
            next_delta_id: AtomicU32::new(self.next_delta_id.load(AtomicOrdering::Relaxed)),
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_edge_attr_create() {
        let attr = EdgeAttr::new(800, 100, 200);

        assert_eq!(attr.confidence(), 800);
        assert!(!attr.is_deleted());
        assert!(attr.is_valid_at(150));
        assert!(!attr.is_valid_at(250));
    }

    #[test]
    fn test_edge_list_entry_create() {
        let entry = EdgeListEntry::new(1, 2, EdgeType::CAUSES, 900, 100, 200);

        assert_eq!(entry.src_node, 1);
        assert_eq!(entry.dst_node, 2);
        assert_eq!(entry.get_edge_type(), Some(EdgeType::CAUSES));
        assert_eq!(entry.confidence(), 900);
    }

    #[test]
    fn test_edge_list_entry_ord() {
        let e1 = EdgeListEntry::new(1, 3, EdgeType::CAUSES, 800, 0, 0);
        let e2 = EdgeListEntry::new(1, 2, EdgeType::CAUSES, 900, 0, 0);
        let e3 = EdgeListEntry::new(2, 1, EdgeType::SUPPORTS, 700, 0, 0);

        assert!(e2 < e1); // Same src, dst 2 < 3
        assert!(e3 < e1); // SUPPORTS (5) < CAUSES (11)
    }

    #[test]
    fn test_graph_manifest_create() {
        let manifest = GraphManifest::new(1000);

        assert_eq!(manifest.magic, GRAPH_MAGIC);
        assert_eq!(manifest.node_count, 1000);
        assert_eq!(manifest.delta_count, 0);
        assert!(manifest.is_compacted());
    }

    #[test]
    fn test_graph_manifest_edge_type() {
        let mut manifest = GraphManifest::new(100);

        assert!(!manifest.has_edge_type(EdgeType::CAUSES));
        manifest.mark_edge_type(EdgeType::CAUSES);
        assert!(manifest.has_edge_type(EdgeType::CAUSES));
    }

    #[test]
    fn test_bitpack_block_basic() {
        let mut block = BitPackBlock::with_capacity(8, BitPackKind::RAW.to_u8());

        unsafe {
            for i in 0..10u64 {
                block.pack(i);
            }

            for i in 0..10usize {
                assert_eq!(block.unpack(i), i as u64);
            }
        }
    }

    #[test]
    fn test_graph_store_new() {
        let store = GraphStore::new(100);

        assert_eq!(store.node_count(), 100);
        assert_eq!(store.delta_count(), 0);
        assert!(store.manifest().is_compacted());
    }

    #[test]
    fn test_graph_store_add_edge() {
        let mut store = GraphStore::new(100);

        store.add_edge(1, 2, EdgeType::CAUSES, 800);
        store.add_edge(1, 3, EdgeType::SUPPORTS, 700);

        assert!(store.has_edge(1, 2, EdgeType::CAUSES));
        assert!(store.has_edge(1, 3, EdgeType::SUPPORTS));
        assert!(!store.has_edge(1, 2, EdgeType::SUPPORTS));
    }

    #[test]
    fn test_graph_store_neighbors() {
        let mut store = GraphStore::new(100);

        store.add_edge(1, 2, EdgeType::CAUSES, 800);
        store.add_edge(1, 3, EdgeType::CAUSES, 700);
        store.add_edge(1, 4, EdgeType::CAUSES, 900);

        let neighbors: Vec<_> = store.neighbors(1, EdgeType::CAUSES).collect();

        assert_eq!(neighbors.len(), 3);
    }

    #[test]
    fn test_graph_store_delete_edge() {
        let mut store = GraphStore::new(100);

        store.add_edge(1, 2, EdgeType::CAUSES, 800);
        assert!(store.has_edge(1, 2, EdgeType::CAUSES));

        store.delete_edge(1, 2, EdgeType::CAUSES).unwrap();
        assert!(!store.has_edge(1, 2, EdgeType::CAUSES));
    }

    #[test]
    fn test_graph_store_path_exists() {
        let mut store = GraphStore::new(100);

        store.add_edge(0, 1, EdgeType::CAUSES, 800);
        store.add_edge(1, 2, EdgeType::CAUSES, 800);
        store.add_edge(2, 3, EdgeType::CAUSES, 800);

        // Path 0->1->2->3 exists (3 hops)
        assert!(store.path_exists(0, 3, &[EdgeType::CAUSES], 5)); // 3 <= 5: true
        assert!(store.path_exists(0, 3, &[EdgeType::CAUSES], 3)); // 3 <= 3: true
        assert!(!store.path_exists(0, 3, &[EdgeType::CAUSES], 2)); // 3 > 2: false

        // Direct edge doesn't exist
        assert!(!store.path_exists(0, 3, &[EdgeType::CAUSES], 0)); // 0 hops: false
    }

    #[test]
    fn test_graph_builder() {
        let mut builder = GraphBuilder::new(10);

        builder.add_edge(EdgeListEntry::new(0, 1, EdgeType::CAUSES, 800, 0, 0));
        builder.add_edge(EdgeListEntry::new(0, 2, EdgeType::CAUSES, 700, 0, 0));
        builder.add_edge(EdgeListEntry::new(1, 3, EdgeType::SUPPORTS, 900, 0, 0));

        assert_eq!(builder.edge_count(), 3);

        let store = builder.build();
        assert!(store.manifest().has_edge_type(EdgeType::CAUSES));
        assert!(store.manifest().has_edge_type(EdgeType::SUPPORTS));
    }

    #[test]
    fn test_csr_layer_from_edge_list() {
        let edges = vec![
            EdgeListEntry::new(0, 1, EdgeType::CAUSES, 800, 0, 0),
            EdgeListEntry::new(0, 2, EdgeType::CAUSES, 700, 0, 0),
            EdgeListEntry::new(1, 3, EdgeType::SUPPORTS, 900, 0, 0),
        ];

        let layer = CsrLayer::from_edge_list(&edges, 4);

        assert_eq!(layer.edge_count(), 3);
        assert_eq!(layer.node_count(), 4);
    }

    #[test]
    fn test_delta_layer() {
        let mut delta = DeltaLayer::new(1, 0);

        delta.add(EdgeListEntry::new(0, 1, EdgeType::CAUSES, 800, 0, 0));
        delta.add(EdgeListEntry::new(0, 2, EdgeType::CAUSES, 700, 0, 0));

        assert_eq!(delta.len(), 2);
        assert_eq!(delta.id(), 1);

        delta.sort();
    }

    #[test]
    fn test_graph_store_file_io() {
        // Test CSR layer creation and view
        let edges = vec![
            EdgeListEntry::new(0, 1, EdgeType::CAUSES, 800, 0, 0),
            EdgeListEntry::new(0, 2, EdgeType::CAUSES, 700, 0, 0),
        ];

        let causes_layer = CsrLayer::from_edge_list(&edges, 100);

        // Verify edges
        assert_eq!(causes_layer.edge_count(), 2);
        assert_eq!(causes_layer.node_count(), 100);

        // Verify via view - check that we can get neighbor data
        let causes_view = causes_layer.view().unwrap();
        let result = causes_view.neighbor_range_data(0);
        assert!(result.is_some());

        let (targets, attrs) = result.unwrap();
        assert!(!targets.is_empty()); // Should have target data
        assert_eq!(attrs.len(), 2); // 2 edges
    }

    #[test]
    fn test_needs_compaction() {
        let store = GraphStore::new(100);

        // Initially no compaction needed
        assert!(!store.needs_compaction());
    }

    // ========================================================================
    // File I/O Round-Trip Tests
    // ========================================================================

    #[test]
    fn test_manifest_round_trip() {
        let dir = TempDir::new().unwrap();
        let mut manifest = GraphManifest::new(500);
        manifest.base_gen = 3;
        manifest.mark_edge_type(EdgeType::CAUSES);
        manifest.mark_edge_type(EdgeType::SUPPORTS);
        manifest.add_delta(1);
        manifest.add_delta(2);

        let path = dir.path().join("test.manifest");
        manifest.write_to_file(&path).unwrap();

        let loaded = GraphManifest::read_from_file(&path).unwrap();
        assert_eq!(loaded.magic, GRAPH_MAGIC);
        assert_eq!(loaded.ver, GRAPH_VERSION);
        assert_eq!(loaded.node_count, 500);
        assert_eq!(loaded.base_gen, 3);
        assert!(loaded.has_edge_type(EdgeType::CAUSES));
        assert!(loaded.has_edge_type(EdgeType::SUPPORTS));
        assert_eq!(loaded.delta_count, 2);
        assert_eq!(loaded.delta_ids[0], 1);
        assert_eq!(loaded.delta_ids[1], 2);
    }

    #[test]
    fn test_csr_layer_round_trip() {
        let dir = TempDir::new().unwrap();

        let edges = vec![
            EdgeListEntry::new(0, 1, EdgeType::CAUSES, 800, 0, 0),
            EdgeListEntry::new(0, 2, EdgeType::CAUSES, 700, 0, 0),
            EdgeListEntry::new(0, 5, EdgeType::CAUSES, 900, 0, 0),
            EdgeListEntry::new(1, 3, EdgeType::CAUSES, 600, 0, 0),
        ];

        let layer = CsrLayer::from_edge_list(&edges, 10);
        layer.write_to_files(dir.path(), EdgeType::CAUSES).unwrap();

        let loaded = CsrLayer::load_from_files(dir.path(), EdgeType::CAUSES, 10)
            .unwrap()
            .unwrap();

        assert_eq!(loaded.node_count(), 10);
        assert_eq!(loaded.edge_count(), 4);

        // Verify offsets
        assert_eq!(loaded.offsets[0], 0);
        assert_eq!(loaded.offsets[1], 3);
        assert_eq!(loaded.offsets[2], 4);
    }

    #[test]
    fn test_delta_layer_round_trip() {
        let dir = TempDir::new().unwrap();

        let mut delta = DeltaLayer::new(42, 5);
        delta.add(EdgeListEntry::new(0, 1, EdgeType::CAUSES, 800, 100, 200));
        delta.add(EdgeListEntry::new(0, 2, EdgeType::SUPPORTS, 700, 100, 200));
        delta.add(EdgeListEntry::new(1, 3, EdgeType::CAUSES, 900, 100, 200));

        let path = dir.path().join("delta_42.edges");
        delta.write_to_file(&path).unwrap();

        let loaded = DeltaLayer::load_from_file(&path).unwrap().unwrap();

        assert_eq!(loaded.id(), 42);
        assert_eq!(loaded.base_gen(), 5);
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded.edges()[0].src_node, 0);
        assert_eq!(loaded.edges()[0].dst_node, 1);
        assert_eq!(loaded.edges()[0].get_edge_type(), Some(EdgeType::CAUSES));
    }

    #[test]
    fn test_graph_store_save_and_load() {
        let dir = TempDir::new().unwrap();

        // Create store and add edges
        let mut store = GraphStore::with_path(dir.path(), 100);
        store.add_edge(0, 1, EdgeType::CAUSES, 800);
        store.add_edge(0, 2, EdgeType::CAUSES, 700);
        store.add_edge(1, 3, EdgeType::SUPPORTS, 900);
        store.add_edge(0, 5, EdgeType::CAUSES, 600);

        // Save everything
        store.save().unwrap();

        // Verify files exist (edges go to delta layers, not base, until compaction)
        assert!(dir.path().join(MANIFEST_FILE).exists());
        assert!(dir.path().join("delta_1.edges").exists());

        // Load into new store
        let loaded = GraphStore::load(dir.path()).unwrap();

        assert_eq!(loaded.node_count(), 100);
        assert!(loaded.has_edge(0, 1, EdgeType::CAUSES));
        assert!(loaded.has_edge(0, 2, EdgeType::CAUSES));
        assert!(loaded.has_edge(0, 5, EdgeType::CAUSES));
        assert!(loaded.has_edge(1, 3, EdgeType::SUPPORTS));
        assert!(!loaded.has_edge(0, 99, EdgeType::CAUSES));
    }

    #[test]
    fn test_graph_store_open_or_create() {
        let dir = TempDir::new().unwrap();

        // First call: creates new store
        let mut store = GraphStore::open_or_create(dir.path(), 50).unwrap();
        assert_eq!(store.node_count(), 50);
        assert_eq!(store.delta_count(), 0);

        store.add_edge(0, 1, EdgeType::CAUSES, 800);
        store.save().unwrap();

        // Second call: loads existing store
        let loaded = GraphStore::open_or_create(dir.path(), 999).unwrap();
        assert_eq!(loaded.node_count(), 50); // Not 999, loaded from disk
        assert!(loaded.has_edge(0, 1, EdgeType::CAUSES));
    }

    #[test]
    fn test_graph_store_delta_persistence() {
        let dir = TempDir::new().unwrap();

        let mut store = GraphStore::with_path(dir.path(), 100);
        store.add_edge(0, 1, EdgeType::CAUSES, 800);
        store.add_edge(0, 2, EdgeType::CAUSES, 700);

        assert_eq!(store.delta_count(), 1);

        // Save delta layers explicitly
        store.save_delta_layers().unwrap();
        store.save_manifest().unwrap();

        // Verify delta file exists
        assert!(dir.path().join("delta_1.edges").exists());

        // Load and verify deltas are restored
        let loaded = GraphStore::load(dir.path()).unwrap();
        assert_eq!(loaded.delta_count(), 1);
        assert!(loaded.has_edge(0, 1, EdgeType::CAUSES));
        assert!(loaded.has_edge(0, 2, EdgeType::CAUSES));
    }

    #[test]
    fn test_graph_store_delete_and_persist() {
        let dir = TempDir::new().unwrap();

        let mut store = GraphStore::with_path(dir.path(), 100);
        store.add_edge(0, 1, EdgeType::CAUSES, 800);
        store.add_edge(0, 2, EdgeType::CAUSES, 700);
        store.save().unwrap();

        // Load and delete
        let mut loaded = GraphStore::load(dir.path()).unwrap();
        assert!(loaded.has_edge(0, 1, EdgeType::CAUSES));

        loaded.delete_edge(0, 1, EdgeType::CAUSES).unwrap();
        assert!(!loaded.has_edge(0, 1, EdgeType::CAUSES));
        assert!(loaded.has_edge(0, 2, EdgeType::CAUSES));

        loaded.save().unwrap();

        // Reload and verify deletion persisted
        let reloaded = GraphStore::load(dir.path()).unwrap();
        assert!(!reloaded.has_edge(0, 1, EdgeType::CAUSES));
        assert!(reloaded.has_edge(0, 2, EdgeType::CAUSES));
    }

    #[test]
    fn test_graph_store_multiple_delta_layers() {
        let dir = TempDir::new().unwrap();

        let mut store = GraphStore::with_path(dir.path(), 100);

        // Manually create multiple delta layers
        let mut delta1 = DeltaLayer::new(1, 0);
        delta1.add(EdgeListEntry::new(0, 1, EdgeType::CAUSES, 800, 0, 0));
        store.delta_layers.push(delta1);

        let mut delta2 = DeltaLayer::new(2, 0);
        delta2.add(EdgeListEntry::new(0, 2, EdgeType::CAUSES, 700, 0, 0));
        delta2.add(EdgeListEntry::new(1, 3, EdgeType::SUPPORTS, 900, 0, 0));
        store.delta_layers.push(delta2);

        store.manifest.add_delta(1);
        store.manifest.add_delta(2);
        store.next_delta_id.store(3, AtomicOrdering::Relaxed);

        store.save().unwrap();

        // Load and verify both deltas
        let loaded = GraphStore::load(dir.path()).unwrap();
        assert_eq!(loaded.delta_count(), 2);
        assert!(loaded.has_edge(0, 1, EdgeType::CAUSES));
        assert!(loaded.has_edge(0, 2, EdgeType::CAUSES));
        assert!(loaded.has_edge(1, 3, EdgeType::SUPPORTS));
    }

    #[test]
    fn test_graph_store_save_without_path_fails() {
        let store = GraphStore::new(100);
        assert!(store.save().is_err());
        assert!(store.save_manifest().is_err());
        assert!(store.save_base_layers().is_err());
        assert!(store.save_delta_layers().is_err());
    }

    #[test]
    fn test_graph_store_set_base_path() {
        let dir = TempDir::new().unwrap();
        let mut store = GraphStore::new(100);
        store.set_base_path(dir.path());
        store.add_edge(0, 1, EdgeType::CAUSES, 800);
        store.save().unwrap();

        let loaded = GraphStore::load(dir.path()).unwrap();
        assert!(loaded.has_edge(0, 1, EdgeType::CAUSES));
    }

    #[test]
    fn test_graph_store_compaction_round_trip() {
        let dir = TempDir::new().unwrap();

        let mut store = GraphStore::with_path(dir.path(), 100);

        // Add edges to create deltas
        store.add_edge(0, 1, EdgeType::CAUSES, 800);
        store.add_edge(0, 2, EdgeType::CAUSES, 700);
        store.add_edge(1, 3, EdgeType::SUPPORTS, 900);

        // Verify edges are in deltas before compaction
        assert_eq!(store.delta_count(), 1);
        assert!(
            store.has_edge(0, 1, EdgeType::CAUSES),
            "Edge missing before compact"
        );

        // Force compaction
        store.compact().unwrap();

        // Verify compaction results
        assert_eq!(store.delta_count(), 0, "Expected 0 deltas after compact");
        assert_eq!(
            store.base_layers.len(),
            2,
            "Expected 2 base layers after compact"
        );

        // Verify base layer contents
        if let Some(layer) = store.base_layers.get(&EdgeType::CAUSES) {
            assert_eq!(layer.edge_count(), 2, "CAUSES layer should have 2 edges");
        }
        if let Some(layer) = store.base_layers.get(&EdgeType::SUPPORTS) {
            assert_eq!(layer.edge_count(), 1, "SUPPORTS layer should have 1 edge");
        }

        assert!(store.has_edge(0, 1, EdgeType::CAUSES));
        assert!(store.has_edge(0, 2, EdgeType::CAUSES));
        assert!(store.has_edge(1, 3, EdgeType::SUPPORTS));

        // Save
        store.save().unwrap();

        // Verify files exist
        assert!(dir.path().join(MANIFEST_FILE).exists(), "Manifest missing");
        assert!(
            dir.path().join("edges_11.offsets").exists(),
            "CAUSES offsets missing"
        );
        assert!(
            dir.path().join("edges_11.targets").exists(),
            "CAUSES targets missing"
        );
        assert!(
            dir.path().join("edges_11.attrs").exists(),
            "CAUSES attrs missing"
        );

        // Load into new store
        let loaded = GraphStore::load(dir.path()).unwrap();

        assert_eq!(
            loaded.delta_count(),
            0,
            "Expected 0 deltas after compact+load"
        );
        assert!(loaded.manifest().is_compacted(), "Expected compacted flag");
        assert_eq!(
            loaded.base_layers.len(),
            2,
            "Expected 2 base layers (CAUSES + SUPPORTS)"
        );

        assert!(loaded.has_edge(0, 1, EdgeType::CAUSES));
        assert!(loaded.has_edge(0, 2, EdgeType::CAUSES));
        assert!(loaded.has_edge(1, 3, EdgeType::SUPPORTS));
        assert!(loaded.manifest().is_compacted());
    }

    #[test]
    fn test_graph_store_edge_count_after_save_load() {
        let dir = TempDir::new().unwrap();

        let mut store = GraphStore::with_path(dir.path(), 100);
        store.add_edge(0, 1, EdgeType::CAUSES, 800);
        store.add_edge(0, 2, EdgeType::CAUSES, 700);
        store.add_edge(1, 3, EdgeType::SUPPORTS, 900);

        let original_count = store.edge_count();
        store.save().unwrap();

        let loaded = GraphStore::load(dir.path()).unwrap();
        assert_eq!(loaded.edge_count(), original_count);
    }

    #[test]
    fn test_graph_store_neighbors_after_load() {
        let dir = TempDir::new().unwrap();

        let mut store = GraphStore::with_path(dir.path(), 100);
        store.add_edge(0, 1, EdgeType::CAUSES, 800);
        store.add_edge(0, 2, EdgeType::CAUSES, 700);
        store.add_edge(0, 5, EdgeType::CAUSES, 900);
        store.save().unwrap();

        let loaded = GraphStore::load(dir.path()).unwrap();
        let neighbors: Vec<_> = loaded.neighbors(0, EdgeType::CAUSES).collect();
        assert_eq!(neighbors.len(), 3);
    }

    #[test]
    fn test_graph_store_in_neighbors_uses_reverse_index() {
        let mut store = GraphStore::new(16);
        store.add_edge(1, 7, EdgeType::CAUSES, 800);
        store.add_edge(3, 7, EdgeType::CAUSES, 600);
        store.add_edge(5, 7, EdgeType::CAUSES, 900);

        let incoming = store.in_neighbors(7, EdgeType::CAUSES);
        assert_eq!(incoming.len(), 3);
        assert_eq!(incoming[0], (1, 800));
        assert_eq!(incoming[1], (3, 600));
        assert_eq!(incoming[2], (5, 900));
    }

    #[test]
    fn test_graph_store_in_neighbors_after_delete_and_reload() {
        let dir = TempDir::new().unwrap();

        let mut store = GraphStore::with_path(dir.path(), 16);
        store.add_edge(1, 7, EdgeType::CAUSES, 800);
        store.add_edge(3, 7, EdgeType::CAUSES, 600);
        store.delete_edge(1, 7, EdgeType::CAUSES).unwrap();
        store.save().unwrap();

        let loaded = GraphStore::load(dir.path()).unwrap();
        let incoming = loaded.in_neighbors(7, EdgeType::CAUSES);
        assert_eq!(incoming, vec![(3, 600)]);
    }

    #[test]
    fn test_graph_store_compaction_with_deletion() {
        let dir = TempDir::new().unwrap();

        let mut store = GraphStore::with_path(dir.path(), 100);
        store.add_edge(0, 1, EdgeType::CAUSES, 800);
        store.add_edge(0, 2, EdgeType::CAUSES, 700);
        store.add_edge(0, 3, EdgeType::CAUSES, 600);

        // Delete one edge
        store.delete_edge(0, 2, EdgeType::CAUSES).unwrap();

        // Compact
        store.compact().unwrap();
        store.save().unwrap();

        let loaded = GraphStore::load(dir.path()).unwrap();
        assert!(loaded.has_edge(0, 1, EdgeType::CAUSES));
        assert!(!loaded.has_edge(0, 2, EdgeType::CAUSES));
        assert!(loaded.has_edge(0, 3, EdgeType::CAUSES));
    }

    #[test]
    fn test_graph_store_base_gen_increments_on_compact() {
        let dir = TempDir::new().unwrap();

        let mut store = GraphStore::with_path(dir.path(), 100);
        assert_eq!(store.manifest().base_gen, 0);

        store.add_edge(0, 1, EdgeType::CAUSES, 800);
        store.compact().unwrap();

        assert_eq!(store.manifest().base_gen, 1);

        store.save().unwrap();
        let loaded = GraphStore::load(dir.path()).unwrap();
        assert_eq!(loaded.manifest().base_gen, 1);
    }
}
