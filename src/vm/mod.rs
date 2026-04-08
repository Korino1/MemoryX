//! Invariant VM Bytecode Interpreter for MemoryX SKF-1.1
//!
//! This module implements a complete bytecode virtual machine for executing invariant checks
//! on atoms and claims. The VM uses a 16-byte aligned instruction format and
//! provides deterministic execution for constraint validation.
//!
//! # Architecture
//!
//! The VM consists of:
//! - **Instruction Set**: 21 opcodes across 4 categories (load, comparison, check, control flow)
//! - **Registers**: 16 general-purpose registers (r0 hardwired to 0)
//! - **Constant Pool**: Typed constants (Sym, U64, I64, F64, Bytes, RefId, Tag)
//! - **Views**: Zero-copy views into atom, context, and query constraint data
//! - **Execution Context**: Full context including source allowlist and conflict index
//!
//! # Instruction Format (16 bytes)
//! ```text
//! +--------+--------+----------------+------------------+
//! |  op    |   a    |       b        |      imm         |
//! | u16    | u16    |     u32        |      u64         |
//! +--------+--------+----------------+------------------+
//! ```
//!
//! # Opcodes (21 total)
//!
//! ## Load Operations (1-4)
//! - `LD_ATOM_META`: Load atom metadata (valid_from, valid_to, atom_type, trust)
//! - `LD_CLAIM`: Load claim fields (subj, pred, obj_tag, obj_value, qmask)
//! - `LD_QC`: Load query constraints (time_ns, domain_mask, trust_min)
//! - `LD_CTX`: Load context policy fields
//!
//! ## Comparison Operations (10-17)
//! - `EQ`, `LT`, `LE`, `GT`, `GE`: Register comparisons
//! - `IN_RANGE`: Check if value in range [imm_low, imm_high]
//! - `HAS_BIT`: Test bit in value
//! - `IS_TAG`: Check object tag match
//!
//! ## Check Operations (20-25)
//! - `CHK_TIME`: Validate time overlap
//! - `CHK_TRUST`: Validate trust >= minimum
//! - `CHK_DOMAIN`: Validate domain mask intersection
//! - `CHK_SOURCE`: Validate source in allowlist
//! - `CTX_PROBE`: Search context for conflicts
//! - `RAISE_CONFLICT`: Raise conflict result
//!
//! ## Control Flow (30-32)
//! - `JZ`: Jump if zero
//! - `JMP`: Unconditional jump
//! - `RET`: Return with result code
//!
//! # Safety Invariants
//! - All instructions are 16-byte aligned
//! - Register indices are bounds-checked (0-15)
//! - Const pool indices are validated before access
//! - PC is validated before instruction fetch
//! - Step limit prevents infinite loops
//! - No raw pointer arithmetic in safe code paths
//!
//! See `interpreter` module for detailed usage examples.

// Re-export interpreter module
pub mod interpreter;

// Re-export all types from interpreter for backward compatibility
pub use interpreter::*;

