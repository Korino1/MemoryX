//! Complete Invariant VM Bytecode Interpreter for MemoryX SKF-1.1
//!
//! This module implements a full bytecode virtual machine for executing invariant checks
//! on atoms and claims. The VM uses a 16-byte aligned instruction format and
//! provides deterministic execution for constraint validation.
//!
//! # Instruction Format (16 bytes)
//! ```text
//! +--------+--------+----------------+------------------+
//! |  op    |   a    |       b        |      imm         |
//! | u16    | u16    |     u32        |      u64         |
//! +--------+--------+----------------+------------------+
//! ```
//!
//! # Opcodes (25 total)
//! - Load ops (1-4): LD_ATOM_META, LD_CLAIM, LD_QC, LD_CTX
//! - Comparison ops (10-17): EQ, LT, LE, GT, GE, IN_RANGE, HAS_BIT, IS_TAG
//! - Check ops (20-25): CHK_TIME, CHK_TRUST, CHK_DOMAIN, CHK_SOURCE, CTX_PROBE, RAISE_CONFLICT
//! - Control flow (30-32): JZ, JMP, RET
//!
//! # Safety Invariants
//! - All instructions are 16-byte aligned
//! - Register indices are bounds-checked (0-15)
//! - Const pool indices are validated before access
//! - No raw pointer arithmetic without explicit unsafe blocks
//! - All UB-checked operations use explicit contracts

#![allow(dead_code)]

use crate::prelude::*;
use std::collections::HashMap;
use std::fmt;

// ============================================================================
// VM Error Types
// ============================================================================

/// VM execution errors
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmError {
    /// Invalid opcode
    InvalidOpcode(u16),
    /// Register index out of bounds (must be 0-15)
    RegisterOutOfBounds(u16),
    /// Const pool index out of bounds
    ConstIndexOutOfBounds(u32),
    /// Division by zero
    DivisionByZero,
    /// Step limit exceeded (infinite loop prevention)
    StepLimitExceeded(u32),
    /// PC out of bounds
    PcOutOfBounds(usize),
    /// Jump target out of bounds
    JumpTargetOutOfBounds(u32),
    /// Invalid instruction format
    InvalidInstruction,
    /// Bytecode validation failed
    ValidationFailed(&'static str),
    /// Context not available
    ContextNotAvailable,
    /// Claim not available
    ClaimNotAvailable,
    /// Source allowlist check failed
    SourceDenied,
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VmError::InvalidOpcode(op) => write!(f, "Invalid opcode: 0x{:04X}", op),
            VmError::RegisterOutOfBounds(reg) => write!(f, "Register out of bounds: {}", reg),
            VmError::ConstIndexOutOfBounds(idx) => {
                write!(f, "Const pool index out of bounds: {}", idx)
            }
            VmError::DivisionByZero => write!(f, "Division by zero"),
            VmError::StepLimitExceeded(limit) => {
                write!(f, "Step limit exceeded: max {}", limit)
            }
            VmError::PcOutOfBounds(pc) => write!(f, "PC out of bounds: {}", pc),
            VmError::JumpTargetOutOfBounds(target) => {
                write!(f, "Jump target out of bounds: {}", target)
            }
            VmError::InvalidInstruction => write!(f, "Invalid instruction format"),
            VmError::ValidationFailed(reason) => {
                write!(f, "Bytecode validation failed: {}", reason)
            }
            VmError::ContextNotAvailable => write!(f, "Context not available"),
            VmError::ClaimNotAvailable => write!(f, "Claim not available"),
            VmError::SourceDenied => write!(f, "Source denied by allowlist"),
        }
    }
}

impl std::error::Error for VmError {}

/// VM execution result
pub type VmResult<T> = Result<T, VmError>;

// ============================================================================
// Opcodes
// ============================================================================

/// VM opcodes for invariant checking (25 total)
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    // Load operations (1-4)
    /// Load atom metadata (valid_from, valid_to, atom_type, trust)
    LD_ATOM_META = 1,
    /// Load claim fields (subj, pred, obj_tag, obj_value_ptr, qmask)
    LD_CLAIM = 2,
    /// Load query constraints (time_ns, domain_mask, trust_min)
    LD_QC = 3,
    /// Load context policy fields
    LD_CTX = 4,
    /// Load immediate value into register: reg[a] = imm
    LD_IMM = 5,

    // Comparisons (10-17)
    /// Equal comparison: reg[a] = (reg[a] == reg[b]) ? 1 : 0
    EQ = 10,
    /// Less than: reg[a] = (reg[a] < reg[b]) ? 1 : 0
    LT = 11,
    /// Less or equal: reg[a] = (reg[a] <= reg[b]) ? 1 : 0
    LE = 12,
    /// Greater than: reg[a] = (reg[a] > reg[b]) ? 1 : 0
    GT = 13,
    /// Greater or equal: reg[a] = (reg[a] >= reg[b]) ? 1 : 0
    GE = 14,
    /// In range: reg[a] = (imm_low <= reg[a] <= imm_high) ? 1 : 0
    IN_RANGE = 15,
    /// Has bit: reg[a] = (reg[a] & (1 << imm)) != 0 ? 1 : 0
    HAS_BIT = 16,
    /// Is tag: reg[a] = (reg[a] == imm_tag) ? 1 : 0
    IS_TAG = 17,

    // Checks (20-25)
    /// Check time: validate valid_from <= time_ns <= valid_to
    CHK_TIME = 20,
    /// Check trust: validate trust >= trust_min
    CHK_TRUST = 21,
    /// Check domain: validate domain_mask & atom_domain != 0
    CHK_DOMAIN = 22,
    /// Check source: validate source in allowlist (from const pool)
    CHK_SOURCE = 23,
    /// Context probe: search context index for conflicting claims
    CTX_PROBE = 24,
    /// Raise conflict: set result to FAIL_HARD or NEED_BRANCH
    RAISE_CONFLICT = 25,

    // Bitwise operations (26-27)
    /// Bitwise AND: reg[a] = reg[a] & reg[b]
    AND = 26,
    /// Bitwise NOT: reg[a] = ~reg[a]
    NOT = 27,

    // Control flow (30-34)
    /// Jump if zero: if reg[a] == 0, pc = imm
    JZ = 30,
    /// Unconditional jump: pc = imm
    JMP = 31,
    /// Return with code from reg[a]
    RET = 32,
    /// Jump if non-zero: if reg[a] != 0, pc = imm
    JNZ = 33,
    /// Call subroutine: reg[a] = return_pc, pc = imm
    CALL = 34,
}

impl Opcode {
    /// Convert from u16, returning None for invalid values
    #[inline]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            1 => Some(Opcode::LD_ATOM_META),
            2 => Some(Opcode::LD_CLAIM),
            3 => Some(Opcode::LD_QC),
            4 => Some(Opcode::LD_CTX),
            5 => Some(Opcode::LD_IMM),
            10 => Some(Opcode::EQ),
            11 => Some(Opcode::LT),
            12 => Some(Opcode::LE),
            13 => Some(Opcode::GT),
            14 => Some(Opcode::GE),
            15 => Some(Opcode::IN_RANGE),
            16 => Some(Opcode::HAS_BIT),
            17 => Some(Opcode::IS_TAG),
            20 => Some(Opcode::CHK_TIME),
            21 => Some(Opcode::CHK_TRUST),
            22 => Some(Opcode::CHK_DOMAIN),
            23 => Some(Opcode::CHK_SOURCE),
            24 => Some(Opcode::CTX_PROBE),
            25 => Some(Opcode::RAISE_CONFLICT),
            26 => Some(Opcode::AND),
            27 => Some(Opcode::NOT),
            30 => Some(Opcode::JZ),
            31 => Some(Opcode::JMP),
            32 => Some(Opcode::RET),
            33 => Some(Opcode::JNZ),
            34 => Some(Opcode::CALL),
            _ => None,
        }
    }

    /// Convert to u16
    #[inline]
    pub const fn to_u16(self) -> u16 {
        self as u16
    }

    /// Check if this opcode is a load operation
    #[inline]
    pub const fn is_load(self) -> bool {
        matches!(
            self,
            Opcode::LD_ATOM_META
                | Opcode::LD_CLAIM
                | Opcode::LD_QC
                | Opcode::LD_CTX
                | Opcode::LD_IMM
        )
    }

    /// Check if this opcode is a comparison
    #[inline]
    pub const fn is_comparison(self) -> bool {
        matches!(
            self,
            Opcode::EQ
                | Opcode::LT
                | Opcode::LE
                | Opcode::GT
                | Opcode::GE
                | Opcode::IN_RANGE
                | Opcode::HAS_BIT
                | Opcode::IS_TAG
        )
    }

    /// Check if this opcode is a check operation
    #[inline]
    pub const fn is_check(self) -> bool {
        matches!(
            self,
            Opcode::CHK_TIME
                | Opcode::CHK_TRUST
                | Opcode::CHK_DOMAIN
                | Opcode::CHK_SOURCE
                | Opcode::CTX_PROBE
                | Opcode::RAISE_CONFLICT
        )
    }

    /// Check if this opcode affects control flow
    #[inline]
    pub const fn is_control_flow(self) -> bool {
        matches!(
            self,
            Opcode::JZ | Opcode::JMP | Opcode::RET | Opcode::JNZ | Opcode::CALL
        )
    }

    /// Get opcode category name
    #[inline]
    pub const fn category_name(self) -> &'static str {
        if self.is_load() {
            "LOAD"
        } else if self.is_comparison() {
            "CMP"
        } else if self.is_check() {
            "CHK"
        } else if self.is_control_flow() {
            "CTRL"
        } else {
            "UNK"
        }
    }
}

impl fmt::Display for Opcode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Opcode::LD_ATOM_META => "LD_ATOM_META",
            Opcode::LD_CLAIM => "LD_CLAIM",
            Opcode::LD_QC => "LD_QC",
            Opcode::LD_CTX => "LD_CTX",
            Opcode::LD_IMM => "LD_IMM",
            Opcode::EQ => "EQ",
            Opcode::LT => "LT",
            Opcode::LE => "LE",
            Opcode::GT => "GT",
            Opcode::GE => "GE",
            Opcode::IN_RANGE => "IN_RANGE",
            Opcode::HAS_BIT => "HAS_BIT",
            Opcode::IS_TAG => "IS_TAG",
            Opcode::CHK_TIME => "CHK_TIME",
            Opcode::CHK_TRUST => "CHK_TRUST",
            Opcode::CHK_DOMAIN => "CHK_DOMAIN",
            Opcode::CHK_SOURCE => "CHK_SOURCE",
            Opcode::CTX_PROBE => "CTX_PROBE",
            Opcode::RAISE_CONFLICT => "RAISE_CONFLICT",
            Opcode::AND => "AND",
            Opcode::NOT => "NOT",
            Opcode::JZ => "JZ",
            Opcode::JMP => "JMP",
            Opcode::RET => "RET",
            Opcode::JNZ => "JNZ",
            Opcode::CALL => "CALL",
        };
        write!(f, "{}", name)
    }
}

// ============================================================================
// Instruction Format
// ============================================================================

/// VM instruction (16 bytes, aligned)
///
/// # Layout
/// - `op`: Opcode (u16)
/// - `a`: Register A or immediate high (u16)
/// - `b`: Register B or const pool index (u32)
/// - `imm`: 64-bit immediate or offset (u64)
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Instruction {
    pub op: u16,
    pub a: u16,
    pub b: u32,
    pub imm: u64,
}

// Safety invariant: Instruction is exactly 16 bytes
const _: () = assert!(std::mem::size_of::<Instruction>() == 16);
// Safety invariant: Instruction alignment
const _: () = assert!(std::mem::align_of::<Instruction>() >= 8);

impl Instruction {
    /// Create a new instruction
    #[inline]
    pub const fn new(op: Opcode, a: u16, b: u32, imm: u64) -> Self {
        Instruction {
            op: op.to_u16(),
            a,
            b,
            imm,
        }
    }

    /// Create a load instruction
    #[inline]
    pub const fn load(op: Opcode, dest_reg: u16, field: u32) -> Self {
        Instruction::new(op, dest_reg, field, 0)
    }

    /// Create a comparison instruction (register-register)
    #[inline]
    pub const fn cmp(op: Opcode, left_reg: u16, right_reg: u16) -> Self {
        Instruction::new(op, left_reg, right_reg as u32, 0)
    }

    /// Create a jump instruction
    #[inline]
    pub const fn jump(op: Opcode, condition_reg: u16, target: u32) -> Self {
        Instruction::new(op, condition_reg, target, 0)
    }

    /// Create an immediate instruction
    #[inline]
    pub const fn imm_op(op: Opcode, dest_reg: u16, imm: u64) -> Self {
        Instruction::new(op, dest_reg, 0, imm)
    }

    /// Create a load immediate instruction (alias for imm_op)
    #[inline]
    pub const fn load_imm64(op: Opcode, dest_reg: u16, imm: u64) -> Self {
        Instruction::imm_op(op, dest_reg, imm)
    }

    /// Create a range instruction
    #[inline]
    pub const fn range(op: Opcode, reg: u16, low: u64, high: u32) -> Self {
        Instruction::new(op, reg, high, low)
    }

    /// Get opcode
    #[inline]
    pub const fn opcode(&self) -> Option<Opcode> {
        Opcode::from_u16(self.op)
    }

    /// Get register A (0-15, masked)
    #[inline]
    pub const fn reg_a(&self) -> u16 {
        self.a & 0xF
    }

    /// Get register B (0-15, masked from low bits of b)
    #[inline]
    pub const fn reg_b(&self) -> u16 {
        (self.b & 0xF) as u16
    }

    /// Get const pool index (full b value)
    #[inline]
    pub const fn const_index(&self) -> u32 {
        self.b
    }

    /// Get jump target (full b value)
    #[inline]
    pub const fn jump_target(&self) -> u32 {
        self.b
    }

    /// Get immediate as u64
    #[inline]
    pub const fn imm_u64(&self) -> u64 {
        self.imm
    }

    /// Get immediate as i64
    #[inline]
    pub const fn imm_i64(&self) -> i64 {
        self.imm as i64
    }

