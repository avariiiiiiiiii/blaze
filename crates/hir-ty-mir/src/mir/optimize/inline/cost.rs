//! rustc's `CostChecker` (`compiler/rustc_mir_transform/src/cost_checker.rs`) and the leaf
//! check of `cross_crate_inlinable` (`cross_crate_inline.rs`). rustc's `LowerIntrinsics` has
//! made most intrinsic calls rvalues and statements before either looks at a body, and this
//! MIR keeps them calls: [`Call`] says what each was made, so it costs what rustc's does.

use crate::mir::{
    BasicBlockId, MirBody, Operand, OperandKind, StatementKind, StoredPlace, TerminatorKind,
};

use super::super::{block_index, successors};

const INSTR_COST: usize = 5;
const CALL_PENALTY: usize = 25;
const LANDINGPAD_PENALTY: usize = 50;
const RESUME_PENALTY: usize = 45;
const LARGE_SWITCH_PENALTY: usize = 20;
const CONST_SWITCH_BONUS: usize = 10;

/// The most statements a function without calls may have that rustc makes inlinable across
/// crates: `-Zcross-crate-inline-threshold`'s default.
const CROSS_CRATE_THRESHOLD: usize = 100;

/// What the callee of a call terminator is to the cost model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Call {
    /// A function with a body of its own, or one called through a pointer.
    Function,
    /// An intrinsic rustc keeps a call.
    Intrinsic,
    /// `ub_checks`, `contract_checks` or `overflow_checks`: rustc's
    /// `NullOp::RuntimeChecks`, `on` when the crate being compiled has the check.
    Check { on: bool },
    /// An intrinsic rustc makes an rvalue or a statement of `cost`; with `statement`, one
    /// the leaf check counts.
    Lowered { cost: usize, statement: bool },
    /// `unreachable`, which rustc makes an `Unreachable` terminator.
    Unreachable,
}

/// What rustc's `LowerIntrinsics` makes of a call of the intrinsic `name`, when it lowers it.
pub(super) fn lowered(name: &str) -> Option<Call> {
    Some(match name {
        "assume" | "forget" | "wrapping_add" | "wrapping_sub" | "wrapping_mul"
        | "three_way_compare" | "unchecked_add" | "unchecked_sub" | "unchecked_mul"
        | "unchecked_div" | "unchecked_rem" | "unchecked_shl" | "unchecked_shr"
        | "add_with_overflow" | "sub_with_overflow" | "mul_with_overflow" | "read_via_copy"
        | "write_via_move" | "discriminant_value" | "offset" | "slice_get_unchecked"
        | "transmute" | "transmute_unchecked" | "aggregate_raw_ptr" | "ptr_metadata" => {
            Call::Lowered { cost: INSTR_COST, statement: true }
        }
        "copy_nonoverlapping" => Call::Lowered { cost: CALL_PENALTY, statement: true },
        "size_of" | "align_of" => Call::Lowered { cost: 0, statement: true },
        "cold_path" => Call::Lowered { cost: 0, statement: false },
        "unreachable" => Call::Unreachable,
        _ => return None,
    })
}

/// rustc's `is_call_like`, of a call terminator that calls `call`.
fn call_like(call: Call) -> bool {
    matches!(call, Call::Function | Call::Intrinsic)
}

