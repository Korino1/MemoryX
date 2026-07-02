//! Core types for MemoryX SKF-1.1 implementation.
//!
//! This module defines the fundamental enums and types used throughout the system:
//! - AtomType: classification of knowledge atoms
//! - EdgeType: types of relationships between atoms
//! - ObjTag: runtime type tags for values
//! - SectionKind: types of sections within an atom
//! - InvariantResult: results of invariant checks
//! - Reason codes: detailed failure reasons
//! - Intent: query intent classification
//! - GapKind: types of knowledge gaps
//! - CRDT kinds: conflict-free replicated data types
//!
//! # Submodules
//! - `api`: Main Store API (StoreConfig, MemoryX, AnswerPack)

#![allow(dead_code)]

use std::fmt;
use std::hash::Hash;

// Main Store API
pub mod api;

// ============================================================================
// AtomType (u32)
// ============================================================================

/// Classification of knowledge atoms in SKF-1.1
#[repr(u32)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AtomType {
    /// Definition of a term/concept
    DEFINITION = 1,
    /// Fact (verifiable statement)
    FACT = 2,
    /// Rule/inference/norm/constraint
    RULE = 3,
    /// Procedure/algorithm/instruction
    PROCEDURE = 4,
    /// Observation (raw signal/log)
    OBSERVATION = 5,
    /// Hypothesis (context branch)
    HYPOTHESIS = 6,
    /// Example/case
    EXAMPLE = 7,
    /// Counterexample
    COUNTEREXAMPLE = 8,
    /// Dataset/table
    DATASET = 9,
    /// Measurement (with units)
    MEASUREMENT = 10,
    /// Decision/choice (with justification)
    DECISION = 11,
    /// Conflict object (first-class)
    CONFLICT = 12,
    /// Cross-database mapping
    MAP = 13,
}

impl AtomType {
    /// Convert from u32, returning None for invalid values
    #[inline]
    pub const fn from_u32(value: u32) -> Option<Self> {
        match value {
            1 => Some(AtomType::DEFINITION),
            2 => Some(AtomType::FACT),
            3 => Some(AtomType::RULE),
            4 => Some(AtomType::PROCEDURE),
            5 => Some(AtomType::OBSERVATION),
            6 => Some(AtomType::HYPOTHESIS),
            7 => Some(AtomType::EXAMPLE),
            8 => Some(AtomType::COUNTEREXAMPLE),
            9 => Some(AtomType::DATASET),
            10 => Some(AtomType::MEASUREMENT),
            11 => Some(AtomType::DECISION),
            12 => Some(AtomType::CONFLICT),
            13 => Some(AtomType::MAP),
            _ => None,
        }
    }

    /// Convert to u32
    #[inline]
    pub const fn to_u32(self) -> u32 {
        self as u32
    }

    /// Check if this atom type can have evidence
    #[inline]
    pub const fn can_have_evidence(self) -> bool {
        matches!(
            self,
            AtomType::FACT
                | AtomType::HYPOTHESIS
                | AtomType::RULE
                | AtomType::DECISION
                | AtomType::CONFLICT
        )
    }

    /// Check if this is a structural atom type
    #[inline]
    pub const fn is_structural(self) -> bool {
        matches!(
            self,
            AtomType::DEFINITION | AtomType::RULE | AtomType::PROCEDURE
        )
    }
}

impl TryFrom<u32> for AtomType {
    type Error = InvalidAtomType;

    #[inline]
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::from_u32(value).ok_or(InvalidAtomType(value))
    }
}

impl From<AtomType> for u32 {
    #[inline]
    fn from(t: AtomType) -> u32 {
        t.to_u32()
    }
}

impl fmt::Display for AtomType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AtomType::DEFINITION => write!(f, "DEFINITION"),
            AtomType::FACT => write!(f, "FACT"),
            AtomType::RULE => write!(f, "RULE"),
            AtomType::PROCEDURE => write!(f, "PROCEDURE"),
            AtomType::OBSERVATION => write!(f, "OBSERVATION"),
            AtomType::HYPOTHESIS => write!(f, "HYPOTHESIS"),
            AtomType::EXAMPLE => write!(f, "EXAMPLE"),
            AtomType::COUNTEREXAMPLE => write!(f, "COUNTEREXAMPLE"),
            AtomType::DATASET => write!(f, "DATASET"),
            AtomType::MEASUREMENT => write!(f, "MEASUREMENT"),
            AtomType::DECISION => write!(f, "DECISION"),
            AtomType::CONFLICT => write!(f, "CONFLICT"),
            AtomType::MAP => write!(f, "MAP"),
        }
    }
}

/// Error for invalid AtomType conversion
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidAtomType(pub u32);

