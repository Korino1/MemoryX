//! CAS Integrity Verifier for MemoryX SKF-1.1
//!
//! Implements full integrity verification as required by SKF-1.1 Section 8.2:
//! - Record header validation (magic, version, lengths, offsets)
//! - Header CRC validation
//! - Body CRC validation
//! - Content-address identity verification (BLAKE3)
//! - Section table bounds validation (overflow-safe)
//! - Section CRC validation
//!
//! # Safety
//! - All bounds checking performed before unsafe operations
//! - Overflow-safe arithmetic for offset/length calculations
//! - No assumptions about data integrity

use std::fmt;

use super::{AtomBodyHeader, AtomId, CasError, RecordHeader, SectionDesc, SectionKind};
use crate::cas::canonical::compute_atom_id_from_payload;
use crate::utils::crc32;

// ============================================================================
// Integrity Verification Error
// ============================================================================

/// Detailed integrity verification error
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrityError {
    /// Record header validation failed
    HeaderInvalid { reason: String },

    /// Header CRC mismatch
    HeaderCrcMismatch { expected: u32, computed: u32 },

    /// Invalid magic number
    InvalidMagic { expected: u32, found: u32 },

    /// Invalid format version
    InvalidVersion { expected: u16, found: u16 },

    /// Body CRC mismatch
    BodyCrcMismatch { expected: u32, computed: u32 },

    /// Body length mismatch
    BodyLengthMismatch { header_len: u64, actual_len: usize },

    /// Content-address identity mismatch (SKF-1.1 critical)
    CanonicalIdentityMismatch {
        stored_id: AtomId,
        computed_id: AtomId,
    },

    /// Atom body header validation failed
    BodyHeaderInvalid { reason: String },

    /// Section table bounds violation (overflow or truncation)
    SectionBoundsViolation {
        section_index: u32,
        offset: u64,
        length: u64,
        body_len: u64,
        reason: String,
    },

    /// Section CRC mismatch
    SectionCrcMismatch {
        section_index: u32,
        section_kind: SectionKind,
        expected: u32,
        computed: u32,
    },

    /// Invalid section kind
    InvalidSectionKind { section_index: u32, kind_value: u32 },

    /// Missing required section (SKF-1.1 Section 3.2.1)
    MissingRequiredSection { kind: SectionKind },

    /// Section count exceeds maximum
    TooManySections { count: u32, max: u32 },

    /// Section table offset overflow
    SectionTableOverflow {
        offset: u64,
        count: u32,
        body_len: u64,
    },

    /// Truncated record
    TruncatedRecord {
        expected_size: usize,
        actual_size: usize,
    },

    /// CAS I/O error
    IoError { reason: String },
}

