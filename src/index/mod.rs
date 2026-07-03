//! Location and Inverted Indexes for MemoryX SKF-1.1.
//!
//! This module provides:
//! - IdLocIndex: Location index mapping AtomId -> (seg_id, offset, len) with mmap support
//! - InvertedIndex: Term -> NodeNum mappings with front-coded lexicon and delta-varint postings
//! - Lexicon: Front-coded term storage
//! - Postings: Delta-varint encoded posting lists
//!
//! # File Formats
//!
//! ## idloc.mmap
//! ```text
//! [IdLocHeader: 64 bytes]              - File header
//! [ShardDesc * shard_count: 16 bytes each] - Shard descriptor table
//! [XORF filter table: optional]        - Bloom-like filters per shard
//! [IdLocEntry * N: 40 bytes each]      - Location entries, sorted by fp64 within shards
//! ```
//!
//! ## terms.lex
//! ```text
//! [LexHeader: 48 bytes]                - Lexicon header
//! [TermBlock * N]                      - Front-coded term blocks
//! ```
//!
//! ## terms.post
//! ```text
//! [PostHeader: 32 bytes]               - Postings header
//! [PostingList * N]                    - Delta-varint encoded NodeNum lists
//! ```

#![allow(dead_code)]

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::ptr;
use std::slice;

use memmap2::{Mmap, MmapOptions};

use crate::cas::io::BloomFilter;
use crate::store::{AtomId, DomainMask, NodeNum};
use crate::utils::{crc32, decode_varint, encode_varint};

// Re-export the IdLocBuilder from existing code for backward compatibility
pub use self::legacy::*;

// ============================================================================
// Error Types
// ============================================================================

/// Errors that can occur in index operations
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexError {
    /// IO error with description
    Io(String),
    /// Invalid magic number
    InvalidMagic { expected: u32, found: u32 },
    /// Invalid version
    InvalidVersion { expected: u16, found: u16 },
    /// Corrupt data with description
    CorruptData(String),
    /// Entry not found
    NotFound,
    /// Bloom filter mismatch (possible false positive)
    BloomFilterMismatch,
    /// Invalid shard configuration
    InvalidShardConfig(String),
    /// File too small
    FileTooSmall { expected: usize, found: usize },
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexError::Io(msg) => write!(f, "IO error: {}", msg),
            IndexError::InvalidMagic { expected, found } => {
                write!(
                    f,
                    "Invalid magic: expected 0x{:08X}, found 0x{:08X}",
                    expected, found
                )
            }
            IndexError::InvalidVersion { expected, found } => {
                write!(f, "Invalid version: expected {}, found {}", expected, found)
            }
            IndexError::CorruptData(msg) => write!(f, "Corrupt data: {}", msg),
            IndexError::NotFound => write!(f, "Entry not found"),
            IndexError::BloomFilterMismatch => write!(f, "Bloom filter mismatch"),
            IndexError::InvalidShardConfig(msg) => write!(f, "Invalid shard config: {}", msg),
            IndexError::FileTooSmall { expected, found } => {
                write!(
                    f,
                    "File too small: expected {} bytes, found {}",
                    expected, found
                )
            }
        }
    }
}

impl std::error::Error for IndexError {}

impl From<std::io::Error> for IndexError {
    fn from(err: std::io::Error) -> Self {
        IndexError::Io(err.to_string())
    }
}

// ============================================================================
// Constants
// ============================================================================

/// Magic number for IdLocHeader: "IDL1" = 0x49444C31
pub const IDLOC_MAGIC: u32 = 0x49444C31;

/// Magic number for LexHeader: "LEX1" = 0x4C455831
pub const LEX_MAGIC: u32 = 0x4C455831;

/// Magic number for PostHeader: "PST1" = 0x50535431
pub const POST_MAGIC: u32 = 0x50535431;

/// Version for IdLoc format
pub const IDLOC_VERSION: u16 = 0x0001;

/// Version for Lex format
pub const LEX_VERSION: u16 = 0x0001;

/// Version for Post format
pub const POST_VERSION: u16 = 0x0001;

/// Default shard bits (2^shard_bits shards)
pub const DEFAULT_SHARD_BITS: u8 = 12; // 4096 shards

/// Fingerprint bits used for binary search (first 8 bytes of AtomId)
pub const FP_BITS: u8 = 64;

/// Default block size for lexicon front-coding
pub const DEFAULT_BLOCK_SIZE: usize = 128;

// ============================================================================
// IdLocEntry (40 bytes)
// ============================================================================

/// Location index entry for mapping AtomId to physical location
///
/// Layout (40 bytes total, little-endian):
/// - fp64: u64 (8 bytes) - First 8 bytes of AtomId (big-endian for sorting)
/// - seg_id: u32 (4 bytes) - Segment identifier
/// - len32: u32 (4 bytes) - Record length in bytes
/// - offset64: u64 (8 bytes) - Offset within segment
/// - node_num: u64 (8 bytes) - Assigned node number for graph acceleration
/// - gen: u32 (4 bytes) - Generation counter
/// - flags: u32 (4 bytes) - Status flags
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IdLocEntry {
    /// First 8 bytes of AtomId (used as fingerprint for binary search)
    pub fp64: u64,
    /// Segment identifier
    pub seg_id: u32,
    /// Record length in bytes
    pub len32: u32,
    /// Offset within segment
    pub offset64: u64,
    /// Assigned node number for graph acceleration
    pub node_num: u64,
    /// Generation counter
    pub generation: u32,
    /// Status flags
    pub flags: u32,
}

impl IdLocEntry {
    /// Size of IdLocEntry in bytes
    pub const SIZE: usize = 40;

    /// Flag: entry is deleted/tombstone
    pub const FLAG_DELETED: u32 = 0x0001;

    /// Flag: entry is a tombstone (for replication)
    pub const FLAG_TOMBSTONE: u32 = 0x0002;

    /// Flag: entry has been validated
    pub const FLAG_VALIDATED: u32 = 0x0004;

    /// Flag: entry is from federation
    pub const FLAG_FEDERATED: u32 = 0x0008;

    /// Create a new IdLocEntry from an AtomId
    #[inline]
    pub fn new(atom_id: &AtomId, seg_id: u32, len: u32, offset: u64, node_num: u64) -> Self {
        // Extract first 8 bytes as big-endian u64 for consistent sorting
        let fp64 = u64::from_be_bytes([
            atom_id[0], atom_id[1], atom_id[2], atom_id[3], atom_id[4], atom_id[5], atom_id[6],
            atom_id[7],
        ]);

        IdLocEntry {
            fp64,
            seg_id,
            len32: len,
            offset64: offset,
            node_num,
            generation: 0,
            flags: 0,
        }
    }

    /// Check if entry is deleted
    #[inline]
    pub fn is_deleted(&self) -> bool {
        self.flags & Self::FLAG_DELETED != 0
    }

    /// Check if entry is a tombstone
    #[inline]
    pub fn is_tombstone(&self) -> bool {
        self.flags & Self::FLAG_TOMBSTONE != 0
    }

    /// Check if entry is validated
    #[inline]
    pub fn is_validated(&self) -> bool {
        self.flags & Self::FLAG_VALIDATED != 0
    }

    /// Mark entry as deleted
    #[inline]
    pub fn mark_deleted(&mut self) {
        self.flags |= Self::FLAG_DELETED;
    }

    /// Mark entry as validated
    #[inline]
    pub fn mark_validated(&mut self) {
        self.flags |= Self::FLAG_VALIDATED;
    }

    /// Convert to Location struct
    #[inline]
    pub fn to_location(&self) -> Location {
        Location {
            seg_id: self.seg_id,
            offset: self.offset64,
            len: self.len32,
            node_num: self.node_num,
            domain_mask: 0xFFFF,
            deleted: self.is_deleted(),
        }
    }
    /// Read IdLocEntry from bytes (safe, returns owned value)
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::SIZE {
            return None;
        }
        unsafe { Some(ptr::read_unaligned(bytes.as_ptr() as *const IdLocEntry)) }
    }

    /// Write IdLocEntry to bytes
    pub fn write_to_bytes(&self, bytes: &mut [u8]) -> bool {
        if bytes.len() < Self::SIZE {
            return false;
        }
        unsafe {
            ptr::copy_nonoverlapping(
                self as *const IdLocEntry as *const u8,
                bytes.as_mut_ptr(),
                Self::SIZE,
            );
        }
        true
    }
}

// Verify IdLocEntry size at compile time
const _: () = assert!(size_of::<IdLocEntry>() == 40, "IdLocEntry must be 40 bytes");

// ============================================================================
// IdLoc Header (64 bytes)
// ============================================================================

