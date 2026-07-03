//! EDGES section implementation for MemoryX SKF-1.1
//!
//! Bit-packed, columnar edge storage per SKF-1.1 spec.
//!
//! Layout on disk:
//!   EdgesHeader (32 bytes)
//!   EdgeTypeDesc[] (16 bytes each)
//!   bit-packed target columns (DELTA mode when sorted by RefId)
//!   confidence column (u16 per target, when present)
//!   blob arena (optional attributes)
//!
//! Within each edge_type, targets are sorted by RefId so that
//! delta-bitpacking achieves maximum compression.

use super::CasError;
use crate::store::{AtomId, EdgeType, InvalidEdgeType};
use crate::utils::{
    BITPACK_BLOCK_SIZE, BitPackBlockHeader, bitpack_decode, bitpack_decode_deltas,
    bitpack_encode_deltas, crc32,
};
use std::fmt;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};

/// Magic for EDGES section: "EDG1" = 0x45444731
pub const EDGES_MAGIC: u32 = 0x45444731;

/// Header size for EDGES section
pub const EDGES_HEADER_SIZE: usize = 48;

/// Bit-packed block alignment (16 bytes per SKF-1.1)
const BLOCK_ALIGN: usize = 16;

/// Header for the EDGES section (48 bytes)
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgesHeader {
    /// Magic number: 0x45444731
    pub magic: u32,
    /// Number of distinct edge types
    pub edge_type_count: u32,
    /// Total number of target entries across all types
    pub total_targets: u32,
    /// Layout flags bitmask
    pub layout_flags: u32,
    /// Offset to EdgeTypeDesc array
    pub off_edge_types: u64,
    /// Offset to bit-packed targets column
    pub off_targets: u64,
    /// Offset to confidence column (0 if absent)
    pub off_confidence: u64,
    /// Offset to blob arena (0 if absent)
    pub off_attrs: u64,
}

impl EdgesHeader {
    pub const SIZE: usize = EDGES_HEADER_SIZE;
    const LAYOUT_HAS_CONFIDENCE: u32 = 0x0001;
    const LAYOUT_HAS_ATTRS: u32 = 0x0002;

    pub fn new(
        edge_type_count: u32,
        total_targets: u32,
        has_confidence: bool,
        has_attrs: bool,
    ) -> Self {
        let mut layout_flags = 0u32;
        if has_confidence {
            layout_flags |= Self::LAYOUT_HAS_CONFIDENCE;
        }
        if has_attrs {
            layout_flags |= Self::LAYOUT_HAS_ATTRS;
        }
        let off_edge_types = EDGES_HEADER_SIZE as u64;
        let off_targets = (EDGES_HEADER_SIZE + edge_type_count as usize * 16) as u64;
        EdgesHeader {
            magic: EDGES_MAGIC,
            edge_type_count,
            total_targets,
            layout_flags,
            off_edge_types,
            off_targets,
            off_confidence: 0,
            off_attrs: 0,
        }
    }

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..4].copy_from_slice(&self.magic.to_le_bytes());
        buf[4..8].copy_from_slice(&self.edge_type_count.to_le_bytes());
        buf[8..12].copy_from_slice(&self.total_targets.to_le_bytes());
        buf[12..16].copy_from_slice(&self.layout_flags.to_le_bytes());
        buf[16..24].copy_from_slice(&self.off_edge_types.to_le_bytes());
        buf[24..32].copy_from_slice(&self.off_targets.to_le_bytes());
        buf[32..40].copy_from_slice(&self.off_confidence.to_le_bytes());
        buf[40..48].copy_from_slice(&self.off_attrs.to_le_bytes());
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CasError> {
        if bytes.len() < Self::SIZE {
            return Err(CasError::BufferTooSmall {
                expected: Self::SIZE,
                actual: bytes.len(),
            });
        }
        let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        if magic != EDGES_MAGIC {
            return Err(CasError::InvalidMagic {
                expected: EDGES_MAGIC,
                found: magic,
            });
        }
        let edge_type_count = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        let total_targets = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let layout_flags = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
        let off_edge_types = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
        let off_targets = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
        let off_confidence = u64::from_le_bytes(bytes[32..40].try_into().unwrap());
        let off_attrs = u64::from_le_bytes(bytes[40..48].try_into().unwrap());
        Ok(EdgesHeader {
            magic,
            edge_type_count,
            total_targets,
            layout_flags,
            off_edge_types,
            off_targets,
            off_confidence,
            off_attrs,
        })
    }

    #[inline]
    pub fn has_confidence(&self) -> bool {
        self.layout_flags & Self::LAYOUT_HAS_CONFIDENCE != 0
    }

    #[inline]
    pub fn has_attrs(&self) -> bool {
        self.layout_flags & Self::LAYOUT_HAS_ATTRS != 0
    }
}

const _: () = assert!(
    size_of::<EdgesHeader>() == 48,
    "EdgesHeader must be 48 bytes"
);

/// Description of a single edge type (16 bytes)
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeTypeDesc {
    pub edge_type: u32,
    pub count: u32,
    pub first_idx: u32,
    pub flags: u32,
}

impl EdgeTypeDesc {
    pub const SIZE: usize = 16;
    pub const FLAG_DIRECTED: u32 = 0x0001;
    pub const FLAG_BIDIRECTIONAL: u32 = 0x0002;
    pub const FLAG_HAS_CONFIDENCE: u32 = 0x0004;

    pub fn new(edge_type: u32, count: u32, first_idx: u32, is_directed: bool) -> Self {
        let flags = if is_directed {
            Self::FLAG_DIRECTED
        } else {
            Self::FLAG_BIDIRECTIONAL
        };
        EdgeTypeDesc {
            edge_type,
            count,
            first_idx,
            flags,
        }
    }

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..4].copy_from_slice(&self.edge_type.to_le_bytes());
        buf[4..8].copy_from_slice(&self.count.to_le_bytes());
        buf[8..12].copy_from_slice(&self.first_idx.to_le_bytes());
        buf[12..16].copy_from_slice(&self.flags.to_le_bytes());
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CasError> {
        if bytes.len() < Self::SIZE {
            return Err(CasError::BufferTooSmall {
                expected: Self::SIZE,
                actual: bytes.len(),
            });
        }
        Ok(EdgeTypeDesc {
            edge_type: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            count: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            first_idx: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            flags: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
        })
    }

    #[inline]
    pub fn is_directed(&self) -> bool {
        self.flags & Self::FLAG_DIRECTED != 0
    }

    #[inline]
    pub fn has_confidence_flag(&self) -> bool {
        self.flags & Self::FLAG_HAS_CONFIDENCE != 0
    }

    pub fn get_edge_type(&self) -> Result<EdgeType, InvalidEdgeType> {
        EdgeType::from_u32(self.edge_type).ok_or(InvalidEdgeType(self.edge_type))
    }
}