impl fmt::Display for IntegrityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IntegrityError::HeaderInvalid { reason } => {
                write!(f, "Invalid record header: {}", reason)
            }
            IntegrityError::HeaderCrcMismatch { expected, computed } => {
                write!(
                    f,
                    "Header CRC mismatch: expected 0x{:08X}, computed 0x{:08X}",
                    expected, computed
                )
            }
            IntegrityError::InvalidMagic { expected, found } => {
                write!(
                    f,
                    "Invalid magic: expected 0x{:08X}, found 0x{:08X}",
                    expected, found
                )
            }
            IntegrityError::InvalidVersion { expected, found } => {
                write!(
                    f,
                    "Invalid version: expected 0x{:04X}, found 0x{:04X}",
                    expected, found
                )
            }
            IntegrityError::BodyCrcMismatch { expected, computed } => {
                write!(
                    f,
                    "Body CRC mismatch: expected 0x{:08X}, computed 0x{:08X}",
                    expected, computed
                )
            }
            IntegrityError::BodyLengthMismatch {
                header_len,
                actual_len,
            } => {
                write!(
                    f,
                    "Body length mismatch: header says {}, actual {}",
                    header_len, actual_len
                )
            }
            IntegrityError::CanonicalIdentityMismatch {
                stored_id,
                computed_id,
            } => {
                write!(
                    f,
                    "Content-address identity mismatch (CRITICAL): stored {:?}, computed {:?}",
                    stored_id, computed_id
                )
            }
            IntegrityError::BodyHeaderInvalid { reason } => {
                write!(f, "Invalid atom body header: {}", reason)
            }
            IntegrityError::SectionBoundsViolation {
                section_index,
                offset,
                length,
                body_len,
                reason,
            } => {
                write!(
                    f,
                    "Section {} bounds violation: offset={}, length={}, body_len={}, {}",
                    section_index, offset, length, body_len, reason
                )
            }
            IntegrityError::SectionCrcMismatch {
                section_index,
                section_kind,
                expected,
                computed,
            } => {
                write!(
                    f,
                    "Section {} ({:?}) CRC mismatch: expected 0x{:08X}, computed 0x{:08X}",
                    section_index, section_kind, expected, computed
                )
            }
            IntegrityError::InvalidSectionKind {
                section_index,
                kind_value,
            } => {
                write!(
                    f,
                    "Invalid section kind at index {}: 0x{:08X}",
                    section_index, kind_value
                )
            }
            IntegrityError::MissingRequiredSection { kind } => {
                write!(f, "Missing required section: {:?}", kind)
            }
            IntegrityError::TooManySections { count, max } => {
                write!(f, "Too many sections: {} (max {})", count, max)
            }
            IntegrityError::SectionTableOverflow {
                offset,
                count,
                body_len,
            } => {
                write!(
                    f,
                    "Section table overflow: offset={}, count={}, body_len={}",
                    offset, count, body_len
                )
            }
            IntegrityError::TruncatedRecord {
                expected_size,
                actual_size,
            } => {
                write!(
                    f,
                    "Truncated record: expected {} bytes, got {}",
                    expected_size, actual_size
                )
            }
            IntegrityError::IoError { reason } => {
                write!(f, "I/O error: {}", reason)
            }
        }
    }
}

impl std::error::Error for IntegrityError {}

// ============================================================================
// Verification Result
// ============================================================================

/// Detailed integrity verification result
#[derive(Debug, Clone)]
pub struct IntegrityReport {
    /// Atom ID being verified
    pub atom_id: AtomId,
    /// Verification passed
    pub valid: bool,
    /// List of errors found (empty if valid)
    pub errors: Vec<IntegrityError>,
    /// List of warnings (non-critical issues)
    pub warnings: Vec<String>,
    /// Sections verified
    pub sections_verified: u32,
    /// Body size in bytes
    pub body_size: u64,
    /// Canonical identity verified
    pub canonical_identity_verified: bool,
    /// CRC checks performed
    pub crc_checks_performed: u32,
}

impl IntegrityReport {
    /// Create a new integrity report
    #[inline]
    pub fn new(atom_id: AtomId) -> Self {
        IntegrityReport {
            atom_id,
            valid: true,
            errors: Vec::new(),
            warnings: Vec::new(),
            sections_verified: 0,
            body_size: 0,
            canonical_identity_verified: false,
            crc_checks_performed: 0,
        }
    }

    /// Add an error (marks report as invalid)
    #[inline]
    pub fn add_error(&mut self, error: IntegrityError) {
        self.valid = false;
        self.errors.push(error);
    }

    /// Add a warning (does not affect validity)
    #[inline]
    pub fn add_warning(&mut self, warning: String) {
        self.warnings.push(warning);
    }

    /// Check if verification passed
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.valid
    }

    /// Get error count
    #[inline]
    pub fn error_count(&self) -> usize {
        self.errors.len()
    }

    /// Get first error if any
    #[inline]
    pub fn first_error(&self) -> Option<&IntegrityError> {
        self.errors.first()
    }
}

// ============================================================================
// Integrity Verifier
// ============================================================================

/// CAS Integrity Verifier
///
/// Performs full integrity verification as per SKF-1.1 Section 8.2:
/// - Record header validation
/// - CRC validation (header and body)
/// - Content-address identity verification
/// - Section bounds validation
/// - Section CRC validation
pub struct IntegrityVerifier {
    /// Maximum allowed section count
    max_sections: u32,
    /// Verify canonical identity (BLAKE3)
    verify_canonical: bool,
    /// Verify section CRCs
    verify_section_crcs: bool,
}

impl Default for IntegrityVerifier {
    fn default() -> Self {
        IntegrityVerifier {
            max_sections: 256,
            verify_canonical: true,
            verify_section_crcs: true,
        }
    }
}

