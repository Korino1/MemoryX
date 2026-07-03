//! INVARIANTS section implementation for MemoryX SKF-1.1
//!
//! This module provides the INVARIANTS section (0x04) of AtomBody:
//! - Bytecode container for invariant checking rules
//! - Constant pool with symbol references and typed values
//! - 16-byte fixed-width instruction format (AVX-512VL aligned)
//!
//! # Spec reference: SKF-1.1 §9

use super::{CasError, crc32};

// ============================================================================
// Magic and constants
// ============================================================================

/// Magic number for InvariantsHeader: "INV1" = 0x494E5631
pub const INVARIANTS_MAGIC: u32 = 0x494E5631;

/// Size of InvariantsHeader in bytes.
/// Fields: magic(4) + inv_count(4) + const_pool_count(4) + reserved(4)
///         + off_const_pool(8) + off_code(8) = 32 bytes
pub const INVARIANTS_HEADER_SIZE: usize = 32;

/// Instruction size in bytes (16 bytes, AVX-512VL aligned)
pub const INSTRUCTION_SIZE: usize = 16;

const INV_CONST_POOL_ALIGNMENT: usize = 16;

// ============================================================================
// InvariantsHeader (32 bytes fixed)
// ============================================================================

/// Header for the INVARIANTS section.
///
/// Layout (32 bytes total):
/// - magic: u32 (4 bytes) = 0x494E5631 ("INV1")
/// - inv_count: u32 (4 bytes) — number of invariants
/// - const_pool_count: u32 (4 bytes) — number of constants in pool
/// - reserved: u32 (4 bytes) — must be 0
/// - off_const_pool: u64 (8 bytes) — offset to const pool from section start
/// - off_code: u64 (8 bytes) — offset to bytecode from section start
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InvariantsHeader {
    /// Magic number: 0x494E5631 ("INV1")
    pub magic: u32,
    /// Number of invariants
    pub inv_count: u32,
    /// Number of constants in pool
    pub const_pool_count: u32,
    /// Reserved (must be 0)
    pub reserved: u32,
    /// Offset to const pool from section start
    pub off_const_pool: u64,
    /// Offset to bytecode from section start
    pub off_code: u64,
}

impl InvariantsHeader {
    /// Size of InvariantsHeader in bytes.
    pub const SIZE: usize = INVARIANTS_HEADER_SIZE;

    /// Create a new InvariantsHeader.
    #[inline]
    pub fn new(inv_count: u32, const_pool_count: u32, off_const_pool: u64, off_code: u64) -> Self {
        Self {
            magic: INVARIANTS_MAGIC,
            inv_count,
            const_pool_count,
            reserved: 0,
            off_const_pool,
            off_code,
        }
    }

    /// Serialize header to bytes (32 bytes, little-endian).
    #[inline]
    pub fn to_bytes(&self) -> [u8; 32] {
        let mut buf = [0u8; 32];
        buf[0..4].copy_from_slice(&self.magic.to_le_bytes());
        buf[4..8].copy_from_slice(&self.inv_count.to_le_bytes());
        buf[8..12].copy_from_slice(&self.const_pool_count.to_le_bytes());
        buf[12..16].copy_from_slice(&self.reserved.to_le_bytes());
        buf[16..24].copy_from_slice(&self.off_const_pool.to_le_bytes());
        buf[24..32].copy_from_slice(&self.off_code.to_le_bytes());
        buf
    }

    /// Deserialize header from bytes.
    #[inline]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::SIZE {
            return None;
        }
        let magic = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
        let inv_count = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
        let const_pool_count = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
        let reserved = u32::from_le_bytes(bytes[12..16].try_into().ok()?);
        let off_const_pool = u64::from_le_bytes(bytes[16..24].try_into().ok()?);
        let off_code = u64::from_le_bytes(bytes[24..32].try_into().ok()?);
        Some(Self {
            magic,
            inv_count,
            const_pool_count,
            reserved,
            off_const_pool,
            off_code,
        })
    }
}

// ============================================================================
// ConstPoolKind
// ============================================================================

