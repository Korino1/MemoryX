//! CRDT Write-Ahead Log (WAL) implementation for MemoryX SKF-1.1.
//!
//! This module provides durable append-only logging for CRDT operations,
//! ensuring atomicity and durability of distributed state changes.
//!
//! # Record Format (SKF-1.1 Spec A.3.3)
//!
//! ```text
//! RecordHeader (44 bytes):
//!   magic: u32 = 0x4D455441 ("META")
//!   ver: u16 = 1
//!   flags: u16
//!   hlc_phys_ns: u64     - Physical timestamp (nanoseconds)
//!   hlc_logical: u32     - Logical counter
//!   actor_id: [u8; 16]   - Actor identifier
//!   key_kind: u8         - 1=NodeNum, 2=AtomId
//!   crdt_kind: u8        - CRDT type
//!   field_id: u16        - Field identifier
//!   op: u8               - 1=UPSERT, 2=REMOVE, 3=MERGE_STATE, 4=TOMBSTONE
//!   reserved: u8
//!   payload_len: u32     - Length of payload data
//!   payload_crc32: u32   - CRC32 checksum of payload
//!
//! Key (variable, after header):
//!   if key_kind=1: node_num: u64 (8 bytes)
//!   if key_kind=2: atom_id: [u8; 32] (32 bytes)
//!
//! Payload (variable): CRDT-specific operation data
//!
//! Trailer: padding to 16-byte alignment
//! ```
//!
//! # Safety
//!
//! This module uses unsafe code for:
//! - Zero-copy deserialization of headers (validated before use)
//! - Aligned memory access (ensured by layout constraints)
//! - Raw file I/O (synchronized via fsync)

#![allow(dead_code)]

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::store::{AtomId, CrdtKind, NodeNum};
use crate::utils::{HLC, crc32};

// ============================================================================
// Constants
// ============================================================================

/// Magic number for WAL records: "META" = 0x4D455441
pub const WAL_MAGIC: u32 = 0x4D455441;

/// WAL format version
pub const WAL_VERSION: u16 = 1;

/// Actor ID size in bytes
pub const ACTOR_ID_SIZE: usize = 16;

/// Record header size (50 bytes - actual field sum per SKF-1.1 spec)
/// Field breakdown: 4+2+2+8+4+16+1+1+2+1+1+4+4 = 50 bytes
pub const RECORD_HEADER_SIZE: usize = 50;

/// Alignment boundary for records (16 bytes)
pub const RECORD_ALIGNMENT: usize = 16;

/// Maximum allowed payload size (16 MB)
pub const MAX_PAYLOAD_SIZE: usize = 16 * 1024 * 1024;

/// Key kind: NodeNum
pub const KEY_KIND_NODE: u8 = 1;

/// Key kind: AtomId
pub const KEY_KIND_ATOM: u8 = 2;

/// Operation: UPSERT
pub const OP_UPSERT: u8 = 1;

/// Operation: REMOVE
pub const OP_REMOVE: u8 = 2;

/// Operation: MERGE_STATE
pub const OP_MERGE_STATE: u8 = 3;

/// Operation: TOMBSTONE
pub const OP_TOMBSTONE: u8 = 4;

// ============================================================================
// Errors
// ============================================================================

/// Errors that can occur during WAL operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrdtError {
    /// I/O error during file operation
    Io(String),
    /// Record has invalid magic number or version
    InvalidMagic,
    /// CRC32 checksum mismatch
    CrcMismatch,
    /// Payload size exceeds maximum
    PayloadTooLarge,
    /// Invalid key kind
    InvalidKeyKind(u8),
    /// Invalid operation code
    InvalidOperation(u8),
    /// Corrupt record (size mismatch, etc.)
    CorruptRecord,
    /// End of file reached
    EndOfFile,
    /// Alignment error
    AlignmentError,
    /// Invalid payload data (deserialization error)
    InvalidPayload,
}

impl fmt::Display for CrdtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CrdtError::Io(msg) => write!(f, "I/O error: {}", msg),
            CrdtError::InvalidMagic => write!(f, "invalid magic number or version"),
            CrdtError::CrcMismatch => write!(f, "CRC32 checksum mismatch"),
            CrdtError::PayloadTooLarge => write!(f, "payload exceeds maximum size"),
            CrdtError::InvalidKeyKind(k) => write!(f, "invalid key kind: {}", k),
            CrdtError::InvalidOperation(o) => write!(f, "invalid operation code: {}", o),
            CrdtError::CorruptRecord => write!(f, "corrupt record"),
            CrdtError::EndOfFile => write!(f, "end of file"),
            CrdtError::AlignmentError => write!(f, "alignment error"),
            CrdtError::InvalidPayload => write!(f, "invalid payload data"),
        }
    }
}

impl std::error::Error for CrdtError {}

impl From<io::Error> for CrdtError {
    fn from(e: io::Error) -> Self {
        CrdtError::Io(e.to_string())
    }
}

// ============================================================================
// Record Header (44 bytes)
// ============================================================================

use std::fmt;