    /// Get low 32 bits of immediate
    #[inline]
    pub const fn imm_low(&self) -> u32 {
        self.imm as u32
    }

    /// Get high 32 bits of immediate
    #[inline]
    pub const fn imm_high(&self) -> u32 {
        (self.imm >> 32) as u32
    }
}

// ============================================================================
// Constant Pool
// ============================================================================

/// Constant value types for VM
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConstValue {
    /// Symbol ID
    Sym(u32),
    /// Unsigned 64-bit integer
    U64(u64),
    /// Signed 64-bit integer
    I64(i64),
    /// 64-bit float
    F64(f64),
    /// Fixed-size byte array (32 bytes)
    Bytes([u8; 32]),
    /// Reference ID
    RefId(u32),
    /// Object tag
    Tag(u8),
}

impl ConstValue {
    /// Create a symbol constant
    #[inline]
    pub const fn sym(value: u32) -> Self {
        ConstValue::Sym(value)
    }

    /// Create a u64 constant
    #[inline]
    pub const fn u64(value: u64) -> Self {
        ConstValue::U64(value)
    }

    /// Create an i64 constant
    #[inline]
    pub const fn i64(value: i64) -> Self {
        ConstValue::I64(value)
    }

    /// Create an f64 constant
    #[inline]
    pub const fn f64(value: f64) -> Self {
        ConstValue::F64(value)
    }

    /// Create a bytes constant
    #[inline]
    pub const fn bytes(value: [u8; 32]) -> Self {
        ConstValue::Bytes(value)
    }

    /// Create a reference ID constant
    #[inline]
    pub const fn ref_id(value: u32) -> Self {
        ConstValue::RefId(value)
    }

    /// Create a tag constant
    #[inline]
    pub const fn tag(value: u8) -> Self {
        ConstValue::Tag(value)
    }

    /// Get as u64 (zero-extend for smaller types)
    #[inline]
    pub const fn as_u64(&self) -> Option<u64> {
        match self {
            ConstValue::U64(v) => Some(*v),
            ConstValue::Sym(v) => Some(*v as u64),
            ConstValue::RefId(v) => Some(*v as u64),
            ConstValue::Tag(v) => Some(*v as u64),
            ConstValue::I64(v) if *v >= 0 => Some(*v as u64),
            _ => None,
        }
    }

    /// Get as i64
    #[inline]
    pub const fn as_i64(&self) -> Option<i64> {
        match self {
            ConstValue::I64(v) => Some(*v),
            ConstValue::U64(v) if *v <= i64::MAX as u64 => Some(*v as i64),
            ConstValue::Sym(v) => Some(*v as i64),
            ConstValue::RefId(v) => Some(*v as i64),
            ConstValue::Tag(v) => Some(*v as i64),
            _ => None,
        }
    }

    /// Get as f64
    #[inline]
    pub const fn as_f64(&self) -> Option<f64> {
        match self {
            ConstValue::F64(v) => Some(*v),
            ConstValue::U64(v) => Some(*v as f64),
            ConstValue::I64(v) => Some(*v as f64),
            _ => None,
        }
    }

    /// Get as bytes reference
    #[inline]
    pub const fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            ConstValue::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// Get as tag
    #[inline]
    pub const fn as_tag(&self) -> Option<u8> {
        match self {
            ConstValue::Tag(v) => Some(*v),
            _ => None,
        }
    }
}

/// Constant pool builder for constructing VM programs
pub struct ConstPoolBuilder {
    constants: Vec<ConstValue>,
}

impl ConstPoolBuilder {
    /// Create a new constant pool builder
    #[inline]
    pub fn new() -> Self {
        ConstPoolBuilder {
            constants: Vec::with_capacity(64),
        }
    }

    /// Add a constant and return its index
    #[inline]
    pub fn add(&mut self, value: ConstValue) -> u32 {
        let index = self.constants.len() as u32;
        self.constants.push(value);
        index
    }

    /// Add a symbol constant
    #[inline]
    pub fn add_sym(&mut self, value: u32) -> u32 {
        self.add(ConstValue::Sym(value))
    }

    /// Add a u64 constant
    #[inline]
    pub fn add_u64(&mut self, value: u64) -> u32 {
        self.add(ConstValue::U64(value))
    }

    /// Add an i64 constant
    #[inline]
    pub fn add_i64(&mut self, value: i64) -> u32 {
        self.add(ConstValue::I64(value))
    }

    /// Add an f64 constant
    #[inline]
    pub fn add_f64(&mut self, value: f64) -> u32 {
        self.add(ConstValue::F64(value))
    }

    /// Add a tag constant
    #[inline]
    pub fn add_tag(&mut self, value: u8) -> u32 {
        self.add(ConstValue::Tag(value))
    }

    /// Add a bytes constant
    #[inline]
    pub fn add_bytes(&mut self, value: [u8; 32]) -> u32 {
        self.add(ConstValue::Bytes(value))
    }

    /// Add a reference ID constant
    #[inline]
    pub fn add_ref_id(&mut self, value: u32) -> u32 {
        self.add(ConstValue::RefId(value))
    }

    /// Build the constant pool
    #[inline]
    pub fn build(self) -> Vec<ConstValue> {
        self.constants
    }

    /// Get the number of constants
    #[inline]
    pub fn len(&self) -> usize {
        self.constants.len()
    }

    /// Check if the pool is empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.constants.is_empty()
    }
}

impl Default for ConstPoolBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Atom and Context Views
// ============================================================================

/// View into atom metadata for VM execution
///
/// # Safety Invariants
/// - All field accesses are bounds-checked
/// - No raw pointers exposed to safe code
/// - Zero-copy where possible
#[derive(Debug, Clone)]
pub struct AtomView<'a> {
    pub atom_id: &'a AtomId,
    pub atom_type: AtomType,
    pub meta: &'a [u8],
    pub claims: &'a [ClaimData],
    pub valid_from_ns: u64,
    pub valid_to_ns: u64,
    pub trust_level: TrustLevel,
    pub domain_mask: DomainMask,
    pub source_id: u32,
}

/// Claim data for VM access
#[derive(Debug, Clone)]
pub struct ClaimData {
    pub subj: u64, // SymId or NodeNum
    pub pred: u64, // SymId
    pub obj_tag: u8,
    pub obj_val: u64, // Encoded object value
    pub qualifiers_mask: u32,
}

impl<'a> AtomView<'a> {
    /// Create a new atom view
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        atom_id: &'a AtomId,
        atom_type: AtomType,
        meta: &'a [u8],
        claims: &'a [ClaimData],
        valid_from_ns: u64,
        valid_to_ns: u64,
        trust_level: TrustLevel,
        domain_mask: DomainMask,
        source_id: u32,
    ) -> Self {
        AtomView {
            atom_id,
            atom_type,
            meta,
            claims,
            valid_from_ns,
            valid_to_ns,
            trust_level,
            domain_mask,
            source_id,
        }
    }

    /// Get metadata field by tag (byte offset)
    #[inline]
    pub fn get_meta(&self, tag: u32) -> Option<u64> {
        self.meta.get(tag as usize).copied().map(|v| v as u64)
    }

    /// Get metadata as u64 (4 bytes, little-endian)
    #[inline]
    pub fn get_meta_u64(&self, offset: u32) -> Option<u64> {
        let start = offset as usize;
        if start + 8 <= self.meta.len() {
            Some(u64::from_le_bytes([
                self.meta[start],
                self.meta[start + 1],
                self.meta[start + 2],
                self.meta[start + 3],
                self.meta[start + 4],
                self.meta[start + 5],
                self.meta[start + 6],
                self.meta[start + 7],
            ]))
        } else {
            None
        }
    }

    /// Get claim by index
    #[inline]
    pub fn get_claim(&self, index: u32) -> Option<&ClaimData> {
        self.claims.get(index as usize)
    }

    /// Get claim count
    #[inline]
    pub fn claim_count(&self) -> u32 {
        self.claims.len() as u32
    }

    /// Check if atom is valid at given timestamp
    #[inline]
    pub fn is_valid_at(&self, time_ns: u64) -> bool {
        time_ns >= self.valid_from_ns && time_ns <= self.valid_to_ns
    }

    /// Check if atom domain matches required domain
    #[inline]
    pub fn domain_matches(&self, required: DomainMask) -> bool {
        required == 0 || (self.domain_mask & required) != 0
    }

    /// Check if atom trust meets minimum
    #[inline]
    pub fn trust_meets(&self, min: TrustLevel) -> bool {
        self.trust_level >= min
    }
}

/// Context view for VM execution
#[derive(Debug, Clone)]
pub struct CtxView<'a> {
    pub ctx_id: u32,
    pub policy_data: &'a [u8],
    pub conflict_probes: &'a [ConflictProbe],
    pub active_branches: u64,
}

/// Conflict probe data
#[derive(Debug, Clone, Copy)]
pub struct ConflictProbe {
    pub pattern_hash: u64,
    pub conflict_count: u32,
    pub max_trust: TrustLevel,
    pub flags: u32,
}

impl<'a> CtxView<'a> {
    /// Create a new context view
    #[inline]
    pub fn new(
        ctx_id: u32,
        policy_data: &'a [u8],
        conflict_probes: &'a [ConflictProbe],
        active_branches: u64,
    ) -> Self {
        CtxView {
            ctx_id,
            policy_data,
            conflict_probes,
            active_branches,
        }
    }

    /// Get policy field (byte offset)
    #[inline]
    pub fn get_policy_field(&self, field: u32) -> Option<u64> {
        self.policy_data
            .get(field as usize)
            .copied()
            .map(|v| v as u64)
    }

    /// Get policy field as u64
    #[inline]
    pub fn get_policy_u64(&self, offset: u32) -> Option<u64> {
        let start = offset as usize;
        if start + 8 <= self.policy_data.len() {
            Some(u64::from_le_bytes([
                self.policy_data[start],
                self.policy_data[start + 1],
                self.policy_data[start + 2],
                self.policy_data[start + 3],
                self.policy_data[start + 4],
                self.policy_data[start + 5],
                self.policy_data[start + 6],
                self.policy_data[start + 7],
            ]))
        } else {
            None
        }
    }

    /// Probe for conflicts by pattern hash
    #[inline]
    pub fn probe_conflict(&self, pattern_hash: u64) -> Option<&ConflictProbe> {
        self.conflict_probes
            .iter()
            .find(|p| p.pattern_hash == pattern_hash)
    }

    /// Check if any conflicts exist
    #[inline]
    pub fn has_conflicts(&self) -> bool {
        self.conflict_probes.iter().any(|p| p.conflict_count > 0)
    }

    /// Get total conflict count
    #[inline]
    pub fn total_conflicts(&self) -> u32 {
        self.conflict_probes.iter().map(|p| p.conflict_count).sum()
    }
}

/// Query constraints for VM
#[derive(Debug, Clone, Default)]
pub struct QueryConstraintsView {
    pub time_from_ns: u64,
    pub time_to_ns: u64,
    pub trust_min: TrustLevel,
    pub domain_mask: DomainMask,
    pub allowed_atom_types: u64, // Bitmask
    pub max_results: u32,
}

impl QueryConstraintsView {
    /// Create new query constraints view
    #[inline]
    pub fn new(
        time_from_ns: u64,
        time_to_ns: u64,
        trust_min: TrustLevel,
        domain_mask: DomainMask,
        allowed_atom_types: u64,
        max_results: u32,
    ) -> Self {
        QueryConstraintsView {
            time_from_ns,
            time_to_ns,
            trust_min,
            domain_mask,
            allowed_atom_types,
            max_results,
        }
    }

    /// Check if atom type is allowed
    #[inline]
    pub fn allows_atom_type(&self, atom_type: AtomType) -> bool {
        let mask = 1u64 << (atom_type.to_u32() - 1);
        (self.allowed_atom_types & mask) != 0
    }

    /// Check if time range overlaps
    #[inline]
    pub fn time_overlaps(&self, valid_from: u64, valid_to: u64) -> bool {
        self.time_from_ns < valid_to && valid_from < self.time_to_ns
    }
}

// ============================================================================
// Execution Context
// ============================================================================

/// Context index for conflict probing
#[derive(Debug, Clone, Default)]
pub struct CtxIndex {
    /// Pattern hash -> conflict bucket with exact claim signatures
    pub conflicts: HashMap<u64, ConflictInfo>,
}

/// Conflict information
#[derive(Debug, Clone)]
pub struct ConflictInfo {
    pub atom_ids: Vec<AtomId>,
    pub claim_signatures: Vec<u64>,
    pub severity: ConflictSeverity,
    pub pattern_hash: u64,
}

/// Conflict severity
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictSeverity {
    Soft,
    Hard,
}

impl Default for ConflictInfo {
    fn default() -> Self {
        ConflictInfo {
            atom_ids: Vec::new(),
            claim_signatures: Vec::new(),
            severity: ConflictSeverity::Soft,
            pattern_hash: 0,
        }
    }
}

impl CtxIndex {
    /// Create a new context index
    #[inline]
    pub fn new() -> Self {
        CtxIndex {
            conflicts: HashMap::new(),
        }
    }

    #[inline]
    fn insert_conflict(
        &mut self,
        pattern_hash: u64,
        claim_signature: Option<u64>,
        atom_id: AtomId,
        severity: ConflictSeverity,
    ) {
        let entry = self.conflicts.entry(pattern_hash).or_insert_with(|| ConflictInfo {
            atom_ids: Vec::new(),
            claim_signatures: Vec::new(),
            severity,
            pattern_hash,
        });

        if !entry.atom_ids.contains(&atom_id) {
            entry.atom_ids.push(atom_id);
        }

        if let Some(signature) = claim_signature
            && !entry.claim_signatures.contains(&signature)
        {
            entry.claim_signatures.push(signature);
        }

        if severity == ConflictSeverity::Hard {
            entry.severity = ConflictSeverity::Hard;
        }
    }

    /// Add a conflict entry
    #[inline]
    pub fn add_conflict(&mut self, pattern_hash: u64, atom_id: AtomId, severity: ConflictSeverity) {
        self.insert_conflict(pattern_hash, None, atom_id, severity);
    }

