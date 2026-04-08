//! META section implementation for MemoryX SKF-1.1
//!
//! This module provides the META section (0x07) of AtomBody:
//! - Metadata for atoms (trust, versions, timestamps, etc.)
//! - Format: u32 field_count followed by field records:
//!   * u16 field_kind
//!   * u16 value_tag
//!   * u32 value (u32, f32, sym_id, etc.)

use super::CasError;
use crate::utils::crc32;
use std::fmt;

/// Metadata field kind enum for different types of metadata
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetaFieldKind {
    /// Unknown field type
    UNKNOWN = 0,
    /// Trust score (f32)
    TRUST_SCORE = 1,
    /// Domain mask (u32)
    DOMAIN_MASK = 2,
    /// Version number (u32)
    VERSION = 3,
    /// Source ID (sym_id)
    SOURCE_ID = 4,
    /// Creation timestamp
    CREATED_AT = 5,
    /// Last modified timestamp
    MODIFIED_AT = 6,
    /// Valid from timestamp
    VALID_FROM = 7,
    /// Valid to timestamp
    VALID_TO = 8,
}

impl MetaFieldKind {
    /// Convert from u16
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            0 => Some(MetaFieldKind::UNKNOWN),
            1 => Some(MetaFieldKind::TRUST_SCORE),
            2 => Some(MetaFieldKind::DOMAIN_MASK),
            3 => Some(MetaFieldKind::VERSION),
            4 => Some(MetaFieldKind::SOURCE_ID),
            5 => Some(MetaFieldKind::CREATED_AT),
            6 => Some(MetaFieldKind::MODIFIED_AT),
            7 => Some(MetaFieldKind::VALID_FROM),
            8 => Some(MetaFieldKind::VALID_TO),
            _ => None,
        }
    }

    /// Convert to u16
    pub const fn to_u16(self) -> u16 {
        self as u16
    }
}

/// Metadata field value
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MetaValue {
    /// Unsigned 32-bit integer
    U32(u32),
    /// Float (f32)
    F32(f32),
    /// Symbol ID (reference to SYMBOLS section)
    SymId(u32),
    /// Unix timestamp (seconds)
    Timestamp(u32),
    /// Boolean flag
    Bool(bool),
}

/// Metadata field in the META section
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetaField {
    /// Field kind/type
    pub field_kind: u16,
    /// Value tag
    pub value_tag: u16,
    /// Value
    pub value: u32,
}

impl MetaField {
    /// Create a new MetaField
    pub fn new(field_kind: MetaFieldKind, value: MetaValue) -> Self {
        let (value_tag, value_u32) = match value {
            MetaValue::U32(v) => (0u16, v),
            MetaValue::F32(v) => (1u16, v.to_bits()),
            MetaValue::SymId(v) => (2u16, v),
            MetaValue::Timestamp(v) => (3u16, v),
            MetaValue::Bool(v) => (4u16, if v { 1 } else { 0 }),
        };

        Self {
            field_kind: field_kind.to_u16(),
            value_tag,
            value: value_u32,
        }
    }

    /// Get field kind as MetaFieldKind enum
    pub fn get_field_kind(&self) -> Option<MetaFieldKind> {
        MetaFieldKind::from_u16(self.field_kind)
    }

    /// Get value as MetaValue enum
    pub fn get_value(&self) -> MetaValue {
        match self.value_tag {
            0 => MetaValue::U32(self.value),
            1 => MetaValue::F32(f32::from_bits(self.value)),
            2 => MetaValue::SymId(self.value),
            3 => MetaValue::Timestamp(self.value),
            4 => MetaValue::Bool(self.value != 0),
            _ => MetaValue::U32(self.value),
        }
    }

    /// Serialize to bytes (8 bytes)
    pub fn to_bytes(&self) -> [u8; 8] {
        let mut bytes = [0u8; 8];
        bytes[0..2].copy_from_slice(&self.field_kind.to_le_bytes());
        bytes[2..4].copy_from_slice(&self.value_tag.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.value.to_le_bytes());
        bytes
    }

    /// Deserialize from bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CasError> {
        if bytes.len() < 8 {
            return Err(CasError::BufferTooSmall {
                expected: 8,
                actual: bytes.len(),
            });
        }

        let field_kind = u16::from_le_bytes([bytes[0], bytes[1]]);
        let value_tag = u16::from_le_bytes([bytes[2], bytes[3]]);
        let value = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);

        Ok(Self {
            field_kind,
            value_tag,
            value,
        })
    }
}

impl fmt::Display for MetaField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "MetaField({:?}, {:?})",
            self.get_field_kind().unwrap_or(MetaFieldKind::UNKNOWN),
            self.get_value()
        )
    }
}

/// META section for metadata in Atom Body
///
/// Format:
/// - u32 field_count
/// - field_count * MetaField (8 bytes each)
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MetaSection {
    /// Vector of metadata fields
    pub fields: Vec<MetaField>,
}

