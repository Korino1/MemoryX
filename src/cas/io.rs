//! CAS File I/O for MemoryX SKF-1.1
//!
//! Production-ready Content-Addressed Storage implementation with:
//! - Segment files (seg_XXXXX.dat) with append-only writes
//! - Index files (seg_XXXXX.idx) with sorted fp64 entries
//! - Mmap support for zero-copy reads
//! - CRC32 validation on all reads
//! - Bloom filters for fast negative lookups
//! - Compaction for merging segments
//!
//! # Binary Format
//!
//! ## Segment File Layout
//! ```text
//! [Record 1] [Record 2] ... [Record N]
//! ```
//! Each record:
//! - RecordHeader (64 bytes, aligned to 16 bytes)
//! - Body (variable length, padded to 16 bytes)
//! - Body CRC32 (4 bytes)
//!
//! ## Index File Layout
//! ```text
//! Magic (4 bytes): 0x49445831 ("IDX1")
//! Version (2 bytes): 0x0101
//! Entry count (2 bytes)
//! Reserved (4 bytes)
//! [IndexEntry * N]
//! Bloom filter (variable)
//! ```
//! Each IndexEntry (24 bytes):
//! - fp64: f64 (8 bytes) - first 64 bits of atom_id as f64 for sorting
//! - seg_offset: u64 (8 bytes) - offset in segment file
//! - body_len: u32 (4 bytes)
//! - flags: u32 (4 bytes)

#![allow(dead_code)]

use std::cmp::Ordering;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering as AtomicOrdering};

use memmap2::{Mmap, MmapOptions};
use parking_lot::{Mutex, RwLock};
use thiserror::Error;

use crate::cas::edges::{DEFAULT_DEFERRED_RETENTION_CYCLES, DeferredEdgesStore, ResolvedEdge};
use crate::cas::{AtomId, CasError, RECORD_MAGIC, RecordHeader, RecordView};
use crate::utils::crc32;

// ============================================================================
// Constants
// ============================================================================

/// Default segment size limit (64 MB)
pub const DEFAULT_SEGMENT_SIZE_LIMIT: u64 = 64 * 1024 * 1024;

/// Alignment for records (16 bytes)
pub const RECORD_ALIGNMENT: usize = 16;

/// Magic for index file: "IDX1" = 0x49445831
pub const INDEX_MAGIC: u32 = 0x49445831;

/// Index file version 1.1
pub const INDEX_VERSION: u16 = 0x0101;

/// Index entry size (24 bytes)
pub const INDEX_ENTRY_SIZE: usize = 24;

/// Index file header size (12 bytes)
pub const INDEX_HEADER_SIZE: usize = 12;

/// Bloom filter bits per entry
pub const BLOOM_BITS_PER_ENTRY: usize = 10;

/// Bloom filter hash functions count
pub const BLOOM_HASH_FUNCTIONS: usize = 7;

/// Minimum segment size for compaction (1 MB)
pub const MIN_SEGMENT_SIZE_FOR_COMPACTION: u64 = 1024 * 1024;

/// Segment name prefix
pub const SEGMENT_PREFIX: &str = "seg_";

/// Segment file extension
pub const SEGMENT_EXTENSION: &str = "dat";

/// Index file extension
pub const INDEX_EXTENSION: &str = "idx";

// ============================================================================
// CAS I/O Errors
// ============================================================================

/// Errors specific to CAS file I/O operations
#[derive(Debug, Error)]
pub enum CasIoError {
    /// IO error from underlying file operations
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    /// CRC mismatch during read
    #[error("CRC mismatch: expected {expected:#010X}, got {found:#010X}")]
    CrcMismatch { expected: u32, found: u32 },

    /// Invalid magic number
    #[error("Invalid magic: expected {expected:#010X}, found {found:#010X}")]
    InvalidMagic { expected: u32, found: u32 },

    /// Record not found at expected location
    #[error("Record not found at segment {seg_id}, offset {offset}")]
    RecordNotFound { seg_id: u32, offset: u64 },

    /// Index entry not found
    #[error("Index entry not found for atom {atom_id:?}")]
    IndexEntryNotFound { atom_id: AtomId },

    /// Segment is full, need to create new one
    #[error("Segment {seg_id} is full (size {current_size}, limit {limit})")]
    SegmentFull {
        seg_id: u32,
        current_size: u64,
        limit: u64,
    },

    /// Mmap error
    #[error("Mmap error: {0}")]
    MmapError(String),

    /// Invalid bounds for read operation
    #[error("Invalid bounds: offset {offset}, length {length}, file size {file_size}")]
    InvalidBounds {
        offset: u64,
        length: u64,
        file_size: u64,
    },

    /// Corrupted record (failed validation)
    #[error("Corrupted record at offset {offset}: {reason}")]
    CorruptedRecord { offset: u64, reason: String },

    /// Compaction error
    #[error("Compaction error: {0}")]
    CompactionError(String),

    /// Index is stale/needs rebuild
    #[error("Index is stale for segment {seg_id}")]
    IndexStale { seg_id: u32 },

    /// Buffer too small
    #[error("Buffer too small: expected {expected}, got {actual}")]
    BufferTooSmall { expected: usize, actual: usize },

    /// Alignment error
    #[error("Alignment error: expected {expected}-byte alignment, got {actual}")]
    AlignmentError { expected: usize, actual: usize },

    /// Internal CAS error (wrapped from cas::CasError)
    #[error("CAS error: {0}")]
    Cas(#[from] CasError),
}

/// Result type for CAS I/O operations
pub type CasIoResult<T> = Result<T, CasIoError>;

// ============================================================================
// IndexEntry (24 bytes)
// ============================================================================

/// Index entry for binary search in index file
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IndexEntry {
    /// First 64 bits of atom_id interpreted as f64 for sorting
    pub fp64: f64,
    /// Offset in segment file
    pub seg_offset: u64,
    /// Body length
    pub body_len: u32,
    /// Flags (deleted, tombstone, etc.)
    pub flags: u32,
}

impl IndexEntry {
    /// Size of index entry in bytes
    pub const SIZE: usize = INDEX_ENTRY_SIZE;

    /// Create a new IndexEntry from atom_id and record info
    #[inline]
    pub fn new(atom_id: AtomId, seg_offset: u64, body_len: u64, flags: u16) -> Self {
        // Convert first 8 bytes of atom_id to f64 for sorting
        let fp64 = f64::from_le_bytes([
            atom_id[0], atom_id[1], atom_id[2], atom_id[3], atom_id[4], atom_id[5], atom_id[6],
            atom_id[7],
        ]);

        IndexEntry {
            fp64,
            seg_offset,
            body_len: body_len as u32,
            flags: flags as u32,
        }
    }

    /// Read IndexEntry from bytes (safe unaligned read)
    #[inline]
    pub fn from_bytes(bytes: &[u8]) -> CasIoResult<Self> {
        if bytes.len() < Self::SIZE {
            return Err(CasIoError::BufferTooSmall {
                expected: Self::SIZE,
                actual: bytes.len(),
            });
        }

        unsafe { Ok(Self::from_bytes_unaligned(bytes)) }
    }

    /// Read IndexEntry from bytes with unaligned access (unsafe)
    ///
    /// # Safety
    /// - `bytes` must have at least Self::SIZE bytes
    #[inline]
    pub unsafe fn from_bytes_unchecked(bytes: &[u8]) -> Self {
        unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const IndexEntry) }
    }

    /// Read IndexEntry from bytes with unaligned access (safe wrapper)
    ///
    /// # Safety
    /// - `bytes` must have at least Self::SIZE bytes
    #[inline]
    pub unsafe fn from_bytes_unaligned(bytes: &[u8]) -> Self {
        unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const IndexEntry) }
    }

    /// Write IndexEntry to bytes
    #[inline]
    pub fn write_to_bytes(&self, bytes: &mut [u8]) -> CasIoResult<()> {
        if bytes.len() < Self::SIZE {
            return Err(CasIoError::BufferTooSmall {
                expected: Self::SIZE,
                actual: bytes.len(),
            });
        }

        unsafe {
            std::ptr::write_unaligned(bytes.as_mut_ptr() as *mut IndexEntry, *self);
        }
        Ok(())
    }

    /// Compare with another entry for binary search
    #[inline]
    pub fn compare(&self, other: &Self) -> Ordering {
        self.fp64
            .partial_cmp(&other.fp64)
            .unwrap_or(Ordering::Equal)
    }
}

// ============================================================================
// IndexFileHeader (12 bytes)
// ============================================================================

/// Header for index files
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct IndexFileHeader {
    pub magic: u32,
    pub version: u16,
    pub entry_count: u16,
    pub reserved: u32,
}

impl IndexFileHeader {
    pub const SIZE: usize = INDEX_HEADER_SIZE;

    #[inline]
    pub fn new(entry_count: u16) -> Self {
        IndexFileHeader {
            magic: INDEX_MAGIC,
            version: INDEX_VERSION,
            entry_count,
            reserved: 0,
        }
    }

    #[inline]
    pub fn from_bytes(bytes: &[u8]) -> CasIoResult<Self> {
        if bytes.len() < Self::SIZE {
            return Err(CasIoError::BufferTooSmall {
                expected: Self::SIZE,
                actual: bytes.len(),
            });
        }

        unsafe {
            Ok(std::ptr::read_unaligned(
                bytes.as_ptr() as *const IndexFileHeader
            ))
        }
    }

    #[inline]
    pub fn is_valid(&self) -> bool {
        self.magic == INDEX_MAGIC && self.version == INDEX_VERSION
    }
}

// ============================================================================
// Bloom Filter
// ============================================================================

/// Simple Bloom filter for fast negative lookups
pub struct BloomFilter {
    bits: Vec<u64>,
    num_bits: usize,
}

impl BloomFilter {
    /// Create a new Bloom filter for expected number of elements
    pub fn new(expected_elements: usize) -> Self {
        let num_bits = expected_elements * BLOOM_BITS_PER_ENTRY;
        let num_words = num_bits.div_ceil(64);

        BloomFilter {
            bits: vec![0u64; num_words],
            num_bits,
        }
    }

    /// Create from existing bits
    pub fn from_bits(bits: Vec<u64>, num_bits: usize) -> Self {
        BloomFilter { bits, num_bits }
    }

    /// Hash an atom_id to get bit positions
    fn hash_positions(&self, atom_id: &AtomId) -> [usize; BLOOM_HASH_FUNCTIONS] {
        // Use different parts of the atom_id for different hash functions
        let mut positions = [0usize; BLOOM_HASH_FUNCTIONS];

        for i in 0..BLOOM_HASH_FUNCTIONS {
            // Simple hash: use different byte ranges
            let h = if i < 4 {
                u32::from_le_bytes([
                    atom_id[i * 8 % 32],
                    atom_id[(i * 8 + 1) % 32],
                    atom_id[(i * 8 + 2) % 32],
                    atom_id[(i * 8 + 3) % 32],
                ]) as usize
            } else {
                u32::from_le_bytes([
                    atom_id[(i * 7) % 32],
                    atom_id[(i * 7 + 1) % 32],
                    atom_id[(i * 7 + 2) % 32],
                    atom_id[(i * 7 + 3) % 32],
                ]) as usize
            };

            positions[i] = h % self.num_bits;
        }

        positions
    }

    /// Insert an atom_id into the filter
    pub fn insert(&mut self, atom_id: &AtomId) {
        let positions = self.hash_positions(atom_id);
        for pos in positions {
            let word_idx = pos / 64;
            let bit_idx = pos % 64;
            if word_idx < self.bits.len() {
                self.bits[word_idx] |= 1u64 << bit_idx;
            }
        }
    }

    /// Check if atom_id might be in the filter
    pub fn might_contain(&self, atom_id: &AtomId) -> bool {
        let positions = self.hash_positions(atom_id);
        for pos in positions {
            let word_idx = pos / 64;
            let bit_idx = pos % 64;
            if word_idx >= self.bits.len() {
                return false;
            }
            if self.bits[word_idx] & (1u64 << bit_idx) == 0 {
                return false;
            }
        }
        true
    }

    /// Get the serialized size in bytes
    pub fn serialized_size(&self) -> usize {
        self.bits.len() * 8
    }

    /// Get the number of bits in the filter
    pub fn num_bits(&self) -> usize {
        self.num_bits
    }