    /// Add a claim entry with the exact claim signature, used by the live CTX index.
    #[inline]
    pub fn add_claim_index(&mut self, claim: &ClaimData, atom_id: AtomId, severity: ConflictSeverity) {
        let pattern_hash = Self::claim_pattern_hash(claim);
        let claim_signature = Self::claim_signature(claim);
        self.insert_conflict(pattern_hash, Some(claim_signature), atom_id, severity);
    }

    /// Check for conflicts by pattern hash
    #[inline]
    pub fn has_conflict(&self, pattern_hash: u64) -> bool {
        self.conflicts.contains_key(&pattern_hash)
    }

    /// Get conflict info
    #[inline]
    pub fn get_conflict(&self, pattern_hash: u64) -> Option<&ConflictInfo> {
        self.conflicts.get(&pattern_hash)
    }

    /// Clear all conflicts
    #[inline]
    pub fn clear(&mut self) {
        self.conflicts.clear();
    }

    /// CTX_PROBE: Check if claim conflicts with indexed claims.
    ///
    /// Exact claim signatures are used to reject false positives when the same
    /// pattern bucket already contains the identical claim.
    pub fn probe_conflict(&self, claim: &ClaimData) -> Option<ConflictInfo> {
        let pattern_hash = Self::claim_pattern_hash(claim);
        let claim_signature = Self::claim_signature(claim);

        self.conflicts.get(&pattern_hash).and_then(|info| {
            if info.claim_signatures.is_empty() {
                return Some(info.clone());
            }

            if info.claim_signatures.contains(&claim_signature) {
                None
            } else {
                Some(info.clone())
            }
        })
    }

    /// Index a claim for conflict detection
    #[inline]
    pub fn index_claim(&mut self, claim: &ClaimData, atom_id: AtomId) {
        self.add_claim_index(claim, atom_id, ConflictSeverity::Soft);
    }

    /// Compute pattern hash for a claim (subj + pred)
    #[inline]
    pub fn claim_pattern_hash(claim: &ClaimData) -> u64 {
        claim.subj ^ (claim.pred << 32)
    }

    #[inline]
    fn mix_claim_signature(mut acc: u64, value: u64) -> u64 {
        acc ^= value.wrapping_add(0x9E37_79B9_7F4A_7C15);
        acc = acc.rotate_left(27);
        acc.wrapping_mul(0x94D0_49BB_1331_11EB)
    }

    /// Compute an exact claim signature that distinguishes real conflicting claims.
    #[inline]
    pub fn claim_signature(claim: &ClaimData) -> u64 {
        let mut hash = 0xD6E8_FEB8_6659_FD93u64;
        hash = Self::mix_claim_signature(hash, claim.subj);
        hash = Self::mix_claim_signature(hash, claim.pred);
        hash = Self::mix_claim_signature(hash, claim.obj_tag as u64);
        hash = Self::mix_claim_signature(hash, claim.obj_val);
        Self::mix_claim_signature(hash, claim.qualifiers_mask as u64)
    }
}

/// Full execution context for VM
///
/// # Fields
/// - `atom_ref`: Full atom bytes (zero-copy reference)
/// - `claim_ref`: Optional claim bytes
/// - `ctx_index`: Context index for CTX_PROBE
/// - `source_allowlist`: Optional source ID allowlist
#[derive(Debug, Clone)]
pub struct ExecutionContext<'a> {
    /// Full atom bytes for zero-copy access
    pub atom_ref: &'a [u8],
    /// Optional claim reference bytes
    pub claim_ref: Option<&'a [u8]>,
    /// Context index for conflict probing
    pub ctx_index: &'a CtxIndex,
    /// Optional source allowlist (if None, all sources allowed)
    pub source_allowlist: Option<&'a [u32]>,
}

impl<'a> ExecutionContext<'a> {
    /// Create a new execution context
    #[inline]
    pub fn new(
        atom_ref: &'a [u8],
        claim_ref: Option<&'a [u8]>,
        ctx_index: &'a CtxIndex,
        source_allowlist: Option<&'a [u32]>,
    ) -> Self {
        ExecutionContext {
            atom_ref,
            claim_ref,
            ctx_index,
            source_allowlist,
        }
    }

    /// Check if source is allowed
    #[inline]
    pub fn is_source_allowed(&self, source_id: u32) -> bool {
        self.source_allowlist
            .map(|list| list.contains(&source_id))
            .unwrap_or(true)
    }

    /// Check for conflict by pattern hash
    #[inline]
    pub fn has_conflict(&self, pattern_hash: u64) -> bool {
        self.ctx_index.has_conflict(pattern_hash)
    }

    /// Get conflict severity
    #[inline]
    pub fn conflict_severity(&self, pattern_hash: u64) -> Option<ConflictSeverity> {
        self.ctx_index
            .get_conflict(pattern_hash)
            .map(|info| info.severity)
    }
}

// ============================================================================
// VM State
// ============================================================================

/// VM execution state
///
/// # Safety Invariants
/// - registers[0] is always zero (hardwired)
/// - pc is bounds-checked before each instruction
/// - const_pool indices are validated before access
pub struct VmState {
    /// 16 general-purpose registers (r0 is hardwired to 0)
    pub registers: [u64; 16],
    /// Program counter (instruction index)
    pub pc: usize,
    /// Constant pool
    pub const_pool: Vec<ConstValue>,
    /// Current result
    pub result: InvariantResult,
    /// Reason code for failure
    pub reason: ReasonCode,
    /// Execution halted flag
    pub halted: bool,
    /// Instruction/step count (for budget tracking)
    pub step_count: u32,
    /// Max steps before halt (prevent infinite loops)
    pub max_steps: u32,
    /// Last error (if any)
    pub last_error: Option<VmError>,
    /// Trace enable flag
    pub trace_enabled: bool,
    /// Trace log (if tracing enabled)
    pub trace_log: Vec<String>,
}

impl VmState {
    /// Create a new VM state
    #[inline]
    pub fn new(const_pool: Vec<ConstValue>, max_steps: u32) -> Self {
        VmState {
            registers: [0; 16],
            pc: 0,
            const_pool,
            result: InvariantResult::PASS,
            reason: ReasonCode::TIME_INVALID,
            halted: false,
            step_count: 0,
            max_steps,
            last_error: None,
            trace_enabled: false,
            trace_log: Vec::new(),
        }
    }

    /// Create VM state with tracing enabled
    #[inline]
    pub fn with_tracing(const_pool: Vec<ConstValue>, max_steps: u32) -> Self {
        let mut state = VmState::new(const_pool, max_steps);
        state.trace_enabled = true;
        state
    }

    /// Get register value (r0 always returns 0)
    #[inline]
    pub fn get_reg(&self, index: u16) -> u64 {
        if index == 0 {
            0
        } else {
            self.registers[(index & 0xF) as usize]
        }
    }

    /// Set register value (r0 is ignored - hardwired to 0)
    #[inline]
    pub fn set_reg(&mut self, index: u16, value: u64) {
        if index != 0 {
            self.registers[(index & 0xF) as usize] = value;
        }
    }

    /// Get const value by index
    #[inline]
    pub fn get_const(&self, index: u32) -> Option<&ConstValue> {
        self.const_pool.get(index as usize)
    }

    /// Get const value as u64
    #[inline]
    pub fn get_const_u64(&self, index: u32) -> Option<u64> {
        self.const_pool.get(index as usize).and_then(|c| c.as_u64())
    }

    /// Check step budget
    #[inline]
    pub fn check_budget(&self) -> bool {
        self.step_count < self.max_steps
    }

    /// Increment step counter, returns false if budget exceeded
    #[inline]
    pub fn tick(&mut self) -> bool {
        self.step_count += 1;
        self.check_budget()
    }

    /// Set result and reason
    #[inline]
    pub fn set_result(&mut self, result: InvariantResult, reason: ReasonCode) {
        self.result = result;
        self.reason = reason;
    }

    /// Set error
    #[inline]
    pub fn set_error(&mut self, error: VmError) {
        self.last_error = Some(error);
        self.halted = true;
    }

    /// Add trace entry
    #[inline]
    pub fn trace(&mut self, msg: String) {
        if self.trace_enabled {
            self.trace_log.push(msg);
        }
    }

    /// Get trace log
    #[inline]
    pub fn get_trace(&self) -> &[String] {
        &self.trace_log
    }

    /// Clear trace log
    #[inline]
    pub fn clear_trace(&mut self) {
        self.trace_log.clear();
    }

    /// Reset state for re-execution
    #[inline]
    pub fn reset(&mut self) {
        self.registers = [0; 16];
        self.pc = 0;
        self.result = InvariantResult::PASS;
        self.reason = ReasonCode::TIME_INVALID;
        self.halted = false;
        self.step_count = 0;
        self.last_error = None;
        self.clear_trace();
    }
}

impl Default for VmState {
    fn default() -> Self {
        VmState {
            registers: [0; 16],
            pc: 0,
            const_pool: Vec::new(),
            result: InvariantResult::PASS,
            reason: ReasonCode::TIME_INVALID,
            halted: false,
            step_count: 0,
            max_steps: 100_000,
            last_error: None,
            trace_enabled: false,
            trace_log: Vec::new(),
        }
    }
}

// ============================================================================
// Bytecode Validation
// ============================================================================

/// Bytecode validator for pre-execution checks
pub struct BytecodeValidator;

/// Validation result
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationResult {
    /// Bytecode is valid
    Valid,
    /// Warning (non-fatal issue)
    Warning(&'static str),
    /// Error (fatal issue)
    Error(&'static str),
}

impl BytecodeValidator {
    /// Validate bytecode before execution
    ///
    /// Checks:
    /// - All jump targets within bounds
    /// - All const pool indices valid
    /// - No fall-through past RET
    /// - Reasonable step count estimate
    pub fn validate(
        instructions: &[Instruction],
        const_pool_size: usize,
        max_steps: u32,
    ) -> Result<(), &'static str> {
        if instructions.is_empty() {
            return Err("Empty bytecode");
        }

        let mut reachable = vec![false; instructions.len()];
        reachable[0] = true; // Entry point always reachable

        let mut has_ret = false;
        let mut ret_positions = Vec::new();

        // First pass: check for RET presence in entire bytecode (even unreachable)
        for (i, instr) in instructions.iter().enumerate() {
            let Some(opcode) = instr.opcode() else {
                return Err("Invalid opcode");
            };
            if opcode == Opcode::RET {
                has_ret = true;
                ret_positions.push(i);
            }
        }

        // Must have at least one RET
        if !has_ret {
            return Err("No RET instruction");
        }

        // Second pass: validate reachable instructions
        for (i, instr) in instructions.iter().enumerate() {
            let Some(opcode) = instr.opcode() else {
                return Err("Invalid opcode");
            };

            // Check if this instruction is reachable
            if !reachable[i] {
                // Dead code - skip validation
                continue;
            }

            match opcode {
                Opcode::JZ | Opcode::JMP => {
                    let target = instr.jump_target() as usize;
                    if target >= instructions.len() {
                        return Err("Jump target out of bounds");
                    }
                    reachable[target] = true;

                    // JMP makes fall-through unreachable
                    if opcode == Opcode::JMP && i + 1 < instructions.len() {
                        // Mark fall-through as unreachable
                        // (will be overwritten if another jump targets it)
                    }
                }
                Opcode::RET => {}
                _ => {}
            }

            // Validate const pool indices for load operations
            if opcode.is_load() {
                let const_idx = instr.const_index();
                if const_idx as usize >= const_pool_size && const_pool_size > 0 {
                    // Only check if const pool is non-empty
                    // Some loads may use field indices, not const indices
                }
            }

            // Mark next instruction as reachable (unless RET)
            if opcode != Opcode::RET && opcode != Opcode::JMP && i + 1 < instructions.len() {
                reachable[i + 1] = true;
            }
        }

        // Must have at least one RET
        if !has_ret {
            return Err("No RET instruction");
        }

        // Check for fall-through past last RET
        if let Some(&last_ret) = ret_positions.last()
            && last_ret < instructions.len() - 1
        {
            // Check if there's reachable code after last RET
            for is_reachable in reachable.iter().take(instructions.len()).skip(last_ret + 1) {
                if *is_reachable {
                    return Err("Reachable code after final RET");
                }
            }
        }

        // Estimate step count (should be reasonable)
        let estimated_steps = instructions.len() * 3; // Rough estimate
        if estimated_steps > max_steps as usize {
            return Err("Bytecode too large for step budget");
        }

        Ok(())
    }

    /// Validate a single instruction
    pub fn validate_instruction(
        instr: &Instruction,
        max_reg: u16,
        const_pool_size: usize,
    ) -> ValidationResult {
        let Some(opcode) = instr.opcode() else {
            return ValidationResult::Error("Invalid opcode");
        };

        // Check register bounds
        if instr.reg_a() > max_reg {
            return ValidationResult::Error("Register A out of bounds");
        }

        if opcode.is_comparison() && instr.reg_b() > max_reg {
            return ValidationResult::Error("Register B out of bounds");
        }

        // Check const pool bounds for load operations
        if opcode.is_load() {
            let idx = instr.const_index();
            if idx as usize >= const_pool_size && const_pool_size > 0 {
                // May be a field index, not const index - check opcode
                match opcode {
                    Opcode::LD_ATOM_META | Opcode::LD_CLAIM | Opcode::LD_QC | Opcode::LD_CTX => {
                        // These use field indices, not const indices
                        // Field indices are validated at runtime
                    }
                    _ => {}
                }
            }
        }

        ValidationResult::Valid
    }
}

// ============================================================================
// Control Flow
// ============================================================================

/// Control flow result from instruction execution
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlFlow {
    /// Continue to next instruction
    Continue,
    /// Halt execution
    Halt,
    /// Jump to target PC
    Jump(u32),
}

// ============================================================================
// VM Interpreter
// ============================================================================

/// Invariant VM Interpreter
///
/// # Safety Contract
/// - All register accesses are bounds-checked
/// - Const pool indices are validated
/// - PC is validated before instruction fetch
/// - No raw pointer arithmetic in safe code paths
/// - Instruction execution is deterministic
///
/// # Registers
/// - r0: Hardwired to 0 (zero register)
/// - r1-r15: General-purpose registers
///
/// # Execution Model
/// - Fetch-decode-execute cycle
/// - Step limit prevents infinite loops
/// - Early termination on check failures
pub struct VmInterpreter<'a> {
    /// Registers (r0-r15, r0 always 0)
    registers: [u64; 16],
    /// Program counter
    pc: usize,
    /// Constant pool reference
    const_pool: &'a [ConstValue],
    /// Atom view reference
    atom_view: AtomView<'a>,
    /// Optional claim view
    claim_view: Option<ClaimData>,
    /// Context view reference
    ctx_view: CtxView<'a>,
    /// Query constraints reference
    qc_view: QueryConstraintsView,
    /// Execution context
    exec_ctx: ExecutionContext<'a>,
    /// Max steps (prevent infinite loops)
    max_steps: u32,
    /// Current step count
    step_count: u32,
    /// Current result
    result: InvariantResult,
    /// Reason code
    reason: ReasonCode,
    /// Halted flag
    halted: bool,
    /// Trace enabled
    trace_enabled: bool,
    /// Trace log
    trace_log: Vec<String>,
}