const _: () = assert!(
    size_of::<EdgeTypeDesc>() == 16,
    "EdgeTypeDesc must be 16 bytes"
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeTarget {
    pub refid: u32,
    pub confidence_q: Option<u16>,
    pub edge_attr_off: Option<u16>,
}

/// EDGES section -- columnar, bit-packed edge storage
#[derive(Debug, Clone, Default)]
pub struct EdgesSection {
    edge_types: Vec<EdgeTypeDesc>,
    target_values: Vec<u64>,
    confidence_values: Vec<u16>,
    blob_arena: Vec<u8>,
}

impl EdgesSection {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn start_type(&mut self, edge_type: EdgeType, is_directed: bool) -> (usize, u32) {
        let first_idx = self.target_values.len() as u32;
        let type_idx = self.edge_types.len();
        self.edge_types.push(EdgeTypeDesc::new(
            edge_type.to_u32(),
            0,
            first_idx,
            is_directed,
        ));
        (type_idx, first_idx)
    }

    pub fn add_edge(&mut self, edge_type: u32, refid: u32) {
        self.ensure_type_entry(edge_type);
        self.target_values.push(refid as u64);
        self.confidence_values.push(0);
        let last = self.edge_types.last_mut().unwrap();
        last.count += 1;
    }

    pub fn add_edge_with_conf(&mut self, edge_type: u32, refid: u32, conf_q: u16) {
        self.ensure_type_entry(edge_type);
        self.target_values.push(refid as u64);
        self.confidence_values.push(conf_q);
        let last = self.edge_types.last_mut().unwrap();
        last.count += 1;
        last.flags |= EdgeTypeDesc::FLAG_HAS_CONFIDENCE;
    }

    fn ensure_type_entry(&mut self, edge_type: u32) {
        if self.edge_types.is_empty() || self.edge_types.last().unwrap().edge_type != edge_type {
            let first_idx = self.target_values.len() as u32;
            self.edge_types
                .push(EdgeTypeDesc::new(edge_type, 0, first_idx, true));
        }
    }

    /// Sort targets by RefId within each edge_type.
    /// Must be called before serialization for effective DELTA bitpacking.
    pub fn sort_targets(&mut self) {
        for desc in &self.edge_types {
            let start = desc.first_idx as usize;
            let count = desc.count as usize;
            if count <= 1 {
                continue;
            }
            let end = start + count;
            let mut paired: Vec<(u64, u16)> = (start..end)
                .map(|i| (self.target_values[i], self.confidence_values[i]))
                .collect();
            paired.sort_by_key(|&(rid, _)| rid);
            for (i, &(rid, conf)) in paired.iter().enumerate() {
                self.target_values[start + i] = rid;
                self.confidence_values[start + i] = conf;
            }
        }
    }

    pub fn get_targets(&self, edge_type: u32) -> Option<Vec<EdgeTarget>> {
        let desc = self.edge_types.iter().find(|d| d.edge_type == edge_type)?;
        let start = desc.first_idx as usize;
        let count = desc.count as usize;
        let has_conf = desc.has_confidence_flag();
        Some(
            (0..count)
                .map(|i| EdgeTarget {
                    refid: self.target_values[start + i] as u32,
                    confidence_q: if has_conf {
                        Some(self.confidence_values[start + i])
                    } else {
                        None
                    },
                    edge_attr_off: None,
                })
                .collect(),
        )
    }

    pub fn find_by_target(&self, refid: u32) -> Vec<(u32, usize)> {
        let mut result = Vec::new();
        for desc in &self.edge_types {
            let start = desc.first_idx as usize;
            let count = desc.count as usize;
            let slice = &self.target_values[start..start + count];
            if let Ok(pos) = slice.binary_search(&(refid as u64)) {
                result.push((desc.edge_type, pos));
            }
        }
        result
    }

    pub fn total_targets(&self) -> usize {
        self.target_values.len()
    }
    pub fn type_count(&self) -> usize {
        self.edge_types.len()
    }
    pub fn is_empty(&self) -> bool {
        self.edge_types.is_empty() || self.target_values.is_empty()
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        if self.edge_types.is_empty() {
            return EdgesHeader::new(0, 0, false, false).to_bytes().to_vec();
        }
        let total_targets = self.target_values.len() as u32;
        let has_confidence = self
            .edge_types
            .iter()
            .any(|d| d.flags & EdgeTypeDesc::FLAG_HAS_CONFIDENCE != 0);
        let off_edge_types = EDGES_HEADER_SIZE as u64;
        let off_types_end =
            off_edge_types + (self.edge_types.len() as u64 * EdgeTypeDesc::SIZE as u64);
        let off_targets = align_up(off_types_end, BLOCK_ALIGN);
        let packed_targets = self.encode_target_blocks();
        let targets_size = packed_targets.len();
        let off_confidence = if has_confidence {
            align_up(off_targets + targets_size as u64, 2)
        } else {
            0
        };
        let confidence_size = if has_confidence {
            self.confidence_values.len() * 2
        } else {
            0
        };
        let off_attrs = if !self.blob_arena.is_empty() {
            align_up(off_confidence + confidence_size as u64, BLOCK_ALIGN)
        } else {
            0
        };
        let has_attrs = !self.blob_arena.is_empty();
        let layout_flags = if has_confidence { 0x01 } else { 0 } | if has_attrs { 0x02 } else { 0 };
        let header = EdgesHeader {
            magic: EDGES_MAGIC,
            edge_type_count: self.edge_types.len() as u32,
            total_targets,
            layout_flags,
            off_edge_types,
            off_targets,
            off_confidence,
            off_attrs,
        };
        let total_cap = EDGES_HEADER_SIZE
            + self.edge_types.len() * EdgeTypeDesc::SIZE
            + BLOCK_ALIGN * 4
            + targets_size
            + confidence_size
            + self.blob_arena.len();
        let mut buf = Vec::with_capacity(total_cap);
        buf.extend_from_slice(&header.to_bytes());
        pad_to(&mut buf, off_edge_types as usize, 8);
        for desc in &self.edge_types {
            buf.extend_from_slice(&desc.to_bytes());
        }
        pad_to(&mut buf, off_targets as usize, BLOCK_ALIGN);
        buf.extend_from_slice(&packed_targets);
        if has_confidence {
            pad_to(&mut buf, off_confidence as usize, 2);
            for &conf in &self.confidence_values {
                buf.extend_from_slice(&conf.to_le_bytes());
            }
        }
        if has_attrs {
            pad_to(&mut buf, off_attrs as usize, BLOCK_ALIGN);
            buf.extend_from_slice(&self.blob_arena);
        }
        buf
    }

    fn encode_target_blocks(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.target_values.len() * 8 + 1024);
        for desc in &self.edge_types {
            let start = desc.first_idx as usize;
            let count = desc.count as usize;
            if count == 0 {
                continue;
            }
            let values = &self.target_values[start..start + count];
            let mut offset = 0;
            while offset < count {
                let remaining = count - offset;
                let this_block = if remaining >= BITPACK_BLOCK_SIZE {
                    BITPACK_BLOCK_SIZE
                } else {
                    remaining
                };
                let block_values = &values[offset..offset + this_block];
                let max_needed =
                    BitPackBlockHeader::data_size(64, this_block as u8) + BitPackBlockHeader::SIZE;
                let mut block_buf = vec![0u8; max_needed];
                if let Some(written) = bitpack_encode_deltas(block_values, &mut block_buf) {
                    buf.extend_from_slice(&block_buf[..written]);
                }
                offset += this_block;
            }
        }
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CasError> {
        let header = EdgesHeader::from_bytes(bytes)?;
        let mut section = EdgesSection::new();
        let off_type_end =
            header.off_edge_types as usize + header.edge_type_count as usize * EdgeTypeDesc::SIZE;
        if bytes.len() < off_type_end {
            return Err(CasError::BufferTooSmall {
                expected: off_type_end,
                actual: bytes.len(),
            });
        }
        let mut offset = header.off_edge_types as usize;
        for _ in 0..header.edge_type_count {
            let desc = EdgeTypeDesc::from_bytes(&bytes[offset..offset + EdgeTypeDesc::SIZE])?;
            section.edge_types.push(desc);
            offset += EdgeTypeDesc::SIZE;
        }
        if header.off_targets == 0 || header.off_targets as usize >= bytes.len() {
            return Ok(section);
        }
        let targets_start = header.off_targets as usize;
        let mut cursor = targets_start;
        for desc in &section.edge_types {
            let count = desc.count as usize;
            if count == 0 {
                continue;
            }
            let type_start_idx = section.target_values.len();
            let mut remaining = count;
            while remaining > 0 && cursor < bytes.len() {
                let block_data = &bytes[cursor..];
                if block_data.len() < BitPackBlockHeader::SIZE {
                    break;
                }
                let hdr = unsafe {
                    std::ptr::read_unaligned(block_data.as_ptr() as *const BitPackBlockHeader)
                };
                if hdr.bits == 0 || hdr.count == 0 {
                    break;
                }
                let mut decode_buf = vec![0u64; hdr.count as usize];
                if let Some(n) = bitpack_decode_deltas(block_data, &mut decode_buf) {
                    section.target_values.extend_from_slice(&decode_buf[..n]);
                    cursor += BitPackBlockHeader::SIZE
                        + BitPackBlockHeader::data_size(hdr.bits, hdr.count);
                    remaining = remaining.saturating_sub(n);
                } else {
                    if let Some(n) = bitpack_decode(block_data, &mut decode_buf) {
                        section.target_values.extend_from_slice(&decode_buf[..n]);
                        cursor += BitPackBlockHeader::SIZE
                            + BitPackBlockHeader::data_size(hdr.bits, hdr.count);
                        remaining = remaining.saturating_sub(n);
                    } else {
                        break;
                    }
                }
            }
            let decoded_count = section.target_values.len() - type_start_idx;
            if desc.has_confidence_flag() && header.off_confidence != 0 {
                let conf_start = header.off_confidence as usize + type_start_idx * 2;
                if conf_start + decoded_count * 2 <= bytes.len() {
                    for i in 0..decoded_count {
                        let off = conf_start + i * 2;
                        let conf = u16::from_le_bytes([bytes[off], bytes[off + 1]]);
                        section.confidence_values.push(conf);
                    }
                }
            } else {
                for _ in 0..decoded_count {
                    section.confidence_values.push(0);
                }
            }
        }
        Ok(section)
    }

    pub fn crc32(&self) -> u32 {
        crc32(&self.to_bytes())
    }
}

impl fmt::Display for EdgesSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Edges({} types, {} targets)",
            self.edge_types.len(),
            self.target_values.len()
        )
    }
}

