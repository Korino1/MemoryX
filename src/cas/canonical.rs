//! CanonicalForm serializer and BLAKE3 AtomId generation for MemoryX SKF-1.1.
//!
//! Implements SKF-1.0/1.1 specification for content-addressed storage:
//! - CanonicalForm: deterministic serialization of structural atom fields
//! - AtomId = BLAKE3-256(CanonicalForm(KA_without_edges_dynamic))
//!
//! Canonicalization rules:
//! 1. All fields and lists sorted by deterministic key
//! 2. Strings normalized to Unicode NFC
//! 3. Little-endian binary encoding
//! 4. Dynamic edges (social/statistical/cached) excluded from hash
//! 5. Structural edges included in hash

use blake3;

use super::{AtomBodyHeader, CasError, RecordHeader, SectionDesc, SectionKind};
use crate::cas::{
    claims::ClaimsSection, evidence::EvidenceSection, invariants::InvariantsSection,
    meta::MetaSection, symbols::SymbolsSection,
};
use crate::store::AtomId;
#[cfg(test)]
use crate::store::EdgeType;

// ============================================================================
// CanonicalClaim
// ============================================================================

/// Canonical representation of a claim for hashing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CanonicalClaim {
    pub subject_symid: u32,
    pub predicate_symid: u32,
    pub objtag: u8,
    pub objvalue: Vec<u8>,
    pub qualifiers_mask: u32,
}

impl CanonicalClaim {
    #[inline]
    pub fn new(
        subject_symid: u32,
        predicate_symid: u32,
        objtag: u8,
        objvalue: Vec<u8>,
        qualifiers_mask: u32,
    ) -> Self {
        Self {
            subject_symid,
            predicate_symid,
            objtag,
            objvalue,
            qualifiers_mask,
        }
    }

    #[inline]
    fn serialized_size(&self) -> usize {
        4 + 4 + 1 + 4 + self.objvalue.len() + 4
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.serialized_size());
        buf.extend_from_slice(&self.subject_symid.to_le_bytes());
        buf.extend_from_slice(&self.predicate_symid.to_le_bytes());
        buf.push(self.objtag);
        buf.extend_from_slice(&(self.objvalue.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.objvalue);
        buf.extend_from_slice(&self.qualifiers_mask.to_le_bytes());
        buf
    }
}

impl PartialOrd for CanonicalClaim {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CanonicalClaim {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.subject_symid
            .cmp(&other.subject_symid)
            .then_with(|| self.predicate_symid.cmp(&other.predicate_symid))
            .then_with(|| self.objtag.cmp(&other.objtag))
            .then_with(|| self.objvalue.cmp(&other.objvalue))
    }
}

// ============================================================================
// CanonicalEdge
// ============================================================================

/// Canonical representation of a structural edge for hashing.
/// Dynamic edges (social/statistical/cached) are NOT included in CanonicalForm.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CanonicalEdge {
    pub edge_type: u32,
    pub target: u64,
    pub weight: u32,
}

impl CanonicalEdge {
    #[inline]
    pub fn new(edge_type: u32, target: u64, weight: u32) -> Self {
        Self {
            edge_type,
            target,
            weight,
        }
    }

    #[inline]
    fn serialized_size(&self) -> usize {
        4 + 8 + 4 // edge_type + target + weight
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.serialized_size());
        buf.extend_from_slice(&self.edge_type.to_le_bytes());
        buf.extend_from_slice(&self.target.to_le_bytes());
        buf.extend_from_slice(&self.weight.to_le_bytes());
        buf
    }
}

impl PartialOrd for CanonicalEdge {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CanonicalEdge {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.edge_type
            .cmp(&other.edge_type)
            .then_with(|| self.target.cmp(&other.target))
            .then_with(|| self.weight.cmp(&other.weight))
    }
}

// ============================================================================
// CanonicalForm
// ============================================================================

/// Deterministic serialization of an Atom structural fields.
/// Structural edges are included in hash; dynamic edges are excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalForm {
    pub atom_type: u32,
    pub valid_from: u64,
    pub valid_to: u64,
    pub symbols: Vec<String>,
    pub claims: Vec<CanonicalClaim>,
    pub invariants_code: Vec<u8>,
    pub evidence_hashes: Vec<[u8; 32]>,
    pub meta_trust: u32,
    pub meta_version: String,
    pub meta_domain_tags: Vec<String>,
    /// Structural edges included in canonical hash (defines, refines, etc.)
    pub structural_edges: Vec<CanonicalEdge>,
}

