//! CLAIMS section implementation for MemoryX SKF-1.1
//!
//! This module provides the CLAIMS section (0x03) of AtomBody:
//! - Bit-packed, columnar claim storage
//! - Format: u32 claim_count followed by claim records:
//!   * u16 subject_local (index in SYMBOLS)
//!   * u16 predicate_local (index in SYMBOLS)
//!   * u8 object_tag (type tag from ObjTag enum)
//!   * object_value (variable length based on object_tag)

use super::CasError;
use crate::store::ObjTag;
use crate::utils::crc32;
use std::fmt;

/// Claim record in the CLAIMS section
#[derive(Debug, Clone, PartialEq)]
pub struct ClaimRecord {
    /// Subject local index (in SYMBOLS section)
    pub subject_local: u16,
    /// Predicate local index (in SYMBOLS section)
    pub predicate_local: u16,
    /// Object tag type
    pub object_tag: ObjTag,
    /// Object value as bytes (interpret based on object_tag)
    pub object_value: Vec<u8>,
}

impl ClaimRecord {
    /// Create a new ClaimRecord with a null object
    pub fn new_null(subject_local: u16, predicate_local: u16) -> Self {
        Self {
            subject_local,
            predicate_local,
            object_tag: ObjTag::NULL,
            object_value: Vec::new(),
        }
    }

    /// Create a new ClaimRecord with a boolean object
    pub fn new_bool(subject_local: u16, predicate_local: u16, value: bool) -> Self {
        Self {
            subject_local,
            predicate_local,
            object_tag: ObjTag::BOOL,
            object_value: vec![if value { 1 } else { 0 }],
        }
    }

    /// Create a new ClaimRecord with an i64 object
    pub fn new_i64(subject_local: u16, predicate_local: u16, value: i64) -> Self {
        Self {
            subject_local,
            predicate_local,
            object_tag: ObjTag::I64,
            object_value: value.to_le_bytes().to_vec(),
        }
    }

    /// Create a new ClaimRecord with a u64 object
    pub fn new_u64(subject_local: u16, predicate_local: u16, value: u64) -> Self {
        Self {
            subject_local,
            predicate_local,
            object_tag: ObjTag::U64,
            object_value: value.to_le_bytes().to_vec(),
        }
    }

    /// Create a new ClaimRecord with an f64 object
    pub fn new_f64(subject_local: u16, predicate_local: u16, value: f64) -> Self {
        Self {
            subject_local,
            predicate_local,
            object_tag: ObjTag::F64,
            object_value: value.to_le_bytes().to_vec(),
        }
    }

    /// Create a new ClaimRecord with bytes object
    pub fn new_bytes(subject_local: u16, predicate_local: u16, value: &[u8]) -> Self {
        let mut object_value = Vec::with_capacity(4 + value.len());
        object_value.extend_from_slice(&(value.len() as u32).to_le_bytes());
        object_value.extend_from_slice(value);
        Self {
            subject_local,
            predicate_local,
            object_tag: ObjTag::BYTES,
            object_value,
        }
    }

    /// Create a new ClaimRecord with a symbol object
    pub fn new_sym(subject_local: u16, predicate_local: u16, sym_id: u32) -> Self {
        Self {
            subject_local,
            predicate_local,
            object_tag: ObjTag::SYM,
            object_value: sym_id.to_le_bytes().to_vec(),
        }
    }

    /// Create a new ClaimRecord with a reference object
    pub fn new_ref(subject_local: u16, predicate_local: u16, atom_id: [u8; 32]) -> Self {
        Self {
            subject_local,
            predicate_local,
            object_tag: ObjTag::REF,
            object_value: atom_id.to_vec(),
        }
    }

    /// Create a new ClaimRecord with a node number object
    pub fn new_nodenum(subject_local: u16, predicate_local: u16, node_num: u64) -> Self {
        Self {
            subject_local,
            predicate_local,
            object_tag: ObjTag::NODENUM,
            object_value: node_num.to_le_bytes().to_vec(),
        }
    }

    /// Get the object value as bool (if BOOL type)
    pub fn as_bool(&self) -> Option<bool> {
        if self.object_tag == ObjTag::BOOL && !self.object_value.is_empty() {
            Some(self.object_value[0] != 0)
        } else {
            None
        }
    }