impl fmt::Display for InvalidAtomType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid AtomType value: {}", self.0)
    }
}

impl std::error::Error for InvalidAtomType {}

// ============================================================================
// EdgeType (u32)
// ============================================================================

/// Types of relationships between atoms
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EdgeType {
    /// Defines (term -> definition)
    DEFINES = 1,
    /// Refines (more specific version)
    REFINES = 2,
    /// Generalizes (more abstract version)
    GENERALIZES = 3,
    /// Implies (logical consequence)
    IMPLIES = 4,
    /// Supports (evidence/argument)
    SUPPORTS = 5,
    /// Contradicts (conflict)
    CONTRADICTS = 6,
    /// Same as (equivalence)
    SAME_AS = 7,
    /// Near duplicate
    NEAR_DUP = 8,
    /// Derived from
    DERIVED_FROM = 9,
    /// Depends on (prerequisite)
    DEPENDS_ON = 10,
    /// Causes (causal relationship)
    CAUSES = 11,
    /// Enables (enabling condition)
    ENABLES = 12,
    /// Prevents (inhibiting condition)
    PREVENTS = 13,
    /// Step of (procedure step)
    STEP_OF = 14,
    /// Input of
    INPUT_OF = 15,
    /// Output of
    OUTPUT_OF = 16,
    /// Maps to (cross-database)
    MAPS_TO = 17,
    /// Imported from (federation)
    IMPORTED_FROM = 18,
    /// Gateway to (federation)
    GATEWAY_TO = 19,
    /// Supercedes (new version replaces old) — SKF-1.1 §2.4
    SUPERSEDES = 20,
    /// Tombstone link (marks deleted atom)
    TOMBSTONE_LINK = 21,
}

impl EdgeType {
    /// Convert from u32, returning None for invalid values
    #[inline]
    pub const fn from_u32(value: u32) -> Option<Self> {
        match value {
            1 => Some(EdgeType::DEFINES),
            2 => Some(EdgeType::REFINES),
            3 => Some(EdgeType::GENERALIZES),
            4 => Some(EdgeType::IMPLIES),
            5 => Some(EdgeType::SUPPORTS),
            6 => Some(EdgeType::CONTRADICTS),
            7 => Some(EdgeType::SAME_AS),
            8 => Some(EdgeType::NEAR_DUP),
            9 => Some(EdgeType::DERIVED_FROM),
            10 => Some(EdgeType::DEPENDS_ON),
            11 => Some(EdgeType::CAUSES),
            12 => Some(EdgeType::ENABLES),
            13 => Some(EdgeType::PREVENTS),
            14 => Some(EdgeType::STEP_OF),
            15 => Some(EdgeType::INPUT_OF),
            16 => Some(EdgeType::OUTPUT_OF),
            17 => Some(EdgeType::MAPS_TO),
            18 => Some(EdgeType::IMPORTED_FROM),
            19 => Some(EdgeType::GATEWAY_TO),
            20 => Some(EdgeType::SUPERSEDES),
            21 => Some(EdgeType::TOMBSTONE_LINK),
            _ => None,
        }
    }

    /// Convert to u32
    #[inline]
    pub const fn to_u32(self) -> u32 {
        self as u32
    }

    /// Check if this edge type is causal
    #[inline]
    pub const fn is_causal(self) -> bool {
        matches!(
            self,
            EdgeType::CAUSES | EdgeType::ENABLES | EdgeType::PREVENTS
        )
    }

    /// Check if this edge type is for federation
    #[inline]
    pub const fn is_federation(self) -> bool {
        matches!(
            self,
            EdgeType::MAPS_TO | EdgeType::IMPORTED_FROM | EdgeType::GATEWAY_TO
        )
    }

    /// Check if this edge type represents conflict
    #[inline]
    pub const fn is_conflict(self) -> bool {
        matches!(self, EdgeType::CONTRADICTS)
    }

    /// Check if this edge type is structural
    #[inline]
    pub const fn is_structural(self) -> bool {
        matches!(
            self,
            EdgeType::DEFINES
                | EdgeType::REFINES
                | EdgeType::GENERALIZES
                | EdgeType::STEP_OF
                | EdgeType::INPUT_OF
                | EdgeType::OUTPUT_OF
        )
    }
}

impl TryFrom<u32> for EdgeType {
    type Error = InvalidEdgeType;

    #[inline]
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::from_u32(value).ok_or(InvalidEdgeType(value))
    }
}

impl From<EdgeType> for u32 {
    #[inline]
    fn from(t: EdgeType) -> u32 {
        t.to_u32()
    }
}

