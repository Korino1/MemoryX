//! ABI entry point for eval_invariants (SKF-1.1 Section 9.1)
//!
//! This module implements the top-level ABI function that evaluates invariant
//! bytecode stored in an atom's INVARIANTS section.
//!
//! # ABI Signature
//! `
//! eval_invariants(atom_view, claim_view?, ctx_view, query_constraints) -> ResultCode
//! `
//!
//! # Result Codes
//! - Pass (0): All invariants satisfied
//! - FailSoft { reason } (1): Soft invariant violation
//! - FailHard { reason } (2): Hard invariant violation
//! - NeedBranch { conflict_id } (3): Context conflict detected

#![allow(dead_code)]

use crate::cas::invariants::{decode_instructions, InvariantsSection};
use crate::cas::{AtomBodyHeader, SectionDesc};
use crate::prelude::*;
use crate::vm::interpreter::{
    AtomView, BytecodeBuilder, ClaimData, ConstValue, CtxIndex, CtxView,
    ExecutionContext, Instruction as VmInstruction, Opcode, QueryConstraintsView, VmInterpreter,
};