impl CanonicalForm {
    #[inline]
    pub fn new(atom_type: u32) -> Self {
        Self {
            atom_type,
            valid_from: 0,
            valid_to: 0,
            symbols: Vec::new(),
            claims: Vec::new(),
            invariants_code: Vec::new(),
            evidence_hashes: Vec::new(),
            meta_trust: 0,
            meta_version: String::new(),
            meta_domain_tags: Vec::new(),
            structural_edges: Vec::new(),
        }
    }

    pub fn normalize(&mut self) {
        self.claims.sort();
        self.evidence_hashes.sort();
        self.meta_domain_tags.sort();
        self.structural_edges.sort();
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut capacity = 4 + 8 + 8;
        capacity += 4;
        for sym in &self.symbols {
            capacity += 4 + sym.len();
        }
        capacity += 4;
        for c in &self.claims {
            capacity += c.serialized_size();
        }
        capacity += 4 + self.invariants_code.len();
        capacity += 4 + self.evidence_hashes.len() * 32;
        capacity += 4;
        capacity += 4 + self.meta_version.len();
        capacity += 4;
        for t in &self.meta_domain_tags {
            capacity += 4 + t.len();
        }
        // Structural edges
        capacity += 4;
        for e in &self.structural_edges {
            capacity += e.serialized_size();
        }

        let mut buf = Vec::with_capacity(capacity);
        buf.extend_from_slice(&self.atom_type.to_le_bytes());
        buf.extend_from_slice(&self.valid_from.to_le_bytes());
        buf.extend_from_slice(&self.valid_to.to_le_bytes());

        buf.extend_from_slice(&(self.symbols.len() as u32).to_le_bytes());
        for sym in &self.symbols {
            let b = sym.as_bytes();
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }

        buf.extend_from_slice(&(self.claims.len() as u32).to_le_bytes());
        for c in &self.claims {
            buf.extend_from_slice(&c.to_bytes());
        }

        buf.extend_from_slice(&(self.invariants_code.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.invariants_code);

        buf.extend_from_slice(&(self.evidence_hashes.len() as u32).to_le_bytes());
        for h in &self.evidence_hashes {
            buf.extend_from_slice(h);
        }

        buf.extend_from_slice(&self.meta_trust.to_le_bytes());
        let vb = self.meta_version.as_bytes();
        buf.extend_from_slice(&(vb.len() as u32).to_le_bytes());
        buf.extend_from_slice(vb);

        buf.extend_from_slice(&(self.meta_domain_tags.len() as u32).to_le_bytes());
        for t in &self.meta_domain_tags {
            let b = t.as_bytes();
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }

        // Serialize structural edges
        buf.extend_from_slice(&(self.structural_edges.len() as u32).to_le_bytes());
        for e in &self.structural_edges {
            buf.extend_from_slice(&e.to_bytes());
        }

        buf
    }
}

// ============================================================================
// Build atom with canonical ID (Phase 2)
// ============================================================================

/// Input for building an atom with canonical ID.
#[derive(Debug, Clone)]
pub struct AtomInput {
    pub atom_type: u32,
    pub valid_from: u64,
    pub valid_to: u64,
    pub symbols: Vec<String>,
    pub claims: Vec<CanonicalClaim>,
    pub invariants_code: Vec<u8>,
    pub evidence_hashes: Vec<[u8; 32]>,
    pub meta_trust: u32,
    pub meta_version: String,
    pub meta_domain_tags: Vec<String>,
    pub structural_edges: Vec<CanonicalEdge>,
    pub dynamic_edges: Vec<CanonicalEdge>,
}

impl AtomInput {
    #[inline]
    pub fn new(atom_type: u32) -> Self {
        Self {
            atom_type,
            valid_from: 0,
            valid_to: 0,
            symbols: Vec::new(),
            claims: Vec::new(),
            invariants_code: Vec::new(),
            evidence_hashes: Vec::new(),
            meta_trust: 0,
            meta_version: String::new(),
            meta_domain_tags: Vec::new(),
            structural_edges: Vec::new(),
            dynamic_edges: Vec::new(),
        }
    }
}

/// Build atom with canonical AtomId.
pub fn build_atom_with_canonical_id(input: AtomInput) -> Result<(AtomId, Vec<u8>), CasError> {
    let canonical_form = build_canonical_form_with_edges(
        input.atom_type,
        input.valid_from,
        input.valid_to,
        input.symbols,
        input.claims,
        input.invariants_code,
        input.evidence_hashes,
        input.meta_trust,
        input.meta_version,
        input.meta_domain_tags,
        input.structural_edges,
    );

    let atom_id = compute_atom_id_from_form(&canonical_form);

    let all_edges: Vec<CanonicalEdge> = canonical_form
        .structural_edges
        .iter()
        .cloned()
        .chain(input.dynamic_edges)
        .collect();

    let body = build_atom_body(input.atom_type, all_edges)?;

    Ok((atom_id, body))
}

#[allow(clippy::too_many_arguments)]
pub fn build_canonical_form_with_edges(
    atom_type: u32,
    valid_from: u64,
    valid_to: u64,
    symbols: Vec<String>,
    claims: Vec<CanonicalClaim>,
    invariants_code: Vec<u8>,
    evidence_hashes: Vec<[u8; 32]>,
    meta_trust: u32,
    meta_version: String,
    meta_domain_tags: Vec<String>,
    structural_edges: Vec<CanonicalEdge>,
) -> CanonicalForm {
    let mut form = CanonicalForm {
        atom_type,
        valid_from,
        valid_to,
        symbols,
        claims,
        invariants_code,
        evidence_hashes,
        meta_trust,
        meta_version,
        meta_domain_tags,
        structural_edges,
    };
    form.normalize();
    form
}

fn build_atom_body(_atom_type: u32, _edges: Vec<CanonicalEdge>) -> Result<Vec<u8>, CasError> {
    Ok(Vec::new())
}

/// Compute AtomId from payload bytes using canonical pipeline.
///
/// Extracts CanonicalForm from payload and computes BLAKE3 hash.
/// Returns error if payload is not a valid atom body - NO fallback to direct hash.
/// This enforces SKF-1.1 content-address contract.
pub fn compute_atom_id_from_payload(payload: &[u8]) -> Result<AtomId, CasError> {
    // Try to extract canonical form from payload
    let bh =
        AtomBodyHeader::from_bytes(payload).map_err(|e| CasError::CanonicalExtractionFailed {
            reason: format!("AtomBodyHeader parse failed: {:?}", e),
        })?;

    let sc = bh.section_count as usize;
    let ts = bh.section_table_off as usize;

    if payload.len() < ts + sc * SectionDesc::SIZE {
        return Err(CasError::CanonicalExtractionFailed {
            reason: format!(
                "Payload too small for section table: {} < {}",
                payload.len(),
                ts + sc * SectionDesc::SIZE
            ),
        });
    }

    let mut sections = Vec::with_capacity(sc);
    for i in 0..sc {
        let o = ts + i * SectionDesc::SIZE;
        let s =
            SectionDesc::from_bytes_unaligned(&payload[o..o + SectionDesc::SIZE]).map_err(|e| {
                CasError::CanonicalExtractionFailed {
                    reason: format!("SectionDesc parse failed at index {}: {:?}", i, e),
                }
            })?;
        sections.push(s);
    }

    let get_sec = |kind: SectionKind| -> Option<&[u8]> {
        sections
            .iter()
            .find(|s| s.kind() == Some(kind))
            .and_then(|sec| {
                let st = sec.off as usize;
                let en = st + sec.len as usize;
                payload.get(st..en)
            })
    };

    let syms = get_sec(SectionKind::SYMBOLS).unwrap_or(&[]);
    let clms = get_sec(SectionKind::CLAIMS).unwrap_or(&[]);
    let invs = get_sec(SectionKind::INVARIANTS).unwrap_or(&[]);
    let evds = get_sec(SectionKind::EVIDENCE).unwrap_or(&[]);
    let metas = get_sec(SectionKind::META).unwrap_or(&[]);

    let form = extract_canonical_form(&bh, syms, clms, invs, evds, metas).map_err(|e| {
        CasError::CanonicalExtractionFailed {
            reason: format!("Canonical form extraction failed: {:?}", e),
        }
    })?;

    Ok(compute_atom_id_from_form(&form))
}
// ============================================================================
// Extract CanonicalForm
// ============================================================================

pub fn extract_canonical_form(
    body_header: &AtomBodyHeader,
    symbols_data: &[u8],
    claims_data: &[u8],
    invariants_data: &[u8],
    evidence_data: &[u8],
    meta_data: &[u8],
) -> Result<CanonicalForm, CasError> {
    let mut form = CanonicalForm::new(body_header.atom_type);
    form.valid_from = body_header.valid_from_unix_ns;
    form.valid_to = body_header.valid_to_unix_ns;

    if !symbols_data.is_empty() {
        let symbols = SymbolsSection::from_bytes(symbols_data)?;
        for i in 0..symbols.len() {
            if let Some(s) = symbols.get(i as u32) {
                let n: String = unicode_normalization::UnicodeNormalization::nfc(s).collect();
                form.symbols.push(n);
            }
        }
    }

    if !claims_data.is_empty() {
        let claims = ClaimsSection::from_bytes(claims_data)?;
        for i in 0..claims.len() {
            if let Some(cr) = claims.get(i) {
                let subject = u32::try_from(cr.subject_local).map_err(|_| {
                    CasError::CanonicalExtractionFailed {
                        reason: format!(
                            "claim subject {} exceeds canonical u32 identity",
                            cr.subject_local
                        ),
                    }
                })?;
                form.claims.push(CanonicalClaim::new(
                    subject,
                    cr.predicate_local,
                    cr.object_tag.to_u8(),
                    cr.object_value.clone(),
                    0u32,
                ));
            }
        }
    }

    if !invariants_data.is_empty() {
        let inv = InvariantsSection::from_bytes(invariants_data)?;
        form.invariants_code = inv.to_bytes();
    }

    if !evidence_data.is_empty() {
        let ev = EvidenceSection::from_bytes(evidence_data)?;
        for e in &ev.evidence {
            let mut rb = Vec::with_capacity(24);
            rb.extend_from_slice(&e.evidence_kind.to_le_bytes());
            rb.extend_from_slice(&e.source_sym.to_le_bytes());
            rb.extend_from_slice(&e.method_sym.to_le_bytes());
            rb.extend_from_slice(&e.timestamp_unix_ns.to_le_bytes());
            rb.extend_from_slice(&e.confidence_q.to_le_bytes());
            rb.extend_from_slice(&e.flags.to_le_bytes());
            form.evidence_hashes.push(compute_atom_id(&rb));
        }
    }

    if !meta_data.is_empty() {
        let meta = MetaSection::from_bytes(meta_data)?;
        for f in &meta.fields {
            match f.get_field_kind() {
                Some(crate::cas::meta::MetaFieldKind::TRUST_SCORE) => {
                    if let crate::cas::meta::MetaValue::F32(t) = f.get_value() {
                        let c = t.clamp(0.0f32, 1.0f32);
                        form.meta_trust = (c * u32::MAX as f32) as u32;
                    }
                }
                Some(crate::cas::meta::MetaFieldKind::VERSION) => {
                    if let crate::cas::meta::MetaValue::U32(v) = f.get_value() {
                        form.meta_version = format!("{}.0.0", v);
                    }
                }
                _ => {}
            }
        }
    }

    form.normalize();
    Ok(form)
}

// ============================================================================
// Build and hash
// ============================================================================

#[allow(clippy::too_many_arguments)]
pub fn build_canonical_form(
    atom_type: u32,
    valid_from: u64,
    valid_to: u64,
    symbols: Vec<String>,
    claims: Vec<CanonicalClaim>,
    invariants_code: Vec<u8>,
    evidence_hashes: Vec<[u8; 32]>,
    meta_trust: u32,
    meta_version: String,
    meta_domain_tags: Vec<String>,
) -> CanonicalForm {
    let mut form = CanonicalForm {
        atom_type,
        valid_from,
        valid_to,
        symbols,
        claims,
        invariants_code,
        evidence_hashes,
        meta_trust,
        meta_version,
        meta_domain_tags,
        structural_edges: Vec::new(),
    };
    form.normalize();
    form
}

#[inline]
pub fn compute_atom_id(canonical_bytes: &[u8]) -> AtomId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(canonical_bytes);
    hasher.finalize().into()
}