impl IntegrityVerifier {
    /// Create a new integrity verifier with default settings
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set maximum allowed section count
    #[inline]
    pub fn with_max_sections(mut self, max: u32) -> Self {
        self.max_sections = max;
        self
    }

    /// Enable/disable canonical identity verification
    #[inline]
    pub fn with_canonical_verification(mut self, enabled: bool) -> Self {
        self.verify_canonical = enabled;
        self
    }

    /// Enable/disable section CRC verification
    #[inline]
    pub fn with_section_crc_verification(mut self, enabled: bool) -> Self {
        self.verify_section_crcs = enabled;
        self
    }

    /// Verify a complete CAS record
    ///
    /// # Arguments
    /// - `atom_id`: Expected atom ID
    /// - `record_bytes`: Complete record bytes (header + body + CRC)
    ///
    /// # Returns
    /// - `Ok(IntegrityReport)`: Verification report with detailed results
    /// - `Err(IntegrityError)`: Fatal error during verification
    ///
    /// # Safety
    /// - All bounds checking performed before unsafe operations
    /// - No assumptions about data integrity
    pub fn verify_record(
        &self,
        atom_id: &AtomId,
        record_bytes: &[u8],
    ) -> Result<IntegrityReport, IntegrityError> {
        let mut report = IntegrityReport::new(*atom_id);

        // Step 1: Verify minimum size for record header
        if record_bytes.len() < RecordHeader::SIZE {
            report.add_error(IntegrityError::TruncatedRecord {
                expected_size: RecordHeader::SIZE,
                actual_size: record_bytes.len(),
            });
            return Ok(report);
        }

        // Step 2: Parse and validate record header
        let header = self.verify_record_header(&record_bytes[..RecordHeader::SIZE], &mut report)?;

        // Step 3: Verify body length
        let body_len = header.body_len as usize;
        let expected_record_size = RecordHeader::SIZE + body_len + 4; // header + body + CRC

        if record_bytes.len() < expected_record_size {
            report.add_error(IntegrityError::TruncatedRecord {
                expected_size: expected_record_size,
                actual_size: record_bytes.len(),
            });
            return Ok(report);
        }

        report.body_size = header.body_len;

        // Step 4: Extract body
        let body_start = RecordHeader::SIZE;
        let body_end = body_start + body_len;
        let body = &record_bytes[body_start..body_end];

        // Step 5: Verify body CRC
        let stored_body_crc = u32::from_le_bytes([
            record_bytes[body_end],
            record_bytes[body_end + 1],
            record_bytes[body_end + 2],
            record_bytes[body_end + 3],
        ]);

        let computed_body_crc = crc32(body);
        report.crc_checks_performed += 1;

        if stored_body_crc != computed_body_crc {
            report.add_error(IntegrityError::BodyCrcMismatch {
                expected: stored_body_crc,
                computed: computed_body_crc,
            });
            // Continue to find more errors
        }

        // Step 6: Verify canonical identity (SKF-1.1 critical)
        if self.verify_canonical {
            match compute_atom_id_from_payload(body) {
                Ok(computed_id) => {
                    if computed_id != header.atom_id {
                        report.add_error(IntegrityError::CanonicalIdentityMismatch {
                            stored_id: header.atom_id,
                            computed_id,
                        });
                    } else {
                        report.canonical_identity_verified = true;
                    }
                }
                Err(e) => {
                    report.add_error(IntegrityError::BodyHeaderInvalid {
                        reason: format!("Canonical identity computation failed: {:?}", e),
                    });
                }
            }
        }

        // Step 7: Verify atom body header
        self.verify_atom_body_header(body, &mut report)?;

        // Step 8: Verify sections
        self.verify_sections(body, &mut report)?;

        Ok(report)
    }

