//! Content-Addressed Storage for MemoryX SKF-1.1 implementation.
//!
//! This module provides the core CAS structures:
//! - RecordHeader: 64-byte fixed header for each CAS record
//! - AtomBodyHeader: 48-byte header for atom body content
//! - SectionDesc: 32-byte section descriptor
//! - Validation functions for headers and CRC checks
//!
//! # Submodules
//! - io: File I/O for CAS (segments, indexes, compaction)
//! - symbols: SYMBOLS section for string interning
//! - refs: REFS section for AtomId references
//! - claims: CLAIMS section for claim storage
//! - invariants: INVARIANTS section for invariant checking
//! - edges: EDGES section for edge storage
//! - evidence: EVIDENCE section for provenance
//! - meta: META section for metadata
//! - canonical: CanonicalForm serializer and BLAKE3 AtomId generation

#![allow(dead_code)]

pub mod canonical;
pub mod claims;
pub mod edges;
pub mod evidence;
pub mod integrity;
pub mod invariants;
pub mod io;
pub mod meta;
pub mod refs;
pub mod symbols;

use std::fmt;
use std::mem::size_of;

pub use crate::store::{AtomId, AtomType, SectionKind};
pub use crate::utils::crc32;
pub use canonical::{CanonicalClaim, CanonicalForm};
pub use integrity::{IntegrityError, IntegrityReport, IntegrityVerifier};

pub const RECORD_MAGIC: u32 = 0x534B4631;
/// Magic number for AtomBodyHeader: "ATOM" = 0x41544F4D
pub const ATOM_MAGIC: u32 = 0x41544F4D;
/// Format version for RecordHeader: 1.1 = 0x0101
pub const RECORD_FORMAT_VERSION: u16 = 0x0101;
/// Body version for AtomBodyHeader: 1.0 = 0x0001
pub const ATOM_BODY_VERSION: u16 = 0x0001; // ============================================================================// RecordHeader (64 bytes fixed)// ============================================================================
/// Fixed-size header for CAS records///
/// Layout (64 bytes total):
/// - magic: u32 (4 bytes) = 0x534B4631 ("SKF1")
/// - format_ver: u16 (2 bytes) = 0x0101
/// - flags: u16 (2 bytes)
/// - seg_id: u32 (4 bytes)
/// - reserved: u32 (4 bytes)
/// - atom_id: [u8; 32] (32 bytes) - BLAKE3-256 hash
/// - body_len: u64 (8 bytes)
/// - header_crc32: u32 (4 bytes)
/// - reserved2: u32 (4 bytes)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RecordHeader {
    /// Magic number: 0x534B4631 ("SKF1")
    pub magic: u32,

    /// Format version: 0x0101
    pub format_ver: u16,

    /// Flags bitfield
    pub flags: u16,

    /// Segment ID
    pub seg_id: u32,

    /// Reserved (must be 0)
    pub reserved: u32,

    /// Atom ID (BLAKE3-256)
    pub atom_id: AtomId,

    /// Body length in bytes
    pub body_len: u64,

    /// CRC32 of header (excluding this field and reserved2)
    pub header_crc32: u32,

    /// Reserved2 (must be 0)
    pub reserved2: u32,
}

impl RecordHeader {
    /// Size of RecordHeader in bytes    
    pub const SIZE: usize = 64;

    /// Offset of atom_id field    
    pub const ATOM_ID_OFFSET: usize = 16;

    /// Offset of body_len field    
    pub const BODY_LEN_OFFSET: usize = 48;

    /// Offset of header_crc32 field    
    pub const CRC_OFFSET: usize = 56;