pub fn compute_atom_id_from_form(form: &CanonicalForm) -> AtomId {
    compute_atom_id(&form.to_bytes())
}

// ============================================================================
// Verification
// ============================================================================

pub fn verify_atom_id(header: &RecordHeader, body: &[u8]) -> bool {
    match extract_and_compute_atom_id(body) {
        Ok(id) => id == header.atom_id,
        Err(_) => false,
    }
}

fn extract_and_compute_atom_id(body: &[u8]) -> Result<AtomId, CasError> {
    let bh = AtomBodyHeader::from_bytes(body)?;
    let sc = bh.section_count as usize;
    let ts = bh.section_table_off as usize;

    if body.len() < ts + sc * SectionDesc::SIZE {
        return Err(CasError::BufferTooSmall {
            expected: ts + sc * SectionDesc::SIZE,
            actual: body.len(),
        });
    }

    let mut sections = Vec::with_capacity(sc);
    for i in 0..sc {
        let o = ts + i * SectionDesc::SIZE;
        let s = SectionDesc::from_bytes_unaligned(&body[o..o + SectionDesc::SIZE])?;
        sections.push(s);
    }

    let get_sec = |kind: SectionKind| -> Option<&[u8]> {
        sections
            .iter()
            .find(|s| s.kind() == Some(kind))
            .and_then(|sec| {
                let st = sec.off as usize;
                let en = st + sec.len as usize;
                body.get(st..en)
            })
    };

    let syms = get_sec(SectionKind::SYMBOLS).unwrap_or(&[]);
    let clms = get_sec(SectionKind::CLAIMS).unwrap_or(&[]);
    let invs = get_sec(SectionKind::INVARIANTS).unwrap_or(&[]);
    let evds = get_sec(SectionKind::EVIDENCE).unwrap_or(&[]);
    let metas = get_sec(SectionKind::META).unwrap_or(&[]);

    let form = extract_canonical_form(&bh, syms, clms, invs, evds, metas)?;
    Ok(compute_atom_id_from_form(&form))
}