/// Align offset up to the given alignment
#[inline]
fn align_up(offset: u64, alignment: usize) -> u64 {
    let mask = (alignment - 1) as u64;
    (offset + mask) & !mask
}

/// Pad buffer to target offset with alignment fill
#[inline]
fn pad_to(buf: &mut Vec<u8>, target_offset: usize, alignment: usize) {
    while buf.len() < target_offset {
        buf.push(0);
    }
    let remainder = buf.len() % alignment;
    if remainder != 0 {
        for _ in 0..(alignment - remainder) {
            buf.push(0);
        }
    }
}

// ============================================================================
// Deferred Edges (SKF-1.1 Spec B.8)
// ============================================================================

/// Magic for deferred edges file: "DEFD" = 0x44454644
pub const DEFERRED_MAGIC: u32 = 0x44454644;

/// Deferred edges file version
pub const DEFERRED_VERSION: u16 = 0x0001;

/// Default retention: drop deferred edges older than this many compaction cycles
pub const DEFAULT_DEFERRED_RETENTION_CYCLES: u32 = 5;

/// A single deferred (unresolved) edge target.
///
/// When building atoms, edge targets reference other atoms by AtomId.
/// But the target atom may not exist yet (forward references).
/// These edges are stored as "deferred" and resolved later when the
/// target atom is created.
///
/// Binary layout (72 bytes, repr(C)):
///   target_atom_id: [u8; 32]  — the unresolved target AtomId
///   edge_type: u32            — type of the edge
///   source_atom_id: [u8; 32]  — the atom that owns this edge
///   created_cycle: u32        — compaction cycle when this deferred edge was created
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeferredEdge {
    /// The AtomId of the target that does not yet have a NodeNum
    pub target_atom_id: AtomId,
    /// Edge type (DEFINES, SUPPORTS, CONTRADICTS, etc.)
    pub edge_type: u32,
    /// The AtomId of the source atom that owns this edge
    pub source_atom_id: AtomId,
    /// Compaction cycle at which this deferred edge was created.
    /// Used for retention policy — edges older than N cycles are dropped.
    pub created_cycle: u32,
}

impl DeferredEdge {
    /// Size of DeferredEdge in bytes (72)
    pub const SIZE: usize = 72;

    /// Create a new DeferredEdge
    #[inline]
    pub fn new(
        target_atom_id: AtomId,
        edge_type: u32,
        source_atom_id: AtomId,
        created_cycle: u32,
    ) -> Self {
        DeferredEdge {
            target_atom_id,
            edge_type,
            source_atom_id,
            created_cycle,
        }
    }

    /// Serialize to bytes (72 bytes, little-endian)
    #[inline]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..32].copy_from_slice(&self.target_atom_id);
        buf[32..36].copy_from_slice(&self.edge_type.to_le_bytes());
        buf[36..68].copy_from_slice(&self.source_atom_id);
        buf[68..72].copy_from_slice(&self.created_cycle.to_le_bytes());
        buf
    }

    /// Deserialize from bytes (72 bytes, little-endian)
    #[inline]
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CasError> {
        if bytes.len() < Self::SIZE {
            return Err(CasError::BufferTooSmall {
                expected: Self::SIZE,
                actual: bytes.len(),
            });
        }
        let mut target_atom_id = [0u8; 32];
        target_atom_id.copy_from_slice(&bytes[0..32]);
        let edge_type = u32::from_le_bytes(bytes[32..36].try_into().unwrap());
        let mut source_atom_id = [0u8; 32];
        source_atom_id.copy_from_slice(&bytes[36..68]);
        let created_cycle = u32::from_le_bytes(bytes[68..72].try_into().unwrap());
        Ok(DeferredEdge {
            target_atom_id,
            edge_type,
            source_atom_id,
            created_cycle,
        })
    }
}

