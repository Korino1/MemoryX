//! CRDT Snapshot persistence for MemoryX SKF-1.1 Phase 2.
//!
//! This module provides snapshot format for CRDT state according to
//! SKF-1.1 Spec A.3.4.
//!
//! # Snapshot Format
//!
//! ```text
//! SnapshotHeader (32 bytes):
//!   magic: u32 = 0x4D534E31 ("MSN1")
//!   ver: u16 = 1
//!   flags: u16
//!   entry_count: u64
//!   off_index: u64
//!   off_data: u64
//!   crc32: u32
//!
//! Index (entry_count entries, 24 bytes each):
//!   node_num: u64
//!   data_off: u64
//!   data_len: u32
//!   data_crc32: u32
//!   (sorted by node_num)
//!
//! Data block per node:
//!   field_count: u32
//!   field_count records:
//!     field_id: u16
//!     crdt_kind: u8
//!     reserved: u8
//!     state_len: u32
//!     state_bytes: [u8; state_len]
//! ```
//!
//! # CRDT State Serializations (A.4)
//!
//! ## GCOUNTER
//! ```text
//! u32 n
//! n records: actor_id[16] + u64 value
//! ```
//!
//! ## PNCounter
//! Two GCounters: P and N
//!
//! ## LWW_REG
//! ```text
//! hlc_phys_ns: u64
//! hlc_logical: u32
//! actor_id: [u8; 16]
//! u32 value_len
//! bytes: [u8; value_len]
//! ```
//!
//! ## ORSet<SymId>
//! ```text
//! u32 elem_count
//! for each:
//!   u32 sym_id
//!   u32 add_dot_count
//!   dots: actor_id[16] + u64 ctr
//!   u32 rem_dot_count
//!   dots: actor_id[16] + u64 ctr
//! ```

#![allow(dead_code)]

use std::collections::HashMap;

use crate::store::{CrdtKind, NodeNum, SymId};
use crate::utils::{crc32, HLC};

use super::{ActorId, CrdtError, Dot, GCounter, LWWReg, ORSet, PNCounter, ACTOR_ID_SIZE};

// ============================================================================
// Constants
// ============================================================================

/// Magic number for snapshots: "MSN1" = 0x4D534E31
pub const SNAPSHOT_MAGIC: u32 = 0x4D534E31;

/// Snapshot format version
pub const SNAPSHOT_VERSION: u16 = 1;

/// Snapshot header size in bytes (36 bytes total)
pub const SNAPSHOT_HEADER_SIZE: usize = 36;

/// Index entry size in bytes (24 bytes)
pub const INDEX_ENTRY_SIZE: usize = 24;

/// Field record header size (8 bytes: field_id + crdt_kind + reserved + state_len)
pub const FIELD_RECORD_HEADER_SIZE: usize = 8;

// ============================================================================
// Snapshot Header
// ============================================================================

/// Snapshot header (32 bytes)
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotHeader {
    /// Magic number: 0x4D534E31 ("MSN1")
    pub magic: u32,
    /// Format version
    pub ver: u16,
    /// Header flags
    pub flags: u16,
    /// Number of entries in index
    pub entry_count: u64,
    /// Offset to index section (from start of file)
    pub off_index: u64,
    /// Offset to data section (from start of file)
    pub off_data: u64,
    /// CRC32 of header (magic through off_data)
    pub crc32: u32,
}

impl SnapshotHeader {
    /// Header size in bytes
    pub const SIZE: usize = SNAPSHOT_HEADER_SIZE;

    /// Create a new snapshot header
    pub fn new(entry_count: u64, index_offset: u64, data_offset: u64) -> Self {
        let mut header = SnapshotHeader {
            magic: SNAPSHOT_MAGIC,
            ver: SNAPSHOT_VERSION,
            flags: 0,
            entry_count,
            off_index: index_offset,
            off_data: data_offset,
            crc32: 0,
        };
        header.crc32 = header.calculate_crc();
        header
    }

    /// Calculate CRC32 of header fields (excluding crc32 field itself)
    fn calculate_crc(&self) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;

        // Update CRC with each field
        crc = Self::crc32_update(crc, &self.magic.to_le_bytes());
        crc = Self::crc32_update(crc, &self.ver.to_le_bytes());
        crc = Self::crc32_update(crc, &self.flags.to_le_bytes());
        crc = Self::crc32_update(crc, &self.entry_count.to_le_bytes());
        crc = Self::crc32_update(crc, &self.off_index.to_le_bytes());
        crc = Self::crc32_update(crc, &self.off_data.to_le_bytes());

