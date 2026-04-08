//! EVIDENCE section implementation for MemoryX SKF-1.1
//!
//! This module provides the EVIDENCE section (0x06) of AtomBody:
//! - Provenance information for atoms
//! - Format: u32 evidence_count followed by evidence records:
//!   * u32 evidence_kind
//!   * u32 source_sym
//!   * u32 method_sym
//!   * u32 timestamp_unix_ns
//!   * u32 confidence_q (0-65535)
//!   * u32 flags

use super::CasError;
use crate::utils::crc32;
use std::fmt;

/// Evidence kind enum for different types of evidence
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EvidenceKind {
    /// Unknown evidence type
    UNKNOWN = 0,
    /// Citation from literature
    CITATION = 1,
    /// Measurement data
    MEASUREMENT = 2,
    /// Expert inference
    EXPERT_INFERENCE = 3,
    /// Logical derivation
    LOGICAL_DERIVATION = 4,
    /// Statistical analysis
    STATISTICAL = 5,
    /// Experimental result
    EXPERIMENTAL = 6,
    /// Observation
    OBSERVATION = 7,
}

impl EvidenceKind {
    /// Convert from u32
    pub const fn from_u32(value: u32) -> Option<Self> {
        match value {
            0 => Some(EvidenceKind::UNKNOWN),
            1 => Some(EvidenceKind::CITATION),
            2 => Some(EvidenceKind::MEASUREMENT),
            3 => Some(EvidenceKind::EXPERT_INFERENCE),
            4 => Some(EvidenceKind::LOGICAL_DERIVATION),
            5 => Some(EvidenceKind::STATISTICAL),
            6 => Some(EvidenceKind::EXPERIMENTAL),
            7 => Some(EvidenceKind::OBSERVATION),
            _ => None,
        }
    }

    /// Convert to u32
    pub const fn to_u32(self) -> u32 {
        self as u32
    }
}

/// Error for invalid EvidenceKind conversion
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidEvidenceKind(pub u32);

/// Evidence record in the EVIDENCE section
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EvidenceRecord {
    /// Evidence kind/type
    pub evidence_kind: u32,
    /// Source symbol index (in SYMBOLS section)
    pub source_sym: u32,
    /// Method symbol index (in SYMBOLS section)
    pub method_sym: u32,
    /// Timestamp in unix nanoseconds
    pub timestamp_unix_ns: u32,
    /// Confidence quantized (0-65535)
    pub confidence_q: u32,
    /// Flags
    pub flags: u32,
}

/// Serialize an EvidenceRecord to bytes
pub fn serialize_evidence_record(record: &EvidenceRecord) -> [u8; 24] {
    let mut bytes = [0u8; 24];
    bytes[0..4].copy_from_slice(&record.evidence_kind.to_le_bytes());
    bytes[4..8].copy_from_slice(&record.source_sym.to_le_bytes());
    bytes[8..12].copy_from_slice(&record.method_sym.to_le_bytes());
    bytes[12..16].copy_from_slice(&record.timestamp_unix_ns.to_le_bytes());
    bytes[16..20].copy_from_slice(&record.confidence_q.to_le_bytes());
    bytes[20..24].copy_from_slice(&record.flags.to_le_bytes());
    bytes
}

/// Deserialize an EvidenceRecord from bytes
pub fn deserialize_evidence_record(bytes: &[u8]) -> Result<EvidenceRecord, CasError> {
    if bytes.len() < 24 {
        return Err(CasError::BufferTooSmall {
            expected: 24,
            actual: bytes.len(),
        });
    }

    let evidence_kind = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let source_sym = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let method_sym = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    let timestamp_unix_ns = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    let confidence_q = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let flags = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);

    Ok(EvidenceRecord {
        evidence_kind,
        source_sym,
        method_sym,
        timestamp_unix_ns,
        confidence_q,
        flags,
    })
}

impl EvidenceRecord {
    /// Create a new EvidenceRecord
    pub fn new(
        evidence_kind: EvidenceKind,
        source_sym: u32,
        method_sym: u32,
        timestamp_unix_ns: u32,
        confidence_q: u32,
        flags: u32,
    ) -> Self {
        Self {
            evidence_kind: evidence_kind.to_u32(),
            source_sym,
            method_sym,
            timestamp_unix_ns,
            confidence_q: confidence_q.min(65535),
            flags,
        }
    }