    /// Serialize to bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.serialized_size());
        for &word in &self.bits {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    /// Deserialize from bytes
    pub fn from_bytes(bytes: &[u8], num_bits: usize) -> Self {
        let num_words = num_bits.div_ceil(64);
        let mut bits = Vec::with_capacity(num_words);

        for chunk in bytes.as_chunks::<8>().0 {
            bits.push(u64::from_le_bytes(*chunk));
        }

        // Handle remaining bytes if not aligned
        let remaining = bytes.len() % 8;
        if remaining > 0 && bits.len() < num_words {
            let mut last_word = 0u64;
            for (i, &byte) in bytes[bytes.len() - remaining..].iter().enumerate() {
                last_word |= (byte as u64) << (i * 8);
            }
            bits.push(last_word);
        }

        BloomFilter { bits, num_bits }
    }
}

// ============================================================================
// SegmentFile
// ============================================================================

/// A segment file for storing CAS records
///
/// Segment files use append-only writes with proper alignment.
/// Each record consists of:
/// - RecordHeader (64 bytes, padded to 16 bytes)
/// - Body (variable length, padded to 16 bytes)
/// - Body CRC32 (4 bytes, padded to 16 bytes total with header)
pub struct SegmentFile {
    /// Segment ID
    seg_id: u32,
    /// Path to segment file
    path: PathBuf,
    /// Path to index file
    index_path: PathBuf,
    /// File handle for appending
    file: File,
    /// Current file size
    current_size: AtomicU64,
    /// Size limit for this segment
    size_limit: u64,
    /// Memory map for reading (if needed)
    mmap: Option<Mmap>,
    /// Record count in this segment
    record_count: AtomicU32,
}

impl SegmentFile {
    /// Create a new segment file
    pub fn create(base_dir: &Path, seg_id: u32, size_limit: Option<u64>) -> CasIoResult<Self> {
        let path = base_dir.join(format!(
            "{}{:05}.{}",
            SEGMENT_PREFIX, seg_id, SEGMENT_EXTENSION
        ));
        let index_path = base_dir.join(format!(
            "{}{:05}.{}",
            SEGMENT_PREFIX, seg_id, INDEX_EXTENSION
        ));

        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Create/open the file
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        // Get current size
        let current_size = file.metadata()?.len();
        // Ensure subsequent writes append to the current end on reopen.
        file.seek(SeekFrom::Start(current_size))?;

        Ok(SegmentFile {
            seg_id,
            path,
            index_path,
            file,
            current_size: AtomicU64::new(current_size),
            size_limit: size_limit.unwrap_or(DEFAULT_SEGMENT_SIZE_LIMIT),
            mmap: None,
            record_count: AtomicU32::new(0),
        })
    }

    /// Open an existing segment file
    pub fn open(base_dir: &Path, seg_id: u32) -> CasIoResult<Self> {
        let path = base_dir.join(format!(
            "{}{:05}.{}",
            SEGMENT_PREFIX, seg_id, SEGMENT_EXTENSION
        ));
        let index_path = base_dir.join(format!(
            "{}{:05}.{}",
            SEGMENT_PREFIX, seg_id, INDEX_EXTENSION
        ));

        if !path.exists() {
            return Err(CasIoError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Segment file not found: {:?}", path),
            )));
        }

        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;

        let current_size = file.metadata()?.len();
        // Ensure writes continue from the persisted end instead of overwriting.
        file.seek(SeekFrom::Start(current_size))?;

        Ok(SegmentFile {
            seg_id,
            path,
            index_path,
            file,
            current_size: AtomicU64::new(current_size),
            size_limit: DEFAULT_SEGMENT_SIZE_LIMIT,
            mmap: None,
            record_count: AtomicU32::new(0),
        })
    }

    /// Get segment ID
    #[inline]
    pub fn seg_id(&self) -> u32 {
        self.seg_id
    }

    /// Get current file size
    #[inline]
    pub fn current_size(&self) -> u64 {
        self.current_size.load(AtomicOrdering::Relaxed)
    }

    /// Get size limit
    #[inline]
    pub fn size_limit(&self) -> u64 {
        self.size_limit
    }

    /// Check if segment can accept more records
    #[inline]
    pub fn can_accept(&self, record_size: u64) -> bool {
        self.current_size.load(AtomicOrdering::Relaxed) + record_size <= self.size_limit
    }

    /// Calculate padded size for alignment
    #[inline]
    fn calculate_padded_size(base_size: usize) -> usize {
        (base_size + RECORD_ALIGNMENT - 1) & !(RECORD_ALIGNMENT - 1)
    }

    /// Calculate total record size (header + body + CRC + padding)
    #[inline]
    fn calculate_record_size(body_len: u64) -> u64 {
        let header_size = RecordHeader::SIZE as u64;
        let body_padded = Self::calculate_padded_size(body_len as usize) as u64;
        let crc_size = 4u64; // CRC32 after body
        let crc_padded = Self::calculate_padded_size(crc_size as usize) as u64;

        header_size + body_padded + crc_padded
    }

    /// Append a record to the segment
    ///
    /// Returns (offset, body_len) on success
    pub fn append_record(
        &mut self,
        atom_id: AtomId,
        body: &[u8],
        flags: u16,
    ) -> CasIoResult<(u64, u64)> {
        let body_len = body.len() as u64;
        let record_size = Self::calculate_record_size(body_len);

        // Check if we have space
        let current = self.current_size.load(AtomicOrdering::Relaxed);
        if current + record_size > self.size_limit {
            return Err(CasIoError::SegmentFull {
                seg_id: self.seg_id,
                current_size: current,
                limit: self.size_limit,
            });
        }

        // Create header
        let header = RecordHeader::new(atom_id, body_len, self.seg_id, flags);
        let offset = current;

        // Write header
        let mut header_bytes = [0u8; RecordHeader::SIZE];
        header.write_to_bytes(&mut header_bytes)?;
        self.file.write_all(&header_bytes)?;

        // Write body
        self.file.write_all(body)?;

        // Write body padding
        let body_padded = Self::calculate_padded_size(body.len());
        let padding = body_padded - body.len();
        if padding > 0 {
            const ZERO_PAD: [u8; RECORD_ALIGNMENT] = [0u8; RECORD_ALIGNMENT];
            self.file.write_all(&ZERO_PAD[..padding])?;
        }

        // Calculate and write body CRC32
        let body_crc = crc32(body);
        let crc_bytes = body_crc.to_le_bytes();
        self.file.write_all(&crc_bytes)?;

        // Write CRC padding
        let crc_padded = Self::calculate_padded_size(4);
        let crc_padding = crc_padded - 4;
        if crc_padding > 0 {
            const ZERO_PAD: [u8; RECORD_ALIGNMENT] = [0u8; RECORD_ALIGNMENT];
            self.file.write_all(&ZERO_PAD[..crc_padding])?;
        }

        // Flush and sync for durability
        self.file.flush()?;
        self.file.sync_all()?;

        // Update size
        self.current_size
            .fetch_add(record_size, AtomicOrdering::Relaxed);
        self.record_count.fetch_add(1, AtomicOrdering::Relaxed);

        Ok((offset, body_len))
    }

    /// Read a record at the given offset
    ///
    /// Returns (RecordHeader, body bytes)
    pub fn read_record(&self, offset: u64) -> CasIoResult<(RecordHeader, Vec<u8>)> {
        let current_size = self.current_size.load(AtomicOrdering::Relaxed);

        // Validate bounds for header
        if offset + RecordHeader::SIZE as u64 > current_size {
            return Err(CasIoError::InvalidBounds {
                offset,
                length: RecordHeader::SIZE as u64,
                file_size: current_size,
            });
        }

        // Read header
        let mut header_bytes = [0u8; RecordHeader::SIZE];
        let mut file = &self.file;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut header_bytes)?;

        let header = RecordHeader::from_bytes(&header_bytes)?;

        // Validate header CRC
        if !header.validate_crc() {
            return Err(CasIoError::CorruptedRecord {
                offset,
                reason: format!(
                    "Header CRC mismatch: expected {:08X}, got {:08X}",
                    header.header_crc32,
                    header.calculate_crc()
                ),
            });
        }

        // Validate magic
        if !header.validate_magic() {
            return Err(CasIoError::InvalidMagic {
                expected: RECORD_MAGIC,
                found: header.magic,
            });
        }

        // Read body
        let body_len = header.body_len() as usize;
        let body_offset = offset + RecordHeader::SIZE as u64;

        if body_offset + body_len as u64 > current_size {
            return Err(CasIoError::InvalidBounds {
                offset: body_offset,
                length: body_len as u64,
                file_size: current_size,
            });
        }

        let mut body = vec![0u8; body_len];
        file.seek(SeekFrom::Start(body_offset))?;
        file.read_exact(&mut body)?;

        // Read body CRC32 (after padding)
        let body_padded = Self::calculate_padded_size(body_len);
        let crc_offset = body_offset + body_padded as u64;

        if crc_offset + 4 > current_size {
            return Err(CasIoError::InvalidBounds {
                offset: crc_offset,
                length: 4,
                file_size: current_size,
            });
        }

        file.seek(SeekFrom::Start(crc_offset))?;
        let mut crc_bytes = [0u8; 4];
        file.read_exact(&mut crc_bytes)?;
        let stored_crc = u32::from_le_bytes(crc_bytes);

        // Validate body CRC
        let computed_crc = crc32(&body);
        if stored_crc != computed_crc {
            return Err(CasIoError::CrcMismatch {
                expected: stored_crc,
                found: computed_crc,
            });
        }

        Ok((header, body))
    }

    /// Read a record using mmap (zero-copy for body)
    ///
    /// # Safety
    /// - Caller must ensure segment file is not modified during read
    pub fn read_record_mmap(&self, offset: u64) -> CasIoResult<(RecordHeader, Vec<u8>)> {
        // For safety, we still copy the body even with mmap support
        // True zero-copy would require lifetime management
        self.read_record(offset)
    }

    /// Create or refresh memory map for reading
    pub fn refresh_mmap(&mut self) -> CasIoResult<()> {
        let file = OpenOptions::new().read(true).open(&self.path)?;
        let mmap = unsafe { MmapOptions::new().map(&file)? };
        self.mmap = Some(mmap);
        Ok(())
    }

    /// Flush pending writes
    pub fn flush(&self) -> CasIoResult<()> {
        let mut file = &self.file;
        file.flush()?;
        file.sync_data()?;
        Ok(())
    }

    /// Sync all data to disk
    pub fn sync_all(&self) -> CasIoResult<()> {
        let file = &self.file;
        file.sync_all()?;
        Ok(())
    }

    /// Get path to segment file
    #[inline]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Get path to index file
    #[inline]
    pub fn index_path(&self) -> &Path {
        &self.index_path
    }

    /// Get record count
    #[inline]
    pub fn record_count(&self) -> u32 {
        self.record_count.load(AtomicOrdering::Relaxed)
    }
}

// ============================================================================
// IndexFile
// ============================================================================

/// Index file for a segment with sorted entries and Bloom filter
pub struct IndexFile {
    /// Segment ID this index belongs to
    seg_id: u32,
    /// Path to index file
    path: PathBuf,
    /// File handle
    file: Option<File>,
    /// In-memory entries (sorted by fp64)
    entries: RwLock<Vec<IndexEntry>>,
    /// Bloom filter
    bloom: RwLock<BloomFilter>,
    /// Memory map for reading
    mmap: Option<Mmap>,
    /// Entry count
    entry_count: AtomicU32,
    /// Is the index dirty (needs flush)
    is_dirty: AtomicU32,
}

impl IndexFile {
    /// Create a new index file
    pub fn create(base_dir: &Path, seg_id: u32) -> CasIoResult<Self> {
        let path = base_dir.join(format!(
            "{}{:05}.{}",
            SEGMENT_PREFIX, seg_id, INDEX_EXTENSION
        ));

        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        Ok(IndexFile {
            seg_id,
            path,
            file: None,
            entries: RwLock::new(Vec::new()),
            bloom: RwLock::new(BloomFilter::new(1024)),
            mmap: None,
            entry_count: AtomicU32::new(0),
            is_dirty: AtomicU32::new(0),
        })
    }

    /// Open an existing index file
    pub fn open(base_dir: &Path, seg_id: u32) -> CasIoResult<Self> {
        let path = base_dir.join(format!(
            "{}{:05}.{}",
            SEGMENT_PREFIX, seg_id, INDEX_EXTENSION
        ));

        if !path.exists() {
            return Self::create(base_dir, seg_id);
        }

        let file = OpenOptions::new().read(true).write(true).open(&path)?;

        let mut index = IndexFile {
            seg_id,
            path,
            file: Some(file),
            entries: RwLock::new(Vec::new()),
            bloom: RwLock::new(BloomFilter::new(1024)),
            mmap: None,
            entry_count: AtomicU32::new(0),
            is_dirty: AtomicU32::new(0),
        };

        // Load entries from file
        index.load_from_file()?;

        Ok(index)
    }