    /// Create a new RecordHeader    
    #[inline]
    pub fn new(atom_id: AtomId, body_len: u64, seg_id: u32, flags: u16) -> Self {
        let mut header = RecordHeader {
            magic: RECORD_MAGIC,

            format_ver: RECORD_FORMAT_VERSION,

            flags,

            seg_id,

            reserved: 0,

            atom_id,

            body_len,

            header_crc32: 0, // Will be calculated

            reserved2: 0,
        };

        header.header_crc32 = header.calculate_crc();

        header
    }
    /// Calculate CRC32 of header (excluding header_crc32 and reserved2 fields)    
    #[inline]
    pub fn calculate_crc(&self) -> u32 {
        // Only hash fields up to header_crc32 (56 bytes)

        unsafe {
            let slice = std::slice::from_raw_parts(
                self as *const RecordHeader as *const u8,
                Self::CRC_OFFSET,
            );

            crc32(slice)
        }
    }
    /// Validate the header CRC    
    #[inline]
    pub fn validate_crc(&self) -> bool {
        self.header_crc32 == self.calculate_crc()
    }
    /// Validate magic and version    
    #[inline]
    pub fn validate_magic(&self) -> bool {
        self.magic == RECORD_MAGIC && self.format_ver == RECORD_FORMAT_VERSION
    }
    /// Check if header is valid (magic + CRC)    
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.validate_magic() && self.validate_crc()
    }
    /// Check if the record is marked as deleted/superseded    
    #[inline]
    pub fn is_deleted(&self) -> bool {
        self.flags & RecordFlags::DELETED as u16 != 0
    }
    /// Check if the record is a tombstone    
    #[inline]
    pub fn is_tombstone(&self) -> bool {
        self.flags & RecordFlags::TOMBSTONE as u16 != 0
    }
    /// Check if the record has compressed body    
    #[inline]
    pub fn is_compressed(&self) -> bool {
        self.flags & RecordFlags::COMPRESSED as u16 != 0
    }
    /// Get body length    
    #[inline]
    pub fn body_len(&self) -> u64 {
        self.body_len
    }
    /// Get atom ID reference    
    #[inline]
    pub fn atom_id(&self) -> &AtomId {
        &self.atom_id
    }
    /// Get segment ID    
    #[inline]
    pub fn seg_id(&self) -> u32 {
        self.seg_id
    }
    /// Read RecordHeader from bytes (zero-copy with validation)    
    ///    
    /// # Safety    
    /// - `bytes` must be at least Self::SIZE bytes    
    /// - Caller must ensure proper alignment (8 bytes recommended)    
    #[inline]
    pub unsafe fn from_bytes_unchecked(bytes: &[u8]) -> &Self {
        unsafe { &*(bytes.as_ptr() as *const RecordHeader) }
    }
    /// Read RecordHeader from bytes with validation (returns owned value for safety)    
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CasError> {
        if bytes.len() < Self::SIZE {
            return Err(CasError::BufferTooSmall {
                expected: Self::SIZE,
                actual: bytes.len(),
            });
        }
        // Use unaligned read for safety
        Self::from_bytes_unaligned(bytes)
    }
    /// Read RecordHeader from bytes with unaligned access    
    pub fn from_bytes_unaligned(bytes: &[u8]) -> Result<Self, CasError> {
        if bytes.len() < Self::SIZE {
            return Err(CasError::BufferTooSmall {
                expected: Self::SIZE,
                actual: bytes.len(),
            });
        }

        unsafe {
            let header = std::ptr::read_unaligned(bytes.as_ptr() as *const RecordHeader);

            if !header.validate_magic() {
                return Err(CasError::InvalidMagic {
                    expected: RECORD_MAGIC,
                    found: header.magic,
                });
            }

            if !header.validate_crc() {
                return Err(CasError::CrcMismatch {
                    expected: header.header_crc32,
                    found: header.calculate_crc(),
                });
            }

            Ok(header)
        }
    }
    /// Write RecordHeader to bytes    
    ///    
    /// # Safety    
    /// - `bytes` must be at least Self::SIZE bytes
    pub unsafe fn write_to_bytes_unchecked(&self, bytes: &mut [u8]) {
        unsafe {
            std::ptr::copy_nonoverlapping(
                self as *const RecordHeader as *const u8,
                bytes.as_mut_ptr(),
                Self::SIZE,
            );
        }
    }
    /// Write RecordHeader to bytes (safe version)    
    pub fn write_to_bytes(&self, bytes: &mut [u8]) -> Result<(), CasError> {
        if bytes.len() < Self::SIZE {
            return Err(CasError::BufferTooSmall {
                expected: Self::SIZE,
                actual: bytes.len(),
            });
        }

        unsafe {
            self.write_to_bytes_unchecked(bytes);
        }

        Ok(())
    }
}

