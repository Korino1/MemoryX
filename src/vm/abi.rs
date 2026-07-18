//! ABI entry point for eval_invariants (SKF-1.1 Section 9.1)
//!
//! This module implements the top-level ABI function that evaluates invariant
//! bytecode stored in an atom's INVARIANTS section.
//!
//! # Result Codes
//! - PASS (0): All invariants satisfied
//! - FailSoft (1): Soft invariant violation  
//! - FailHard (2): Hard invariant violation
//! - NeedBranch (3): Context conflict detected

#![allow(dead_code)]

use crate::cas::invariants::InvariantsSection;
use crate::cas::{AtomBodyHeader, SectionDesc};
use crate::prelude::*;
#[cfg(test)]
use crate::vm::interpreter::BytecodeBuilder;
use crate::vm::interpreter::{
    AtomView, ClaimData, ConflictProbe, ConflictSeverity, ConstValue, CtxIndex, CtxView,
    ExecutionContext, Instruction as VmInstruction, Opcode, QueryConstraintsView, VmInterpreter,
};

// ============================================================================
// ABI Result Type
// ============================================================================

/// Result of invariant evaluation per SKF-1.1
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvariantResult {
    /// All invariants satisfied
    Pass,
    /// Soft invariant violation (can be overridden)
    FailSoft { reason: u16 },
    /// Hard invariant violation (must not proceed)
    FailHard { reason: u16 },
    /// Context conflict detected -- needs branching
    NeedBranch { conflict_id: u32 },
}

impl InvariantResult {
    /// Convert ABI result to result code byte
    #[inline]
    pub const fn to_result_code(self) -> u8 {
        match self {
            InvariantResult::Pass => 0,
            InvariantResult::FailSoft { .. } => 1,
            InvariantResult::FailHard { .. } => 2,
            InvariantResult::NeedBranch { .. } => 3,
        }
    }

    /// Create ABI result from result code byte
    #[inline]
    pub fn from_result_code(code: u8, reason: u16) -> Self {
        match code {
            0 => InvariantResult::Pass,
            1 => InvariantResult::FailSoft { reason },
            2 => InvariantResult::FailHard { reason },
            3 => InvariantResult::NeedBranch {
                conflict_id: reason as u32,
            },
            _ => InvariantResult::FailHard {
                reason: ReasonCode::CORRUPT_SECTION as u16,
            },
        }
    }

    /// Returns true if the check allows proceeding
    #[inline]
    pub const fn allows_proceed(self) -> bool {
        matches!(self, InvariantResult::Pass)
    }
}

// ============================================================================
// Section Location
// ============================================================================

/// Find INVARIANTS section bytes within an atom body.
pub fn find_invariants_section(atom_body: &[u8]) -> Option<&[u8]> {
    if atom_body.len() < AtomBodyHeader::SIZE {
        return None;
    }
    let body_header = AtomBodyHeader::from_bytes(atom_body).ok()?;
    if !body_header.validate_magic() {
        return None;
    }
    let section_count = body_header.section_count as usize;
    let table_start = body_header.section_table_off as usize;
    let table_size = section_count.checked_mul(SectionDesc::SIZE)?;
    if table_start.checked_add(table_size)? > atom_body.len() {
        return None;
    }
    for i in 0..section_count {
        let offset = table_start + i * SectionDesc::SIZE;
        if offset + SectionDesc::SIZE > atom_body.len() {
            return None;
        }
        let section_bytes = &atom_body[offset..offset + SectionDesc::SIZE];
        let section_kind = u32::from_le_bytes(section_bytes[0..4].try_into().ok()?);
        if section_kind == SectionKind::INVARIANTS as u32 {
            let section_off = u64::from_le_bytes(section_bytes[8..16].try_into().ok()?);
            let section_len = u64::from_le_bytes(section_bytes[16..24].try_into().ok()?);
            let start = section_off as usize;
            let end = start.checked_add(section_len as usize)?;
            if end <= atom_body.len() {
                return Some(&atom_body[start..end]);
            }
        }
    }
    None
}

// ============================================================================
// Const Pool Conversion
// ============================================================================

/// Convert CAS InvariantsSection const pool entries to VM ConstValues.
fn convert_const_pool(section: &InvariantsSection) -> Vec<ConstValue> {
    use crate::cas::invariants::ConstPoolKind;
    section
        .const_pool
        .iter()
        .map(|entry| match entry.kind {
            ConstPoolKind::SYM => {
                let val = if entry.data.len() >= 4 {
                    u32::from_le_bytes(entry.data[0..4].try_into().unwrap_or([0; 4]))
                } else {
                    0
                };
                ConstValue::sym(val)
            }
            ConstPoolKind::U64 => {
                let val = if entry.data.len() >= 8 {
                    u64::from_le_bytes(entry.data[0..8].try_into().unwrap_or([0; 8]))
                } else {
                    0
                };
                ConstValue::u64(val)
            }
            ConstPoolKind::I64 => {
                let val = if entry.data.len() >= 8 {
                    i64::from_le_bytes(entry.data[0..8].try_into().unwrap_or([0; 8]))
                } else {
                    0
                };
                ConstValue::i64(val)
            }
            ConstPoolKind::F64 => {
                let val = if entry.data.len() >= 8 {
                    f64::from_le_bytes(entry.data[0..8].try_into().unwrap_or([0; 8]))
                } else {
                    0.0
                };
                ConstValue::f64(val)
            }
            ConstPoolKind::BYTES => {
                let mut buf = [0u8; 32];
                let len = entry.data.len().min(32);
                buf[..len].copy_from_slice(&entry.data[..len]);
                ConstValue::bytes(buf)
            }
            ConstPoolKind::REFID => {
                let val = if entry.data.len() >= 4 {
                    u32::from_le_bytes(entry.data[0..4].try_into().unwrap_or([0; 4]))
                } else {
                    0
                };
                ConstValue::ref_id(val)
            }
            ConstPoolKind::TAG => {
                let val = if entry.data.len() >= 4 {
                    u32::from_le_bytes(entry.data[0..4].try_into().unwrap_or([0; 4])) as u8
                } else {
                    0
                };
                ConstValue::tag(val)
            }
        })
        .collect()
}