impl fmt::Display for EdgeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EdgeType::DEFINES => write!(f, "DEFINES"),
            EdgeType::REFINES => write!(f, "REFINES"),
            EdgeType::GENERALIZES => write!(f, "GENERALIZES"),
            EdgeType::IMPLIES => write!(f, "IMPLIES"),
            EdgeType::SUPPORTS => write!(f, "SUPPORTS"),
            EdgeType::CONTRADICTS => write!(f, "CONTRADICTS"),
            EdgeType::SAME_AS => write!(f, "SAME_AS"),
            EdgeType::NEAR_DUP => write!(f, "NEAR_DUP"),
            EdgeType::DERIVED_FROM => write!(f, "DERIVED_FROM"),
            EdgeType::DEPENDS_ON => write!(f, "DEPENDS_ON"),
            EdgeType::CAUSES => write!(f, "CAUSES"),
            EdgeType::ENABLES => write!(f, "ENABLES"),
            EdgeType::PREVENTS => write!(f, "PREVENTS"),
            EdgeType::STEP_OF => write!(f, "STEP_OF"),
            EdgeType::INPUT_OF => write!(f, "INPUT_OF"),
            EdgeType::OUTPUT_OF => write!(f, "OUTPUT_OF"),
            EdgeType::MAPS_TO => write!(f, "MAPS_TO"),
            EdgeType::IMPORTED_FROM => write!(f, "IMPORTED_FROM"),
            EdgeType::GATEWAY_TO => write!(f, "GATEWAY_TO"),
            EdgeType::SUPERSEDES => write!(f, "SUPERSEDES"),
            EdgeType::TOMBSTONE_LINK => write!(f, "TOMBSTONE_LINK"),
        }
    }
}

/// Error for invalid EdgeType conversion
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidEdgeType(pub u32);

impl fmt::Display for InvalidEdgeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid EdgeType value: {}", self.0)
    }
}

impl std::error::Error for InvalidEdgeType {}

// ============================================================================
// ObjTag (u8)
// ============================================================================

/// Runtime type tags for values
#[repr(u8)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum ObjTag {
    /// Null value
    NULL = 0,
    /// Boolean (1 byte: 0/1)
    BOOL = 1,
    /// Signed 64-bit integer (varint zigzag)
    I64 = 2,
    /// Unsigned 64-bit integer (varint)
    U64 = 3,
    /// 64-bit float (8 bytes raw)
    F64 = 4,
    /// Bytes (u32 off, u32 len in blob)
    BYTES = 5,
    /// Symbol (SymId u32)
    SYM = 6,
    /// Reference (RefId u32)
    REF = 7,
    /// Node number (u64 for GraphStore acceleration)
    NODENUM = 8,
}

impl ObjTag {
    /// Convert from u8, returning None for invalid values
    #[inline]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(ObjTag::NULL),
            1 => Some(ObjTag::BOOL),
            2 => Some(ObjTag::I64),
            3 => Some(ObjTag::U64),
            4 => Some(ObjTag::F64),
            5 => Some(ObjTag::BYTES),
            6 => Some(ObjTag::SYM),
            7 => Some(ObjTag::REF),
            8 => Some(ObjTag::NODENUM),
            _ => None,
        }
    }

    /// Convert to u8
    #[inline]
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// Get the size in bytes for fixed-size types
    #[inline]
    pub const fn fixed_size(self) -> Option<usize> {
        match self {
            ObjTag::NULL => Some(0),
            ObjTag::BOOL => Some(1),
            ObjTag::F64 => Some(8),
            ObjTag::SYM => Some(4),
            ObjTag::REF => Some(4),
            ObjTag::NODENUM => Some(8),
            _ => None, // Variable size (varint or pointer)
        }
    }

    /// Check if this is a numeric type
    #[inline]
    pub const fn is_numeric(self) -> bool {
        matches!(self, ObjTag::I64 | ObjTag::U64 | ObjTag::F64)
    }

    /// Check if this is a reference type
    #[inline]
    pub const fn is_reference(self) -> bool {
        matches!(self, ObjTag::REF | ObjTag::NODENUM)
    }
}

impl TryFrom<u8> for ObjTag {
    type Error = InvalidObjTag;

    #[inline]
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::from_u8(value).ok_or(InvalidObjTag(value))
    }
}

impl From<ObjTag> for u8 {
    #[inline]
    fn from(t: ObjTag) -> u8 {
        t.to_u8()
    }
}

impl fmt::Display for ObjTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ObjTag::NULL => write!(f, "NULL"),
            ObjTag::BOOL => write!(f, "BOOL"),
            ObjTag::I64 => write!(f, "I64"),
            ObjTag::U64 => write!(f, "U64"),
            ObjTag::F64 => write!(f, "F64"),
            ObjTag::BYTES => write!(f, "BYTES"),
            ObjTag::SYM => write!(f, "SYM"),
            ObjTag::REF => write!(f, "REF"),
            ObjTag::NODENUM => write!(f, "NODENUM"),
        }
    }
}