/// Kind of constant pool entry.
///
/// Matches SKF-1.1 §9.2:
/// - 0: SYM (symbol reference)
/// - 1: U64 (unsigned 64-bit integer)
/// - 2: I64 (signed 64-bit integer)
/// - 3: F64 (64-bit float)
/// - 4: BYTES (raw byte data)
/// - 5: REFID (reference ID)
/// - 6: TAG (type tag)
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConstPoolKind {
    /// Symbol reference (u32 sym_id, 4 bytes)
    SYM = 0,
    /// Unsigned 64-bit integer (8 bytes inline)
    U64 = 1,
    /// Signed 64-bit integer (8 bytes inline)
    I64 = 2,
    /// 64-bit float (8 bytes inline)
    F64 = 3,
    /// Raw byte data (variable length)
    BYTES = 4,
    /// Reference ID (32 bytes)
    REFID = 5,
    /// Type tag (u32, 4 bytes)
    TAG = 6,
}

impl ConstPoolKind {
    /// Create from raw u8 value.
    #[inline]
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::SYM),
            1 => Some(Self::U64),
            2 => Some(Self::I64),
            3 => Some(Self::F64),
            4 => Some(Self::BYTES),
            5 => Some(Self::REFID),
            6 => Some(Self::TAG),
            _ => None,
        }
    }

    /// Fixed inline size for fixed-width types.
    /// Returns None for variable-length types (SYM, BYTES).
    #[inline]
    pub fn fixed_size(self) -> Option<usize> {
        match self {
            Self::U64 | Self::I64 | Self::F64 => Some(8),
            Self::TAG => Some(4),
            Self::REFID => Some(32),
            _ => None,
        }
    }

    /// Whether this kind uses variable-length data.
    #[inline]
    pub fn is_variable_length(self) -> bool {
        matches!(self, Self::SYM | Self::BYTES)
    }
}

// ============================================================================
// ConstPoolEntry
// ============================================================================

/// A single constant pool entry.
///
/// Encoded as:
/// - u8 kind (1 byte)
/// - u32 len_or_zero (4 bytes) — 0 for fixed types, or length for variable-length data
/// - bytes[...] (inline for fixed types, variable for SYM/BYTES)
#[derive(Debug, Clone)]
pub struct ConstPoolEntry {
    /// Kind of constant
    pub kind: ConstPoolKind,
    /// Raw data bytes. Interpretation depends on kind:
    /// - SYM: 4 bytes (u32 little-endian sym_id)
    /// - U64: 8 bytes (u64 little-endian)
    /// - I64: 8 bytes (i64 little-endian)
    /// - F64: 8 bytes (f64 little-endian)
    /// - BYTES: variable length raw bytes
    /// - REFID: 32 bytes (AtomId)
    /// - TAG: 4 bytes (u32 little-endian tag value)
    pub data: Vec<u8>,
}

impl ConstPoolEntry {
    /// Size of the serialized entry on disk.
    /// Header (1 byte kind + 4 bytes len) + data.
    #[inline]
    pub fn serialized_size(&self) -> usize {
        1 + 4 + self.data.len()
    }

    /// Serialize entry to bytes.
    #[inline]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.serialized_size());
        buf.push(self.kind as u8);
        buf.extend_from_slice(&(self.data.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.data);
        buf
    }

    /// Deserialize entry from bytes.
    /// Returns (entry, bytes_consumed).
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), CasError> {
        if bytes.len() < 5 {
            return Err(CasError::BufferTooSmall {
                expected: 5,
                actual: bytes.len(),
            });
        }
        let kind = ConstPoolKind::from_u8(bytes[0])
            .ok_or_else(|| CasError::Io("invalid const pool kind".into()))?;
        let len = u32::from_le_bytes(
            bytes[1..5]
                .try_into()
                .map_err(|_| CasError::Io("const pool len read error".into()))?,
        );
        let total = 5 + len as usize;
        if bytes.len() < total {
            return Err(CasError::BufferTooSmall {
                expected: total,
                actual: bytes.len(),
            });
        }
        let data = bytes[5..total].to_vec();
        Ok((Self { kind, data }, total))
    }
}

// ============================================================================
// InvariantsSection
// ============================================================================

/// The complete INVARIANTS section: const pool + bytecode.
///
/// Serialization layout:
/// - InvariantsHeader (32 bytes) with correct offsets computed
/// - Const pool entries: kind(u8) + len(u32) + data...
/// - Padding to 16-byte boundary before code
/// - Code bytes (16-byte aligned instructions)
#[derive(Debug, Clone, Default)]
pub struct InvariantsSection {
    /// Constant pool entries.
    pub const_pool: Vec<ConstPoolEntry>,
    /// Raw bytecode bytes (each instruction is 16 bytes).
    pub code: Vec<u8>,
    /// Number of invariants.
    pub inv_count: u32,
}