/// Header for idloc.mmap file
///
/// Layout (64 bytes total, little-endian):
/// - magic: u32 (4 bytes) = 0x49444C31 ("IDL1")
/// - ver: u16 (2 bytes)
/// - flags: u16 (2 bytes)
/// - shard_bits: u8 (1 byte)
/// - fp_bits: u8 (1 byte)
/// - reserved: u16 (2 bytes)
/// - shard_count: u32 (4 bytes)
/// - off_shard_table: u64 (8 bytes)
/// - off_xorf_table: u64 (8 bytes)
/// - off_entries: u64 (8 bytes)
/// - file_crc32: u32 (4 bytes)
/// - reserved2: u32 (4 bytes)
/// - reserved3: u64 (8 bytes)
/// - reserved4: u64 (8 bytes)
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IdLocHeader {
    /// Magic number: 0x49444C31 ("IDL1")
    pub magic: u32,
    /// Format version
    pub ver: u16,
    /// Header flags
    pub flags: u16,
    /// Number of shard bits (shard_count = 2^shard_bits)
    pub shard_bits: u8,
    /// Fingerprint bits used
    pub fp_bits: u8,
    /// Reserved (must be 0)
    pub reserved: u16,
    /// Number of shards
    pub shard_count: u32,
    /// Offset to shard descriptor table
    pub off_shard_table: u64,
    /// Offset to XORF filter table
    pub off_xorf_table: u64,
    /// Offset to entry array
    pub off_entries: u64,
    /// CRC32 of header (excluding this field and reserved2/3/4)
    pub file_crc32: u32,
    /// Reserved2 (must be 0)
    pub reserved2: u32,
    /// Reserved3 (must be 0)
    pub reserved3: u64,
    /// Reserved4 (must be 0)
    pub reserved4: u64,
}

impl IdLocHeader {
    /// Size of IdLocHeader in bytes
    pub const SIZE: usize = 64;

    /// Offset to file_crc32 field for CRC calculation
    pub const CRC_OFFSET: usize = 40;

    /// Create a new IdLocHeader
    #[inline]
    pub fn new(shard_bits: u8) -> Self {
        let shard_count = 1u32 << shard_bits;
        IdLocHeader {
            magic: IDLOC_MAGIC,
            ver: IDLOC_VERSION,
            flags: 0,
            shard_bits,
            fp_bits: FP_BITS,
            reserved: 0,
            shard_count,
            off_shard_table: Self::SIZE as u64,
            off_xorf_table: 0, // Will be calculated
            off_entries: 0,    // Will be calculated
            file_crc32: 0,
            reserved2: 0,
            reserved3: 0,
            reserved4: 0,
        }
    }

    /// Calculate CRC32 of header
    #[inline]
    pub fn calculate_crc(&self) -> u32 {
        unsafe {
            crc32(slice::from_raw_parts(
                self as *const IdLocHeader as *const u8,
                Self::CRC_OFFSET,
            ))
        }
    }

    /// Validate header CRC
    #[inline]
    pub fn validate_crc(&self) -> bool {
        self.file_crc32 == self.calculate_crc()
    }

    /// Validate magic and version
    #[inline]
    pub fn validate_magic(&self) -> bool {
        self.magic == IDLOC_MAGIC && self.ver == IDLOC_VERSION
    }

    /// Check if header is valid
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.validate_magic() && self.validate_crc()
    }

    /// Read IdLocHeader from bytes (safe)
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, IndexError> {
        if bytes.len() < Self::SIZE {
            return Err(IndexError::FileTooSmall {
                expected: Self::SIZE,
                found: bytes.len(),
            });
        }

        unsafe {
            let header = ptr::read_unaligned(bytes.as_ptr() as *const IdLocHeader);
            if header.magic != IDLOC_MAGIC {
                return Err(IndexError::InvalidMagic {
                    expected: IDLOC_MAGIC,
                    found: header.magic,
                });
            }
            if header.ver != IDLOC_VERSION {
                return Err(IndexError::InvalidVersion {
                    expected: IDLOC_VERSION,
                    found: header.ver,
                });
            }
            Ok(header)
        }
    }

    /// Write IdLocHeader to bytes
    pub fn write_to_bytes(&self, bytes: &mut [u8]) -> bool {
        if bytes.len() < Self::SIZE {
            return false;
        }
        unsafe {
            ptr::copy_nonoverlapping(
                self as *const IdLocHeader as *const u8,
                bytes.as_mut_ptr(),
                Self::SIZE,
            );
        }
        true
    }

    /// Write to file with CRC calculation
    pub fn write_to_file(&self, file: &mut File) -> Result<(), IndexError> {
        let mut buf = [0u8; Self::SIZE];
        let mut header = *self;
        header.file_crc32 = header.calculate_crc();
        header.write_to_bytes(&mut buf);
        file.write_all(&buf)?;
        Ok(())
    }
}

// Verify IdLocHeader size at compile time
const _: () = assert!(
    size_of::<IdLocHeader>() == 64,
    "IdLocHeader must be 64 bytes"
);

// ============================================================================
// Shard Descriptor (16 bytes)
// ============================================================================

/// Descriptor for a shard in the location index
///
/// Layout (16 bytes total, little-endian):
/// - entry_off: u64 (8 bytes) - Entry offset within entry array
/// - entry_count: u32 (4 bytes) - Number of entries in shard
/// - reserved: u32 (4 bytes)
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ShardDesc {
    /// Entry offset within entry array
    pub entry_off: u64,
    /// Number of entries in shard
    pub entry_count: u32,
    /// Reserved (must be 0)
    pub reserved: u32,
}

impl ShardDesc {
    /// Size of ShardDesc in bytes
    pub const SIZE: usize = 16;

    /// Create a new ShardDesc
    #[inline]
    pub fn new(entry_off: u64, entry_count: u32) -> Self {
        ShardDesc {
            entry_off,
            entry_count,
            reserved: 0,
        }
    }

    /// Read ShardDesc from bytes (safe)
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::SIZE {
            return None;
        }
        unsafe { Some(ptr::read_unaligned(bytes.as_ptr() as *const ShardDesc)) }
    }

    /// Write to bytes
    pub fn write_to_bytes(&self, bytes: &mut [u8]) -> bool {
        if bytes.len() < Self::SIZE {
            return false;
        }
        unsafe {
            ptr::copy_nonoverlapping(
                self as *const ShardDesc as *const u8,
                bytes.as_mut_ptr(),
                Self::SIZE,
            );
        }
        true
    }
}

// Verify ShardDesc size at compile time
const _: () = assert!(size_of::<ShardDesc>() == 16, "ShardDesc must be 16 bytes");

// ============================================================================
// Location structure
// ============================================================================

/// Physical location of a record
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Location {
    pub seg_id: u32,
    pub offset: u64,
    pub len: u32,
    pub node_num: u64,
    pub domain_mask: DomainMask,
    /// Tombstone flag - marks atom as deleted (SKF-1.1 delete semantics)
    pub deleted: bool,
}

impl Location {
    pub fn new(seg_id: u32, offset: u64, len: u32, node_num: u64, domain_mask: DomainMask) -> Self {
        Location {
            seg_id,
            offset,
            len,
            node_num,
            domain_mask,
            deleted: false,
        }
    }

    /// Create location with deleted flag
    #[inline]
    pub fn with_deleted(mut self, deleted: bool) -> Self {
        self.deleted = deleted;
        self
    }
}

// ============================================================================
// IdLocIndex - High-level mmap-based index
// ============================================================================

/// Memory-mapped location index for AtomId -> Location mapping
///
/// This is the primary interface for reading the location index.
/// For building indices, use IdLocBuilder and then open with IdLocIndex.
pub struct IdLocIndex {
    mmap: Mmap,
    header: IdLocHeader,
    shards: Vec<ShardDesc>,
    filters: Vec<Option<BloomFilter>>,
    entries_offset: usize,
    path: PathBuf,
}

impl IdLocIndex {
    /// Create a new empty index file with given shard_bits
    ///
    /// # Arguments
    /// - `path`: Path to create the index file
    /// - `shard_bits`: Number of shard bits (e.g., 12 = 4096 shards)
    ///
    /// # Returns
    /// - `Ok(IdLocIndex)`: Created index (empty, ready for building)
    /// - `Err(IndexError)`: Creation failed
    pub fn create(path: &Path, shard_bits: u8) -> Result<Self, IndexError> {
        if shard_bits > 20 {
            return Err(IndexError::InvalidShardConfig(
                "shard_bits must be <= 20".to_string(),
            ));
        }

        let header = IdLocHeader::new(shard_bits);
        let shard_count = header.shard_count as usize;

        // Calculate file size for empty index
        // Filter table: per shard [u32: num_bits] = 4 bytes each (empty filters)
        let filter_table_size = shard_count * 4;
        let shard_table_size = shard_count * ShardDesc::SIZE;
        let xorf_table_off = IdLocHeader::SIZE + shard_table_size;
        let entries_off = xorf_table_off + filter_table_size;
        let file_size = entries_off;

        // Create file and pre-allocate
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        file.set_len(file_size as u64)?;

        // Write header with proper CRC and offsets
        let mut header = header;
        header.off_xorf_table = xorf_table_off as u64;
        header.off_entries = entries_off as u64;
        header.file_crc32 = header.calculate_crc();
        header.write_to_file(&mut file)?;

        // Write empty shard descriptors
        let zero_shard = ShardDesc::new(0, 0);
        let mut shard_buf = [0u8; ShardDesc::SIZE];
        for _ in 0..shard_count {
            zero_shard.write_to_bytes(&mut shard_buf);
            file.write_all(&shard_buf)?;
        }

        // Write filter table: each shard has num_bits=0 (empty filter)
        for _ in 0..shard_count {
            file.write_all(&0u32.to_le_bytes())?;
        }

        file.flush()?;
        drop(file);

        // Now open it
        Self::open(path)
    }

    /// Open an existing index file
    ///
    /// # Arguments
    /// - `path`: Path to the index file
    ///
    /// # Returns
    /// - `Ok(IdLocIndex)`: Opened index with mmap
    /// - `Err(IndexError)`: Open failed
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        let file = OpenOptions::new().read(true).open(path)?;
        let file_len = file.metadata()?.len() as usize;