/// Error for invalid ObjTag conversion
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidObjTag(pub u8);

impl fmt::Display for InvalidObjTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid ObjTag value: {}", self.0)
    }
}

impl std::error::Error for InvalidObjTag {}

// ============================================================================
// SectionKind (u32)
// ============================================================================

/// Types of sections within an atom
#[repr(u32)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum SectionKind {
    /// Symbol definitions
    SYMBOLS = 0x01,
    /// References
    REFS = 0x02,
    /// Claims (main content)
    CLAIMS = 0x03,
    /// Invariants (constraints)
    INVARIANTS = 0x04,
    /// Edges (relationships)
    EDGES = 0x05,
    /// Evidence (sources, proofs)
    EVIDENCE = 0x06,
    /// Metadata
    META = 0x07,
}

impl SectionKind {
    /// Convert from u32, returning None for invalid values
    #[inline]
    pub const fn from_u32(value: u32) -> Option<Self> {
        match value {
            0x01 => Some(SectionKind::SYMBOLS),
            0x02 => Some(SectionKind::REFS),
            0x03 => Some(SectionKind::CLAIMS),
            0x04 => Some(SectionKind::INVARIANTS),
            0x05 => Some(SectionKind::EDGES),
            0x06 => Some(SectionKind::EVIDENCE),
            0x07 => Some(SectionKind::META),
            _ => None,
        }
    }

    /// Convert to u32
    #[inline]
    pub const fn to_u32(self) -> u32 {
        self as u32
    }

    /// Check if this section type is required for all atoms
    #[inline]
    pub const fn is_required(self) -> bool {
        matches!(self, SectionKind::CLAIMS)
    }

    /// Check if this section can contain references to other atoms
    #[inline]
    pub const fn can_contain_refs(self) -> bool {
        matches!(
            self,
            SectionKind::REFS | SectionKind::EDGES | SectionKind::EVIDENCE
        )
    }
}

impl TryFrom<u32> for SectionKind {
    type Error = InvalidSectionKind;

    #[inline]
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::from_u32(value).ok_or(InvalidSectionKind(value))
    }
}

impl From<SectionKind> for u32 {
    #[inline]
    fn from(t: SectionKind) -> u32 {
        t.to_u32()
    }
}

impl fmt::Display for SectionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SectionKind::SYMBOLS => write!(f, "SYMBOLS"),
            SectionKind::REFS => write!(f, "REFS"),
            SectionKind::CLAIMS => write!(f, "CLAIMS"),
            SectionKind::INVARIANTS => write!(f, "INVARIANTS"),
            SectionKind::EDGES => write!(f, "EDGES"),
            SectionKind::EVIDENCE => write!(f, "EVIDENCE"),
            SectionKind::META => write!(f, "META"),
        }
    }
}

/// Error for invalid SectionKind conversion
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidSectionKind(pub u32);

impl fmt::Display for InvalidSectionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid SectionKind value: {}", self.0)
    }
}

impl std::error::Error for InvalidSectionKind {}

// ============================================================================
// InvariantResult
// ============================================================================

/// Result of an invariant check
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum InvariantResult {
    /// Invariant passed
    PASS = 0,
    /// Soft failure (warning, can proceed with caution)
    FAIL_SOFT = 1,
    /// Hard failure (must not proceed)
    FAIL_HARD = 2,
    /// Need branching (context split required)
    NEED_BRANCH = 3,
}

impl InvariantResult {
    /// Convert from u8, returning None for invalid values
    #[inline]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(InvariantResult::PASS),
            1 => Some(InvariantResult::FAIL_SOFT),
            2 => Some(InvariantResult::FAIL_HARD),
            3 => Some(InvariantResult::NEED_BRANCH),
            _ => None,
        }
    }

    /// Convert to u8
    #[inline]
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// Check if this result allows proceeding
    #[inline]
    pub const fn allows_proceed(self) -> bool {
        matches!(self, InvariantResult::PASS | InvariantResult::FAIL_SOFT)
    }

    /// Check if this is a failure result
    #[inline]
    pub const fn is_failure(self) -> bool {
        matches!(
            self,
            InvariantResult::FAIL_SOFT | InvariantResult::FAIL_HARD
        )
    }
}

impl TryFrom<u8> for InvariantResult {
    type Error = InvalidInvariantResult;

    #[inline]
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::from_u8(value).ok_or(InvalidInvariantResult(value))
    }
}

impl From<InvariantResult> for u8 {
    #[inline]
    fn from(r: InvariantResult) -> u8 {
        r.to_u8()
    }
}