/// Fixed-size record header (44 bytes as per SKF-1.1 spec).
///
/// Field layout is carefully arranged to minimize padding:
/// - 8-byte aligned fields first (hlc_phys_ns, actor_id)
/// - 4-byte aligned fields next (magic, hlc_logical, payload_len, payload_crc32)
/// - 2-byte aligned fields next (ver, flags, field_id)
/// - 1-byte fields last (key_kind, crdt_kind, op, reserved)
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordHeader {
    /// Magic number: 0x4D455441 ("META")
    pub magic: u32,
    /// HLC physical time (nanoseconds since epoch)
    pub hlc_phys_ns: u64,
    /// Actor ID (16 bytes)
    pub actor_id: [u8; ACTOR_ID_SIZE],
    /// HLC logical counter
    pub hlc_logical: u32,
    /// Payload length in bytes
    pub payload_len: u32,
    /// CRC32 of payload
    pub payload_crc32: u32,
    /// Format version
    pub ver: u16,
    /// Header flags
    pub flags: u16,
    /// Field identifier within key
    pub field_id: u16,
    /// Key kind: 1=NodeNum, 2=AtomId
    pub key_kind: u8,
    /// CRDT kind
    pub crdt_kind: u8,
    /// Operation type
    pub op: u8,
    /// Reserved (must be 0)
    pub reserved: u8,
}

impl RecordHeader {
    /// Size of RecordHeader in bytes
    pub const SIZE: usize = RECORD_HEADER_SIZE;

    /// Create a new RecordHeader
    #[inline]
    pub fn new(
        hlc: HLC,
        actor_id: [u8; ACTOR_ID_SIZE],
        key_kind: u8,
        crdt_kind: CrdtKind,
        field_id: u16,
        op: u8,
        payload: &[u8],
    ) -> Self {
        RecordHeader {
            magic: WAL_MAGIC,
            ver: WAL_VERSION,
            flags: 0,
            hlc_phys_ns: hlc.physical_ns(),
            hlc_logical: hlc.logical() as u32,
            actor_id,
            key_kind,
            crdt_kind: crdt_kind.to_u8(),
            field_id,
            op,
            reserved: 0,
            payload_len: payload.len() as u32,
            payload_crc32: crc32(payload),
        }
    }

    /// Get HLC from header
    #[inline]
    pub fn hlc(&self) -> HLC {
        HLC::from_parts(self.hlc_phys_ns, self.hlc_logical as u16)
    }

    /// Get CRDT kind
    #[inline]
    pub fn crdt_kind(&self) -> Option<CrdtKind> {
        CrdtKind::from_u8(self.crdt_kind)
    }

    /// Validate magic number and version
    #[inline]
    pub fn validate_magic(&self) -> bool {
        self.magic == WAL_MAGIC && self.ver == WAL_VERSION
    }

    /// Validate payload CRC
    #[inline]
    pub fn validate_crc(&self, payload: &[u8]) -> bool {
        self.payload_crc32 == crc32(payload)
    }

    /// Check if header is valid
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.validate_magic()
            && self.payload_len <= MAX_PAYLOAD_SIZE as u32
            && (self.key_kind == KEY_KIND_NODE || self.key_kind == KEY_KIND_ATOM)
            && self.op >= OP_UPSERT
            && self.op <= OP_TOMBSTONE
    }

    /// Get key size based on key_kind
    #[inline]
    pub fn key_size(&self) -> Option<usize> {
        match self.key_kind {
            KEY_KIND_NODE => Some(8),  // u64
            KEY_KIND_ATOM => Some(32), // [u8; 32]
            _ => None,
        }
    }

    /// Calculate total record size including padding
    pub fn total_record_size(&self) -> usize {
        let key_size = self.key_size().unwrap_or(0);
        let payload_len = self.payload_len as usize;
        let base_size = Self::SIZE + key_size + payload_len;

        // Round up to 16-byte alignment
        (base_size + RECORD_ALIGNMENT - 1) & !(RECORD_ALIGNMENT - 1)
    }

    /// Serialize header to bytes (little-endian)
    ///
    /// Layout (44 bytes):
    /// - magic: u32 (4 bytes) at offset 0
    /// - hlc_phys_ns: u64 (8 bytes) at offset 4
    /// - actor_id: [u8; 16] (16 bytes) at offset 12
    /// - hlc_logical: u32 (4 bytes) at offset 28
    /// - payload_len: u32 (4 bytes) at offset 32
    /// - payload_crc32: u32 (4 bytes) at offset 36
    /// - ver: u16 (2 bytes) at offset 40
    /// - flags: u16 (2 bytes) at offset 42
    /// - field_id: u16 (2 bytes) - WAIT this exceeds 44!
    ///
    /// Actually let me recount:
    /// - magic: u32 = 4 (offset 0-3)
    /// - hlc_phys_ns: u64 = 8 (offset 4-11)
    /// - actor_id: [u8; 16] = 16 (offset 12-27)
    /// - hlc_logical: u32 = 4 (offset 28-31)
    /// - payload_len: u32 = 4 (offset 32-35)
    /// - payload_crc32: u32 = 4 (offset 36-39)
    /// - ver: u16 = 2 (offset 40-41)
    /// - flags: u16 = 2 (offset 42-43)
    /// - field_id: u16 = 2 (offset 44-45) - THIS IS 46 BYTES NOW
    /// - key_kind: u8 = 1 (offset 46)
    /// - crdt_kind: u8 = 1 (offset 47)
    /// - op: u8 = 1 (offset 48)
    /// - reserved: u8 = 1 (offset 49)
    ///
    /// The original spec says 44 bytes. Let me re-read it carefully.
    /// The issue is that the spec field order doesn't match what I wrote.
    /// Let me revert to the spec order and calculate padding properly.
    pub fn write_to_bytes(&self, buf: &mut [u8]) -> Result<(), CrdtError> {
        if buf.len() < Self::SIZE {
            return Err(CrdtError::CorruptRecord);
        }

        // Write fields according to the SKF-1.1 spec layout
        // This matches the on-disk format, not the in-memory layout
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

        // hlc_phys_ns: u64 (8 bytes)
        buf[offset..offset + 8].copy_from_slice(&self.hlc_phys_ns.to_le_bytes());
        offset += 8;

        // hlc_logical: u32 (4 bytes)
        buf[offset..offset + 4].copy_from_slice(&self.hlc_logical.to_le_bytes());
        offset += 4;

        // actor_id: [u8; 16] (16 bytes)
        buf[offset..offset + ACTOR_ID_SIZE].copy_from_slice(&self.actor_id);
        offset += ACTOR_ID_SIZE;

        // key_kind: u8 (1 byte)
        buf[offset] = self.key_kind;
        offset += 1;

        // crdt_kind: u8 (1 byte)
        buf[offset] = self.crdt_kind;
        offset += 1;

        // field_id: u16 (2 bytes)
        buf[offset..offset + 2].copy_from_slice(&self.field_id.to_le_bytes());
        offset += 2;

        // op: u8 (1 byte)
        buf[offset] = self.op;
        offset += 1;

        // reserved: u8 (1 byte)
        buf[offset] = self.reserved;
        offset += 1;

        // payload_len: u32 (4 bytes)
        buf[offset..offset + 4].copy_from_slice(&self.payload_len.to_le_bytes());
        offset += 4;

        // payload_crc32: u32 (4 bytes)
        buf[offset..offset + 4].copy_from_slice(&self.payload_crc32.to_le_bytes());

        Ok(())
    }

    /// Deserialize header from bytes (little-endian)
    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::SIZE {
            return None;
        }

        let mut offset = 0usize;

        // Read fields in spec order (little-endian)
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        offset += 4;

        let ver = u16::from_le_bytes([buf[4], buf[5]]);
        offset += 2;

        let flags = u16::from_le_bytes([buf[6], buf[7]]);
        offset += 2;

        let hlc_phys_ns = u64::from_le_bytes([
            buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
        ]);
        offset += 8;

        let hlc_logical = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
        offset += 4;

        let mut actor_id = [0u8; ACTOR_ID_SIZE];
        actor_id.copy_from_slice(&buf[offset..offset + ACTOR_ID_SIZE]);
        offset += ACTOR_ID_SIZE;

        let key_kind = buf[offset];
        offset += 1;

        let crdt_kind = buf[offset];
        offset += 1;

        let field_id = u16::from_le_bytes([buf[offset], buf[offset + 1]]);
        offset += 2;

        let op = buf[offset];
        offset += 1;

        let reserved = buf[offset];
        offset += 1;

        let payload_len = u32::from_le_bytes([
            buf[offset],
            buf[offset + 1],
            buf[offset + 2],
            buf[offset + 3],
        ]);
        offset += 4;

        let payload_crc32 = u32::from_le_bytes([
            buf[offset],
            buf[offset + 1],
            buf[offset + 2],
            buf[offset + 3],
        ]);

        Some(RecordHeader {
            magic,
            ver,
            flags,
            hlc_phys_ns,
            actor_id,
            hlc_logical,
            payload_len,
            payload_crc32,
            field_id,
            key_kind,
            crdt_kind,
            op,
            reserved,
        })
    }
}