    /// Get the evidence kind as EvidenceKind enum
    pub fn get_evidence_kind(&self) -> Result<EvidenceKind, InvalidEvidenceKind> {
        EvidenceKind::from_u32(self.evidence_kind).ok_or(InvalidEvidenceKind(self.evidence_kind))
    }

    /// Get confidence as f64 (0.0 to 1.0)
    pub fn confidence(&self) -> f64 {
        self.confidence_q as f64 / 65535.0
    }

    /// Serialize to bytes
    pub fn to_bytes(&self) -> [u8; 24] {
        serialize_evidence_record(self)
    }

    /// Deserialize from bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CasError> {
        deserialize_evidence_record(bytes)
    }
}

impl fmt::Display for EvidenceRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Evidence({:?}, src={}, method={}, conf={:.3})",
            self.get_evidence_kind().unwrap_or(EvidenceKind::UNKNOWN),
            self.source_sym,
            self.method_sym,
            self.confidence()
        )
    }
}

/// EVIDENCE section for provenance information in Atom Body
///
/// Format:
/// - u32 evidence_count
/// - evidence_count * EvidenceRecord (24 bytes each)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvidenceSection {
    /// Vector of evidence records
    pub evidence: Vec<EvidenceRecord>,
}

impl EvidenceSection {
    /// Create a new empty Evidence section
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an evidence record to the section
    pub fn add_evidence(&mut self, evidence: EvidenceRecord) {
        self.evidence.push(evidence);
    }

    /// Get evidence by index
    pub fn get(&self, index: usize) -> Option<&EvidenceRecord> {
        self.evidence.get(index)
    }

    /// Get mutable evidence by index
    pub fn get_mut(&mut self, index: usize) -> Option<&mut EvidenceRecord> {
        self.evidence.get_mut(index)
    }

    /// Get number of evidence records
    pub fn len(&self) -> usize {
        self.evidence.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.evidence.is_empty()
    }

    /// Find evidence by kind
    pub fn find_by_kind(&self, kind: EvidenceKind) -> Vec<&EvidenceRecord> {
        let kind_val = kind.to_u32();
        self.evidence
            .iter()
            .filter(|e| e.evidence_kind == kind_val)
            .collect()
    }

    /// Find evidence by source
    pub fn find_by_source(&self, source_sym: u32) -> Vec<&EvidenceRecord> {
        self.evidence
            .iter()
            .filter(|e| e.source_sym == source_sym)
            .collect()
    }

    /// Calculate the serialized size in bytes
    pub fn serialized_size(&self) -> usize {
        4 + self.evidence.len() * 24 // evidence_count + evidence_count * EvidenceRecord
    }

    /// Serialize to bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.serialized_size());

        // Write evidence count
        bytes.extend_from_slice(&(self.evidence.len() as u32).to_le_bytes());

        // Write each EvidenceRecord
        for evidence in &self.evidence {
            bytes.extend_from_slice(&evidence.to_bytes());
        }

        bytes
    }

    /// Deserialize from bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CasError> {
        if bytes.len() < 4 {
            return Err(CasError::BufferTooSmall {
                expected: 4,
                actual: bytes.len(),
            });
        }

        let evidence_count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let expected_size = 4 + evidence_count * 24;

        if bytes.len() < expected_size {
            return Err(CasError::BufferTooSmall {
                expected: expected_size,
                actual: bytes.len(),
            });
        }

        let mut evidence = Vec::with_capacity(evidence_count);
        let mut offset = 4usize;

        for _ in 0..evidence_count {
            if offset + 24 > bytes.len() {
                return Err(CasError::BufferTooSmall {
                    expected: offset + 24,
                    actual: bytes.len(),
                });
            }

            let record = EvidenceRecord::from_bytes(&bytes[offset..offset + 24])?;
            evidence.push(record);
            offset += 24;
        }

        Ok(EvidenceSection { evidence })
    }

    /// Calculate CRC32 of the section data
    pub fn crc32(&self) -> u32 {
        crc32(&self.to_bytes())
    }
}

impl fmt::Display for EvidenceSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Evidence({} records)", self.evidence.len())
    }
}