impl InvariantsSection {
    /// Create an empty InvariantsSection.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a constant to the pool. Returns the index of the added constant.
    #[inline]
    pub fn add_const(&mut self, kind: ConstPoolKind, data: &[u8]) -> u32 {
        let idx = self.const_pool.len() as u32;
        self.const_pool.push(ConstPoolEntry {
            kind,
            data: data.to_vec(),
        });
        idx
    }

    /// Convenience: add a symbol reference (SYM kind with u32 sym_id).
    #[inline]
    pub fn add_const_sym(&mut self, sym_id: u32) -> u32 {
        self.add_const(ConstPoolKind::SYM, &sym_id.to_le_bytes())
    }

    /// Convenience: add a u64 constant (U64 kind, 8 bytes inline).
    #[inline]
    pub fn add_const_u64(&mut self, v: u64) -> u32 {
        self.add_const(ConstPoolKind::U64, &v.to_le_bytes())
    }

    /// Convenience: add an i64 constant (I64 kind, 8 bytes inline).
    #[inline]
    pub fn add_const_i64(&mut self, v: i64) -> u32 {
        self.add_const(ConstPoolKind::I64, &v.to_le_bytes())
    }

    /// Convenience: add an f64 constant (F64 kind, 8 bytes inline).
    #[inline]
    pub fn add_const_f64(&mut self, v: f64) -> u32 {
        self.add_const(ConstPoolKind::F64, &v.to_le_bytes())
    }

    /// Get a constant by index (0-based).
    #[inline]
    pub fn get_const(&self, idx: u32) -> Option<&ConstPoolEntry> {
        self.const_pool.get(idx as usize)
    }

    /// Append a 16-byte instruction to the code buffer.
    ///
    /// Instruction format (SKF-1.1 §9.3):
    /// - u16 op  (2 bytes)
    /// - u16 a   (2 bytes)
    /// - u32 b   (4 bytes)
    /// - u64 imm (8 bytes)
    ///   Total: 16 bytes
    #[inline]
    pub fn emit_instruction(&mut self, op: u16, a: u16, b: u32, imm: u64) {
        self.code.extend_from_slice(&op.to_le_bytes());
        self.code.extend_from_slice(&a.to_le_bytes());
        self.code.extend_from_slice(&b.to_le_bytes());
        self.code.extend_from_slice(&imm.to_le_bytes());
        debug_assert_eq!(self.code.len() % INSTRUCTION_SIZE, 0);
    }

    /// Serialize the complete INVARIANTS section to bytes.
    ///
    /// Layout:
    /// 1. InvariantsHeader (32 bytes) with correct offsets computed
    /// 2. Const pool entries (each: kind(u8) + len(u32) + data)
    /// 3. Padding to 16-byte boundary before code
    /// 4. Code bytes (16-byte aligned instructions)
    pub fn to_bytes(&self) -> Vec<u8> {
        // Calculate sizes
        let header_size = InvariantsHeader::SIZE;

        // Const pool serialized size
        let const_pool_size: usize = self.const_pool.iter().map(|e| e.serialized_size()).sum();

        // Padding to 16-byte alignment before code
        let after_const = header_size + const_pool_size;
        let padding = if !after_const.is_multiple_of(INV_CONST_POOL_ALIGNMENT) {
            INV_CONST_POOL_ALIGNMENT - (after_const % INV_CONST_POOL_ALIGNMENT)
        } else {
            0
        };

        let off_const_pool = header_size as u64;
        let off_code = (header_size + const_pool_size + padding) as u64;

        // Build buffer
        let total_size = header_size + const_pool_size + padding + self.code.len();
        let mut buf = Vec::with_capacity(total_size);

        // Write header with computed offsets
        let header = InvariantsHeader::new(
            self.inv_count,
            self.const_pool.len() as u32,
            off_const_pool,
            off_code,
        );
        buf.extend_from_slice(&header.to_bytes());

        // Write const pool entries
        for entry in &self.const_pool {
            buf.extend(entry.to_bytes());
        }

        // Write padding to 16-byte boundary
        buf.resize(header_size + const_pool_size + padding, 0);

        // Write code
        buf.extend_from_slice(&self.code);

        buf
    }