// ============================================================================
// Module Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::AtomType;

    #[test]
    fn test_module_reexports() {
        // Verify all major types are re-exported
        let _opcode = Opcode::LD_ATOM_META;
        let _instr = Instruction::default();
        let _const = ConstValue::u64(42);
        let _builder = BytecodeBuilder::new();

        // Verify programs can be built
        let (instructions, pool) = build_basic_invariant_program();
        assert!(!instructions.is_empty());
        assert!(pool.is_empty());
    }

    #[test]
    fn test_all_opcodes_defined() {
        // Verify all 22 opcodes are defined and convertible
        let all_opcodes = [
            1, 2, 3, 4, 5, // Load
            10, 11, 12, 13, 14, 15, 16, 17, // Comparison
            20, 21, 22, 23, 24, 25, // Check
            30, 31, 32, // Control flow
        ];

        for op in all_opcodes {
            assert!(
                Opcode::from_u16(op).is_some(),
                "Opcode {} should be defined",
                op
            );
        }

        // Verify invalid opcodes return None
        assert!(Opcode::from_u16(0).is_none());
        assert!(Opcode::from_u16(6).is_none());
        assert!(Opcode::from_u16(9).is_none());
        assert!(Opcode::from_u16(18).is_none());
        assert!(Opcode::from_u16(28).is_none());
        assert!(Opcode::from_u16(29).is_none());
        assert!(Opcode::from_u16(35).is_none());
    }

    #[test]
    fn test_opcode_categories() {
        // Verify category methods work correctly
        assert!(Opcode::LD_ATOM_META.is_load());
        assert!(Opcode::LD_CLAIM.is_load());
        assert!(Opcode::LD_QC.is_load());
        assert!(Opcode::LD_CTX.is_load());
        assert!(Opcode::LD_IMM.is_load());

        assert!(Opcode::EQ.is_comparison());
        assert!(Opcode::LT.is_comparison());
        assert!(Opcode::LE.is_comparison());
        assert!(Opcode::GT.is_comparison());
        assert!(Opcode::GE.is_comparison());
        assert!(Opcode::IN_RANGE.is_comparison());
        assert!(Opcode::HAS_BIT.is_comparison());
        assert!(Opcode::IS_TAG.is_comparison());

        assert!(Opcode::CHK_TIME.is_check());
        assert!(Opcode::CHK_TRUST.is_check());
        assert!(Opcode::CHK_DOMAIN.is_check());
        assert!(Opcode::CHK_SOURCE.is_check());
        assert!(Opcode::CTX_PROBE.is_check());
        assert!(Opcode::RAISE_CONFLICT.is_check());

        assert!(Opcode::JZ.is_control_flow());
        assert!(Opcode::JMP.is_control_flow());
        assert!(Opcode::RET.is_control_flow());
    }

    #[test]
    fn test_instruction_format() {
        // Verify instruction size
        assert_eq!(std::mem::size_of::<Instruction>(), 16);

        // Create test instruction
        let instr = Instruction::new(Opcode::EQ, 1, 2, 0);
        assert_eq!(instr.opcode(), Some(Opcode::EQ));
        assert_eq!(instr.reg_a(), 1);
        assert_eq!(instr.reg_b(), 2);

        // Test immediate instruction
        let instr = Instruction::imm_op(Opcode::LD_CTX, 5, 0x12345678);
        assert_eq!(instr.reg_a(), 5);
        assert_eq!(instr.imm_u64(), 0x12345678);

        // Test jump instruction
        let instr = Instruction::jump(Opcode::JZ, 1, 100);
        assert_eq!(instr.jump_target(), 100);
    }

    #[test]
    fn test_const_value_types() {
        // Test all const value types
        let sym = ConstValue::sym(42);
        assert_eq!(sym.as_u64(), Some(42));

        let u64_val = ConstValue::u64(100);
        assert_eq!(u64_val.as_u64(), Some(100));

        let i64_pos = ConstValue::i64(50);
        assert_eq!(i64_pos.as_u64(), Some(50));
        assert_eq!(i64_pos.as_i64(), Some(50));

        let i64_neg = ConstValue::i64(-50);
        assert_eq!(i64_neg.as_u64(), None);
        assert_eq!(i64_neg.as_i64(), Some(-50));

        let f64_val = ConstValue::f64(std::f64::consts::PI);
        assert_eq!(f64_val.as_f64(), Some(std::f64::consts::PI));

        let tag = ConstValue::tag(5);
        assert_eq!(tag.as_tag(), Some(5));
        assert_eq!(tag.as_u64(), Some(5));

        let bytes = ConstValue::bytes([1u8; 32]);
        assert_eq!(bytes.as_bytes(), Some(&[1u8; 32][..]));
    }

    #[test]
    fn test_atom_view() {
        let atom_id = [0u8; 32];
        let claims = vec![
            ClaimData {
                subj: 1,
                pred: 2,
                obj_tag: 3,
                obj_val: 42,
                qualifiers_mask: 0,
            },
            ClaimData {
                subj: 5,
                pred: 6,
                obj_tag: 7,
                obj_val: 100,
                qualifiers_mask: 1,
            },
        ];

        let atom_view = AtomView::new(
            &atom_id,
            AtomType::FACT,
            &[1, 2, 3, 4],
            &claims,
            1000,
            2000,
            800,
            0x0F,
            42,
        );

        // Test validity checks
        assert!(atom_view.is_valid_at(1500));
        assert!(!atom_view.is_valid_at(500));
        assert!(!atom_view.is_valid_at(3000));

        // Test domain check
        assert!(atom_view.domain_matches(0x01));
        assert!(atom_view.domain_matches(0x0F));
        assert!(!atom_view.domain_matches(0x10));

        // Test trust check
        assert!(atom_view.trust_meets(500));
        assert!(atom_view.trust_meets(800));
        assert!(!atom_view.trust_meets(900));

        // Test metadata access
        assert_eq!(atom_view.get_meta(0), Some(1));
        assert_eq!(atom_view.get_meta(3), Some(4));
        assert_eq!(atom_view.get_meta(10), None);

        // Test claim access
        assert_eq!(atom_view.claim_count(), 2);
        let claim0 = atom_view.get_claim(0).unwrap();
        assert_eq!(claim0.subj, 1);
        assert_eq!(claim0.pred, 2);
    }

    #[test]
    fn test_ctx_view() {
        let ctx_view = CtxView::new(
            5,
            &[10, 20, 30],
            &[
                ConflictProbe {
                    pattern_hash: 0x100,
                    conflict_count: 2,
                    max_trust: 500,
                    flags: 0,
                },
                ConflictProbe {
                    pattern_hash: 0x200,
                    conflict_count: 0,
                    max_trust: 1000,
                    flags: 1,
                },
            ],
            3,
        );

        assert_eq!(ctx_view.ctx_id, 5);
        assert_eq!(ctx_view.active_branches, 3);
        assert_eq!(ctx_view.total_conflicts(), 2);
        assert!(ctx_view.has_conflicts());

        let probe = ctx_view.probe_conflict(0x100);
        assert!(probe.is_some());
        assert_eq!(probe.unwrap().conflict_count, 2);
    }

    #[test]
    fn test_query_constraints() {
        let qc = QueryConstraintsView::new(1000, 2000, 500, 0x0F, 0xFF, 100);

        assert_eq!(qc.time_from_ns, 1000);
        assert_eq!(qc.time_to_ns, 2000);
        assert_eq!(qc.trust_min, 500);
        assert_eq!(qc.domain_mask, 0x0F);
        assert_eq!(qc.max_results, 100);

        // Test atom type check
        assert!(qc.allows_atom_type(AtomType::FACT));
        assert!(qc.allows_atom_type(AtomType::DEFINITION));

        // Test time overlap
        assert!(qc.time_overlaps(1500, 1600));
        assert!(!qc.time_overlaps(0, 500));
    }

    #[test]
    fn test_ctx_index() {
        let mut ctx_index = CtxIndex::new();

        // Add conflicts
        ctx_index.add_conflict(0x100, [1u8; 32], ConflictSeverity::Hard);
        ctx_index.add_conflict(0x200, [2u8; 32], ConflictSeverity::Soft);

        assert!(ctx_index.has_conflict(0x100));
        assert!(ctx_index.has_conflict(0x200));
        assert!(!ctx_index.has_conflict(0x999));

        let info = ctx_index.get_conflict(0x100).unwrap();
        assert_eq!(info.severity, ConflictSeverity::Hard);
        assert_eq!(info.atom_ids.len(), 1);
    }

    #[test]
    fn test_execution_context() {
        let ctx_index = CtxIndex::new();
        let allowlist = [1, 2, 3, 10, 20];

        let exec_ctx =
            ExecutionContext::new(&[0u8; 64], Some(&[0u8; 32]), &ctx_index, Some(&allowlist));

        assert!(exec_ctx.is_source_allowed(1));
        assert!(exec_ctx.is_source_allowed(10));
        assert!(!exec_ctx.is_source_allowed(5));
        assert!(!exec_ctx.is_source_allowed(100));
    }

    #[test]
    fn test_vm_error_types() {
        // Test error display
        let err = VmError::InvalidOpcode(999);
        assert!(err.to_string().contains("03E7")); // Hex format

        let err = VmError::RegisterOutOfBounds(20);
        assert!(err.to_string().contains("20"));

        let err = VmError::StepLimitExceeded(1000);
        assert!(err.to_string().contains("1000"));
    }

    #[test]
    fn test_bytecode_validation() {
        // Valid program
        let instructions = vec![
            Instruction::new(Opcode::CHK_TIME, 0, 0, 0),
            Instruction::new(Opcode::RET, 0, 0, 0),
        ];
        assert!(BytecodeValidator::validate(&instructions, 0, 1000).is_ok());

        // Jump out of bounds
        let instructions = vec![
            Instruction::jump(Opcode::JMP, 0, 100),
            Instruction::new(Opcode::RET, 0, 0, 0),
        ];
        assert!(BytecodeValidator::validate(&instructions, 0, 1000).is_err());

        // No RET
        let instructions = vec![Instruction::new(Opcode::CHK_TIME, 0, 0, 0)];
        assert!(BytecodeValidator::validate(&instructions, 0, 1000).is_err());

        // Empty program
        let instructions: Vec<Instruction> = vec![];
        assert!(BytecodeValidator::validate(&instructions, 0, 1000).is_err());
    }

    #[test]
    fn test_all_builtin_programs() {
        // Test all built-in program builders
        let (instr, _pool) = build_basic_invariant_program();
        assert!(!instr.is_empty());

        let (instr, _pool) = build_conflict_probe_program();
        assert!(!instr.is_empty());

        let (instr, _pool) = build_full_validation_program(3);
        assert!(!instr.is_empty());

        let (instr, _pool) = build_time_range_program();
        assert!(!instr.is_empty());

        let (instr, _pool) = build_trust_threshold_program();
        assert!(!instr.is_empty());
    }
}
// Append pub mod abi to vm/mod.rs

pub mod abi;
pub use abi::{eval_invariants, InvariantResult};