#[cfg(test)]
mod tests {
    use super::{EvidenceKind, EvidenceRecord, EvidenceSection};

    #[test]
    fn test_evidence_record_new() {
        let evidence = EvidenceRecord::new(EvidenceKind::CITATION, 1, 2, 1234567890, 65535, 0);

        assert_eq!(evidence.evidence_kind, EvidenceKind::CITATION.to_u32());
        assert_eq!(evidence.source_sym, 1);
        assert_eq!(evidence.method_sym, 2);
        assert_eq!(evidence.timestamp_unix_ns, 1234567890);
        assert_eq!(evidence.confidence_q, 65535);
        assert_eq!(evidence.flags, 0);
    }

    #[test]
    fn test_evidence_record_serialization() {
        let evidence =
            EvidenceRecord::new(EvidenceKind::MEASUREMENT, 5, 10, 1234567890, 32768, 0x0001);

        let bytes = evidence.to_bytes();
        let restored = EvidenceRecord::from_bytes(&bytes).unwrap();

        assert_eq!(evidence, restored);
        assert_eq!(
            evidence.get_evidence_kind().unwrap(),
            restored.get_evidence_kind().unwrap()
        );
    }

    #[test]
    fn test_evidence_record_confidence() {
        let evidence = EvidenceRecord::new(EvidenceKind::CITATION, 1, 2, 1234567890, 32768, 0);

        // 32768 / 65535 ≈ 0.5
        let confidence = evidence.confidence();
        assert!((confidence - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_evidence_section_serialization() {
        let mut evidence = EvidenceSection::new();
        evidence.add_evidence(EvidenceRecord::new(
            EvidenceKind::CITATION,
            1,
            2,
            1234567890,
            65535,
            0,
        ));
        evidence.add_evidence(EvidenceRecord::new(
            EvidenceKind::MEASUREMENT,
            3,
            4,
            1234567891,
            32768,
            0x0001,
        ));
        evidence.add_evidence(EvidenceRecord::new(
            EvidenceKind::EXPERT_INFERENCE,
            5,
            6,
            1234567892,
            16384,
            0x0002,
        ));

        let bytes = evidence.to_bytes();
        let restored = EvidenceSection::from_bytes(&bytes).unwrap();

        assert_eq!(evidence.len(), restored.len());
        for i in 0..evidence.len() {
            assert_eq!(evidence.get(i), restored.get(i));
        }
        assert_eq!(evidence.crc32(), restored.crc32());
    }

    #[test]
    fn test_evidence_section_find_by_kind() {
        let mut evidence = EvidenceSection::new();
        evidence.add_evidence(EvidenceRecord::new(
            EvidenceKind::CITATION,
            1,
            2,
            1234567890,
            65535,
            0,
        ));
        evidence.add_evidence(EvidenceRecord::new(
            EvidenceKind::MEASUREMENT,
            3,
            4,
            1234567891,
            32768,
            0,
        ));
        evidence.add_evidence(EvidenceRecord::new(
            EvidenceKind::CITATION,
            5,
            6,
            1234567892,
            16384,
            0,
        ));

        let citations = evidence.find_by_kind(EvidenceKind::CITATION);
        assert_eq!(citations.len(), 2);

        let measurements = evidence.find_by_kind(EvidenceKind::MEASUREMENT);
        assert_eq!(measurements.len(), 1);
    }

    #[test]
    fn test_evidence_section_find_by_source() {
        let mut evidence = EvidenceSection::new();
        evidence.add_evidence(EvidenceRecord::new(
            EvidenceKind::CITATION,
            1,
            2,
            1234567890,
            65535,
            0,
        ));
        evidence.add_evidence(EvidenceRecord::new(
            EvidenceKind::MEASUREMENT,
            1,
            4,
            1234567891,
            32768,
            0,
        ));
        evidence.add_evidence(EvidenceRecord::new(
            EvidenceKind::CITATION,
            3,
            6,
            1234567892,
            16384,
            0,
        ));

        let from_source_1 = evidence.find_by_source(1);
        assert_eq!(from_source_1.len(), 2);

        let from_source_3 = evidence.find_by_source(3);
        assert_eq!(from_source_3.len(), 1);
    }
}