impl fmt::Display for InvariantResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InvariantResult::PASS => write!(f, "PASS"),
            InvariantResult::FAIL_SOFT => write!(f, "FAIL_SOFT"),
            InvariantResult::FAIL_HARD => write!(f, "FAIL_HARD"),
            InvariantResult::NEED_BRANCH => write!(f, "NEED_BRANCH"),
        }
    }
}

/// Error for invalid InvariantResult conversion
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidInvariantResult(pub u8);

impl fmt::Display for InvalidInvariantResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid InvariantResult value: {}", self.0)
    }
}

impl std::error::Error for InvalidInvariantResult {}

// ============================================================================
// Reason Codes (u16)
// ============================================================================

/// Detailed failure reasons for invariant checks and operations
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ReasonCode {
    /// Time constraint violated
    TIME_INVALID = 1,
    /// Trust level too low
    TRUST_TOO_LOW = 2,
    /// Domain mismatch
    DOMAIN_MISMATCH = 3,
    /// Schema mismatch
    SCHEMA_MISMATCH = 4,
    /// Source denied access
    SOURCE_DENIED = 5,
    /// Conflict found
    CONFLICT_FOUND = 6,
    /// Missing evidence
    MISSING_EVIDENCE = 7,
    /// Version incompatibility
    VERSION_INCOMPATIBLE = 8,
    /// Corrupt section data
    CORRUPT_SECTION = 9,
    /// Budget exceeded (IO/compute)
    BUDGET_EXCEEDED = 10,
}

impl ReasonCode {
    /// Convert from u16, returning None for invalid values
    #[inline]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            1 => Some(ReasonCode::TIME_INVALID),
            2 => Some(ReasonCode::TRUST_TOO_LOW),
            3 => Some(ReasonCode::DOMAIN_MISMATCH),
            4 => Some(ReasonCode::SCHEMA_MISMATCH),
            5 => Some(ReasonCode::SOURCE_DENIED),
            6 => Some(ReasonCode::CONFLICT_FOUND),
            7 => Some(ReasonCode::MISSING_EVIDENCE),
            8 => Some(ReasonCode::VERSION_INCOMPATIBLE),
            9 => Some(ReasonCode::CORRUPT_SECTION),
            10 => Some(ReasonCode::BUDGET_EXCEEDED),
            _ => None,
        }
    }

    /// Convert to u16
    #[inline]
    pub const fn to_u16(self) -> u16 {
        self as u16
    }

    /// Check if this reason is recoverable
    #[inline]
    pub const fn is_recoverable(self) -> bool {
        matches!(
            self,
            ReasonCode::MISSING_EVIDENCE | ReasonCode::BUDGET_EXCEEDED
        )
    }

    /// Check if this indicates data corruption
    #[inline]
    pub const fn is_corruption(self) -> bool {
        matches!(self, ReasonCode::CORRUPT_SECTION)
    }
}

impl TryFrom<u16> for ReasonCode {
    type Error = InvalidReasonCode;

    #[inline]
    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Self::from_u16(value).ok_or(InvalidReasonCode(value))
    }
}

impl From<ReasonCode> for u16 {
    #[inline]
    fn from(r: ReasonCode) -> u16 {
        r.to_u16()
    }
}

impl fmt::Display for ReasonCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReasonCode::TIME_INVALID => write!(f, "TIME_INVALID"),
            ReasonCode::TRUST_TOO_LOW => write!(f, "TRUST_TOO_LOW"),
            ReasonCode::DOMAIN_MISMATCH => write!(f, "DOMAIN_MISMATCH"),
            ReasonCode::SCHEMA_MISMATCH => write!(f, "SCHEMA_MISMATCH"),
            ReasonCode::SOURCE_DENIED => write!(f, "SOURCE_DENIED"),
            ReasonCode::CONFLICT_FOUND => write!(f, "CONFLICT_FOUND"),
            ReasonCode::MISSING_EVIDENCE => write!(f, "MISSING_EVIDENCE"),
            ReasonCode::VERSION_INCOMPATIBLE => write!(f, "VERSION_INCOMPATIBLE"),
            ReasonCode::CORRUPT_SECTION => write!(f, "CORRUPT_SECTION"),
            ReasonCode::BUDGET_EXCEEDED => write!(f, "BUDGET_EXCEEDED"),
        }
    }
}

/// Error for invalid ReasonCode conversion
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidReasonCode(pub u16);

impl fmt::Display for InvalidReasonCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid ReasonCode value: {}", self.0)
    }
}

impl std::error::Error for InvalidReasonCode {}

// ============================================================================
// Intent (u8)
// ============================================================================

