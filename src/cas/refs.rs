//! REFS section implementation for MemoryX SKF-1.1
//!
//! This module provides the REFS section (0x02) of AtomBody:
//! - Reference table for AtomId references
//! - Format: u32 ref_count followed by ref_count AtomIds (32 bytes each)

use super::CasError;
use crate::cas::AtomId;
use crate::utils::crc32;

/// REFS section for AtomId references in Atom Body
///
/// Format:
/// - u32 ref_count
/// - ref_count * AtomId (32 bytes each)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefsSection {
    /// Vector of referenced AtomIds
    pub refs: Vec<AtomId>,
}

impl RefsSection {
    /// Create a new empty Refs section
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a reference to the refs table
    pub fn add_ref(&mut self, atom_id: AtomId) {
        self.refs.push(atom_id);
    }

    /// Get reference by index
    pub fn get(&self, index: usize) -> Option<&AtomId> {
        self.refs.get(index)
    }

    /// Get mutable reference by index
    pub fn get_mut(&mut self, index: usize) -> Option<&mut AtomId> {
        self.refs.get_mut(index)
    }

    /// Get number of references
    pub fn len(&self) -> usize {
        self.refs.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.refs.is_empty()
    }

    /// Calculate the serialized size in bytes
    pub fn serialized_size(&self) -> usize {
        4 + self.refs.len() * 32 // ref_count + ref_count * AtomId
    }

    /// Serialize to bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.serialized_size());

        // Write reference count
        bytes.extend_from_slice(&(self.refs.len() as u32).to_le_bytes());

        // write each AtomId
        for atom_id in &self.refs {
            bytes.extend_from_slice(atom_id);
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

        let ref_count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let expected_size = 4 + ref_count * 32;

        if bytes.len() < expected_size {
            return Err(CasError::BufferTooSmall {
                expected: expected_size,
                actual: bytes.len(),
            });
        }

        let mut refs = Vec::with_capacity(ref_count);
        let mut offset = 4usize;

        for _ in 0..ref_count {
            if offset + 32 > bytes.len() {
                return Err(CasError::BufferTooSmall {
                    expected: offset + 32,
                    actual: bytes.len(),
                });
            }

            let mut atom_id = [0u8; 32];
            atom_id.copy_from_slice(&bytes[offset..offset + 32]);
            refs.push(atom_id);
            offset += 32;
        }

        Ok(RefsSection { refs })
    }

    /// Calculate CRC32 of the section data
    pub fn crc32(&self) -> u32 {
        crc32(&self.to_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::hex_decode;

    #[test]
    fn test_refs_section_add_ref() {
        let mut refs = RefsSection::new();
        let atom_id1 =
            hex_decode("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff").unwrap();
        let atom_id2 =
            hex_decode("ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100").unwrap();

        refs.add_ref(atom_id1);
        refs.add_ref(atom_id2);
        refs.add_ref(atom_id1); // duplicate

        assert_eq!(refs.len(), 3);
        assert_eq!(refs.get(0), Some(&atom_id1));
        assert_eq!(refs.get(1), Some(&atom_id2));
        assert_eq!(refs.get(2), Some(&atom_id1));
    }

    #[test]
    fn test_refs_section_serialization() {
        let mut refs = RefsSection::new();
        let atom_id1 =
            hex_decode("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff").unwrap();
        let atom_id2 =
            hex_decode("ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100").unwrap();

        refs.add_ref(atom_id1);
        refs.add_ref(atom_id2);

        let bytes = refs.to_bytes();
        let restored = RefsSection::from_bytes(&bytes).unwrap();

        assert_eq!(refs.len(), restored.len());
        assert_eq!(refs.get(0), restored.get(0));
        assert_eq!(refs.get(1), restored.get(1));
        assert_eq!(refs.crc32(), restored.crc32());
    }

    #[test]
    fn test_refs_section_from_bytes() {
        let atom_id1 =
            hex_decode("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff").unwrap();
        let atom_id2 =
            hex_decode("ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100").unwrap();

        // Manually create the byte representation
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(2u32).to_le_bytes()); // ref_count = 2
        bytes.extend_from_slice(&atom_id1);
        bytes.extend_from_slice(&atom_id2);

        let refs = RefsSection::from_bytes(&bytes).unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs.get(0), Some(&atom_id1));
        assert_eq!(refs.get(1), Some(&atom_id2));
    }
}