    /// Deserialize INVARIANTS section from bytes.
    ///
    /// Returns an error if:
    /// - Buffer too small for header
    /// - Invalid magic number
    /// - Buffer too small for declared const pool or code
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CasError> {
        if bytes.len() < InvariantsHeader::SIZE {
            return Err(CasError::BufferTooSmall {
                expected: InvariantsHeader::SIZE,
                actual: bytes.len(),
            });
        }

        // Parse header
        let header = InvariantsHeader::from_bytes(bytes).ok_or(CasError::BufferTooSmall {
            expected: InvariantsHeader::SIZE,
            actual: bytes.len(),
        })?;

        if header.magic != INVARIANTS_MAGIC {
            return Err(CasError::InvalidMagic {
                expected: INVARIANTS_MAGIC,
                found: header.magic,
            });
        }

        let mut section = Self {
            inv_count: header.inv_count,
            const_pool: Vec::with_capacity(header.const_pool_count as usize),
            code: Vec::new(),
        };

        // Parse const pool entries
        let mut offset = header.off_const_pool as usize;
        for _i in 0..header.const_pool_count {
            if offset >= bytes.len() {
                return Err(CasError::BufferTooSmall {
                    expected: bytes.len() + 1,
                    actual: bytes.len(),
                });
            }
            let (entry, consumed) = ConstPoolEntry::from_bytes(&bytes[offset..])?;
            section.const_pool.push(entry);
            offset += consumed;
        }

        // Parse code (skip padding to reach off_code)
        let code_start = header.off_code as usize;
        if code_start > bytes.len() {
            return Err(CasError::InvalidSectionBounds {
                offset: header.off_code,
                length: 0,
                body_size: bytes.len() as u64,
            });
        }
        section.code = bytes[code_start..].to_vec();

        Ok(section)
    }
}

// ============================================================================
// CRC helper for INVARIANTS section validation
// ============================================================================

/// Compute CRC32 of the serialized INVARIANTS section data.
///
/// This can be used by the caller when embedding the INVARIANTS section
/// into SectionDesc which handles CRC at the section level.
#[inline]
pub fn compute_invariants_crc(data: &[u8]) -> u32 {
    crc32(data)
}

// ============================================================================
// Instruction parsing
// ============================================================================

/// A decoded 16-byte instruction.
///
/// Format (SKF-1.1 §9.3):
/// - u16 op  (2 bytes) — opcode
/// - u16 a   (2 bytes) — register/operand A
/// - u32 b   (4 bytes) — register/operand B / const pool index
/// - u64 imm (8 bytes) — immediate value
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Instruction {
    /// Opcode
    pub op: u16,
    /// Operand A
    pub a: u16,
    /// Operand B / const pool index
    pub b: u32,
    /// Immediate value
    pub imm: u64,
}

impl Instruction {
    /// Size of an instruction in bytes.
    pub const SIZE: usize = INSTRUCTION_SIZE;

    /// Serialize instruction to bytes (16 bytes, little-endian).
    #[inline]
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut buf = [0u8; 16];
        buf[0..2].copy_from_slice(&self.op.to_le_bytes());
        buf[2..4].copy_from_slice(&self.a.to_le_bytes());
        buf[4..8].copy_from_slice(&self.b.to_le_bytes());
        buf[8..16].copy_from_slice(&self.imm.to_le_bytes());
        buf
    }

    /// Deserialize instruction from bytes.
    #[inline]
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CasError> {
        if bytes.len() < Self::SIZE {
            return Err(CasError::BufferTooSmall {
                expected: Self::SIZE,
                actual: bytes.len(),
            });
        }
        Ok(Self {
            op: u16::from_le_bytes(bytes[0..2].try_into().unwrap()),
            a: u16::from_le_bytes(bytes[2..4].try_into().unwrap()),
            b: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            imm: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        })
    }
}