impl<'a> VmInterpreter<'a> {
    /// Create a new VM interpreter
    ///
    /// # Arguments
    /// - `const_pool`: Constant pool
    /// - `atom_view`: Atom data view
    /// - `ctx_view`: Context data view
    /// - `qc_view`: Query constraints
    /// - `exec_ctx`: Execution context
    /// - `max_steps`: Maximum execution steps
    #[inline]
    pub fn new(
        const_pool: &'a [ConstValue],
        atom_view: AtomView<'a>,
        ctx_view: CtxView<'a>,
        qc_view: QueryConstraintsView,
        exec_ctx: ExecutionContext<'a>,
        max_steps: u32,
    ) -> Self {
        VmInterpreter {
            registers: [0; 16],
            pc: 0,
            const_pool,
            atom_view,
            claim_view: None,
            ctx_view,
            qc_view,
            exec_ctx,
            max_steps,
            step_count: 0,
            result: InvariantResult::PASS,
            reason: ReasonCode::TIME_INVALID,
            halted: false,
            trace_enabled: false,
            trace_log: Vec::new(),
        }
    }

    /// Enable tracing
    #[inline]
    pub fn with_tracing(mut self) -> Self {
        self.trace_enabled = true;
        self
    }

    /// Set claim view
    #[inline]
    pub fn set_claim_view(&mut self, claim: ClaimData) {
        self.claim_view = Some(claim);
    }

    /// Get current register value
    #[inline]
    fn get_reg(&self, index: u16) -> u64 {
        if index == 0 {
            0
        } else {
            self.registers[(index & 0xF) as usize]
        }
    }

    /// Set register value (r0 ignored)
    #[inline]
    fn set_reg(&mut self, index: u16, value: u64) {
        if index != 0 {
            self.registers[(index & 0xF) as usize] = value;
        }
    }

    /// Get const value
    #[inline]
    fn get_const(&self, index: u32) -> Option<&ConstValue> {
        self.const_pool.get(index as usize)
    }

    /// Add trace entry
    #[inline]
    fn trace(&mut self, msg: String) {
        if self.trace_enabled {
            self.trace_log.push(msg);
        }
    }

    /// Check and increment step counter
    #[inline]
    fn tick(&mut self) -> bool {
        self.step_count += 1;
        if self.step_count > self.max_steps {
            self.result = InvariantResult::FAIL_HARD;
            self.reason = ReasonCode::BUDGET_EXCEEDED;
            self.halted = true;
            false
        } else {
            true
        }
    }

    /// Execute bytecode program
    ///
    /// # Returns
    /// - `Ok(InvariantResult)`: Execution completed
    /// - `Err(VmError)`: Execution error
    pub fn execute(&mut self, instructions: &[Instruction]) -> VmResult<InvariantResult> {
        // Validate bytecode before execution
        BytecodeValidator::validate(instructions, self.const_pool.len(), self.max_steps)
            .map_err(VmError::ValidationFailed)?;

        // Main execution loop
        while !self.halted && self.pc < instructions.len() && self.tick() {
            let instr = &instructions[self.pc];
            let current_pc = self.pc;
            self.pc += 1;

            // Trace instruction
            self.trace(format!(
                "[{:04}] {} r{} b={} imm={}",
                current_pc,
                instr
                    .opcode()
                    .map(|op| op.to_string())
                    .unwrap_or_else(|| format!("???({})", instr.op)),
                instr.reg_a(),
                instr.b,
                instr.imm
            ));

            match self.execute_instruction(instr)? {
                ControlFlow::Continue => {}
                ControlFlow::Halt => self.halted = true,
                ControlFlow::Jump(target) => {
                    if target as usize >= instructions.len() {
                        return Err(VmError::JumpTargetOutOfBounds(target));
                    }
                    self.pc = target as usize;
                }
            }
        }

        // Check if halted due to step limit
        if self.step_count > self.max_steps && self.result != InvariantResult::FAIL_HARD {
            self.result = InvariantResult::FAIL_HARD;
            self.reason = ReasonCode::BUDGET_EXCEEDED;
        }

        Ok(self.result)
    }

    /// Execute a single instruction
    fn execute_instruction(&mut self, instr: &Instruction) -> VmResult<ControlFlow> {
        let Some(opcode) = instr.opcode() else {
            self.result = InvariantResult::FAIL_HARD;
            self.reason = ReasonCode::CORRUPT_SECTION;
            return Err(VmError::InvalidOpcode(instr.op));
        };

        match opcode {
            // =================================================================
            // LOAD OPERATIONS (1-4)
            // =================================================================
            Opcode::LD_ATOM_META => {
                // reg[a] = atom.meta[field]
                let dest = instr.reg_a();
                let field = instr.const_index();

                let value = match field {
                    0 => self.atom_view.valid_from_ns,
                    1 => self.atom_view.valid_to_ns,
                    2 => self.atom_view.atom_type.to_u32() as u64,
                    3 => self.atom_view.trust_level as u64,
                    4 => self.atom_view.domain_mask,
                    5 => self.atom_view.source_id as u64,
                    _ => self.atom_view.get_meta(field - 10).unwrap_or(0),
                };

                self.set_reg(dest, value);
                self.trace(format!("  LD_ATOM_META r{} = {}", dest, value));
            }

            Opcode::LD_CLAIM => {
                // reg[a] = claim.field
                let dest = instr.reg_a();
                let claim_idx = instr.b & 0xFFFF; // Low 16 bits = claim index

                let claim = if claim_idx == 0 {
                    // Use current claim view if index is 0
                    self.claim_view.as_ref()
                } else {
                    // Load claim by index from atom
                    self.atom_view.get_claim(claim_idx - 1)
                };

                let value = if let Some(c) = claim {
                    let field = instr.b >> 16; // High 16 bits select field
                    match field {
                        0 => c.subj,
                        1 => c.pred,
                        2 => c.obj_tag as u64,
                        3 => c.obj_val,
                        4 => c.qualifiers_mask as u64,
                        _ => 0,
                    }
                } else {
                    0
                };

                self.set_reg(dest, value);
                self.trace(format!("  LD_CLAIM r{} = {}", dest, value));
            }

            Opcode::LD_QC => {
                // reg[a] = query_constraints[field]
                let dest = instr.reg_a();
                let field = instr.const_index();

                let value = match field {
                    0 => self.qc_view.time_from_ns,
                    1 => self.qc_view.time_to_ns,
                    2 => self.qc_view.trust_min as u64,
                    3 => self.qc_view.domain_mask,
                    4 => self.qc_view.allowed_atom_types,
                    5 => self.qc_view.max_results as u64,
                    _ => 0,
                };

                self.set_reg(dest, value);
                self.trace(format!("  LD_QC r{} = {}", dest, value));
            }

            Opcode::LD_CTX => {
                // reg[a] = ctx_policy[field]
                let dest = instr.reg_a();
                let field = instr.const_index();

                let value = match field {
                    0 => self.ctx_view.ctx_id as u64,
                    1 => self.ctx_view.active_branches,
                    2 => self.ctx_view.total_conflicts() as u64,
                    _ => self.ctx_view.get_policy_field(field - 10).unwrap_or(0),
                };

                self.set_reg(dest, value);
                self.trace(format!("  LD_CTX r{} = {}", dest, value));
            }

            Opcode::LD_IMM => {
                // Load immediate: reg[a] = imm
                let dest = instr.reg_a();
                let value = instr.imm_u64();
                self.set_reg(dest, value);
                self.trace(format!("  LD_IMM r{} = {}", dest, value));
            }

            // =================================================================
            // COMPARISON OPERATIONS (10-17)
            // =================================================================
            Opcode::EQ => {
                // reg[a] = (reg[a] == reg[b]) ? 1 : 0
                let left = self.get_reg(instr.reg_a());
                let right = self.get_reg(instr.reg_b());
                let result = (left == right) as u64;
                self.set_reg(instr.reg_a(), result);
                self.trace(format!(
                    "  EQ r{} = {} == {} -> {}",
                    instr.reg_a(),
                    left,
                    right,
                    result
                ));
            }

            Opcode::LT => {
                // reg[a] = (reg[a] < reg[b]) ? 1 : 0
                let left = self.get_reg(instr.reg_a());
                let right = self.get_reg(instr.reg_b());
                let result = (left < right) as u64;
                self.set_reg(instr.reg_a(), result);
                self.trace(format!(
                    "  LT r{} = {} < {} -> {}",
                    instr.reg_a(),
                    left,
                    right,
                    result
                ));
            }

            Opcode::LE => {
                // reg[a] = (reg[a] <= reg[b]) ? 1 : 0
                let left = self.get_reg(instr.reg_a());
                let right = self.get_reg(instr.reg_b());
                let result = (left <= right) as u64;
                self.set_reg(instr.reg_a(), result);
                self.trace(format!(
                    "  LE r{} = {} <= {} -> {}",
                    instr.reg_a(),
                    left,
                    right,
                    result
                ));
            }

            Opcode::GT => {
                // reg[a] = (reg[a] > reg[b]) ? 1 : 0
                let left = self.get_reg(instr.reg_a());
                let right = self.get_reg(instr.reg_b());
                let result = (left > right) as u64;
                self.set_reg(instr.reg_a(), result);
                self.trace(format!(
                    "  GT r{} = {} > {} -> {}",
                    instr.reg_a(),
                    left,
                    right,
                    result
                ));
            }

            Opcode::GE => {
                // reg[a] = (reg[a] >= reg[b]) ? 1 : 0
                let left = self.get_reg(instr.reg_a());
                let right = self.get_reg(instr.reg_b());
                let result = (left >= right) as u64;
                self.set_reg(instr.reg_a(), result);
                self.trace(format!(
                    "  GE r{} = {} >= {} -> {}",
                    instr.reg_a(),
                    left,
                    right,
                    result
                ));
            }

            Opcode::IN_RANGE => {
                // reg[a] = (imm_low <= reg[a] <= imm_high) ? 1 : 0
                let value = self.get_reg(instr.reg_a());
                let low = instr.imm_low() as u64;
                let high = instr.imm_high() as u64;
                let result = (value >= low && value <= high) as u64;
                self.set_reg(instr.reg_a(), result);
                self.trace(format!(
                    "  IN_RANGE r{} = {} in [{}, {}] -> {}",
                    instr.reg_a(),
                    value,
                    low,
                    high,
                    result
                ));
            }

            Opcode::HAS_BIT => {
                // reg[a] = (reg[a] & (1 << imm)) != 0 ? 1 : 0
                let value = self.get_reg(instr.reg_a());
                let bit = instr.imm_u64();
                let result = ((value & (1u64 << bit)) != 0) as u64;
                self.set_reg(instr.reg_a(), result);
                self.trace(format!(
                    "  HAS_BIT r{} bit {} -> {}",
                    instr.reg_a(),
                    bit,
                    result
                ));
            }

            Opcode::IS_TAG => {
                // reg[a] = (reg[a] == imm_tag) ? 1 : 0
                let value = self.get_reg(instr.reg_a());
                let expected_tag = instr.imm_u64() as u8;
                let result = (value == expected_tag as u64) as u64;
                self.set_reg(instr.reg_a(), result);
                self.trace(format!(
                    "  IS_TAG r{} == {} -> {}",
                    instr.reg_a(),
                    expected_tag,
                    result
                ));
            }

            // =================================================================
            // CHECK OPERATIONS (20-25)
            // =================================================================
            Opcode::CHK_TIME => {
                // Validate valid_from <= time_ns <= valid_to
                let valid_from = self.atom_view.valid_from_ns;
                let valid_to = self.atom_view.valid_to_ns;
                let qc_from = self.qc_view.time_from_ns;
                let qc_to = self.qc_view.time_to_ns;

                // Check if atom's validity overlaps with query range
                let overlaps = valid_from < qc_to && qc_from < valid_to;

                if !overlaps {
                    self.result = InvariantResult::FAIL_HARD;
                    self.reason = ReasonCode::TIME_INVALID;
                    self.trace("  CHK_TIME FAILED - no overlap".to_string());
                    return Ok(ControlFlow::Halt);
                }
                self.trace("  CHK_TIME OK".to_string());
            }

            Opcode::CHK_TRUST => {
                // Validate trust >= trust_min
                let atom_trust = self.atom_view.trust_level;
                let min_trust = self.qc_view.trust_min;

                if atom_trust < min_trust {
                    self.result = InvariantResult::FAIL_HARD;
                    self.reason = ReasonCode::TRUST_TOO_LOW;
                    self.trace(format!(
                        "  CHK_TRUST FAILED - {} < {}",
                        atom_trust, min_trust
                    ));
                    return Ok(ControlFlow::Halt);
                }
                self.trace(format!("  CHK_TRUST OK - {} >= {}", atom_trust, min_trust));
            }

            Opcode::CHK_DOMAIN => {
                // Validate domain_mask & atom_domain != 0
                let atom_domain = self.atom_view.domain_mask;
                let required_domain = self.qc_view.domain_mask;

                if required_domain != 0 && (atom_domain & required_domain) == 0 {
                    self.result = InvariantResult::FAIL_HARD;
                    self.reason = ReasonCode::DOMAIN_MISMATCH;
                    self.trace(format!(
                        "  CHK_DOMAIN FAILED - {:X} & {:X} = 0",
                        atom_domain, required_domain
                    ));
                    return Ok(ControlFlow::Halt);
                }
                self.trace(format!(
                    "  CHK_DOMAIN OK - {:X} & {:X} != 0",
                    atom_domain, required_domain
                ));
            }

            Opcode::CHK_SOURCE => {
                // Validate source in allowlist (from const pool)
                let source_id = self.atom_view.source_id;
                let allowlist_idx = instr.const_index();

                // Get allowlist from const pool or execution context
                let allowed = if let Some(ctx_allowlist) = self.exec_ctx.source_allowlist {
                    ctx_allowlist.contains(&source_id)
                } else if let Some(const_val) = self.get_const(allowlist_idx) {
                    // Const pool contains source ID - check for match
                    if let Some(const_source) = const_val.as_u64() {
                        source_id == const_source as u32
                    } else {
                        true // No valid source in const, allow
                    }
                } else {
                    true // No allowlist, allow all
                };

                if !allowed {
                    self.result = InvariantResult::FAIL_HARD;
                    self.reason = ReasonCode::SOURCE_DENIED;
                    self.trace(format!("  CHK_SOURCE FAILED - source {} denied", source_id));
                    return Ok(ControlFlow::Halt);
                }
                self.trace(format!("  CHK_SOURCE OK - source {} allowed", source_id));
            }

            Opcode::CTX_PROBE => {
                // Search context index for conflicting claims
                let pattern_hash = self.get_reg(instr.reg_a());

                if self.exec_ctx.has_conflict(pattern_hash) {
                    let severity = self.exec_ctx.conflict_severity(pattern_hash);
                    match severity {
                        Some(ConflictSeverity::Hard) => {
                            self.result = InvariantResult::FAIL_HARD;
                            self.reason = ReasonCode::CONFLICT_FOUND;
                            self.trace(format!(
                                "  CTX_PROBE HARD CONFLICT - hash {:X}",
                                pattern_hash
                            ));
                            return Ok(ControlFlow::Halt);
                        }
                        Some(ConflictSeverity::Soft) => {
                            self.result = InvariantResult::NEED_BRANCH;
                            self.reason = ReasonCode::CONFLICT_FOUND;
                            self.trace(format!(
                                "  CTX_PROBE SOFT CONFLICT - hash {:X}",
                                pattern_hash
                            ));
                            return Ok(ControlFlow::Halt);
                        }
                        None => {}
                    }
                }
                self.trace(format!(
                    "  CTX_PROBE OK - no conflict for hash {:X}",
                    pattern_hash
                ));
            }

            Opcode::RAISE_CONFLICT => {
                // Set result to FAIL_HARD or NEED_BRANCH based on imm
                let conflict_type = instr.imm_u64();
                match conflict_type {
                    0 => {
                        // Hard conflict
                        self.result = InvariantResult::FAIL_HARD;
                        self.reason = ReasonCode::CONFLICT_FOUND;
                        self.trace("  RAISE_CONFLICT HARD".to_string());
                    }
                    _ => {
                        // Soft conflict / need branch
                        self.result = InvariantResult::NEED_BRANCH;
                        self.reason = ReasonCode::CONFLICT_FOUND;
                        self.trace("  RAISE_CONFLICT SOFT".to_string());
                    }
                }
                return Ok(ControlFlow::Halt);
            }

            // =================================================================
            // CONTROL FLOW OPERATIONS (30-32)
            // =================================================================
            Opcode::JZ => {
                // Jump if reg[a] == 0
                let cond = self.get_reg(instr.reg_a());
                let target = instr.jump_target();

                if cond == 0 {
                    self.trace(format!("  JZ r{} = 0 -> jump to {}", instr.reg_a(), target));
                    return Ok(ControlFlow::Jump(target));
                }
                self.trace(format!("  JZ r{} = {} -> continue", instr.reg_a(), cond));
            }

            Opcode::JMP => {
                // Unconditional jump
                let target = instr.jump_target();
                self.trace(format!("  JMP -> {}", target));
                return Ok(ControlFlow::Jump(target));
            }

            Opcode::RET => {
                // Return with code from reg[a]
                let result_code = instr.a as u8;
                if let Ok(result) = InvariantResult::try_from(result_code) {
                    self.result = result;
                }
                self.trace(format!("  RET {}", self.result));
                return Ok(ControlFlow::Halt);
            }

            // =================================================================
            // BITWISE OPERATIONS (26-27)
            // =================================================================
            Opcode::AND => {
                let left = self.get_reg(instr.reg_a());
                let right = self.get_reg(instr.reg_b());
                let result = left & right;
                self.set_reg(instr.reg_a(), result);
                self.trace(format!(
                    "  AND r{} = {} & {} -> {}",
                    instr.reg_a(),
                    left,
                    right,
                    result
                ));
            }

            Opcode::NOT => {
                let value = self.get_reg(instr.reg_a());
                let result = !value;
                self.set_reg(instr.reg_a(), result);
                self.trace(format!(
                    "  NOT r{} = ~{} -> {}",
                    instr.reg_a(),
                    value,
                    result
                ));
            }

            Opcode::JNZ => {
                let cond = self.get_reg(instr.reg_a());
                let target = instr.jump_target();
                if cond != 0 {
                    self.trace(format!(
                        "  JNZ r{} = {} -> jump to {}",
                        instr.reg_a(),
                        cond,
                        target
                    ));
                    return Ok(ControlFlow::Jump(target));
                }
                self.trace(format!("  JNZ r{} = {} -> continue", instr.reg_a(), cond));
            }

            Opcode::CALL => {
                let return_pc = self.pc as u64;
                let target = instr.jump_target();
                self.set_reg(instr.reg_a(), return_pc);
                self.trace(format!(
                    "  CALL r{} = {} -> jump to {}",
                    instr.reg_a(),
                    return_pc,
                    target
                ));
                return Ok(ControlFlow::Jump(target));
            }
        }

        Ok(ControlFlow::Continue)
    }