    /// Verify atom body only (without record header)
    ///
    /// # Arguments
    /// - `atom_id`: Expected atom ID
    /// - `body`: Atom body bytes
    ///
    /// # Returns
    /// - `Ok(IntegrityReport)`: Verification report
    /// - `Err(IntegrityError)`: Fatal error
    pub fn verify_body(
        &self,
        atom_id: &AtomId,
        body: &[u8],
    ) -> Result<IntegrityReport, IntegrityError> {
        let mut report = IntegrityReport::new(*atom_id);
        report.body_size = body.len() as u64;

        // Step 1: Verify canonical identity
        if self.verify_canonical {
            match compute_atom_id_from_payload(body) {
                Ok(computed_id) => {
                    if computed_id != *atom_id {
                        report.add_error(IntegrityError::CanonicalIdentityMismatch {
                            stored_id: *atom_id,
                            computed_id,
                        });
                    } else {
                        report.canonical_identity_verified = true;
                    }
                }
                Err(e) => {
                    report.add_error(IntegrityError::BodyHeaderInvalid {
                        reason: format!("Canonical identity computation failed: {:?}", e),
                    });
                }
            }
        }

        // Step 2: Verify atom body header
        self.verify_atom_body_header(body, &mut report)?;

        // Step 3: Verify sections
        self.verify_sections(body, &mut report)?;

        Ok(report)
    }

    /// Verify record header
    fn verify_record_header(
        &self,
        header_bytes: &[u8],
        report: &mut IntegrityReport,
    ) -> Result<RecordHeader, IntegrityError> {
        // Parse header
        let header = RecordHeader::from_bytes(header_bytes).map_err(|e| {
            let error = match e {
                CasError::BufferTooSmall { expected, actual } => IntegrityError::TruncatedRecord {
                    expected_size: expected,
                    actual_size: actual,
                },
                CasError::InvalidMagic { expected, found } => {
                    IntegrityError::InvalidMagic { expected, found }
                }
                CasError::CrcMismatch { expected, found } => IntegrityError::HeaderCrcMismatch {
                    expected,
                    computed: found,
                },
                _ => IntegrityError::HeaderInvalid {
                    reason: format!("{:?}", e),
                },
            };
            report.add_error(error.clone());
            error
        })?;

        report.crc_checks_performed += 1; // Header CRC was checked

        // Verify magic
        if header.magic != super::RECORD_MAGIC {
            report.add_error(IntegrityError::InvalidMagic {
                expected: super::RECORD_MAGIC,
                found: header.magic,
            });
        }

        // Verify version
        if header.format_ver != super::RECORD_FORMAT_VERSION {
            report.add_error(IntegrityError::InvalidVersion {
                expected: super::RECORD_FORMAT_VERSION,
                found: header.format_ver,
            });
        }

        Ok(header)
    }

    /// Verify atom body header
    fn verify_atom_body_header(
        &self,
        body: &[u8],
        report: &mut IntegrityReport,
    ) -> Result<(), IntegrityError> {
        if body.len() < AtomBodyHeader::SIZE {
            report.add_error(IntegrityError::BodyHeaderInvalid {
                reason: format!(
                    "Body too small for header: {} < {}",
                    body.len(),
                    AtomBodyHeader::SIZE
                ),
            });
            return Ok(());
        }

        let body_header =
            AtomBodyHeader::from_bytes(&body[..AtomBodyHeader::SIZE]).map_err(|e| {
                let error = IntegrityError::BodyHeaderInvalid {
                    reason: format!("{:?}", e),
                };
                report.add_error(error.clone());
                error
            })?;

        // Verify magic
        if body_header.body_magic != super::ATOM_MAGIC {
            report.add_error(IntegrityError::InvalidMagic {
                expected: super::ATOM_MAGIC,
                found: body_header.body_magic,
            });
        }

        // Verify section count
        if body_header.section_count > self.max_sections {
            report.add_error(IntegrityError::TooManySections {
                count: body_header.section_count,
                max: self.max_sections,
            });
        }

        Ok(())
    }