        crc ^ 0xFFFF_FFFF
    }

    /// Helper for CRC32 calculation
    #[inline]
    fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
        for &byte in data {
            crc = (crc >> 8) ^ crate::utils::crc32(&[byte]);
        }
        crc
    }

    /// Validate header magic and version
    #[inline]
    pub fn validate_magic(&self) -> bool {
        self.magic == SNAPSHOT_MAGIC && self.ver == SNAPSHOT_VERSION
    }

    /// Validate header CRC
    #[inline]
    pub fn validate_crc(&self) -> bool {
        self.crc32 == self.calculate_crc()
    }

    /// Check if header is valid
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.validate_magic() && self.validate_crc()
    }

    /// Serialize header to bytes (little-endian)
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        let mut offset = 0usize;

        // magic: u32 (4 bytes)
        buf[offset..offset + 4].copy_from_slice(&self.magic.to_le_bytes());
        offset += 4;

        // ver: u16 (2 bytes)
        buf[offset..offset + 2].copy_from_slice(&self.ver.to_le_bytes());
        offset += 2;

        // flags: u16 (2 bytes)
        buf[offset..offset + 2].copy_from_slice(&self.flags.to_le_bytes());
        offset += 2;

        // entry_count: u64 (8 bytes)
        buf[offset..offset + 8].copy_from_slice(&self.entry_count.to_le_bytes());
        offset += 8;

        // off_index: u64 (8 bytes)
        buf[offset..offset + 8].copy_from_slice(&self.off_index.to_le_bytes());
        offset += 8;

        // off_data: u64 (8 bytes)
        buf[offset..offset + 8].copy_from_slice(&self.off_data.to_le_bytes());
        offset += 8;

        // crc32: u32 (4 bytes)
        buf[offset..offset + 4].copy_from_slice(&self.crc32.to_le_bytes());

        buf
    }

    /// Deserialize header from bytes (little-endian)
    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::SIZE {
            return None;
        }

        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let ver = u16::from_le_bytes([buf[4], buf[5]]);
        let flags = u16::from_le_bytes([buf[6], buf[7]]);

        let entry_count = u64::from_le_bytes([
            buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
        ]);

        let off_index = u64::from_le_bytes([
            buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23],
        ]);

        let off_data = u64::from_le_bytes([
            buf[24], buf[25], buf[26], buf[27], buf[28], buf[29], buf[30], buf[31],
        ]);

        // CRC is read from the header bytes
        let crc32 = u32::from_le_bytes([buf[32], buf[33], buf[34], buf[35]]);

        Some(SnapshotHeader {
            magic,
            ver,
            flags,
            entry_count,
            off_index,
            off_data,
            crc32,
        })
    }
}

// ============================================================================
// Index Entry
// ============================================================================

/// Index entry for a node (24 bytes)
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry {
    /// Node number
    pub node_num: NodeNum,
    /// Offset to data in data section (relative to off_data)
    pub data_off: u64,
    /// Data length in bytes
    pub data_len: u32,
    /// CRC32 of data block
    pub data_crc32: u32,
}

impl IndexEntry {
    /// Entry size in bytes
    pub const SIZE: usize = INDEX_ENTRY_SIZE;

    /// Create a new index entry
    #[inline]
    pub fn new(node_num: NodeNum, data_off: u64, data_len: u32, data_crc32: u32) -> Self {
        IndexEntry {
            node_num,
            data_off,
            data_len,
            data_crc32,
        }
    }

    /// Serialize entry to bytes (little-endian)
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        let mut offset = 0usize;

        // node_num: u64 (8 bytes)
        buf[offset..offset + 8].copy_from_slice(&self.node_num.to_le_bytes());
        offset += 8;

        // data_off: u64 (8 bytes)
        buf[offset..offset + 8].copy_from_slice(&self.data_off.to_le_bytes());
        offset += 8;

        // data_len: u32 (4 bytes)
        buf[offset..offset + 4].copy_from_slice(&self.data_len.to_le_bytes());
        offset += 4;

        // data_crc32: u32 (4 bytes)
        buf[offset..offset + 4].copy_from_slice(&self.data_crc32.to_le_bytes());

        buf
    }

    /// Deserialize entry from bytes (little-endian)
    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::SIZE {
            return None;
        }

        let node_num = u64::from_le_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
        ]);

        let data_off = u64::from_le_bytes([
            buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
        ]);

        let data_len = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);

        let data_crc32 = u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);

        Some(IndexEntry::new(node_num, data_off, data_len, data_crc32))
    }
}

// ============================================================================
// Field State
// ============================================================================

/// Field record header (8 bytes)
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldRecordHeader {
    /// Field identifier
    pub field_id: u16,
    /// CRDT kind
    pub crdt_kind: u8,
    /// Reserved (must be 0)
    pub reserved: u8,
    /// State length in bytes
    pub state_len: u32,
}

impl FieldRecordHeader {
    /// Header size in bytes
    pub const SIZE: usize = FIELD_RECORD_HEADER_SIZE;

    /// Create a new field record header
    #[inline]
    pub fn new(field_id: u16, crdt_kind: u8, state_len: u32) -> Self {
        FieldRecordHeader {
            field_id,
            crdt_kind,
            reserved: 0,
            state_len,
        }
    }

    /// Serialize to bytes
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];

        buf[0..2].copy_from_slice(&self.field_id.to_le_bytes());
        buf[2] = self.crdt_kind;
        buf[3] = self.reserved;
        buf[4..8].copy_from_slice(&self.state_len.to_le_bytes());

        buf
    }

    /// Deserialize from bytes
    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::SIZE {
            return None;
        }

        let field_id = u16::from_le_bytes([buf[0], buf[1]]);
        let crdt_kind = buf[2];
        // Reserved byte at buf[3] - intentionally ignored
        let state_len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);

        Some(FieldRecordHeader::new(field_id, crdt_kind, state_len))
    }
}

/// Field state for serialization
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldState {
    /// Field identifier
    pub field_id: u16,
    /// CRDT kind
    pub crdt_kind: CrdtKind,
    /// Serialized state bytes
    pub state: Vec<u8>,
}

impl FieldState {
    /// Create a new field state from a GCounter
    pub fn from_gcounter(field_id: u16, counter: &GCounter) -> Self {
        FieldState {
            field_id,
            crdt_kind: CrdtKind::GCOUNTER,
            state: serialize_gcounter(counter),
        }
    }

    /// Create a new field state from a PNCounter
    pub fn from_pncounter(field_id: u16, counter: &PNCounter) -> Self {
        FieldState {
            field_id,
            crdt_kind: CrdtKind::PNCOUNTER,
            state: serialize_pncounter(counter),
        }
    }