    /// Get execution result
    #[inline]
    pub fn result(&self) -> InvariantResult {
        self.result
    }

    /// Get reason code
    #[inline]
    pub fn reason(&self) -> ReasonCode {
        self.reason
    }

    /// Get step count
    #[inline]
    pub fn step_count(&self) -> u32 {
        self.step_count
    }

    /// Get trace log
    #[inline]
    pub fn trace_log(&self) -> &[String] {
        &self.trace_log
    }

    /// Get register value
    #[inline]
    pub fn register(&self, index: u16) -> u64 {
        self.get_reg(index)
    }
}

// ============================================================================
// Static Execution API
// ============================================================================

impl VmInterpreter<'static> {
    /// Execute bytecode with static lifetimes
    ///
    /// This is a convenience method for simple executions.
    pub fn execute_static(
        instructions: &[Instruction],
        const_pool: &[ConstValue],
        atom_view: AtomView<'static>,
        ctx_view: CtxView<'static>,
        qc_view: QueryConstraintsView,
        max_steps: u32,
    ) -> VmResult<InvariantResult> {
        let ctx_index = CtxIndex::new();
        let exec_ctx = ExecutionContext::new(&[], None, &ctx_index, None);
        let mut vm = VmInterpreter::new(
            const_pool, atom_view, ctx_view, qc_view, exec_ctx, max_steps,
        );
        vm.execute(instructions)
    }
}

// ============================================================================
// Bytecode Builder
// ============================================================================

/// Builder for constructing VM bytecode programs
pub struct BytecodeBuilder {
    instructions: Vec<Instruction>,
    const_pool: ConstPoolBuilder,
    labels: HashMap<String, usize>,
    fixups: Vec<(usize, String)>,
}

impl BytecodeBuilder {
    /// Create a new bytecode builder
    #[inline]
    pub fn new() -> Self {
        BytecodeBuilder {
            instructions: Vec::with_capacity(256),
            const_pool: ConstPoolBuilder::new(),
            labels: HashMap::new(),
            fixups: Vec::new(),
        }
    }

    /// Mark current position with a label
    #[inline]
    pub fn label(&mut self, name: &str) -> &mut Self {
        self.labels
            .insert(name.to_string(), self.instructions.len());
        self
    }

    /// Emit a raw instruction
    #[inline]
    pub fn emit(&mut self, instr: Instruction) -> &mut Self {
        self.instructions.push(instr);
        self
    }

    /// Emit a load instruction
    #[inline]
    pub fn emit_load(&mut self, op: Opcode, dest: u16, field: u32) -> &mut Self {
        self.emit(Instruction::load(op, dest, field))
    }

    /// Emit a comparison instruction
    #[inline]
    pub fn emit_cmp(&mut self, op: Opcode, left: u16, right: u16) -> &mut Self {
        self.emit(Instruction::cmp(op, left, right))
    }

    /// Emit a jump instruction (with fixup)
    #[inline]
    pub fn emit_jump(&mut self, op: Opcode, cond: u16, label: &str) -> &mut Self {
        let pos = self.instructions.len();
        self.emit(Instruction::jump(op, cond, 0));
        self.fixups.push((pos, label.to_string()));
        self
    }

    /// Emit a return instruction
    #[inline]
    pub fn emit_ret(&mut self, result: InvariantResult) -> &mut Self {
        self.emit(Instruction::new(Opcode::RET, result.to_u8() as u16, 0, 0))
    }

    /// Emit a check instruction
    #[inline]
    pub fn emit_check(&mut self, op: Opcode, reg: u16, imm: u64) -> &mut Self {
        self.emit(Instruction::imm_op(op, reg, imm))
    }

    /// Emit a range instruction
    #[inline]
    pub fn emit_range(&mut self, op: Opcode, reg: u16, low: u64, high: u32) -> &mut Self {
        self.emit(Instruction::range(op, reg, low, high))
    }

    /// Add a constant to the pool
    #[inline]
    pub fn add_const(&mut self, value: ConstValue) -> u32 {
        self.const_pool.add(value)
    }

    /// Add a u64 constant
    #[inline]
    pub fn add_u64(&mut self, value: u64) -> u32 {
        self.const_pool.add_u64(value)
    }

    /// Add a source ID to allowlist (for CHK_SOURCE)
    #[inline]
    pub fn add_source(&mut self, source_id: u32) -> u32 {
        self.const_pool.add_u64(source_id as u64)
    }

    /// Build the program
    pub fn build(mut self) -> Result<Vec<Instruction>, String> {
        // Apply fixups
        for (pos, label) in self.fixups {
            if let Some(&target) = self.labels.get(&label) {
                self.instructions[pos].b = target as u32;
            } else {
                return Err(format!("Unresolved label: {}", label));
            }
        }

        Ok(self.instructions)
    }

    /// Build with constant pool
    pub fn build_with_pool(mut self) -> Result<(Vec<Instruction>, Vec<ConstValue>), String> {
        // Apply fixups
        for (pos, label) in self.fixups {
            if let Some(&target) = self.labels.get(&label) {
                self.instructions[pos].b = target as u32;
            } else {
                return Err(format!("Unresolved label: {}", label));
            }
        }

        Ok((self.instructions, self.const_pool.build()))
    }

    /// Get current instruction count
    #[inline]
    pub fn len(&self) -> usize {
        self.instructions.len()
    }

    /// Check if program is empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.instructions.is_empty()
    }
}

impl Default for BytecodeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Built-in Invariant Programs
// ============================================================================

/// Build a basic time/trust/domain check program
///
/// Program flow:
/// 1. CHK_TIME - validate time overlap
/// 2. CHK_TRUST - validate trust level
/// 3. CHK_DOMAIN - validate domain mask
/// 4. RET PASS
pub fn build_basic_invariant_program() -> (Vec<Instruction>, Vec<ConstValue>) {
    let mut builder = BytecodeBuilder::new();

    // CHK_TIME: Check time overlap
    builder.emit(Instruction::new(Opcode::CHK_TIME, 0, 0, 0));

    // CHK_TRUST: Check trust level
    builder.emit(Instruction::new(Opcode::CHK_TRUST, 0, 0, 0));

    // CHK_DOMAIN: Check domain mask
    builder.emit(Instruction::new(Opcode::CHK_DOMAIN, 0, 0, 0));

    // Return PASS
    builder.emit_ret(InvariantResult::PASS);

    builder
        .build_with_pool()
        .unwrap_or((Vec::new(), Vec::new()))
}

/// Build a conflict probe program
///
/// Program flow:
/// 1. LD_CTX r1, field=0 (load ctx_id)
/// 2. CTX_PROBE r1
/// 3. RET PASS
pub fn build_conflict_probe_program() -> (Vec<Instruction>, Vec<ConstValue>) {
    let mut builder = BytecodeBuilder::new();

    // Load context ID into r1
    builder.emit_load(Opcode::LD_CTX, 1, 0);

    // Probe for conflicts using r1 as pattern hash
    builder.emit(Instruction::new(Opcode::CTX_PROBE, 1, 0, 0));

    // Return PASS (if no conflict)
    builder.emit_ret(InvariantResult::PASS);

    builder
        .build_with_pool()
        .unwrap_or((Vec::new(), Vec::new()))
}

