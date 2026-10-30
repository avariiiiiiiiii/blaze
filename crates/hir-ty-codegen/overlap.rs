//! The locals of a body whose storage never overlaps share memory in the frame, as LLVM's
//! stack colouring has them share it from the lifetime markers rustc writes. Cranelift gives
//! every slot room of its own, so a function that matches on many shapes, each arm with its
//! temporaries, had a frame of all of them at once: hir-ide's `<Ty as HirDisplay>::hir_fmt`
//! had one of 20 KB, and its tests that print a type nested two hundred deep overflowed a
//! test thread's stack of 2 MB.
//!
//! Run on the function as [`promote`](super::promote) leaves it, just before Cranelift or
//! LLVM: a local's slot that nothing addresses any more takes no room at all, and the rest are
//! put together greedily, the largest first, into one slot per group of locals of which no two
//! conflict ([`FrameStorage::frame_storage_conflicts`](hir_ty_mir::mir::FrameStorage::frame_storage_conflicts)).

use cranelift_codegen::{
    entity::{EntitySet, PackedOption, SecondaryMap},
    ir::{self, InstructionData},
};
use rustc_hash::FxHashMap;
use rustc_index::bit_set::{BitMatrix, DenseBitSet};

use super::fragment::Named;

/// The slots of a body's locals, and which of them may hold a value at the same time.
pub(super) struct FrameLocals {
    /// `None` for a local kept in a register that never needed memory.
    pub(super) slots: Vec<Option<ir::StackSlot>>,
    /// By index in `slots`.
    pub(super) conflicts: BitMatrix<usize, usize>,
    /// Where in `slots` a local kept in a register puts the slot it gets when it needs one.
    pub(super) lazy: FxHashMap<hir_ty_mir::mir::LocalId, usize>,
}

/// Shares the slots of `frame` between locals that never hold storage at once, and has
/// `named` say where each named local now is.
pub(super) fn share(func: &mut ir::Function, frame: &FrameLocals, named: &mut [Named]) {
    let mut slots: Vec<ir::StackSlot> = named.iter().map(|it| it.slot).collect();
    share_slots(func, frame, &mut slots, None);
    for (it, slot) in named.iter_mut().zip(slots) {
        it.slot = slot;
    }
}

/// [`share`] for LLVM, on a copy of `func` with `named` in it: only slots whose address
/// escapes share. LLVM's SROA takes the others apart into values, which a slot shared with
/// one whose address escapes would keep it from; LLVM's own stack colouring shares none,
/// as the IR has no lifetime markers.
pub(super) fn share_escaping(
    func: &ir::Function,
    frame: &FrameLocals,
    named: &mut [ir::StackSlot],
) -> ir::Function {
    let mut shared = func.clone();
    let escaping = escaping(func);
    share_slots(&mut shared, frame, named, Some(&escaping));
    shared
}

/// The slots whose address goes anywhere but to the address of a load or a store, an
/// offset from it, or a `memcpy`, `memmove` or `memset`.
fn escaping(func: &ir::Function) -> EntitySet<ir::StackSlot> {
    let dfg = &func.dfg;
    // What each address of a slot, or an offset from one, is of, indexed by value.
    let mut of: SecondaryMap<ir::Value, PackedOption<ir::StackSlot>> = SecondaryMap::new();
    let address_of = |of: &SecondaryMap<ir::Value, PackedOption<ir::StackSlot>>, value| {
        of[dfg.resolve_aliases(value)].expand()
    };
    let mut known = 0;
    loop {
        let before = known;
        for block in func.layout.blocks() {
            for inst in func.layout.block_insts(block) {
                let slot = match dfg.insts[inst] {
                    InstructionData::StackAddr { stack_slot, .. } => Some(stack_slot),
                    InstructionData::Binary { opcode: ir::Opcode::Iadd, args } => {
                        args.iter().find_map(|&it| address_of(&of, it))
                    }
                    _ => None,
                };
                if let Some(slot) = slot {
                    let value = dfg.first_result(inst);
                    known += usize::from(of[value].is_none());
                    of[value] = slot.into();
                }
            }
        }
        if known == before {
            break;
        }
    }
    let mut escaping = EntitySet::with_capacity(func.sized_stack_slots.len());
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            let data = &dfg.insts[inst];
            let followed = match *data {
                InstructionData::Load { .. } | InstructionData::StackAddr { .. } => true,
                InstructionData::Store { args: [value, _], .. } => {
                    if let Some(slot) = address_of(&of, value) {
                        escaping.insert(slot);
                    }
                    true
                }
                InstructionData::Binary { opcode: ir::Opcode::Iadd, .. } => true,
                InstructionData::Call { func_ref, .. } => matches!(
                    dfg.ext_funcs[func_ref].name,
                    ir::ExternalName::LibCall(
                        ir::LibCall::Memcpy | ir::LibCall::Memmove | ir::LibCall::Memset
                    )
                ),
                _ => false,
            };
            if !followed {
                for slot in dfg.inst_values(inst).filter_map(|it| address_of(&of, it)) {
                    escaping.insert(slot);
                }
            }
        }
    }
    escaping
}