impl fmt::Debug for RecordHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordHeader")
            .field("magic", &format_args!("0x{:08X}", self.magic))
            .field("format_ver", &format_args!("0x{:04X}", self.format_ver))
            .field("flags", &format_args!("0x{:04X}", self.flags))
            .field("seg_id", &self.seg_id)
            .field("body_len", &self.body_len)
            .field("atom_id", &hex_encode(&self.atom_id))
            .field("header_crc32", &format_args!("0x{:08X}", self.header_crc32))
            .finish()
    }
} // Verify RecordHeader size at compile time

const _: () = assert!(
    size_of::<RecordHeader>() == 64,
    "RecordHeader must be 64 bytes"
); // ============================================================================// RecordFlags// ============================================================================
/// Bit flags for RecordHeader.flags
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(non_camel_case_types)]
pub enum RecordFlags {
    /// Record is deleted/superseded
    DELETED = 0x0001,

    /// Record is a tombstone (for replication)
    TOMBSTONE = 0x0002,

    /// Body is compressed
    COMPRESSED = 0x0004,

    /// Body is encrypted
    ENCRYPTED = 0x0008,

    /// Record is from federation (imported)
    FEDERATED = 0x0010,

    /// Record has inline edges (optimization)
    INLINE_EDGES = 0x0020,

    /// Record is a redirect to another atom
    REDIRECT = 0x0040,

    /// Reserved for future use
    RESERVED_HIGH = 0x8000,
} // ============================================================================// AtomBodyHeader (48 bytes fixed)// ============================================================================
/// Header for atom body content///
/// Layout (48 bytes total):
/// - body_magic: u32 (4 bytes) = 0x41544F4D ("ATOM")
/// - body_ver: u16 (2 bytes) = 0x0001
/// - body_flags: u16 (2 bytes)
/// - created_at_unix_ns: u64 (8 bytes)
/// - valid_from_unix_ns: u64 (8 bytes)
/// - valid_to_unix_ns: u64 (8 bytes)
/// - atom_type: u32 (4 bytes)
/// - section_count: u32 (4 bytes)
/// - section_table_off: u64 (8 bytes)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AtomBodyHeader {
    /// Magic number: 0x41544F4D ("ATOM")
    pub body_magic: u32,

    /// Body version: 0x0001
    pub body_ver: u16,

    /// Body flags
    pub body_flags: u16,

    /// Creation timestamp (unix nanoseconds)
    pub created_at_unix_ns: u64,

    /// Valid from timestamp (unix nanoseconds)
    pub valid_from_unix_ns: u64,

    /// Valid to timestamp (unix nanoseconds, 0 = infinity)
    pub valid_to_unix_ns: u64,

    /// Atom type
    pub atom_type: u32,

    /// Number of sections
    pub section_count: u32,

    /// Offset to section table (from start of body)
    pub section_table_off: u64,
}

impl AtomBodyHeader {
    /// Size of AtomBodyHeader in bytes    
    pub const SIZE: usize = 48;