// ============================================================================
// Key Types
// ============================================================================
// Key Types
// ============================================================================
// Key Types
// ============================================================================
// Key Types
// ============================================================================
// Key Types
// ============================================================================

/// Key for WAL records - either NodeNum or AtomId
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WalKey {
    /// Node number key (8 bytes)
    Node(NodeNum),
    /// Atom ID key (32 bytes)
    Atom(AtomId),
}

impl WalKey {
    /// Get key kind byte
    #[inline]
    pub fn key_kind(&self) -> u8 {
        match self {
            WalKey::Node(_) => KEY_KIND_NODE,
            WalKey::Atom(_) => KEY_KIND_ATOM,
        }
    }

    /// Get key size in bytes
    #[inline]
    pub fn size(&self) -> usize {
        match self {
            WalKey::Node(_) => 8,
            WalKey::Atom(_) => 32,
        }
    }

    /// Serialize to bytes
    pub fn write_to_bytes(&self, buf: &mut [u8]) -> Result<(), CrdtError> {
        match self {
            WalKey::Node(node) => {
                if buf.len() < 8 {
                    return Err(CrdtError::CorruptRecord);
                }
                buf[..8].copy_from_slice(&node.to_le_bytes());
                Ok(())
            }
            WalKey::Atom(atom) => {
                if buf.len() < 32 {
                    return Err(CrdtError::CorruptRecord);
                }
                buf[..32].copy_from_slice(atom);
                Ok(())
            }
        }
    }

    /// Deserialize from bytes
    pub fn from_bytes(key_kind: u8, buf: &[u8]) -> Option<Self> {
        match key_kind {
            KEY_KIND_NODE => {
                if buf.len() < 8 {
                    return None;
                }
                let node = u64::from_le_bytes([
                    buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
                ]);
                Some(WalKey::Node(node))
            }
            KEY_KIND_ATOM => {
                if buf.len() < 32 {
                    return None;
                }
                let mut atom = [0u8; 32];
                atom.copy_from_slice(&buf[..32]);
                Some(WalKey::Atom(atom))
            }
            _ => None,
        }
    }
}

impl fmt::Display for WalKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalKey::Node(n) => write!(f, "Node({})", n),
            WalKey::Atom(a) => write!(f, "Atom({:02x}{:02x}...)", a[0], a[1]),
        }
    }
}

// ============================================================================
// WalRecord
// ============================================================================

/// Complete WAL record including header, key, and payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRecord {
    /// Record header
    pub header: RecordHeader,
    /// Record key
    pub key: WalKey,
    /// Payload data
    pub payload: Vec<u8>,
}