    /// Verify all sections
    fn verify_sections(
        &self,
        body: &[u8],
        report: &mut IntegrityReport,
    ) -> Result<(), IntegrityError> {
        if body.len() < AtomBodyHeader::SIZE {
            return Ok(());
        }

        // Parse body header to get section table location
        let body_header = match AtomBodyHeader::from_bytes(&body[..AtomBodyHeader::SIZE]) {
            Ok(h) => h,
            Err(_) => return Ok(()), // Already reported in body header verification
        };

        let section_count = body_header.section_count as usize;
        let section_table_off = body_header.section_table_off as usize;

        // Verify section table fits in body (overflow-safe)
        let section_table_size = match section_count.checked_mul(SectionDesc::SIZE) {
            Some(size) => size,
            None => {
                report.add_error(IntegrityError::SectionTableOverflow {
                    offset: body_header.section_table_off,
                    count: body_header.section_count,
                    body_len: body.len() as u64,
                });
                return Ok(());
            }
        };

        let section_table_end = match section_table_off.checked_add(section_table_size) {
            Some(end) => end,
            None => {
                report.add_error(IntegrityError::SectionTableOverflow {
                    offset: body_header.section_table_off,
                    count: body_header.section_count,
                    body_len: body.len() as u64,
                });
                return Ok(());
            }
        };

        if section_table_end > body.len() {
            report.add_error(IntegrityError::SectionTableOverflow {
                offset: body_header.section_table_off,
                count: body_header.section_count,
                body_len: body.len() as u64,
            });
            return Ok(());
        }

        // Track which required sections were found
        let mut found_sections: u32 = 0;

        // Verify each section
        for i in 0..section_count {
            let desc_offset = section_table_off + i * SectionDesc::SIZE;
            let desc_bytes = &body[desc_offset..desc_offset + SectionDesc::SIZE];

            let section_desc = match SectionDesc::from_bytes(desc_bytes) {
                Ok(d) => d,
                Err(e) => {
                    report.add_error(IntegrityError::HeaderInvalid {
                        reason: format!("Section {} descriptor parse failed: {:?}", i, e),
                    });
                    continue;
                }
            };

            // Validate section kind
            let section_kind = match section_desc.kind() {
                Some(k) => {
                    // Track required sections (SKF-1.1 Section 3.2.1)
                    // All 7 section kinds are required
                    match k {
                        SectionKind::SYMBOLS => found_sections |= 0x01,
                        SectionKind::REFS => found_sections |= 0x02,
                        SectionKind::CLAIMS => found_sections |= 0x04,
                        SectionKind::INVARIANTS => found_sections |= 0x08,
                        SectionKind::EDGES => found_sections |= 0x10,
                        SectionKind::EVIDENCE => found_sections |= 0x20,
                        SectionKind::META => found_sections |= 0x40,
                    }
                    k
                }
                None => {
                    report.add_error(IntegrityError::InvalidSectionKind {
                        section_index: i as u32,
                        kind_value: section_desc.section_kind,
                    });
                    continue;
                }
            };

            // Verify section bounds (overflow-safe)
            match self.verify_section_bounds(&section_desc, i as u32, body.len() as u64, report) {
                Ok(_) => {}
                Err(_) => continue, // Error already added
            }

            // Verify section CRC if enabled
            if self.verify_section_crcs {
                let section_start = section_desc.off as usize;
                let section_end = section_start + section_desc.len as usize;

                if section_end <= body.len() {
                    let section_data = &body[section_start..section_end];
                    let computed_crc = crc32(section_data);
                    report.crc_checks_performed += 1;

                    if computed_crc != section_desc.crc32 {
                        report.add_error(IntegrityError::SectionCrcMismatch {
                            section_index: i as u32,
                            section_kind,
                            expected: section_desc.crc32,
                            computed: computed_crc,
                        });
                    }
                }
            }

            report.sections_verified += 1;
        }

        // Verify all required sections present (SKF-1.1 Section 3.2.1)
        // Required: SYMBOLS, REFS, CLAIMS, INVARIANTS, EDGES, EVIDENCE, META (0x7F)
        if found_sections != 0x7F {
            for kind in [
                SectionKind::SYMBOLS,
                SectionKind::REFS,
                SectionKind::CLAIMS,
                SectionKind::INVARIANTS,
                SectionKind::EDGES,
                SectionKind::EVIDENCE,
                SectionKind::META,
            ] {
                let bit = match kind {
                    SectionKind::SYMBOLS => 0x01,
                    SectionKind::REFS => 0x02,
                    SectionKind::CLAIMS => 0x04,
                    SectionKind::INVARIANTS => 0x08,
                    SectionKind::EDGES => 0x10,
                    SectionKind::EVIDENCE => 0x20,
                    SectionKind::META => 0x40,
                };

                if (found_sections & bit) == 0 {
                    report.add_error(IntegrityError::MissingRequiredSection { kind });
                }
            }
        }

        Ok(())
    }