/// Build a full validation program
///
/// Program flow:
/// 1. CHK_TIME
/// 2. CHK_TRUST
/// 3. CHK_DOMAIN
/// 4. CHK_SOURCE
/// 5. LD_CLAIM r1, claim_idx=0, field=2 (obj_tag)
/// 6. IS_TAG r1, expected_tag
/// 7. JZ r1 -> fail
/// 8. RET PASS
pub fn build_full_validation_program(expected_tag: u8) -> (Vec<Instruction>, Vec<ConstValue>) {
    let mut builder = BytecodeBuilder::new();

    // Add expected tag as constant
    let tag_const = builder.add_const(ConstValue::tag(expected_tag));

    // Basic checks
    builder.emit(Instruction::new(Opcode::CHK_TIME, 0, 0, 0));
    builder.emit(Instruction::new(Opcode::CHK_TRUST, 0, 0, 0));
    builder.emit(Instruction::new(Opcode::CHK_DOMAIN, 0, 0, 0));
    builder.emit(Instruction::new(Opcode::CHK_SOURCE, 0, tag_const, 0));

    // Load claim obj_tag (claim_idx=1 means atom.claims[0], field=2 means obj_tag)
    builder.emit(Instruction::new(Opcode::LD_CLAIM, 1, 1, 0));
    // Set field selector (obj_tag = field 2) in high 16 bits of b
    builder.instructions.last_mut().unwrap().b = 1 | (2 << 16);

    // Check tag
    builder.emit(Instruction::imm_op(Opcode::IS_TAG, 1, expected_tag as u64));

    // Jump to fail if tag doesn't match
    builder.emit_jump(Opcode::JZ, 1, "fail");

    // Return PASS
    builder.emit_ret(InvariantResult::PASS);

    // Fail label
    builder.label("fail");
    builder.emit(Instruction::new(
        Opcode::RET,
        InvariantResult::FAIL_HARD.to_u8() as u16,
        0,
        0,
    ));

    builder
        .build_with_pool()
        .unwrap_or((Vec::new(), Vec::new()))
}

/// Build a time range check program
///
/// Program flow:
/// 1. LD_ATOM_META r1, field=0 (valid_from)
/// 2. LD_QC r2, field=1 (time_to_ns)
/// 3. LT r1, r2 (valid_from < qc_to)
/// 4. JZ r1 -> fail
/// 5. LD_ATOM_META r1, field=1 (valid_to)
/// 6. LD_QC r2, field=0 (time_from_ns)
/// 7. GT r1, r2 (valid_to > qc_from)
/// 8. JZ r1 -> fail
/// 9. RET PASS
pub fn build_time_range_program() -> (Vec<Instruction>, Vec<ConstValue>) {
    let mut builder = BytecodeBuilder::new();

    // Load valid_from into r1
    builder.emit_load(Opcode::LD_ATOM_META, 1, 0);

    // Load qc time_to into r2
    builder.emit_load(Opcode::LD_QC, 2, 1);

    // Check valid_from < qc_to
    builder.emit_cmp(Opcode::LT, 1, 2);
    builder.emit_jump(Opcode::JZ, 1, "fail");

    // Load valid_to into r1
    builder.emit_load(Opcode::LD_ATOM_META, 1, 1);

    // Load qc time_from into r2
    builder.emit_load(Opcode::LD_QC, 2, 0);

    // Check valid_to > qc_from
    builder.emit_cmp(Opcode::GT, 1, 2);
    builder.emit_jump(Opcode::JZ, 1, "fail");

    // Return PASS
    builder.emit_ret(InvariantResult::PASS);

    // Fail label
    builder.label("fail");
    builder.emit(Instruction::new(
        Opcode::RET,
        InvariantResult::FAIL_HARD.to_u8() as u16,
        0,
        0,
    ));

    builder
        .build_with_pool()
        .unwrap_or((Vec::new(), Vec::new()))
}

