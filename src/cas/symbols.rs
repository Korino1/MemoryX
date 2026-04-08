//! SYMBOLS section implementation for MemoryX SKF-1.1
//!
//! This module provides the SYMBOLS section (0x01) of AtomBody:
//! - Local dictionary for string interning
//! - Format: u32 sym_count followed by sym_count records of:
//!   * u32 len
//!   * bytes[len] UTF-8 (NFC)
//!   * pad to 4 bytes

use super::CasError;
use crate::store::SymId;
use crate::utils::crc32;
use std::collections::HashMap;

/// SYMBOLS section for string interning in Atom Body
///
/// Format:
/// - u32 sym_count
/// - sym_count records:
///   * u32 len
///   * bytes[len] UTF-8 string
///   * pad to 4 byte boundary
#[derive(Debug, Clone, Default)]
pub struct SymbolsSection {
    /// Mapping from string to symbol ID
    pub string_to_sym: HashMap<String, SymId>,
    /// Mapping from symbol ID to string
    pub sym_to_string: Vec<String>,
}

impl SymbolsSection {
    /// Create a new empty Symbols section
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a string to the symbols table
    ///
    /// Returns the SymbolId for the string (existing or newly added)
    pub fn intern(&mut self, s: String) -> SymId {
        if let Some(&sym_id) = self.string_to_sym.get(&s) {
            return sym_id;
        }

        let sym_id = self.sym_to_string.len() as SymId;
        self.string_to_sym.insert(s.clone(), sym_id);
        self.sym_to_string.push(s);
        sym_id
    }

    /// Get string by symbol ID
    pub fn get(&self, sym_id: SymId) -> Option<&str> {
        self.sym_to_string.get(sym_id as usize).map(|s| s.as_str())
    }

    /// Get symbol ID by string
    pub fn find(&self, s: &str) -> Option<SymId> {
        self.string_to_sym.get(s).copied()
    }

    /// Get number of symbols
    pub fn len(&self) -> usize {
        self.sym_to_string.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.sym_to_string.is_empty()
    }

    /// Calculate the serialized size in bytes
    pub fn serialized_size(&self) -> usize {
        let mut size = 4; // sym_count
        for s in &self.sym_to_string {
            size += 4; // len
            size += s.len(); // UTF-8 bytes
            size += (4 - (s.len() % 4)) % 4; // padding to 4-byte boundary
        }
        size
    }

    /// Serialize to bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.serialized_size());

        // Write symbol count
        bytes.extend_from_slice(&(self.sym_to_string.len() as u32).to_le_bytes());

        // Write each string
        for s in &self.sym_to_string {
            let len = s.len() as u32;
            bytes.extend_from_slice(&len.to_le_bytes());
            bytes.extend_from_slice(s.as_bytes());

            // Add padding to 4-byte boundary
            let padding = (4 - (s.len() % 4)) % 4;
            if padding > 0 {
                bytes.extend_from_slice(&vec![0u8; padding]);
            }
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

        let sym_count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let mut offset = 4usize;

        let mut string_to_sym = HashMap::new();
        let mut sym_to_string = Vec::with_capacity(sym_count);

        for _i in 0..sym_count {
            if offset + 4 > bytes.len() {
                return Err(CasError::BufferTooSmall {
                    expected: offset + 4,
                    actual: bytes.len(),
                });
            }

            let len = u32::from_le_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ]) as usize;
            offset += 4;

            if offset + len > bytes.len() {
                return Err(CasError::BufferTooSmall {
                    expected: offset + len,
                    actual: bytes.len(),
                });
            }

            let s = String::from_utf8_lossy(&bytes[offset..offset + len]).to_string();
            offset += len;

            // Skip padding
            let padding = (4 - (len % 4)) % 4;
            offset += padding;

            let sym_id = sym_to_string.len() as SymId;
            string_to_sym.insert(s.clone(), sym_id);
            sym_to_string.push(s);
        }

        Ok(SymbolsSection {
            string_to_sym,
            sym_to_string,
        })
    }

    /// Calculate CRC32 of the section data
    pub fn crc32(&self) -> u32 {
        crc32(&self.to_bytes())
    }
}

impl PartialEq for SymbolsSection {
    fn eq(&self, other: &Self) -> bool {
        self.sym_to_string == other.sym_to_string
    }
}

impl Eq for SymbolsSection {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symbols_section_intern() {
        let mut symbols = SymbolsSection::new();
        let id1 = symbols.intern("hello".to_string());
        let id2 = symbols.intern("world".to_string());
        let id3 = symbols.intern("hello".to_string()); // duplicate

        assert_eq!(id1, 0);
        assert_eq!(id2, 1);
        assert_eq!(id3, 0); // should return existing ID
        assert_eq!(symbols.len(), 2);
    }

    #[test]
    fn test_symbols_section_get() {
        let mut symbols = SymbolsSection::new();
        let id = symbols.intern("test".to_string());
        assert_eq!(symbols.get(id), Some("test"));
        assert_eq!(symbols.get(999), None);
    }

    #[test]
    fn test_symbols_section_find() {
        let mut symbols = SymbolsSection::new();
        symbols.intern("hello".to_string());
        assert_eq!(symbols.find("hello"), Some(0));
        assert_eq!(symbols.find("world"), None);
    }

    #[test]
    fn test_symbols_section_serialization() {
        let mut symbols = SymbolsSection::new();
        symbols.intern("hello".to_string());
        symbols.intern("world".to_string());

        let bytes = symbols.to_bytes();
        let restored = SymbolsSection::from_bytes(&bytes).unwrap();

        assert_eq!(symbols.len(), restored.len());
        assert_eq!(symbols.get(0), restored.get(0));
        assert_eq!(symbols.get(1), restored.get(1));
        assert_eq!(symbols.crc32(), restored.crc32());
    }
}