const _: () = assert!(
    size_of::<DeferredEdge>() == 72,
    "DeferredEdge must be 72 bytes"
);

/// A resolved edge — produced when a deferred edge's target atom is created.
///
/// Contains the original edge metadata plus the resolved NodeNum of the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEdge {
    /// The AtomId of the source atom
    pub source_atom_id: AtomId,
    /// The AtomId of the target atom (now resolved)
    pub target_atom_id: AtomId,
    /// The resolved NodeNum of the target atom
    pub target_node_num: u64,
    /// Edge type
    pub edge_type: u32,
}

/// Persistent store for deferred edges.
///
/// Manages the `extract_deferred.bin` file per SKF-1.1 Spec B.8.
///
/// File format:
///   [u32: magic = 0x44454644]
///   [u16: version = 0x0001]
///   [u16: reserved]
///   [u32: count]
///   [u32: current_compaction_cycle]
///   [DeferredEdge * count]  (72 bytes each)
///
/// When a new atom is created, `try_resolve` checks if its AtomId matches
/// any pending deferred edge targets. Matching edges are returned as
/// `ResolvedEdge` and removed from the store.
///
/// Retention policy: `apply_retention` drops deferred edges whose
/// `created_cycle` is older than `max_cycles` behind the current cycle.
pub struct DeferredEdgesStore {
    /// Path to the extract_deferred.bin file
    file_path: PathBuf,
    /// In-memory deferred edges
    edges: Vec<DeferredEdge>,
    /// Current compaction cycle counter
    current_cycle: u32,
    /// Dirty flag — true if edges have been modified since last save
    dirty: bool,
}

impl DeferredEdgesStore {
    /// File name for deferred edges
    pub const FILE_NAME: &'static str = "extract_deferred.bin";

    /// Create or open a DeferredEdgesStore in the given directory.
    ///
    /// If `extract_deferred.bin` exists, loads existing deferred edges.
    /// Otherwise creates an empty store.
    pub fn open(base_dir: &Path) -> Result<Self, CasError> {
        let file_path = base_dir.join(Self::FILE_NAME);

        if file_path.exists() {
            Self::load_from_file(&file_path)
        } else {
            Ok(DeferredEdgesStore {
                file_path,
                edges: Vec::new(),
                current_cycle: 0,
                dirty: false,
            })
        }
    }

    /// Create a new empty DeferredEdgesStore (does not persist until save).
    pub fn new(base_dir: &Path) -> Self {
        DeferredEdgesStore {
            file_path: base_dir.join(Self::FILE_NAME),
            edges: Vec::new(),
            current_cycle: 0,
            dirty: false,
        }
    }

    /// Load deferred edges from a specific file path.
    fn load_from_file(path: &Path) -> Result<Self, CasError> {
        let file = File::open(path).map_err(|e| CasError::Io(e.to_string()))?;
        let mut reader = BufReader::new(file);

        // Read magic
        let mut magic_buf = [0u8; 4];
        reader
            .read_exact(&mut magic_buf)
            .map_err(|e| CasError::Io(e.to_string()))?;
        let magic = u32::from_le_bytes(magic_buf);
        if magic != DEFERRED_MAGIC {
            return Err(CasError::InvalidMagic {
                expected: DEFERRED_MAGIC,
                found: magic,
            });
        }

        // Read version
        let mut ver_buf = [0u8; 2];
        reader
            .read_exact(&mut ver_buf)
            .map_err(|e| CasError::Io(e.to_string()))?;
        let version = u16::from_le_bytes(ver_buf);
        if version != DEFERRED_VERSION {
            return Err(CasError::Io(format!(
                "Unsupported deferred edges version: {}",
                version
            )));
        }

        // Skip reserved
        let mut _reserved = [0u8; 2];
        reader
            .read_exact(&mut _reserved)
            .map_err(|e| CasError::Io(e.to_string()))?;

        // Read count
        let mut count_buf = [0u8; 4];
        reader
            .read_exact(&mut count_buf)
            .map_err(|e| CasError::Io(e.to_string()))?;
        let count = u32::from_le_bytes(count_buf) as usize;

        // Read current cycle
        let mut cycle_buf = [0u8; 4];
        reader
            .read_exact(&mut cycle_buf)
            .map_err(|e| CasError::Io(e.to_string()))?;
        let current_cycle = u32::from_le_bytes(cycle_buf);

        // Read edges
        let mut edges = Vec::with_capacity(count);
        let mut edge_buf = [0u8; DeferredEdge::SIZE];
        for _ in 0..count {
            reader
                .read_exact(&mut edge_buf)
                .map_err(|e| CasError::Io(e.to_string()))?;
            let edge = DeferredEdge::from_bytes(&edge_buf)?;
            edges.push(edge);
        }

        Ok(DeferredEdgesStore {
            file_path: path.to_path_buf(),
            edges,
            current_cycle,
            dirty: false,
        })
    }