    /// Get the object value as i64 (if I64 type)
    pub fn as_i64(&self) -> Option<i64> {
        if self.object_tag == ObjTag::I64 && self.object_value.len() == 8 {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&self.object_value);
            Some(i64::from_le_bytes(bytes))
        } else {
            None
        }
    }

    /// Get the object value as u64 (if U64 type)
    pub fn as_u64(&self) -> Option<u64> {
        if self.object_tag == ObjTag::U64 && self.object_value.len() == 8 {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&self.object_value);
            Some(u64::from_le_bytes(bytes))
        } else {
            None
        }
    }

    /// Get the object value as f64 (if F64 type)
    pub fn as_f64(&self) -> Option<f64> {
        if self.object_tag == ObjTag::F64 && self.object_value.len() == 8 {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&self.object_value);
            Some(f64::from_le_bytes(bytes))
        } else {
            None
        }
    }

    /// Get the object value as bytes (if BYTES type)
    pub fn as_bytes(&self) -> Option<&[u8]> {
        if self.object_tag == ObjTag::BYTES && self.object_value.len() >= 4 {
            let len = u32::from_le_bytes([
                self.object_value[0],
                self.object_value[1],
                self.object_value[2],
                self.object_value[3],
            ]) as usize;
            if self.object_value.len() >= 4 + len {
                Some(&self.object_value[4..4 + len])
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Get the object value as symbol ID (if SYM type)
    pub fn as_sym(&self) -> Option<u32> {
        if self.object_tag == ObjTag::SYM && self.object_value.len() == 4 {
            let mut bytes = [0u8; 4];
            bytes.copy_from_slice(&self.object_value);
            Some(u32::from_le_bytes(bytes))
        } else {
            None
        }
    }

    /// Get the object value as atom ID (if REF type)
    pub fn as_ref(&self) -> Option<[u8; 32]> {
        if self.object_tag == ObjTag::REF && self.object_value.len() == 32 {
            let mut atom_id = [0u8; 32];
            atom_id.copy_from_slice(&self.object_value);
            Some(atom_id)
        } else {
            None
        }
    }

    /// Get the object value as node number (if NODENUM type)
    pub fn as_nodenum(&self) -> Option<u64> {
        if self.object_tag == ObjTag::NODENUM && self.object_value.len() == 8 {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&self.object_value);
            Some(u64::from_le_bytes(bytes))
        } else {
            None
        }
    }

    /// Serialize to bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();

        // Write subject and predicate indices
        bytes.extend_from_slice(&self.subject_local.to_le_bytes());
        bytes.extend_from_slice(&self.predicate_local.to_le_bytes());

        // Write object tag
        bytes.push(self.object_tag.to_u8());

        // Write object value
        bytes.extend_from_slice(&self.object_value);

        bytes
    }

    /// Deserialize from bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CasError> {
        if bytes.len() < 5 {
            return Err(CasError::BufferTooSmall {
                expected: 5,
                actual: bytes.len(),
            });
        }

        let subject_local = u16::from_le_bytes([bytes[0], bytes[1]]);
        let predicate_local = u16::from_le_bytes([bytes[2], bytes[3]]);
        let object_tag_byte = bytes[4];

        let object_tag = ObjTag::from_u8(object_tag_byte)
            .ok_or(CasError::InvalidSectionKind(object_tag_byte as u32))?;

        let object_value = bytes[5..].to_vec();

        Ok(Self {
            subject_local,
            predicate_local,
            object_tag,
            object_value,
        })
    }
}

impl fmt::Display for ClaimRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Claim(subj={}, pred={}, obj_tag={:?})",
            self.subject_local, self.predicate_local, self.object_tag
        )
    }
}

/// CLAIMS section for bit-packed, columnar claim storage in Atom Body
///
/// Format:
/// - u32 claim_count
/// - claim_count * ClaimRecord (variable length)
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaimsSection {
    /// Vector of claim records
    pub claims: Vec<ClaimRecord>,
}