    /// Verify section bounds with overflow-safe arithmetic
    fn verify_section_bounds(
        &self,
        section: &SectionDesc,
        index: u32,
        body_len: u64,
        report: &mut IntegrityReport,
    ) -> Result<(), IntegrityError> {
        // Check for overflow in end calculation
        let section_end = match section.off.checked_add(section.len) {
            Some(end) => end,
            None => {
                report.add_error(IntegrityError::SectionBoundsViolation {
                    section_index: index,
                    offset: section.off,
                    length: section.len,
                    body_len,
                    reason: "Overflow in offset + length".to_string(),
                });
                return Err(IntegrityError::SectionBoundsViolation {
                    section_index: index,
                    offset: section.off,
                    length: section.len,
                    body_len,
                    reason: "Overflow".to_string(),
                });
            }
        };

        // Check if section fits within body
        if section_end > body_len {
            report.add_error(IntegrityError::SectionBoundsViolation {
                section_index: index,
                offset: section.off,
                length: section.len,
                body_len,
                reason: "Section extends beyond body".to_string(),
            });
            return Err(IntegrityError::SectionBoundsViolation {
                section_index: index,
                offset: section.off,
                length: section.len,
                body_len,
                reason: "Out of bounds".to_string(),
            });
        }

        Ok(())
    }
}

// ============================================================================
// Convenience Functions
// ============================================================================

/// Quick verification of an atom record
///
/// # Arguments
/// - `atom_id`: Expected atom ID
/// - `record_bytes`: Complete record bytes
///
/// # Returns
/// - `Ok(true)`: Record is valid
/// - `Ok(false)`: Record is corrupted (check report for details)
/// - `Err(IntegrityError)`: Fatal error during verification
#[inline]
pub fn verify_record(atom_id: &AtomId, record_bytes: &[u8]) -> Result<bool, IntegrityError> {
    let verifier = IntegrityVerifier::new();
    let report = verifier.verify_record(atom_id, record_bytes)?;
    Ok(report.is_valid())
}

/// Quick verification of an atom body
///
/// # Arguments
/// - `atom_id`: Expected atom ID
/// - `body`: Atom body bytes
///
/// # Returns
/// - `Ok(true)`: Body is valid
/// - `Ok(false)`: Body is corrupted
/// - `Err(IntegrityError)`: Fatal error
#[inline]
pub fn verify_body(atom_id: &AtomId, body: &[u8]) -> Result<bool, IntegrityError> {
    let verifier = IntegrityVerifier::new();
    let report = verifier.verify_body(atom_id, body)?;
    Ok(report.is_valid())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_integrity_report_new() {
        let atom_id = [1u8; 32];
        let report = IntegrityReport::new(atom_id);
        assert!(report.is_valid());
        assert_eq!(report.error_count(), 0);
        assert!(!report.canonical_identity_verified);
    }

    #[test]
    fn test_integrity_report_add_error() {
        let atom_id = [1u8; 32];
        let mut report = IntegrityReport::new(atom_id);
        report.add_error(IntegrityError::InvalidMagic {
            expected: 0,
            found: 1,
        });
        assert!(!report.is_valid());
        assert_eq!(report.error_count(), 1);
    }

    #[test]
    fn test_verifier_default() {
        let verifier = IntegrityVerifier::new();
        assert_eq!(verifier.max_sections, 256);
        assert!(verifier.verify_canonical);
        assert!(verifier.verify_section_crcs);
    }

    #[test]
    fn test_verify_truncated_record() {
        let verifier = IntegrityVerifier::new();
        let atom_id = [0u8; 32];
        let small_bytes = [0u8; 10];

        let report = verifier.verify_record(&atom_id, &small_bytes).unwrap();
        assert!(!report.is_valid());
        assert!(matches!(
            report.first_error(),
            Some(IntegrityError::TruncatedRecord { .. })
        ));
    }

    #[test]
    fn test_verify_body_too_small() {
        let verifier = IntegrityVerifier::new();
        let atom_id = [0u8; 32];
        let small_body = [0u8; 10];

        let report = verifier.verify_body(&atom_id, &small_body).unwrap();
        assert!(!report.is_valid());
    }
}
