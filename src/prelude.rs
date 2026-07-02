//! Prelude module for MemoryX SKF-1.1 implementation.
//!
//! This module re-exports all main types for convenient importing:
//! ```
//! use memoryx::prelude::*;
//! ```

// ============================================================================
// Core Store Types
// ============================================================================

pub use crate::store::AtomType;
pub use crate::store::CrdtKind;
pub use crate::store::EdgeType;
pub use crate::store::GapKind;
pub use crate::store::Intent;
pub use crate::store::InvariantResult;
pub use crate::store::ObjTag;
pub use crate::store::ReasonCode;
pub use crate::store::SectionKind;

// Store type aliases
pub use crate::store::AtomId;
pub use crate::store::ClaimPattern;
pub use crate::store::DomainMask;
pub use crate::store::NavHint;
pub use crate::store::NodeNum;
pub use crate::store::PatternRef;
pub use crate::store::RefId;
pub use crate::store::StopCond;
pub use crate::store::SymId;
pub use crate::store::TimeBucket;
pub use crate::store::TrustLevel;

// Store error types
pub use crate::store::InvalidAtomType;
pub use crate::store::InvalidCrdtKind;
pub use crate::store::InvalidEdgeType;
pub use crate::store::InvalidGapKind;
pub use crate::store::InvalidIntent;
pub use crate::store::InvalidInvariantResult;
pub use crate::store::InvalidObjTag;
pub use crate::store::InvalidReasonCode;
pub use crate::store::InvalidSectionKind;

// ============================================================================
// VM Types
// ============================================================================

pub use crate::vm::AtomView;
pub use crate::vm::BytecodeBuilder;
pub use crate::vm::ClaimData;
pub use crate::vm::ConflictProbe;
pub use crate::vm::ConstPoolBuilder;
pub use crate::vm::ConstValue;
pub use crate::vm::CtxView;
pub use crate::vm::Instruction;
pub use crate::vm::Opcode;
pub use crate::vm::QueryConstraintsView;
pub use crate::vm::VmInterpreter;
pub use crate::vm::VmState;
pub use crate::vm::build_basic_invariant_program;
pub use crate::vm::build_conflict_probe_program;

// ============================================================================
// Query Types
// ============================================================================

pub use crate::query::AgEdge;
pub use crate::query::AgNode;
pub use crate::query::AnswerGraph;
pub use crate::query::BackendKind;
pub use crate::query::BackendResult;
pub use crate::query::BackwardWaveGenerator;
pub use crate::query::Candidate;
pub use crate::query::CostWeights;
pub use crate::query::EntityRef;
pub use crate::query::FixedPointSolver;
pub use crate::query::FixedPointState;
pub use crate::query::Gap;
pub use crate::query::GoalSpec;
pub use crate::query::GoalSpecCompiler;
pub use crate::query::IoMode;
pub use crate::query::OutputSchema;
pub use crate::query::QueryRouter;
pub use crate::query::RetrievalPlan;
pub use crate::query::Router;
pub use crate::query::SourcePriority;
pub use crate::query::TimeMode;

// ============================================================================
// CAS Types
// ============================================================================

pub use crate::cas::AtomBodyFlags;
pub use crate::cas::AtomBodyHeader;
pub use crate::cas::CasError;
pub use crate::cas::RecordFlags;
pub use crate::cas::RecordHeader;
pub use crate::cas::RecordView;
pub use crate::cas::SectionDesc;
pub use crate::cas::SectionFlags;

// CAS constants
pub use crate::cas::ATOM_BODY_VERSION;
pub use crate::cas::ATOM_MAGIC;
pub use crate::cas::RECORD_FORMAT_VERSION;
pub use crate::cas::RECORD_MAGIC;

// CAS helper functions
pub use crate::cas::find_section;
pub use crate::cas::get_section_data;
pub use crate::cas::get_section_data_unchecked;
pub use crate::cas::hex_decode;
pub use crate::cas::hex_encode;
pub use crate::cas::section_table_entry_offset;
pub use crate::cas::validate_section_bounds;
pub use crate::cas::validate_sections;

// ============================================================================
// Utility Functions
// ============================================================================

pub use crate::utils::BitPackBlockHeader;
pub use crate::utils::HLC;
pub use crate::utils::HLCGenerator;

// CRC32 functions
pub use crate::utils::crc32;
pub use crate::utils::crc32_u16;
pub use crate::utils::crc32_u32;
pub use crate::utils::crc32_u64;