/// What inlining `body` costs, as rustc's `check_callee_mir_body` adds it up: its blocks
/// reachable from the start, the unwind edge of a drop of a value with nothing to drop
/// not followed. `overflow_checks` says the crate being compiled checks overflow;
/// `classify` sorts the callee of each call, `needs_drop` says what a drop drops.
pub(super) fn cost(
    body: &MirBody<'_>,
    overflow_checks: bool,
    mut classify: impl FnMut(&Operand) -> Call,
    mut needs_drop: impl FnMut(&StoredPlace) -> bool,
) -> usize {
    let mut calls: Vec<Option<Call>> = Vec::with_capacity(body.basic_blocks.len());
    let mut call_likes = 0usize;
    for (_, block) in body.basic_blocks.iter() {
        let asm = block
            .statements
            .iter()
            .filter(|it| matches!(it.kind, StatementKind::InlineAsm(_)))
            .count();
        let call = match block.terminator.as_ref().map(|it| &it.kind) {
            Some(TerminatorKind::Call { func, .. }) => Some(classify(func)),
            _ => None,
        };
        let terminator = match block.terminator.as_ref().map(|it| &it.kind) {
            Some(TerminatorKind::Drop { .. } | TerminatorKind::Assert { .. }) => true,
            Some(TerminatorKind::Call { .. }) => call.is_some_and(call_like),
            _ => false,
        };
        call_likes += asm + usize::from(terminator);
        calls.push(call);
    }
    let mut penalty = 0usize;
    let mut bonus = 0usize;
    if call_likes == 1 {
        bonus += CALL_PENALTY;
    }
    let landing_pad = |unwind: &Option<BasicBlockId>| {
        if unwind.is_some() { LANDINGPAD_PENALTY } else { 0 }
    };
    let mut visited = vec![false; body.basic_blocks.len()];
    let mut work = vec![body.start_block];
    while let Some(id) = work.pop() {
        if std::mem::replace(&mut visited[block_index(id)], true) {
            continue;
        }
        let block = &body.basic_blocks[id];
        for statement in &block.statements {
            match statement.kind {
                StatementKind::Assign(..) | StatementKind::InlineAsm(_) => penalty += INSTR_COST,
                StatementKind::FakeRead(_)
                | StatementKind::Deinit(_)
                | StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_)
                | StatementKind::Nop => {}
            }
        }
        let Some(terminator) = &block.terminator else { continue };
        match &terminator.kind {
            TerminatorKind::Drop { place, target, unwind } => {
                work.push(*target);
                if needs_drop(place) {
                    penalty += CALL_PENALTY + landing_pad(unwind);
                    work.extend(*unwind);
                }
                continue;
            }
            TerminatorKind::Call { func, target, cleanup, .. } => {
                let call = match calls[block_index(id)] {
                    Some(call) => call,
                    None => classify(func),
                };
                match call {
                    Call::Function => penalty += CALL_PENALTY + landing_pad(cleanup),
                    Call::Intrinsic => penalty += INSTR_COST + landing_pad(cleanup),
                    Call::Check { on } => {
                        if !on {
                            bonus += CALL_PENALTY;
                        }
                        work.extend(*target);
                        continue;
                    }
                    Call::Lowered { cost, .. } => {
                        penalty += cost;
                        work.extend(*target);
                        continue;
                    }
                    Call::Unreachable => {
                        bonus += INSTR_COST;
                        continue;
                    }
                }
            }
            TerminatorKind::SwitchInt { discr, targets } => {
                if matches!(discr.kind, OperandKind::Constant { .. } | OperandKind::Allocation { .. })
                {
                    bonus += CONST_SWITCH_BONUS;
                } else if targets.all_targets().len() > 3 {
                    penalty += LARGE_SWITCH_PENALTY;
                } else {
                    penalty += INSTR_COST;
                }
            }
            TerminatorKind::Assert { msg, cleanup, .. } => {
                penalty += if msg.is_optional_overflow_check() && !overflow_checks {
                    INSTR_COST
                } else {
                    CALL_PENALTY
                };
                penalty += landing_pad(cleanup);
            }
            TerminatorKind::UnwindResume => penalty += RESUME_PENALTY,
            TerminatorKind::Unreachable => bonus += INSTR_COST,
            TerminatorKind::Goto { .. }
            | TerminatorKind::Return
            | TerminatorKind::Abort
            | TerminatorKind::DropAndReplace { .. }
            | TerminatorKind::Yield { .. }
            | TerminatorKind::CoroutineDrop
            | TerminatorKind::FalseEdge { .. }
            | TerminatorKind::FalseUnwind { .. } => {}
        }
        work.extend(successors(&terminator.kind));
    }
    penalty.saturating_sub(bonus)
}

/// rustc's leaf check of `cross_crate_inlinable`: `body` calls nothing, unwinds nowhere and
/// has at most [`CROSS_CRATE_THRESHOLD`] statements. `trivially_copy` is rustc's
/// `is_trivially_pure_clone_copy` of what a drop drops.
pub(super) fn is_small_leaf(
    body: &MirBody<'_>,
    mut classify: impl FnMut(&Operand) -> Call,
    mut trivially_copy: impl FnMut(&StoredPlace) -> bool,
) -> bool {
    let (mut calls, mut resumes, mut landing_pads, mut statements) = (0usize, 0usize, 0usize, 0);
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            match statement.kind {
                StatementKind::Assign(..)
                | StatementKind::InlineAsm(_)
                | StatementKind::FakeRead(_) => statements += 1,
                StatementKind::Deinit(_)
                | StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_)
                | StatementKind::Nop => {}
            }
        }
        let Some(terminator) = &block.terminator else { continue };
        match &terminator.kind {
            TerminatorKind::Drop { place, unwind, .. } => {
                if !trivially_copy(place) {
                    calls += 1;
                    landing_pads += usize::from(unwind.is_some());
                }
            }
            TerminatorKind::Call { func, cleanup, .. } => match classify(func) {
                Call::Function => {
                    calls += 1;
                    landing_pads += usize::from(cleanup.is_some());
                }
                Call::Intrinsic => {}
                Call::Check { .. } => statements += 2,
                Call::Lowered { statement, .. } => statements += 1 + usize::from(statement),
                Call::Unreachable => statements += 1,
            },
            TerminatorKind::Assert { cleanup, .. } => {
                calls += 1;
                landing_pads += usize::from(cleanup.is_some());
            }
            TerminatorKind::UnwindResume => resumes += 1,
            TerminatorKind::Return => {}
            _ => statements += 1,
        }
    }
    calls == 0 && resumes == 0 && landing_pads == 0 && statements <= CROSS_CRATE_THRESHOLD
}