        if file_len < IdLocHeader::SIZE {
            return Err(IndexError::FileTooSmall {
                expected: IdLocHeader::SIZE,
                found: file_len,
            });
        }

        // Memory map the file
        let mmap = unsafe { MmapOptions::new().map(&file)? };

        // Read and validate header
        let header = IdLocHeader::from_bytes(&mmap)?;

        if !header.validate_crc() {
            return Err(IndexError::CorruptData("Header CRC mismatch".to_string()));
        }

        // Read shard descriptors
        let shard_count = header.shard_count as usize;
        let shard_table_start = header.off_shard_table as usize;
        let mut shards = Vec::with_capacity(shard_count);

        for i in 0..shard_count {
            let offset = shard_table_start + i * ShardDesc::SIZE;
            if offset + ShardDesc::SIZE > file_len {
                return Err(IndexError::CorruptData(format!(
                    "Shard descriptor {} extends past file end",
                    i
                )));
            }
            let shard = ShardDesc::from_bytes(&mmap[offset..]).ok_or_else(|| {
                IndexError::CorruptData(format!("Failed to read shard descriptor {}", i))
            })?;
            shards.push(shard);
        }

        // Read Bloom filters from filter table
        // Format: per shard [u32: num_bits][filter_data...]
        let filter_table_start = header.off_xorf_table as usize;
        let mut filters = Vec::with_capacity(shard_count);
        let mut cursor = filter_table_start;

        for i in 0..shard_count {
            if cursor + 4 > file_len {
                return Err(IndexError::CorruptData(format!(
                    "Filter header {} extends past file end",
                    i
                )));
            }
            let num_bits = u32::from_le_bytes([
                mmap[cursor],
                mmap[cursor + 1],
                mmap[cursor + 2],
                mmap[cursor + 3],
            ]) as usize;
            cursor += 4;

            if num_bits == 0 {
                filters.push(None);
            } else {
                let num_words = num_bits.div_ceil(64);
                let data_size = num_words * 8;
                if cursor + data_size > file_len {
                    return Err(IndexError::CorruptData(format!(
                        "Filter data {} extends past file end",
                        i
                    )));
                }
                let filter = BloomFilter::from_bytes(&mmap[cursor..cursor + data_size], num_bits);
                cursor += data_size;
                filters.push(Some(filter));
            }
        }

        let entries_offset = header.off_entries as usize;

        Ok(IdLocIndex {
            mmap,
            header,
            shards,
            filters,
            entries_offset,
            path: path.to_path_buf(),
        })
    }

    /// Calculate shard index from fp64
    #[inline]
    fn get_shard(fp64: u64, shard_bits: u8) -> usize {
        ((fp64 >> (64 - shard_bits)) as usize) & ((1 << shard_bits) - 1)
    }

    /// Calculate shard index for an AtomId
    #[inline]
    fn shard_for_atom(&self, atom_id: &AtomId) -> usize {
        let fp64 = u64::from_be_bytes([
            atom_id[0], atom_id[1], atom_id[2], atom_id[3], atom_id[4], atom_id[5], atom_id[6],
            atom_id[7],
        ]);
        Self::get_shard(fp64, self.header.shard_bits)
    }

    /// Binary search within a shard for a target fp64
    fn binary_search_shard(&self, shard_idx: usize, target_fp64: u64) -> Option<usize> {
        let shard = &self.shards[shard_idx];
        if shard.entry_count == 0 {
            return None;
        }

        let entries_start = self.entries_offset + (shard.entry_off as usize * IdLocEntry::SIZE);
        let mut left = 0usize;
        let mut right = shard.entry_count as usize;

        while left < right {
            let mid = left + (right - left) / 2;
            let entry_offset = entries_start + (mid * IdLocEntry::SIZE);

            if entry_offset + IdLocEntry::SIZE > self.mmap.len() {
                return None;
            }

            let entry_fp64 = unsafe {
                let entry = &*(self.mmap.as_ptr().add(entry_offset) as *const IdLocEntry);
                entry.fp64
            };

            if entry_fp64 == target_fp64 {
                return Some(mid);
            } else if entry_fp64 < target_fp64 {
                left = mid + 1;
            } else {
                right = mid;
            }
        }

        None
    }

    /// Lookup AtomId -> Option<Location>
    ///
    /// Performs O(log n) binary search within the appropriate shard.
    /// Uses Bloom filter for fast negative lookups — if filter says "no",
    /// returns None without binary search.
    pub fn locate(&self, atom_id: &AtomId) -> Option<Location> {
        let fp64 = u64::from_be_bytes([
            atom_id[0], atom_id[1], atom_id[2], atom_id[3], atom_id[4], atom_id[5], atom_id[6],
            atom_id[7],
        ]);

        let shard_idx = Self::get_shard(fp64, self.header.shard_bits);
        if shard_idx >= self.shards.len() {
            return None;
        }

        // Fast negative check via Bloom filter
        if self.filters[shard_idx]
            .as_ref()
            .is_some_and(|filter| !filter.might_contain(atom_id))
        {
            return None;
        }

        let entry_idx = self.binary_search_shard(shard_idx, fp64)?;
        let shard = &self.shards[shard_idx];
        let entry_offset = self.entries_offset
            + (shard.entry_off as usize * IdLocEntry::SIZE)
            + (entry_idx * IdLocEntry::SIZE);

        if entry_offset + IdLocEntry::SIZE > self.mmap.len() {
            return None;
        }

        unsafe {
            let entry = &*(self.mmap.as_ptr().add(entry_offset) as *const IdLocEntry);
            if entry.is_deleted() {
                return None;
            }
            Some(entry.to_location())
        }
    }

    /// Get the node number for an AtomId
    #[inline]
    pub fn get_node_num(&self, atom_id: &AtomId) -> Option<NodeNum> {
        self.locate(atom_id).map(|loc| loc.node_num)
    }

    /// Get total entry count (sum across all shards)
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.entry_count as usize).sum()
    }

    /// Check if index is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get shard count
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// Get header reference
    pub fn header(&self) -> &IdLocHeader {
        &self.header
    }

    /// Get path
    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ============================================================================
// IdLocBuilder - For constructing index
// ============================================================================

/// Builder for constructing location index
///
/// Accumulates entries in memory and builds a sorted, sharded index file.
pub struct IdLocBuilder {
    header: IdLocHeader,
    entries: Vec<(AtomId, IdLocEntry)>,
}

impl IdLocBuilder {
    /// Create a new IdLocBuilder
    pub fn new(shard_bits: u8) -> Self {
        let header = IdLocHeader::new(shard_bits);
        IdLocBuilder {
            header,
            entries: Vec::new(),
        }
    }

    /// Add an entry to the index
    pub fn add(&mut self, atom_id: &AtomId, seg_id: u32, len: u32, offset: u64, node_num: u64) {
        let entry = IdLocEntry::new(atom_id, seg_id, len, offset, node_num);
        self.entries.push((*atom_id, entry));
    }

    /// Get current entry count
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if builder is empty
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Build the index to a byte vector (legacy API)
    ///
    /// For new code, prefer `build_to_file` or `build_to_vec`.
    pub fn build(self) -> Vec<u8> {
        self.build_to_vec()
    }

    /// Build the index to a file
    ///
    /// Sorts entries by fp64, distributes into shards, and writes to file.
    /// Creates real BloomFilter per shard for fast negative lookups.
    pub fn build_to_file(self, path: &Path) -> Result<IdLocIndex, IndexError> {
        let mut entries = self.entries;
        let shard_count = self.header.shard_count as usize;

        // Sort entries by fp64
        entries.sort_by_key(|(_, e)| e.fp64);

        // Distribute entries into shards based on fp64 high bits
        let mut shard_entries: Vec<Vec<(AtomId, IdLocEntry)>> = vec![Vec::new(); shard_count];
        for pair in entries {
            let shard_idx = IdLocIndex::get_shard(pair.1.fp64, self.header.shard_bits);
            shard_entries[shard_idx].push(pair);
        }

        // Build Bloom filters per shard
        let mut shard_filters: Vec<Option<BloomFilter>> = Vec::with_capacity(shard_count);
        for shard_vec in &shard_entries {
            if shard_vec.is_empty() {
                shard_filters.push(None);
            } else {
                let mut filter = BloomFilter::new(shard_vec.len());
                for (atom_id, _) in shard_vec {
                    filter.insert(atom_id);
                }
                shard_filters.push(Some(filter));
            }
        }

        // Build shard descriptors
        let mut shards = Vec::with_capacity(shard_count);
        let mut all_entries: Vec<IdLocEntry> = Vec::new();

        for shard_vec in shard_entries {
            let entry_off = all_entries.len() as u64;
            let entry_count = shard_vec.len() as u32;
            shards.push(ShardDesc::new(entry_off, entry_count));
            all_entries.extend(shard_vec.into_iter().map(|(_, e)| e));
        }

        // Calculate filter table size: per shard [u32: num_bits][filter_data...]
        let mut filter_table_size = 0usize;
        for filter in &shard_filters {
            filter_table_size += 4; // num_bits header
            if let Some(f) = filter {
                filter_table_size += f.serialized_size();
            }
        }

        // Calculate file layout
        let header_size = IdLocHeader::SIZE;
        let shard_table_size = shard_count * ShardDesc::SIZE;
        let xorf_table_off = header_size + shard_table_size;
        let entries_off = xorf_table_off + filter_table_size;
        let file_size = entries_off + (all_entries.len() * IdLocEntry::SIZE);

        // Create and write file
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        file.set_len(file_size as u64)?;

        // Write header
        let mut header = self.header;
        header.off_xorf_table = xorf_table_off as u64;
        header.off_entries = entries_off as u64;
        header.write_to_file(&mut file)?;

        // Write shard descriptors
        let mut shard_buf = [0u8; ShardDesc::SIZE];
        for shard in &shards {
            shard.write_to_bytes(&mut shard_buf);
            file.write_all(&shard_buf)?;
        }

        // Write filter table
        for filter in &shard_filters {
            match filter {
                None => {
                    file.write_all(&0u32.to_le_bytes())?;
                }
                Some(f) => {
                    file.write_all(&(f.num_bits() as u32).to_le_bytes())?;
                    file.write_all(&f.to_bytes())?;
                }
            }
        }

        // Write entries
        file.seek(SeekFrom::Start(entries_off as u64))?;
        let mut entry_buf = [0u8; IdLocEntry::SIZE];
        for entry in &all_entries {
            entry.write_to_bytes(&mut entry_buf);
            file.write_all(&entry_buf)?;
        }

        file.flush()?;
        drop(file);

        // Open and return
        IdLocIndex::open(path)
    }