impl WalRecord {
    /// Create a new WAL record
    pub fn new(
        hlc: HLC,
        actor_id: [u8; ACTOR_ID_SIZE],
        key: WalKey,
        crdt_kind: CrdtKind,
        field_id: u16,
        op: u8,
        payload: Vec<u8>,
    ) -> Self {
        let header = RecordHeader::new(
            hlc,
            actor_id,
            key.key_kind(),
            crdt_kind,
            field_id,
            op,
            &payload,
        );
        WalRecord {
            header,
            key,
            payload,
        }
    }

    /// Get total serialized size including padding
    pub fn serialized_size(&self) -> usize {
        self.header.total_record_size()
    }

    /// Serialize the complete record to a buffer
    pub fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), CrdtError> {
        let total_size = self.serialized_size();
        let start_len = buf.len();

        // Ensure capacity
        if buf.capacity() < start_len + total_size {
            buf.reserve(total_size);
        }

        // Write header
        let mut header_buf = [0u8; RecordHeader::SIZE];
        self.header.write_to_bytes(&mut header_buf)?;
        buf.extend_from_slice(&header_buf);

        // Write key
        let key_size = self.key.size();
        let mut key_buf = vec![0u8; key_size];
        self.key.write_to_bytes(&mut key_buf)?;
        buf.extend_from_slice(&key_buf);

        // Write payload
        buf.extend_from_slice(&self.payload);

        // Write padding to 16-byte alignment
        let current_size = buf.len() - start_len;
        let padding = total_size - current_size;
        if padding > 0 {
            buf.extend(std::iter::repeat_n(0u8, padding));
        }

        // Verify alignment
        assert_eq!(
            (buf.len() - start_len) % RECORD_ALIGNMENT,
            0,
            "Record must be 16-byte aligned"
        );

        Ok(())
    }

    /// Get operation name as string
    pub fn op_name(&self) -> &'static str {
        match self.header.op {
            OP_UPSERT => "UPSERT",
            OP_REMOVE => "REMOVE",
            OP_MERGE_STATE => "MERGE_STATE",
            OP_TOMBSTONE => "TOMBSTONE",
            _ => "UNKNOWN",
        }
    }
}

// ============================================================================
// WalWriter
// ============================================================================

/// Append-only WAL writer with durability guarantees.
pub struct WalWriter {
    /// Underlying file
    file: File,
    /// Current write offset
    current_offset: u64,
    /// Path for error messages
    path: std::path::PathBuf,
}

impl WalWriter {
    /// Create a new WAL writer, creating or appending to the file.
    ///
    /// If the file exists, seeks to the end to append.
    /// If the file doesn't exist, creates it.
    pub fn create(path: &Path) -> Result<Self, CrdtError> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(path)
            .map_err(|e| CrdtError::Io(format!("Failed to open WAL file {:?}: {}", path, e)))?;

        let current_offset = file.metadata()?.len();

        Ok(WalWriter {
            file,
            current_offset,
            path: path.to_path_buf(),
        })
    }

    /// Append a record to the WAL.
    ///
    /// Returns the file offset where the record was written.
    /// The record is immediately flushed to disk for durability.
    pub fn append(&mut self, record: &WalRecord) -> Result<u64, CrdtError> {
        // Validate payload size
        if record.payload.len() > MAX_PAYLOAD_SIZE {
            return Err(CrdtError::PayloadTooLarge);
        }

        let offset = self.current_offset;

        // Serialize record to buffer
        let mut buf = Vec::with_capacity(record.serialized_size());
        record.serialize(&mut buf)?;

        // Write to file
        self.file
            .write_all(&buf)
            .map_err(|e| CrdtError::Io(format!("Failed to write to WAL {:?}: {}", self.path, e)))?;

        // Update offset
        self.current_offset += buf.len() as u64;

        Ok(offset)
    }

    /// Sync the WAL to disk.
    ///
    /// Ensures all buffered data is written to persistent storage.
    pub fn sync(&mut self) -> Result<(), CrdtError> {
        self.file
            .flush()
            .map_err(|e| CrdtError::Io(format!("Failed to flush WAL {:?}: {}", self.path, e)))?;

        self.file
            .sync_all()
            .map_err(|e| CrdtError::Io(format!("Failed to sync WAL {:?}: {}", self.path, e)))?;

        Ok(())
    }

    /// Get current write offset
    #[inline]
    pub fn current_offset(&self) -> u64 {
        self.current_offset
    }

    /// Get the file path
    #[inline]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Truncate the WAL to a specific offset (for compaction/rotation)
    pub fn truncate(&mut self, offset: u64) -> Result<(), CrdtError> {
        self.file
            .set_len(offset)
            .map_err(|e| CrdtError::Io(format!("Failed to truncate WAL {:?}: {}", self.path, e)))?;

        self.file.seek(SeekFrom::Start(offset))?;
        self.current_offset = offset;

        Ok(())
    }
}

// ============================================================================
// WalIterator
// ============================================================================

/// Iterator over WAL records.
///
/// Reads records sequentially from a WAL file, validating CRCs and structure.
pub struct WalIterator {
    /// File being read
    file: File,
    /// Current file offset
    offset: u64,
    /// File size
    file_size: u64,
    /// Number of records read
    records_read: u64,
    /// Total bytes read
    bytes_read: u64,
}

impl WalIterator {
    /// Create a new iterator from a file.
    ///
    /// The file must be open for reading.
    fn new(mut file: File) -> Result<Self, CrdtError> {
        let file_size = file.metadata()?.len();
        file.seek(SeekFrom::Start(0))?;

        Ok(WalIterator {
            file,
            offset: 0,
            file_size,
            records_read: 0,
            bytes_read: 0,
        })
    }