impl ClaimsSection {
    /// Create a new empty Claims section
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a claim to the section
    pub fn add_claim(&mut self, claim: ClaimRecord) {
        self.claims.push(claim);
    }

    /// Get claim by index
    pub fn get(&self, index: usize) -> Option<&ClaimRecord> {
        self.claims.get(index)
    }

    /// Get mutable claim by index
    pub fn get_mut(&mut self, index: usize) -> Option<&mut ClaimRecord> {
        self.claims.get_mut(index)
    }

    /// Get number of claims
    pub fn len(&self) -> usize {
        self.claims.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.claims.is_empty()
    }

    /// Find claims by subject
    pub fn find_by_subject(&self, subject_local: u16) -> Vec<&ClaimRecord> {
        self.claims
            .iter()
            .filter(|c| c.subject_local == subject_local)
            .collect()
    }

    /// Find claims by predicate
    pub fn find_by_predicate(&self, predicate_local: u16) -> Vec<&ClaimRecord> {
        self.claims
            .iter()
            .filter(|c| c.predicate_local == predicate_local)
            .collect()
    }

    /// Calculate the serialized size in bytes
    pub fn serialized_size(&self) -> usize {
        let mut size = 4; // claim_count
        for claim in &self.claims {
            size += 5; // subject_local + predicate_local + object_tag
            size += claim.object_value.len();
        }
        size
    }

    /// Serialize to bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.serialized_size());

        // Write claim count
        bytes.extend_from_slice(&(self.claims.len() as u32).to_le_bytes());

        // Write each ClaimRecord
        for claim in &self.claims {
            bytes.extend_from_slice(&claim.to_bytes());
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

        let claim_count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let mut claims = Vec::with_capacity(claim_count);
        let mut offset = 4usize;

        for _ in 0..claim_count {
            if offset >= bytes.len() {
                return Err(CasError::BufferTooSmall {
                    expected: offset + 5,
                    actual: bytes.len(),
                });
            }

            // Read subject_local, predicate_local, and object_tag
            if offset + 5 > bytes.len() {
                return Err(CasError::BufferTooSmall {
                    expected: offset + 5,
                    actual: bytes.len(),
                });
            }

            let subject_local = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
            let predicate_local = u16::from_le_bytes([bytes[offset + 2], bytes[offset + 3]]);
            let object_tag_byte = bytes[offset + 4];

            let object_tag = ObjTag::from_u8(object_tag_byte)
                .ok_or(CasError::InvalidSectionKind(object_tag_byte as u32))?;

            // Find the end of this claim record based on object_tag
            let object_value_start = offset + 5;
            let object_value_len = match object_tag {
                ObjTag::NULL => 0,
                ObjTag::BOOL => 1,
                ObjTag::I64 => 8,
                ObjTag::U64 => 8,
                ObjTag::F64 => 8,
                ObjTag::BYTES => {
                    if object_value_start + 4 > bytes.len() {
                        return Err(CasError::BufferTooSmall {
                            expected: object_value_start + 4,
                            actual: bytes.len(),
                        });
                    }
                    let len = u32::from_le_bytes([
                        bytes[object_value_start],
                        bytes[object_value_start + 1],
                        bytes[object_value_start + 2],
                        bytes[object_value_start + 3],
                    ]) as usize;
                    4 + len
                }
                ObjTag::SYM => 4,
                ObjTag::REF => 32,
                ObjTag::NODENUM => 8,
            };

            if object_value_start + object_value_len > bytes.len() {
                return Err(CasError::BufferTooSmall {
                    expected: object_value_start + object_value_len,
                    actual: bytes.len(),
                });
            }

            let object_value =
                bytes[object_value_start..object_value_start + object_value_len].to_vec();

            claims.push(ClaimRecord {
                subject_local,
                predicate_local,
                object_tag,
                object_value,
            });

            offset = object_value_start + object_value_len;
        }

        Ok(ClaimsSection { claims })
    }

    /// Calculate CRC32 of the section data
    pub fn crc32(&self) -> u32 {
        crc32(&self.to_bytes())
    }
}