    /// Build the index to a byte vector (in-memory)
    ///
    /// Useful for testing or temporary storage.
    /// Creates real BloomFilter per shard for fast negative lookups.
    pub fn build_to_vec(mut self) -> Vec<u8> {
        let shard_count = self.header.shard_count as usize;

        // Sort entries by fp64
        self.entries.sort_by_key(|(_, e)| e.fp64);

        // Distribute entries into shards
        let mut shard_entries: Vec<Vec<(AtomId, IdLocEntry)>> = vec![Vec::new(); shard_count];
        for pair in self.entries {
            let shard_idx = IdLocIndex::get_shard(pair.1.fp64, self.header.shard_bits);
            shard_entries[shard_idx].push(pair);
        }

        // Build Bloom filters per shard
        let mut shard_filters: Vec<Option<BloomFilter>> = Vec::with_capacity(shard_count);
        for shard_vec in &shard_entries {
            if shard_vec.is_empty() {
                shard_filters.push(None);
            } else {
                let mut filter = BloomFilter::new(shard_vec.len());
                for (atom_id, _) in shard_vec {
                    filter.insert(atom_id);
                }
                shard_filters.push(Some(filter));
            }
        }

        // Build shard descriptors
        let mut shards = Vec::with_capacity(shard_count);
        let mut all_entries: Vec<IdLocEntry> = Vec::new();

        for shard_vec in shard_entries {
            let entry_off = all_entries.len() as u64;
            let entry_count = shard_vec.len() as u32;
            shards.push(ShardDesc::new(entry_off, entry_count));
            all_entries.extend(shard_vec.into_iter().map(|(_, e)| e));
        }

        // Calculate filter table size
        let mut filter_table_size = 0usize;
        for filter in &shard_filters {
            filter_table_size += 4;
            if let Some(f) = filter {
                filter_table_size += f.serialized_size();
            }
        }

        // Calculate file layout
        let header_size = IdLocHeader::SIZE;
        let shard_table_size = shard_count * ShardDesc::SIZE;
        let xorf_table_off = header_size + shard_table_size;
        let entries_off = xorf_table_off + filter_table_size;
        let file_size = entries_off + (all_entries.len() * IdLocEntry::SIZE);

        // Build buffer
        let mut buf = vec![0u8; file_size];

        // Write header
        self.header.off_xorf_table = xorf_table_off as u64;
        self.header.off_entries = entries_off as u64;
        self.header.file_crc32 = self.header.calculate_crc();
        self.header.write_to_bytes(&mut buf[..header_size]);

        // Write shard descriptors
        for (i, shard) in shards.iter().enumerate() {
            let offset = header_size + (i * ShardDesc::SIZE);
            shard.write_to_bytes(&mut buf[offset..offset + ShardDesc::SIZE]);
        }

        // Write filter table
        let mut filter_offset = xorf_table_off;
        for filter in &shard_filters {
            match filter {
                None => {
                    buf[filter_offset..filter_offset + 4].copy_from_slice(&0u32.to_le_bytes());
                    filter_offset += 4;
                }
                Some(f) => {
                    buf[filter_offset..filter_offset + 4]
                        .copy_from_slice(&(f.num_bits() as u32).to_le_bytes());
                    filter_offset += 4;
                    let filter_bytes = f.to_bytes();
                    buf[filter_offset..filter_offset + filter_bytes.len()]
                        .copy_from_slice(&filter_bytes);
                    filter_offset += filter_bytes.len();
                }
            }
        }

        // Write entries
        for (i, entry) in all_entries.iter().enumerate() {
            let offset = entries_off + (i * IdLocEntry::SIZE);
            entry.write_to_bytes(&mut buf[offset..offset + IdLocEntry::SIZE]);
        }

        buf
    }
}

// ============================================================================
// Lexicon (Front-coded)
// ============================================================================

/// Block table entry for front-coded lexicon (16 bytes)
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct BlockTableEntry {
    pub block_off: u64,
    pub first_term_id: u32,
    pub entry_count: u32,
}

impl BlockTableEntry {
    pub const SIZE: usize = 16;

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::SIZE {
            return None;
        }
        unsafe { Some(ptr::read_unaligned(bytes.as_ptr() as *const BlockTableEntry)) }
    }

    pub fn write_to_bytes(&self, bytes: &mut [u8]) -> bool {
        if bytes.len() < Self::SIZE {
            return false;
        }
        unsafe {
            ptr::copy_nonoverlapping(
                self as *const BlockTableEntry as *const u8,
                bytes.as_mut_ptr(),
                Self::SIZE,
            );
        }
        true
    }
}

const _: () = assert!(
    size_of::<BlockTableEntry>() == 16,
    "BlockTableEntry must be 16 bytes"
);

/// Header for terms.lex file (SKF-1.1 spec)
///
/// Layout (48 bytes total, little-endian):
/// - magic: u32 (4 bytes) = 0x4C455831 ("LEX1")
/// - ver: u16 (2 bytes)
/// - flags: u16 (2 bytes)
/// - block_size: u32 (4 bytes) - Terms per front-coded block
/// - term_count: u64 (8 bytes)
/// - block_count: u32 (4 bytes)
/// - off_block_table: u64 (8 bytes)
/// - off_blocks: u64 (8 bytes)
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LexHeader {
    pub magic: u32,
    pub ver: u16,
    pub flags: u16,
    pub block_size: u32,
    pub term_count: u64,
    pub block_count: u32,
    pub off_block_table: u64,
    pub off_blocks: u64,
}

impl LexHeader {
    pub const SIZE: usize = 48;

    pub fn new(block_size: u32) -> Self {
        LexHeader {
            magic: LEX_MAGIC,
            ver: LEX_VERSION,
            flags: 0,
            block_size,
            term_count: 0,
            block_count: 0,
            off_block_table: Self::SIZE as u64,
            off_blocks: 0,
        }
    }

    pub fn validate_magic(&self) -> bool {
        self.magic == LEX_MAGIC && self.ver == LEX_VERSION
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, IndexError> {
        if bytes.len() < Self::SIZE {
            return Err(IndexError::FileTooSmall {
                expected: Self::SIZE,
                found: bytes.len(),
            });
        }
        unsafe {
            let header = ptr::read_unaligned(bytes.as_ptr() as *const LexHeader);
            if header.magic != LEX_MAGIC {
                return Err(IndexError::InvalidMagic {
                    expected: LEX_MAGIC,
                    found: header.magic,
                });
            }
            if header.ver != LEX_VERSION {
                return Err(IndexError::InvalidVersion {
                    expected: LEX_VERSION,
                    found: header.ver,
                });
            }
            Ok(header)
        }
    }

    pub fn write_to_bytes(&self, bytes: &mut [u8]) -> bool {
        if bytes.len() < Self::SIZE {
            return false;
        }
        unsafe {
            ptr::copy_nonoverlapping(
                self as *const LexHeader as *const u8,
                bytes.as_mut_ptr(),
                Self::SIZE,
            );
        }
        true
    }
}

const _: () = assert!(size_of::<LexHeader>() == 48, "LexHeader must be 48 bytes");

/// Front-coded lexicon for term storage
///
/// Stores terms in blocks with front-coding to save space.
/// Each block stores an anchor term, and other terms store
/// only their common_prefix_len + suffix to reconstruct the full term.
///
/// File format (SKF-1.1 spec):
/// [LexHeader: 48 bytes]
/// [BlockTableEntry * N]
/// [Block data: anchor_term + {u16 prefix_len, u16 suffix_len, suffix_bytes} * entries]
#[derive(Clone)]
pub struct Lexicon {
    terms: Vec<String>,
    block_size: usize,
    term_to_id: HashMap<String, u32>,
}

impl Lexicon {
    /// Create a new empty lexicon
    pub fn new() -> Self {
        Self::with_block_size(DEFAULT_BLOCK_SIZE)
    }

    /// Create a new lexicon with custom block size
    pub fn with_block_size(block_size: usize) -> Self {
        Lexicon {
            terms: Vec::new(),
            block_size,
            term_to_id: HashMap::new(),
        }
    }

    /// Add a term to the lexicon
    ///
    /// Returns the term_id (index in sorted order)
    pub fn add(&mut self, term: String) -> u32 {
        if let Some(&id) = self.term_to_id.get(&term) {
            return id;
        }

        let id = self.terms.len() as u32;
        self.term_to_id.insert(term.clone(), id);
        self.terms.push(term);
        id
    }