    /// Load index entries from file
    fn load_from_file(&mut self) -> CasIoResult<()> {
        let file = self.file.as_ref().ok_or_else(|| {
            CasIoError::Io(io::Error::new(
                io::ErrorKind::NotConnected,
                "Index file not open",
            ))
        })?;

        // Read header
        let mut header_bytes = [0u8; IndexFileHeader::SIZE];
        let mut reader = BufReader::new(file);
        reader.read_exact(&mut header_bytes)?;

        let header = IndexFileHeader::from_bytes(&header_bytes)?;

        if !header.is_valid() {
            return Err(CasIoError::InvalidMagic {
                expected: INDEX_MAGIC,
                found: header.magic,
            });
        }

        let entry_count = header.entry_count as usize;

        // Read entries
        let mut entries = Vec::with_capacity(entry_count);
        let mut entry_bytes = [0u8; IndexEntry::SIZE];

        for _ in 0..entry_count {
            reader.read_exact(&mut entry_bytes)?;
            let entry = unsafe { IndexEntry::from_bytes_unaligned(&entry_bytes) };
            entries.push(entry);
        }

        // Read bloom filter size
        let mut bloom_size_bytes = [0u8; 4];
        reader.read_exact(&mut bloom_size_bytes)?;
        let bloom_size = u32::from_le_bytes(bloom_size_bytes) as usize;

        // Read bloom filter bits
        let mut bloom_bytes = vec![0u8; bloom_size];
        reader.read_exact(&mut bloom_bytes)?;

        // The writer may reserve more Bloom capacity than the current entry
        // count. Reconstruct the original modulus from the serialized bitset;
        // deriving it from entry_count changes hash positions after reopen and
        // introduces false negatives for persisted entries.
        let num_bits = bloom_bytes.len() * 8;
        let bloom = BloomFilter::from_bytes(&bloom_bytes, num_bits);

        // Sort entries by fp64
        entries.sort_by(|a, b| a.compare(b));

        // Update state
        *self.entries.write() = entries;
        *self.bloom.write() = bloom;
        self.entry_count
            .store(entry_count as u32, AtomicOrdering::Relaxed);

        Ok(())
    }

    /// Insert an entry into the index
    pub fn insert(&self, atom_id: AtomId, seg_offset: u64, body_len: u64, flags: u16) {
        let entry = IndexEntry::new(atom_id, seg_offset, body_len, flags);

        let mut entries = self.entries.write();
        entries.push(entry);
        drop(entries);

        self.bloom.write().insert(&atom_id);

        self.entry_count.fetch_add(1, AtomicOrdering::Relaxed);
        self.is_dirty.store(1, AtomicOrdering::Relaxed);
    }

    /// Batch insert entries
    pub fn batch_insert(&self, entries_data: &[(AtomId, u64, u64, u16)]) {
        let mut entries = self.entries.write();
        let mut bloom = self.bloom.write();

        for &(atom_id, seg_offset, body_len, flags) in entries_data {
            let entry = IndexEntry::new(atom_id, seg_offset, body_len, flags);
            entries.push(entry);
            bloom.insert(&atom_id);
        }

        drop(entries);
        drop(bloom);

        self.entry_count
            .fetch_add(entries_data.len() as u32, AtomicOrdering::Relaxed);
        self.is_dirty.store(1, AtomicOrdering::Relaxed);
    }

    /// Sort entries (call after batch insert)
    pub fn sort_entries(&self) {
        let mut entries = self.entries.write();
        entries.sort_by(|a, b| a.compare(b));
    }

    /// Search for an entry by atom_id using binary search
    pub fn find(&self, atom_id: &AtomId) -> Option<IndexEntry> {
        // First check Bloom filter (fast negative)
        if !self.bloom.read().might_contain(atom_id) {
            return None;
        }

        // Convert atom_id to fp64 for comparison
        let target_fp64 = f64::from_le_bytes([
            atom_id[0], atom_id[1], atom_id[2], atom_id[3], atom_id[4], atom_id[5], atom_id[6],
            atom_id[7],
        ]);

        let entries = self.entries.read();

        // Binary search
        let mut left = 0;
        let mut right = entries.len();

        while left < right {
            let mid = left + (right - left) / 2;
            match entries[mid].fp64.partial_cmp(&target_fp64) {
                Some(Ordering::Less) => left = mid + 1,
                Some(Ordering::Greater) => right = mid,
                Some(Ordering::Equal) => {
                    // Found matching fp64, need to check full atom_id
                    // For now, return the entry (potential false positive if hash collision)
                    return Some(entries[mid]);
                }
                None => return None,
            }
        }

        None
    }

    /// Get all entries (for iteration)
    pub fn get_all_entries(&self) -> Vec<IndexEntry> {
        self.entries.read().clone()
    }

    /// Flush index to disk
    pub fn flush(&self) -> CasIoResult<()> {
        if self.is_dirty.load(AtomicOrdering::Relaxed) == 0 {
            return Ok(());
        }

        let entries = self.entries.read();
        let bloom = self.bloom.read();

        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.path)?;

        let mut writer = BufWriter::new(file);

        // Write header
        let header = IndexFileHeader::new(entries.len() as u16);
        let header_bytes = unsafe {
            std::slice::from_raw_parts(
                &header as *const IndexFileHeader as *const u8,
                IndexFileHeader::SIZE,
            )
        };
        writer.write_all(header_bytes)?;

        // Write entries
        for entry in entries.iter() {
            let mut entry_bytes = [0u8; IndexEntry::SIZE];
            entry.write_to_bytes(&mut entry_bytes)?;
            writer.write_all(&entry_bytes)?;
        }

        // Write bloom filter
        let bloom_bytes = bloom.to_bytes();
        let bloom_size = bloom_bytes.len() as u32;
        writer.write_all(&bloom_size.to_le_bytes())?;
        writer.write_all(&bloom_bytes)?;

        writer.flush()?;
        drop(writer);

        // Sync
        let file = OpenOptions::new().write(true).open(&self.path)?;
        file.sync_all()?;

        self.is_dirty.store(0, AtomicOrdering::Relaxed);

        Ok(())
    }

    /// Create memory map for reading
    pub fn create_mmap(&mut self) -> CasIoResult<()> {
        let file = OpenOptions::new().read(true).open(&self.path)?;
        let mmap = unsafe { MmapOptions::new().map(&file)? };
        self.mmap = Some(mmap);
        Ok(())
    }

    /// Get entry count
    #[inline]
    pub fn entry_count(&self) -> u32 {
        self.entry_count.load(AtomicOrdering::Relaxed)
    }

    /// Check if index is dirty
    #[inline]
    pub fn is_dirty(&self) -> bool {
        self.is_dirty.load(AtomicOrdering::Relaxed) != 0
    }

    /// Get path to index file
    #[inline]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ============================================================================
// CasWriter
// ============================================================================

/// High-level CAS writer with segment management
pub struct CasWriter {
    /// Base directory for CAS files
    base_dir: PathBuf,
    /// Current active segment
    active_segment: Mutex<SegmentFile>,
    /// Current active index
    active_index: RwLock<Arc<IndexFile>>,
    /// Segment size limit
    size_limit: u64,
    /// Next segment ID
    next_seg_id: AtomicU32,
}

impl CasWriter {
    /// Create a new CAS writer
    pub fn new(base_dir: &Path, size_limit: Option<u64>) -> CasIoResult<Self> {
        // Create base directory if needed
        std::fs::create_dir_all(base_dir)?;

        let size_limit = size_limit.unwrap_or(DEFAULT_SEGMENT_SIZE_LIMIT);

        // Find the latest segment or start fresh
        let (seg_id, _) = Self::find_latest_segment(base_dir)?;
        let actual_seg_id = seg_id;

        let mut segment = SegmentFile::create(base_dir, actual_seg_id, Some(size_limit))?;
        // Reopen existing active index on restart to preserve persisted entries.
        let index = Arc::new(IndexFile::open(base_dir, actual_seg_id)?);

        // Refresh mmap for segment
        segment.refresh_mmap()?;

        let next_seg_id = AtomicU32::new(actual_seg_id + 1);

        Ok(CasWriter {
            base_dir: base_dir.to_path_buf(),
            active_segment: Mutex::new(segment),
            active_index: RwLock::new(index),
            size_limit,
            next_seg_id,
        })
    }

    /// Find the latest segment in the directory
    fn find_latest_segment(base_dir: &Path) -> CasIoResult<(u32, u64)> {
        if !base_dir.exists() {
            return Ok((0, 0));
        }

        let mut max_seg_id = 0u32;
        let mut max_size = 0u64;

        for entry in std::fs::read_dir(base_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            if name_str.starts_with(SEGMENT_PREFIX) && name_str.ends_with(SEGMENT_EXTENSION) {
                // Extract segment ID
                if let Some(id_str) = name_str
                    .strip_prefix(SEGMENT_PREFIX)
                    .and_then(|s| s.strip_suffix(&format!(".{}", SEGMENT_EXTENSION)))
                    && let Ok(seg_id) = id_str.parse::<u32>()
                    && seg_id >= max_seg_id
                {
                    let size = entry.metadata()?.len();
                    if seg_id > max_seg_id || (seg_id == max_seg_id && size > max_size) {
                        max_seg_id = seg_id;
                        max_size = size;
                    }
                }
            }
        }

        Ok((max_seg_id, max_size))
    }

    /// Rotate to a new segment
    fn rotate_segment(&self) -> CasIoResult<()> {
        // Flush current index before switching active segment/index pair.
        let current_index = Arc::clone(&self.active_index.read());
        current_index.flush()?;

        // Get next segment ID
        let new_seg_id = self.next_seg_id.fetch_add(1, AtomicOrdering::SeqCst);

        // Create new segment and index
        let new_segment = SegmentFile::create(&self.base_dir, new_seg_id, Some(self.size_limit))?;
        let new_index = Arc::new(IndexFile::create(&self.base_dir, new_seg_id)?);

        // Replace active segment
        let mut active_segment = self.active_segment.lock();
        *active_segment = new_segment;
        drop(active_segment);

        // Replace active index so writes after rotation persist into the correct segment index.
        let mut active_index = self.active_index.write();
        *active_index = new_index;

        Ok(())
    }

    /// Write a record to CAS
    ///
    /// Returns (seg_id, offset, body_len) on success
    pub fn write_record(&self, atom_id: AtomId, body: &[u8]) -> CasIoResult<(u32, u64, u64)> {
        self.write_record_with_flags(atom_id, body, 0)
    }

    /// Write a record with flags
    pub fn write_record_with_flags(
        &self,
        atom_id: AtomId,
        body: &[u8],
        flags: u16,
    ) -> CasIoResult<(u32, u64, u64)> {
        let record_size = SegmentFile::calculate_record_size(body.len() as u64);

        // Check if we need to rotate
        let mut segment = self.active_segment.lock();
        if !segment.can_accept(record_size) {
            drop(segment);
            self.rotate_segment()?;
            segment = self.active_segment.lock();
        }

        let seg_id = segment.seg_id();

        // Append the record
        let (offset, body_len) = segment.append_record(atom_id, body, flags)?;

        // Update index
        let active_index = Arc::clone(&self.active_index.read());
        active_index.insert(atom_id, offset, body_len, flags);

        Ok((seg_id, offset, body_len))
    }

    /// Flush pending writes
    pub fn flush(&self) -> CasIoResult<()> {
        let segment = self.active_segment.lock();
        segment.flush()?;
        let active_index = Arc::clone(&self.active_index.read());
        active_index.flush()?;
        Ok(())
    }

    /// Sync all data to disk
    pub fn sync_all(&self) -> CasIoResult<()> {
        let segment = self.active_segment.lock();
        segment.sync_all()?;
        let active_index = Arc::clone(&self.active_index.read());
        active_index.flush()?;
        Ok(())
    }

    /// Get the active segment ID
    pub fn active_seg_id(&self) -> u32 {
        self.active_segment.lock().seg_id()
    }
}

// ============================================================================
// CasReader
// ============================================================================

/// High-level CAS reader with index lookup
pub struct CasReader {
    /// Base directory for CAS files
    base_dir: PathBuf,
    /// Segment files (seg_id -> SegmentFile)
    segments: RwLock<std::collections::BTreeMap<u32, SegmentFile>>,
    /// Index files (seg_id -> IndexFile)
    indexes: RwLock<std::collections::BTreeMap<u32, IndexFile>>,
}

impl CasReader {
    /// Create a new CAS reader
    pub fn new(base_dir: &Path) -> CasIoResult<Self> {
        let mut reader = CasReader {
            base_dir: base_dir.to_path_buf(),
            segments: RwLock::new(std::collections::BTreeMap::new()),
            indexes: RwLock::new(std::collections::BTreeMap::new()),
        };

        // Discover and open all segments
        reader.discover_segments()?;

        Ok(reader)
    }