    /// Create a new AtomBodyHeader    
    #[inline]
    pub fn new(
        atom_type: AtomType,

        section_count: u32,

        created_at_unix_ns: u64,

        valid_from_unix_ns: u64,

        valid_to_unix_ns: u64,
    ) -> Self {
        AtomBodyHeader {
            body_magic: ATOM_MAGIC,

            body_ver: ATOM_BODY_VERSION,

            body_flags: 0,

            created_at_unix_ns,

            valid_from_unix_ns,

            valid_to_unix_ns,

            atom_type: atom_type.to_u32(),

            section_count,

            section_table_off: Self::SIZE as u64, // Section table follows header
        }
    }
    /// Validate magic and version    
    #[inline]
    pub fn validate_magic(&self) -> bool {
        self.body_magic == ATOM_MAGIC && self.body_ver == ATOM_BODY_VERSION
    }
    /// Check if header is valid    
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.validate_magic() && self.section_count <= 256 // Reasonable limit
    }
    /// Get atom type    
    #[inline]
    pub fn atom_type(&self) -> Option<AtomType> {
        AtomType::from_u32(self.atom_type)
    }
    /// Check if atom is currently valid (within time window)    
    #[inline]
    pub fn is_valid_now(&self, now_unix_ns: u64) -> bool {
        now_unix_ns >= self.valid_from_unix_ns
            && (self.valid_to_unix_ns == 0 || now_unix_ns < self.valid_to_unix_ns)
    }
    /// Check if body flags contain compressed marker    
    #[inline]
    pub fn is_compressed(&self) -> bool {
        self.body_flags & AtomBodyFlags::COMPRESSED as u16 != 0
    }
    /// Read AtomBodyHeader from bytes (returns owned value for safety)    
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CasError> {
        if bytes.len() < Self::SIZE {
            return Err(CasError::BufferTooSmall {
                expected: Self::SIZE,
                actual: bytes.len(),
            });
        }
        Self::from_bytes_unaligned(bytes)
    }
    /// Read AtomBodyHeader from bytes with unaligned access    
    pub fn from_bytes_unaligned(bytes: &[u8]) -> Result<Self, CasError> {
        if bytes.len() < Self::SIZE {
            return Err(CasError::BufferTooSmall {
                expected: Self::SIZE,
                actual: bytes.len(),
            });
        }

        unsafe {
            let header = std::ptr::read_unaligned(bytes.as_ptr() as *const AtomBodyHeader);

            if !header.validate_magic() {
                return Err(CasError::InvalidMagic {
                    expected: ATOM_MAGIC,
                    found: header.body_magic,
                });
            }

            Ok(header)
        }
    }
}

impl fmt::Debug for AtomBodyHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AtomBodyHeader")
            .field("body_magic", &format_args!("0x{:08X}", self.body_magic))
            .field("body_ver", &format_args!("0x{:04X}", self.body_ver))
            .field("body_flags", &format_args!("0x{:04X}", self.body_flags))
            .field("created_at_unix_ns", &self.created_at_unix_ns)
            .field("valid_from_unix_ns", &self.valid_from_unix_ns)
            .field("valid_to_unix_ns", &self.valid_to_unix_ns)
            .field("atom_type", &self.atom_type)
            .field("section_count", &self.section_count)
            .field("section_table_off", &self.section_table_off)
            .finish()
    }
} // Verify AtomBodyHeader size at compile time

const _: () = assert!(
    size_of::<AtomBodyHeader>() == 48,
    "AtomBodyHeader must be 48 bytes"
); // ============================================================================// AtomBodyFlags// ============================================================================
/// Bit flags for AtomBodyHeader.body_flags
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomBodyFlags {
    /// Body is compressed
    COMPRESSED = 0x0001,

    /// Body is encrypted
    ENCRYPTED = 0x0002,

    /// Atom has been validated
    VALIDATED = 0x0004,

    /// Atom is a redirect
    REDIRECT = 0x0008,

    /// Atom is from federation
    FEDERATED = 0x0010,
} // ============================================================================// SectionDesc (32 bytes fixed)// ============================================================================
/// Descriptor for a section within an atom body///
/// Layout (32 bytes total):
/// - section_kind: u32 (4 bytes)
/// - section_flags: u32 (4 bytes)
/// - off: u64 (8 bytes) - offset from start of body
/// - len: u64 (8 bytes) - length in bytes
/// - crc32: u32 (4 bytes) - CRC of section data
/// - reserved: u32 (4 bytes)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SectionDesc {
    /// Section kind
    pub section_kind: u32,

    /// Section flags
    pub section_flags: u32,

    /// Offset from start of body
    pub off: u64,

    /// Length in bytes
    pub len: u64,

    /// CRC32 of section data
    pub crc32: u32,

    /// Reserved (must be 0)
    pub reserved: u32,
}