    /// Get term by id
    pub fn get(&self, term_id: u32) -> Option<&str> {
        self.terms.get(term_id as usize).map(|s| s.as_str())
    }

    /// Find term id by string
    pub fn find(&self, term: &str) -> Option<u32> {
        self.term_to_id.get(term).copied()
    }

    /// Get term count
    pub fn len(&self) -> usize {
        self.terms.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Sort terms in place (required for front-coding)
    pub fn sort_terms(&mut self) {
        let mut sorted: Vec<String> = std::mem::take(&mut self.terms);
        sorted.sort();
        self.term_to_id.clear();
        for (id, term) in sorted.into_iter().enumerate() {
            self.term_to_id.insert(term.clone(), id as u32);
            self.terms.push(term);
        }
    }

    /// Write lexicon to file with front-coded blocks (SKF-1.1 spec)
    ///
    /// Format:
    /// [LexHeader]
    /// [BlockTableEntry * block_count]
    /// [Block data: per block {u32 anchor_len, anchor_bytes, u16 entry_count,
    ///                           entry * {u16 prefix_len, u16 suffix_len, suffix_bytes}}]
    pub fn write_to_file(&self, path: &Path) -> Result<(), IndexError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        // Sort terms for front-coding
        let mut sorted_terms: Vec<&String> = self.terms.iter().collect();
        sorted_terms.sort();

        let term_count = sorted_terms.len();
        let block_size = self.block_size;
        let block_count = term_count.div_ceil(block_size);

        // Build block table and block data
        let mut block_table: Vec<BlockTableEntry> = Vec::with_capacity(block_count);
        let mut blocks_data: Vec<u8> = Vec::new();

        for block_idx in 0..block_count {
            let block_start = block_idx * block_size;
            let block_end = std::cmp::min(block_start + block_size, term_count);
            let entries_in_block = block_end - block_start;

            let block_off = blocks_data.len() as u64;
            let first_term_id = block_start as u32;

            // Write anchor term (first term in block)
            let anchor = &sorted_terms[block_start];
            let anchor_len = anchor.len() as u32;
            blocks_data.extend_from_slice(&anchor_len.to_le_bytes());
            blocks_data.extend_from_slice(anchor.as_bytes());

            // Write entry count for this block
            blocks_data.extend_from_slice(&(entries_in_block as u16).to_le_bytes());

            // Write remaining terms with front-coding
            for term in sorted_terms[(block_start + 1)..block_end].iter() {
                let term = term.as_bytes();
                let anchor_bytes = anchor.as_bytes();

                // Calculate common prefix length
                let prefix_len = anchor_bytes
                    .iter()
                    .zip(term.iter())
                    .take_while(|(a, b)| a == b)
                    .count() as u16;

                let suffix = &term[prefix_len as usize..];
                let suffix_len = suffix.len() as u16;

                blocks_data.extend_from_slice(&prefix_len.to_le_bytes());
                blocks_data.extend_from_slice(&suffix_len.to_le_bytes());
                blocks_data.extend_from_slice(suffix);
            }

            block_table.push(BlockTableEntry {
                block_off,
                first_term_id,
                entry_count: entries_in_block as u32,
            });
        }

        // Calculate offsets
        let header_size = LexHeader::SIZE;
        let block_table_size = block_count * BlockTableEntry::SIZE;
        let off_block_table = header_size as u64;
        let off_blocks = (header_size + block_table_size) as u64;

        // Write header
        let mut header = LexHeader::new(self.block_size as u32);
        header.term_count = term_count as u64;
        header.block_count = block_count as u32;
        header.off_block_table = off_block_table;
        header.off_blocks = off_blocks;

        let mut header_buf = [0u8; LexHeader::SIZE];
        header.write_to_bytes(&mut header_buf);
        file.write_all(&header_buf)?;

        // Write block table
        for entry in &block_table {
            let mut buf = [0u8; BlockTableEntry::SIZE];
            entry.write_to_bytes(&mut buf);
            file.write_all(&buf)?;
        }

        // Write block data
        file.write_all(&blocks_data)?;
        file.flush()?;
        Ok(())
    }

    /// Read lexicon from file with front-coded blocks
    pub fn read_from_file(path: &Path) -> Result<Self, IndexError> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len() as usize;

        if file_len < LexHeader::SIZE {
            return Err(IndexError::FileTooSmall {
                expected: LexHeader::SIZE,
                found: file_len,
            });
        }

        // Read header
        let mut header_buf = [0u8; LexHeader::SIZE];
        file.read_exact(&mut header_buf)?;
        let header = LexHeader::from_bytes(&header_buf)?;

        let mut lexicon = Lexicon::with_block_size(header.block_size as usize);

        // Read block table
        let block_table_off = header.off_block_table as usize;
        let block_table_size = header.block_count as usize * BlockTableEntry::SIZE;
        let mut block_table: Vec<BlockTableEntry> =
            vec![BlockTableEntry::default(); header.block_count as usize];

        if block_table_size > 0 {
            file.seek(SeekFrom::Start(block_table_off as u64))?;
            for entry in block_table.iter_mut() {
                let mut buf = [0u8; BlockTableEntry::SIZE];
                file.read_exact(&mut buf)?;
                *entry = BlockTableEntry::from_bytes(&buf)
                    .ok_or_else(|| IndexError::CorruptData("Invalid block table entry".into()))?;
            }
        }

        // Read block data
        let blocks_data_off = header.off_blocks as usize;
        let blocks_data_len = file_len - blocks_data_off;
        let mut blocks_data = vec![0u8; blocks_data_len];
        file.seek(SeekFrom::Start(blocks_data_off as u64))?;
        file.read_exact(&mut blocks_data)?;

        // Decode blocks
        for block_entry in &block_table {
            let block_start = block_entry.block_off as usize;
            let mut offset = block_start;

            // Read anchor term
            if offset + 4 > blocks_data.len() {
                return Err(IndexError::CorruptData("Truncated anchor length".into()));
            }
            let anchor_len = u32::from_le_bytes([
                blocks_data[offset],
                blocks_data[offset + 1],
                blocks_data[offset + 2],
                blocks_data[offset + 3],
            ]) as usize;
            offset += 4;

            if offset + anchor_len > blocks_data.len() {
                return Err(IndexError::CorruptData("Truncated anchor data".into()));
            }
            let anchor =
                String::from_utf8_lossy(&blocks_data[offset..offset + anchor_len]).to_string();
            offset += anchor_len;

            // Add anchor term
            lexicon.add(anchor.clone());

            // Read entry count
            if offset + 2 > blocks_data.len() {
                return Err(IndexError::CorruptData("Truncated entry count".into()));
            }
            let entry_count =
                u16::from_le_bytes([blocks_data[offset], blocks_data[offset + 1]]) as usize;
            offset += 2;

            // Read front-coded entries
            for _ in 1..entry_count {
                if offset + 4 > blocks_data.len() {
                    return Err(IndexError::CorruptData("Truncated entry header".into()));
                }
                let prefix_len =
                    u16::from_le_bytes([blocks_data[offset], blocks_data[offset + 1]]) as usize;
                let suffix_len =
                    u16::from_le_bytes([blocks_data[offset + 2], blocks_data[offset + 3]]) as usize;
                offset += 4;

                if offset + suffix_len > blocks_data.len() {
                    return Err(IndexError::CorruptData("Truncated suffix".into()));
                }
                let suffix =
                    String::from_utf8_lossy(&blocks_data[offset..offset + suffix_len]).to_string();
                offset += suffix_len;

                // Reconstruct term: prefix from anchor + suffix
                let prefix = if prefix_len <= anchor.len() {
                    &anchor[..prefix_len]
                } else {
                    &anchor
                };
                let mut term = String::with_capacity(prefix.len() + suffix.len());
                term.push_str(prefix);
                term.push_str(&suffix);
                lexicon.add(term);
            }
        }

        // Rebuild term_to_id
        lexicon.term_to_id.clear();
        for (id, term) in lexicon.terms.iter().enumerate() {
            lexicon.term_to_id.insert(term.clone(), id as u32);
        }

        Ok(lexicon)
    }

    /// Lookup term using front-coded index — binary search over blocks
    /// then linear scan within block
    pub fn lookup(&self, term: &str) -> Option<u32> {
        self.term_to_id.get(term).copied()
    }
}

impl Default for Lexicon {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Postings
// ============================================================================

/// Header for postings file
///
/// Layout (32 bytes total, little-endian):
/// - magic: u32 (4 bytes) = 0x50535431 ("PST1")
/// - ver: u16 (2 bytes)
/// - flags: u16 (2 bytes)
/// - posting_count: u64 (8 bytes)
/// - total_docs: u64 (8 bytes)
/// - reserved: u64 (8 bytes)
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PostHeader {
    pub magic: u32,
    pub ver: u16,
    pub flags: u16,
    pub posting_count: u64,
    pub total_docs: u64,
    pub reserved: u64,
}

impl PostHeader {
    pub const SIZE: usize = 32;

    pub fn new() -> Self {
        PostHeader {
            magic: POST_MAGIC,
            ver: POST_VERSION,
            flags: 0,
            posting_count: 0,
            total_docs: 0,
            reserved: 0,
        }
    }