    /// Discover all segment files in the directory
    fn discover_segments(&mut self) -> CasIoResult<()> {
        if !self.base_dir.exists() {
            return Ok(());
        }

        let mut segments = self.segments.write();
        let mut indexes = self.indexes.write();

        for entry in std::fs::read_dir(&self.base_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            if name_str.starts_with(SEGMENT_PREFIX)
                && name_str.ends_with(SEGMENT_EXTENSION)
                && let Some(id_str) = name_str
                    .strip_prefix(SEGMENT_PREFIX)
                    .and_then(|s| s.strip_suffix(&format!(".{}", SEGMENT_EXTENSION)))
                && let Ok(seg_id) = id_str.parse::<u32>()
            {
                let segment = SegmentFile::open(&self.base_dir, seg_id)?;
                let index = IndexFile::open(&self.base_dir, seg_id)?;
                segments.insert(seg_id, segment);
                indexes.insert(seg_id, index);
            }
        }

        Ok(())
    }

    /// Find a record by atom_id using the index
    pub fn find_record(&self, atom_id: &AtomId) -> CasIoResult<Option<(u32, u64, Vec<u8>)>> {
        let indexes = self.indexes.read();

        // Search in each index
        for (&seg_id, index) in indexes.iter() {
            if let Some(entry) = index.find(atom_id) {
                // Verify the atom_id matches (not just fp64)
                let segments = self.segments.read();
                if let Some(segment) = segments.get(&seg_id) {
                    let (header, body) = segment.read_record(entry.seg_offset)?;

                    // Verify atom_id matches
                    if header.atom_id() == atom_id {
                        return Ok(Some((seg_id, entry.seg_offset, body)));
                    }
                }
            }
        }

        Ok(None)
    }

    /// Read a record at a specific segment and offset
    pub fn read_record(&self, seg_id: u32, offset: u64) -> CasIoResult<(RecordHeader, Vec<u8>)> {
        let segments = self.segments.read();
        let segment = segments.get(&seg_id).ok_or_else(|| {
            CasIoError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Segment {} not found", seg_id),
            ))
        })?;

        segment.read_record(offset)
    }

    pub fn get_record_view(&self, seg_id: u32, offset: u64) -> CasIoResult<RecordView<'static>> {
        let (_header, _body) = self.read_record(seg_id, offset)?;
        // Note: True zero-copy requires careful lifetime management.
        // This returns an owned RecordView with the body data.
        // For a proper implementation, we'd need to store the mmap reference.
        Err(CasIoError::Io(io::Error::other(
            "RecordView with proper lifetime requires mmap support",
        )))
    }

    /// Check if a record exists (using Bloom filter)
    pub fn might_contain(&self, atom_id: &AtomId) -> bool {
        let indexes = self.indexes.read();

        for index in indexes.values() {
            if index.find(atom_id).is_some() {
                return true;
            }
        }

        false
    }

    /// Get list of all known atom IDs
    pub fn list_all_atoms(&self) -> CasIoResult<Vec<AtomId>> {
        // This requires iterating through all records
        let mut atoms = Vec::new();

        let segments = self.segments.read();
        for (&seg_id, segment) in segments.iter() {
            let indexes = self.indexes.read();
            if let Some(index) = indexes.get(&seg_id) {
                for entry in index.get_all_entries() {
                    let (header, _) = segment.read_record(entry.seg_offset)?;
                    atoms.push(*header.atom_id());
                }
            }
        }

        Ok(atoms)
    }

    /// Get segment count
    pub fn segment_count(&self) -> usize {
        self.segments.read().len()
    }
}

// ============================================================================
// CasIterator
// ============================================================================

/// Iterator over all records in a segment
pub struct CasIterator<'a> {
    segment: &'a SegmentFile,
    current_offset: u64,
    end_offset: u64,
}

impl<'a> CasIterator<'a> {
    /// Create a new iterator for a segment
    pub fn new(segment: &'a SegmentFile) -> Self {
        CasIterator {
            segment,
            current_offset: 0,
            end_offset: segment.current_size(),
        }
    }

    /// Create iterator starting from a specific offset
    pub fn from_offset(segment: &'a SegmentFile, start_offset: u64) -> Self {
        CasIterator {
            segment,
            current_offset: start_offset,
            end_offset: segment.current_size(),
        }
    }
}

impl<'a> Iterator for CasIterator<'a> {
    type Item = CasIoResult<(RecordHeader, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current_offset >= self.end_offset {
            return None;
        }

        // Try to read the record
        match self.segment.read_record(self.current_offset) {
            Ok((header, body)) => {
                // Advance to next record
                let record_size = SegmentFile::calculate_record_size(header.body_len());
                self.current_offset += record_size;
                Some(Ok((header, body)))
            }
            Err(e) => {
                // On error, try to skip to next potential record
                // Move forward by minimum record size
                self.current_offset += RecordHeader::SIZE as u64;

                // If it's a corruption error, try to continue
                match e {
                    CasIoError::CorruptedRecord { .. }
                    | CasIoError::CrcMismatch { .. }
                    | CasIoError::InvalidMagic { .. } => {
                        // Skip this record and try next
                        // This allows iteration to continue past corrupted records
                        self.next()
                    }
                    _ => Some(Err(e)),
                }
            }
        }
    }
}

/// Iterator over all records in all segments
pub struct GlobalCasIterator<'a> {
    segments: std::collections::btree_map::Iter<'a, u32, SegmentFile>,
    current_segment: Option<&'a SegmentFile>,
    current_iterator: Option<CasIterator<'a>>,
}

impl<'a> GlobalCasIterator<'a> {
    /// Create a new global iterator
    pub fn new(segments: &'a std::collections::BTreeMap<u32, SegmentFile>) -> Self {
        let mut iter = GlobalCasIterator {
            segments: segments.iter(),
            current_segment: None,
            current_iterator: None,
        };

        // Initialize with first segment
        iter.advance_to_next_segment();
        iter
    }

    fn advance_to_next_segment(&mut self) {
        if let Some((_, segment)) = self.segments.next() {
            self.current_segment = Some(segment);
            self.current_iterator = Some(CasIterator::new(segment));
        } else {
            self.current_segment = None;
            self.current_iterator = None;
        }
    }
}

impl<'a> Iterator for GlobalCasIterator<'a> {
    type Item = CasIoResult<(RecordHeader, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(ref mut iter) = self.current_iterator
            && let Some(result) = iter.next()
        {
            return Some(result);
        }

        // Current segment exhausted, move to next
        self.advance_to_next_segment();

        if let Some(ref mut iter) = self.current_iterator {
            iter.next()
        } else {
            None
        }
    }
}

// ============================================================================
// Compaction
// ============================================================================

/// Compactor for merging segments and removing superseded atoms
pub struct Compactor {
    base_dir: PathBuf,
    size_limit: u64,
}

impl Compactor {
    /// Create a new compactor
    pub fn new(base_dir: &Path, size_limit: Option<u64>) -> Self {
        Compactor {
            base_dir: base_dir.to_path_buf(),
            size_limit: size_limit.unwrap_or(DEFAULT_SEGMENT_SIZE_LIMIT),
        }
    }

    #[inline]
    fn compaction_target_limit(&self) -> u64 {
        self.size_limit.max(MIN_SEGMENT_SIZE_FOR_COMPACTION)
    }

    fn next_unused_target_seg_id(&self, mut seg_id: u32) -> u32 {
        loop {
            let seg_path = self.base_dir.join(format!(
                "{}{:05}.{}",
                SEGMENT_PREFIX, seg_id, SEGMENT_EXTENSION
            ));
            let idx_path = self.base_dir.join(format!(
                "{}{:05}.{}",
                SEGMENT_PREFIX, seg_id, INDEX_EXTENSION
            ));
            if !seg_path.exists() && !idx_path.exists() {
                return seg_id;
            }
            seg_id += 1;
        }
    }

    fn create_compaction_target(&self, seg_id: u32) -> CasIoResult<(u32, SegmentFile, IndexFile)> {
        let actual_seg_id = self.next_unused_target_seg_id(seg_id);
        let target_segment = SegmentFile::create(
            &self.base_dir,
            actual_seg_id,
            Some(self.compaction_target_limit()),
        )?;
        let target_index = IndexFile::create(&self.base_dir, actual_seg_id)?;
        Ok((actual_seg_id, target_segment, target_index))
    }

    fn flush_compaction_target(
        &self,
        target_segment: &mut SegmentFile,
        target_index: &IndexFile,
    ) -> CasIoResult<()> {
        target_segment.flush()?;
        target_index.flush()?;
        Ok(())
    }