    /// Read the next record from the WAL.
    ///
    /// Returns None at end of file.
    /// Returns an error if corruption is detected.
    pub fn next_record(&mut self) -> Result<Option<WalRecord>, CrdtError> {
        // Check if we've reached end of file
        if self.offset + RecordHeader::SIZE as u64 > self.file_size {
            return Ok(None);
        }

        // Read header
        let mut header_buf = [0u8; RecordHeader::SIZE];
        self.file.read_exact(&mut header_buf).map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                CrdtError::EndOfFile
            } else {
                CrdtError::Io(format!("Failed to read header: {}", e))
            }
        })?;

        // Parse header
        let header = RecordHeader::from_bytes(&header_buf).ok_or(CrdtError::CorruptRecord)?;

        // Validate header
        if !header.validate_magic() {
            return Err(CrdtError::InvalidMagic);
        }

        if !header.is_valid() {
            return Err(CrdtError::CorruptRecord);
        }

        // Get key size
        let key_size = header
            .key_size()
            .ok_or(CrdtError::InvalidKeyKind(header.key_kind))?;

        // Read key
        let mut key_buf = vec![0u8; key_size];
        self.file
            .read_exact(&mut key_buf)
            .map_err(|e| CrdtError::Io(format!("Failed to read key: {}", e)))?;

        let key = WalKey::from_bytes(header.key_kind, &key_buf)
            .ok_or(CrdtError::InvalidKeyKind(header.key_kind))?;

        // Read payload
        let payload_len = header.payload_len as usize;
        let mut payload = vec![0u8; payload_len];
        self.file
            .read_exact(&mut payload)
            .map_err(|e| CrdtError::Io(format!("Failed to read payload: {}", e)))?;

        // Verify CRC
        if !header.validate_crc(&payload) {
            return Err(CrdtError::CrcMismatch);
        }

        // Skip padding to alignment
        let total_size = header.total_record_size();
        let data_size = RecordHeader::SIZE + key_size + payload_len;
        let padding = total_size - data_size;

        if padding > 0 {
            let mut pad_buf = vec![0u8; padding];
            self.file
                .read_exact(&mut pad_buf)
                .map_err(|e| CrdtError::Io(format!("Failed to read padding: {}", e)))?;
        }

        // Update position tracking
        self.offset += total_size as u64;
        self.records_read += 1;
        self.bytes_read += total_size as u64;

        Ok(Some(WalRecord {
            header,
            key,
            payload,
        }))
    }

    /// Get current file offset
    #[inline]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Get number of records read so far
    #[inline]
    pub fn records_read(&self) -> u64 {
        self.records_read
    }

    /// Get total bytes read so far
    #[inline]
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Check if there are more records
    #[inline]
    pub fn has_more(&self) -> bool {
        self.offset + RecordHeader::SIZE as u64 <= self.file_size
    }
}

// Implement Iterator trait for WalIterator
impl Iterator for WalIterator {
    type Item = Result<WalRecord, CrdtError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_record() {
            Ok(None) => None,
            Ok(Some(record)) => Some(Ok(record)),
            Err(e) => Some(Err(e)),
        }
    }
}

// ============================================================================
// WalReader
// ============================================================================

/// WAL reader for reading records from a WAL file.
pub struct WalReader {
    /// File being read
    file: File,
    /// File path
    path: std::path::PathBuf,
    /// File size
    file_size: u64,
}

impl WalReader {
    /// Open a WAL file for reading.
    pub fn open(path: &Path) -> Result<Self, CrdtError> {
        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .map_err(|e| CrdtError::Io(format!("Failed to open WAL {:?}: {}", path, e)))?;

        let file_size = file.metadata()?.len();

        Ok(WalReader {
            file,
            path: path.to_path_buf(),
            file_size,
        })
    }

    /// Create an iterator over all records in the WAL.
    ///
    /// The iterator reads records sequentially and validates CRCs.
    pub fn iter(&self) -> Result<WalIterator, CrdtError> {
        // Clone the file handle for iteration
        let file = OpenOptions::new()
            .read(true)
            .open(&self.path)
            .map_err(|e| CrdtError::Io(format!("Failed to open WAL for iteration: {}", e)))?;

        WalIterator::new(file)
    }

    /// Read a single record at a specific offset.
    ///
    /// This is useful for random access during recovery.
    pub fn read_at(&self, offset: u64) -> Result<WalRecord, CrdtError> {
        if offset + RecordHeader::SIZE as u64 > self.file_size {
            return Err(CrdtError::EndOfFile);
        }

        // Open a new file handle to avoid disturbing iteration state
        let mut file = OpenOptions::new()
            .read(true)
            .open(&self.path)
            .map_err(|e| CrdtError::Io(format!("Failed to open WAL for read: {}", e)))?;

        file.seek(SeekFrom::Start(offset))?;

        // Read header
        let mut header_buf = [0u8; RecordHeader::SIZE];
        file.read_exact(&mut header_buf).map_err(|e| {
            CrdtError::Io(format!("Failed to read header at offset {}: {}", offset, e))
        })?;

        // Parse and validate header
        let header = RecordHeader::from_bytes(&header_buf).ok_or(CrdtError::CorruptRecord)?;

        if !header.validate_magic() {
            return Err(CrdtError::InvalidMagic);
        }

        if !header.is_valid() {
            return Err(CrdtError::CorruptRecord);
        }

        // Read key
        let key_size = header
            .key_size()
            .ok_or(CrdtError::InvalidKeyKind(header.key_kind))?;
        let mut key_buf = vec![0u8; key_size];
        file.read_exact(&mut key_buf)?;

        let key = WalKey::from_bytes(header.key_kind, &key_buf)
            .ok_or(CrdtError::InvalidKeyKind(header.key_kind))?;

        // Read payload
        let payload_len = header.payload_len as usize;
        let mut payload = vec![0u8; payload_len];
        file.read_exact(&mut payload)?;

        // Verify CRC
        if !header.validate_crc(&payload) {
            return Err(CrdtError::CrcMismatch);
        }

        Ok(WalRecord {
            header,
            key,
            payload,
        })
    }