impl SectionDesc {
    /// Size of SectionDesc in bytes    
    pub const SIZE: usize = 32;

    /// Create a new SectionDesc    
    #[inline]
    pub fn new(kind: SectionKind, off: u64, len: u64, data: &[u8]) -> Self {
        SectionDesc {
            section_kind: kind.to_u32(),

            section_flags: 0,

            off,

            len,

            crc32: crc32(data),

            reserved: 0,
        }
    }
    /// Create a new SectionDesc with pre-calculated CRC    
    #[inline]
    pub fn with_crc(kind: SectionKind, off: u64, len: u64, crc: u32) -> Self {
        SectionDesc {
            section_kind: kind.to_u32(),

            section_flags: 0,

            off,

            len,

            crc32: crc,

            reserved: 0,
        }
    }
    /// Get section kind    
    #[inline]
    pub fn kind(&self) -> Option<SectionKind> {
        SectionKind::from_u32(self.section_kind)
    }
    /// Validate section descriptor    
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.kind().is_some() && self.len <= 1024 * 1024 * 1024 // 1GB max
    }
    /// Validate CRC against data    
    #[inline]
    pub fn validate_crc(&self, data: &[u8]) -> bool {
        if data.len() as u64 != self.len {
            return false;
        }

        self.crc32 == crc32(data)
    }
    /// Check if section is marked as compressed    
    #[inline]
    pub fn is_compressed(&self) -> bool {
        self.section_flags & SectionFlags::COMPRESSED as u32 != 0
    }
    /// Check if section is marked as encrypted    
    #[inline]
    pub fn is_encrypted(&self) -> bool {
        self.section_flags & SectionFlags::ENCRYPTED as u32 != 0
    }
    /// Read SectionDesc from bytes (returns owned value for safety)    
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CasError> {
        if bytes.len() < Self::SIZE {
            return Err(CasError::BufferTooSmall {
                expected: Self::SIZE,
                actual: bytes.len(),
            });
        }
        Self::from_bytes_unaligned(bytes)
    }
    /// Read SectionDesc from bytes with unaligned access    
    pub fn from_bytes_unaligned(bytes: &[u8]) -> Result<Self, CasError> {
        if bytes.len() < Self::SIZE {
            return Err(CasError::BufferTooSmall {
                expected: Self::SIZE,
                actual: bytes.len(),
            });
        }

        unsafe {
            Ok(std::ptr::read_unaligned(
                bytes.as_ptr() as *const SectionDesc
            ))
        }
    }
}

impl fmt::Debug for SectionDesc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SectionDesc")
            .field("section_kind", &self.section_kind)
            .field(
                "section_flags",
                &format_args!("0x{:08X}", self.section_flags),
            )
            .field("off", &self.off)
            .field("len", &self.len)
            .field("crc32", &format_args!("0x{:08X}", self.crc32))
            .finish()
    }
} // Verify SectionDesc size at compile time

const _: () = assert!(
    size_of::<SectionDesc>() == 32,
    "SectionDesc must be 32 bytes"
); // ============================================================================// SectionFlags// ============================================================================
/// Bit flags for SectionDesc.section_flags
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionFlags {
    /// Section data is compressed
    COMPRESSED = 0x00000001,

    /// Section data is encrypted
    ENCRYPTED = 0x00000002,

    /// Section contains references to other atoms
    HAS_REFS = 0x00000004,

    /// Section is a delta (incremental update)
    DELTA = 0x00000008,

    /// Section has been validated
    VALIDATED = 0x00000010,
} // ============================================================================// CAS Errors// ============================================================================
/// Errors that can occur in CAS operations
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CasError {
    /// Buffer too small for operation
    BufferTooSmall { expected: usize, actual: usize },

    /// Invalid magic number
    InvalidMagic { expected: u32, found: u32 },

    /// CRC mismatch
    CrcMismatch { expected: u32, found: u32 },

    /// Invalid section offset/length
    InvalidSectionBounds {
        offset: u64,

        length: u64,
        body_size: u64,
    },

    /// Invalid atom type
    InvalidAtomType(u32),

    /// Invalid section kind
    InvalidSectionKind(u32),

    /// Section not found
    SectionNotFound(u32),

    /// IO error
    Io(String),

    /// Alignment error
    AlignmentError { expected: usize, actual: usize },

    /// Canonical form extraction failed - payload is not a valid atom body
    CanonicalExtractionFailed { reason: String },
}