impl MetaSection {
    /// Create a new empty Meta section
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a metadata field to the section
    pub fn add_field(&mut self, field: MetaField) {
        self.fields.push(field);
    }

    /// Get field by index
    pub fn get(&self, index: usize) -> Option<&MetaField> {
        self.fields.get(index)
    }

    /// Get mutable field by index
    pub fn get_mut(&mut self, index: usize) -> Option<&mut MetaField> {
        self.fields.get_mut(index)
    }

    /// Get number of fields
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Find fields by kind
    pub fn find_by_kind(&self, kind: MetaFieldKind) -> Vec<&MetaField> {
        let kind_val = kind.to_u16();
        self.fields
            .iter()
            .filter(|f| f.field_kind == kind_val)
            .collect()
    }

    /// Calculate the serialized size in bytes
    pub fn serialized_size(&self) -> usize {
        4 + self.fields.len() * 8 // field_count + field_count * MetaField
    }

    /// Serialize to bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.serialized_size());

        // Write field count
        bytes.extend_from_slice(&(self.fields.len() as u32).to_le_bytes());

        // Write each MetaField
        for field in &self.fields {
            bytes.extend_from_slice(&field.to_bytes());
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

        let field_count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let expected_size = 4 + field_count * 8;

        if bytes.len() < expected_size {
            return Err(CasError::BufferTooSmall {
                expected: expected_size,
                actual: bytes.len(),
            });
        }

        let mut fields = Vec::with_capacity(field_count);
        let mut offset = 4usize;

        for _ in 0..field_count {
            if offset + 8 > bytes.len() {
                return Err(CasError::BufferTooSmall {
                    expected: offset + 8,
                    actual: bytes.len(),
                });
            }

            let field = MetaField::from_bytes(&bytes[offset..offset + 8])?;
            fields.push(field);
            offset += 8;
        }

        Ok(MetaSection { fields })
    }

    /// Calculate CRC32 of the section data
    pub fn crc32(&self) -> u32 {
        crc32(&self.to_bytes())
    }
}

impl fmt::Display for MetaSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Meta({} fields)", self.fields.len())
    }
}

#[cfg(test)]
mod tests {
    use super::{MetaField, MetaFieldKind, MetaSection, MetaValue};

    #[test]
    fn test_meta_field_new() {
        let field = MetaField::new(MetaFieldKind::TRUST_SCORE, MetaValue::F32(0.85));

        assert_eq!(field.field_kind, MetaFieldKind::TRUST_SCORE.to_u16());
        assert_eq!(field.value_tag, 1); // F32
        assert_eq!(field.get_value(), MetaValue::F32(0.85));
    }

    #[test]
    fn test_meta_field_serialization() {
        let field = MetaField::new(MetaFieldKind::DOMAIN_MASK, MetaValue::U32(0xFF));

        let bytes = field.to_bytes();
        let restored = MetaField::from_bytes(&bytes).unwrap();

        assert_eq!(field, restored);
        assert_eq!(
            field.get_field_kind().unwrap(),
            restored.get_field_kind().unwrap()
        );
        assert_eq!(field.get_value(), restored.get_value());
    }

    #[test]
    fn test_meta_section_serialization() {
        let mut meta = MetaSection::new();
        meta.add_field(MetaField::new(
            MetaFieldKind::TRUST_SCORE,
            MetaValue::F32(0.85),
        ));
        meta.add_field(MetaField::new(
            MetaFieldKind::DOMAIN_MASK,
            MetaValue::U32(0xFF),
        ));
        meta.add_field(MetaField::new(MetaFieldKind::VERSION, MetaValue::U32(3)));
        meta.add_field(MetaField::new(
            MetaFieldKind::SOURCE_ID,
            MetaValue::SymId(42),
        ));

        let bytes = meta.to_bytes();
        let restored = MetaSection::from_bytes(&bytes).unwrap();

        assert_eq!(meta.len(), restored.len());
        for i in 0..meta.len() {
            assert_eq!(meta.get(i), restored.get(i));
        }
        assert_eq!(meta.crc32(), restored.crc32());
    }

    #[test]
    fn test_meta_section_find_by_kind() {
        let mut meta = MetaSection::new();
        meta.add_field(MetaField::new(
            MetaFieldKind::TRUST_SCORE,
            MetaValue::F32(0.85),
        ));
        meta.add_field(MetaField::new(
            MetaFieldKind::DOMAIN_MASK,
            MetaValue::U32(0xFF),
        ));
        meta.add_field(MetaField::new(MetaFieldKind::VERSION, MetaValue::U32(3)));

        let trust_fields = meta.find_by_kind(MetaFieldKind::TRUST_SCORE);
        assert_eq!(trust_fields.len(), 1);

        let domain_fields = meta.find_by_kind(MetaFieldKind::DOMAIN_MASK);
        assert_eq!(domain_fields.len(), 1);

        let version_fields = meta.find_by_kind(MetaFieldKind::VERSION);
        assert_eq!(version_fields.len(), 1);
    }
}