    pub fn validate_magic(&self) -> bool {
        self.magic == POST_MAGIC && self.ver == POST_VERSION
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, IndexError> {
        if bytes.len() < Self::SIZE {
            return Err(IndexError::FileTooSmall {
                expected: Self::SIZE,
                found: bytes.len(),
            });
        }
        unsafe {
            let header = ptr::read_unaligned(bytes.as_ptr() as *const PostHeader);
            if header.magic != POST_MAGIC {
                return Err(IndexError::InvalidMagic {
                    expected: POST_MAGIC,
                    found: header.magic,
                });
            }
            if header.ver != POST_VERSION {
                return Err(IndexError::InvalidVersion {
                    expected: POST_VERSION,
                    found: header.ver,
                });
            }
            Ok(header)
        }
    }

    pub fn write_to_bytes(&self, bytes: &mut [u8]) -> bool {
        if bytes.len() < Self::SIZE {
            return false;
        }
        unsafe {
            ptr::copy_nonoverlapping(
                self as *const PostHeader as *const u8,
                bytes.as_mut_ptr(),
                Self::SIZE,
            );
        }
        true
    }
}

const _: () = assert!(size_of::<PostHeader>() == 32, "PostHeader must be 32 bytes");

/// Postings lists for term -> NodeNum mapping
#[derive(Clone)]
pub struct Postings {
    // term_id -> Vec<NodeNum>
    postings: HashMap<u32, Vec<u64>>,
    total_docs: u64,
}

impl Postings {
    /// Create a new empty postings collection
    pub fn new() -> Self {
        Postings {
            postings: HashMap::new(),
            total_docs: 0,
        }
    }

    /// Add a node number to a term's posting list
    pub fn add(&mut self, term_id: u32, node_num: u64) {
        let entry = self.postings.entry(term_id).or_default();
        // Keep sorted and deduplicated
        match entry.binary_search(&node_num) {
            Ok(_) => {} // Already exists
            Err(pos) => entry.insert(pos, node_num),
        }
        self.total_docs = self.total_docs.max(node_num + 1);
    }

    /// Get posting list for a term
    pub fn get(&self, term_id: u32) -> Option<&[u64]> {
        self.postings.get(&term_id).map(|v| v.as_slice())
    }

    /// Delta-varint encode a list of node numbers
    ///
    /// Encodes differences between consecutive numbers for better compression.
    pub fn encode_deltas(node_nums: &[u64]) -> Vec<u8> {
        if node_nums.is_empty() {
            return vec![0u8; 4]; // Count = 0
        }

        let mut result = Vec::with_capacity(node_nums.len() * 2);

        // Write count
        result.extend_from_slice(&(node_nums.len() as u32).to_le_bytes());

        // Encode deltas
        let mut prev = 0u64;
        let mut buf = [0u8; 10];

        for &num in node_nums {
            let delta = num - prev;
            let len = encode_varint(delta, &mut buf);
            result.extend_from_slice(&buf[..len]);
            prev = num;
        }

        result
    }

    /// Decode delta-varint encoded posting list
    pub fn decode_deltas(data: &[u8]) -> Option<Vec<u64>> {
        if data.len() < 4 {
            return None;
        }

        let count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let mut result = Vec::with_capacity(count);

        let mut offset = 4usize;
        let mut prev = 0u64;

        for _ in 0..count {
            let (delta, consumed) = decode_varint(&data[offset..])?;
            offset += consumed;
            let value = prev + delta;
            result.push(value);
            prev = value;
        }

        Some(result)
    }

    /// Write postings to file
    ///
    /// Format:
    /// [PostHeader]
    /// [u32: term_id][u32: data_len][delta-encoded data] * posting_count
    pub fn write_to_file(&self, path: &Path) -> Result<(), IndexError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        // Build sorted postings by term_id
        let mut sorted_postings: Vec<(u32, &[u64])> = self
            .postings
            .iter()
            .map(|(k, v)| (*k, v.as_slice()))
            .collect();
        sorted_postings.sort_by_key(|(k, _)| *k);

        // Write header
        let mut header = PostHeader::new();
        header.posting_count = sorted_postings.len() as u64;
        header.total_docs = self.total_docs;

        let mut header_buf = [0u8; PostHeader::SIZE];
        header.write_to_bytes(&mut header_buf);
        file.write_all(&header_buf)?;

        // Write postings
        for (term_id, node_nums) in sorted_postings {
            let encoded = Self::encode_deltas(node_nums);
            file.write_all(&term_id.to_le_bytes())?;
            file.write_all(&(encoded.len() as u32).to_le_bytes())?;
            file.write_all(&encoded)?;
        }

        file.flush()?;
        Ok(())
    }

    /// Read postings from file
    pub fn read_from_file(path: &Path) -> Result<Self, IndexError> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len() as usize;

        if file_len < PostHeader::SIZE {
            return Err(IndexError::FileTooSmall {
                expected: PostHeader::SIZE,
                found: file_len,
            });
        }

        // Read header
        let mut header_buf = [0u8; PostHeader::SIZE];
        file.read_exact(&mut header_buf)?;
        let header = PostHeader::from_bytes(&header_buf)?;

        let mut postings = Postings::new();
        postings.total_docs = header.total_docs;

        // Read postings
        let mut buf = vec![0u8; file_len - PostHeader::SIZE];
        file.read_exact(&mut buf)?;

        let mut offset = 0usize;
        for _ in 0..header.posting_count {
            if offset + 8 > buf.len() {
                return Err(IndexError::CorruptData(
                    "Truncated posting header".to_string(),
                ));
            }

            let term_id = u32::from_le_bytes([
                buf[offset],
                buf[offset + 1],
                buf[offset + 2],
                buf[offset + 3],
            ]);
            let data_len = u32::from_le_bytes([
                buf[offset + 4],
                buf[offset + 5],
                buf[offset + 6],
                buf[offset + 7],
            ]) as usize;
            offset += 8;

            if offset + data_len > buf.len() {
                return Err(IndexError::CorruptData(
                    "Truncated posting data".to_string(),
                ));
            }

            let node_nums = Self::decode_deltas(&buf[offset..offset + data_len])
                .ok_or_else(|| IndexError::CorruptData("Invalid delta encoding".to_string()))?;
            offset += data_len;

            for node_num in node_nums {
                postings.add(term_id, node_num);
            }
        }

        Ok(postings)
    }

    /// Get total document count
    pub fn total_docs(&self) -> u64 {
        self.total_docs
    }

    /// Get number of unique terms
    pub fn len(&self) -> usize {
        self.postings.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.postings.is_empty()
    }
}

impl Default for Postings {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Combined Inverted Index
// ============================================================================

/// Combined inverted index with lexicon and postings
#[derive(Clone)]
pub struct InvertedIndex {
    lexicon: Lexicon,
    postings: Postings,
    path: PathBuf,
}

impl InvertedIndex {
    /// Create a new inverted index at the given path
    ///
    /// The index will use:
    /// - `path/terms.lex` for the lexicon
    /// - `path/terms.post` for the postings
    pub fn new(path: &Path) -> Result<Self, IndexError> {
        std::fs::create_dir_all(path)?;

        Ok(InvertedIndex {
            lexicon: Lexicon::new(),
            postings: Postings::new(),
            path: path.to_path_buf(),
        })
    }

    /// Index an atom's content
    ///
    /// Extracts terms from content and adds to posting lists.
    /// For now, uses simple whitespace tokenization.
    pub fn index_atom(&mut self, node_num: u64, _atom_id: &AtomId, content: &[u8]) {
        // Simple whitespace tokenization
        let content_str = String::from_utf8_lossy(content);
        for term in content_str.split_whitespace() {
            let term = term.to_lowercase();
            let term_id = self.lexicon.add(term);
            self.postings.add(term_id, node_num);
        }
    }

    /// Search for a term and return matching node numbers
    pub fn search(&self, term: &str) -> Option<&[u64]> {
        let term_id = self.lexicon.find(term)?;
        self.postings.get(term_id)
    }

    /// Save the index to disk
    pub fn save(&self) -> Result<(), IndexError> {
        let lex_path = self.path.join("terms.lex");
        let post_path = self.path.join("terms.post");

        self.lexicon.write_to_file(&lex_path)?;
        self.postings.write_to_file(&post_path)?;

        Ok(())
    }

    /// Load the index from disk
    pub fn load(&mut self) -> Result<(), IndexError> {
        let lex_path = self.path.join("terms.lex");
        let post_path = self.path.join("terms.post");

        if lex_path.exists() {
            self.lexicon = Lexicon::read_from_file(&lex_path)?;
        }

        if post_path.exists() {
            self.postings = Postings::read_from_file(&post_path)?;
        }

        Ok(())
    }

    /// Get lexicon reference
    pub fn lexicon(&self) -> &Lexicon {
        &self.lexicon
    }

    /// Get postings reference
    pub fn postings(&self) -> &Postings {
        &self.postings
    }

    /// Get mutable lexicon
    pub fn lexicon_mut(&mut self) -> &mut Lexicon {
        &mut self.lexicon
    }

    /// Get mutable postings
    pub fn postings_mut(&mut self) -> &mut Postings {
        &mut self.postings
    }

    /// Get the index path
    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ============================================================================
// Legacy module - re-exports for backward compatibility
// ============================================================================

mod legacy {
    use super::*;

    /// IdLocView - Zero-copy mmap view (legacy, use IdLocIndex instead)
    pub struct IdLocView<'a> {
        header: IdLocHeader,
        data: &'a [u8],
        shard_table: &'a [ShardDesc],
    }