    /// Create a new field state from an LWWReg
    pub fn from_lwwreg(field_id: u16, reg: &LWWReg<Vec<u8>>) -> Self {
        FieldState {
            field_id,
            crdt_kind: CrdtKind::LWW_REG,
            state: serialize_lwwreg(reg),
        }
    }

    /// Create a new field state from an ORSet<SymId>
    pub fn from_orset(field_id: u16, set: &ORSet<SymId>) -> Self {
        FieldState {
            field_id,
            crdt_kind: CrdtKind::ORSET,
            state: serialize_orset(set),
        }
    }

    /// Get the total serialized size (header + state)
    #[inline]
    pub fn serialized_size(&self) -> usize {
        FieldRecordHeader::SIZE + self.state.len()
    }

    /// Serialize field to a buffer
    pub fn serialize(&self, buf: &mut Vec<u8>) {
        let header = FieldRecordHeader::new(
            self.field_id,
            self.crdt_kind.to_u8(),
            self.state.len() as u32,
        );
        buf.extend_from_slice(&header.to_bytes());
        buf.extend_from_slice(&self.state);
    }
}

// ============================================================================
// CRDT State Serialization (A.4)
// ============================================================================

/// Serialize a GCounter
/// Format: u32 n + n*(actor_id[16] + u64 value)
fn serialize_gcounter(counter: &GCounter) -> Vec<u8> {
    let counts = counter.counts();
    let mut buf = Vec::with_capacity(4 + counts.len() * (ACTOR_ID_SIZE + 8));

    // Write count
    buf.extend_from_slice(&(counts.len() as u32).to_le_bytes());

    // Write each (actor, count) pair
    for (actor, count) in counts {
        buf.extend_from_slice(&actor.0);
        buf.extend_from_slice(&count.to_le_bytes());
    }

    buf
}