    /// Save deferred edges to disk.
    ///
    /// Writes the full file atomically: writes to a temp file first,
    /// then renames to the target path.
    pub fn save(&mut self) -> Result<(), CasError> {
        if !self.dirty && self.file_path.exists() {
            return Ok(());
        }

        // Ensure parent directory exists
        if let Some(parent) = self.file_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| CasError::Io(e.to_string()))?;
        }

        let tmp_path = self.file_path.with_extension("bin.tmp");

        {
            let file = File::create(&tmp_path).map_err(|e| CasError::Io(e.to_string()))?;
            let mut writer = BufWriter::new(file);

            // Write header
            writer
                .write_all(&DEFERRED_MAGIC.to_le_bytes())
                .map_err(|e| CasError::Io(e.to_string()))?;
            writer
                .write_all(&DEFERRED_VERSION.to_le_bytes())
                .map_err(|e| CasError::Io(e.to_string()))?;
            writer
                .write_all(&0u16.to_le_bytes()) // reserved
                .map_err(|e| CasError::Io(e.to_string()))?;
            writer
                .write_all(&(self.edges.len() as u32).to_le_bytes())
                .map_err(|e| CasError::Io(e.to_string()))?;
            writer
                .write_all(&self.current_cycle.to_le_bytes())
                .map_err(|e| CasError::Io(e.to_string()))?;

            // Write edges
            for edge in &self.edges {
                writer
                    .write_all(&edge.to_bytes())
                    .map_err(|e| CasError::Io(e.to_string()))?;
            }

            writer.flush().map_err(|e| CasError::Io(e.to_string()))?;
        }

        // Atomic rename
        std::fs::rename(&tmp_path, &self.file_path).map_err(|e| CasError::Io(e.to_string()))?;

        self.dirty = false;
        Ok(())
    }

    /// Add a deferred edge to the store.
    ///
    /// This is called when an edge target AtomId cannot be resolved to a
    /// NodeNum at the time the source atom is being built.
    #[inline]
    pub fn add_deferred(&mut self, target_atom_id: AtomId, edge_type: u32, source_atom_id: AtomId) {
        let edge = DeferredEdge::new(
            target_atom_id,
            edge_type,
            source_atom_id,
            self.current_cycle,
        );
        self.edges.push(edge);
        self.dirty = true;
    }

    /// Try to resolve deferred edges when a new atom is created.
    ///
    /// Checks if `created_atom_id` matches the `target_atom_id` of any
    /// pending deferred edges. For each match:
    /// - Produces a `ResolvedEdge` with the resolved `target_node_num`
    /// - Removes the deferred edge from the store
    ///
    /// Returns all resolved edges (may be empty if no matches).
    pub fn try_resolve(
        &mut self,
        created_atom_id: &AtomId,
        created_node_num: u64,
    ) -> Vec<ResolvedEdge> {
        if self.edges.is_empty() {
            return Vec::new();
        }

        // Find all deferred edges whose target matches the newly created atom
        let mut resolved = Vec::new();
        let mut remove_indices = Vec::new();

        for (i, edge) in self.edges.iter().enumerate() {
            if edge.target_atom_id == *created_atom_id {
                resolved.push(ResolvedEdge {
                    source_atom_id: edge.source_atom_id,
                    target_atom_id: edge.target_atom_id,
                    target_node_num: created_node_num,
                    edge_type: edge.edge_type,
                });
                remove_indices.push(i);
            }
        }

        // Remove resolved edges (reverse order to preserve indices)
        for &idx in remove_indices.iter().rev() {
            self.edges.remove(idx);
        }

        if !remove_indices.is_empty() {
            self.dirty = true;
        }

        resolved
    }

    /// Check if a specific AtomId has any pending deferred edges targeting it.
    #[inline]
    pub fn has_pending_for(&self, atom_id: &AtomId) -> bool {
        self.edges.iter().any(|e| e.target_atom_id == *atom_id)
    }

    /// Get all pending deferred edges (read-only view).
    #[inline]
    pub fn pending_edges(&self) -> &[DeferredEdge] {
        &self.edges
    }

    /// Get the count of pending deferred edges.
    #[inline]
    pub fn pending_count(&self) -> usize {
        self.edges.len()
    }

    /// Get the current compaction cycle.
    #[inline]
    pub fn current_cycle(&self) -> u32 {
        self.current_cycle
    }

    /// Advance the compaction cycle counter.
    ///
    /// Should be called at the start of each compaction run.
    #[inline]
    pub fn advance_cycle(&mut self) {
        self.current_cycle += 1;
        self.dirty = true;
    }

    /// Apply retention policy: drop deferred edges older than `max_cycles`.
    ///
    /// An edge is dropped if:
    ///   `current_cycle - edge.created_cycle > max_cycles`
    ///
    /// Returns the number of edges that were dropped.
    pub fn apply_retention(&mut self, max_cycles: u32) -> usize {
        if self.edges.is_empty() {
            return 0;
        }

        let initial_count = self.edges.len();
        self.edges.retain(|edge| {
            let age = self.current_cycle.saturating_sub(edge.created_cycle);
            age <= max_cycles
        });
        let dropped = initial_count - self.edges.len();

        if dropped > 0 {
            self.dirty = true;
        }

        dropped
    }

    /// Apply retention with the default retention cycle count.
    #[inline]
    pub fn apply_default_retention(&mut self) -> usize {
        self.apply_retention(DEFAULT_DEFERRED_RETENTION_CYCLES)
    }

    /// Clear all deferred edges (emergency reset).
    #[inline]
    pub fn clear(&mut self) {
        self.edges.clear();
        self.dirty = true;
    }

    /// Get the file path.
    #[inline]
    pub fn file_path(&self) -> &Path {
        &self.file_path
    }
}