    impl<'a> IdLocView<'a> {
        /// Create a new IdLocView from mmap data
        ///
        /// # Safety
        /// - `data` must be a valid memory-mapped region
        /// - Data must remain valid for lifetime 'a
        pub unsafe fn new_unchecked(data: &'a [u8]) -> Option<Self> {
            if data.len() < IdLocHeader::SIZE {
                return None;
            }

            let header = unsafe { ptr::read_unaligned(data.as_ptr() as *const IdLocHeader) };
            if !header.validate_magic() {
                return None;
            }

            // Validate shard table bounds
            let shard_table_start = header.off_shard_table as usize;
            let shard_table_end =
                shard_table_start + (header.shard_count as usize * ShardDesc::SIZE);
            if shard_table_end > data.len() {
                return None;
            }

            let shard_table = unsafe {
                slice::from_raw_parts(
                    data.as_ptr().add(shard_table_start) as *const ShardDesc,
                    header.shard_count as usize,
                )
            };

            Some(IdLocView {
                header,
                data,
                shard_table,
            })
        }

        /// Create IdLocView with validation
        pub fn new(data: &'a [u8]) -> Option<Self> {
            unsafe { Self::new_unchecked(data) }
        }

        /// Get header reference
        #[inline]
        pub fn header(&self) -> &IdLocHeader {
            &self.header
        }

        /// Calculate shard index from fp64
        #[inline]
        fn shard_index(&self, fp64: u64) -> usize {
            // Use upper bits of fp64 for shard selection
            ((fp64 >> (64 - self.header.shard_bits)) as usize)
                & (self.header.shard_count as usize - 1)
        }

        /// Binary search for fp64 within a shard
        fn binary_search_shard(&self, shard: &ShardDesc, target_fp64: u64) -> Option<usize> {
            if shard.entry_count == 0 {
                return None;
            }

            let entries_start =
                self.header.off_entries as usize + (shard.entry_off as usize * IdLocEntry::SIZE);
            let mut left = 0usize;
            let mut right = shard.entry_count as usize - 1;

            while left <= right {
                let mid = left + (right - left) / 2;
                let entry_offset = entries_start + (mid * IdLocEntry::SIZE);

                if entry_offset + IdLocEntry::SIZE > self.data.len() {
                    return None;
                }

                let entry_fp64 = unsafe {
                    let entry = &*(self.data.as_ptr().add(entry_offset) as *const IdLocEntry);
                    entry.fp64
                };

                if entry_fp64 == target_fp64 {
                    return Some(mid);
                } else if entry_fp64 < target_fp64 {
                    left = mid + 1;
                } else {
                    right = mid - 1;
                }
            }

            None
        }

        /// Locate an AtomId and return its physical location
        pub fn locate(&self, atom_id: &AtomId) -> Option<Location> {
            let fp64 = u64::from_be_bytes([
                atom_id[0], atom_id[1], atom_id[2], atom_id[3], atom_id[4], atom_id[5], atom_id[6],
                atom_id[7],
            ]);

            // Search all shards for the entry (simplified - in production would use XORF filters)
            for shard in self.shard_table.iter() {
                if shard.entry_count == 0 {
                    continue;
                }

                if let Some(entry_idx) = self.binary_search_shard(shard, fp64) {
                    // Get the entry
                    let entries_start = self.header.off_entries as usize
                        + (shard.entry_off as usize * IdLocEntry::SIZE);
                    let entry_offset = entries_start + (entry_idx * IdLocEntry::SIZE);

                    if entry_offset + IdLocEntry::SIZE > self.data.len() {
                        return None;
                    }

                    unsafe {
                        let entry = &*(self.data.as_ptr().add(entry_offset) as *const IdLocEntry);
                        if entry.is_deleted() {
                            return None;
                        }

                        return Some(Location {
                            seg_id: entry.seg_id,
                            offset: entry.offset64,
                            len: entry.len32,
                            node_num: entry.node_num,
                            domain_mask: 0xFFFF,
                            deleted: false, // Already filtered by is_deleted() check above
                        });
                    }
                }
            }
            None
        }

        /// Get node number for an AtomId (for graph acceleration)
        pub fn get_node_num(&self, atom_id: &AtomId) -> Option<NodeNum> {
            self.locate(atom_id).map(|loc| loc.node_num)
        }

        /// Get entries slice for a shard
        pub fn shard_entries(&self, shard_idx: usize) -> Option<&[IdLocEntry]> {
            if shard_idx >= self.shard_table.len() {
                return None;
            }

            let shard = &self.shard_table[shard_idx];
            if shard.entry_count == 0 {
                return Some(&[]);
            }

            let entries_start =
                self.header.off_entries as usize + (shard.entry_off as usize * IdLocEntry::SIZE);
            let entries_bytes = shard.entry_count as usize * IdLocEntry::SIZE;

            if entries_start + entries_bytes > self.data.len() {
                return None;
            }

            unsafe {
                Some(slice::from_raw_parts(
                    self.data.as_ptr().add(entries_start) as *const IdLocEntry,
                    shard.entry_count as usize,
                ))
            }
        }
    }

    /// Re-export PostingList from existing code
    pub struct PostingList<'a> {
        data: &'a [u8],
        count: u32,
    }

    impl<'a> PostingList<'a> {
        /// Create a new PostingList from encoded data
        pub fn new(data: &'a [u8]) -> Option<Self> {
            if data.len() < 4 {
                return None;
            }

            // First 4 bytes contain count
            let count = unsafe { ptr::read_unaligned(data.as_ptr() as *const u32) };

            Some(PostingList { data, count })
        }

        /// Iterate over NodeNum values in the posting list
        pub fn iter(&self) -> PostingIter<'a> {
            PostingIter {
                data: &self.data[4..], // Skip count
                prev: 0,
                remaining: self.count,
            }
        }