/// Query intent classification
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Intent {
    /// Lookup specific information
    LOOKUP = 1,
    /// Define a term/concept
    DEFINE = 2,
    /// Explain a phenomenon
    EXPLAIN = 3,
    /// Compare entities
    COMPARE = 4,
    /// Derive conclusions
    DERIVE = 5,
    /// Verify a claim
    VERIFY = 6,
    /// Plan actions
    PLAN = 7,
}

impl Intent {
    /// Convert from u8, returning None for invalid values
    #[inline]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Intent::LOOKUP),
            2 => Some(Intent::DEFINE),
            3 => Some(Intent::EXPLAIN),
            4 => Some(Intent::COMPARE),
            5 => Some(Intent::DERIVE),
            6 => Some(Intent::VERIFY),
            7 => Some(Intent::PLAN),
            _ => None,
        }
    }

    /// Convert to u8
    #[inline]
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// Check if this intent requires causal chain
    #[inline]
    pub const fn needs_causal_chain(self) -> bool {
        matches!(self, Intent::EXPLAIN | Intent::DERIVE)
    }

    /// Check if this intent requires evidence
    #[inline]
    pub const fn needs_evidence(self) -> bool {
        matches!(self, Intent::VERIFY | Intent::DERIVE)
    }
}

impl TryFrom<u8> for Intent {
    type Error = InvalidIntent;

    #[inline]
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::from_u8(value).ok_or(InvalidIntent(value))
    }
}

impl From<Intent> for u8 {
    #[inline]
    fn from(i: Intent) -> u8 {
        i.to_u8()
    }
}

impl fmt::Display for Intent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Intent::LOOKUP => write!(f, "LOOKUP"),
            Intent::DEFINE => write!(f, "DEFINE"),
            Intent::EXPLAIN => write!(f, "EXPLAIN"),
            Intent::COMPARE => write!(f, "COMPARE"),
            Intent::DERIVE => write!(f, "DERIVE"),
            Intent::VERIFY => write!(f, "VERIFY"),
            Intent::PLAN => write!(f, "PLAN"),
        }
    }
}

/// Error for invalid Intent conversion
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidIntent(pub u8);

impl fmt::Display for InvalidIntent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid Intent value: {}", self.0)
    }
}

impl std::error::Error for InvalidIntent {}

// ============================================================================
// GapKind (u8)
// ============================================================================

/// Types of knowledge gaps
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GapKind {
    /// Need definition
    NEED_DEFINITION = 1,
    /// Need fact
    NEED_FACT = 2,
    /// Need causal chain
    NEED_CAUSAL_CHAIN = 3,
    /// Need procedure
    NEED_PROCEDURE = 4,
    /// Need constraints
    NEED_CONSTRAINTS = 5,
    /// Need counterexample
    NEED_COUNTEREXAMPLE = 6,
    /// Need comparison axis
    NEED_COMPARISON_AXIS = 7,
    /// Need evidence
    NEED_EVIDENCE = 8,
}

impl GapKind {
    /// Convert from u8, returning None for invalid values
    #[inline]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(GapKind::NEED_DEFINITION),
            2 => Some(GapKind::NEED_FACT),
            3 => Some(GapKind::NEED_CAUSAL_CHAIN),
            4 => Some(GapKind::NEED_PROCEDURE),
            5 => Some(GapKind::NEED_CONSTRAINTS),
            6 => Some(GapKind::NEED_COUNTEREXAMPLE),
            7 => Some(GapKind::NEED_COMPARISON_AXIS),
            8 => Some(GapKind::NEED_EVIDENCE),
            _ => None,
        }
    }

    /// Convert to u8
    #[inline]
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// Check if this gap type requires graph traversal
    #[inline]
    pub const fn needs_graph_walk(self) -> bool {
        matches!(
            self,
            GapKind::NEED_CAUSAL_CHAIN | GapKind::NEED_COUNTEREXAMPLE
        )
    }

    /// Check if this gap type requires definition lookup
    #[inline]
    pub const fn needs_definition(self) -> bool {
        matches!(self, GapKind::NEED_DEFINITION | GapKind::NEED_CONSTRAINTS)
    }
}

impl TryFrom<u8> for GapKind {
    type Error = InvalidGapKind;

    #[inline]
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::from_u8(value).ok_or(InvalidGapKind(value))
    }
}

impl From<GapKind> for u8 {
    #[inline]
    fn from(g: GapKind) -> u8 {
        g.to_u8()
    }
}