impl fmt::Display for ClaimsSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Claims({} claims)", self.claims.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ObjTag;

    #[test]
    fn test_claim_record_new_null() {
        let claim = ClaimRecord::new_null(1, 2);

        assert_eq!(claim.subject_local, 1);
        assert_eq!(claim.predicate_local, 2);
        assert_eq!(claim.object_tag, ObjTag::NULL);
        assert!(claim.object_value.is_empty());
    }

    #[test]
    fn test_claim_record_new_bool() {
        let claim = ClaimRecord::new_bool(5, 10, true);

        assert_eq!(claim.subject_local, 5);
        assert_eq!(claim.predicate_local, 10);
        assert_eq!(claim.object_tag, ObjTag::BOOL);
        assert_eq!(claim.object_value.len(), 1);
        assert_eq!(claim.as_bool(), Some(true));
    }

    #[test]
    fn test_claim_record_new_i64() {
        let claim = ClaimRecord::new_i64(3, 7, -42);

        assert_eq!(claim.subject_local, 3);
        assert_eq!(claim.predicate_local, 7);
        assert_eq!(claim.object_tag, ObjTag::I64);
        assert_eq!(claim.object_value.len(), 8);
        assert_eq!(claim.as_i64(), Some(-42));
    }

    #[test]
    fn test_claim_record_new_u64() {
        let claim = ClaimRecord::new_u64(1, 2, 1234567890);

        assert_eq!(claim.subject_local, 1);
        assert_eq!(claim.predicate_local, 2);
        assert_eq!(claim.object_tag, ObjTag::U64);
        assert_eq!(claim.object_value.len(), 8);
        assert_eq!(claim.as_u64(), Some(1234567890));
    }

    #[test]
    fn test_claim_record_new_f64() {
        let claim = ClaimRecord::new_f64(8, 9, std::f64::consts::PI);

        assert_eq!(claim.subject_local, 8);
        assert_eq!(claim.predicate_local, 9);
        assert_eq!(claim.object_tag, ObjTag::F64);
        assert_eq!(claim.object_value.len(), 8);
        let value = claim.as_f64().unwrap();
        assert!((value - std::f64::consts::PI).abs() < f64::EPSILON);
    }

    #[test]
    fn test_claim_record_serialization() {
        let claim = ClaimRecord::new_u64(5, 10, 42);

        let bytes = claim.to_bytes();
        let restored = ClaimRecord::from_bytes(&bytes).unwrap();

        assert_eq!(claim, restored);
    }

    #[test]
    fn test_claims_section_serialization() {
        let mut claims = ClaimsSection::new();
        claims.add_claim(ClaimRecord::new_u64(1, 2, 42));
        claims.add_claim(ClaimRecord::new_f64(3, 4, 3.5));
        claims.add_claim(ClaimRecord::new_sym(5, 6, 7));

        let bytes = claims.to_bytes();
        let restored = ClaimsSection::from_bytes(&bytes).unwrap();

        assert_eq!(claims.len(), restored.len());
        for i in 0..claims.len() {
            assert_eq!(claims.get(i), restored.get(i));
        }
        assert_eq!(claims.crc32(), restored.crc32());
    }

    #[test]
    fn test_claims_section_find_by_subject() {
        let mut claims = ClaimsSection::new();
        claims.add_claim(ClaimRecord::new_u64(1, 2, 42));
        claims.add_claim(ClaimRecord::new_f64(3, 4, 4.5));
        claims.add_claim(ClaimRecord::new_sym(1, 5, 6));

        let from_subject_1 = claims.find_by_subject(1);
        assert_eq!(from_subject_1.len(), 2);

        let from_subject_3 = claims.find_by_subject(3);
        assert_eq!(from_subject_3.len(), 1);
    }

    #[test]
    fn test_claims_section_find_by_predicate() {
        let mut claims = ClaimsSection::new();
        claims.add_claim(ClaimRecord::new_u64(1, 2, 42));
        claims.add_claim(ClaimRecord::new_f64(3, 2, 5.5));
        claims.add_claim(ClaimRecord::new_sym(5, 6, 7));

        let with_predicate_2 = claims.find_by_predicate(2);
        assert_eq!(with_predicate_2.len(), 2);

        let with_predicate_6 = claims.find_by_predicate(6);
        assert_eq!(with_predicate_6.len(), 1);
    }
}