/// Shares the slots of `frame`, those in `only` alone when it is given, and moves the slots
/// of `named` along.
fn share_slots(
    func: &mut ir::Function,
    frame: &FrameLocals,
    named: &mut [ir::StackSlot],
    only: Option<&EntitySet<ir::StackSlot>>,
) {
    if frame.slots.is_empty() {
        return;
    }
    // An address of a slot that nothing uses is what `promote` left of its loads and stores.
    let mut used = EntitySet::with_capacity(func.dfg.num_values());
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            for value in func.dfg.inst_values(inst) {
                used.insert(func.dfg.resolve_aliases(value));
            }
        }
    }
    let mut addressed = vec![false; func.sized_stack_slots.len()];
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            let Some(slot) = slot_of(&func.dfg.insts[inst]) else { continue };
            if used.contains(func.dfg.first_result(inst)) {
                addressed[slot.as_u32() as usize] = true;
            }
        }
    }
    for slot in named.iter() {
        addressed[slot.as_u32() as usize] = true;
    }
    let mut order: Vec<usize> = Vec::new();
    for (at, slot) in frame.slots.iter().enumerate() {
        let Some(slot) = *slot else { continue };
        if !addressed[slot.as_u32() as usize] {
            func.sized_stack_slots[slot].size = 0;
        } else if only.is_none_or(|only| only.contains(slot)) {
            order.push(at);
        }
    }
    let slot = |at: usize| frame.slots[at].expect("a local's slot");
    let size = |func: &ir::Function, at: usize| func.sized_stack_slots[slot(at)].size;
    order.sort_by_key(|&at| (std::cmp::Reverse(size(func, at)), at));
    // Each group: the slot it keeps, and every local any of its members conflicts with (the
    // rows of its members, or'ed), so that whether one more fits is one bit.
    let mut groups: Vec<(ir::StackSlot, DenseBitSet<usize>)> = Vec::new();
    let mut moved: SecondaryMap<ir::StackSlot, PackedOption<ir::StackSlot>> = SecondaryMap::new();
    let mut any = false;
    for at in order {
        let found = groups.iter().position(|group| !group.1.contains(at));
        let first = found.is_none();
        let index = found.unwrap_or_else(|| {
            groups.push((slot(at), DenseBitSet::new_empty(frame.slots.len())));
            groups.len() - 1
        });
        let into = &mut groups[index];
        for it in frame.conflicts.iter(at) {
            into.1.insert(it);
        }
        if first {
            continue;
        }
        let (kept, slot) = (into.0, slot(at));
        let (size, align_shift) =
            (func.sized_stack_slots[slot].size, func.sized_stack_slots[slot].align_shift);
        let into = &mut func.sized_stack_slots[kept];
        into.size = into.size.max(size);
        into.align_shift = into.align_shift.max(align_shift);
        func.sized_stack_slots[slot].size = 0;
        moved[slot] = kept.into();
        any = true;
    }
    if !any {
        return;
    }
    let stencil = &mut func.stencil;
    for block in stencil.layout.blocks() {
        for inst in stencil.layout.block_insts(block) {
            if let Some(slot) = slot_mut(&mut stencil.dfg.insts[inst])
                && let Some(kept) = moved[*slot].expand()
            {
                *slot = kept;
            }
        }
    }
    for slot in named.iter_mut() {
        if let Some(kept) = moved[*slot].expand() {
            *slot = kept;
        }
    }
}

/// The slot an instruction addresses: only `stack_addr` names one, `stack_load` and
/// `stack_store` being built of it.
fn slot_of(inst: &InstructionData) -> Option<ir::StackSlot> {
    match *inst {
        InstructionData::StackAddr { stack_slot, .. } => Some(stack_slot),
        _ => None,
    }
}

fn slot_mut(inst: &mut InstructionData) -> Option<&mut ir::StackSlot> {
    match inst {
        InstructionData::StackAddr { stack_slot, .. } => Some(stack_slot),
        _ => None,
    }
}