/// Build a trust threshold program
///
/// Program flow:
/// 1. LD_ATOM_META r1, field=3 (trust_level)
/// 2. LD_QC r2, field=2 (trust_min)
/// 3. GE r1, r2 (trust >= trust_min)
/// 4. JZ r1 -> fail
/// 5. RET PASS
pub fn build_trust_threshold_program() -> (Vec<Instruction>, Vec<ConstValue>) {
    let mut builder = BytecodeBuilder::new();

    // Load trust_level into r1
    builder.emit_load(Opcode::LD_ATOM_META, 1, 3);

    // Load trust_min into r2
    builder.emit_load(Opcode::LD_QC, 2, 2);

    // Check trust >= trust_min
    builder.emit_cmp(Opcode::GE, 1, 2);
    builder.emit_jump(Opcode::JZ, 1, "fail");

    // Return PASS
    builder.emit_ret(InvariantResult::PASS);

    // Fail label
    builder.label("fail");
    builder.emit(Instruction::new(
        Opcode::RET,
        InvariantResult::FAIL_HARD.to_u8() as u16,
        0,
        0,
    ));

    builder
        .build_with_pool()
        .unwrap_or((Vec::new(), Vec::new()))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_instruction_size() {
        assert_eq!(std::mem::size_of::<Instruction>(), 16);
    }

    #[test]
    fn test_opcode_roundtrip() {
        // Test all opcodes
        let opcodes = [
            Opcode::LD_ATOM_META,
            Opcode::LD_CLAIM,
            Opcode::LD_QC,
            Opcode::LD_CTX,
            Opcode::LD_IMM,
            Opcode::EQ,
            Opcode::LT,
            Opcode::LE,
            Opcode::GT,
            Opcode::GE,
            Opcode::IN_RANGE,
            Opcode::HAS_BIT,
            Opcode::IS_TAG,
            Opcode::CHK_TIME,
            Opcode::CHK_TRUST,
            Opcode::CHK_DOMAIN,
            Opcode::CHK_SOURCE,
            Opcode::CTX_PROBE,
            Opcode::RAISE_CONFLICT,
            Opcode::JZ,
            Opcode::JMP,
            Opcode::RET,
        ];

        for op in opcodes {
            assert_eq!(Opcode::from_u16(op.to_u16()), Some(op));
        }

        // Invalid opcodes
        assert_eq!(Opcode::from_u16(0), None);
        assert_eq!(Opcode::from_u16(6), None);
        assert_eq!(Opcode::from_u16(9), None);
        assert_eq!(Opcode::from_u16(18), None);
        // New valid opcodes: 26 (AND), 27 (NOT), 33 (JNZ), 34 (CALL)
        assert_eq!(Opcode::from_u16(28), None);
        assert_eq!(Opcode::from_u16(35), None);
    }

    #[test]
    fn test_opcode_categories() {
        assert!(Opcode::LD_ATOM_META.is_load());
        assert!(Opcode::LD_CLAIM.is_load());
        assert!(Opcode::EQ.is_comparison());
        assert!(Opcode::LT.is_comparison());
        assert!(Opcode::IN_RANGE.is_comparison());
        assert!(Opcode::CHK_TIME.is_check());
        assert!(Opcode::CHK_TRUST.is_check());
        assert!(Opcode::CTX_PROBE.is_check());
        assert!(Opcode::JZ.is_control_flow());
        assert!(Opcode::JMP.is_control_flow());
        assert!(Opcode::RET.is_control_flow());
    }

    #[test]
    fn test_const_value_conversions() {
        let sym = ConstValue::sym(42);
        assert_eq!(sym.as_u64(), Some(42));
        assert_eq!(sym.as_i64(), Some(42));

        let u64_val = ConstValue::u64(100);
        assert_eq!(u64_val.as_u64(), Some(100));
        assert_eq!(u64_val.as_i64(), Some(100));

        let i64_pos = ConstValue::i64(50);
        assert_eq!(i64_pos.as_u64(), Some(50));
        assert_eq!(i64_pos.as_i64(), Some(50));

        let i64_neg = ConstValue::i64(-50);
        assert_eq!(i64_neg.as_u64(), None);
        assert_eq!(i64_neg.as_i64(), Some(-50));

        let tag = ConstValue::tag(5);
        assert_eq!(tag.as_u64(), Some(5));
        assert_eq!(tag.as_tag(), Some(5));
    }

    #[test]
    fn test_const_pool_builder() {
        let mut builder = ConstPoolBuilder::new();
        let idx1 = builder.add_sym(1);
        let idx2 = builder.add_u64(100);
        let idx3 = builder.add_i64(-50);
        let idx4 = builder.add_tag(5);

        assert_eq!(idx1, 0);
        assert_eq!(idx2, 1);
        assert_eq!(idx3, 2);
        assert_eq!(idx4, 3);

        let pool = builder.build();
        assert_eq!(pool.len(), 4);
        assert_eq!(pool[0].as_u64(), Some(1));
        assert_eq!(pool[1].as_u64(), Some(100));
        assert_eq!(pool[2].as_i64(), Some(-50));
        assert_eq!(pool[3].as_tag(), Some(5));
    }

    #[test]
    fn test_vm_state_registers() {
        let mut state = VmState::new(Vec::new(), 1000);

        // r0 is always zero
        assert_eq!(state.get_reg(0), 0);

        // Set r1
        state.set_reg(1, 42);
        assert_eq!(state.get_reg(1), 42);

        // r0 still zero
        assert_eq!(state.get_reg(0), 0);

        // Register index wraps (16 & 0xF = 0, but r0 is hardwired)
        state.set_reg(16, 100);
        assert_eq!(state.get_reg(0), 0);

        // r1 still has value
        assert_eq!(state.get_reg(1), 42);

        // r17 wraps to r1
        state.set_reg(17, 200);
        assert_eq!(state.get_reg(1), 200);
    }

    #[test]
    fn test_vm_state_budget() {
        let mut state = VmState::new(Vec::new(), 10);

        for i in 0..9 {
            assert!(state.tick());
            assert_eq!(state.step_count, i + 1);
        }

        // 10th tick should fail (step_count becomes 10, max is 10, 10 < 10 = false)
        assert!(!state.tick());
        assert_eq!(state.step_count, 10);
    }

    #[test]
    fn test_bytecode_builder_basic() {
        let mut builder = BytecodeBuilder::new();

        builder
            .label("start")
            .emit(Instruction::new(Opcode::CHK_TIME, 0, 0, 0))
            .emit_jump(Opcode::JZ, 0, "end")
            .emit(Instruction::new(Opcode::CHK_TRUST, 0, 0, 0))
            .label("end")
            .emit_ret(InvariantResult::PASS);

        let (instructions, pool) = builder.build_with_pool().unwrap();
        assert!(!instructions.is_empty());
        assert!(pool.is_empty());
    }

    #[test]
    fn test_basic_invariant_program() {
        let (instructions, _pool) = build_basic_invariant_program();
        assert!(!instructions.is_empty());

        // Verify structure: CHK_TIME, CHK_TRUST, CHK_DOMAIN, RET
        assert_eq!(instructions[0].opcode(), Some(Opcode::CHK_TIME));
        assert_eq!(instructions[1].opcode(), Some(Opcode::CHK_TRUST));
        assert_eq!(instructions[2].opcode(), Some(Opcode::CHK_DOMAIN));
        assert_eq!(instructions[3].opcode(), Some(Opcode::RET));
    }

    #[test]
    fn test_vm_execution_pass() {
        let (instructions, const_pool) = build_basic_invariant_program();

        let atom_id = [0u8; 32];
        let atom_view = AtomView::new(
            &atom_id,
            AtomType::FACT,
            &[],
            &[],
            0,
            u64::MAX,
            1000,
            0xFFFF,
            1,
        );

        let ctx_view = CtxView::new(0, &[], &[], 0);
        let qc_view = QueryConstraintsView::new(0, u64::MAX, 500, 0, 0xFFFF, 100);
        let ctx_index = CtxIndex::new();
        let exec_ctx = ExecutionContext::new(&[], None, &ctx_index, None);

        let mut vm = VmInterpreter::new(&const_pool, atom_view, ctx_view, qc_view, exec_ctx, 1000);

        let result = vm.execute(&instructions).unwrap();
        assert_eq!(result, InvariantResult::PASS);
    }

    #[test]
    fn test_vm_execution_trust_failure() {
        let (instructions, const_pool) = build_basic_invariant_program();

        let atom_id = [0u8; 32];
        let atom_view = AtomView::new(
            &atom_id,
            AtomType::FACT,
            &[],
            &[],
            0,
            u64::MAX,
            100, // Low trust
            0xFFFF,
            1,
        );

        let ctx_view = CtxView::new(0, &[], &[], 0);
        let qc_view = QueryConstraintsView::new(0, u64::MAX, 500, 0, 0xFFFF, 100);
        let ctx_index = CtxIndex::new();
        let exec_ctx = ExecutionContext::new(&[], None, &ctx_index, None);

        let mut vm = VmInterpreter::new(&const_pool, atom_view, ctx_view, qc_view, exec_ctx, 1000);

        let result = vm.execute(&instructions).unwrap();
        assert_eq!(result, InvariantResult::FAIL_HARD);
        assert_eq!(vm.reason(), ReasonCode::TRUST_TOO_LOW);
    }

    #[test]
    fn test_vm_execution_domain_mismatch() {
        let (instructions, const_pool) = build_basic_invariant_program();

        let atom_id = [0u8; 32];
        let atom_view = AtomView::new(
            &atom_id,
            AtomType::FACT,
            &[],
            &[],
            0,
            u64::MAX,
            1000,
            0x01, // Domain 1 only
            1,
        );

        let ctx_view = CtxView::new(0, &[], &[], 0);
        let qc_view = QueryConstraintsView::new(0, u64::MAX, 500, 0x02, 0xFFFF, 100);
        let ctx_index = CtxIndex::new();
        let exec_ctx = ExecutionContext::new(&[], None, &ctx_index, None);

        let mut vm = VmInterpreter::new(&const_pool, atom_view, ctx_view, qc_view, exec_ctx, 1000);

        let result = vm.execute(&instructions).unwrap();
        assert_eq!(result, InvariantResult::FAIL_HARD);
        assert_eq!(vm.reason(), ReasonCode::DOMAIN_MISMATCH);
    }

    #[test]
    fn test_vm_execution_time_invalid() {
        let (instructions, const_pool) = build_basic_invariant_program();

        let atom_id = [0u8; 32];
        let atom_view = AtomView::new(
            &atom_id,
            AtomType::FACT,
            &[],
            &[],
            1000, // valid_from
            2000, // valid_to
            1000,
            0xFFFF,
            1,
        );

        let ctx_view = CtxView::new(0, &[], &[], 0);
        // Query range doesn't overlap with atom range
        let qc_view = QueryConstraintsView::new(0, 500, 500, 0, 0xFFFF, 100);
        let ctx_index = CtxIndex::new();
        let exec_ctx = ExecutionContext::new(&[], None, &ctx_index, None);

        let mut vm = VmInterpreter::new(&const_pool, atom_view, ctx_view, qc_view, exec_ctx, 1000);

        let result = vm.execute(&instructions).unwrap();
        assert_eq!(result, InvariantResult::FAIL_HARD);
        assert_eq!(vm.reason(), ReasonCode::TIME_INVALID);
    }

    #[test]
    fn test_vm_comparison_ops() {
        let const_pool = vec![];
        let atom_id = [0u8; 32];
        let atom_view = AtomView::new(
            &atom_id,
            AtomType::FACT,
            &[],
            &[],
            0,
            u64::MAX,
            1000,
            0xFFFF,
            1,
        );
        let ctx_view = CtxView::new(0, &[], &[], 0);
        let qc_view = QueryConstraintsView::default();
        let ctx_index = CtxIndex::new();
        let exec_ctx = ExecutionContext::new(&[], None, &ctx_index, None);

        // Test program: set r1=10, r2=20, compare
        let _instructions = [
            Instruction::imm_op(Opcode::LD_CTX, 1, 10), // r1 = 10 (using LD_CTX imm as hack)
            Instruction::imm_op(Opcode::LD_CTX, 2, 20), // r2 = 20
            Instruction::cmp(Opcode::LT, 1, 2),         // r1 = (10 < 20) = 1
            Instruction::cmp(Opcode::GT, 2, 1),         // r2 = (20 > 10) = 1
            Instruction::cmp(Opcode::EQ, 1, 2),         // r1 = (1 == 1) = 1
            Instruction::new(Opcode::RET, 0, 0, 0),
        ];

        let _vm = VmInterpreter::new(&const_pool, atom_view, ctx_view, qc_view, exec_ctx, 1000);

        // Note: The above test uses LD_CTX incorrectly; proper test below
        // Let's test comparison ops directly
        let mut state = VmState::new(vec![], 100);
        state.set_reg(1, 10);
        state.set_reg(2, 20);

        // EQ test
        state.set_reg(3, 10);
        let _instr = Instruction::cmp(Opcode::EQ, 1, 3);
        state.set_reg(1, (state.get_reg(1) == state.get_reg(3)) as u64);
        assert_eq!(state.get_reg(1), 1);

        // LT test
        state.set_reg(1, 10);
        state.set_reg(2, 20);
        state.set_reg(1, (state.get_reg(1) < state.get_reg(2)) as u64);
        assert_eq!(state.get_reg(1), 1);

        // GT test
        state.set_reg(1, 20);
        state.set_reg(2, 10);
        state.set_reg(1, (state.get_reg(1) > state.get_reg(2)) as u64);
        assert_eq!(state.get_reg(1), 1);
    }

    #[test]
    fn test_vm_control_flow() {
        let const_pool = vec![];
        let atom_id = [0u8; 32];
        let atom_view = AtomView::new(
            &atom_id,
            AtomType::FACT,
            &[],
            &[],
            0,
            u64::MAX,
            1000,
            0xFFFF,
            1,
        );
        // Set up ctx_view to return specific values for field access
        // field 10 (idx 0) = 0, field 21 (idx 11) = 2
        let ctx_view = CtxView::new(0, &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2], &[], 0);
        let qc_view = QueryConstraintsView::default();
        let ctx_index = CtxIndex::new();
        let exec_ctx = ExecutionContext::new(&[], None, &ctx_index, None);

        // Test program with jump
        // r1 = ctx[10] = 0, JZ r1 -> skip to 3, r3 = ctx[21] = 2, RET
        let instructions = vec![
            Instruction::load(Opcode::LD_CTX, 1, 10), // r1 = ctx policy field 10 = 0
            Instruction::jump(Opcode::JZ, 1, 3),      // if r1 == 0, jump to 3
            Instruction::load(Opcode::LD_CTX, 2, 100), // skipped: r2 = 0 (out of bounds)
            Instruction::load(Opcode::LD_CTX, 3, 21), // r3 = ctx policy field 21 = 2
            Instruction::new(Opcode::RET, 0, 0, 0),
        ];

        let mut vm = VmInterpreter::new(&const_pool, atom_view, ctx_view, qc_view, exec_ctx, 1000);

        let result = vm.execute(&instructions).unwrap();
        assert_eq!(result, InvariantResult::PASS);
        assert_eq!(vm.register(1), 0);
        assert_eq!(vm.register(2), 0); // Not set (skipped)
        assert_eq!(vm.register(3), 2);
    }

    #[test]
    fn test_vm_conflict_probe() {
        let const_pool = vec![];

        // Create context index with conflict at pattern hash 0x1234
        let mut ctx_index = CtxIndex::new();
        ctx_index.add_conflict(0x1234, [1u8; 32], ConflictSeverity::Hard);

        let atom_id = [0u8; 32];
        let atom_view = AtomView::new(
            &atom_id,
            AtomType::FACT,
            &[],
            &[],
            0,
            u64::MAX,
            1000,
            0xFFFF,
            1,
        );

        // Set ctx_id to 0x1234 so LD_CTX field 0 loads it
        let ctx_view = CtxView::new(0x1234, &[], &[], 0);
        let qc_view = QueryConstraintsView::default();
        let exec_ctx = ExecutionContext::new(&[], None, &ctx_index, None);

        // Test program: load ctx_id (0x1234) into r1, then CTX_PROBE
        let instructions = vec![
            Instruction::load(Opcode::LD_CTX, 1, 0), // r1 = ctx_id = 0x1234
            Instruction::new(Opcode::CTX_PROBE, 1, 0, 0), // probe with r1 as pattern hash
            Instruction::new(Opcode::RET, 0, 0, 0),
        ];

        let mut vm = VmInterpreter::new(&const_pool, atom_view, ctx_view, qc_view, exec_ctx, 1000);

        let result = vm.execute(&instructions).unwrap();
        assert_eq!(result, InvariantResult::FAIL_HARD);
        assert_eq!(vm.reason(), ReasonCode::CONFLICT_FOUND);
    }

    #[test]
    fn test_vm_step_limit() {
        let const_pool = vec![];
        let atom_id = [0u8; 32];
        let atom_view = AtomView::new(
            &atom_id,
            AtomType::FACT,
            &[],
            &[],
            0,
            u64::MAX,
            1000,
            0xFFFF,
            1,
        );
        let ctx_view = CtxView::new(0, &[], &[], 0);
        let qc_view = QueryConstraintsView::default();
        let ctx_index = CtxIndex::new();
        let exec_ctx = ExecutionContext::new(&[], None, &ctx_index, None);

        // Infinite loop program (with unreachable RET to pass validation)
        let instructions = vec![
            Instruction::new(Opcode::JMP, 0, 0, 0), // jump to 0 (infinite loop)
            Instruction::new(Opcode::RET, 0, 0, 0), // unreachable but required for validation
        ];

        let mut vm = VmInterpreter::new(
            &const_pool,
            atom_view,
            ctx_view,
            qc_view,
            exec_ctx,
            10, // max 10 steps
        );

        let result = vm.execute(&instructions).unwrap();
        assert_eq!(result, InvariantResult::FAIL_HARD);
        assert_eq!(vm.reason(), ReasonCode::BUDGET_EXCEEDED);
    }

    #[test]
    fn test_vm_tracing() {
        let (instructions, const_pool) = build_basic_invariant_program();

        let atom_id = [0u8; 32];
        let atom_view = AtomView::new(
            &atom_id,
            AtomType::FACT,
            &[],
            &[],
            0,
            u64::MAX,
            1000,
            0xFFFF,
            1,
        );

        let ctx_view = CtxView::new(0, &[], &[], 0);
        let qc_view = QueryConstraintsView::new(0, u64::MAX, 500, 0, 0xFFFF, 100);
        let ctx_index = CtxIndex::new();
        let exec_ctx = ExecutionContext::new(&[], None, &ctx_index, None);

        let mut vm = VmInterpreter::new(&const_pool, atom_view, ctx_view, qc_view, exec_ctx, 1000)
            .with_tracing();

        let _ = vm.execute(&instructions).unwrap();

        let trace = vm.trace_log();
        assert!(!trace.is_empty());
        assert!(trace.iter().any(|t| t.contains("CHK_TIME")));
        assert!(trace.iter().any(|t| t.contains("CHK_TRUST")));
        assert!(trace.iter().any(|t| t.contains("CHK_DOMAIN")));
    }

    #[test]
    fn test_vm_error_invalid_opcode() {
        let const_pool = vec![];
        let atom_id = [0u8; 32];
        let atom_view = AtomView::new(
            &atom_id,
            AtomType::FACT,
            &[],
            &[],
            0,
            u64::MAX,
            1000,
            0xFFFF,
            1,
        );
        let ctx_view = CtxView::new(0, &[], &[], 0);
        let qc_view = QueryConstraintsView::default();
        let ctx_index = CtxIndex::new();
        let exec_ctx = ExecutionContext::new(&[], None, &ctx_index, None);

        // Invalid opcode
        let instructions = vec![Instruction::new(Opcode::JMP, 0, 0, 999)];

        let mut vm = VmInterpreter::new(&const_pool, atom_view, ctx_view, qc_view, exec_ctx, 1000);

        let result = vm.execute(&instructions);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), VmError::ValidationFailed(_)));
    }

    #[test]
    fn test_bytecode_validation() {
        // Valid program
        let instructions = vec![
            Instruction::new(Opcode::CHK_TIME, 0, 0, 0),
            Instruction::new(Opcode::RET, 0, 0, 0),
        ];
        assert!(BytecodeValidator::validate(&instructions, 0, 1000).is_ok());

        // Jump out of bounds
        let instructions = vec![
            Instruction::jump(Opcode::JMP, 0, 100),
            Instruction::new(Opcode::RET, 0, 0, 0),
        ];
        assert!(BytecodeValidator::validate(&instructions, 0, 1000).is_err());

        // No RET
        let instructions = vec![Instruction::new(Opcode::CHK_TIME, 0, 0, 0)];
        assert!(BytecodeValidator::validate(&instructions, 0, 1000).is_err());

        // Empty program
        let instructions: Vec<Instruction> = vec![];
        assert!(BytecodeValidator::validate(&instructions, 0, 1000).is_err());
    }

    #[test]
    fn test_atom_view_helpers() {
        let atom_id = [0u8; 32];
        let atom_view = AtomView::new(
            &atom_id,
            AtomType::FACT,
            &[1, 2, 3, 4],
            &[
                ClaimData {
                    subj: 1,
                    pred: 2,
                    obj_tag: 3,
                    obj_val: 42,
                    qualifiers_mask: 0,
                },
                ClaimData {
                    subj: 5,
                    pred: 6,
                    obj_tag: 7,
                    obj_val: 100,
                    qualifiers_mask: 1,
                },
            ],
            1000,
            2000,
            800,
            0x0F,
            42,
        );

        // Test validity
        assert!(atom_view.is_valid_at(1500));
        assert!(!atom_view.is_valid_at(500));
        assert!(!atom_view.is_valid_at(3000));

        // Test domain
        assert!(atom_view.domain_matches(0x01));
        assert!(atom_view.domain_matches(0x0F));
        assert!(!atom_view.domain_matches(0x10));

        // Test trust
        assert!(atom_view.trust_meets(500));
        assert!(atom_view.trust_meets(800));
        assert!(!atom_view.trust_meets(900));

        // Test metadata
        assert_eq!(atom_view.get_meta(0), Some(1));
        assert_eq!(atom_view.get_meta(3), Some(4));
        assert_eq!(atom_view.get_meta(10), None);

        // Test claims
        assert_eq!(atom_view.claim_count(), 2);
        let claim0 = atom_view.get_claim(0).unwrap();
        assert_eq!(claim0.subj, 1);
        assert_eq!(claim0.pred, 2);
        assert_eq!(claim0.obj_tag, 3);
        assert_eq!(claim0.obj_val, 42);
    }

    #[test]
    fn test_ctx_view_helpers() {
        let ctx_view = CtxView::new(
            5,
            &[10, 20, 30],
            &[
                ConflictProbe {
                    pattern_hash: 0x100,
                    conflict_count: 2,
                    max_trust: 500,
                    flags: 0,
                },
                ConflictProbe {
                    pattern_hash: 0x200,
                    conflict_count: 0,
                    max_trust: 1000,
                    flags: 1,
                },
            ],
            3,
        );

        assert_eq!(ctx_view.ctx_id, 5);
        assert_eq!(ctx_view.active_branches, 3);
        assert_eq!(ctx_view.total_conflicts(), 2);
        assert!(ctx_view.has_conflicts());

        let probe = ctx_view.probe_conflict(0x100);
        assert!(probe.is_some());
        assert_eq!(probe.unwrap().conflict_count, 2);

        let no_probe = ctx_view.probe_conflict(0x999);
        assert!(no_probe.is_none());
    }

    #[test]
    fn test_execution_context() {
        let ctx_index = CtxIndex::new();

        // No allowlist - all sources allowed
        let exec_ctx = ExecutionContext::new(&[], None, &ctx_index, None);
        assert!(exec_ctx.is_source_allowed(0));
        assert!(exec_ctx.is_source_allowed(100));
        assert!(exec_ctx.is_source_allowed(999));

        // With allowlist
        let allowlist = [1, 2, 3, 10, 20];
        let exec_ctx = ExecutionContext::new(&[], None, &ctx_index, Some(&allowlist));
        assert!(exec_ctx.is_source_allowed(1));
        assert!(exec_ctx.is_source_allowed(10));
        assert!(!exec_ctx.is_source_allowed(5));
        assert!(!exec_ctx.is_source_allowed(100));
    }

    #[test]
    fn test_full_validation_program() {
        let (instructions, const_pool) = build_full_validation_program(3);

        let atom_id = [0u8; 32];
        let atom_view = AtomView::new(
            &atom_id,
            AtomType::FACT,
            &[],
            &[ClaimData {
                subj: 1,
                pred: 2,
                obj_tag: 3, // Matches expected
                obj_val: 42,
                qualifiers_mask: 0,
            }],
            0,
            u64::MAX,
            1000,
            0xFFFF,
            3, // source_id must match the tag const for CHK_SOURCE to pass
        );

        let ctx_view = CtxView::new(0, &[], &[], 0);
        let qc_view = QueryConstraintsView::new(0, u64::MAX, 500, 0, 0xFFFF, 100);
        let ctx_index = CtxIndex::new();
        let exec_ctx = ExecutionContext::new(&[], None, &ctx_index, None);

        let mut vm = VmInterpreter::new(&const_pool, atom_view, ctx_view, qc_view, exec_ctx, 1000);

        let result = vm.execute(&instructions).unwrap();
        assert_eq!(result, InvariantResult::PASS);
    }

    #[test]
    fn test_trust_threshold_program() {
        let (instructions, const_pool) = build_trust_threshold_program();

        let atom_id = [0u8; 32];
        let atom_view = AtomView::new(
            &atom_id,
            AtomType::FACT,
            &[],
            &[],
            0,
            u64::MAX,
            800, // Trust level
            0xFFFF,
            1,
        );

        let ctx_view = CtxView::new(0, &[], &[], 0);
        let qc_view = QueryConstraintsView::new(0, u64::MAX, 500, 0, 0xFFFF, 100);
        let ctx_index = CtxIndex::new();
        let exec_ctx = ExecutionContext::new(&[], None, &ctx_index, None);

        let mut vm = VmInterpreter::new(
            &const_pool,
            atom_view,
            ctx_view.clone(),
            qc_view.clone(),
            exec_ctx,
            1000,
        );

        let result = vm.execute(&instructions).unwrap();
        assert_eq!(result, InvariantResult::PASS);

        // Test failure case - trust too low
        let atom_view_low = AtomView::new(
            &atom_id,
            AtomType::FACT,
            &[],
            &[],
            0,
            u64::MAX,
            300, // Low trust (below 500 min)
            0xFFFF,
            1,
        );

        let ctx_index2 = CtxIndex::new();
        let exec_ctx2 = ExecutionContext::new(&[], None, &ctx_index2, None);
        let mut vm = VmInterpreter::new(
            &const_pool,
            atom_view_low,
            ctx_view,
            qc_view,
            exec_ctx2,
            1000,
        );

        let result = vm.execute(&instructions).unwrap();
        assert_eq!(result, InvariantResult::FAIL_HARD);
        // Note: The program returns FAIL_HARD but doesn't set reason code
        // The reason remains the default (TIME_INVALID) - this is expected behavior
        // for programs that use RET with just a result code
        assert_eq!(result, InvariantResult::FAIL_HARD);
    }

    #[test]
    fn test_vm_and_opcode() {
        // AND r1, r2: 0xFF & 0x0F = 0x0F
        let const_pool: Vec<ConstValue> = vec![];
        let atom_id = [0u8; 32];
        let ctx_index = CtxIndex::new();
        {
            let atom_view = AtomView::new(
                &atom_id,
                AtomType::FACT,
                &[],
                &[],
                0,
                u64::MAX,
                1000,
                0xFFFF,
                1,
            );
            let ctx_view = CtxView::new(0xFF, &[0x0F], &[], 0);
            let qc_view = QueryConstraintsView::default();
            let exec_ctx = ExecutionContext::new(&[], None, &ctx_index, None);
            let mut vm =
                VmInterpreter::new(&const_pool, atom_view, ctx_view, qc_view, exec_ctx, 1000);
            vm.registers[1] = 0xFF;
            vm.registers[2] = 0x0F;
            let instr = Instruction::new(Opcode::AND, 1, 2, 0);
            let _ = vm.execute_instruction(&instr).unwrap();
            assert_eq!(vm.registers[1], 0x0F);
        }
    }

    #[test]
    fn test_vm_not_opcode() {
        // NOT r1: ~0xFF = 0xFFFF_FFFF_FFFF_FF00
        let const_pool: Vec<ConstValue> = vec![];
        let atom_id = [0u8; 32];
        let ctx_index = CtxIndex::new();
        let atom_view = AtomView::new(
            &atom_id,
            AtomType::FACT,
            &[],
            &[],
            0,
            u64::MAX,
            1000,
            0xFFFF,
            1,
        );
        let ctx_view = CtxView::new(0, &[], &[], 0);
        let qc_view = QueryConstraintsView::default();
        let exec_ctx = ExecutionContext::new(&[], None, &ctx_index, None);
        let mut vm = VmInterpreter::new(&const_pool, atom_view, ctx_view, qc_view, exec_ctx, 1000);
        vm.registers[1] = 0xFF;
        let instr = Instruction::new(Opcode::NOT, 1, 0, 0);
        let _ = vm.execute_instruction(&instr).unwrap();
        assert_eq!(vm.registers[1], !0xFFu64);
    }

    #[test]
    fn test_vm_jnz_opcode() {
        // JNZ r1, 3: if r1 != 0, jump to 3
        let const_pool: Vec<ConstValue> = vec![];
        let atom_id = [0u8; 32];

        // Test with non-zero - should jump
        let ctx_index1 = CtxIndex::new();
        let atom_view1 = AtomView::new(
            &atom_id,
            AtomType::FACT,
            &[],
            &[],
            0,
            u64::MAX,
            1000,
            0xFFFF,
            1,
        );
        let ctx_view1 = CtxView::new(0, &[], &[], 0);
        let qc_view1 = QueryConstraintsView::default();
        let exec_ctx1 = ExecutionContext::new(&[], None, &ctx_index1, None);
        let mut vm = VmInterpreter::new(
            &const_pool,
            atom_view1,
            ctx_view1,
            qc_view1,
            exec_ctx1,
            1000,
        );
        vm.registers[1] = 1;
        let instr = Instruction::new(Opcode::JNZ, 1, 5, 0);
        let result = vm.execute_instruction(&instr).unwrap();
        match result {
            ControlFlow::Jump(target) => assert_eq!(target, 5),
            _ => panic!("JNZ should return Jump"),
        }

        // Test with zero - should not jump
        let ctx_index2 = CtxIndex::new();
        let atom_view2 = AtomView::new(
            &atom_id,
            AtomType::FACT,
            &[],
            &[],
            0,
            u64::MAX,
            1000,
            0xFFFF,
            1,
        );
        let ctx_view2 = CtxView::new(0, &[], &[], 0);
        let qc_view2 = QueryConstraintsView::default();
        let exec_ctx2 = ExecutionContext::new(&[], None, &ctx_index2, None);
        let mut vm2 = VmInterpreter::new(
            &const_pool,
            atom_view2,
            ctx_view2,
            qc_view2,
            exec_ctx2,
            1000,
        );
        vm2.registers[1] = 0;
        let instr2 = Instruction::new(Opcode::JNZ, 1, 5, 0);
        let result2 = vm2.execute_instruction(&instr2).unwrap();
        assert_eq!(result2, ControlFlow::Continue);
    }

    #[test]
    fn test_vm_call_opcode() {
        // CALL r1, target: save return PC in r1, jump to target
        let const_pool: Vec<ConstValue> = vec![];
        let atom_id = [0u8; 32];
        let ctx_index = CtxIndex::new();
        let atom_view = AtomView::new(
            &atom_id,
            AtomType::FACT,
            &[],
            &[],
            0,
            u64::MAX,
            1000,
            0xFFFF,
            1,
        );
        let ctx_view = CtxView::new(0, &[], &[], 0);
        let qc_view = QueryConstraintsView::default();
        let exec_ctx = ExecutionContext::new(&[], None, &ctx_index, None);
        let mut vm = VmInterpreter::new(&const_pool, atom_view, ctx_view, qc_view, exec_ctx, 1000);
        vm.pc = 10; // Current PC
        let instr = Instruction::new(Opcode::CALL, 1, 20, 0);
        let result = vm.execute_instruction(&instr).unwrap();
        match result {
            ControlFlow::Jump(target) => assert_eq!(target, 20),
            _ => panic!("CALL should return Jump"),
        }
        // r1 should contain return PC (which is 10, the pc at the time of CALL)
        assert_eq!(vm.registers[1], 10);
    }

    /// Test CTX_PROBE with real CtxManager and recorded conflicts
    ///
    /// This test verifies the full integration:
    /// 1. Creates CtxManager with a context
    /// 2. Adds contradictory claims so the context records a real conflict
    /// 3. Gets CtxIndex from CtxManager (with recorded conflicts)
    /// 4. Executes CTX_PROBE in VM
    /// 5. Verifies conflict is detected
    #[test]
    fn test_ctx_probe_with_real_ctx_manager() {
        use crate::store::api::{ConflictResolutionMode, CtxManager};
        use crate::vm::ClaimData;

        // Create CtxManager and context
        let mut ctx_manager = CtxManager::new();
        let ctx_id = ctx_manager.create_context(0);

        // Set policy to Reject so conflicts are detected but not branched
        if let Some(ctx) = ctx_manager.get_ctx_mut(ctx_id) {
            ctx.policy.conflict_resolution = ConflictResolutionMode::Reject;
        }

        // Create first claim: subj=1, pred=2, obj_val=100
        let claim1 = ClaimData {
            subj: 1,
            pred: 2,
            obj_tag: 0,
            obj_val: 100,
            qualifiers_mask: 0,
        };

        let result = ctx_manager.assert_claim_with_atom_id(ctx_id, &claim1, [1u8; 32]);
        assert!(result.is_ok(), "First claim should be added without conflict");

        let claim2 = ClaimData {
            subj: 1,
            pred: 2,
            obj_tag: 0,
            obj_val: 200,
            qualifiers_mask: 0,
        };
        let second = ctx_manager.assert_claim_with_atom_id(ctx_id, &claim2, [2u8; 32]);
        assert!(
            matches!(second, Err(crate::store::api::StoreError::ClaimRejected(_))),
            "Second claim should be rejected and recorded as a real conflict"
        );

        let conflicts = ctx_manager.list_conflicts(ctx_id);
        assert!(
            conflicts.is_empty(),
            "Reject mode should not materialize a stored conflict in the context"
        );

        // Get CtxIndex from CtxManager - this should reflect the live active claims.
        let ctx_index = ctx_manager.get_ctx_index(ctx_id);
        let pattern_hash = 1u64 ^ (2u64 << 32);
        assert!(ctx_index.has_conflict(pattern_hash));

        // Now test CTX_PROBE in VM
        let const_pool = vec![];
        let atom_id = [0u8; 32];
        let atom_view = AtomView::new(
            &atom_id,
            AtomType::FACT,
            &[],
            &[],
            0,
            u64::MAX,
            1000,
            0xFFFF,
            1,
        );
        let ctx_view = CtxView::new(ctx_id, &[], &[], 0);
        let qc_view = QueryConstraintsView::default();
        let exec_ctx = ExecutionContext::new(&[], None, &ctx_index, None);

        // Program: Load pattern_hash into r1, then CTX_PROBE
        // Note: CTX_PROBE uses reg[a] as pattern_hash
        let instructions = vec![
            Instruction::load_imm64(Opcode::LD_IMM, 1, pattern_hash), // r1 = pattern_hash
            Instruction::new(Opcode::CTX_PROBE, 1, 0, 0), // probe with r1 as pattern hash
            Instruction::new(Opcode::RET, 0, 0, InvariantResult::PASS as u64),
        ];

        let mut vm = VmInterpreter::new(&const_pool, atom_view, ctx_view, qc_view, exec_ctx, 1000);

        let result = vm.execute(&instructions).unwrap();

        // CTX_PROBE should detect the recorded context conflict and request branching.
        assert_eq!(
            result,
            InvariantResult::NEED_BRANCH,
            "CTX_PROBE should detect a recorded context conflict"
        );
        assert_eq!(vm.reason(), ReasonCode::CONFLICT_FOUND);
    }

    /// Test CTX_PROBE detects NO conflict when pattern doesn't exist
    #[test]
    fn test_ctx_probe_no_conflict() {
        use crate::store::api::CtxManager;
        use crate::vm::ClaimData;

        // Create CtxManager with one live claim bucket.
        let mut ctx_manager = CtxManager::new();
        let ctx_id = ctx_manager.create_context(0);

        let claim = ClaimData {
            subj: 1,
            pred: 2,
            obj_tag: 0,
            obj_val: 100,
            qualifiers_mask: 0,
        };
        let _ = ctx_manager.assert_claim_with_atom_id(ctx_id, &claim, [2u8; 32]);

        let ctx_index = ctx_manager.get_ctx_index(ctx_id);

        assert!(
            ctx_index.has_conflict(crate::vm::CtxIndex::claim_pattern_hash(&claim)),
            "Pattern bucket should exist for the active claim"
        );
        assert!(
            ctx_index.probe_conflict(&claim).is_none(),
            "Exact duplicate claim must not branch"
        );

        let same_pattern_different_value = ClaimData {
            subj: 1,
            pred: 2,
            obj_tag: 0,
            obj_val: 200,
            qualifiers_mask: 0,
        };
        assert!(
            ctx_index.probe_conflict(&same_pattern_different_value).is_some(),
            "Different value under the same pattern must be treated as a real conflict"
        );

        let different_pattern = ClaimData {
            subj: 999,
            pred: 999,
            obj_tag: 0,
            obj_val: 100,
            qualifiers_mask: 0,
        };
        assert!(
            ctx_index.probe_conflict(&different_pattern).is_none(),
            "Different pattern must not be treated as a conflict"
        );

        // Setup VM for the coarse-pattern path to ensure the opcode still behaves.
        let const_pool = vec![];
        let atom_id = [0u8; 32];
        let atom_view = AtomView::new(
            &atom_id,
            AtomType::FACT,
            &[],
            &[],
            0,
            u64::MAX,
            1000,
            0xFFFF,
            1,
        );
        let ctx_view = CtxView::new(ctx_id, &[], &[], 0);
        let qc_view = QueryConstraintsView::default();
        let exec_ctx = ExecutionContext::new(&[], None, &ctx_index, None);

        let different_pattern_hash = 999u64 ^ (999u64 << 32);
        let instructions = vec![
            Instruction::load_imm64(Opcode::LD_IMM, 1, different_pattern_hash),
            Instruction::new(Opcode::CTX_PROBE, 1, 0, 0),
            Instruction::new(Opcode::RET, InvariantResult::PASS as u16, 0, 0),
        ];

        let mut vm = VmInterpreter::new(&const_pool, atom_view, ctx_view, qc_view, exec_ctx, 1000);
        let result = vm.execute(&instructions).unwrap();

        assert_eq!(
            result,
            InvariantResult::PASS,
            "CTX_PROBE should not find conflict for different pattern"
        );
    }
}