impl fmt::Display for GapKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GapKind::NEED_DEFINITION => write!(f, "NEED_DEFINITION"),
            GapKind::NEED_FACT => write!(f, "NEED_FACT"),
            GapKind::NEED_CAUSAL_CHAIN => write!(f, "NEED_CAUSAL_CHAIN"),
            GapKind::NEED_PROCEDURE => write!(f, "NEED_PROCEDURE"),
            GapKind::NEED_CONSTRAINTS => write!(f, "NEED_CONSTRAINTS"),
            GapKind::NEED_COUNTEREXAMPLE => write!(f, "NEED_COUNTEREXAMPLE"),
            GapKind::NEED_COMPARISON_AXIS => write!(f, "NEED_COMPARISON_AXIS"),
            GapKind::NEED_EVIDENCE => write!(f, "NEED_EVIDENCE"),
        }
    }
}

/// Error for invalid GapKind conversion
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidGapKind(pub u8);

impl fmt::Display for InvalidGapKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid GapKind value: {}", self.0)
    }
}

impl std::error::Error for InvalidGapKind {}

// ============================================================================
// CRDT Kinds (u8)
// ============================================================================

/// Conflict-free Replicated Data Type kinds
#[repr(u8)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CrdtKind {
    /// G-Counter (grow-only counter)
    GCOUNTER = 1,
    /// PN-Counter (positive-negative counter)
    PNCOUNTER = 2,
    /// LWW Register (last-writer-wins)
    LWW_REG = 3,
    /// OR-Set (observed-remove set)
    ORSET = 4,
    /// OR-Map (observed-remove map)
    ORMAP = 5,
    /// MV-Register (multi-value register)
    MVREG = 6,
    /// Flag Set (LWW bitmask)
    FLAGSET = 7,
}

impl CrdtKind {
    /// Convert from u8, returning None for invalid values
    #[inline]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(CrdtKind::GCOUNTER),
            2 => Some(CrdtKind::PNCOUNTER),
            3 => Some(CrdtKind::LWW_REG),
            4 => Some(CrdtKind::ORSET),
            5 => Some(CrdtKind::ORMAP),
            6 => Some(CrdtKind::MVREG),
            7 => Some(CrdtKind::FLAGSET),
            _ => None,
        }
    }

    /// Convert to u8
    #[inline]
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// Check if this CRDT is a counter type
    #[inline]
    pub const fn is_counter(self) -> bool {
        matches!(self, CrdtKind::GCOUNTER | CrdtKind::PNCOUNTER)
    }

    /// Check if this CRDT uses LWW semantics
    #[inline]
    pub const fn is_lww(self) -> bool {
        matches!(self, CrdtKind::LWW_REG | CrdtKind::FLAGSET)
    }

    /// Check if this CRDT is a collection type
    #[inline]
    pub const fn is_collection(self) -> bool {
        matches!(self, CrdtKind::ORSET | CrdtKind::ORMAP | CrdtKind::MVREG)
    }
}

impl TryFrom<u8> for CrdtKind {
    type Error = InvalidCrdtKind;

    #[inline]
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::from_u8(value).ok_or(InvalidCrdtKind(value))
    }
}

impl From<CrdtKind> for u8 {
    #[inline]
    fn from(k: CrdtKind) -> u8 {
        k.to_u8()
    }
}

impl fmt::Display for CrdtKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CrdtKind::GCOUNTER => write!(f, "GCOUNTER"),
            CrdtKind::PNCOUNTER => write!(f, "PNCOUNTER"),
            CrdtKind::LWW_REG => write!(f, "LWW_REG"),
            CrdtKind::ORSET => write!(f, "ORSET"),
            CrdtKind::ORMAP => write!(f, "ORMAP"),
            CrdtKind::MVREG => write!(f, "MVREG"),
            CrdtKind::FLAGSET => write!(f, "FLAGSET"),
        }
    }
}

/// Error for invalid CrdtKind conversion
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidCrdtKind(pub u8);

impl fmt::Display for InvalidCrdtKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid CrdtKind value: {}", self.0)
    }
}

impl std::error::Error for InvalidCrdtKind {}

// ============================================================================
// AtomId type alias
// ============================================================================

/// Atom identifier: BLAKE3-256 hash (32 bytes)
pub type AtomId = [u8; 32];

/// Symbol identifier (u32)
pub type SymId = u32;

/// Reference identifier (u32)
pub type RefId = u32;

/// Node number for graph acceleration (u64)
pub type NodeNum = u64;

// ============================================================================
// Additional utility types
// ============================================================================

/// Trust level (0-65535)
pub type TrustLevel = u16;

/// Bucket for time-based filtering (unix timestamp / bucket_size)
pub type TimeBucket = u32;

/// Domain mask (bitmask of domains)
pub type DomainMask = u64;