impl fmt::Display for CasError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CasError::BufferTooSmall { expected, actual } => {
                write!(
                    f,
                    "buffer too small: expected {} bytes, got {}",
                    expected, actual
                )
            }
            CasError::InvalidMagic { expected, found } => {
                write!(
                    f,
                    "invalid magic: expected 0x{:08X}, got 0x{:08X}",
                    expected, found
                )
            }
            CasError::CrcMismatch { expected, found } => {
                write!(
                    f,
                    "CRC mismatch: expected 0x{:08X}, got 0x{:08X}",
                    expected, found
                )
            }
            CasError::InvalidSectionBounds {
                offset,

                length,
                body_size,
            } => {
                write!(
                    f,
                    "invalid section bounds: offset={}, length={}, body_size={}",
                    offset, length, body_size
                )
            }
            CasError::InvalidAtomType(t) => write!(f, "invalid atom type: {}", t),
            CasError::InvalidSectionKind(k) => write!(f, "invalid section kind: {}", k),
            CasError::SectionNotFound(k) => write!(f, "section not found: kind={}", k),
            CasError::Io(msg) => write!(f, "IO error: {}", msg),
            CasError::AlignmentError { expected, actual } => {
                write!(
                    f,
                    "alignment error: expected {} bytes, got {}",
                    expected, actual
                )
            }
            CasError::CanonicalExtractionFailed { reason } => {
                write!(f, "canonical form extraction failed: {}", reason)
            }
        }
    }
}

impl std::error::Error for CasError {}