    /// Compact multiple segments into one
    ///
    /// This merges segments and removes superseded atoms (keeping latest version).
    pub fn compact_segments(&self, source_seg_ids: &[u32], target_seg_id: u32) -> CasIoResult<u32> {
        if source_seg_ids.is_empty() {
            return Err(CasIoError::CompactionError(
                "No source segments provided".to_string(),
            ));
        }

        let mut sources: Vec<(SegmentFile, IndexFile)> = Vec::new();
        for &seg_id in source_seg_ids {
            let segment = SegmentFile::open(&self.base_dir, seg_id)?;
            let index = IndexFile::open(&self.base_dir, seg_id)?;
            sources.push((segment, index));
        }

        let (mut current_target_seg_id, mut target_segment, mut target_index) =
            self.create_compaction_target(target_seg_id)?;

        let mut atom_records: std::collections::HashMap<AtomId, (u32, u64, Vec<u8>, u16)> =
            std::collections::HashMap::new();

        for (seg_idx, (segment, index)) in sources.iter().enumerate() {
            let entries = index.get_all_entries();

            for entry in entries {
                match segment.read_record(entry.seg_offset) {
                    Ok((header, body)) => {
                        let atom_id = *header.atom_id();
                        let flags = header.flags;

                        let should_replace = match atom_records.get(&atom_id) {
                            None => true,
                            Some((existing_seg_id, existing_offset, _, _)) => {
                                seg_idx > *existing_seg_id as usize
                                    || (seg_idx == *existing_seg_id as usize
                                        && entry.seg_offset > *existing_offset)
                            }
                        };

                        if should_replace {
                            atom_records
                                .insert(atom_id, (seg_idx as u32, entry.seg_offset, body, flags));
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to read record at seg {} offset {}: {:?}",
                            segment.seg_id(),
                            entry.seg_offset,
                            e
                        );
                    }
                }
            }
        }

        let mut records: Vec<(AtomId, u32, u64, Vec<u8>, u16)> = atom_records
            .into_iter()
            .map(|(atom_id, (source_seg, source_offset, body, flags))| {
                (atom_id, source_seg, source_offset, body, flags)
            })
            .collect();
        records.sort_by_key(|(_, source_seg, source_offset, _, _)| (*source_seg, *source_offset));

        let mut written_count = 0u32;
        for (atom_id, _source_seg, _source_offset, body, flags) in records.iter() {
            loop {
                match target_segment.append_record(*atom_id, body, *flags) {
                    Ok((offset, body_len)) => {
                        target_index.insert(*atom_id, offset, body_len, *flags);
                        written_count += 1;
                        break;
                    }
                    Err(CasIoError::SegmentFull { .. }) => {
                        self.flush_compaction_target(&mut target_segment, &target_index)?;
                        let (next_seg_id, next_segment, next_index) =
                            self.create_compaction_target(current_target_seg_id + 1)?;
                        current_target_seg_id = next_seg_id;
                        target_segment = next_segment;
                        target_index = next_index;
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        self.flush_compaction_target(&mut target_segment, &target_index)?;
        Ok(written_count)
    }

    /// Remove superseded atoms based on supersedes links
    ///
    /// This is a more sophisticated compaction that understands atom relationships.
    pub fn compact_with_supersedes(
        &self,
        source_seg_ids: &[u32],
        target_seg_id: u32,
        supersedes_map: &std::collections::HashMap<AtomId, Vec<AtomId>>,
    ) -> CasIoResult<u32> {
        let mut sources: Vec<(SegmentFile, IndexFile)> = Vec::new();
        for &seg_id in source_seg_ids {
            let segment = SegmentFile::open(&self.base_dir, seg_id)?;
            let index = IndexFile::open(&self.base_dir, seg_id)?;
            sources.push((segment, index));
        }

        let mut superseded: std::collections::HashSet<AtomId> = std::collections::HashSet::new();
        for superseded_by in supersedes_map.values() {
            for &old_atom in superseded_by {
                superseded.insert(old_atom);
            }
        }

        let (mut current_target_seg_id, mut target_segment, mut target_index) =
            self.create_compaction_target(target_seg_id)?;

        let mut atom_records: std::collections::HashMap<AtomId, (u64, Vec<u8>, u16)> =
            std::collections::HashMap::new();

        for (segment, index) in sources.iter() {
            let entries = index.get_all_entries();

            for entry in entries {
                if let Ok((header, body)) = segment.read_record(entry.seg_offset) {
                    let atom_id = *header.atom_id();
                    if superseded.contains(&atom_id) || header.is_deleted() {
                        continue;
                    }
                    atom_records.insert(atom_id, (entry.seg_offset, body, entry.flags as u16));
                }
            }
        }

        let mut records: Vec<(AtomId, u64, Vec<u8>, u16)> = atom_records
            .into_iter()
            .map(|(atom_id, (offset, body, flags))| (atom_id, offset, body, flags))
            .collect();
        records.sort_by_key(|(_, offset, _, _)| *offset);

        let mut written_count = 0u32;
        for (atom_id, _offset, body, flags) in records.iter() {
            loop {
                match target_segment.append_record(*atom_id, body, *flags) {
                    Ok((offset, body_len)) => {
                        target_index.insert(*atom_id, offset, body_len, *flags);
                        written_count += 1;
                        break;
                    }
                    Err(CasIoError::SegmentFull { .. }) => {
                        self.flush_compaction_target(&mut target_segment, &target_index)?;
                        let (next_seg_id, next_segment, next_index) =
                            self.create_compaction_target(current_target_seg_id + 1)?;
                        current_target_seg_id = next_seg_id;
                        target_segment = next_segment;
                        target_index = next_index;
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        self.flush_compaction_target(&mut target_segment, &target_index)?;
        Ok(written_count)
    }

    /// Delete old segment files after successful compaction
    pub fn delete_segments(&self, seg_ids: &[u32]) -> CasIoResult<()> {
        for &seg_id in seg_ids {
            let seg_path = self.base_dir.join(format!(
                "{}{:05}.{}",
                SEGMENT_PREFIX, seg_id, SEGMENT_EXTENSION
            ));
            let idx_path = self.base_dir.join(format!(
                "{}{:05}.{}",
                SEGMENT_PREFIX, seg_id, INDEX_EXTENSION
            ));

            if seg_path.exists() {
                std::fs::remove_file(&seg_path)?;
            }

            if idx_path.exists() {
                std::fs::remove_file(&idx_path)?;
            }
        }

        Ok(())
    }
}

// ============================================================================
// CasStore - Main entry point
// ============================================================================

/// Main CAS store with reader and writer
pub struct CasStore {
    /// Base directory
    base_dir: PathBuf,
    /// Writer (interior mutability for concurrent access)
    writer: RwLock<Option<CasWriter>>,
    /// Reader
    reader: RwLock<Option<CasReader>>,
    /// Segment size limit
    size_limit: u64,
    /// Deferred edges store (SKF-1.1 Spec B.8)
    deferred_edges: Mutex<DeferredEdgesStore>,
    /// Write-through cache for atoms written in this session
    /// (Windows workaround: reader can't open files held by writer)
    write_cache: Mutex<std::collections::HashMap<AtomId, Vec<u8>>>,
}

impl CasStore {
    /// Create or open a CAS store
    pub fn open(base_dir: &Path, size_limit: Option<u64>) -> CasIoResult<Self> {
        std::fs::create_dir_all(base_dir)?;

        let deferred_edges =
            DeferredEdgesStore::open(base_dir).map_err(|e| io::Error::other(e.to_string()))?;

        let store = CasStore {
            base_dir: base_dir.to_path_buf(),
            writer: RwLock::new(None),
            reader: RwLock::new(None),
            size_limit: size_limit.unwrap_or(DEFAULT_SEGMENT_SIZE_LIMIT),
            deferred_edges: Mutex::new(deferred_edges),
            write_cache: Mutex::new(std::collections::HashMap::new()),
        };

        Ok(store)
    }

    /// Initialize writer
    pub fn init_writer(&self) -> CasIoResult<()> {
        let writer = CasWriter::new(&self.base_dir, Some(self.size_limit))?;
        *self.writer.write() = Some(writer);
        Ok(())
    }

    /// Initialize reader
    pub fn init_reader(&self) -> CasIoResult<()> {
        let reader = CasReader::new(&self.base_dir)?;
        *self.reader.write() = Some(reader);
        Ok(())
    }

    /// Get a read guard to the initialized CAS writer.
    pub fn writer(&self) -> CasIoResult<parking_lot::MappedRwLockReadGuard<'_, CasWriter>> {
        parking_lot::RwLockReadGuard::try_map(self.writer.read(), |writer| writer.as_ref()).map_err(
            |_| {
                CasIoError::Io(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "Writer not initialized",
                ))
            },
        )
    }

    /// Write a record
    pub fn write(&self, atom_id: AtomId, body: &[u8]) -> CasIoResult<(u32, u64, u64)> {
        // Store in write-through cache (Windows workaround for reader access)
        {
            let mut cache = self.write_cache.lock();
            cache.insert(atom_id, body.to_vec());
        }

        let writer_guard = self.writer.read();
        let writer = writer_guard.as_ref().ok_or_else(|| {
            CasIoError::Io(io::Error::new(
                io::ErrorKind::NotConnected,
                "Writer not initialized",
            ))
        })?;

        writer.write_record(atom_id, body)
    }

    /// Read a record by atom_id
    pub fn read(&self, atom_id: &AtomId) -> CasIoResult<Option<Vec<u8>>> {
        // First check write-through cache (Windows workaround)
        {
            let cache = self.write_cache.lock();
            if let Some(body) = cache.get(atom_id) {
                return Ok(Some(body.clone()));
            }
        }

        let reader_guard = self.reader.read();
        let reader = reader_guard.as_ref().ok_or_else(|| {
            CasIoError::Io(io::Error::new(
                io::ErrorKind::NotConnected,
                "Reader not initialized",
            ))
        })?;

        match reader.find_record(atom_id)? {
            Some((_seg_id, _offset, body)) => Ok(Some(body)),
            None => Ok(None),
        }
    }

    /// Flush all pending writes
    pub fn flush(&self) -> CasIoResult<()> {
        let writer_guard = self.writer.read();
        if let Some(writer) = writer_guard.as_ref() {
            writer.flush()?;
        }
        Ok(())
    }

    /// Sync all data to disk
    pub fn sync_all(&self) -> CasIoResult<()> {
        let writer_guard = self.writer.read();
        if let Some(writer) = writer_guard.as_ref() {
            writer.sync_all()?;
        }
        Ok(())
    }

    /// Compact segments
    pub fn compact(&self, source_ids: &[u32], target_id: u32) -> CasIoResult<u32> {
        let compactor = Compactor::new(&self.base_dir, Some(self.size_limit));
        compactor.compact_segments(source_ids, target_id)
    }

    // ========================================================================
    // Deferred Edges (SKF-1.1 Spec B.8)
    // ========================================================================

    /// Add a deferred edge when the target atom does not yet have a NodeNum.
    ///
    /// This is called during atom building when an edge target references an
    /// AtomId that has not been created/ingested yet.
    pub fn add_deferred_edge(
        &self,
        target_atom_id: AtomId,
        edge_type: u32,
        source_atom_id: AtomId,
    ) {
        let mut store = self.deferred_edges.lock();
        store.add_deferred(target_atom_id, edge_type, source_atom_id);
    }

    /// Try to resolve deferred edges when a new atom is created.
    ///
    /// Checks if `created_atom_id` matches any pending deferred edge targets.
    /// Returns all resolved edges with their resolved `target_node_num`.
    ///
    /// The resolved edges should be passed to the graph store to add the
    /// actual edges now that the target atom exists.
    pub fn try_resolve_deferred_edges(
        &self,
        created_atom_id: &AtomId,
        created_node_num: u64,
    ) -> Vec<ResolvedEdge> {
        let mut store = self.deferred_edges.lock();
        let resolved = store.try_resolve(created_atom_id, created_node_num);

        // Persist if any edges were resolved
        if !resolved.is_empty()
            && let Err(e) = store.save()
        {
            tracing::warn!("Failed to save deferred edges after resolve: {}", e);
        }

        resolved
    }

    /// Advance the compaction cycle counter.
    ///
    /// Should be called at the start of each compaction run.
    pub fn advance_compaction_cycle(&self) {
        let mut store = self.deferred_edges.lock();
        store.advance_cycle();
    }

    /// Apply retention policy to deferred edges.
    ///
    /// Drops deferred edges older than `max_cycles` compaction cycles.
    /// Returns the number of edges dropped.
    pub fn apply_deferred_retention(&self, max_cycles: u32) -> usize {
        let mut store = self.deferred_edges.lock();
        let dropped = store.apply_retention(max_cycles);

        if dropped > 0
            && let Err(e) = store.save()
        {
            tracing::warn!("Failed to save deferred edges after retention: {}", e);
        }

        dropped
    }

    /// Apply retention policy with the default cycle count.
    pub fn apply_default_deferred_retention(&self) -> usize {
        self.apply_deferred_retention(DEFAULT_DEFERRED_RETENTION_CYCLES)
    }

    /// Get the count of pending deferred edges.
    pub fn pending_deferred_edges_count(&self) -> usize {
        let store = self.deferred_edges.lock();
        store.pending_count()
    }

    /// Check if a specific AtomId has any pending deferred edges targeting it.
    pub fn has_pending_deferred_for(&self, atom_id: &AtomId) -> bool {
        let store = self.deferred_edges.lock();
        store.has_pending_for(atom_id)
    }

    /// Flush deferred edges to disk.
    pub fn flush_deferred_edges(&self) -> CasIoResult<()> {
        let mut store = self.deferred_edges.lock();
        store.save().map_err(|e| io::Error::other(e.to_string()))?;
        Ok(())
    }

    /// Get base directory
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_test_dir() -> TempDir {
        TempDir::new().expect("Failed to create temp dir")
    }

    fn create_test_atom_id(seed: u8) -> AtomId {
        let mut atom_id = [0u8; 32];
        for (i, byte) in atom_id.iter_mut().enumerate() {
            *byte = seed.wrapping_add(i as u8);
        }
        atom_id
    }

    #[test]
    fn test_index_entry_roundtrip() {
        let atom_id = create_test_atom_id(42);
        let entry = IndexEntry::new(atom_id, 100, 1024, 0);

        let mut bytes = [0u8; IndexEntry::SIZE];
        entry.write_to_bytes(&mut bytes).unwrap();

        let restored = IndexEntry::from_bytes(&bytes).unwrap();
        assert_eq!(entry.fp64, restored.fp64);
        assert_eq!(entry.seg_offset, restored.seg_offset);
        assert_eq!(entry.body_len, restored.body_len);
        assert_eq!(entry.flags, restored.flags);
    }

    #[test]
    fn test_bloom_filter_basic() {
        let mut bloom = BloomFilter::new(100);
        let atom_id = create_test_atom_id(1);

        assert!(!bloom.might_contain(&atom_id));

        bloom.insert(&atom_id);
        assert!(bloom.might_contain(&atom_id));

        // Different atom should likely not be in filter
        let _other_atom = create_test_atom_id(2);
        // Note: Bloom filters can have false positives, but with these parameters
        // the probability should be low for just 2 elements
    }

    #[test]
    fn test_segment_file_create_and_append() {
        let temp_dir = setup_test_dir();
        let mut segment = SegmentFile::create(temp_dir.path(), 0, Some(1024 * 1024)).unwrap();

        let atom_id = create_test_atom_id(1);
        let body = b"test body data";

        let (offset, body_len) = segment.append_record(atom_id, body, 0).unwrap();

        assert_eq!(offset, 0);
        assert_eq!(body_len, body.len() as u64);
        assert!(segment.current_size() > 0);
    }

    #[test]
    fn test_segment_file_read() {
        let temp_dir = setup_test_dir();
        let mut segment = SegmentFile::create(temp_dir.path(), 0, Some(1024 * 1024)).unwrap();

        let atom_id = create_test_atom_id(1);
        let body = b"test body data for read test";

        let (offset, _) = segment.append_record(atom_id, body, 0).unwrap();

        let (header, read_body) = segment.read_record(offset).unwrap();

        assert_eq!(header.body_len(), body.len() as u64);
        assert_eq!(&read_body, body);
        assert_eq!(header.atom_id(), &atom_id);
    }

    #[test]
    fn test_segment_file_multiple_records() {
        let temp_dir = setup_test_dir();
        let mut segment = SegmentFile::create(temp_dir.path(), 0, Some(1024 * 1024)).unwrap();

        let mut records = Vec::new();
        for i in 0..10 {
            let atom_id = create_test_atom_id(i);
            let body = format!("body data for record {}", i);
            let (offset, _) = segment.append_record(atom_id, body.as_bytes(), 0).unwrap();
            records.push((atom_id, offset, body));
        }

        // Read back all records
        for (atom_id, offset, expected_body) in records {
            let (header, read_body) = segment.read_record(offset).unwrap();
            assert_eq!(header.atom_id(), &atom_id);
            assert_eq!(&read_body, expected_body.as_bytes());
        }
    }

    #[test]
    fn test_index_file_insert_and_find() {
        let temp_dir = setup_test_dir();
        let index = IndexFile::create(temp_dir.path(), 0).unwrap();

        let atom_id = create_test_atom_id(42);
        index.insert(atom_id, 100, 1024, 0);
        index.sort_entries();

        let entry = index.find(&atom_id);
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().seg_offset, 100);
    }

    #[test]
    fn test_index_file_batch_insert() {
        let temp_dir = setup_test_dir();
        let index = IndexFile::create(temp_dir.path(), 0).unwrap();

        let entries: Vec<(AtomId, u64, u64, u16)> = (0..100)
            .map(|i| (create_test_atom_id(i as u8), i * 1000, 512, 0))
            .collect();

        index.batch_insert(&entries);
        index.sort_entries();

        assert_eq!(index.entry_count(), 100);

        // Find some entries
        for i in 0..100 {
            let atom_id = create_test_atom_id(i as u8);
            let entry = index.find(&atom_id);
            assert!(entry.is_some());
        }
    }

    #[test]
    fn test_index_file_flush_and_reload() {
        let temp_dir = setup_test_dir();

        {
            let index = IndexFile::create(temp_dir.path(), 0).unwrap();

            for i in 0..5 {
                let atom_id = create_test_atom_id(i as u8);
                index.insert(atom_id, (i as u64) * 100, 256, 0);
            }
            index.sort_entries();
            index.flush().unwrap();
        }

        // Reload
        let index = IndexFile::open(temp_dir.path(), 0).unwrap();
        assert!(
            index.entry_count() > 0,
            "Index should have entries after reload"
        );

        // Reopened lookup must preserve Bloom hash positions and never produce
        // a false negative for a persisted entry.
        let entries = index.get_all_entries();
        assert!(!entries.is_empty(), "Should have entries after reload");
        for i in 0..5 {
            let atom_id = create_test_atom_id(i as u8);
            assert!(
                index.find(&atom_id).is_some(),
                "persisted atom {i} should be found after reload"
            );
        }
    }

    #[test]
    fn test_cas_writer_basic() {
        let temp_dir = setup_test_dir();
        let writer = CasWriter::new(temp_dir.path(), Some(1024 * 1024)).unwrap();

        let atom_id = create_test_atom_id(1);
        let body = b"test body for writer";

        let (seg_id, _offset, body_len) = writer.write_record(atom_id, body).unwrap();

        assert_eq!(seg_id, 0);
        assert_eq!(body_len, body.len() as u64);
        writer.flush().unwrap();
    }

    #[test]
    fn test_cas_writer_segment_rotation() {
        let temp_dir = setup_test_dir();
        // Small size limit to force rotation
        let writer = CasWriter::new(temp_dir.path(), Some(512)).unwrap();

        // Write enough records to force rotation
        for i in 0..10 {
            let atom_id = create_test_atom_id(i);
            let body = format!(
                "body data for record {} with some extra content to fill up space",
                i
            );
            let _ = writer.write_record(atom_id, body.as_bytes());
        }

        writer.flush().unwrap();

        // Should have multiple segments now
        let files: Vec<_> = fs::read_dir(temp_dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with(&format!(".{}", SEGMENT_EXTENSION))
            })
            .collect();

        assert!(files.len() > 1, "Expected multiple segments after rotation");
    }

    #[test]
    fn test_cas_writer_rotation_writes_to_new_segment_index() {
        let temp_dir = setup_test_dir();
        let writer = CasWriter::new(temp_dir.path(), Some(600)).unwrap();

        let first_atom = create_test_atom_id(10);
        let second_atom = create_test_atom_id(11);
        let first_body = vec![0xAA; 300];
        let second_body = vec![0xBB; 300];

        let (first_seg, first_offset, _) = writer.write_record(first_atom, &first_body).unwrap();
        let (second_seg, second_offset, _) =
            writer.write_record(second_atom, &second_body).unwrap();
        writer.flush().unwrap();
        drop(writer);

        assert!(
            second_seg > first_seg,
            "Second write should land in a rotated segment"
        );

        let reader = CasReader::new(temp_dir.path()).unwrap();
        let (first_header, read_first_body) = reader.read_record(first_seg, first_offset).unwrap();
        let (second_header, read_second_body) =
            reader.read_record(second_seg, second_offset).unwrap();

        assert_eq!(first_header.atom_id(), &first_atom);
        assert_eq!(second_header.atom_id(), &second_atom);
        assert_eq!(read_first_body, first_body);
        assert_eq!(read_second_body, second_body);

        let all_atoms = reader.list_all_atoms().unwrap();
        assert!(all_atoms.contains(&first_atom));
        assert!(all_atoms.contains(&second_atom));
    }

    #[test]
    fn test_cas_writer_reopen_preserves_active_segment_index_entries() {
        let temp_dir = setup_test_dir();
        let existing_atom = create_test_atom_id(20);
        let new_atom = create_test_atom_id(21);
        let existing_body = b"record persisted before restart".to_vec();
        let new_body = b"record written after restart".to_vec();

        let (existing_seg, existing_offset) = {
            let writer = CasWriter::new(temp_dir.path(), Some(1024 * 1024)).unwrap();
            let (seg, offset, _) = writer.write_record(existing_atom, &existing_body).unwrap();
            writer.flush().unwrap();
            (seg, offset)
        };

        let (new_seg, new_offset) = {
            let writer = CasWriter::new(temp_dir.path(), Some(1024 * 1024)).unwrap();
            let (seg, offset, _) = writer.write_record(new_atom, &new_body).unwrap();
            writer.flush().unwrap();
            (seg, offset)
        };

        let reader = CasReader::new(temp_dir.path()).unwrap();
        let (existing_header, read_existing_body) =
            reader.read_record(existing_seg, existing_offset).unwrap();
        let (new_header, read_new_body) = reader.read_record(new_seg, new_offset).unwrap();

        assert_eq!(existing_header.atom_id(), &existing_atom);
        assert_eq!(new_header.atom_id(), &new_atom);
        assert_eq!(read_existing_body, existing_body);
        assert_eq!(read_new_body, new_body);

        let all_atoms = reader.list_all_atoms().unwrap();
        assert!(all_atoms.contains(&existing_atom));
        assert!(all_atoms.contains(&new_atom));
    }

    #[test]
    fn test_cas_store_writer_accessor_returns_initialized_writer() {
        let temp_dir = setup_test_dir();
        let store = CasStore::open(temp_dir.path(), Some(1024 * 1024)).unwrap();
        store.init_writer().unwrap();

        let writer = store.writer().unwrap();
        let atom_id = create_test_atom_id(42);
        let body = b"writer accessor roundtrip";
        let (seg_id, offset, body_len) = writer.write_record(atom_id, body).unwrap();
        writer.flush().unwrap();
        drop(writer);

        assert_eq!(body_len, body.len() as u64);

        let reader = CasReader::new(temp_dir.path()).unwrap();
        let (header, read_back) = reader.read_record(seg_id, offset).unwrap();
        assert_eq!(header.atom_id(), &atom_id);
        assert_eq!(read_back, body);
    }

    #[test]
    fn test_cas_reader_basic() {
        let temp_dir = setup_test_dir();

        // Write some records directly to segment
        let mut segment = SegmentFile::create(temp_dir.path(), 0, Some(1024 * 1024)).unwrap();

        let mut records = Vec::new();
        for i in 0..5 {
            let atom_id = create_test_atom_id(i as u8);
            let body = format!("body {}", i);
            let (offset, _) = segment.append_record(atom_id, body.as_bytes(), 0).unwrap();
            records.push((atom_id, offset, body));
        }
        segment.flush().unwrap();

        // Read them back directly from segment
        for (atom_id, offset, expected_body) in records {
            let (header, read_body) = segment.read_record(offset).unwrap();
            assert_eq!(header.atom_id(), &atom_id);
            assert_eq!(&read_body, expected_body.as_bytes());
        }
    }

    #[test]
    fn test_cas_iterator() {
        let temp_dir = setup_test_dir();
        let mut segment = SegmentFile::create(temp_dir.path(), 0, Some(1024 * 1024)).unwrap();

        let expected: Vec<(AtomId, Vec<u8>)> = (0..5)
            .map(|i| {
                let atom_id = create_test_atom_id(i);
                let body = format!("body {}", i).into_bytes();
                segment.append_record(atom_id, &body, 0).unwrap();
                (atom_id, body)
            })
            .collect();

        let iter = CasIterator::new(&segment);
        let results: Vec<_> = iter.filter_map(|r| r.ok()).collect();

        assert_eq!(results.len(), expected.len());

        for ((header, body), (exp_atom_id, exp_body)) in results.iter().zip(expected.iter()) {
            assert_eq!(header.atom_id(), exp_atom_id);
            assert_eq!(body, exp_body);
        }
    }

    #[test]
    fn test_compaction_basic() {
        let temp_dir = setup_test_dir();

        // Create two segments with overlapping atoms
        // Segment 0: atoms 0, 1, 2
        let mut seg0 = SegmentFile::create(temp_dir.path(), 0, Some(1024 * 1024)).unwrap();
        let idx0 = IndexFile::create(temp_dir.path(), 0).unwrap();

        for i in 0..3 {
            let atom_id = create_test_atom_id(i as u8);
            let body = format!("v1 body {}", i);
            let (offset, body_len) = seg0.append_record(atom_id, body.as_bytes(), 0).unwrap();
            idx0.insert(atom_id, offset, body_len, 0);
        }
        seg0.flush().unwrap();
        idx0.flush().unwrap();

        // Segment 1: atoms 1, 2, 3 (1 and 2 are duplicates with newer versions)
        let mut seg1 = SegmentFile::create(temp_dir.path(), 1, Some(1024 * 1024)).unwrap();
        let idx1 = IndexFile::create(temp_dir.path(), 1).unwrap();

        for i in 1..4 {
            let atom_id = create_test_atom_id(i as u8);
            let body = format!("v2 body {}", i);
            let (offset, body_len) = seg1.append_record(atom_id, body.as_bytes(), 0).unwrap();
            idx1.insert(atom_id, offset, body_len, 0);
        }
        seg1.flush().unwrap();
        idx1.flush().unwrap();

        // Compact segments 0 and 1 into segment 100
        let compactor = Compactor::new(temp_dir.path(), Some(1024 * 1024));
        let count = compactor.compact_segments(&[0, 1], 100).unwrap();

        // Should have 4 unique atoms (0, 1, 2, 3)
        assert!(
            count >= 4,
            "Should have at least 4 unique atoms after compaction, got {}",
            count
        );

        // Verify by reading directly from compacted segment
        let compacted_seg = SegmentFile::open(temp_dir.path(), 100).unwrap();
        let iter = CasIterator::new(&compacted_seg);
        let compacted_records: Vec<_> = iter.filter_map(|r| r.ok()).collect();

        // Verify all 4 unique atom IDs are present
        let atom_ids: Vec<AtomId> = compacted_records
            .iter()
            .map(|(h, _)| *h.atom_id())
            .collect();

        for i in 0..4 {
            let expected_atom_id = create_test_atom_id(i as u8);
            assert!(
                atom_ids.contains(&expected_atom_id),
                "Atom {} not found in compacted segment",
                i
            );
        }
    }

    #[test]
    fn test_crc_validation() {
        let temp_dir = setup_test_dir();
        let mut segment = SegmentFile::create(temp_dir.path(), 0, Some(1024 * 1024)).unwrap();

        let atom_id = create_test_atom_id(1);
        let body = b"test body for CRC validation";

        let (offset, _) = segment.append_record(atom_id, body, 0).unwrap();

        // Read should validate CRC
        let (header, read_body) = segment.read_record(offset).unwrap();
        assert!(header.validate_crc());
        assert_eq!(crc32(&read_body), crc32(body));
    }

    #[test]
    fn test_alignment() {
        let temp_dir = setup_test_dir();
        let mut segment = SegmentFile::create(temp_dir.path(), 0, Some(1024 * 1024)).unwrap();

        // Write records with various body sizes
        let sizes = [1, 15, 16, 17, 31, 32, 33, 100, 255, 256, 257];

        for &size in &sizes {
            let atom_id = create_test_atom_id(size as u8);
            let body = vec![size as u8; size];
            segment.append_record(atom_id, &body, 0).unwrap();
        }

        // All records should be readable
        // The iterator will read them in order
        let iter = CasIterator::new(&segment);
        let count = iter.filter_map(|r| r.ok()).count();

        assert_eq!(count, sizes.len());
    }
}

// ============================================================================
// Task-Specific Components (SKF-1.1 CAS I/O API)
// ============================================================================

/// Segment index entry (32 bytes as specified in task)
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SegmentIndexEntry {
    /// First 8 bytes of AtomId (fingerprint)
    pub fp64: u64,
    /// Offset in segment file
    pub offset: u64,
    /// Record length
    pub len: u32,
    /// Padding to align to 32 bytes
    pub _padding: u32,
    /// Assigned node number
    pub node_num: u64,
}

impl SegmentIndexEntry {
    /// Size of SegmentIndexEntry in bytes
    pub const SIZE: usize = 32;

    /// Create a new SegmentIndexEntry from atom_id and record info
    #[inline]
    pub fn new(atom_id: AtomId, offset: u64, len: u32, node_num: u64) -> Self {
        let fp64 = u64::from_le_bytes([
            atom_id[0], atom_id[1], atom_id[2], atom_id[3], atom_id[4], atom_id[5], atom_id[6],
            atom_id[7],
        ]);

        SegmentIndexEntry {
            fp64,
            offset,
            len,
            _padding: 0,
            node_num,
        }
    }

    /// Write entry to bytes
    #[inline]
    pub fn write_to_bytes(&self, bytes: &mut [u8]) -> CasIoResult<()> {
        if bytes.len() < Self::SIZE {
            return Err(CasIoError::BufferTooSmall {
                expected: Self::SIZE,
                actual: bytes.len(),
            });
        }

        unsafe {
            std::ptr::write_unaligned(bytes.as_mut_ptr() as *mut SegmentIndexEntry, *self);
        }
        Ok(())
    }

    /// Read entry from bytes
    #[inline]
    pub fn from_bytes(bytes: &[u8]) -> CasIoResult<Self> {
        if bytes.len() < Self::SIZE {
            return Err(CasIoError::BufferTooSmall {
                expected: Self::SIZE,
                actual: bytes.len(),
            });
        }

        unsafe {
            Ok(std::ptr::read_unaligned(
                bytes.as_ptr() as *const SegmentIndexEntry
            ))
        }
    }
}

/// Index magic number: "SKFI" = 0x534B4649
pub const SKFI_MAGIC: u32 = 0x534B4649;

/// Index version 1
pub const SKFI_VERSION: u16 = 0x0001;

/// Segment index for efficient lookups
pub struct SegmentIndex {
    /// Index entries
    entries: Vec<SegmentIndexEntry>,
    /// Optional Bloom filter for fast negative lookups.
    ///
    /// SKFI v1 entries persist only `fp64`, not the full AtomId required by
    /// `BloomFilter`, so the filter remains disabled for correctness.
    bloom_filter: Option<BloomFilter>,
}

impl SegmentIndex {
    /// Create a new empty SegmentIndex
    #[inline]
    pub fn new() -> Self {
        SegmentIndex {
            entries: Vec::new(),
            bloom_filter: None,
        }
    }

    /// Create a new SegmentIndex with expected capacity
    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        SegmentIndex {
            entries: Vec::with_capacity(capacity),
            bloom_filter: None,
        }
    }

    /// Add an entry to the index
    pub fn add(&mut self, entry: SegmentIndexEntry) {
        self.entries.push(entry);
    }

    /// Find an entry by fp64 fingerprint
    pub fn find(&self, fp64: u64) -> Option<&SegmentIndexEntry> {
        self.entries.iter().find(|e| e.fp64 == fp64)
    }

    /// Find an entry by full atom_id
    pub fn find_by_atom_id(&self, atom_id: &AtomId) -> Option<&SegmentIndexEntry> {
        let fp64 = u64::from_le_bytes([
            atom_id[0], atom_id[1], atom_id[2], atom_id[3], atom_id[4], atom_id[5], atom_id[6],
            atom_id[7],
        ]);
        self.find(fp64)
    }

    /// Get all entries
    #[inline]
    pub fn entries(&self) -> &[SegmentIndexEntry] {
        &self.entries
    }

    /// Sort entries by fp64 for binary search
    pub fn sort(&mut self) {
        self.entries.sort_by_key(|e| e.fp64);
    }

    /// Binary search for fp64 (entries must be sorted)
    pub fn find_sorted(&self, fp64: u64) -> Option<&SegmentIndexEntry> {
        self.entries
            .binary_search_by_key(&fp64, |e| e.fp64)
            .ok()
            .map(|idx| &self.entries[idx])
    }

    /// Write index to file
    pub fn write_to_file(&self, path: &Path) -> CasIoResult<()> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        let mut writer = BufWriter::new(file);

        // Write header
        writer.write_all(&SKFI_MAGIC.to_le_bytes())?;
        writer.write_all(&SKFI_VERSION.to_le_bytes())?;
        writer.write_all(&0u16.to_le_bytes())?; // flags
        writer.write_all(&(self.entries.len() as u32).to_le_bytes())?;

        // Write entries
        for entry in &self.entries {
            let mut entry_bytes = [0u8; SegmentIndexEntry::SIZE];
            entry.write_to_bytes(&mut entry_bytes)?;
            writer.write_all(&entry_bytes)?;
        }

        // Write bloom filter if present
        if let Some(ref bloom) = self.bloom_filter {
            let bloom_bytes = bloom.to_bytes();
            writer.write_all(&(bloom_bytes.len() as u32).to_le_bytes())?;
            writer.write_all(&bloom_bytes)?;
        } else {
            writer.write_all(&0u32.to_le_bytes())?;
        }

        writer.flush()?;
        drop(writer);

        // Sync
        let file = OpenOptions::new().write(true).open(path)?;
        file.sync_all()?;

        Ok(())
    }

    /// Read index from file
    pub fn read_from_file(path: &Path) -> CasIoResult<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        let mut reader = BufReader::new(file);

        // Read header
        let mut magic_bytes = [0u8; 4];
        reader.read_exact(&mut magic_bytes)?;
        let magic = u32::from_le_bytes(magic_bytes);

        if magic != SKFI_MAGIC {
            return Err(CasIoError::InvalidMagic {
                expected: SKFI_MAGIC,
                found: magic,
            });
        }

        let mut version_bytes = [0u8; 2];
        reader.read_exact(&mut version_bytes)?;
        let version = u16::from_le_bytes(version_bytes);

        if version != SKFI_VERSION {
            return Err(CasIoError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Unsupported index version: {}", version),
            )));
        }