impl fmt::Display for DeferredEdgesStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "DeferredEdgesStore(cycle={}, pending={})",
            self.current_cycle,
            self.edges.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::EdgeType;

    #[test]
    fn test_edges_header_roundtrip() {
        let h = EdgesHeader::new(3, 42, true, false);
        let r = EdgesHeader::from_bytes(&h.to_bytes()).unwrap();
        assert_eq!(r.magic, EDGES_MAGIC);
        assert_eq!(r.edge_type_count, 3);
        assert_eq!(r.total_targets, 42);
        assert!(r.has_confidence());
        assert!(!r.has_attrs());
    }

    #[test]
    fn test_edges_header_invalid_magic() {
        let mut b = [0u8; 32];
        b[0..4].copy_from_slice(&0xDEADBEEFu32.to_le_bytes());
        assert!(EdgesHeader::from_bytes(&b).is_err());
    }

    #[test]
    fn test_edges_header_size() {
        assert_eq!(size_of::<EdgesHeader>(), 48);
    }

    #[test]
    fn test_empty_section() {
        let s = EdgesSection::new();
        assert!(s.is_empty());
        assert_eq!(s.type_count(), 0);
        assert_eq!(s.total_targets(), 0);
        let b = s.to_bytes();
        assert!(b.len() >= EDGES_HEADER_SIZE);
    }

    #[test]
    fn test_edge_type_desc_roundtrip() {
        let d = EdgeTypeDesc::new(5, 10, 0, true);
        let r = EdgeTypeDesc::from_bytes(&d.to_bytes()).unwrap();
        assert_eq!(d, r);
        assert_eq!(r.edge_type, 5);
        assert_eq!(r.count, 10);
        assert_eq!(r.first_idx, 0);
        assert!(r.is_directed());
    }

    #[test]
    fn test_edge_type_desc_size() {
        assert_eq!(size_of::<EdgeTypeDesc>(), 16);
    }

    #[test]
    fn test_add_edge_and_get_targets() {
        let mut s = EdgesSection::new();
        s.start_type(EdgeType::DEFINES, true);
        s.add_edge(EdgeType::DEFINES.to_u32(), 5);
        s.add_edge(EdgeType::DEFINES.to_u32(), 3);
        s.add_edge(EdgeType::DEFINES.to_u32(), 8);
        assert_eq!(s.type_count(), 1);
        assert_eq!(s.total_targets(), 3);
        let t = s.get_targets(EdgeType::DEFINES.to_u32()).unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].refid, 5);
        assert_eq!(t[1].refid, 3);
        assert_eq!(t[2].refid, 8);
    }

    #[test]
    fn test_sort_targets() {
        let mut s = EdgesSection::new();
        s.start_type(EdgeType::DEFINES, true);
        s.add_edge(EdgeType::DEFINES.to_u32(), 50);
        s.add_edge(EdgeType::DEFINES.to_u32(), 10);
        s.add_edge(EdgeType::DEFINES.to_u32(), 30);
        s.add_edge(EdgeType::DEFINES.to_u32(), 20);
        s.sort_targets();
        let t = s.get_targets(EdgeType::DEFINES.to_u32()).unwrap();
        assert_eq!(t[0].refid, 10);
        assert_eq!(t[1].refid, 20);
        assert_eq!(t[2].refid, 30);
        assert_eq!(t[3].refid, 50);
    }

    #[test]
    fn test_add_edge_with_confidence() {
        let mut s = EdgesSection::new();
        s.start_type(EdgeType::SUPPORTS, true);
        s.add_edge_with_conf(EdgeType::SUPPORTS.to_u32(), 5, 40000);
        s.add_edge_with_conf(EdgeType::SUPPORTS.to_u32(), 10, 50000);
        s.sort_targets();
        let t = s.get_targets(EdgeType::SUPPORTS.to_u32()).unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].refid, 5);
        assert_eq!(t[0].confidence_q, Some(40000));
        assert_eq!(t[1].refid, 10);
        assert_eq!(t[1].confidence_q, Some(50000));
    }

    #[test]
    fn test_find_by_target() {
        let mut s = EdgesSection::new();
        s.start_type(EdgeType::DEFINES, true);
        s.add_edge(EdgeType::DEFINES.to_u32(), 10);
        s.add_edge(EdgeType::DEFINES.to_u32(), 20);
        s.add_edge(EdgeType::DEFINES.to_u32(), 30);
        s.start_type(EdgeType::SUPPORTS, true);
        s.add_edge(EdgeType::SUPPORTS.to_u32(), 20);
        s.add_edge(EdgeType::SUPPORTS.to_u32(), 40);
        s.sort_targets();
        let r = s.find_by_target(20);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn test_multiple_edge_types() {
        let mut s = EdgesSection::new();
        s.start_type(EdgeType::DEFINES, true);
        s.add_edge(EdgeType::DEFINES.to_u32(), 1);
        s.add_edge(EdgeType::DEFINES.to_u32(), 2);
        s.start_type(EdgeType::SUPPORTS, true);
        s.add_edge(EdgeType::SUPPORTS.to_u32(), 3);
        s.add_edge(EdgeType::SUPPORTS.to_u32(), 4);
        s.add_edge(EdgeType::SUPPORTS.to_u32(), 5);
        s.start_type(EdgeType::CONTRADICTS, false);
        s.add_edge(EdgeType::CONTRADICTS.to_u32(), 6);
        s.sort_targets();
        assert_eq!(s.type_count(), 3);
        assert_eq!(s.total_targets(), 6);
        assert_eq!(s.get_targets(EdgeType::DEFINES.to_u32()).unwrap().len(), 2);
        assert_eq!(s.get_targets(EdgeType::SUPPORTS.to_u32()).unwrap().len(), 3);
        assert_eq!(
            s.get_targets(EdgeType::CONTRADICTS.to_u32()).unwrap().len(),
            1
        );
    }

    #[test]
    fn test_full_serialization_roundtrip_single_type() {
        let mut s = EdgesSection::new();
        s.start_type(EdgeType::DEFINES, true);
        s.add_edge(EdgeType::DEFINES.to_u32(), 10);
        s.add_edge(EdgeType::DEFINES.to_u32(), 20);
        s.add_edge(EdgeType::DEFINES.to_u32(), 30);
        s.add_edge(EdgeType::DEFINES.to_u32(), 100);
        s.add_edge(EdgeType::DEFINES.to_u32(), 200);
        s.sort_targets();
        let b = s.to_bytes();
        let r = EdgesSection::from_bytes(&b).unwrap();
        assert_eq!(r.type_count(), 1);
        assert_eq!(r.total_targets(), 5);
        let t = r.get_targets(EdgeType::DEFINES.to_u32()).unwrap();
        assert_eq!(t.len(), 5);
        assert_eq!(t[0].refid, 10);
        assert_eq!(t[1].refid, 20);
        assert_eq!(t[2].refid, 30);
        assert_eq!(t[3].refid, 100);
        assert_eq!(t[4].refid, 200);
    }

    #[test]
    fn test_full_serialization_roundtrip_multiple_types() {
        let mut s = EdgesSection::new();
        s.start_type(EdgeType::DEFINES, true);
        s.add_edge(EdgeType::DEFINES.to_u32(), 1);
        s.add_edge(EdgeType::DEFINES.to_u32(), 5);
        s.add_edge(EdgeType::DEFINES.to_u32(), 10);
        s.start_type(EdgeType::CONTRADICTS, false);
        s.add_edge(EdgeType::CONTRADICTS.to_u32(), 2);
        s.add_edge(EdgeType::CONTRADICTS.to_u32(), 8);
        s.sort_targets();
        let b = s.to_bytes();
        let r = EdgesSection::from_bytes(&b).unwrap();
        assert_eq!(r.type_count(), 2);
        assert_eq!(r.total_targets(), 5);
        let d = r.get_targets(EdgeType::DEFINES.to_u32()).unwrap();
        assert_eq!(d.len(), 3);
        assert_eq!(d[0].refid, 1);
        let c = r.get_targets(EdgeType::CONTRADICTS.to_u32()).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].refid, 2);
        assert_eq!(c[1].refid, 8);
    }

    #[test]
    fn test_serialization_with_confidence() {
        let mut s = EdgesSection::new();
        s.start_type(EdgeType::SUPPORTS, true);
        s.add_edge_with_conf(EdgeType::SUPPORTS.to_u32(), 5, 10000);
        s.add_edge_with_conf(EdgeType::SUPPORTS.to_u32(), 15, 32768);
        s.add_edge_with_conf(EdgeType::SUPPORTS.to_u32(), 25, 65535);
        s.sort_targets();
        let b = s.to_bytes();
        let r = EdgesSection::from_bytes(&b).unwrap();
        let t = r.get_targets(EdgeType::SUPPORTS.to_u32()).unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].refid, 5);
        assert_eq!(t[0].confidence_q, Some(10000));
        assert_eq!(t[1].refid, 15);
        assert_eq!(t[1].confidence_q, Some(32768));
        assert_eq!(t[2].refid, 25);
        assert_eq!(t[2].confidence_q, Some(65535));
    }

    #[test]
    fn test_empty_section_roundtrip() {
        let s = EdgesSection::new();
        let r = EdgesSection::from_bytes(&s.to_bytes()).unwrap();
        assert!(r.is_empty());
        assert_eq!(r.type_count(), 0);
    }

    #[test]
    fn test_serialization_magic_verification() {
        let mut s = EdgesSection::new();
        s.start_type(EdgeType::DEFINES, true);
        s.add_edge(EdgeType::DEFINES.to_u32(), 42);
        s.sort_targets();
        let h = EdgesHeader::from_bytes(&s.to_bytes()).unwrap();
        assert_eq!(h.magic, EDGES_MAGIC);
    }

    #[test]
    fn test_large_targets_block_boundary() {
        let mut s = EdgesSection::new();
        s.start_type(EdgeType::DEFINES, true);
        for i in 0..256u32 {
            s.add_edge(EdgeType::DEFINES.to_u32(), i * 10);
        }
        s.sort_targets();
        let b = s.to_bytes();
        let r = EdgesSection::from_bytes(&b).unwrap();
        assert_eq!(r.total_targets(), 256);
        let t = r.get_targets(EdgeType::DEFINES.to_u32()).unwrap();
        assert_eq!(t.len(), 256);
        assert_eq!(t[0].refid, 0);
        assert_eq!(t[127].refid, 1270);
        assert_eq!(t[255].refid, 2550);
    }

    #[test]
    fn test_crc32_consistency() {
        let mut s = EdgesSection::new();
        s.start_type(EdgeType::DEFINES, true);
        s.add_edge(EdgeType::DEFINES.to_u32(), 1);
        s.add_edge(EdgeType::DEFINES.to_u32(), 2);
        s.sort_targets();
        let c1 = s.crc32();
        let c2 = s.crc32();
        assert_eq!(c1, c2);
    }

    // ========================================================================
    // DeferredEdge tests
    // ========================================================================

    fn make_atom_id(seed: u8) -> AtomId {
        let mut id = [0u8; 32];
        for (i, byte) in id.iter_mut().enumerate() {
            *byte = seed.wrapping_add(i as u8);
        }
        id
    }

    #[test]
    fn test_deferred_edge_size() {
        assert_eq!(size_of::<DeferredEdge>(), 72);
    }

    #[test]
    fn test_deferred_edge_roundtrip() {
        let src = make_atom_id(10);
        let tgt = make_atom_id(20);
        let edge = DeferredEdge::new(tgt, EdgeType::DEFINES.to_u32(), src, 3);

        let bytes = edge.to_bytes();
        assert_eq!(bytes.len(), 72);

        let restored = DeferredEdge::from_bytes(&bytes).unwrap();
        assert_eq!(restored.target_atom_id, tgt);
        assert_eq!(restored.edge_type, EdgeType::DEFINES.to_u32());
        assert_eq!(restored.source_atom_id, src);
        assert_eq!(restored.created_cycle, 3);
    }

    #[test]
    fn test_deferred_edge_from_bytes_too_small() {
        let small = [0u8; 36];
        assert!(DeferredEdge::from_bytes(&small).is_err());
    }

    #[test]
    fn test_deferred_edge_equality() {
        let src = make_atom_id(1);
        let tgt = make_atom_id(2);
        let a = DeferredEdge::new(tgt, 5, src, 0);
        let b = DeferredEdge::new(tgt, 5, src, 0);
        let c = DeferredEdge::new(tgt, 5, src, 1);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    // ========================================================================
    // DeferredEdgesStore tests
    // ========================================================================

    #[test]
    fn test_deferred_store_new_empty() {
        let tmp = std::env::temp_dir().join(format!("deferred_test_{}", std::process::id()));
        let store = DeferredEdgesStore::new(&tmp);
        assert_eq!(store.pending_count(), 0);
        assert_eq!(store.current_cycle(), 0);
        assert!(!store.has_pending_for(&make_atom_id(1)));
    }

    #[test]
    fn test_deferred_store_add_and_query() {
        let tmp = std::env::temp_dir().join(format!("deferred_test_{}_2", std::process::id()));
        let mut store = DeferredEdgesStore::new(&tmp);

        let src = make_atom_id(1);
        let tgt = make_atom_id(2);
        store.add_deferred(tgt, EdgeType::SUPPORTS.to_u32(), src);

        assert_eq!(store.pending_count(), 1);
        assert!(store.has_pending_for(&tgt));
        assert!(!store.has_pending_for(&make_atom_id(99)));
    }

    #[test]
    fn test_deferred_store_resolve() {
        let tmp = std::env::temp_dir().join(format!("deferred_test_{}_3", std::process::id()));
        let mut store = DeferredEdgesStore::new(&tmp);

        let src1 = make_atom_id(1);
        let src2 = make_atom_id(3);
        let tgt = make_atom_id(2);
        let other_tgt = make_atom_id(99);

        store.add_deferred(tgt, EdgeType::DEFINES.to_u32(), src1);
        store.add_deferred(other_tgt, EdgeType::SUPPORTS.to_u32(), src2);

        assert_eq!(store.pending_count(), 2);

        // Resolve: create the atom that tgt points to
        let resolved = store.try_resolve(&tgt, 42);

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].source_atom_id, src1);
        assert_eq!(resolved[0].target_atom_id, tgt);
        assert_eq!(resolved[0].target_node_num, 42);
        assert_eq!(resolved[0].edge_type, EdgeType::DEFINES.to_u32());

        // The deferred edge for tgt should be removed
        assert_eq!(store.pending_count(), 1);
        assert!(!store.has_pending_for(&tgt));
        assert!(store.has_pending_for(&other_tgt));
    }

    #[test]
    fn test_deferred_store_resolve_multiple() {
        let tmp = std::env::temp_dir().join(format!("deferred_test_{}_4", std::process::id()));
        let mut store = DeferredEdgesStore::new(&tmp);

        let tgt = make_atom_id(5);
        let src1 = make_atom_id(10);
        let src2 = make_atom_id(11);
        let src3 = make_atom_id(12);

        // Three different edges all targeting the same atom
        store.add_deferred(tgt, EdgeType::DEFINES.to_u32(), src1);
        store.add_deferred(tgt, EdgeType::SUPPORTS.to_u32(), src2);
        store.add_deferred(tgt, EdgeType::CONTRADICTS.to_u32(), src3);

        assert_eq!(store.pending_count(), 3);

        let resolved = store.try_resolve(&tgt, 100);

        assert_eq!(resolved.len(), 3);
        assert_eq!(store.pending_count(), 0);

        // Verify all three resolved edges
        let mut types_found: Vec<u32> = resolved.iter().map(|r| r.edge_type).collect();
        types_found.sort();
        let mut expected_types = vec![
            EdgeType::DEFINES.to_u32(),
            EdgeType::SUPPORTS.to_u32(),
            EdgeType::CONTRADICTS.to_u32(),
        ];
        expected_types.sort();
        assert_eq!(types_found, expected_types);
    }

    #[test]
    fn test_deferred_store_resolve_no_match() {
        let tmp = std::env::temp_dir().join(format!("deferred_test_{}_5", std::process::id()));
        let mut store = DeferredEdgesStore::new(&tmp);

        let src = make_atom_id(1);
        let tgt = make_atom_id(2);
        store.add_deferred(tgt, EdgeType::DEFINES.to_u32(), src);

        // Try to resolve with a different atom
        let resolved = store.try_resolve(&make_atom_id(99), 50);
        assert_eq!(resolved.len(), 0);
        assert_eq!(store.pending_count(), 1); // Still pending
    }

    #[test]
    fn test_deferred_store_save_and_load() {
        let tmp = std::env::temp_dir().join(format!("deferred_test_{}_6", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        {
            let mut store = DeferredEdgesStore::new(&tmp);
            store.current_cycle = 7;

            for i in 0..5u8 {
                let src = make_atom_id(i);
                let tgt = make_atom_id(i + 100);
                store.add_deferred(tgt, EdgeType::DEFINES.to_u32(), src);
            }

            store.save().unwrap();
        }

        // Reload from disk
        let store = DeferredEdgesStore::open(&tmp).unwrap();
        assert_eq!(store.pending_count(), 5);
        assert_eq!(store.current_cycle(), 7);

        // Verify edges
        for i in 0..5u8 {
            let tgt = make_atom_id(i + 100);
            assert!(store.has_pending_for(&tgt));
        }
    }

    #[test]
    fn test_deferred_store_retention() {
        let tmp = std::env::temp_dir().join(format!("deferred_test_{}_7", std::process::id()));
        let mut store = DeferredEdgesStore::new(&tmp);

        // Add edges at different cycles
        let src = make_atom_id(1);
        for cycle in 0..10u32 {
            store.current_cycle = cycle;
            let tgt = make_atom_id(cycle as u8);
            store.add_deferred(tgt, EdgeType::DEFINES.to_u32(), src);
        }

        assert_eq!(store.pending_count(), 10);

        // Advance to cycle 10 and apply retention with max_cycles=3
        store.current_cycle = 10;
        let dropped = store.apply_retention(3);

        // Edges from cycles 0-6 should be dropped (age > 3)
        // Edges from cycles 7-9 should remain (age <= 3)
        assert_eq!(dropped, 7);
        assert_eq!(store.pending_count(), 3);

        // Verify remaining edges are from cycles 7, 8, 9
        let remaining_cycles: Vec<u32> = store
            .pending_edges()
            .iter()
            .map(|e| e.created_cycle)
            .collect();
        assert!(remaining_cycles.contains(&7));
        assert!(remaining_cycles.contains(&8));
        assert!(remaining_cycles.contains(&9));
    }

    #[test]
    fn test_deferred_store_default_retention() {
        let tmp = std::env::temp_dir().join(format!("deferred_test_{}_8", std::process::id()));
        let mut store = DeferredEdgesStore::new(&tmp);

        let src = make_atom_id(1);
        for cycle in 0..10u32 {
            store.current_cycle = cycle;
            let tgt = make_atom_id(cycle as u8);
            store.add_deferred(tgt, EdgeType::DEFINES.to_u32(), src);
        }

        store.current_cycle = 10;
        let dropped = store.apply_default_retention();

        // DEFAULT_DEFERRED_RETENTION_CYCLES = 5
        // Edges from cycles 0-4 dropped (age > 5), cycles 5-9 remain
        assert_eq!(dropped, 5);
        assert_eq!(store.pending_count(), 5);
    }

    #[test]
    fn test_deferred_store_advance_cycle() {
        let tmp = std::env::temp_dir().join(format!("deferred_test_{}_9", std::process::id()));
        let mut store = DeferredEdgesStore::new(&tmp);

        assert_eq!(store.current_cycle(), 0);
        store.advance_cycle();
        assert_eq!(store.current_cycle(), 1);
        store.advance_cycle();
        assert_eq!(store.current_cycle(), 2);
    }

    #[test]
    fn test_deferred_store_clear() {
        let tmp = std::env::temp_dir().join(format!("deferred_test_{}_10", std::process::id()));
        let mut store = DeferredEdgesStore::new(&tmp);

        let src = make_atom_id(1);
        for i in 0..5u8 {
            let tgt = make_atom_id(i);
            store.add_deferred(tgt, EdgeType::DEFINES.to_u32(), src);
        }

        assert_eq!(store.pending_count(), 5);
        store.clear();
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn test_deferred_store_empty_resolve() {
        let tmp = std::env::temp_dir().join(format!("deferred_test_{}_11", std::process::id()));
        let mut store = DeferredEdgesStore::new(&tmp);

        let resolved = store.try_resolve(&make_atom_id(1), 0);
        assert_eq!(resolved.len(), 0);
    }

    #[test]
    fn test_deferred_store_display() {
        let tmp = std::env::temp_dir().join(format!("deferred_test_{}_12", std::process::id()));
        let mut store = DeferredEdgesStore::new(&tmp);
        store.current_cycle = 5;

        let src = make_atom_id(1);
        let tgt = make_atom_id(2);
        store.add_deferred(tgt, EdgeType::DEFINES.to_u32(), src);

        let display = format!("{}", store);
        assert!(display.contains("cycle=5"));
        assert!(display.contains("pending=1"));
    }

    #[test]
    fn test_deferred_store_retention_zero_edges() {
        let tmp = std::env::temp_dir().join(format!("deferred_test_{}_13", std::process::id()));
        let mut store = DeferredEdgesStore::new(&tmp);

        let dropped = store.apply_retention(3);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn test_deferred_store_retention_all_expired() {
        let tmp = std::env::temp_dir().join(format!("deferred_test_{}_14", std::process::id()));
        let mut store = DeferredEdgesStore::new(&tmp);

        let src = make_atom_id(1);
        store.current_cycle = 0;
        let tgt = make_atom_id(2);
        store.add_deferred(tgt, EdgeType::DEFINES.to_u32(), src);

        store.current_cycle = 100;
        let dropped = store.apply_retention(5);

        assert_eq!(dropped, 1);
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn test_deferred_store_save_load_empty() {
        let tmp = std::env::temp_dir().join(format!("deferred_test_{}_15", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        {
            let mut store = DeferredEdgesStore::new(&tmp);
            store.current_cycle = 42;
            store.save().unwrap();
        }

        let store = DeferredEdgesStore::open(&tmp).unwrap();
        assert_eq!(store.pending_count(), 0);
        assert_eq!(store.current_cycle(), 42);
    }

    #[test]
    fn test_deferred_store_open_nonexistent() {
        let tmp =
            std::env::temp_dir().join(format!("deferred_test_{}_nonexist", std::process::id()));
        // Don't create the directory
        let store = DeferredEdgesStore::open(&tmp).unwrap();
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn test_deferred_store_resolve_removes_correctly() {
        let tmp = std::env::temp_dir().join(format!("deferred_test_{}_16", std::process::id()));
        let mut store = DeferredEdgesStore::new(&tmp);

        let src = make_atom_id(1);
        let tgt_a = make_atom_id(10);
        let tgt_b = make_atom_id(20);
        let tgt_c = make_atom_id(30);

        store.add_deferred(tgt_a, EdgeType::DEFINES.to_u32(), src);
        store.add_deferred(tgt_b, EdgeType::SUPPORTS.to_u32(), src);
        store.add_deferred(tgt_c, EdgeType::CONTRADICTS.to_u32(), src);

        // Resolve tgt_b (middle element)
        let resolved = store.try_resolve(&tgt_b, 777);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].target_node_num, 777);

        assert_eq!(store.pending_count(), 2);
        assert!(store.has_pending_for(&tgt_a));
        assert!(!store.has_pending_for(&tgt_b));
        assert!(store.has_pending_for(&tgt_c));

        // Resolve tgt_a (first element)
        let resolved2 = store.try_resolve(&tgt_a, 888);
        assert_eq!(resolved2.len(), 1);
        assert_eq!(store.pending_count(), 1);
        assert!(!store.has_pending_for(&tgt_a));
        assert!(store.has_pending_for(&tgt_c));
    }
}