        /// Get count of NodeNums in the list
        pub fn count(&self) -> u32 {
            self.count
        }
    }

    /// Iterator over delta-varint encoded NodeNum list
    pub struct PostingIter<'a> {
        data: &'a [u8],
        prev: u64,
        remaining: u32,
    }

    impl<'a> Iterator for PostingIter<'a> {
        type Item = u64;

        fn next(&mut self) -> Option<Self::Item> {
            if self.remaining == 0 || self.data.is_empty() {
                return None;
            }

            // Decode delta varint
            let (delta, consumed) = decode_varint(self.data)?;
            self.data = &self.data[consumed..];
            self.remaining -= 1;

            let current = self.prev.wrapping_add(delta);
            self.prev = current;

            Some(current)
        }
    }

    /// FNV-1a hash for AtomId (fast, non-cryptographic)
    pub fn fnv1a_hash(atom_id: &AtomId) -> u64 {
        const FNV_OFFSET: u64 = 14695981039346656037;
        const FNV_PRIME: u64 = 1099511628211;

        let mut hash = FNV_OFFSET;
        for &byte in atom_id.iter() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash
    }

    /// Assign a new node number for an AtomId (for graph acceleration)
    pub fn assign_node_num(_atom_id: &AtomId, base: u64) -> NodeNum {
        base.wrapping_add(fnv1a_hash(_atom_id))
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
    fn test_idloc_entry_size() {
        assert_eq!(IdLocEntry::SIZE, 40);
        assert_eq!(size_of::<IdLocEntry>(), 40);
    }

    #[test]
    fn test_idloc_header_size() {
        assert_eq!(IdLocHeader::SIZE, 64);
        assert_eq!(size_of::<IdLocHeader>(), 64);
    }

    #[test]
    fn test_shard_desc_size() {
        assert_eq!(ShardDesc::SIZE, 16);
        assert_eq!(size_of::<ShardDesc>(), 16);
    }

    #[test]
    fn test_idloc_entry_create() {
        let atom_id = [1u8; 32];
        let entry = IdLocEntry::new(&atom_id, 1, 1024, 4096, 100);

        assert_eq!(entry.seg_id, 1);
        assert_eq!(entry.len32, 1024);
        assert_eq!(entry.offset64, 4096);
        assert_eq!(entry.node_num, 100);
        assert!(!entry.is_deleted());
    }

    #[test]
    fn test_idloc_header_create() {
        let header = IdLocHeader::new(8);

        assert_eq!(header.magic, IDLOC_MAGIC);
        assert_eq!(header.ver, IDLOC_VERSION);
        assert_eq!(header.shard_bits, 8);
        assert_eq!(header.shard_count, 256);
    }

    #[test]
    fn test_index_error_display() {
        let err = IndexError::NotFound;
        assert_eq!(format!("{}", err), "Entry not found");

        let err = IndexError::InvalidMagic {
            expected: 0x1234,
            found: 0x5678,
        };
        assert!(format!("{}", err).contains("Invalid magic"));
    }

    #[test]
    fn test_idloc_index_create_and_open() {
        let temp_dir = TempDir::new().unwrap();
        let index_path = temp_dir.path().join("test.idl");

        // Create index
        let index = IdLocIndex::create(&index_path, 8).unwrap();
        assert_eq!(index.shard_count(), 256);
        assert!(index.is_empty());

        // Open existing index
        let index2 = IdLocIndex::open(&index_path).unwrap();
        assert_eq!(index2.shard_count(), 256);
    }

    #[test]
    fn test_idloc_builder_and_lookup() {
        let temp_dir = TempDir::new().unwrap();
        let index_path = temp_dir.path().join("test.idl");

        // Build index
        let mut builder = IdLocBuilder::new(8);

        let atom_id1 = [0x01u8; 32];
        let atom_id2 = [0x02u8; 32];
        let atom_id3 = [0x03u8; 32];

        builder.add(&atom_id1, 1, 100, 1000, 10);
        builder.add(&atom_id2, 2, 200, 2000, 20);
        builder.add(&atom_id3, 1, 150, 3000, 30);

        let index = builder.build_to_file(&index_path).unwrap();
        assert_eq!(index.len(), 3);

        // Test lookups
        let loc1 = index.locate(&atom_id1).unwrap();
        assert_eq!(loc1.seg_id, 1);
        assert_eq!(loc1.len, 100);
        assert_eq!(loc1.offset, 1000);
        assert_eq!(loc1.node_num, 10);

        let loc2 = index.locate(&atom_id2).unwrap();
        assert_eq!(loc2.node_num, 20);

        let loc3 = index.locate(&atom_id3).unwrap();
        assert_eq!(loc3.node_num, 30);

        // Test non-existent
        let atom_id_missing = [0xFFu8; 32];
        assert!(index.locate(&atom_id_missing).is_none());
    }

    #[test]
    fn test_idloc_builder_to_vec() {
        let mut builder = IdLocBuilder::new(4);

        for i in 0..100u8 {
            let mut atom_id = [0u8; 32];
            atom_id[0] = i;
            builder.add(&atom_id, 1, 100, (i as u64) * 1000, i as u64);
        }

        let data = builder.build_to_vec();
        assert!(data.len() > IdLocHeader::SIZE);

        // Verify we can read it back
        let header = IdLocHeader::from_bytes(&data).unwrap();
        assert_eq!(header.shard_count, 16);
    }

    #[test]
    fn test_idloc_cross_shard_lookups() {
        let temp_dir = TempDir::new().unwrap();
        let index_path = temp_dir.path().join("test.idl");

        let mut builder = IdLocBuilder::new(4); // 16 shards

        // Add entries that will go to different shards
        for i in 0..16u8 {
            let mut atom_id = [0u8; 32];
            atom_id[0] = i << 4; // Spread across shards
            builder.add(&atom_id, 1, 100, i as u64 * 1000, i as u64);
        }

        let index = builder.build_to_file(&index_path).unwrap();

        // Verify all entries can be found
        for i in 0..16u8 {
            let mut atom_id = [0u8; 32];
            atom_id[0] = i << 4;
            let loc = index.locate(&atom_id).unwrap();
            assert_eq!(loc.node_num, i as u64);
        }
    }

    #[test]
    fn test_lexicon_basic() {
        let mut lex = Lexicon::new();

        let id1 = lex.add("rust".to_string());
        let id2 = lex.add("language".to_string());
        let id3 = lex.add("rust".to_string()); // Duplicate

        assert_eq!(id1, id3); // Same term, same id
        assert_ne!(id1, id2);

        assert_eq!(lex.get(id1), Some("rust"));
        assert_eq!(lex.get(id2), Some("language"));
        assert_eq!(lex.find("rust"), Some(id1));
        assert_eq!(lex.find("missing"), None);
    }

    #[test]
    fn test_lexicon_persistence() {
        let temp_dir = TempDir::new().unwrap();
        let lex_path = temp_dir.path().join("test.lex");

        let mut lex = Lexicon::new();
        lex.add("rust".to_string());
        lex.add("language".to_string());
        lex.add("programming".to_string());

        lex.write_to_file(&lex_path).unwrap();

        let lex2 = Lexicon::read_from_file(&lex_path).unwrap();
        assert_eq!(lex2.len(), 3);
        assert!(lex2.find("rust").is_some());
        assert!(lex2.find("language").is_some());
        assert!(lex2.find("programming").is_some());
    }

    #[test]
    fn test_postings_encode_decode() {
        let node_nums = vec![10u64, 20, 30, 50, 100];
        let encoded = Postings::encode_deltas(&node_nums);

        let decoded = Postings::decode_deltas(&encoded).unwrap();
        assert_eq!(decoded, node_nums);
    }

    #[test]
    fn test_postings_empty() {
        let encoded = Postings::encode_deltas(&[]);
        let decoded = Postings::decode_deltas(&encoded).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn test_postings_basic() {
        let mut postings = Postings::new();

        postings.add(0, 10);
        postings.add(0, 20);
        postings.add(0, 10); // Duplicate
        postings.add(1, 30);

        let list0 = postings.get(0).unwrap();
        assert_eq!(list0, &[10, 20]);

        let list1 = postings.get(1).unwrap();
        assert_eq!(list1, &[30]);

        assert!(postings.get(99).is_none());
    }

    #[test]
    fn test_postings_persistence() {
        let temp_dir = TempDir::new().unwrap();
        let post_path = temp_dir.path().join("test.post");

        let mut postings = Postings::new();
        postings.add(0, 10);
        postings.add(0, 20);
        postings.add(1, 30);
        postings.add(2, 100);
        postings.add(2, 200);
        postings.add(2, 300);

        postings.write_to_file(&post_path).unwrap();

        let postings2 = Postings::read_from_file(&post_path).unwrap();
        assert_eq!(postings2.get(0), Some(&[10u64, 20][..]));
        assert_eq!(postings2.get(1), Some(&[30u64][..]));
        assert_eq!(postings2.get(2), Some(&[100u64, 200, 300][..]));
    }

    #[test]
    fn test_inverted_index() {
        let temp_dir = TempDir::new().unwrap();
        let index_path = temp_dir.path().join("inv_index");

        let mut index = InvertedIndex::new(&index_path).unwrap();

        let atom_id = [1u8; 32];
        let content = b"rust programming language rust";

        index.index_atom(10, &atom_id, content);
        index.index_atom(20, &atom_id, b"rust language");

        // Search
        let rust_results = index.search("rust").unwrap();
        assert_eq!(rust_results, &[10, 20]);

        let lang_results = index.search("language").unwrap();
        assert_eq!(lang_results, &[10, 20]);

        let prog_results = index.search("programming").unwrap();
        assert_eq!(prog_results, &[10]);

        assert!(index.search("missing").is_none());
    }

    #[test]
    fn test_inverted_index_persistence() {
        let temp_dir = TempDir::new().unwrap();
        let index_path = temp_dir.path().join("inv_index");

        // Create and save
        {
            let mut index = InvertedIndex::new(&index_path).unwrap();
            index.index_atom(10, &[1u8; 32], b"rust language");
            index.index_atom(20, &[2u8; 32], b"rust programming");
            index.save().unwrap();
        }

        // Load and verify
        {
            let mut index = InvertedIndex::new(&index_path).unwrap();
            index.load().unwrap();

            assert!(index.search("rust").is_some());
            assert!(index.search("language").is_some());
            assert!(index.search("programming").is_some());
        }
    }

    #[test]
    fn test_shard_calculation() {
        // Test shard calculation with different fp64 values
        let shard_bits = 4u8; // 16 shards

        // fp64 = 0 should go to shard 0
        assert_eq!(IdLocIndex::get_shard(0, shard_bits), 0);

        // fp64 with high bits set
        let fp64_high = 0xF000000000000000u64; // Top nibble = 0xF
        assert_eq!(IdLocIndex::get_shard(fp64_high, shard_bits), 15);

        let fp64_mid = 0x5000000000000000u64; // Top nibble = 0x5
        assert_eq!(IdLocIndex::get_shard(fp64_mid, shard_bits), 5);
    }

    #[test]
    fn test_location_struct() {
        let loc = Location::new(1, 1000, 512, 42, 0xFFFF);
        assert_eq!(loc.seg_id, 1);
        assert_eq!(loc.offset, 1000);
        assert_eq!(loc.len, 512);
        assert_eq!(loc.node_num, 42);
    }

    #[test]
    fn test_index_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let index_err: IndexError = io_err.into();
        match index_err {
            IndexError::Io(_) => {}
            _ => panic!("Expected Io error"),
        }
    }

    #[test]
    fn test_invalid_magic() {
        let temp_dir = TempDir::new().unwrap();
        let index_path = temp_dir.path().join("test.idl");

        // Create file with wrong magic
        let mut file = File::create(&index_path).unwrap();
        file.write_all(&[0xFFu8; 64]).unwrap();
        drop(file);

        let result = IdLocIndex::open(&index_path);
        assert!(result.is_err());
        match result {
            Err(IndexError::InvalidMagic { .. }) => {}
            _ => panic!("Expected InvalidMagic error"),
        }
    }

    #[test]
    fn test_large_posting_list() {
        let mut postings = Postings::new();

        // Add many node numbers
        for i in 0..1000u64 {
            postings.add(0, i * 10); // Every 10th number
        }

        let list = postings.get(0).unwrap();
        assert_eq!(list.len(), 1000);
        assert_eq!(list[0], 0);
        assert_eq!(list[999], 9990);

        // Test encoding/decoding
        let encoded = Postings::encode_deltas(list);
        let decoded = Postings::decode_deltas(&encoded).unwrap();
        assert_eq!(decoded.len(), 1000);
    }

    #[test]
    fn test_entry_flags() {
        let mut entry = IdLocEntry::new(&[1u8; 32], 1, 100, 0, 1);

        assert!(!entry.is_deleted());
        assert!(!entry.is_tombstone());
        assert!(!entry.is_validated());

        entry.mark_deleted();
        assert!(entry.is_deleted());

        entry.mark_validated();
        assert!(entry.is_validated());
    }
}