/// Claim pattern reference
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PatternRef {
    /// Match any
    #[default]
    Any,
    /// Match specific symbol
    Sym(SymId),
    /// Match specific node
    Node(NodeNum),
    /// Match range (for numeric values)
    Range { min: i64, max: i64 },
    /// Match set of values
    Set(&'static [SymId]),
}

impl PatternRef {
    /// Check if this is Any pattern
    #[inline]
    pub const fn is_any(&self) -> bool {
        matches!(self, PatternRef::Any)
    }

    /// Get node number if available
    #[inline]
    pub const fn as_node(&self) -> Option<NodeNum> {
        match self {
            PatternRef::Node(n) => Some(*n),
            _ => None,
        }
    }

    /// Get symbol ID if available
    #[inline]
    pub const fn as_sym(&self) -> Option<SymId> {
        match self {
            PatternRef::Sym(s) => Some(*s),
            _ => None,
        }
    }
}

/// Claim pattern for gap matching
#[derive(Debug, Clone)]
pub struct ClaimPattern {
    pub subj: PatternRef,
    pub pred: PatternRef,
    pub obj_tag: Option<ObjTag>,
    pub obj: PatternRef,
    pub qualifiers_mask: u32,
}

impl Default for ClaimPattern {
    fn default() -> Self {
        ClaimPattern {
            subj: PatternRef::Any,
            pred: PatternRef::Any,
            obj_tag: None,
            obj: PatternRef::Any,
            qualifiers_mask: 0,
        }
    }
}

/// Navigation hint for graph traversal
#[derive(Debug, Clone)]
pub struct NavHint {
    pub seed_nodes: Vec<NodeNum>,
    pub edge_types: Vec<EdgeType>,
    pub max_depth: u8,
    pub fanout_limit: u16,
}

impl Default for NavHint {
    fn default() -> Self {
        NavHint {
            seed_nodes: Vec::new(),
            edge_types: Vec::new(),
            max_depth: 3,
            fanout_limit: 128,
        }
    }
}

/// Stop conditions for traversal
#[derive(Debug, Clone, Copy, Default)]
pub struct StopCond {
    pub max_nodes: u32,
    pub max_io_bytes: u64,
    pub min_trust: TrustLevel,
    pub max_conflicts: u32,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_atom_type_roundtrip() {
        for i in 1..=13u32 {
            let atom_type = AtomType::from_u32(i).unwrap();
            assert_eq!(atom_type.to_u32(), i);
        }
        assert!(AtomType::from_u32(0).is_none());
        assert!(AtomType::from_u32(14).is_none());
    }

    #[test]
    fn test_edge_type_roundtrip() {
        for i in 1..=21u32 {
            let edge_type = EdgeType::from_u32(i).unwrap();
            assert_eq!(edge_type.to_u32(), i);
        }
        assert!(EdgeType::from_u32(0).is_none());
        assert!(EdgeType::from_u32(22).is_none());
    }

    #[test]
    fn test_obj_tag_roundtrip() {
        for i in 0..=8u8 {
            let obj_tag = ObjTag::from_u8(i).unwrap();
            assert_eq!(obj_tag.to_u8(), i);
        }
        assert!(ObjTag::from_u8(9).is_none());
    }

    #[test]
    fn test_section_kind_roundtrip() {
        let values = [0x01u32, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];
        for &v in &values {
            let kind = SectionKind::from_u32(v).unwrap();
            assert_eq!(kind.to_u32(), v);
        }
    }

    #[test]
    fn test_invariant_result() {
        assert!(InvariantResult::PASS.allows_proceed());
        assert!(InvariantResult::FAIL_SOFT.allows_proceed());
        assert!(!InvariantResult::FAIL_HARD.allows_proceed());
        assert!(!InvariantResult::NEED_BRANCH.allows_proceed());
    }

    #[test]
    fn test_reason_codes() {
        for i in 1..=10u16 {
            let code = ReasonCode::from_u16(i).unwrap();
            assert_eq!(code.to_u16(), i);
        }
    }

    #[test]
    fn test_intent_properties() {
        assert!(Intent::EXPLAIN.needs_causal_chain());
        assert!(Intent::VERIFY.needs_evidence());
        assert!(!Intent::LOOKUP.needs_causal_chain());
    }

    #[test]
    fn test_gap_kind_properties() {
        assert!(GapKind::NEED_CAUSAL_CHAIN.needs_graph_walk());
        assert!(GapKind::NEED_DEFINITION.needs_definition());
    }

    #[test]
    fn test_crdt_kind_classification() {
        assert!(CrdtKind::GCOUNTER.is_counter());
        assert!(CrdtKind::LWW_REG.is_lww());
        assert!(CrdtKind::ORSET.is_collection());
    }

    #[test]
    fn test_display_traits() {
        assert_eq!(format!("{}", AtomType::FACT), "FACT");
        assert_eq!(format!("{}", EdgeType::CAUSES), "CAUSES");
        assert_eq!(format!("{}", Intent::VERIFY), "VERIFY");
    }
}