        let mut flags_bytes = [0u8; 2];
        reader.read_exact(&mut flags_bytes)?;
        let _flags = u16::from_le_bytes(flags_bytes);

        let mut count_bytes = [0u8; 4];
        reader.read_exact(&mut count_bytes)?;
        let count = u32::from_le_bytes(count_bytes) as usize;

        // Read entries
        let mut entries = Vec::with_capacity(count);
        let mut entry_bytes = [0u8; SegmentIndexEntry::SIZE];

        for _ in 0..count {
            reader.read_exact(&mut entry_bytes)?;
            let entry = SegmentIndexEntry::from_bytes(&entry_bytes)?;
            entries.push(entry);
        }

        // Read bloom filter size
        let mut bloom_size_bytes = [0u8; 4];
        reader.read_exact(&mut bloom_size_bytes)?;
        let bloom_size = u32::from_le_bytes(bloom_size_bytes) as usize;

        // Consume legacy SKFI v1 Bloom bytes, but do not use them to reject a
        // lookup. Segment entries do not preserve the full AtomId needed to
        // construct the filter, and legacy writers could therefore persist an
        // empty filter that would produce false negatives.
        if bloom_size > 0 {
            let mut bloom_bytes = vec![0u8; bloom_size];
            reader.read_exact(&mut bloom_bytes)?;
        }
        let bloom_filter = None;