// Varint functions
pub use crate::utils::VARINT_MAX_BYTES;
pub use crate::utils::ZIGZAG_MAX_BYTES;
pub use crate::utils::decode_varint;
pub use crate::utils::decode_zigzag;
pub use crate::utils::decode_zigzag_varint;
pub use crate::utils::encode_varint;
pub use crate::utils::encode_varint_fixed;
pub use crate::utils::encode_zigzag;
pub use crate::utils::encode_zigzag_varint;

// Bit packing functions
pub use crate::utils::BITPACK_BLOCK_SIZE;
pub use crate::utils::bitpack_decode;
pub use crate::utils::bitpack_decode_deltas;
pub use crate::utils::bitpack_encode;
pub use crate::utils::bitpack_encode_deltas;

// Read/write helpers
pub use crate::utils::read_u16_le;
pub use crate::utils::read_u32_le;
pub use crate::utils::read_u64_le;
pub use crate::utils::write_u16_le;
pub use crate::utils::write_u32_le;
pub use crate::utils::write_u64_le;

// ============================================================================
// Store API Types (from store::api)
// ============================================================================

pub use crate::store::api::ActiveClaim;
pub use crate::store::api::AnswerPack;
pub use crate::store::api::BranchReason;
pub use crate::store::api::ClaimStatus;
pub use crate::store::api::ClaimView;
pub use crate::store::api::ClaimViewV2;
pub use crate::store::api::Conflict;
pub use crate::store::api::ConflictSeverity;
pub use crate::store::api::ConflictType;
pub use crate::store::api::ContextBranch;
pub use crate::store::api::CoverageReport;
pub use crate::store::api::CtxId;
pub use crate::store::api::CtxManager;
pub use crate::store::api::CtxPolicyId;
pub use crate::store::api::EvidenceRecord;
pub use crate::store::api::EvidenceRef;
pub use crate::store::api::EvidenceSpan;
pub use crate::store::api::Limitation;
pub use crate::store::api::LimitationCode;
pub use crate::store::api::LimitationSeverity;
pub use crate::store::api::MemoryX;
pub use crate::store::api::Modality;
pub use crate::store::api::Polarity;
pub use crate::store::api::Qualifier;
pub use crate::store::api::SourceId;
pub use crate::store::api::SourceKind;
pub use crate::store::api::SourceLocation;
pub use crate::store::api::SourceRecord;
pub use crate::store::api::StoreConfig;
pub use crate::store::api::StoreError;
pub use crate::store::api::TimeInterval;

// Federation types
#[cfg(feature = "federation")]
pub use crate::federation::{
    AtomMetadata, AtomTypeInfo, AtomTypeSupport, BaseId, ConstraintType, CrdtConflict,
    CrdtMetadata, DiscoverRequest, DiscoverResponse, DiscoveryResult, EvidenceType,
    FEDERATION_PROTOCOL_VERSION, FederationClient, FederationConfig, FederationError, FetchRequest,
    FetchResponse, FieldMapping, Gateway, MappingConstraint, MappingEvidence, MapsTo,
    NegotiateRequest, NegotiateResponse, PeerConfig, SchemaAgreement, SyncDirection, SyncRequest,
    SyncResponse,
};

// Internal types (not part of public API)
// pub use crate::store::api::TermIndex;
// pub use crate::store::api::LocationIndex;
// pub use crate::store::api::CasStore;
// pub use crate::store::api::MetaStore;

// ============================================================================
// Re-export commonly used standard library items
// ============================================================================

pub use std::convert::TryFrom;
pub use std::fmt::{Debug, Display};
pub use std::hash::Hash;

// ============================================================================
// Convenience types for common operations
// ============================================================================

/// Result type for MemoryX operations
pub type Result<T, E = CasError> = std::result::Result<T, E>;

/// Byte buffer type
pub type Buffer = Vec<u8>;

/// Optional value with trust level
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustValue<T> {
    pub value: T,
    pub trust: TrustLevel,
}

impl<T> TrustValue<T> {
    pub fn new(value: T, trust: TrustLevel) -> Self {
        TrustValue { value, trust }
    }

    pub fn map<U, F: FnOnce(T) -> U>(self, f: F) -> TrustValue<U> {
        TrustValue {
            value: f(self.value),
            trust: self.trust,
        }
    }
}

/// Time range specification
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeRange {
    pub from_ns: u64,
    pub to_ns: u64,
}

impl TimeRange {
    pub fn new(from_ns: u64, to_ns: u64) -> Self {
        TimeRange { from_ns, to_ns }
    }

    pub fn contains(&self, timestamp_ns: u64) -> bool {
        timestamp_ns >= self.from_ns && timestamp_ns < self.to_ns
    }

    pub fn overlaps(&self, other: &TimeRange) -> bool {
        self.from_ns < other.to_ns && other.from_ns < self.to_ns
    }