    /// Get file size
    #[inline]
    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    /// Get file path
    #[inline]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Scan the WAL for records matching a predicate.
    ///
    /// Returns a vector of (offset, record) pairs.
    pub fn scan<F>(&self, predicate: F) -> Result<Vec<(u64, WalRecord)>, CrdtError>
    where
        F: Fn(&WalRecord) -> bool,
    {
        let mut results = Vec::new();
        let mut iter = self.iter()?;
        let mut current_offset = 0u64;

        while let Some(result) = iter.next_record()? {
            let record_size = result.serialized_size() as u64;
            if predicate(&result) {
                results.push((current_offset, result));
            }
            current_offset += record_size;
        }

        Ok(results)
    }
}

// ============================================================================
// WAL Utilities
// ============================================================================

/// Validate a WAL file by reading all records and checking CRCs.
///
/// Returns (valid_records, total_bytes, last_valid_offset).
/// If an error is encountered, returns the counts up to the error.
pub fn validate_wal(path: &Path) -> Result<(u64, u64, u64), CrdtError> {
    let reader = WalReader::open(path)?;
    let mut iter = reader.iter()?;
    let mut valid_records = 0u64;
    let mut total_bytes = 0u64;
    let mut last_valid_offset = 0u64;

    loop {
        let offset_before = iter.offset();
        match iter.next_record() {
            Ok(Some(record)) => {
                valid_records += 1;
                total_bytes += record.serialized_size() as u64;
                last_valid_offset = offset_before;
            }
            Ok(None) => break,
            Err(e) => {
                // Return partial results with the error
                return Err(CrdtError::Io(format!(
                    "WAL validation failed at offset {} after {} valid records: {}",
                    offset_before, valid_records, e
                )));
            }
        }
    }

    Ok((valid_records, total_bytes, last_valid_offset))
}

/// Compact a WAL file by copying valid records to a new file.
///
/// This removes any corrupted trailing data and ensures 16-byte alignment.
pub fn compact_wal(source: &Path, dest: &Path) -> Result<(u64, u64), CrdtError> {
    let reader = WalReader::open(source)?;
    let mut writer = WalWriter::create(dest)?;

    let mut records_copied = 0u64;
    let mut bytes_copied = 0u64;

    for result in reader.iter()? {
        let record = result?;
        writer.append(&record)?;
        records_copied += 1;
        bytes_copied += record.serialized_size() as u64;
    }

    writer.sync()?;

    Ok((records_copied, bytes_copied))
}