impl From<std::io::Error> for CasError {
    fn from(err: std::io::Error) -> Self {
        CasError::Io(err.to_string())
    }
} // ============================================================================// Helper functions// ============================================================================
/// Validate section bounds against body size
#[inline]
pub fn validate_section_bounds(
    section_offset: u64,
    section_len: u64,
    body_size: u64,
) -> Result<(), CasError> {
    let end = section_offset
        .checked_add(section_len)
        .ok_or(CasError::InvalidSectionBounds {
            offset: section_offset,

            length: section_len,
            body_size,
        })?;

    if end > body_size {
        return Err(CasError::InvalidSectionBounds {
            offset: section_offset,

            length: section_len,
            body_size,
        });
    }

    Ok(())
}
/// Calculate offset to section table entry
#[inline]
pub fn section_table_entry_offset(base_offset: u64, index: u32) -> u64 {
    base_offset + (index as u64 * SectionDesc::SIZE as u64)
}
/// Read section data from body buffer///
/// # Safety
/// - Caller must ensure bounds are valid
#[inline]
pub unsafe fn get_section_data_unchecked<'a>(body: &'a [u8], section: &SectionDesc) -> &'a [u8] {
    let start = section.off as usize;

    let end = start + section.len as usize;

    unsafe { body.get_unchecked(start..end) }
}
/// Read section data from body buffer with validation
#[inline]
pub fn get_section_data<'a>(body: &'a [u8], section: &SectionDesc) -> Result<&'a [u8], CasError> {
    validate_section_bounds(section.off, section.len, body.len() as u64)?;

    let start = section.off as usize;

    let end = start + section.len as usize;

    body.get(start..end).ok_or(CasError::InvalidSectionBounds {
        offset: section.off,

        length: section.len,
        body_size: body.len() as u64,
    })
}
/// Validate all sections in a body
pub fn validate_sections(body: &[u8], sections: &[SectionDesc]) -> Result<(), CasError> {
    for section in sections {
        if !section.is_valid() {
            return Err(CasError::InvalidSectionKind(section.section_kind));
        }

        let data = get_section_data(body, section)?;

        if !section.validate_crc(data) {
            return Err(CasError::CrcMismatch {
                expected: section.crc32,
                found: crc32(data),
            });
        }
    }

    Ok(())
}
/// Find section by kind
pub fn find_section(sections: &[SectionDesc], kind: SectionKind) -> Option<&SectionDesc> {
    sections.iter().find(|s| s.kind() == Some(kind))
}
/// Encode AtomId to hex string (for debugging)
pub fn hex_encode(atom_id: &AtomId) -> String {
    const CHARS: &[u8] = b"0123456789abcdef";

    let mut buf = String::with_capacity(64);

    for &byte in atom_id.iter() {
        buf.push(CHARS[(byte >> 4) as usize] as char);
        buf.push(CHARS[(byte & 0x0F) as usize] as char);
    }
    buf
}
/// Decode hex string to AtomId
pub fn hex_decode(s: &str) -> Result<AtomId, CasError> {
    let s = s.trim();

    if s.len() != 64 {
        return Err(CasError::BufferTooSmall {
            expected: 64,
            actual: s.len(),
        });
    }

    let mut atom_id = [0u8; 32];

    let chars: &[u8] = s.as_bytes();

    for i in 0..32 {
        let high = hex_char_to_u8(chars[i * 2])?;

        let low = hex_char_to_u8(chars[i * 2 + 1])?;

        atom_id[i] = (high << 4) | low;
    }

    Ok(atom_id)
}
#[inline]
fn hex_char_to_u8(c: u8) -> Result<u8, CasError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(CasError::Io(format!("invalid hex char: {}", c as char))),
    }
} // ============================================================================// Zero-copy views for efficient access// ============================================================================
/// Zero-copy view of a CAS record
pub struct RecordView<'a> {
    header: RecordHeader,
    body: &'a [u8],
}
impl<'a> RecordView<'a> {
    /// Create a new RecordView from header and body    
    ///    
    /// # Safety    
    /// - Header must be valid and properly aligned    
    /// - Body must match the header's body_len
    pub unsafe fn new_unchecked(header: RecordHeader, body: &'a [u8]) -> Self {
        RecordView { header, body }
    }
    /// Create a RecordView with validation    
    pub fn new(header_bytes: &'a [u8], body: &'a [u8]) -> Result<Self, CasError> {
        let header = RecordHeader::from_bytes(header_bytes)?;

        if body.len() as u64 != header.body_len {
            return Err(CasError::BufferTooSmall {
                expected: header.body_len as usize,
                actual: body.len(),
            });
        }

        Ok(RecordView { header, body })
    }
    /// Get reference to header    
    #[inline]
    pub fn header(&self) -> &RecordHeader {
        &self.header
    }
    /// Get reference to body    
    #[inline]
    pub fn body(&self) -> &[u8] {
        self.body
    }
    /// Get atom ID    
    #[inline]
    pub fn atom_id(&self) -> &AtomId {
        &self.header.atom_id
    }
    /// Get atom body header    
    pub fn body_header(&self) -> Result<AtomBodyHeader, CasError> {
        AtomBodyHeader::from_bytes(self.body)
    }
    /// Get section descriptors    
    pub fn sections(&self) -> Result<Vec<SectionDesc>, CasError> {
        let body_header = self.body_header()?;

        let section_count = body_header.section_count as usize;

        let table_start = body_header.section_table_off as usize;

        let table_bytes = self
            .body
            .get(table_start..table_start + section_count * SectionDesc::SIZE)
            .ok_or(CasError::BufferTooSmall {
                expected: table_start + section_count * SectionDesc::SIZE,
                actual: self.body.len(),
            })?;

        let mut sections = Vec::with_capacity(section_count);

        for i in 0..section_count {
            let offset = i * SectionDesc::SIZE;

            let section = SectionDesc::from_bytes_unaligned(
                &table_bytes[offset..offset + SectionDesc::SIZE],
            )?;
            sections.push(section);
        }

        Ok(sections)
    }
    /// Get section data by kind    
    pub fn get_section(&self, kind: SectionKind) -> Result<&[u8], CasError> {
        let sections = self.sections()?;

        let section =
            find_section(&sections, kind).ok_or(CasError::SectionNotFound(kind.to_u32()))?;
        get_section_data(self.body, section)
    }
} // ============================================================================// Tests// ============================================================================

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_record_header_size() {
        assert_eq!(RecordHeader::SIZE, 64);

        assert_eq!(size_of::<RecordHeader>(), 64);
    }
    #[test]
    fn test_atom_body_header_size() {
        assert_eq!(AtomBodyHeader::SIZE, 48);

        assert_eq!(size_of::<AtomBodyHeader>(), 48);
    }
    #[test]
    fn test_section_desc_size() {
        assert_eq!(SectionDesc::SIZE, 32);

        assert_eq!(size_of::<SectionDesc>(), 32);
    }
    #[test]
    fn test_record_header_create_and_validate() {
        let atom_id = [1u8; 32];

        let header = RecordHeader::new(atom_id, 1024, 1, 0);

        assert!(header.validate_magic());

        assert!(header.validate_crc());

        assert!(header.is_valid());

        assert_eq!(header.body_len(), 1024);

        assert_eq!(header.seg_id(), 1);
    }
    #[test]
    fn test_atom_body_header_create() {
        let header = AtomBodyHeader::new(AtomType::FACT, 3, 1_000_000_000, 0, 0);

        assert!(header.validate_magic());

        assert_eq!(header.atom_type(), Some(AtomType::FACT));

        assert_eq!(header.section_count, 3);
    }
    #[test]
    fn test_section_desc_create() {
        let data = b"test section data";

        let section = SectionDesc::new(SectionKind::CLAIMS, 100, data.len() as u64, data);

        assert_eq!(section.kind(), Some(SectionKind::CLAIMS));

        assert_eq!(section.off, 100);

        assert_eq!(section.len, data.len() as u64);

        assert!(section.validate_crc(data));
    }
    #[test]
    fn test_record_header_roundtrip() {
        let atom_id = [0xABu8; 32];

        let header = RecordHeader::new(atom_id, 2048, 42, RecordFlags::COMPRESSED as u16);

        let mut buf = [0u8; RecordHeader::SIZE];

        header.write_to_bytes(&mut buf).unwrap();

        let restored = RecordHeader::from_bytes(&buf).unwrap();

        assert_eq!(restored.magic, header.magic);

        assert_eq!(restored.atom_id, header.atom_id);

        assert_eq!(restored.body_len, header.body_len);

        assert_eq!(restored.seg_id, header.seg_id);
    }
    #[test]
    fn test_hex_encode_decode() {
        let atom_id = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D,
            0x0E, 0x0F, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B,
            0x1C, 0x1D, 0x1E, 0x1F,
        ];

        let encoded = hex_encode(&atom_id);

        assert_eq!(encoded.len(), 64);

        let decoded = hex_decode(&encoded).unwrap();

        assert_eq!(atom_id, decoded);
    }
    #[test]
    fn test_validate_section_bounds() {
        // Valid bounds

        assert!(validate_section_bounds(100, 200, 500).is_ok());
        // Bounds at edge

        assert!(validate_section_bounds(300, 200, 500).is_ok());
        // Bounds exceed

        assert!(validate_section_bounds(400, 200, 500).is_err());
        // Overflow

        assert!(validate_section_bounds(u64::MAX, 1, 500).is_err());
    }
    #[test]
    fn test_section_table_offset() {
        let base = 48u64;

        assert_eq!(section_table_entry_offset(base, 0), 48);

        assert_eq!(section_table_entry_offset(base, 1), 80);

        assert_eq!(section_table_entry_offset(base, 2), 112);
    }
    #[test]
    fn test_atom_body_validity_check() {
        let header = AtomBodyHeader::new(
            AtomType::FACT,
            1,
            1_000_000_000,
            500_000_000,   // valid from 0.5s
            1_500_000_000, // valid to 1.5s
        );
        // Within validity window

        assert!(header.is_valid_now(1_000_000_000));
        // Before validity window

        assert!(!header.is_valid_now(400_000_000));
        // After validity window

        assert!(!header.is_valid_now(2_000_000_000));
    }
}