/// Decode all instructions from the code buffer.
///
/// Returns an error if code length is not a multiple of 16 bytes.
pub fn decode_instructions(code: &[u8]) -> Result<Vec<Instruction>, CasError> {
    if !code.len().is_multiple_of(INSTRUCTION_SIZE) {
        return Err(CasError::AlignmentError {
            expected: INSTRUCTION_SIZE,
            actual: code.len(),
        });
    }
    let mut instructions = Vec::with_capacity(code.len() / INSTRUCTION_SIZE);
    for chunk in code.as_chunks::<INSTRUCTION_SIZE>().0 {
        instructions.push(Instruction::from_bytes(chunk)?);
    }
    Ok(instructions)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_invariants_header_size() {
        assert_eq!(InvariantsHeader::SIZE, 32);
        assert_eq!(std::mem::size_of::<InvariantsHeader>(), 32);
    }

    #[test]
    fn test_invariants_header_roundtrip() {
        let header = InvariantsHeader::new(5, 10, 32, 128);
        assert_eq!(header.magic, INVARIANTS_MAGIC);
        assert_eq!(header.inv_count, 5);
        assert_eq!(header.const_pool_count, 10);
        assert_eq!(header.reserved, 0);
        assert_eq!(header.off_const_pool, 32);
        assert_eq!(header.off_code, 128);

        let bytes = header.to_bytes();
        assert_eq!(bytes.len(), 32);

        let restored = InvariantsHeader::from_bytes(&bytes).unwrap();
        assert_eq!(restored, header);
    }

    #[test]
    fn test_invariants_header_from_bytes_too_small() {
        let small = [0u8; 16];
        assert!(InvariantsHeader::from_bytes(&small).is_none());

        let empty: [u8; 0] = [];
        assert!(InvariantsHeader::from_bytes(&empty).is_none());
    }

    #[test]
    fn test_invariants_header_invalid_magic() {
        let mut bytes = InvariantsHeader::new(0, 0, 0, 0).to_bytes();
        bytes[0] = 0xFF;
        let header = InvariantsHeader::from_bytes(&bytes).unwrap();
        assert_ne!(header.magic, INVARIANTS_MAGIC);
    }

    #[test]
    fn test_const_pool_kind_from_u8() {
        assert_eq!(ConstPoolKind::from_u8(0), Some(ConstPoolKind::SYM));
        assert_eq!(ConstPoolKind::from_u8(1), Some(ConstPoolKind::U64));
        assert_eq!(ConstPoolKind::from_u8(2), Some(ConstPoolKind::I64));
        assert_eq!(ConstPoolKind::from_u8(3), Some(ConstPoolKind::F64));
        assert_eq!(ConstPoolKind::from_u8(4), Some(ConstPoolKind::BYTES));
        assert_eq!(ConstPoolKind::from_u8(5), Some(ConstPoolKind::REFID));
        assert_eq!(ConstPoolKind::from_u8(6), Some(ConstPoolKind::TAG));
        assert_eq!(ConstPoolKind::from_u8(7), None);
        assert_eq!(ConstPoolKind::from_u8(255), None);
    }

    #[test]
    fn test_const_pool_kind_fixed_size() {
        assert_eq!(ConstPoolKind::U64.fixed_size(), Some(8));
        assert_eq!(ConstPoolKind::I64.fixed_size(), Some(8));
        assert_eq!(ConstPoolKind::F64.fixed_size(), Some(8));
        assert_eq!(ConstPoolKind::TAG.fixed_size(), Some(4));
        assert_eq!(ConstPoolKind::REFID.fixed_size(), Some(32));
        assert_eq!(ConstPoolKind::SYM.fixed_size(), None);
        assert_eq!(ConstPoolKind::BYTES.fixed_size(), None);
    }

    #[test]
    fn test_const_pool_entry_serialization() {
        // SYM entry (4 bytes data)
        let entry = ConstPoolEntry {
            kind: ConstPoolKind::SYM,
            data: vec![0x01, 0x00, 0x00, 0x00], // sym_id = 1
        };
        assert_eq!(entry.serialized_size(), 1 + 4 + 4); // kind + len + data
        let bytes = entry.to_bytes();
        assert_eq!(bytes.len(), 9);
        assert_eq!(bytes[0], 0); // kind = SYM
        let len = u32::from_le_bytes(bytes[1..5].try_into().unwrap());
        assert_eq!(len, 4);

        // Roundtrip
        let (restored, consumed) = ConstPoolEntry::from_bytes(&bytes).unwrap();
        assert_eq!(consumed, 9);
        assert_eq!(restored.kind, ConstPoolKind::SYM);
        assert_eq!(restored.data, entry.data);
    }

    #[test]
    fn test_const_pool_entry_u64() {
        let entry = ConstPoolEntry {
            kind: ConstPoolKind::U64,
            data: 42u64.to_le_bytes().to_vec(),
        };
        let bytes = entry.to_bytes();
        assert_eq!(bytes.len(), 1 + 4 + 8);

        let (restored, _) = ConstPoolEntry::from_bytes(&bytes).unwrap();
        assert_eq!(restored.kind, ConstPoolKind::U64);
        let val = u64::from_le_bytes(restored.data.try_into().unwrap());
        assert_eq!(val, 42);
    }

    #[test]
    fn test_const_pool_entry_f64() {
        let val = std::f64::consts::PI;
        let entry = ConstPoolEntry {
            kind: ConstPoolKind::F64,
            data: val.to_le_bytes().to_vec(),
        };
        let bytes = entry.to_bytes();
        let (restored, _) = ConstPoolEntry::from_bytes(&bytes).unwrap();
        let f = f64::from_le_bytes(restored.data.try_into().unwrap());
        assert!((f - val).abs() < f64::EPSILON);
    }

    #[test]
    fn test_const_pool_entry_bytes_variable() {
        let data = b"some arbitrary bytecode for invariant rule #1";
        let entry = ConstPoolEntry {
            kind: ConstPoolKind::BYTES,
            data: data.to_vec(),
        };
        let bytes = entry.to_bytes();
        assert_eq!(bytes.len(), 1 + 4 + data.len());

        let (restored, _) = ConstPoolEntry::from_bytes(&bytes).unwrap();
        assert_eq!(restored.kind, ConstPoolKind::BYTES);
        assert_eq!(restored.data, data);
    }

    #[test]
    fn test_const_pool_entry_empty_data() {
        let entry = ConstPoolEntry {
            kind: ConstPoolKind::BYTES,
            data: vec![],
        };
        let bytes = entry.to_bytes();
        assert_eq!(bytes.len(), 5); // kind + len(0) + no data
        let len = u32::from_le_bytes(bytes[1..5].try_into().unwrap());
        assert_eq!(len, 0);

        let (restored, consumed) = ConstPoolEntry::from_bytes(&bytes).unwrap();
        assert_eq!(consumed, 5);
        assert!(restored.data.is_empty());
    }

    #[test]
    fn test_const_pool_entry_from_bytes_too_small() {
        let small = [0u8; 3];
        assert!(ConstPoolEntry::from_bytes(&small).is_err());

        let kind_and_len = [0u8, 1, 0, 0, 0]; // kind=0, len=1 but only 5 bytes no data
        assert!(ConstPoolEntry::from_bytes(&kind_and_len).is_err());
    }

    #[test]
    fn test_invariants_section_new() {
        let section = InvariantsSection::new();
        assert!(section.const_pool.is_empty());
        assert!(section.code.is_empty());
        assert_eq!(section.inv_count, 0);
    }

    #[test]
    fn test_invariants_section_add_const_sym() {
        let mut section = InvariantsSection::new();
        let idx0 = section.add_const_sym(100);
        assert_eq!(idx0, 0);
        let idx1 = section.add_const_sym(200);
        assert_eq!(idx1, 1);

        let sym0 = section.get_const(0).unwrap();
        assert_eq!(sym0.kind, ConstPoolKind::SYM);
        let sym_id = u32::from_le_bytes(sym0.data.clone().try_into().unwrap());
        assert_eq!(sym_id, 100);

        let sym1 = section.get_const(1).unwrap();
        let sym_id = u32::from_le_bytes(sym1.data.clone().try_into().unwrap());
        assert_eq!(sym_id, 200);
    }

    #[test]
    fn test_invariants_section_add_const_numeric() {
        let mut section = InvariantsSection::new();
        let u64_idx = section.add_const_u64(999);
        let i64_idx = section.add_const_i64(-42);
        let f64_idx = section.add_const_f64(std::f64::consts::E);

        assert_eq!(u64_idx, 0);
        assert_eq!(i64_idx, 1);
        assert_eq!(f64_idx, 2);

        assert_eq!(section.const_pool.len(), 3);

        let u64_val = u64::from_le_bytes(
            section
                .get_const(0)
                .unwrap()
                .data
                .clone()
                .try_into()
                .unwrap(),
        );
        assert_eq!(u64_val, 999);

        let i64_val = i64::from_le_bytes(
            section
                .get_const(1)
                .unwrap()
                .data
                .clone()
                .try_into()
                .unwrap(),
        );
        assert_eq!(i64_val, -42);

        let f64_val = f64::from_le_bytes(
            section
                .get_const(2)
                .unwrap()
                .data
                .clone()
                .try_into()
                .unwrap(),
        );
        assert!((f64_val - std::f64::consts::E).abs() < f64::EPSILON);
    }

    #[test]
    fn test_invariants_section_get_const_out_of_bounds() {
        let section = InvariantsSection::new();
        assert!(section.get_const(0).is_none());

        let mut section = InvariantsSection::new();
        section.add_const_u64(1);
        assert!(section.get_const(1).is_none());
    }

    #[test]
    fn test_invariants_section_emit_instruction() {
        let mut section = InvariantsSection::new();
        section.emit_instruction(0x01, 0x02, 0x03, 0xDEADBEEF);

        assert_eq!(section.code.len(), INSTRUCTION_SIZE);

        let instr = Instruction::from_bytes(&section.code).unwrap();
        assert_eq!(instr.op, 0x01);
        assert_eq!(instr.a, 0x02);
        assert_eq!(instr.b, 0x03);
        assert_eq!(instr.imm, 0xDEADBEEF);
    }

    #[test]
    fn test_invariants_section_emit_multiple_instructions() {
        let mut section = InvariantsSection::new();
        section.emit_instruction(1, 2, 3, 4);
        section.emit_instruction(5, 6, 7, 8);
        section.emit_instruction(9, 10, 11, 12);

        assert_eq!(section.code.len(), 3 * INSTRUCTION_SIZE);

        let instrs = decode_instructions(&section.code).unwrap();
        assert_eq!(instrs.len(), 3);
        assert_eq!(instrs[0].op, 1);
        assert_eq!(instrs[1].a, 6);
        assert_eq!(instrs[2].imm, 12);
    }

    #[test]
    fn test_invariants_section_to_bytes_empty() {
        let section = InvariantsSection::new();
        let bytes = section.to_bytes();

        // Header only, no const pool, no padding, no code
        assert_eq!(bytes.len(), InvariantsHeader::SIZE);

        let header = InvariantsHeader::from_bytes(&bytes).unwrap();
        assert_eq!(header.magic, INVARIANTS_MAGIC);
        assert_eq!(header.const_pool_count, 0);
        assert_eq!(header.off_const_pool, InvariantsHeader::SIZE as u64);
        assert_eq!(header.off_code, InvariantsHeader::SIZE as u64);
    }

    #[test]
    fn test_invariants_section_full_roundtrip() {
        let mut section = InvariantsSection::new();
        section.inv_count = 2;

        // Add mixed const types
        section.add_const_sym(42);
        section.add_const_u64(1234567890);
        section.add_const_i64(-9999);
        section.add_const_f64(std::f64::consts::SQRT_2);
        section.add_const(ConstPoolKind::BYTES, b"rule_data");
        // REFID (32 bytes)
        let refid_data = [0xABu8; 32];
        section.add_const(ConstPoolKind::REFID, &refid_data);
        // TAG
        section.add_const(ConstPoolKind::TAG, &3u32.to_le_bytes());

        // Emit instructions
        section.emit_instruction(0x01, 0x00, 0x00, 0);
        section.emit_instruction(0x02, 0x01, 0x02, 1);
        section.emit_instruction(0x10, 0x0F, 0x00, 0xDEAD);

        // Serialize
        let bytes = section.to_bytes();

        // Deserialize
        let restored = InvariantsSection::from_bytes(&bytes).unwrap();

        // Verify header fields
        assert_eq!(restored.inv_count, 2);
        assert_eq!(restored.const_pool.len(), 7);

        // Verify const pool
        let sym = restored.get_const(0).unwrap();
        assert_eq!(sym.kind, ConstPoolKind::SYM);
        assert_eq!(u32::from_le_bytes(sym.data.clone().try_into().unwrap()), 42);

        let u64 = restored.get_const(1).unwrap();
        assert_eq!(u64.kind, ConstPoolKind::U64);
        assert_eq!(
            u64::from_le_bytes(u64.data.clone().try_into().unwrap()),
            1234567890u64
        );

        let i64 = restored.get_const(2).unwrap();
        assert_eq!(i64.kind, ConstPoolKind::I64);
        assert_eq!(
            i64::from_le_bytes(i64.data.clone().try_into().unwrap()),
            -9999i64
        );

        let f64 = restored.get_const(3).unwrap();
        assert_eq!(f64.kind, ConstPoolKind::F64);
        let fval = f64::from_le_bytes(f64.data.clone().try_into().unwrap());
        assert!((fval - std::f64::consts::SQRT_2).abs() < f64::EPSILON);

        let bytes_entry = restored.get_const(4).unwrap();
        assert_eq!(bytes_entry.kind, ConstPoolKind::BYTES);
        assert_eq!(bytes_entry.data, b"rule_data");

        let refid = restored.get_const(5).unwrap();
        assert_eq!(refid.kind, ConstPoolKind::REFID);
        assert_eq!(refid.data.as_slice(), &refid_data);

        let tag = restored.get_const(6).unwrap();
        assert_eq!(tag.kind, ConstPoolKind::TAG);
        assert_eq!(u32::from_le_bytes(tag.data.clone().try_into().unwrap()), 3);

        // Verify instructions
        let instrs = decode_instructions(&restored.code).unwrap();
        assert_eq!(instrs.len(), 3);
        assert_eq!(instrs[0].op, 0x01);
        assert_eq!(instrs[1].op, 0x02);
        assert_eq!(instrs[2].imm, 0xDEAD);

        // Verify alignment
        let header = InvariantsHeader::from_bytes(&bytes).unwrap();
        assert_eq!(header.off_const_pool, InvariantsHeader::SIZE as u64);
        assert_eq!(header.off_code % INV_CONST_POOL_ALIGNMENT as u64, 0);
    }

    #[test]
    fn test_invariants_section_from_bytes_invalid_magic() {
        let mut section = InvariantsSection::new();
        section.add_const_u64(1);
        let mut bytes = section.to_bytes();
        bytes[0] = 0xFF;

        assert!(InvariantsSection::from_bytes(&bytes).is_err());
    }

    #[test]
    fn test_invariants_section_from_bytes_truncated() {
        assert!(InvariantsSection::from_bytes(&[0u8; 4]).is_err());
    }

    #[test]
    fn test_decode_instructions_not_aligned() {
        let not_aligned = vec![0u8; 15];
        assert!(decode_instructions(&not_aligned).is_err());

        let not_aligned = vec![0u8; 17];
        assert!(decode_instructions(&not_aligned).is_err());

        let aligned = vec![0u8; INSTRUCTION_SIZE * 5];
        assert!(decode_instructions(&aligned).is_ok());
        assert_eq!(decode_instructions(&aligned).unwrap().len(), 5);
    }

    #[test]
    fn test_instruction_roundtrip() {
        let instr = Instruction {
            op: 0xFFFF,
            a: 0xABCD,
            b: 0x12345678,
            imm: 0xFEDCBA9876543210,
        };
        let bytes = instr.to_bytes();
        assert_eq!(bytes.len(), 16);

        let restored = Instruction::from_bytes(&bytes).unwrap();
        assert_eq!(restored, instr);
    }

    #[test]
    fn test_instruction_from_bytes_too_small() {
        assert!(Instruction::from_bytes(&[0u8; 8]).is_err());
        assert!(Instruction::from_bytes(&[]).is_err());
    }

    #[test]
    fn test_mixed_const_types_serialization_order() {
        let mut section = InvariantsSection::new();
        section.inv_count = 1;

        // Add in specific order
        section.add_const(ConstPoolKind::BYTES, b"first");
        section.add_const_u64(100);
        section.add_const_sym(500);

        let bytes = section.to_bytes();
        let restored = InvariantsSection::from_bytes(&bytes).unwrap();

        assert_eq!(restored.get_const(0).unwrap().kind, ConstPoolKind::BYTES);
        assert_eq!(restored.get_const(1).unwrap().kind, ConstPoolKind::U64);
        assert_eq!(restored.get_const(2).unwrap().kind, ConstPoolKind::SYM);

        // Verify data integrity
        assert_eq!(restored.get_const(0).unwrap().data, b"first");
        assert_eq!(
            u64::from_le_bytes(
                restored
                    .get_const(1)
                    .unwrap()
                    .data
                    .clone()
                    .try_into()
                    .unwrap()
            ),
            100
        );
        assert_eq!(
            u32::from_le_bytes(
                restored
                    .get_const(2)
                    .unwrap()
                    .data
                    .clone()
                    .try_into()
                    .unwrap()
            ),
            500
        );
    }

    #[test]
    fn test_code_alignment_after_const_pool() {
        let mut section = InvariantsSection::new();

        // Add entries that will not naturally align to 16 bytes
        // SYM entry: 1 + 4 + 4 = 9 bytes
        section.add_const_sym(1);
        // SYM entry: 1 + 4 + 4 = 9 bytes (total 18 bytes, not aligned to 16)
        section.add_const_sym(2);

        let bytes = section.to_bytes();
        let header = InvariantsHeader::from_bytes(&bytes).unwrap();

        // off_code should be aligned to 16 bytes
        assert_eq!(header.off_code % 16, 0);
        assert!(header.off_code > header.off_const_pool + 18);

        // Roundtrip should preserve instructions
        section.emit_instruction(1, 0, 0, 0);
        section.emit_instruction(2, 0, 0, 0);

        let bytes = section.to_bytes();
        let restored = InvariantsSection::from_bytes(&bytes).unwrap();
        assert_eq!(restored.code.len(), 32);
    }

    #[test]
    fn test_compute_invariants_crc() {
        let data = vec![0u8, 1, 2, 3, 4, 5, 77, 88, 99];
        let crc = compute_invariants_crc(&data);
        // Just verify it returns a value (CRC is deterministic)
        assert_eq!(crc, crc32(&data));
    }
}