// ============================================================================
// Raw Bytecode Parser
// ============================================================================

/// Parse raw 16-byte aligned instructions from InvariantsSection code bytes.
/// Returns None if code is not 16-byte aligned (malformed).
fn parse_raw_instructions(code: &[u8]) -> Option<Vec<VmInstruction>> {
    if !code.len().is_multiple_of(16) {
        return None;
    }
    if code.is_empty() {
        return Some(Vec::new());
    }
    let count = code.len() / 16;
    let mut instrs = Vec::with_capacity(count);
    for chunk in code.as_chunks::<16>().0 {
        let op = u16::from_le_bytes(chunk[0..2].try_into().unwrap());
        let a = u16::from_le_bytes(chunk[2..4].try_into().unwrap());
        let b = u32::from_le_bytes(chunk[4..8].try_into().unwrap());
        let imm = u64::from_le_bytes(chunk[8..16].try_into().unwrap());
        let opcode = Opcode::from_u16(op)?;
        instrs.push(VmInstruction::new(opcode, a, b, imm));
    }
    Some(instrs)
}

// ============================================================================
// Claims Extraction
// ============================================================================

/// Extract section bytes by kind from atom body.
fn extract_section_bytes(
    atom_body: &[u8],
    body_header: &AtomBodyHeader,
    target_kind: SectionKind,
) -> Option<Vec<u8>> {
    let section_count = body_header.section_count as usize;
    let table_start = body_header.section_table_off as usize;
    let table_size = section_count.checked_mul(SectionDesc::SIZE)?;
    if table_start.checked_add(table_size)? > atom_body.len() {
        return None;
    }
    for i in 0..section_count {
        let offset = table_start + i * SectionDesc::SIZE;
        if offset + SectionDesc::SIZE > atom_body.len() {
            continue;
        }
        let sb = &atom_body[offset..offset + SectionDesc::SIZE];
        let sk = u32::from_le_bytes(sb[0..4].try_into().unwrap_or([0; 4]));
        if sk == target_kind as u32 {
            let so = u64::from_le_bytes(sb[8..16].try_into().unwrap_or([0; 8]));
            let sl = u64::from_le_bytes(sb[16..24].try_into().unwrap_or([0; 8]));
            let start = so as usize;
            let end = start.checked_add(sl as usize)?.min(atom_body.len());
            if start < end {
                return Some(atom_body[start..end].to_vec());
            }
            return Some(Vec::new());
        }
    }
    None
}

/// Parse CLAIMS section bytes into owned ClaimData records.
fn parse_claims_bytes(bytes: &[u8]) -> Vec<ClaimData> {
    let mut claims = Vec::new();
    if bytes.len() < 5 {
        return claims;
    }

    // Try CAS ClaimRecord format first: u16 subj + u16 pred + u8 obj_tag + variable
    let mut offset = 0;
    let mut tried_cas = false;
    while offset + 5 <= bytes.len() {
        tried_cas = true;
        let subj = u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap());
        let pred = u16::from_le_bytes(bytes[offset + 2..offset + 4].try_into().unwrap());
        let obj_tag_byte = bytes[offset + 4];
        let Some(tag) = ObjTag::from_u8(obj_tag_byte) else {
            break;
        };
        let obj_start = offset + 5;
        let obj_len = match tag {
            ObjTag::NULL => 0,
            ObjTag::BOOL => 1,
            ObjTag::I64 | ObjTag::U64 | ObjTag::F64 | ObjTag::NODENUM => 8,
            ObjTag::SYM => 4,
            ObjTag::REF => 32,
            ObjTag::BYTES => {
                if obj_start + 4 > bytes.len() {
                    break;
                }
                4 + u32::from_le_bytes(bytes[obj_start..obj_start + 4].try_into().unwrap()) as usize
            }
        };
        if obj_start + obj_len > bytes.len() {
            break;
        }
        let val_len = obj_len.min(8);
        let obj_val = if val_len > 0 {
            let mut buf = [0u8; 8];
            buf[..val_len].copy_from_slice(&bytes[obj_start..obj_start + val_len]);
            u64::from_le_bytes(buf)
        } else {
            0
        };
        claims.push(ClaimData {
            subj: subj as u64,
            pred: pred as u64,
            obj_tag: obj_tag_byte,
            obj_val,
            qualifiers_mask: 0,
        });
        offset = obj_start + obj_len;
    }

    // If CAS format produced nothing, try 25-byte ClaimData flat format
    if claims.is_empty() && (!tried_cas || offset == 0) && bytes.len() >= 25 {
        const CLAIM_SIZE: usize = 25;
        for i in 0..bytes.len() / CLAIM_SIZE {
            let o = i * CLAIM_SIZE;
            let subj = u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
            let pred = u64::from_le_bytes(bytes[o + 8..o + 16].try_into().unwrap());
            let obj_tag = bytes[o + 16];
            let obj_val = u64::from_le_bytes(bytes[o + 17..o + 25].try_into().unwrap());
            claims.push(ClaimData {
                subj,
                pred,
                obj_tag,
                obj_val,
                qualifiers_mask: 0,
            });
        }
    }
    claims
}

// ============================================================================
// Meta Info Extraction
// ============================================================================