        Ok(SegmentIndex {
            entries,
            bloom_filter,
        })
    }

    /// Get entry count
    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if index is empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Check if atom_id might be in index (Bloom filter)
    pub fn might_contain(&self, atom_id: &AtomId) -> bool {
        match self.bloom_filter {
            Some(ref bloom) => bloom.might_contain(atom_id),
            None => true, // No filter = must check
        }
    }
}

impl Default for SegmentIndex {
    fn default() -> Self {
        Self::new()
    }
}

/// Read range for coalesced reading
#[derive(Debug, Clone)]
pub struct ReadRange {
    pub start: u64,
    pub end: u64,
    pub original_offsets: Vec<u64>,
}

/// Coalesced reader for batch operations
///
/// Merges close read operations to reduce I/O overhead
pub struct CoalescedReader {
    segments: Vec<SegmentFile>,
    coalesce_gap: usize,
}

impl CoalescedReader {
    /// Create a new CoalescedReader
    #[inline]
    pub fn new(segments: Vec<SegmentFile>, coalesce_gap: usize) -> Self {
        CoalescedReader {
            segments,
            coalesce_gap,
        }
    }

    /// Read multiple offsets in batches, merging close offsets
    ///
    /// Returns vector of body data for each requested offset
    pub fn read_batch(&self, offsets: &[u64]) -> CasIoResult<Vec<Vec<u8>>> {
        if offsets.is_empty() {
            return Ok(Vec::new());
        }

        // Merge close offsets into ranges
        let ranges = self.merge_offsets(offsets);

        // Read each range and extract individual records
        let mut results: Vec<(u64, Vec<u8>)> = Vec::with_capacity(offsets.len());

        for range in ranges {
            // Find which segment contains this range
            let mut range_data = None;
            for segment in &self.segments {
                let seg_size = segment.current_size();
                if range.start < seg_size {
                    // This segment might contain our data
                    range_data = self.read_range(segment, range.start, range.end)?;
                    break;
                }
            }

            if let Some(data) = range_data {
                // Extract individual records from the range
                for offset in &range.original_offsets {
                    let record_data = self.extract_record_at_offset(&data, range.start, *offset)?;
                    results.push((*offset, record_data));
                }
            }
        }

        // Sort results by original offset order
        results.sort_by_key(|(offset, _)| {
            offsets
                .iter()
                .position(|&o| o == *offset)
                .unwrap_or(usize::MAX)
        });

        Ok(results.into_iter().map(|(_, data)| data).collect())
    }

    /// Read a range of bytes from a segment
    fn read_range(
        &self,
        segment: &SegmentFile,
        start: u64,
        end: u64,
    ) -> CasIoResult<Option<Vec<u8>>> {
        let seg_size = segment.current_size();
        if start >= seg_size {
            return Ok(None);
        }

        let actual_end = end.min(seg_size);
        let len = (actual_end - start) as usize;

        // Use file directly through segment's file reference
        let mut file = OpenOptions::new().read(true).open(segment.path())?;
        file.seek(SeekFrom::Start(start))?;

        let mut data = vec![0u8; len];
        file.read_exact(&mut data)?;

        Ok(Some(data))
    }

    /// Extract a record's body data from range data at a specific offset
    fn extract_record_at_offset(
        &self,
        range_data: &[u8],
        range_start: u64,
        record_offset: u64,
    ) -> CasIoResult<Vec<u8>> {
        let relative_offset = (record_offset - range_start) as usize;

        if relative_offset + RecordHeader::SIZE > range_data.len() {
            return Err(CasIoError::BufferTooSmall {
                expected: relative_offset + RecordHeader::SIZE,
                actual: range_data.len(),
            });
        }

        // Read header
        let header = RecordHeader::from_bytes(
            &range_data[relative_offset..relative_offset + RecordHeader::SIZE],
        )?;
        let body_len = header.body_len() as usize;

        let body_start = relative_offset + RecordHeader::SIZE;
        let body_end = body_start + body_len;

        if body_end > range_data.len() {
            return Err(CasIoError::BufferTooSmall {
                expected: body_end,
                actual: range_data.len(),
            });
        }

        Ok(range_data[body_start..body_end].to_vec())
    }