/// Deserialize a GCounter
fn deserialize_gcounter(data: &[u8]) -> Option<GCounter> {
    if data.len() < 4 {
        return None;
    }

    let count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let mut counter = GCounter::new();

    let mut offset = 4usize;
    for _ in 0..count {
        if offset + ACTOR_ID_SIZE + 8 > data.len() {
            return None;
        }

        let mut actor_bytes = [0u8; ACTOR_ID_SIZE];
        actor_bytes.copy_from_slice(&data[offset..offset + ACTOR_ID_SIZE]);
        offset += ACTOR_ID_SIZE;

        let value = u64::from_le_bytes([
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

        counter.inc(ActorId(actor_bytes), value);
    }

    Some(counter)
}

/// Serialize a PNCounter
/// Format: serialize_gcounter(P) + serialize_gcounter(N)
fn serialize_pncounter(counter: &PNCounter) -> Vec<u8> {
    let p_bytes = counter.p_counts().to_bytes();
    let n_bytes = counter.n_counts().to_bytes();

    let mut buf = Vec::with_capacity(8 + p_bytes.len() + n_bytes.len());

    // Write P counter
    buf.extend_from_slice(&(p_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(&p_bytes);

    // Write N counter
    buf.extend_from_slice(&(n_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(&n_bytes);

    buf
}

/// Deserialize a PNCounter
fn deserialize_pncounter(data: &[u8]) -> Option<PNCounter> {
    if data.len() < 8 {
        return None;
    }

    let p_len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if 4 + p_len > data.len() {
        return None;
    }
    let p_data = &data[4..4 + p_len];

    let n_offset = 4 + p_len;
    if n_offset + 4 > data.len() {
        return None;
    }
    let n_len = u32::from_le_bytes([
        data[n_offset],
        data[n_offset + 1],
        data[n_offset + 2],
        data[n_offset + 3],
    ]) as usize;
    if n_offset + 4 + n_len > data.len() {
        return None;
    }
    let n_data = &data[n_offset + 4..n_offset + 4 + n_len];

    let p = deserialize_gcounter(p_data)?;
    let n = deserialize_gcounter(n_data)?;

    Some(PNCounter::from_gcounters(p, n))
}

/// Serialize an LWWReg<Vec<u8>>
/// Format: hlc_phys_ns: u64 + hlc_logical: u32 + actor_id: [u8; 16] + value_len: u32 + value: [u8]
fn serialize_lwwreg(reg: &LWWReg<Vec<u8>>) -> Vec<u8> {
    let value = reg.get();
    let mut buf = Vec::with_capacity(8 + 4 + 16 + 4 + value.len());

    // HLC physical time
    buf.extend_from_slice(&reg.hlc().physical_ns().to_le_bytes());
    // HLC logical counter
    buf.extend_from_slice(&(reg.hlc().logical() as u32).to_le_bytes());
    // Actor ID
    buf.extend_from_slice(&reg.actor().0);
    // Value length
    buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
    // Value bytes
    buf.extend_from_slice(value);

    buf
}

/// Deserialize an LWWReg<Vec<u8>>
fn deserialize_lwwreg(data: &[u8]) -> Option<LWWReg<Vec<u8>>> {
    if data.len() < 8 + 4 + 16 + 4 {
        return None;
    }

    let hlc_phys = u64::from_le_bytes([
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ]);
    let hlc_logical = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as u16;

    let mut actor_bytes = [0u8; ACTOR_ID_SIZE];
    actor_bytes.copy_from_slice(&data[12..12 + ACTOR_ID_SIZE]);

    let value_len = u32::from_le_bytes([data[28], data[29], data[30], data[31]]) as usize;

    if 32 + value_len > data.len() {
        return None;
    }

    let value = data[32..32 + value_len].to_vec();

    Some(LWWReg::new(
        HLC::from_parts(hlc_phys, hlc_logical),
        ActorId(actor_bytes),
        value,
    ))
}

/// Serialize an ORSet<SymId>
/// Format: u32 elem_count + for each: sym_id + add_dot_count + dots + rem_dot_count + dots
fn serialize_orset(set: &ORSet<SymId>) -> Vec<u8> {
    let elements = set.elements();

    // Calculate size needed
    let mut size = 4usize; // elem_count
    for (elem, add_dots) in elements {
        size += 4; // sym_id
        size += 4; // add_dot_count
        size += add_dots.len() * (ACTOR_ID_SIZE + 8); // add dots

        let rem_dots = set.tombstones().get(elem).map(|d| d.len()).unwrap_or(0);
        size += 4; // rem_dot_count
        size += rem_dots * (ACTOR_ID_SIZE + 8); // rem dots
    }

    let mut buf = Vec::with_capacity(size);

    // Write element count
    buf.extend_from_slice(&(elements.len() as u32).to_le_bytes());

    // Write each element
    for (elem, add_dots) in elements {
        // Symbol ID
        buf.extend_from_slice(&elem.to_le_bytes());

        // Add dots
        buf.extend_from_slice(&(add_dots.len() as u32).to_le_bytes());
        for dot in add_dots {
            buf.extend_from_slice(&dot.actor.0);
            buf.extend_from_slice(&dot.counter.to_le_bytes());
        }

        // Remove dots
        let rem_dots = set.tombstones().get(elem);
        let rem_count = rem_dots.map(|d| d.len()).unwrap_or(0);
        buf.extend_from_slice(&(rem_count as u32).to_le_bytes());
        if let Some(dots) = rem_dots {
            for dot in dots {
                buf.extend_from_slice(&dot.actor.0);
                buf.extend_from_slice(&dot.counter.to_le_bytes());
            }
        }
    }

    buf
}

/// Deserialize an ORSet<SymId>
fn deserialize_orset(data: &[u8]) -> Option<ORSet<SymId>> {
    if data.len() < 4 {
        return None;
    }

    let elem_count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let mut set = ORSet::new();

    let mut offset = 4usize;
    for _ in 0..elem_count {
        // Read sym_id
        if offset + 4 > data.len() {
            return None;
        }
        let sym_id = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        offset += 4;

        // Read add_dot_count
        if offset + 4 > data.len() {
            return None;
        }
        let add_count = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;

        // Read add dots
        for _ in 0..add_count {
            if offset + ACTOR_ID_SIZE + 8 > data.len() {
                return None;
            }
            let mut actor_bytes = [0u8; ACTOR_ID_SIZE];
            actor_bytes.copy_from_slice(&data[offset..offset + ACTOR_ID_SIZE]);
            offset += ACTOR_ID_SIZE;

            let counter = u64::from_le_bytes([
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

            set.add_dot(ActorId(actor_bytes), sym_id, counter);
        }

        // Read rem_dot_count
        if offset + 4 > data.len() {
            return None;
        }
        let rem_count = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;

        // Read rem dots
        for _ in 0..rem_count {
            if offset + ACTOR_ID_SIZE + 8 > data.len() {
                return None;
            }
            let mut actor_bytes = [0u8; ACTOR_ID_SIZE];
            actor_bytes.copy_from_slice(&data[offset..offset + ACTOR_ID_SIZE]);
            offset += ACTOR_ID_SIZE;

            let counter = u64::from_le_bytes([
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

            set.remove_dot(ActorId(actor_bytes), sym_id, counter);
        }
    }

    Some(set)
}

// ============================================================================
// Node State
// ============================================================================

/// Node state containing all field CRDTs
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeState {
    /// Node number
    pub node_num: NodeNum,
    /// Fields by field_id
    pub fields: HashMap<u16, FieldState>,
}

impl NodeState {
    /// Create a new node state
    #[inline]
    pub fn new(node_num: NodeNum) -> Self {
        NodeState {
            node_num,
            fields: HashMap::new(),
        }
    }

    /// Add a field to the node state
    #[inline]
    pub fn add_field(&mut self, field: FieldState) {
        self.fields.insert(field.field_id, field);
    }

    /// Get a field by ID
    #[inline]
    pub fn get_field(&self, field_id: u16) -> Option<&FieldState> {
        self.fields.get(&field_id)
    }

    /// Serialize node state to bytes
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::new();

        // field_count: u32
        buf.extend_from_slice(&(self.fields.len() as u32).to_le_bytes());

        // Serialize each field
        for field in self.fields.values() {
            field.serialize(&mut buf);
        }

        buf
    }

    /// Deserialize node state from bytes
    pub fn deserialize(node_num: NodeNum, data: &[u8]) -> Option<Self> {
        if data.len() < 4 {
            return None;
        }

        let field_count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let mut state = NodeState::new(node_num);

        let mut offset = 4usize;
        for _ in 0..field_count {
            // Read field header
            if offset + FieldRecordHeader::SIZE > data.len() {
                return None;
            }
            let header =
                FieldRecordHeader::from_bytes(&data[offset..offset + FieldRecordHeader::SIZE])?;
            offset += FieldRecordHeader::SIZE;

            // Read field state
            if offset + header.state_len as usize > data.len() {
                return None;
            }
            let state_bytes = data[offset..offset + header.state_len as usize].to_vec();
            offset += header.state_len as usize;

            let crdt_kind = CrdtKind::from_u8(header.crdt_kind)?;

            state.fields.insert(
                header.field_id,
                FieldState {
                    field_id: header.field_id,
                    crdt_kind,
                    state: state_bytes,
                },
            );
        }

        Some(state)
    }
}

// ============================================================================
// Snapshot Builder
// ============================================================================

/// Builder for creating CRDT snapshots
///
/// # Example
///
/// ```
/// use memoryx::crdt::{SnapshotBuilder, FieldState, GCounter, ActorId};
///
/// let mut builder = SnapshotBuilder::new();
///
/// let mut counter = GCounter::new();
/// counter.inc(ActorId::generate(), 10);
///
/// let field = FieldState::from_gcounter(0, &counter);
/// builder.add_node(1, vec![field]);
///
/// let snapshot = builder.build();
/// ```
#[derive(Debug)]
pub struct SnapshotBuilder {
    /// Map of node_num -> node state being built
    nodes: HashMap<NodeNum, NodeState>,
}

impl SnapshotBuilder {
    /// Create a new snapshot builder
    #[inline]
    pub fn new() -> Self {
        SnapshotBuilder {
            nodes: HashMap::new(),
        }
    }

    /// Add a node with its fields to the snapshot
    ///
    /// If the node already exists, the fields will be merged.
    pub fn add_node(&mut self, node_num: NodeNum, fields: Vec<FieldState>) {
        let node = self
            .nodes
            .entry(node_num)
            .or_insert_with(|| NodeState::new(node_num));
        for field in fields {
            node.add_field(field);
        }
    }

    /// Build the snapshot into a byte vector
    ///
    /// Returns a complete snapshot ready for storage or transmission.
    pub fn build(self) -> Vec<u8> {
        let entry_count = self.nodes.len() as u64;

        // Calculate offsets
        let header_size = SnapshotHeader::SIZE;
        let index_size = entry_count as usize * IndexEntry::SIZE;
        let index_offset = header_size as u64;
        let data_offset = index_offset + index_size as u64;

        // Serialize all node data and build index
        let mut index_entries: Vec<IndexEntry> = Vec::with_capacity(self.nodes.len());
        let mut data_blocks: Vec<(NodeNum, Vec<u8>)> = Vec::with_capacity(self.nodes.len());
        let mut current_data_offset: u64 = 0;

        // Sort nodes by node_num for binary search capability
        let mut nodes: Vec<_> = self.nodes.into_iter().collect();
        nodes.sort_by_key(|(k, _)| *k);

        for (node_num, node_state) in nodes {
            let data = node_state.serialize();
            let data_len = data.len() as u32;
            let data_crc = crc32(&data);

            index_entries.push(IndexEntry::new(
                node_num,
                current_data_offset,
                data_len,
                data_crc,
            ));

            current_data_offset += data_len as u64;
            data_blocks.push((node_num, data));
        }

        // Calculate total size
        let total_data_size = current_data_offset as usize;
        let total_size = header_size + index_size + total_data_size;

        // Allocate buffer
        let mut buf = Vec::with_capacity(total_size);

        // Write header
        let header = SnapshotHeader::new(entry_count, index_offset, data_offset);
        buf.extend_from_slice(&header.to_bytes());

        // Write index
        for entry in index_entries {
            buf.extend_from_slice(&entry.to_bytes());
        }

        // Write data blocks
        for (_, data) in data_blocks {
            buf.extend_from_slice(&data);
        }

        buf
    }

    /// Get the number of nodes in the builder
    #[inline]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
}

impl Default for SnapshotBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Node Iterator
// ============================================================================

/// Iterator over nodes in a snapshot
pub struct NodeIterator<'a> {
    /// Reference to snapshot data
    data: &'a [u8],
    /// Index entries (owned)
    index: std::vec::IntoIter<IndexEntry>,
    /// Data section offset (absolute)
    data_offset: u64,
}

impl<'a> NodeIterator<'a> {
    /// Create a new node iterator
    fn new(data: &'a [u8], index: Vec<IndexEntry>, data_offset: u64) -> Self {
        NodeIterator {
            data,
            index: index.into_iter(),
            data_offset,
        }
    }
}

impl<'a> Iterator for NodeIterator<'a> {
    type Item = Result<NodeState, CrdtError>;

    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.index.next()?;

        // Calculate absolute offset in data
        let abs_offset = self.data_offset + entry.data_off;
        let end_offset = abs_offset + entry.data_len as u64;

        if end_offset as usize > self.data.len() {
            return Some(Err(CrdtError::CorruptRecord));
        }

        let node_data = &self.data[abs_offset as usize..end_offset as usize];

        // Verify CRC
        let crc = crc32(node_data);
        if crc != entry.data_crc32 {
            return Some(Err(CrdtError::CrcMismatch));
        }

        match NodeState::deserialize(entry.node_num, node_data) {
            Some(state) => Some(Ok(state)),
            None => Some(Err(CrdtError::CorruptRecord)),
        }
    }
}

// ============================================================================
// Snapshot Reader
// ============================================================================

/// Reader for CRDT snapshots
///
/// # Example
///
/// ```no_run
/// use memoryx::crdt::SnapshotReader;
///
/// let data = vec![/* snapshot bytes */];
/// let reader = SnapshotReader::from_bytes(&data).unwrap();
///
/// if let Some(node) = reader.get_node(1) {
///     // Process node state
/// }
/// ```
#[derive(Debug)]
pub struct SnapshotReader<'a> {
    /// Reference to snapshot data
    data: &'a [u8],
    /// Parsed header
    header: SnapshotHeader,
    /// Parsed index entries (sorted by node_num)
    index: Vec<IndexEntry>,
}

impl<'a> SnapshotReader<'a> {
    /// Create a snapshot reader from byte slice
    ///
    /// Validates the header and index, but does not validate data blocks
    /// until they are accessed.
    pub fn from_bytes(data: &'a [u8]) -> Result<Self, CrdtError> {
        if data.len() < SnapshotHeader::SIZE {
            return Err(CrdtError::CorruptRecord);
        }

        // Parse header
        let header = SnapshotHeader::from_bytes(&data[..SnapshotHeader::SIZE])
            .ok_or(CrdtError::CorruptRecord)?;

        if !header.is_valid() {
            return Err(CrdtError::InvalidMagic);
        }

        // Parse index
        let index_start = header.off_index as usize;
        let index_end = header.off_data as usize;

        if index_end < index_start || index_end > data.len() {
            return Err(CrdtError::CorruptRecord);
        }

        let index_data = &data[index_start..index_end];
        let expected_index_size = header.entry_count as usize * IndexEntry::SIZE;

        if index_data.len() != expected_index_size {
            return Err(CrdtError::CorruptRecord);
        }

        let mut index = Vec::with_capacity(header.entry_count as usize);
        for i in 0..header.entry_count as usize {
            let entry_offset = i * IndexEntry::SIZE;
            let entry =
                IndexEntry::from_bytes(&index_data[entry_offset..entry_offset + IndexEntry::SIZE])
                    .ok_or(CrdtError::CorruptRecord)?;
            index.push(entry);
        }

        // Verify index is sorted by node_num
        for i in 1..index.len() {
            if index[i].node_num < index[i - 1].node_num {
                return Err(CrdtError::CorruptRecord);
            }
        }

        Ok(SnapshotReader {
            data,
            header,
            index,
        })
    }

    /// Get node state by node number
    ///
    /// Uses binary search on the index for O(log n) lookup.
    pub fn get_node(&self, node_num: NodeNum) -> Option<NodeState> {
        // Binary search for the node
        let entry = self
            .index
            .binary_search_by_key(&node_num, |e| e.node_num)
            .ok()
            .map(|idx| &self.index[idx])?;

        // Calculate absolute offset
        let abs_offset = self.header.off_data + entry.data_off;
        let end_offset = abs_offset + entry.data_len as u64;

        if end_offset as usize > self.data.len() {
            return None;
        }

        let node_data = &self.data[abs_offset as usize..end_offset as usize];

        // Verify CRC
        let crc = crc32(node_data);
        if crc != entry.data_crc32 {
            return None;
        }

        NodeState::deserialize(node_num, node_data)
    }

    /// Iterate over all nodes in the snapshot
    ///
    /// Returns nodes in sorted order by node_num.
    pub fn iter_nodes(&self) -> NodeIterator<'a> {
        NodeIterator::new(self.data, self.index.clone(), self.header.off_data)
    }

    /// Get the header
    #[inline]
    pub fn header(&self) -> &SnapshotHeader {
        &self.header
    }

    /// Get the entry count
    #[inline]
    pub fn entry_count(&self) -> u64 {
        self.header.entry_count
    }

    /// Validate all data blocks (expensive)
    ///
    /// This checks CRCs for all data blocks.
    pub fn validate(&self) -> Result<(), CrdtError> {
        for entry in &self.index {
            let abs_offset = self.header.off_data + entry.data_off;
            let end_offset = abs_offset + entry.data_len as u64;

            if end_offset as usize > self.data.len() {
                return Err(CrdtError::CorruptRecord);
            }

            let node_data = &self.data[abs_offset as usize..end_offset as usize];
            let crc = crc32(node_data);

            if crc != entry.data_crc32 {
                return Err(CrdtError::CrcMismatch);
            }

            // Also try to deserialize
            if NodeState::deserialize(entry.node_num, node_data).is_none() {
                return Err(CrdtError::CorruptRecord);
            }
        }

        Ok(())
    }
}

// ============================================================================
// Extension traits for CRDT types (to support serialization)
// ============================================================================

/// Extension trait for PNCounter to access internal state
pub trait PNCounterExt {
    /// Get the positive GCounter
    fn p_counts(&self) -> &GCounter;
    /// Get the negative GCounter
    fn n_counts(&self) -> &GCounter;
    /// Create from two GCounters
    fn from_gcounters(p: GCounter, n: GCounter) -> Self;
}

impl PNCounterExt for PNCounter {
    fn p_counts(&self) -> &GCounter {
        &self.p
    }

    fn n_counts(&self) -> &GCounter {
        &self.n
    }

    fn from_gcounters(p: GCounter, n: GCounter) -> Self {
        PNCounter { p, n }
    }
}

/// Extension trait for ORSet to access internal state
pub trait ORSetExt<T: Clone> {
    /// Get the elements HashMap
    fn elements(&self) -> &HashMap<T, std::collections::HashSet<Dot>>;
    /// Get the tombstones HashMap
    fn tombstones(&self) -> &HashMap<T, std::collections::HashSet<Dot>>;
    /// Add a specific dot for an element
    fn add_dot(&mut self, actor: ActorId, element: T, counter: u64);
    /// Remove a specific dot for an element
    fn remove_dot(&mut self, actor: ActorId, element: T, counter: u64);
}

impl ORSetExt<SymId> for ORSet<SymId> {
    fn elements(&self) -> &HashMap<SymId, std::collections::HashSet<Dot>> {
        &self.elements
    }

    fn tombstones(&self) -> &HashMap<SymId, std::collections::HashSet<Dot>> {
        &self.tombstones
    }

    fn add_dot(&mut self, actor: ActorId, element: SymId, counter: u64) {
        self.elements
            .entry(element)
            .or_default()
            .insert(Dot::new(actor, counter));

        // Update counters
        let current = self.counters.entry(actor).or_insert(0);
        *current = (*current).max(counter + 1);
    }

    fn remove_dot(&mut self, actor: ActorId, element: SymId, counter: u64) {
        self.tombstones
            .entry(element)
            .or_default()
            .insert(Dot::new(actor, counter));

        // Update counters
        let current = self.counters.entry(actor).or_insert(0);
        *current = (*current).max(counter + 1);
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crdt::{ActorId, GCounter, LWWReg, ORSet, PNCounter};
    use crate::store::CrdtKind;
    use crate::utils::HLC;

    fn test_actor() -> ActorId {
        ActorId::new([0x42; ACTOR_ID_SIZE])
    }

    #[test]
    fn test_snapshot_header_roundtrip() {
        let header = SnapshotHeader::new(100, 32, 2432);

        let bytes = header.to_bytes();
        assert_eq!(bytes.len(), SnapshotHeader::SIZE);

        let header2 = SnapshotHeader::from_bytes(&bytes).unwrap();
        assert_eq!(header.magic, header2.magic);
        assert_eq!(header.ver, header2.ver);
        assert_eq!(header.flags, header2.flags);
        assert_eq!(header.entry_count, header2.entry_count);
        assert_eq!(header.off_index, header2.off_index);
        assert_eq!(header.off_data, header2.off_data);
    }

    #[test]
    fn test_snapshot_header_validation() {
        let header = SnapshotHeader::new(10, 32, 512);
        assert!(header.is_valid());
        assert!(header.validate_magic());
        assert!(header.validate_crc());

        // Corrupt the header
        let mut bytes = header.to_bytes();
        bytes[0] = 0xFF; // Corrupt magic

        let header2 = SnapshotHeader::from_bytes(&bytes).unwrap();
        assert!(!header2.validate_magic());
        assert!(!header2.is_valid());
    }

    #[test]
    fn test_index_entry_roundtrip() {
        let entry = IndexEntry::new(42, 1024, 256, 0xDEADBEEF);

        let bytes = entry.to_bytes();
        assert_eq!(bytes.len(), IndexEntry::SIZE);

        let entry2 = IndexEntry::from_bytes(&bytes).unwrap();
        assert_eq!(entry.node_num, entry2.node_num);
        assert_eq!(entry.data_off, entry2.data_off);
        assert_eq!(entry.data_len, entry2.data_len);
        assert_eq!(entry.data_crc32, entry2.data_crc32);
    }

    #[test]
    fn test_field_record_header_roundtrip() {
        let header = FieldRecordHeader::new(5, CrdtKind::GCOUNTER.to_u8(), 128);

        let bytes = header.to_bytes();
        assert_eq!(bytes.len(), FieldRecordHeader::SIZE);

        let header2 = FieldRecordHeader::from_bytes(&bytes).unwrap();
        assert_eq!(header.field_id, header2.field_id);
        assert_eq!(header.crdt_kind, header2.crdt_kind);
        assert_eq!(header.reserved, header2.reserved);
        assert_eq!(header.state_len, header2.state_len);
    }

    #[test]
    fn test_gcounter_serialization() {
        let mut counter = GCounter::new();
        let actor1 = ActorId::new([1u8; ACTOR_ID_SIZE]);
        let actor2 = ActorId::new([2u8; ACTOR_ID_SIZE]);

        counter.inc(actor1, 100);
        counter.inc(actor2, 200);

        let serialized = serialize_gcounter(&counter);
        let deserialized = deserialize_gcounter(&serialized).unwrap();

        assert_eq!(deserialized.value(), 300);
        assert_eq!(deserialized.get(&actor1), 100);
        assert_eq!(deserialized.get(&actor2), 200);
    }

    #[test]
    fn test_pncounter_serialization() {
        let mut counter = PNCounter::new();
        let actor = test_actor();

        counter.inc(actor, 100);
        counter.dec(actor, 30);

        let serialized = serialize_pncounter(&counter);
        let deserialized = deserialize_pncounter(&serialized).unwrap();

        assert_eq!(deserialized.value(), 70);
    }

    #[test]
    fn test_lwwreg_serialization() {
        let hlc = HLC::from_parts(1_000_000, 42);
        let actor = test_actor();
        let value = vec![0xAA, 0xBB, 0xCC, 0xDD];

        let reg = LWWReg::new(hlc, actor, value.clone());
        let serialized = serialize_lwwreg(&reg);
        let deserialized = deserialize_lwwreg(&serialized).unwrap();

        assert_eq!(deserialized.get(), &value);
        assert_eq!(deserialized.hlc().physical_ns(), hlc.physical_ns());
        assert_eq!(deserialized.hlc().logical(), hlc.logical());
    }

    #[test]
    fn test_orset_serialization() {
        let mut set: ORSet<SymId> = ORSet::new();
        let actor = test_actor();

        set.add(actor, 1);
        set.add(actor, 2);
        set.remove(actor, &1);

        let serialized = serialize_orset(&set);
        let deserialized = deserialize_orset(&serialized).unwrap();

        assert!(!deserialized.contains(&1));
        assert!(deserialized.contains(&2));
    }

    #[test]
    fn test_node_state_serialization() {
        let mut node = NodeState::new(42);

        // Add GCounter field
        let mut counter = GCounter::new();
        counter.inc(test_actor(), 100);
        node.add_field(FieldState::from_gcounter(0, &counter));

        // Add LWWReg field
        let reg = LWWReg::new(HLC::now(), test_actor(), vec![1, 2, 3]);
        node.add_field(FieldState::from_lwwreg(1, &reg));

        let serialized = node.serialize();
        let deserialized = NodeState::deserialize(42, &serialized).unwrap();

        assert_eq!(deserialized.node_num, 42);
        assert_eq!(deserialized.fields.len(), 2);
        assert!(deserialized.get_field(0).is_some());
        assert!(deserialized.get_field(1).is_some());
    }

    #[test]
    fn test_snapshot_builder_empty() {
        let builder = SnapshotBuilder::new();
        let snapshot = builder.build();

        // Should have just the header
        assert_eq!(snapshot.len(), SnapshotHeader::SIZE);

        let reader = SnapshotReader::from_bytes(&snapshot).unwrap();
        assert_eq!(reader.entry_count(), 0);
        assert!(reader.iter_nodes().next().is_none());
    }

    #[test]
    fn test_snapshot_builder_single_node() {
        let mut builder = SnapshotBuilder::new();

        let mut counter = GCounter::new();
        counter.inc(test_actor(), 100);

        let field = FieldState::from_gcounter(0, &counter);
        builder.add_node(1, vec![field]);

        let snapshot = builder.build();
        assert!(snapshot.len() > SnapshotHeader::SIZE);

        let reader = SnapshotReader::from_bytes(&snapshot).unwrap();
        assert_eq!(reader.entry_count(), 1);

        let node = reader.get_node(1).unwrap();
        assert_eq!(node.node_num, 1);
        assert_eq!(node.fields.len(), 1);
    }

    #[test]
    fn test_snapshot_builder_multiple_nodes() {
        let mut builder = SnapshotBuilder::new();

        for i in 1..=5 {
            let mut counter = GCounter::new();
            counter.inc(test_actor(), i * 10);
            let field = FieldState::from_gcounter(0, &counter);
            builder.add_node(i, vec![field]);
        }

        let snapshot = builder.build();
        let reader = SnapshotReader::from_bytes(&snapshot).unwrap();

        assert_eq!(reader.entry_count(), 5);

        // Test get_node for each
        for i in 1..=5 {
            let node = reader.get_node(i).unwrap();
            assert_eq!(node.node_num, i);
        }

        // Test iteration
        let nodes: Vec<_> = reader.iter_nodes().filter_map(|r| r.ok()).collect();
        assert_eq!(nodes.len(), 5);
    }

    #[test]
    fn test_snapshot_builder_multiple_fields() {
        let mut builder = SnapshotBuilder::new();

        let mut counter = GCounter::new();
        counter.inc(test_actor(), 100);

        let reg = LWWReg::new(HLC::now(), test_actor(), vec![1, 2, 3]);

        let fields = vec![
            FieldState::from_gcounter(0, &counter),
            FieldState::from_lwwreg(1, &reg),
        ];

        builder.add_node(1, fields);

        let snapshot = builder.build();
        let reader = SnapshotReader::from_bytes(&snapshot).unwrap();

        let node = reader.get_node(1).unwrap();
        assert_eq!(node.fields.len(), 2);
        assert!(node.get_field(0).is_some());
        assert!(node.get_field(1).is_some());
    }

    #[test]
    fn test_snapshot_corruption_detection() {
        let mut builder = SnapshotBuilder::new();

        let mut counter = GCounter::new();
        counter.inc(test_actor(), 100);

        builder.add_node(1, vec![FieldState::from_gcounter(0, &counter)]);

        let mut snapshot = builder.build();

        // Corrupt a byte in the data section
        snapshot[SnapshotHeader::SIZE + IndexEntry::SIZE + 10] ^= 0xFF;

        // Reader should still parse, but get_node should fail CRC check
        let reader = SnapshotReader::from_bytes(&snapshot).unwrap();
        assert!(reader.get_node(1).is_none());
    }

    #[test]
    fn test_snapshot_validation() {
        let mut builder = SnapshotBuilder::new();

        let mut counter = GCounter::new();
        counter.inc(test_actor(), 100);

        builder.add_node(1, vec![FieldState::from_gcounter(0, &counter)]);

        let snapshot = builder.build();
        let reader = SnapshotReader::from_bytes(&snapshot).unwrap();

        // Should pass validation
        assert!(reader.validate().is_ok());
    }

    #[test]
    fn test_binary_search_lookup() {
        let mut builder = SnapshotBuilder::new();

        // Add nodes in random order
        let nodes = vec![100, 50, 200, 25, 150, 75, 300];
        for &node_num in &nodes {
            let mut counter = GCounter::new();
            counter.inc(test_actor(), node_num);
            builder.add_node(node_num, vec![FieldState::from_gcounter(0, &counter)]);
        }

        let snapshot = builder.build();
        let reader = SnapshotReader::from_bytes(&snapshot).unwrap();

        // Test lookup for each
        for &node_num in &nodes {
            assert!(
                reader.get_node(node_num).is_some(),
                "Failed to find node {}",
                node_num
            );
        }

        // Test non-existent node
        assert!(reader.get_node(999).is_none());
    }

    #[test]
    fn test_field_state_helpers() {
        let counter = GCounter::new();
        let field = FieldState::from_gcounter(5, &counter);
        assert_eq!(field.field_id, 5);
        assert_eq!(field.crdt_kind, CrdtKind::GCOUNTER);

        let pncounter = PNCounter::new();
        let field = FieldState::from_pncounter(10, &pncounter);
        assert_eq!(field.field_id, 10);
        assert_eq!(field.crdt_kind, CrdtKind::PNCOUNTER);

        let reg = LWWReg::new(HLC::now(), test_actor(), vec![1, 2, 3]);
        let field = FieldState::from_lwwreg(15, &reg);
        assert_eq!(field.field_id, 15);
        assert_eq!(field.crdt_kind, CrdtKind::LWW_REG);

        let set: ORSet<SymId> = ORSet::new();
        let field = FieldState::from_orset(20, &set);
        assert_eq!(field.field_id, 20);
        assert_eq!(field.crdt_kind, CrdtKind::ORSET);
    }

    #[test]
    fn test_invalid_snapshot_data() {
        // Too short
        let data = vec![0u8; 10];
        assert!(SnapshotReader::from_bytes(&data).is_err());

        // Invalid magic
        let mut data = vec![0u8; SnapshotHeader::SIZE];
        data[0..4].copy_from_slice(&0xDEADBEEFu32.to_le_bytes());
        assert!(SnapshotReader::from_bytes(&data).is_err());
    }
}