/// Parse META section bytes into trust, domain_mask, source_id.
fn parse_meta_bytes(bytes: &[u8]) -> (TrustLevel, DomainMask, u32) {
    if bytes.len() >= 16 {
        let trust_raw = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let domain_mask = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        let source_id = if bytes.len() >= 20 {
            u32::from_le_bytes(bytes[16..20].try_into().unwrap_or([0; 4]))
        } else {
            0
        };
        (
            trust_raw.min(u16::MAX as u64) as TrustLevel,
            domain_mask,
            source_id,
        )
    } else if bytes.len() >= 8 {
        let trust_raw = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        (trust_raw.min(u16::MAX as u64) as TrustLevel, 0xFFFF, 0)
    } else {
        (5000, 0xFFFF, 0)
    }
}

// ============================================================================
// VM Result Conversion
// ============================================================================

/// Convert VM prelude InvariantResult + reason into ABI InvariantResult.
fn vm_result_to_abi(
    vm_result: crate::prelude::InvariantResult,
    reason: ReasonCode,
) -> InvariantResult {
    match vm_result {
        crate::prelude::InvariantResult::PASS => InvariantResult::Pass,
        crate::prelude::InvariantResult::FAIL_SOFT => InvariantResult::FailSoft {
            reason: reason as u16,
        },
        crate::prelude::InvariantResult::FAIL_HARD => InvariantResult::FailHard {
            reason: reason as u16,
        },
        crate::prelude::InvariantResult::NEED_BRANCH => {
            InvariantResult::NeedBranch { conflict_id: 0 }
        }
    }
}

/// Convert store/prelude InvariantResult to ABI InvariantResult.
pub fn store_result_to_abi_result(result: crate::prelude::InvariantResult) -> InvariantResult {
    match result {
        crate::prelude::InvariantResult::PASS => InvariantResult::Pass,
        crate::prelude::InvariantResult::FAIL_SOFT => InvariantResult::FailSoft { reason: 0 },
        crate::prelude::InvariantResult::FAIL_HARD => InvariantResult::FailHard { reason: 0 },
        crate::prelude::InvariantResult::NEED_BRANCH => {
            InvariantResult::NeedBranch { conflict_id: 0 }
        }
    }
}

// ============================================================================
// Domain Mask Helpers
// ============================================================================

/// Convert DomainMask (u64 bitmask) to list of set bit indices (tags).
pub fn domain_mask_to_tags(mask: DomainMask) -> Vec<u32> {
    if mask == 0 {
        return Vec::new();
    }
    let mut tags = Vec::with_capacity(8);
    let mut m = mask;
    while m != 0 {
        let bit = m.trailing_zeros();
        tags.push(bit);
        m &= !(1u64 << bit);
    }
    tags
}

// ============================================================================
// Conflict Probe Builders
// ============================================================================

/// Build ConflictProbe Vec from CtxIndex (owned - lifetime safe).
fn build_conflict_probes(ctx_index: &CtxIndex) -> Vec<ConflictProbe> {
    ctx_index
        .conflicts
        .values()
        .map(|info| ConflictProbe {
            pattern_hash: info.pattern_hash,
            conflict_count: info.atom_ids.len() as u32,
            max_trust: 5000,
            flags: match info.severity {
                ConflictSeverity::Hard => 1,
                ConflictSeverity::Soft => 0,
            },
        })
        .collect()
}

// ============================================================================
// Main ABI Entry Point
// ============================================================================

/// Evaluate invariants stored in an atom body per SKF-1.1 section 9.1.
pub fn eval_invariants(
    atom_body: &[u8],
    claim_idx: Option<usize>,
    ctx_index: &CtxIndex,
    query_time_from: u64,
    query_time_to: u64,
    trust_min: u16,
    domain_tags: &[u32],
) -> InvariantResult {
    // Build domain mask from tags
    let domain_mask: DomainMask = domain_tags
        .iter()
        .fold(0u64, |mask, &tag| mask | (1u64 << (tag & 63)));

    // Step 1: Find INVARIANTS section
    let invariants_bytes = match find_invariants_section(atom_body) {
        Some(bytes) => bytes,
        None => return InvariantResult::Pass,
    };

    // Step 2: Parse InvariantsSection
    let section = match InvariantsSection::from_bytes(invariants_bytes) {
        Ok(s) => s,
        Err(_) => {
            return InvariantResult::FailHard {
                reason: ReasonCode::CORRUPT_SECTION as u16,
            };
        }
    };

    // Step 3: Empty bytecode -> PASS
    if section.code.is_empty() {
        return InvariantResult::Pass;
    }

    // Step 4a: Parse raw instructions (NOT using decode_instructions - types differ)
    let Some(instructions) = parse_raw_instructions(&section.code) else {
        return InvariantResult::FailHard {
            reason: ReasonCode::CORRUPT_SECTION as u16,
        };
    };
    if instructions.is_empty() {
        return InvariantResult::Pass;
    }

    // Step 4b: Convert const pool (owned - stays alive for VM)
    let const_pool = convert_const_pool(&section);

    // Step 4c: Parse header for claims and meta extraction
    let body_header = match AtomBodyHeader::from_bytes(atom_body) {
        Ok(h) => h,
        Err(_) => {
            return InvariantResult::FailHard {
                reason: ReasonCode::CORRUPT_SECTION as u16,
            };
        }
    };

    // Step 4d: Extract claims bytes then parse (owned Vec - satisfies AtomView lifetime)
    let claims_data = parse_claims_bytes(
        extract_section_bytes(atom_body, &body_header, SectionKind::CLAIMS)
            .as_deref()
            .unwrap_or(&[]),
    );

    // Step 4e: Extract meta info
    let (trust_level, body_domain, _source_id) =
        extract_section_bytes(atom_body, &body_header, SectionKind::META)
            .as_ref()
            .map(|b| parse_meta_bytes(b))
            .unwrap_or((5000, 0xFFFF, 0));

    // Step 4f: Build ConflictProbes (owned Vec - satisfies CtxView lifetime)
    let conflict_probes = build_conflict_probes(ctx_index);

    let trust_used = trust_level;
    let domain_used = body_domain;

    // Step 5: Optional claim view
    let claim_view = match claim_idx {
        Some(idx) if idx < claims_data.len() => Some(claims_data[idx].clone()),
        _ => None,
    };

    // Step 6: Build QueryConstraintsView
    let qc_view = QueryConstraintsView::new(
        query_time_from,
        query_time_to,
        trust_min,
        domain_mask,
        u64::MAX,
        100,
    );

    // Step 7: Build AtomView (all owned data lives in this scope)
    let dummy_atom_id = [0u8; 32];
    let atom_view = AtomView::new(
        &dummy_atom_id,
        body_header.atom_type().unwrap_or(AtomType::FACT),
        &[],
        &claims_data,
        body_header.valid_from_unix_ns,
        if body_header.valid_to_unix_ns == 0 {
            u64::MAX
        } else {
            body_header.valid_to_unix_ns
        },
        trust_used,
        domain_used,
        0,
    );

    // Step 8: Build CtxView
    let empty_policy: [u8; 0] = [];
    let ctx_view = CtxView::new(
        0,
        &empty_policy,
        &conflict_probes,
        ctx_index.conflicts.len() as u64,
    );

    // Step 9: Build ExecutionContext
    let exec_ctx = ExecutionContext::new(atom_body, None, ctx_index, None);

    // Step 10: Execute VM
    let mut vm = VmInterpreter::new(&const_pool, atom_view, ctx_view, qc_view, exec_ctx, 10_000);
    if let Some(cv) = claim_view {
        vm.set_claim_view(cv);
    }

    let vm_result = match vm.execute(&instructions) {
        Ok(r) => r,
        Err(_) => {
            return InvariantResult::FailHard {
                reason: ReasonCode::CORRUPT_SECTION as u16,
            };
        }
    };

    vm_result_to_abi(vm_result, vm.reason())
}