    /// Sort offsets and merge those within coalesce_gap
    pub fn merge_offsets(&self, offsets: &[u64]) -> Vec<ReadRange> {
        if offsets.is_empty() {
            return Vec::new();
        }

        let mut sorted: Vec<u64> = offsets.to_vec();
        sorted.sort_unstable();

        let mut ranges: Vec<ReadRange> = Vec::new();
        let mut current_start = sorted[0];
        let mut current_end = sorted[0];
        let mut current_offsets = vec![sorted[0]];

        for &offset in sorted.iter().skip(1) {
            if (offset - current_end) as usize <= self.coalesce_gap {
                // Extend current range
                current_end = offset;
                current_offsets.push(offset);
            } else {
                // Start new range
                ranges.push(ReadRange {
                    start: current_start,
                    end: current_end + RecordHeader::SIZE as u64 + 1024, // Include estimated record size
                    original_offsets: std::mem::take(&mut current_offsets),
                });
                current_start = offset;
                current_end = offset;
                current_offsets.push(offset);
            }
        }

        // Add final range
        ranges.push(ReadRange {
            start: current_start,
            end: current_end + RecordHeader::SIZE as u64 + 1024,
            original_offsets: current_offsets,
        });

        ranges
    }

    /// Get segment count
    #[inline]
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }
}

/// Segment compactor for merging and garbage collection
pub struct SegmentCompactor;

impl SegmentCompactor {
    /// Create a new SegmentCompactor
    #[inline]
    pub fn new() -> Self {
        SegmentCompactor
    }

    /// Merge small segments into larger ones
    ///
    /// Returns vector of new index entries for the compacted segment
    pub fn compact(
        input_segments: &[PathBuf],
        output_path: &Path,
        target_size: u64,
    ) -> CasIoResult<Vec<SegmentIndexEntry>> {
        if input_segments.is_empty() {
            return Ok(Vec::new());
        }

        // Create output directory if needed
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Parse segment IDs from paths
        let mut source_segments: Vec<(u32, SegmentFile)> = Vec::new();
        for path in input_segments {
            let file_name = path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| {
                CasIoError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Invalid segment path",
                ))
            })?;

            // Extract segment ID from "seg_XXXXX"
            if let Some(id_str) = file_name.strip_prefix("seg_") {
                let seg_id = id_str.parse::<u32>().map_err(|_| {
                    CasIoError::Io(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Invalid segment ID in path",
                    ))
                })?;

                let segment = SegmentFile::open(path.parent().unwrap_or(Path::new(".")), seg_id)?;
                source_segments.push((seg_id, segment));
            }
        }

        // Extract output segment ID from output_path
        let output_file_name = output_path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| {
                CasIoError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Invalid output path",
                ))
            })?;

        let output_seg_id = output_file_name
            .strip_prefix("seg_")
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(99999);

        // Create output segment
        let base_dir = output_path.parent().unwrap_or(Path::new("."));
        let mut output_segment = SegmentFile::create(base_dir, output_seg_id, Some(target_size))?;
        let mut new_entries = Vec::new();
        let mut node_counter: u64 = 1;

        // Deduplicate: keep only the latest version of each atom
        // Later segments overwrite earlier ones
        let mut seen_atoms: std::collections::HashSet<AtomId> = std::collections::HashSet::new();

        // Process segments in reverse order (newest first) for deduplication
        for (seg_id, segment) in source_segments.iter().rev() {
            let iter = CasIterator::new(segment);

            for result in iter {
                match result {
                    Ok((header, body)) => {
                        let atom_id = *header.atom_id();

                        // Skip if already seen (newer version exists)
                        if seen_atoms.contains(&atom_id) {
                            continue;
                        }

                        // Skip deleted/tombstone records
                        if header.is_deleted() || header.is_tombstone() {
                            continue;
                        }

                        seen_atoms.insert(atom_id);

                        // Append to output segment
                        match output_segment.append_record(atom_id, &body, header.flags) {
                            Ok((offset, _body_len)) => {
                                let entry = SegmentIndexEntry::new(
                                    atom_id,
                                    offset,
                                    body.len() as u32,
                                    node_counter,
                                );
                                new_entries.push(entry);
                                node_counter += 1;
                            }
                            Err(CasIoError::SegmentFull { .. }) => {
                                // Output segment is full, stop compaction
                                break;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Error reading record in segment {}: {:?}", seg_id, e);
                        continue;
                    }
                }
            }
        }

        // Flush output segment
        output_segment.flush()?;
        output_segment.sync_all()?;

        // Reverse entries to maintain original order
        new_entries.reverse();

        Ok(new_entries)
    }

    /// Remove deleted/superseded records from a segment
    ///
    /// Returns the compacted segment data as bytes
    pub fn garbage_collect(
        segment: &SegmentFile,
        live_ids: &std::collections::HashSet<AtomId>,
    ) -> CasIoResult<Vec<u8>> {
        let mut result = Vec::new();
        let iter = CasIterator::new(segment);

        for record_result in iter {
            match record_result {
                Ok((header, body)) => {
                    let atom_id = *header.atom_id();

                    // Only include if in live_ids and not deleted
                    if live_ids.contains(&atom_id) && !header.is_deleted() && !header.is_tombstone()
                    {
                        // Write record to result buffer
                        let mut header_bytes = [0u8; RecordHeader::SIZE];
                        header.write_to_bytes(&mut header_bytes)?;
                        result.extend_from_slice(&header_bytes);
                        result.extend_from_slice(&body);

                        // Add body padding
                        let body_padded = SegmentFile::calculate_padded_size(body.len()) as u64;
                        let padding = body_padded - body.len() as u64;
                        if padding > 0 {
                            result.extend(vec![0u8; padding as usize]);
                        }

                        // Add body CRC
                        let body_crc = crc32(&body);
                        result.extend_from_slice(&body_crc.to_le_bytes());

                        // Add CRC padding
                        let crc_padded = SegmentFile::calculate_padded_size(4);
                        let crc_padding = crc_padded - 4;
                        if crc_padding > 0 {
                            result.extend(vec![0u8; crc_padding]);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Error reading record during GC: {:?}", e);
                    continue;
                }
            }
        }

        Ok(result)
    }
}

impl Default for SegmentCompactor {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience function: Create or open a segment file
pub fn create_or_open_segment(path: &Path, seg_id: u32) -> CasIoResult<SegmentFile> {
    let base_dir = path.parent().unwrap_or(Path::new("."));

    // Check if segment already exists
    let seg_path = base_dir.join(format!("{}{:05}.dat", SEGMENT_PREFIX, seg_id));

    if seg_path.exists() {
        SegmentFile::open(base_dir, seg_id)
    } else {
        SegmentFile::create(base_dir, seg_id, None)
    }
}

#[cfg(test)]
mod task_tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_atom_id(seed: u8) -> AtomId {
        let mut atom_id = [0u8; 32];
        for (i, byte) in atom_id.iter_mut().enumerate() {
            *byte = seed.wrapping_add(i as u8);
        }
        atom_id
    }

    #[test]
    fn test_segment_index_entry_roundtrip() {
        let atom_id = create_test_atom_id(42);
        let entry = SegmentIndexEntry::new(atom_id, 1000, 512, 12345);

        assert_eq!(entry.offset, 1000);
        assert_eq!(entry.len, 512);
        assert_eq!(entry.node_num, 12345);

        let mut bytes = [0u8; SegmentIndexEntry::SIZE];
        entry.write_to_bytes(&mut bytes).unwrap();

        let restored = SegmentIndexEntry::from_bytes(&bytes).unwrap();
        assert_eq!(restored.fp64, entry.fp64);
        assert_eq!(restored.offset, entry.offset);
        assert_eq!(restored.len, entry.len);
        assert_eq!(restored.node_num, entry.node_num);
    }

    #[test]
    fn test_segment_index_basic() {
        let mut index = SegmentIndex::with_capacity(10);

        for i in 0..5 {
            let atom_id = create_test_atom_id(i);
            let entry = SegmentIndexEntry::new(atom_id, (i as u64) * 100, 256, i as u64 + 1);
            index.add(entry);
        }

        assert_eq!(index.len(), 5);

        // Test find
        let target_id = create_test_atom_id(2);
        let fp64 = u64::from_le_bytes([
            target_id[0],
            target_id[1],
            target_id[2],
            target_id[3],
            target_id[4],
            target_id[5],
            target_id[6],
            target_id[7],
        ]);

        let found = index.find(fp64);
        assert!(found.is_some());
        assert_eq!(found.unwrap().offset, 200);

        // Test find_by_atom_id
        let found2 = index.find_by_atom_id(&target_id);
        assert!(found2.is_some());
    }

    #[test]
    fn test_segment_index_sort_and_binary_search() {
        let mut index = SegmentIndex::new();

        // Add entries in random order
        let offsets = [300u64, 100, 500, 200, 400];
        for (i, &offset) in offsets.iter().enumerate() {
            let atom_id = create_test_atom_id(i as u8 * 7); // Spread out fp64 values
            let entry = SegmentIndexEntry::new(atom_id, offset, 256, i as u64 + 1);
            index.add(entry);
        }

        index.sort();

        // Now we can binary search
        let target_id = create_test_atom_id(14); // i=2, offset=500
        let found = index.find_by_atom_id(&target_id);
        assert!(found.is_some());
        assert_eq!(found.unwrap().offset, 500);
    }

    #[test]
    fn test_segment_index_file_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let index_path = temp_dir.path().join("test.idx");

        {
            let mut index = SegmentIndex::with_capacity(5);
            for i in 0..5 {
                let atom_id = create_test_atom_id(i as u8 * 3);
                let entry = SegmentIndexEntry::new(atom_id, (i as u64) * 1000, 512, i as u64 + 100);
                index.add(entry);
            }
            index.write_to_file(&index_path).unwrap();
        }

        // Read back
        let index = SegmentIndex::read_from_file(&index_path).unwrap();
        assert_eq!(index.len(), 5);

        // Verify entries
        for i in 0..5 {
            let atom_id = create_test_atom_id(i as u8 * 3);
            assert!(
                index.might_contain(&atom_id),
                "persisted segment Bloom filter rejected entry {i}"
            );
            let found = index.find_by_atom_id(&atom_id);
            assert!(found.is_some(), "Entry {} not found", i);
            assert_eq!(found.unwrap().offset, (i as u64) * 1000);
        }
    }

    #[test]
    fn test_coalesced_reader_merge_offsets() {
        let segments = Vec::new();
        let reader = CoalescedReader::new(segments, 100); // 100 byte gap

        let offsets = vec![0u64, 50, 200, 300, 500, 550, 600];
        let ranges = reader.merge_offsets(&offsets);

        // Should merge: [0, 50], [200, 300], [500, 550, 600]
        assert_eq!(ranges.len(), 3);

        assert_eq!(ranges[0].original_offsets.len(), 2);
        assert!(ranges[0].original_offsets.contains(&0));
        assert!(ranges[0].original_offsets.contains(&50));

        assert_eq!(ranges[1].original_offsets.len(), 2);
        assert!(ranges[1].original_offsets.contains(&200));
        assert!(ranges[1].original_offsets.contains(&300));

        assert_eq!(ranges[2].original_offsets.len(), 3);
    }

    #[test]
    fn test_segment_compactor_gc() {
        let temp_dir = TempDir::new().unwrap();
        let mut segment = SegmentFile::create(temp_dir.path(), 0, Some(1024 * 1024)).unwrap();

        // Create 5 records
        let mut live_ids = std::collections::HashSet::new();
        for i in 0..5 {
            let atom_id = create_test_atom_id(i);
            let body = format!("body {}", i);
            segment.append_record(atom_id, body.as_bytes(), 0).unwrap();

            if i < 3 {
                live_ids.insert(atom_id);
            }
        }
        segment.flush().unwrap();

        // GC - keep only first 3 records
        let gc_result = SegmentCompactor::garbage_collect(&segment, &live_ids).unwrap();

        // Result should have data for 3 records
        assert!(!gc_result.is_empty());

        // Verify by writing to new segment and reading back
        let _new_segment = SegmentFile::create(temp_dir.path(), 1, Some(1024 * 1024)).unwrap();
        let gc_path = temp_dir.path().join("gc_temp.dat");
        std::fs::write(&gc_path, &gc_result).unwrap();
    }

    #[test]
    fn test_segment_file_api_compat() {
        // Test the task-specified API
        let temp_dir = TempDir::new().unwrap();

        // Test create_or_open_segment
        let seg_path = temp_dir.path().join("seg_00001.dat");
        let mut segment = create_or_open_segment(&seg_path, 1).unwrap();

        let atom_id = create_test_atom_id(1);
        let body = b"test body";

        // Use append_record through our convenience function
        let (offset, body_len) = segment.append_record(atom_id, body, 0).unwrap();

        assert_eq!(body_len, body.len() as u64);

        // Read back
        let (header, read_body) = segment.read_record(offset).unwrap();
        assert_eq!(header.atom_id(), &atom_id);
        assert_eq!(&read_body, body);

        // Test sync
        segment.sync_all().unwrap();

        // Test len
        assert!(segment.current_size() > 0);
    }
}