    pub fn from_hlc_range(from: HLC, to: HLC) -> Self {
        TimeRange {
            from_ns: from.physical_ns(),
            to_ns: to.physical_ns(),
        }
    }
}

impl Default for TimeRange {
    fn default() -> Self {
        TimeRange {
            from_ns: 0,
            to_ns: u64::MAX,
        }
    }
}

/// Query constraints
#[derive(Debug, Clone, Default)]
pub struct QueryConstraints {
    pub time_range: Option<TimeRange>,
    pub trust_min: Option<TrustLevel>,
    pub domain_mask: Option<DomainMask>,
    pub atom_types: Option<Vec<AtomType>>,
    pub max_results: Option<u32>,
}

impl QueryConstraints {
    pub fn new() -> Self {
        QueryConstraints::default()
    }

    pub fn with_time_range(mut self, range: TimeRange) -> Self {
        self.time_range = Some(range);
        self
    }

    pub fn with_trust_min(mut self, trust: TrustLevel) -> Self {
        self.trust_min = Some(trust);
        self
    }

    pub fn with_domain(mut self, mask: DomainMask) -> Self {
        self.domain_mask = Some(mask);
        self
    }

    pub fn with_atom_types(mut self, types: Vec<AtomType>) -> Self {
        self.atom_types = Some(types);
        self
    }

    pub fn with_max_results(mut self, max: u32) -> Self {
        self.max_results = Some(max);
        self
    }

    pub fn matches_time(&self, from_ns: u64, to_ns: u64) -> bool {
        if let Some(range) = &self.time_range {
            range.from_ns < to_ns && from_ns < range.to_ns
        } else {
            true
        }
    }

    pub fn matches_trust(&self, trust: TrustLevel) -> bool {
        self.trust_min.is_none_or(|min| trust >= min)
    }

    pub fn matches_domain(&self, domain: DomainMask) -> bool {
        self.domain_mask.is_none_or(|mask| domain & mask != 0)
    }

    pub fn matches_atom_type(&self, atom_type: AtomType) -> bool {
        self.atom_types
            .as_ref()
            .is_none_or(|types| types.contains(&atom_type))
    }
}

/// Edge reference for graph traversal
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeRef {
    pub src_node: NodeNum,
    pub dst_node: NodeNum,
    pub edge_type: EdgeType,
    pub confidence: TrustLevel,
}

impl EdgeRef {
    pub fn new(
        src_node: NodeNum,
        dst_node: NodeNum,
        edge_type: EdgeType,
        confidence: TrustLevel,
    ) -> Self {
        EdgeRef {
            src_node,
            dst_node,
            edge_type,
            confidence,
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trust_value() {
        let tv = TrustValue::new(42u64, 1000);
        assert_eq!(tv.value, 42);
        assert_eq!(tv.trust, 1000);

        let mapped = tv.map(|v| v * 2);
        assert_eq!(mapped.value, 84);
        assert_eq!(mapped.trust, 1000);
    }

    #[test]
    fn test_time_range() {
        let range = TimeRange::new(100, 200);
        assert!(range.contains(150));
        assert!(!range.contains(50));
        assert!(!range.contains(250));

        let other = TimeRange::new(150, 250);
        assert!(range.overlaps(&other));

        let disjoint = TimeRange::new(300, 400);
        assert!(!range.overlaps(&disjoint));
    }

    #[test]
    fn test_query_constraints() {
        let constraints = QueryConstraints::new()
            .with_time_range(TimeRange::new(100, 200))
            .with_trust_min(500)
            .with_max_results(10);

        assert!(constraints.matches_time(150, 160));
        assert!(!constraints.matches_time(50, 60));
        assert!(constraints.matches_trust(600));
        assert!(!constraints.matches_trust(400));
    }

    #[test]
    fn test_evidence_ref() {
        let atom_id = [1u8; 32];
        let ev = EvidenceRef::new(atom_id, SectionKind::EVIDENCE, 100, 50, 1000);

        assert_eq!(ev.atom_id, atom_id);
        assert_eq!(ev.section_kind, SectionKind::EVIDENCE);
        assert_eq!(ev.offset, 100);
        assert_eq!(ev.length, 50);
    }

    #[test]
    fn test_edge_ref() {
        let edge = EdgeRef::new(1, 2, EdgeType::CAUSES, 800);

        assert_eq!(edge.src_node, 1);
        assert_eq!(edge.dst_node, 2);
        assert_eq!(edge.edge_type, EdgeType::CAUSES);
        assert_eq!(edge.confidence, 800);
    }
}