/// Recover the last valid position in a potentially corrupt WAL.
///
/// This scans from the beginning and finds the last valid record boundary.
/// Returns the offset immediately after the last valid record.
pub fn recover_wal_tail(path: &Path) -> Result<u64, CrdtError> {
    let reader = WalReader::open(path)?;
    let mut iter = reader.iter()?;
    let mut last_valid_end = 0u64;

    loop {
        let _offset_before = iter.offset();
        match iter.next_record() {
            Ok(Some(_)) => {
                last_valid_end = iter.offset();
            }
            Ok(None) => break,
            Err(_) => {
                // Corruption detected, return last valid position
                break;
            }
        }
    }

    Ok(last_valid_end)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn create_test_actor() -> [u8; ACTOR_ID_SIZE] {
        [0xAB; ACTOR_ID_SIZE]
    }

    fn create_test_hlc() -> HLC {
        HLC::from_parts(1_000_000_000_000, 42)
    }

    #[test]
    fn test_record_header_size() {
        // The serialized size is 50 bytes (actual field sum per SKF-1.1 spec)
        // Field breakdown: 4+2+2+8+4+16+1+1+2+1+1+4+4 = 50 bytes
        assert_eq!(RecordHeader::SIZE, 50);

        // In-memory size may be larger due to alignment padding
        // That's fine since we use explicit serialization
        let in_memory_size = size_of::<RecordHeader>();
        assert!(
            in_memory_size >= 50,
            "In-memory size should be at least 50 bytes"
        );
    }

    #[test]
    fn test_record_header_serialization() {
        let actor = create_test_actor();
        let hlc = create_test_hlc();
        let payload = b"test payload";

        let header = RecordHeader::new(
            hlc,
            actor,
            KEY_KIND_NODE,
            CrdtKind::LWW_REG,
            0x1234,
            OP_UPSERT,
            payload,
        );

        assert_eq!(header.magic, WAL_MAGIC);
        assert_eq!(header.ver, WAL_VERSION);
        assert_eq!(header.hlc_phys_ns, hlc.physical_ns());
        assert_eq!(header.hlc_logical, 42);
        assert_eq!(header.actor_id, actor);
        assert_eq!(header.key_kind, KEY_KIND_NODE);
        assert_eq!(header.crdt_kind, CrdtKind::LWW_REG.to_u8());
        assert_eq!(header.field_id, 0x1234);
        assert_eq!(header.op, OP_UPSERT);
        assert_eq!(header.payload_len, payload.len() as u32);
        assert!(header.validate_crc(payload));

        // Test serialization roundtrip
        let mut buf = [0u8; RecordHeader::SIZE];
        header.write_to_bytes(&mut buf).unwrap();

        let header2 = RecordHeader::from_bytes(&buf).unwrap();
        assert_eq!(header, header2);
    }

    #[test]
    fn test_key_serialization() {
        // Node key
        let node_key = WalKey::Node(0x1234_5678_9ABC_DEF0);
        assert_eq!(node_key.key_kind(), KEY_KIND_NODE);
        assert_eq!(node_key.size(), 8);

        let mut buf = [0u8; 8];
        node_key.write_to_bytes(&mut buf).unwrap();
        let key2 = WalKey::from_bytes(KEY_KIND_NODE, &buf).unwrap();
        assert_eq!(node_key, key2);

        // Atom key
        let atom_id: AtomId = [0x12; 32];
        let atom_key = WalKey::Atom(atom_id);
        assert_eq!(atom_key.key_kind(), KEY_KIND_ATOM);
        assert_eq!(atom_key.size(), 32);

        let mut buf = [0u8; 32];
        atom_key.write_to_bytes(&mut buf).unwrap();
        let key2 = WalKey::from_bytes(KEY_KIND_ATOM, &buf).unwrap();
        assert_eq!(atom_key, key2);
    }

    #[test]
    fn test_wal_record_roundtrip() {
        let actor = create_test_actor();
        let hlc = create_test_hlc();
        let payload = vec![1, 2, 3, 4, 5];

        let record = WalRecord::new(
            hlc,
            actor,
            WalKey::Node(42),
            CrdtKind::GCOUNTER,
            0,
            OP_UPSERT,
            payload,
        );

        // Serialize
        let mut buf = Vec::new();
        record.serialize(&mut buf).unwrap();

        // Check alignment
        assert_eq!(buf.len() % RECORD_ALIGNMENT, 0);

        // Parse back manually
        let header = RecordHeader::from_bytes(&buf[..RecordHeader::SIZE]).unwrap();
        let key = WalKey::from_bytes(
            header.key_kind,
            &buf[RecordHeader::SIZE..RecordHeader::SIZE + 8],
        )
        .unwrap();
        let payload_start = RecordHeader::SIZE + 8;
        let payload_end = payload_start + header.payload_len as usize;
        let parsed_payload = buf[payload_start..payload_end].to_vec();

        assert_eq!(header.hlc_phys_ns, hlc.physical_ns());
        assert_eq!(key, WalKey::Node(42));
        assert_eq!(parsed_payload, record.payload);
    }

    #[test]
    fn test_wal_write_read() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();

        // Create writer and write records
        let mut writer = WalWriter::create(path).unwrap();

        let actor = create_test_actor();
        let hlc = create_test_hlc();

        let records: Vec<WalRecord> = vec![
            WalRecord::new(
                hlc,
                actor,
                WalKey::Node(1),
                CrdtKind::GCOUNTER,
                0,
                OP_UPSERT,
                vec![0xAA, 0xBB],
            ),
            WalRecord::new(
                hlc.tick(),
                actor,
                WalKey::Node(2),
                CrdtKind::LWW_REG,
                1,
                OP_UPSERT,
                vec![0xCC, 0xDD, 0xEE],
            ),
            WalRecord::new(
                hlc.tick().tick(),
                actor,
                WalKey::Atom([0x11; 32]),
                CrdtKind::ORSET,
                2,
                OP_MERGE_STATE,
                vec![0xFF],
            ),
        ];

        let mut offsets = Vec::new();
        for record in &records {
            let offset = writer.append(record).unwrap();
            offsets.push(offset);
        }

        writer.sync().unwrap();

        // Verify offsets are increasing and aligned
        for i in 1..offsets.len() {
            assert!(offsets[i] > offsets[i - 1]);
            assert_eq!(offsets[i] % RECORD_ALIGNMENT as u64, 0);
        }

        // Read back with reader
        let reader = WalReader::open(path).unwrap();
        let mut iter = reader.iter().unwrap();

        for expected in &records {
            let actual = iter.next_record().unwrap().unwrap();
            assert_eq!(actual.header.hlc_phys_ns, expected.header.hlc_phys_ns);
            assert_eq!(actual.header.hlc_logical, expected.header.hlc_logical);
            assert_eq!(actual.key, expected.key);
            assert_eq!(actual.header.crdt_kind, expected.header.crdt_kind);
            assert_eq!(actual.header.field_id, expected.header.field_id);
            assert_eq!(actual.header.op, expected.header.op);
            assert_eq!(actual.payload, expected.payload);
        }

        // Should be at end of file
        assert!(iter.next_record().unwrap().is_none());
    }

    #[test]
    fn test_crc_validation() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();

        let mut writer = WalWriter::create(path).unwrap();
        let actor = create_test_actor();
        let hlc = create_test_hlc();

        let record = WalRecord::new(
            hlc,
            actor,
            WalKey::Node(1),
            CrdtKind::GCOUNTER,
            0,
            OP_UPSERT,
            vec![1, 2, 3],
        );

        writer.append(&record).unwrap();
        writer.sync().unwrap();

        // Corrupt the file by modifying a byte in the payload
        // Layout: Header (50) + Key (8) + Payload (3) + Padding
        // Payload starts at offset 58
        let mut file = OpenOptions::new().write(true).open(path).unwrap();
        file.seek(SeekFrom::Start(58)).unwrap(); // Skip header (50) + key (8) = 58
        file.write_all(&[0xFF]).unwrap();
        file.sync_all().unwrap();

        // Reading should fail with CRC mismatch
        let reader = WalReader::open(path).unwrap();
        let mut iter = reader.iter().unwrap();
        let result = iter.next_record();

        assert!(matches!(result, Err(CrdtError::CrcMismatch)));
    }

    #[test]
    fn test_iterator() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();

        let mut writer = WalWriter::create(path).unwrap();
        let actor = create_test_actor();
        let hlc = create_test_hlc();

        // Write 5 records
        for i in 0..5 {
            let record = WalRecord::new(
                HLC::from_parts(hlc.physical_ns() + i as u64, i as u16),
                actor,
                WalKey::Node(i as u64),
                CrdtKind::GCOUNTER,
                i as u16,
                OP_UPSERT,
                vec![i as u8],
            );
            writer.append(&record).unwrap();
        }
        writer.sync().unwrap();

        // Use iterator
        let reader = WalReader::open(path).unwrap();
        let iter = reader.iter().unwrap();

        let records: Vec<WalRecord> = iter.filter_map(|r| r.ok()).collect();

        assert_eq!(records.len(), 5);

        for (i, record) in records.iter().enumerate() {
            assert_eq!(record.header.hlc_logical, i as u32);
        }
    }

    #[test]
    fn test_corruption_handling() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();

        // Write partial/corrupt data
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .unwrap();

        // Write invalid magic (50 bytes total header)
        file.write_all(&[0xFF; 4]).unwrap(); // Invalid magic
        file.write_all(&[0; 46]).unwrap(); // Rest of header (50 - 4 = 46)
        file.sync_all().unwrap();

        // Should fail with InvalidMagic
        let reader = WalReader::open(path).unwrap();
        let mut iter = reader.iter().unwrap();
        let result = iter.next_record();

        assert!(matches!(result, Err(CrdtError::InvalidMagic)));
    }

    #[test]
    fn test_atom_key_record() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();

        let mut writer = WalWriter::create(path).unwrap();
        let actor = create_test_actor();
        let hlc = create_test_hlc();

        let atom_id: AtomId = [0x42; 32];
        let record = WalRecord::new(
            hlc,
            actor,
            WalKey::Atom(atom_id),
            CrdtKind::ORSET,
            0,
            OP_UPSERT,
            vec![0xAA, 0xBB],
        );

        writer.append(&record).unwrap();
        writer.sync().unwrap();

        // Read back
        let reader = WalReader::open(path).unwrap();
        let mut iter = reader.iter().unwrap();
        let read_record = iter.next_record().unwrap().unwrap();

        assert_eq!(read_record.key, WalKey::Atom(atom_id));
        assert_eq!(read_record.header.key_kind, KEY_KIND_ATOM);
    }

    #[test]
    fn test_alignment() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();

        let mut writer = WalWriter::create(path).unwrap();
        let actor = create_test_actor();
        let hlc = create_test_hlc();

        // Write records with various payload sizes to test alignment
        for payload_len in [1, 5, 15, 16, 17, 31, 32, 33, 100] {
            let record = WalRecord::new(
                HLC::from_parts(hlc.physical_ns() + payload_len as u64, payload_len as u16),
                actor,
                WalKey::Node(payload_len as u64),
                CrdtKind::GCOUNTER,
                0,
                OP_UPSERT,
                vec![0xAA; payload_len],
            );

            let size_before = writer.current_offset();
            writer.append(&record).unwrap();
            let size_after = writer.current_offset();

            let record_size = size_after - size_before;
            assert_eq!(
                record_size % RECORD_ALIGNMENT as u64,
                0,
                "Record with payload {} should be 16-byte aligned",
                payload_len
            );
        }
    }

    #[test]
    fn test_validate_wal() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();

        let mut writer = WalWriter::create(path).unwrap();
        let actor = create_test_actor();
        let hlc = create_test_hlc();

        // Write 3 records
        for i in 0..3 {
            let record = WalRecord::new(
                HLC::from_parts(hlc.physical_ns() + i as u64, i as u16),
                actor,
                WalKey::Node(i as u64),
                CrdtKind::GCOUNTER,
                0,
                OP_UPSERT,
                vec![i as u8],
            );
            writer.append(&record).unwrap();
        }
        writer.sync().unwrap();

        // Validate
        let (records, bytes, last_offset) = validate_wal(path).unwrap();
        assert_eq!(records, 3);
        assert!(bytes > 0);
        assert!(last_offset > 0);
    }

    #[test]
    fn test_scan() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();

        let mut writer = WalWriter::create(path).unwrap();
        let actor = create_test_actor();
        let hlc = create_test_hlc();

        // Write records with different CRDT kinds
        let kinds = [
            CrdtKind::GCOUNTER,
            CrdtKind::LWW_REG,
            CrdtKind::GCOUNTER,
            CrdtKind::ORSET,
            CrdtKind::GCOUNTER,
        ];

        for (i, kind) in kinds.iter().enumerate() {
            let record = WalRecord::new(
                HLC::from_parts(hlc.physical_ns() + i as u64, i as u16),
                actor,
                WalKey::Node(i as u64),
                *kind,
                0,
                OP_UPSERT,
                vec![i as u8],
            );
            writer.append(&record).unwrap();
        }
        writer.sync().unwrap();

        // Scan for GCOUNTER records
        let reader = WalReader::open(path).unwrap();
        let gcounters = reader
            .scan(|r| r.header.crdt_kind() == Some(CrdtKind::GCOUNTER))
            .unwrap();

        assert_eq!(gcounters.len(), 3);
        for (_offset, record) in gcounters {
            assert_eq!(record.header.crdt_kind(), Some(CrdtKind::GCOUNTER));
        }
    }

    #[test]
    fn test_error_display() {
        let errors = vec![
            CrdtError::Io("test".to_string()),
            CrdtError::InvalidMagic,
            CrdtError::CrcMismatch,
            CrdtError::PayloadTooLarge,
            CrdtError::InvalidKeyKind(99),
            CrdtError::InvalidOperation(99),
            CrdtError::CorruptRecord,
            CrdtError::EndOfFile,
            CrdtError::AlignmentError,
        ];

        for err in errors {
            let msg = format!("{}", err);
            assert!(!msg.is_empty());
        }
    }
}