// ============================================================================
// Wrapper for QueryConstraints
// ============================================================================

/// Evaluate invariants using QueryConstraints.
pub fn eval_invariants_for_atom(
    atom_body: &[u8],
    claim_idx: Option<usize>,
    ctx_index: &CtxIndex,
    constraints: &QueryConstraints,
) -> InvariantResult {
    let (tf, tt) = if let Some(r) = &constraints.time_range {
        (r.from_ns, r.to_ns)
    } else {
        (0, u64::MAX)
    };
    let tm = constraints.trust_min.unwrap_or(0);
    let dt = constraints
        .domain_mask
        .map(domain_mask_to_tags)
        .unwrap_or(vec![]);
    eval_invariants(atom_body, claim_idx, ctx_index, tf, tt, tm, &dt)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::invariants::{ConstPoolKind, InvariantsSection};
    use crate::store::AtomType;

    // ---- Test Helpers ----

    fn build_claims_bytes(claims: &[ClaimData]) -> Vec<u8> {
        let mut section = crate::cas::claims::ClaimsSection::new();
        for c in claims {
            let rec = crate::cas::claims::ClaimRecord::from_scalar(
                c.subj,
                c.pred as u32,
                crate::store::ObjTag::from_u8(c.obj_tag).unwrap_or(crate::store::ObjTag::U64),
                c.obj_val,
            )
            .unwrap();
            section.add_claim(rec);
        }
        section.to_bytes()
    }

    fn build_meta_bytes(trust: TrustLevel, domain: DomainMask, source_id: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(trust as u64).to_le_bytes());
        buf.extend_from_slice(&domain.to_le_bytes());
        buf.extend_from_slice(&source_id.to_le_bytes());
        buf
    }

    #[allow(clippy::too_many_arguments)]
    fn build_atom_body(
        inv_code: Vec<VmInstruction>,
        inv_pool: Vec<ConstValue>,
        valid_from: u64,
        valid_to: u64,
        trust: TrustLevel,
        domain: DomainMask,
        source_id: u32,
        claims: Vec<ClaimData>,
    ) -> Vec<u8> {
        let mut inv_section = InvariantsSection::new();
        for cv in &inv_pool {
            match cv {
                ConstValue::Sym(v) => inv_section.add_const(ConstPoolKind::SYM, &v.to_le_bytes()),
                ConstValue::U64(v) => inv_section.add_const(ConstPoolKind::U64, &v.to_le_bytes()),
                ConstValue::I64(v) => inv_section.add_const(ConstPoolKind::I64, &v.to_le_bytes()),
                ConstValue::F64(v) => inv_section.add_const(ConstPoolKind::F64, &v.to_le_bytes()),
                ConstValue::Bytes(b) => inv_section.add_const(ConstPoolKind::BYTES, b.as_slice()),
                ConstValue::RefId(v) => {
                    inv_section.add_const(ConstPoolKind::REFID, &v.to_le_bytes())
                }
                ConstValue::Tag(v) => {
                    inv_section.add_const(ConstPoolKind::TAG, &(*v as u32).to_le_bytes())
                }
            };
        }
        for instr in &inv_code {
            inv_section.emit_instruction(
                instr.opcode().map_or(0, |o| o.to_u16()),
                instr.reg_a(),
                instr.const_index(),
                instr.imm_u64(),
            );
        }
        let inv_bytes = inv_section.to_bytes();
        let claims_bytes = build_claims_bytes(&claims);
        let meta_bytes = build_meta_bytes(trust, domain, source_id);

        let sections_data_start: usize = 48 + 3 * 32;
        let inv_off: usize = sections_data_start;
        let claims_off: usize = inv_off + inv_bytes.len();
        let meta_off: usize = claims_off + claims_bytes.len();

        let mut body = Vec::new();
        body.extend_from_slice(&0x41544F4Du32.to_le_bytes());
        body.extend_from_slice(&0x0001u16.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&0u64.to_le_bytes());
        body.extend_from_slice(&valid_from.to_le_bytes());
        body.extend_from_slice(&(if valid_to == 0 { u64::MAX } else { valid_to }).to_le_bytes());
        body.extend_from_slice(&(AtomType::FACT as u32).to_le_bytes());
        body.extend_from_slice(&3u32.to_le_bytes());
        body.extend_from_slice(&48u64.to_le_bytes());

        for (sk, data, off) in [
            (SectionKind::INVARIANTS, inv_bytes.as_slice(), inv_off),
            (SectionKind::CLAIMS, claims_bytes.as_slice(), claims_off),
            (SectionKind::META, meta_bytes.as_slice(), meta_off),
        ] {
            let crc = crate::utils::crc32(data);
            let len = data.len() as u64;
            body.extend_from_slice(&(sk as u32).to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes());
            body.extend_from_slice(&(off as u64).to_le_bytes());
            body.extend_from_slice(&len.to_le_bytes());
            body.extend_from_slice(&crc.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes());
        }
        body.extend_from_slice(&inv_bytes);
        body.extend_from_slice(&claims_bytes);
        body.extend_from_slice(&meta_bytes);
        body
    }

    fn build_atom_body_no_inv(
        valid_from: u64,
        valid_to: u64,
        trust: TrustLevel,
        domain: DomainMask,
        source_id: u32,
        claims: Vec<ClaimData>,
    ) -> Vec<u8> {
        let claims_bytes = build_claims_bytes(&claims);
        let meta_bytes = build_meta_bytes(trust, domain, source_id);
        let sections_data_start: usize = 48 + 2 * 32;
        let claims_off: usize = sections_data_start;
        let meta_off: usize = claims_off + claims_bytes.len();

        let mut body = Vec::new();
        body.extend_from_slice(&0x41544F4Du32.to_le_bytes());
        body.extend_from_slice(&0x0001u16.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&0u64.to_le_bytes());
        body.extend_from_slice(&valid_from.to_le_bytes());
        body.extend_from_slice(&(if valid_to == 0 { u64::MAX } else { valid_to }).to_le_bytes());
        body.extend_from_slice(&(AtomType::FACT as u32).to_le_bytes());
        body.extend_from_slice(&2u32.to_le_bytes());
        body.extend_from_slice(&48u64.to_le_bytes());

        {
            let crc = crate::utils::crc32(&claims_bytes);
            body.extend_from_slice(&(SectionKind::CLAIMS as u32).to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes());
            body.extend_from_slice(&(claims_off as u64).to_le_bytes());
            body.extend_from_slice(&(claims_bytes.len() as u64).to_le_bytes());
            body.extend_from_slice(&crc.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes());
        }
        {
            let crc = crate::utils::crc32(&meta_bytes);
            body.extend_from_slice(&(SectionKind::META as u32).to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes());
            body.extend_from_slice(&(meta_off as u64).to_le_bytes());
            body.extend_from_slice(&(meta_bytes.len() as u64).to_le_bytes());
            body.extend_from_slice(&crc.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes());
        }
        body.extend_from_slice(&claims_bytes);
        body.extend_from_slice(&meta_bytes);
        body
    }

    #[test]
    fn t01_eval_pass_no_invariants() {
        let atom_body = build_atom_body_no_inv(0, u64::MAX, 5000, 0xFFFF, 0, Vec::new());
        let ctx = CtxIndex::new();
        let result = eval_invariants(&atom_body, None, &ctx, 0, u64::MAX, 0, &[]);
        assert_eq!(result, InvariantResult::Pass);
    }

    #[test]
    fn t02_eval_pass_empty_bytecode() {
        let inv_section = InvariantsSection::new();
        let inv_bytes = inv_section.to_bytes();
        let claims_bytes = build_claims_bytes(&[]);
        let meta_bytes = build_meta_bytes(5000, 0xFFFF, 0);
        let sds = 48 + 3 * 32;
        let inv_off = sds;
        let claims_off = inv_off + inv_bytes.len();
        let meta_off = claims_off + claims_bytes.len();
        let mut body = Vec::new();
        body.extend_from_slice(&0x41544F4Du32.to_le_bytes());
        body.extend_from_slice(&0x0001u16.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&0u64.to_le_bytes());
        body.extend_from_slice(&0u64.to_le_bytes());
        body.extend_from_slice(&u64::MAX.to_le_bytes());
        body.extend_from_slice(&(AtomType::FACT as u32).to_le_bytes());
        body.extend_from_slice(&3u32.to_le_bytes());
        body.extend_from_slice(&48u64.to_le_bytes());
        for (sk, data, off) in [
            (SectionKind::INVARIANTS as u32, &inv_bytes, inv_off as u64),
            (SectionKind::CLAIMS as u32, &claims_bytes, claims_off as u64),
            (SectionKind::META as u32, &meta_bytes, meta_off as u64),
        ] {
            let crc = crate::utils::crc32(data);
            body.extend_from_slice(&sk.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes());
            body.extend_from_slice(&off.to_le_bytes());
            body.extend_from_slice(&(data.len() as u64).to_le_bytes());
            body.extend_from_slice(&crc.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes());
        }
        body.extend_from_slice(&inv_bytes);
        body.extend_from_slice(&claims_bytes);
        body.extend_from_slice(&meta_bytes);
        let ctx = CtxIndex::new();
        let result = eval_invariants(&body, None, &ctx, 0, u64::MAX, 0, &[]);
        assert_eq!(result, InvariantResult::Pass);
    }

    #[test]
    fn t03_eval_pass_basic_time_trust_domain() {
        let body = build_atom_body(
            vec![
                VmInstruction::new(Opcode::CHK_TIME, 0, 0, 0),
                VmInstruction::new(Opcode::CHK_TRUST, 0, 0, 0),
                VmInstruction::new(Opcode::CHK_DOMAIN, 0, 0, 0),
                VmInstruction::new(Opcode::RET, 0, 0, 0),
            ],
            vec![],
            0,
            u64::MAX,
            5000,
            0xFFFF,
            0,
            Vec::new(),
        );
        let ctx = CtxIndex::new();
        let result = eval_invariants(&body, None, &ctx, 0, u64::MAX, 0, &[]);
        assert_eq!(result, InvariantResult::Pass);
    }

    #[test]
    fn t04_eval_fail_hard_time() {
        let body = build_atom_body(
            vec![
                VmInstruction::new(Opcode::CHK_TIME, 0, 0, 0),
                VmInstruction::new(Opcode::RET, 0, 0, 0),
            ],
            vec![],
            1_000_000,
            2_000_000,
            5000,
            0xFFFF,
            0,
            Vec::new(),
        );
        let ctx = CtxIndex::new();
        let result = eval_invariants(&body, None, &ctx, 0, 500_000, 0, &[]);
        assert!(
            matches!(result, InvariantResult::FailHard { reason } if reason == ReasonCode::TIME_INVALID as u16)
        );
    }

    #[test]
    fn t05_eval_fail_hard_trust() {
        let body = build_atom_body(
            vec![
                VmInstruction::new(Opcode::CHK_TRUST, 0, 0, 0),
                VmInstruction::new(Opcode::RET, 0, 0, 0),
            ],
            vec![],
            0,
            u64::MAX,
            300,
            0xFFFF,
            0,
            Vec::new(),
        );
        let ctx = CtxIndex::new();
        let result = eval_invariants(&body, None, &ctx, 0, u64::MAX, 500, &[]);
        assert!(
            matches!(result, InvariantResult::FailHard { reason } if reason == ReasonCode::TRUST_TOO_LOW as u16)
        );
    }

    #[test]
    fn t06_eval_fail_hard_domain() {
        let body = build_atom_body(
            vec![
                VmInstruction::new(Opcode::CHK_DOMAIN, 0, 0, 0),
                VmInstruction::new(Opcode::RET, 0, 0, 0),
            ],
            vec![],
            0,
            u64::MAX,
            5000,
            0x01,
            0,
            Vec::new(),
        );
        let ctx = CtxIndex::new();
        let result = eval_invariants(&body, None, &ctx, 0, u64::MAX, 0, &[5]);
        assert!(
            matches!(result, InvariantResult::FailHard { reason } if reason == ReasonCode::DOMAIN_MISMATCH as u16)
        );
    }

    #[test]
    fn t07_eval_fail_soft() {
        let body = build_atom_body(
            vec![
                VmInstruction::new(Opcode::CHK_TIME, 0, 0, 0),
                VmInstruction::new(
                    Opcode::RET,
                    crate::prelude::InvariantResult::PASS as u16,
                    0,
                    0,
                ),
            ],
            vec![],
            0,
            u64::MAX,
            5000,
            0xFFFF,
            0,
            Vec::new(),
        );
        let ctx = CtxIndex::new();
        let result = eval_invariants(&body, None, &ctx, 0, u64::MAX, 0, &[]);
        assert_eq!(result, InvariantResult::Pass);
    }

    #[test]
    fn t08_eval_need_branch() {
        let mut builder = BytecodeBuilder::new();
        builder.emit_load(Opcode::LD_CTX, 1, 0);
        builder.emit(VmInstruction::new(Opcode::CTX_PROBE, 1, 0, 0));
        builder.emit(VmInstruction::new(Opcode::RET, 0, 0, 0));
        let (code, pool) = builder.build_with_pool().unwrap();
        let body = build_atom_body(code, pool, 0, u64::MAX, 5000, 0xFFFF, 0, Vec::new());
        let mut ctx = CtxIndex::new();
        ctx.add_conflict(0, [1u8; 32], ConflictSeverity::Soft);
        let result = eval_invariants(&body, None, &ctx, 0, u64::MAX, 0, &[]);
        assert!(matches!(result, InvariantResult::NeedBranch { .. }));
    }

    #[test]
    fn t09_eval_corrupt_section() {
        let mut body = build_atom_body(
            vec![VmInstruction::new(Opcode::RET, 0, 0, 0)],
            vec![],
            0,
            u64::MAX,
            5000,
            0xFFFF,
            0,
            Vec::new(),
        );
        let sds = 48 + 3 * 32;
        if body.len() > sds + 4 {
            body[sds] = 0xFF;
        }
        let ctx = CtxIndex::new();
        let result = eval_invariants(&body, None, &ctx, 0, u64::MAX, 0, &[]);
        assert!(
            matches!(result, InvariantResult::FailHard { reason } if reason == ReasonCode::CORRUPT_SECTION as u16)
        );
    }

    #[test]
    fn t10_result_code_conversion() {
        assert_eq!(InvariantResult::Pass.to_result_code(), 0);
        assert_eq!(InvariantResult::FailSoft { reason: 5 }.to_result_code(), 1);
        assert_eq!(InvariantResult::FailHard { reason: 3 }.to_result_code(), 2);
        assert_eq!(
            InvariantResult::NeedBranch { conflict_id: 42 }.to_result_code(),
            3
        );
        let r = InvariantResult::from_result_code(1, 5);
        assert_eq!(r, InvariantResult::FailSoft { reason: 5 });
        let r = InvariantResult::from_result_code(2, 3);
        assert_eq!(r, InvariantResult::FailHard { reason: 3 });
        let r = InvariantResult::from_result_code(3, 99);
        assert_eq!(r, InvariantResult::NeedBranch { conflict_id: 99 });
    }

    #[test]
    fn t11_allows_proceed() {
        assert!(InvariantResult::Pass.allows_proceed());
        assert!(!InvariantResult::FailSoft { reason: 1 }.allows_proceed());
        assert!(!InvariantResult::FailHard { reason: 1 }.allows_proceed());
        assert!(!InvariantResult::NeedBranch { conflict_id: 1 }.allows_proceed());
    }

    #[test]
    fn t12_store_result_to_abi_result() {
        assert_eq!(
            store_result_to_abi_result(crate::prelude::InvariantResult::PASS),
            InvariantResult::Pass
        );
        assert!(matches!(
            store_result_to_abi_result(crate::prelude::InvariantResult::FAIL_SOFT),
            InvariantResult::FailSoft { .. }
        ));
        assert!(matches!(
            store_result_to_abi_result(crate::prelude::InvariantResult::FAIL_HARD),
            InvariantResult::FailHard { .. }
        ));
        assert!(matches!(
            store_result_to_abi_result(crate::prelude::InvariantResult::NEED_BRANCH),
            InvariantResult::NeedBranch { .. }
        ));
    }

    #[test]
    fn t13_domain_mask_to_tags() {
        assert!(domain_mask_to_tags(0).is_empty());
        assert_eq!(domain_mask_to_tags(0x03), vec![0, 1]);
        assert_eq!(domain_mask_to_tags(0x05), vec![0, 2]);
    }

    #[test]
    fn t14_eval_for_atom() {
        let constraints = QueryConstraints::new();
        let body = build_atom_body_no_inv(0, u64::MAX, 5000, 0xFFFF, 0, Vec::new());
        let ctx = CtxIndex::new();
        let result = eval_invariants_for_atom(&body, None, &ctx, &constraints);
        assert_eq!(result, InvariantResult::Pass);
    }

    #[test]
    fn t15_empty_buffer() {
        let ctx = CtxIndex::new();
        assert_eq!(
            eval_invariants(b"", None, &ctx, 0, u64::MAX, 0, &[]),
            InvariantResult::Pass
        );
        assert_eq!(
            eval_invariants(&[0u8; 10], None, &ctx, 0, u64::MAX, 0, &[]),
            InvariantResult::Pass
        );
    }

    #[test]
    fn t16_const_pool_conversions() {
        let mut sec = InvariantsSection::new();
        sec.add_const_sym(42);
        sec.add_const_u64(123456789);
        sec.add_const_i64(-9999);
        sec.add_const_f64(std::f64::consts::PI);
        sec.add_const(ConstPoolKind::TAG, &5u32.to_le_bytes());
        let cv = convert_const_pool(&sec);
        assert_eq!(cv.len(), 5);
        assert_eq!(cv[0], ConstValue::sym(42));
        assert_eq!(cv[1], ConstValue::u64(123456789));
        assert_eq!(cv[2], ConstValue::i64(-9999));
        assert!((cv[3].as_f64().unwrap() - std::f64::consts::PI).abs() < 0.0001);
        assert_eq!(cv[4], ConstValue::tag(5));
    }

    #[test]
    fn t17_invalid_magic() {
        let buf = vec![0xFF; 48];
        let ctx = CtxIndex::new();
        assert_eq!(
            eval_invariants(&buf, None, &ctx, 0, u64::MAX, 0, &[]),
            InvariantResult::Pass
        );
    }

    #[test]
    fn t18_pass_high_trust() {
        let body = build_atom_body(
            vec![
                VmInstruction::new(Opcode::CHK_TRUST, 0, 0, 0),
                VmInstruction::new(Opcode::RET, 0, 0, 0),
            ],
            vec![],
            0,
            u64::MAX,
            9000,
            0xFFFF,
            0,
            Vec::new(),
        );
        let ctx = CtxIndex::new();
        let result = eval_invariants(&body, None, &ctx, 0, u64::MAX, 5000, &[]);
        assert_eq!(result, InvariantResult::Pass);
    }

    #[test]
    fn t19_pass_domain_overlap() {
        let body = build_atom_body(
            vec![
                VmInstruction::new(Opcode::CHK_DOMAIN, 0, 0, 0),
                VmInstruction::new(Opcode::RET, 0, 0, 0),
            ],
            vec![],
            0,
            u64::MAX,
            5000,
            0x0F,
            0,
            Vec::new(),
        );
        let ctx = CtxIndex::new();
        let result = eval_invariants(&body, None, &ctx, 0, u64::MAX, 0, &[1]);
        assert_eq!(result, InvariantResult::Pass);
    }

    #[test]
    fn t20_with_claims() {
        let claims = vec![ClaimData {
            subj: 1,
            pred: 2,
            obj_tag: 3,
            obj_val: 42,
            qualifiers_mask: 0,
        }];
        let body = build_atom_body_no_inv(0, u64::MAX, 5000, 0xFFFF, 0, claims);
        let ctx = CtxIndex::new();
        assert_eq!(
            eval_invariants(&body, None, &ctx, 0, u64::MAX, 0, &[]),
            InvariantResult::Pass
        );
    }

    #[test]
    fn t21_malformed_bytecode() {
        let mut inv_section = InvariantsSection::new();
        inv_section.inv_count = 1;
        inv_section.add_const_u64(42);
        inv_section.code.extend_from_slice(&[
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]);
        let inv_bytes = inv_section.to_bytes();
        let claims_bytes = build_claims_bytes(&[]);
        let meta_bytes = build_meta_bytes(5000, 0xFFFF, 0);
        let sds = 48 + 3 * 32;
        let inv_off = sds;
        let claims_off = inv_off + inv_bytes.len();
        let meta_off = claims_off + claims_bytes.len();
        let mut body = Vec::new();
        body.extend_from_slice(&0x41544F4Du32.to_le_bytes());
        body.extend_from_slice(&0x0001u16.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&0u64.to_le_bytes());
        body.extend_from_slice(&0u64.to_le_bytes());
        body.extend_from_slice(&u64::MAX.to_le_bytes());
        body.extend_from_slice(&(AtomType::FACT as u32).to_le_bytes());
        body.extend_from_slice(&3u32.to_le_bytes());
        body.extend_from_slice(&48u64.to_le_bytes());
        for (sk, data, off) in &[
            (SectionKind::INVARIANTS as u32, &inv_bytes, inv_off as u64),
            (SectionKind::CLAIMS as u32, &claims_bytes, claims_off as u64),
            (SectionKind::META as u32, &meta_bytes, meta_off as u64),
        ] {
            let crc = crate::utils::crc32(data);
            body.extend_from_slice(&sk.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes());
            body.extend_from_slice(&off.to_le_bytes());
            body.extend_from_slice(&(data.len() as u64).to_le_bytes());
            body.extend_from_slice(&crc.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes());
        }
        body.extend_from_slice(&inv_bytes);
        body.extend_from_slice(&claims_bytes);
        body.extend_from_slice(&meta_bytes);
        let ctx = CtxIndex::new();
        let result = eval_invariants(&body, None, &ctx, 0, u64::MAX, 0, &[]);
        assert!(
            matches!(result, InvariantResult::FailHard { reason } if reason == ReasonCode::CORRUPT_SECTION as u16)
        );
    }

    #[test]
    fn t22_fail_all_checks() {
        let body = build_atom_body(
            vec![
                VmInstruction::new(Opcode::CHK_TIME, 0, 0, 0),
                VmInstruction::new(Opcode::CHK_TRUST, 0, 0, 0),
                VmInstruction::new(Opcode::CHK_DOMAIN, 0, 0, 0),
                VmInstruction::new(Opcode::RET, 0, 0, 0),
            ],
            vec![],
            100_000,
            200_000,
            100,
            0x01,
            0,
            Vec::new(),
        );
        let ctx = CtxIndex::new();
        let result = eval_invariants(&body, None, &ctx, 0, 1_000, 500, &[10]);
        assert!(matches!(result, InvariantResult::FailHard { .. }));
    }

    #[test]
    fn t23_ret_pass() {
        let body = build_atom_body(
            vec![VmInstruction::new(Opcode::RET, 0, 0, 0)],
            vec![],
            0,
            u64::MAX,
            5000,
            0xFFFF,
            0,
            Vec::new(),
        );
        let ctx = CtxIndex::new();
        let result = eval_invariants(&body, None, &ctx, 0, u64::MAX, 0, &[]);
        assert_eq!(result, InvariantResult::Pass);
    }

    #[test]
    fn t24_find_returns_none_no_inv() {
        let body = build_atom_body_no_inv(0, u64::MAX, 5000, 0xFFFF, 0, Vec::new());
        assert!(find_invariants_section(&body).is_none());
    }

    #[test]
    fn t25_for_atom_with_range() {
        let constraints = QueryConstraints::new()
            .with_time_range(TimeRange::new(0, u64::MAX))
            .with_trust_min(0)
            .with_domain(0xFFFF);
        let body = build_atom_body_no_inv(0, u64::MAX, 5000, 0xFFFF, 0, Vec::new());
        let ctx = CtxIndex::new();
        let result = eval_invariants_for_atom(&body, None, &ctx, &constraints);
        assert_eq!(result, InvariantResult::Pass);
    }

    #[test]
    fn t26_hard_conflict() {
        let mut builder = BytecodeBuilder::new();
        builder.emit_load(Opcode::LD_CTX, 1, 0);
        builder.emit(VmInstruction::new(Opcode::CTX_PROBE, 1, 0, 0));
        builder.emit(VmInstruction::new(Opcode::RET, 0, 0, 0));
        let (code, pool) = builder.build_with_pool().unwrap();
        let body = build_atom_body(code, pool, 0, u64::MAX, 5000, 0xFFFF, 0, Vec::new());
        let mut ctx = CtxIndex::new();
        ctx.add_conflict(0, [1u8; 32], ConflictSeverity::Hard);
        let result = eval_invariants(&body, None, &ctx, 0, u64::MAX, 0, &[]);
        assert!(
            matches!(result, InvariantResult::FailHard { reason } if reason == ReasonCode::CONFLICT_FOUND as u16)
        );
    }

    #[test]
    fn t27_claim_idx_inbounds() {
        let claims = vec![ClaimData {
            subj: 10,
            pred: 20,
            obj_tag: ObjTag::SYM as u8,
            obj_val: 42,
            qualifiers_mask: 0,
        }];
        let body = build_atom_body_no_inv(0, u64::MAX, 5000, 0xFFFF, 0, claims);
        let ctx = CtxIndex::new();
        assert_eq!(
            eval_invariants(&body, Some(0), &ctx, 0, u64::MAX, 0, &[]),
            InvariantResult::Pass
        );
    }

    #[test]
    fn t28_dynamic_builder() {
        let mut builder = BytecodeBuilder::new();
        builder.emit(VmInstruction::new(Opcode::CHK_TIME, 0, 0, 0));
        builder.emit(VmInstruction::new(Opcode::CHK_TRUST, 0, 0, 0));
        builder.emit(VmInstruction::new(Opcode::CHK_DOMAIN, 0, 0, 0));
        builder.emit_ret(crate::prelude::InvariantResult::PASS);
        let (code, pool) = builder.build_with_pool().unwrap();
        let body = build_atom_body(code, pool, 0, u64::MAX, 5000, 0xFFFF, 0, Vec::new());
        let ctx = CtxIndex::new();
        let result = eval_invariants(&body, None, &ctx, 0, u64::MAX, 0, &[]);
        assert_eq!(result, InvariantResult::Pass);
    }
}