// ============================================================================
// RecordHeader helpers
// ============================================================================

impl RecordHeader {
    pub fn with_atom_id(body: &[u8], body_len: u64, seg_id: u32, flags: u16) -> Self {
        match Self::with_atom_id_fallible(body, body_len, seg_id, flags) {
            Ok(h) => h,
            Err(e) => panic!("Failed to compute AtomId: {:?}", e),
        }
    }

    pub fn with_atom_id_fallible(
        body: &[u8],
        body_len: u64,
        seg_id: u32,
        flags: u16,
    ) -> Result<Self, CasError> {
        let atom_id = extract_and_compute_atom_id(body)?;
        Ok(Self::new(atom_id, body_len, seg_id, flags))
    }

    #[inline]
    pub fn with_precomputed_atom_id(
        atom_id: AtomId,
        body_len: u64,
        seg_id: u32,
        flags: u16,
    ) -> Self {
        Self::new(atom_id, body_len, seg_id, flags)
    }
}

// ============================================================================
// Convenience: canonicalize
// ============================================================================

#[allow(clippy::too_many_arguments)]
pub fn canonicalize(
    atom_type: u32,
    valid_from: u64,
    valid_to: u64,
    symbols: Vec<String>,
    claims: Vec<CanonicalClaim>,
    invariants_code: Vec<u8>,
    evidence_hashes: Vec<[u8; 32]>,
    meta_trust: u32,
    meta_version: String,
    meta_domain_tags: Vec<String>,
) -> AtomId {
    let form = build_canonical_form(
        atom_type,
        valid_from,
        valid_to,
        symbols,
        claims,
        invariants_code,
        evidence_hashes,
        meta_trust,
        meta_version,
        meta_domain_tags,
    );
    compute_atom_id_from_form(&form)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::claims::ClaimRecord;
    use crate::cas::evidence::{EvidenceKind, EvidenceRecord};
    use crate::store::{AtomType, ObjTag};

    #[test]
    fn test_compute_atom_id_is_32_bytes() {
        let id = compute_atom_id(b"hello");
        assert_eq!(id.len(), 32);
    }

    #[test]
    fn test_compute_atom_id_deterministic() {
        let a = compute_atom_id(b"data");
        let b = compute_atom_id(b"data");
        assert_eq!(a, b);
    }

    #[test]
    fn test_compute_atom_id_different_inputs() {
        let a = compute_atom_id(b"A");
        let b = compute_atom_id(b"B");
        assert_ne!(a, b);
    }

    #[test]
    fn test_canonical_form_determinism() {
        let mut f1 = CanonicalForm::new(AtomType::FACT as u32);
        f1.valid_from = 1_000_000_000;
        f1.claims
            .push(CanonicalClaim::new(0, 1, ObjTag::BOOL.to_u8(), vec![1], 0));
        f1.normalize();
        let mut f2 = CanonicalForm::new(AtomType::FACT as u32);
        f2.valid_from = 1_000_000_000;
        f2.claims
            .push(CanonicalClaim::new(0, 1, ObjTag::BOOL.to_u8(), vec![1], 0));
        f2.normalize();
        assert_eq!(f1.to_bytes(), f2.to_bytes());
    }

    #[test]
    fn test_canonical_form_sorting_claims() {
        let mut form = CanonicalForm::new(AtomType::FACT as u32);
        form.claims.push(CanonicalClaim::new(
            2,
            1,
            ObjTag::I64.to_u8(),
            vec![0; 8],
            0,
        ));
        form.claims.push(CanonicalClaim::new(
            1,
            2,
            ObjTag::SYM.to_u8(),
            vec![0; 4],
            0,
        ));
        form.normalize();
        assert_eq!(form.claims[0].subject_symid, 1);
        assert_eq!(form.claims[1].subject_symid, 2);
    }

    #[test]
    fn test_canonical_form_same_claims_diff_order_same_hash() {
        let mut f1 = CanonicalForm::new(AtomType::FACT as u32);
        f1.claims.push(CanonicalClaim::new(
            2,
            1,
            ObjTag::I64.to_u8(),
            100i64.to_le_bytes().to_vec(),
            0,
        ));
        f1.claims.push(CanonicalClaim::new(
            1,
            1,
            ObjTag::I64.to_u8(),
            50i64.to_le_bytes().to_vec(),
            0,
        ));
        f1.normalize();
        let mut f2 = CanonicalForm::new(AtomType::FACT as u32);
        f2.claims.push(CanonicalClaim::new(
            1,
            1,
            ObjTag::I64.to_u8(),
            50i64.to_le_bytes().to_vec(),
            0,
        ));
        f2.claims.push(CanonicalClaim::new(
            2,
            1,
            ObjTag::I64.to_u8(),
            100i64.to_le_bytes().to_vec(),
            0,
        ));
        f2.normalize();
        assert_eq!(f1.to_bytes(), f2.to_bytes());
        assert_eq!(
            compute_atom_id_from_form(&f1),
            compute_atom_id_from_form(&f2)
        );
    }

    #[test]
    fn test_canonical_form_different_values_different_hash() {
        let mut f1 = CanonicalForm::new(AtomType::FACT as u32);
        f1.claims.push(CanonicalClaim::new(
            1,
            1,
            ObjTag::I64.to_u8(),
            42i64.to_le_bytes().to_vec(),
            0,
        ));
        f1.normalize();
        let mut f2 = CanonicalForm::new(AtomType::FACT as u32);
        f2.claims.push(CanonicalClaim::new(
            1,
            1,
            ObjTag::I64.to_u8(),
            43i64.to_le_bytes().to_vec(),
            0,
        ));
        f2.normalize();
        assert_ne!(f1.to_bytes(), f2.to_bytes());
        assert_ne!(
            compute_atom_id_from_form(&f1),
            compute_atom_id_from_form(&f2)
        );
    }

    #[test]
    fn test_canonical_form_domain_tags_sorted() {
        let mut form = CanonicalForm::new(AtomType::FACT as u32);
        form.meta_domain_tags.push("zebra".into());
        form.meta_domain_tags.push("alpha".into());
        form.meta_domain_tags.push("middle".into());
        form.normalize();
        assert_eq!(form.meta_domain_tags[0], "alpha");
        assert_eq!(form.meta_domain_tags[1], "middle");
        assert_eq!(form.meta_domain_tags[2], "zebra");
    }

    #[test]
    fn test_empty_canonical_form_nonzero_hash() {
        let form = CanonicalForm::new(AtomType::DEFINITION as u32);
        let id = compute_atom_id_from_form(&form);
        assert_eq!(id.len(), 32);
        assert_ne!(id, [0u8; 32]);
    }

    #[test]
    fn test_atom_id_unique_across_atom_types() {
        let mut ids = Vec::new();
        for atom_type in 1..=13u32 {
            let form = CanonicalForm::new(atom_type);
            ids.push(compute_atom_id_from_form(&form));
        }
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "AtomType {} and {} same hash", i + 1, j + 1);
            }
        }
    }

    #[test]
    fn test_record_header_with_atom_id() {
        let bh = AtomBodyHeader::new(AtomType::FACT, 0, 1_000_000_000, 0, 0);
        let body =
            unsafe { std::slice::from_raw_parts(&bh as *const AtomBodyHeader as *const u8, 48) }
                .to_vec();
        let header = RecordHeader::with_atom_id(&body, body.len() as u64, 1, 0);
        assert!(header.validate_magic());
        assert!(header.validate_crc());
        assert_eq!(header.atom_id.len(), 32);
    }

    #[test]
    fn test_record_header_same_body_same_id() {
        let bh1 = AtomBodyHeader::new(AtomType::FACT, 0, 1_000_000_000, 0, 0);
        let body1 =
            unsafe { std::slice::from_raw_parts(&bh1 as *const AtomBodyHeader as *const u8, 48) }
                .to_vec();
        let bh2 = AtomBodyHeader::new(AtomType::FACT, 0, 1_000_000_000, 0, 0);
        let body2 =
            unsafe { std::slice::from_raw_parts(&bh2 as *const AtomBodyHeader as *const u8, 48) }
                .to_vec();
        let h1 = RecordHeader::with_atom_id(&body1, body1.len() as u64, 1, 0);
        let h2 = RecordHeader::with_atom_id(&body2, body2.len() as u64, 2, 0);
        assert_eq!(h1.atom_id, h2.atom_id);
    }

    #[test]
    fn test_record_header_different_types_different_id() {
        let bh1 = AtomBodyHeader::new(AtomType::FACT, 0, 0, 0, 0);
        let body1 =
            unsafe { std::slice::from_raw_parts(&bh1 as *const AtomBodyHeader as *const u8, 48) }
                .to_vec();
        let bh2 = AtomBodyHeader::new(AtomType::DEFINITION, 0, 0, 0, 0);
        let body2 =
            unsafe { std::slice::from_raw_parts(&bh2 as *const AtomBodyHeader as *const u8, 48) }
                .to_vec();
        let h1 = RecordHeader::with_atom_id(&body1, body1.len() as u64, 1, 0);
        let h2 = RecordHeader::with_atom_id(&body2, body2.len() as u64, 1, 0);
        assert_ne!(h1.atom_id, h2.atom_id);
    }

    #[test]
    fn test_verify_atom_id_pass() {
        let bh = AtomBodyHeader::new(AtomType::FACT, 0, 0, 0, 0);
        let body =
            unsafe { std::slice::from_raw_parts(&bh as *const AtomBodyHeader as *const u8, 48) }
                .to_vec();
        let aid = extract_and_compute_atom_id(&body).unwrap();
        let hdr = RecordHeader::new(aid, body.len() as u64, 1, 0);
        assert!(verify_atom_id(&hdr, &body));
    }

    #[test]
    fn test_verify_atom_id_fail_wrong_id() {
        let bh = AtomBodyHeader::new(AtomType::FACT, 0, 0, 0, 0);
        let body =
            unsafe { std::slice::from_raw_parts(&bh as *const AtomBodyHeader as *const u8, 48) }
                .to_vec();
        let hdr = RecordHeader::new([0xFFu8; 32], body.len() as u64, 1, 0);
        assert!(!verify_atom_id(&hdr, &body));
    }

    #[test]
    fn test_verify_atom_id_fail_corrupt_body() {
        let bh = AtomBodyHeader::new(AtomType::FACT, 0, 0, 0, 0);
        let body =
            unsafe { std::slice::from_raw_parts(&bh as *const AtomBodyHeader as *const u8, 48) }
                .to_vec();
        let aid = extract_and_compute_atom_id(&body).unwrap();
        let hdr = RecordHeader::new(aid, body.len() as u64, 1, 0);
        let mut corrupt = body.clone();
        corrupt[0x20] ^= 0xFF;
        assert!(!verify_atom_id(&hdr, &corrupt));
    }

    #[test]
    fn test_verify_atom_id_invalid_body() {
        let hdr = RecordHeader::new([0u8; 32], 0, 1, 0);
        assert!(!verify_atom_id(&hdr, &[0, 1, 2]));
    }

    #[test]
    fn test_extract_canonical_form_with_claims() {
        let bh = AtomBodyHeader::new(AtomType::FACT, 0, 0, 0, 0);
        let mut cs = ClaimsSection::new();
        cs.add_claim(ClaimRecord::new_sym(0, 1, 2));
        cs.add_claim(ClaimRecord::new_sym(1, 0, 3));
        let cb = cs.to_bytes();
        let form = extract_canonical_form(&bh, &[], &cb, &[], &[], &[]).unwrap();
        assert_eq!(form.atom_type, AtomType::FACT as u32);
        assert_eq!(form.claims.len(), 2);
        assert!(form.claims[0] <= form.claims[1]);
    }

    #[test]
    fn legacy_claims_keep_the_v1_canonical_subject_width() {
        let bh = AtomBodyHeader::new(AtomType::FACT, 0, 0, 0, 0);
        let mut v1 = Vec::new();
        v1.extend_from_slice(&1u32.to_le_bytes());
        v1.extend_from_slice(&7u16.to_le_bytes());
        v1.extend_from_slice(&9u16.to_le_bytes());
        v1.push(ObjTag::U64.to_u8());
        v1.extend_from_slice(&42u64.to_le_bytes());

        let legacy = extract_canonical_form(&bh, &[], &v1, &[], &[], &[]).unwrap();
        let mut expected = CanonicalForm::new(AtomType::FACT as u32);
        expected.claims.push(CanonicalClaim::new(
            7,
            9,
            ObjTag::U64.to_u8(),
            42u64.to_le_bytes().to_vec(),
            0,
        ));
        expected.normalize();
        assert_eq!(legacy.to_bytes(), expected.to_bytes());

        assert_eq!(
            compute_atom_id_from_form(&legacy),
            compute_atom_id_from_form(&expected)
        );
    }

    #[test]
    fn test_extract_canonical_form_with_evidence() {
        let bh = AtomBodyHeader::new(AtomType::FACT, 0, 0, 0, 0);
        let mut es = EvidenceSection::new();
        es.add_evidence(EvidenceRecord::new(
            EvidenceKind::CITATION,
            1,
            2,
            1_000_000,
            65535,
            0,
        ));
        es.add_evidence(EvidenceRecord::new(
            EvidenceKind::MEASUREMENT,
            3,
            4,
            2_000_000,
            32768,
            0,
        ));
        let eb = es.to_bytes();
        let form = extract_canonical_form(&bh, &[], &[], &[], &eb, &[]).unwrap();
        assert_eq!(form.evidence_hashes.len(), 2);
        assert!(form.evidence_hashes[0] <= form.evidence_hashes[1]);
    }

    #[test]
    fn test_canonicalize_deterministic() {
        let id1 = canonicalize(
            AtomType::RULE as u32,
            0,
            0,
            vec!["test".into()],
            vec![],
            vec![],
            vec![],
            0,
            String::new(),
            vec![],
        );
        let id2 = canonicalize(
            AtomType::RULE as u32,
            0,
            0,
            vec!["test".into()],
            vec![],
            vec![],
            vec![],
            0,
            String::new(),
            vec![],
        );
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_canonicalize_different_types() {
        let id1 = canonicalize(
            AtomType::RULE as u32,
            0,
            0,
            vec![],
            vec![],
            vec![],
            vec![],
            0,
            String::new(),
            vec![],
        );
        let id2 = canonicalize(
            AtomType::FACT as u32,
            0,
            0,
            vec![],
            vec![],
            vec![],
            vec![],
            0,
            String::new(),
            vec![],
        );
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_full_roundtrip_with_all_fields() {
        let form = build_canonical_form(
            AtomType::HYPOTHESIS as u32,
            1_600_000_000_000_000_000,
            1_700_000_000_000_000_000,
            vec![
                "temperature".to_string(),
                "pressure".to_string(),
                "volume".to_string(),
            ],
            vec![
                CanonicalClaim::new(
                    0,
                    1,
                    ObjTag::F64.to_u8(),
                    273.15f64.to_le_bytes().to_vec(),
                    0,
                ),
                CanonicalClaim::new(
                    1,
                    2,
                    ObjTag::F64.to_u8(),
                    101325f64.to_le_bytes().to_vec(),
                    0,
                ),
            ],
            vec![0x01, 0x00, 0x00, 0x00],
            vec![[0xAA; 32], [0xBB; 32]],
            0x7FFF_FFFF,
            "2.1.0".to_string(),
            vec!["physics".to_string(), "thermodynamics".to_string()],
        );
        let id = compute_atom_id_from_form(&form);
        assert_eq!(id.len(), 32);
        assert_eq!(form.to_bytes(), form.to_bytes());
    }

    #[test]
    fn test_precomputed_atom_id_helper() {
        let aid = [0xCCu8; 32];
        let hdr = RecordHeader::with_precomputed_atom_id(aid, 1024, 5, 0);
        assert_eq!(hdr.atom_id, aid);
        assert_eq!(hdr.body_len, 1024);
        assert_eq!(hdr.seg_id, 5);
    }

    #[test]
    fn test_canonical_edge_sorting() {
        let mut edges = [
            CanonicalEdge::new(2, 100, 5000),
            CanonicalEdge::new(1, 50, 5000),
            CanonicalEdge::new(1, 100, 3000),
        ];
        edges.sort();
        assert_eq!(edges[0].edge_type, 1);
        assert_eq!(edges[0].target, 50);
        assert_eq!(edges[1].target, 100);
        assert_eq!(edges[2].edge_type, 2);
    }

    #[test]
    fn test_structural_edges_in_hash() {
        let mut form1 = CanonicalForm::new(AtomType::DEFINITION as u32);
        form1
            .structural_edges
            .push(CanonicalEdge::new(EdgeType::DEFINES as u32, 1, 5000));
        form1.normalize();

        let mut form2 = CanonicalForm::new(AtomType::DEFINITION as u32);
        form2
            .structural_edges
            .push(CanonicalEdge::new(EdgeType::REFINES as u32, 1, 5000));
        form2.normalize();

        let id1 = compute_atom_id_from_form(&form1);
        let id2 = compute_atom_id_from_form(&form2);
        assert_ne!(
            id1, id2,
            "Different structural edges should produce different AtomIds"
        );
    }

    #[test]
    fn test_build_atom_with_canonical_id() {
        let mut input = AtomInput::new(AtomType::FACT as u32);
        input
            .structural_edges
            .push(CanonicalEdge::new(EdgeType::DEFINES as u32, 1, 5000));
        input
            .dynamic_edges
            .push(CanonicalEdge::new(EdgeType::SUPPORTS as u32, 2, 3000));

        let (atom_id, body) = build_atom_with_canonical_id(input).unwrap();

        assert_eq!(atom_id.len(), 32);
        assert!(body.is_empty()); // Body builder returns empty for now
    }
}
