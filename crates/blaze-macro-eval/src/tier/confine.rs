//! The confining pass (macros-tier.md, "What hir-ty-codegen must emit, and the confining
//! pass"): the IR hir-ty-codegen hands out for copy-and-patch (`hir_ty_codegen::v2::lowered`),
//! rewritten so the code it compiles to reaches nothing outside its region.
//!
//! - Each function takes the region's base as a last parameter (`vmctx`) and passes it on in
//!   every call, a host function's too: one more integer argument, which a C callee never
//!   reads, so every signature has it and a pointer of any function has one table signature.
//! - Every load, store and atomic at `p + off` goes to `vmctx + uextend(ireduce(p + off))`
//!   at offset 0, through a chain of its own: that is the pattern stencil fuses into one
//!   access (`Op::LoadConfined`, `Op::StoreConfined`), which it takes only unshared.
//! - Stack slots move to a shadow frame in the region, below the header's shadow stack
//!   pointer; each `stack_addr` is the frame's offset.
//! - `func_addr` and the address of a function as data are its table pointer; an indirect
//!   call checks its pointer against the table and the entry's signature before it calls.
//! - Whatever would give the macro an address of the machine (its stack, frame or return
//!   address, a stack switch, the pinned register, an exception handler's address) is
//!   refused, naming the function, as is any name the caller can't bind.
//!
//! The pass decides nothing about names: the caller says what each is ([`Name`]), from the
//! function's references and [`super::bind`].

use cranelift_codegen::{
    cursor::{Cursor as _, FuncCursor},
    ir::{
        self, ArgumentPurpose, InstBuilder as _, InstructionData, Opcode, condcodes::IntCC,
        immediates::Imm64, types,
    },
};

use super::{
    bind::{Refusal, Use, Why},
    region::{Layout, TAIL_GUARD, cell},
    table::{SIG, Sig, Signatures},
};
use crate::guest::Stop;

/// An indirect call's pointer is no entry of the table: misaligned, or past its end.
pub(crate) const BAD_FN_PTR: ir::TrapCode = ir::TrapCode::unwrap_user(240);
/// An indirect call's entry has another signature than the call's (the null entry has none).
pub(crate) const BAD_SIGNATURE: ir::TrapCode = ir::TrapCode::unwrap_user(241);

/// The namespace of the names the pass adds: [`Ref::Pointer`], [`Ref::Table`] and
/// [`Ref::TableSize`]. hir-ty-codegen's names are in 0 (functions) and 1 (data).
pub(crate) const NAMESPACE: u32 = 0x7469_6572;
const TABLE: u32 = u32::MAX;
const TABLE_SIZE: u32 = u32::MAX - 1;

/// The shadow stack's alignment: the machine's, the widest any slot asks for here.
const SHADOW_ALIGN: u32 = 16;

/// What a name of the function is, as the caller binds it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Name {
    /// Code: a function of the macro or a shim stub, called with `vmctx`; its address as
    /// data is its table pointer.
    Code,
    /// Data in the region: its address is its guest offset. It is never called.
    Data,
}

/// What each user name of the confined function is, by the index of
/// `func.params.user_named_funcs()`: what the loader resolves each relocation to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Ref {
    /// The function's own name `i`, as hir-ty-codegen's `names[i]` says: code at its code
    /// address, data at its guest offset.
    Given(u32),
    /// The table pointer of the code name `i`.
    Pointer(u32),
    /// The table's host address.
    Table,
    /// The table's size in bytes.
    TableSize,
}

/// What the pass gives besides the rewritten function.
#[derive(Debug)]
pub(crate) struct Confined {
    pub(crate) refs: Vec<Ref>,
    /// The table signature of the function itself, for an entry of its pointer.
    pub(crate) sig: Sig,
}

/// Confines `func`, the IR of the macro's function `function` (its name, for refusals),
/// in place. `name(i, used)` binds its user name `i`; it refuses what it can't bind.
pub(crate) fn confine(
    func: &mut ir::Function,
    function: &str,
    signatures: &mut Signatures,
    mut name: impl FnMut(u32, Use) -> Result<Name, Stop>,
) -> Result<Confined, Stop> {
    let refuse = |why: Why| {
        // alloc: the refusal, once per crate compiled
        Refusal { name: function.to_owned(), why }.stop()
    };
    let Some(entry) = func.layout.entry_block() else {
        // alloc: error path
        return Err(Stop(format!("has a function with no blocks, `{function}`")));
    };
    if !func.dynamic_stack_slots.is_empty() {
        return Err(refuse(Why::Operation("a dynamic stack slot")));
    }
    let given = func.params.user_named_funcs().len();
    if func.params.user_named_funcs().values().any(|it| it.namespace == NAMESPACE) {
        // alloc: error path
        return Err(Stop(format!("has a name in the compiled tier's own namespace in `{function}`")));
    }

    // Every signature takes `vmctx` last. A struct argument (x86-64's `byval`) is copied by the
    // call from the confined host address of the caller's guest pointer, so the copy reads at
    // most its size past the region's last byte, within the tail guard; the callee copies it
    // on to its shadow frame. None is returned.
    let vmctx_param = ir::AbiParam::special(types::I64, ArgumentPurpose::VMContext);
    let struct_return = |sig: &ir::Signature| {
        sig.returns.iter().any(|it| matches!(it.purpose, ArgumentPurpose::StructArgument(_)))
    };
    if struct_return(&func.signature) || func.dfg.signatures.values().any(struct_return) {
        return Err(refuse(Why::Operation("a struct returned on the machine's stack")));
    }
    let unconfinable = |sig: &ir::Signature| {
        sig.params.iter().any(|it| {
            matches!(it.purpose, ArgumentPurpose::StructArgument(size) if size as usize > TAIL_GUARD || size % 8 != 0)
        })
    };
    if unconfinable(&func.signature) || func.dfg.signatures.values().any(unconfinable) {
        return Err(refuse(Why::Operation("a struct argument past 64 KiB or of no whole words")));
    }
    let structs: Vec<(ir::Value, u32)> = func
        .signature
        .params
        .iter()
        .zip(func.dfg.block_params(entry))
        .filter_map(|(param, &value)| match param.purpose {
            ArgumentPurpose::StructArgument(size) => Some((value, size)),
            _ => None,
        })
        .collect(); // alloc: per function compiled, its struct parameters
    func.signature.params.push(vmctx_param);
    for sig in func.dfg.signatures.values_mut() {
        sig.params.push(vmctx_param);
    }
    let vmctx = func.dfg.append_block_param(entry, types::I64);

    // Names: what the code calls is code; what it takes the address of is bound as data, and
    // when it is code its address becomes its table pointer.
    for ext in func.dfg.ext_funcs.values_mut() {
        let ir::ExternalName::User(user) = ext.name else {
            return Err(refuse(Why::Operation("a call of a Cranelift library function")));
        };
        let index = user.as_u32();
        if index as usize >= given {
            // alloc: error path
            return Err(Stop(format!("calls an undeclared name in `{function}`")));
        }
        if name(index, Use::Call)? != Name::Code {
            return Err(refuse(Why::Operation("a call of data")));
        }
        ext.colocated = false;
    }
    let mut pointers: Vec<Option<ir::GlobalValue>> = vec![None; given]; // alloc: one slot per name, per function compiled
    let globals: Vec<ir::GlobalValue> = func.global_values.keys().collect(); // alloc: per function compiled, the values rewritten below
    for gv in globals {
        let ir::GlobalValueData::Symbol { name: ir::ExternalName::User(user), offset, tls, .. } = func.global_values[gv]
        else {
            return Err(refuse(Why::Operation("a global value that is no symbol of the function's names")));
        };
        let index = user.as_u32();
        if index as usize >= given {
            // alloc: error path
            return Err(Stop(format!("takes the address of an undeclared name in `{function}`")));
        }
        let user = match name(index, Use::Address)? {
            Name::Data => user,
            Name::Code if tls => return Err(refuse(Why::Operation("a thread-local function"))),
            Name::Code => declared(func, index),
        };
        // A thread-local is a static of the region: one thread runs in a region.
        func.global_values[gv] =
            ir::GlobalValueData::Symbol { name: ir::ExternalName::User(user), offset, colocated: false, tls: false };
    }

    // The shadow frame: each slot at its offset, the frame aligned as the machine's stack.
    // alloc: one offset per stack slot, per function compiled
    let mut slots: Vec<u32> = Vec::with_capacity(func.sized_stack_slots.len());
    let mut size: u32 = 0;
    for slot in func.sized_stack_slots.values() {
        let align = 1u32.checked_shl(u32::from(slot.align_shift)).unwrap_or(0);
        if align == 0 || align > SHADOW_ALIGN {
            return Err(refuse(Why::Operation("a stack slot aligned past 16 bytes")));
        }
        let Some(at) = size.checked_next_multiple_of(align) else {
            return Err(refuse(Why::Operation("a stack frame past 4 GiB")));
        };
        slots.push(at);
        let Some(end) = at.checked_add(slot.size) else {
            return Err(refuse(Why::Operation("a stack frame past 4 GiB")));
        };
        size = end;
    }
    // Each struct parameter's copy, after the slots.
    // alloc: one per struct parameter, per function compiled
    let mut copies: Vec<(ir::Value, u32, u32)> = Vec::with_capacity(structs.len());
    for &(from, len) in &structs {
        let Some(at) = size.checked_next_multiple_of(SHADOW_ALIGN) else {
            return Err(refuse(Why::Operation("a stack frame past 4 GiB")));
        };
        copies.push((from, at, len));
        let Some(end) = at.checked_add(len) else {
            return Err(refuse(Why::Operation("a stack frame past 4 GiB")));
        };
        size = end;
    }
    let Some(size) = size.checked_next_multiple_of(SHADOW_ALIGN) else {
        return Err(refuse(Why::Operation("a stack frame past 4 GiB")));
    };

    // alloc: per function compiled, its instructions in layout order before any is added
    let insts: Vec<ir::Inst> = func.layout.blocks().flat_map(|b| func.layout.block_insts(b)).collect();
    // alloc: per function compiled, the blocks an unwind lands in
    let mut handlers: Vec<ir::Block> = Vec::new();
    for &inst in &insts {
        if let InstructionData::TryCall { exception, .. } | InstructionData::TryCallIndirect { exception, .. } =
            func.dfg.insts[inst]
        {
            let table = &func.dfg.exception_tables[exception];
            for item in table.items() {
                match item {
                    ir::ExceptionTableItem::Tag(_, call) | ir::ExceptionTableItem::Default(call) => {
                        handlers.push(call.block(&func.dfg.value_lists));
                    }
                    ir::ExceptionTableItem::Context(_) => {}
                }
            }
        }
    }
    handlers.sort_unstable();
    handlers.dedup();

    // The frame: below the shadow stack pointer, the pointer moved to it until it returns.
    // A function that lands an unwind reads the pointer too, to put it back where its own
    // frame is when the unwind skipped the returns that would have.
    let trusted = ir::MemFlagsData::trusted();
    let sp_cell = (Layout::HEADER + cell::SHADOW_SP) as i32;
    let limit_cell = (Layout::HEADER + cell::SHADOW_LIMIT) as i32;
    let mut frame = None;
    // alloc: one per struct parameter, per function compiled
    let mut moved: Vec<(ir::Value, ir::Value)> = Vec::with_capacity(copies.len());
    if !slots.is_empty() || !handlers.is_empty() || !copies.is_empty() {
        let mut cur = FuncCursor::new(func).at_first_insertion_point(entry);
        let sp = cur.ins().load(types::I64, trusted, vmctx, sp_cell);
        let own = if size > 0 {
            let below = cur.ins().iadd_imm(sp, -i64::from(size));
            let lowest = cur.ins().load(types::I64, trusted, vmctx, limit_cell);
            let under = cur.ins().icmp(IntCC::UnsignedLessThan, below, lowest);
            cur.ins().trapnz(under, ir::TrapCode::STACK_OVERFLOW);
            let wrapped = cur.ins().icmp(IntCC::UnsignedGreaterThan, below, sp);
            cur.ins().trapnz(wrapped, ir::TrapCode::STACK_OVERFLOW);
            cur.ins().store(trusted, below, vmctx, sp_cell);
            below
        } else {
            sp
        };
        // The caller's copy is on the machine's stack, which no guest address names: its
        // words go to the frame, unconfined (the pass made both addresses), and the function
        // uses the frame's guest address from here on.
        for &(from, at, len) in &copies {
            let guest = cur.ins().iadd_imm(own, i64::from(at));
            let to = cur.ins().iadd(vmctx, guest);
            for word in (0..len).step_by(8) {
                let value = cur.ins().load(types::I64, trusted, from, word as i32);
                cur.ins().store(trusted, value, to, word as i32);
            }
            moved.push((from, guest));
        }
        frame = Some((sp, own));
    }
    if !moved.is_empty() {
        for &inst in &insts {
            func.dfg.map_inst_values(inst, |value| {
                moved.iter().find(|(from, _)| *from == value).map_or(value, |(_, guest)| *guest)
            });
        }
    }

    let mut table: Option<(ir::GlobalValue, ir::GlobalValue)> = None;
    for &inst in &insts {
        let opcode = func.dfg.insts[inst].opcode();
        match opcode {
            Opcode::GetFramePointer => return Err(refuse(Why::Operation("get_frame_pointer"))),
            Opcode::GetStackPointer => return Err(refuse(Why::Operation("get_stack_pointer"))),
            Opcode::GetReturnAddress => return Err(refuse(Why::Operation("get_return_address"))),
            Opcode::StackSwitch => return Err(refuse(Why::Operation("stack_switch"))),
            Opcode::GetPinnedReg | Opcode::SetPinnedReg => return Err(refuse(Why::Operation("the pinned register"))),
            Opcode::GetExceptionHandlerAddress => {
                return Err(refuse(Why::Operation("get_exception_handler_address")));
            }
            Opcode::DynamicStackAddr => return Err(refuse(Why::Operation("dynamic_stack_addr"))),
            _ => {}
        }
        match func.dfg.insts[inst] {
            InstructionData::Load { arg, offset, flags, .. } => {
                let at = confined(func, inst, vmctx, arg, offset.into(), function)?;
                let flags = trapping(func, flags);
                if let InstructionData::Load { arg, offset, flags: f, .. } = &mut func.dfg.insts[inst] {
                    (*arg, *offset, *f) = (at, 0.into(), flags);
                }
            }
            InstructionData::Store { args: [value, p], offset, flags, .. } => {
                let at = confined(func, inst, vmctx, p, offset.into(), function)?;
                let flags = trapping(func, flags);
                if let InstructionData::Store { args, offset, flags: f, .. } = &mut func.dfg.insts[inst] {
                    (*args, *offset, *f) = ([value, at], 0.into(), flags);
                }
            }
            InstructionData::LoadNoOffset { arg, flags, .. } if opcode == Opcode::AtomicLoad => {
                let at = confined(func, inst, vmctx, arg, 0, function)?;
                let flags = trapping(func, flags);
                if let InstructionData::LoadNoOffset { arg, flags: f, .. } = &mut func.dfg.insts[inst] {
                    (*arg, *f) = (at, flags);
                }
            }
            InstructionData::StoreNoOffset { args: [_, p], .. }
            | InstructionData::AtomicRmw { args: [p, _], .. }
            | InstructionData::AtomicCas { args: [p, _, _], .. } => {
                let at = confined(func, inst, vmctx, p, 0, function)?;
                let slot = match &mut func.dfg.insts[inst] {
                    InstructionData::StoreNoOffset { args: [_, p], .. }
                    | InstructionData::AtomicRmw { args: [p, _], .. }
                    | InstructionData::AtomicCas { args: [p, _, _], .. } => p,
                    _ => unreachable!("matched above"),
                };
                *slot = at;
                if let Some(flags) = func.dfg.insts[inst].memflags() {
                    let flags = trapping(func, flags);
                    if let Some(f) = func.dfg.insts[inst].memflags_mut() {
                        *f = flags;
                    }
                }
            }
            InstructionData::StackAddr { stack_slot, offset, .. } => {
                let ty = func.dfg.value_type(func.dfg.first_result(inst));
                let Some((_, own)) = frame else { unreachable!("a stack slot makes a frame") };
                if ty != types::I64 {
                    return Err(refuse(Why::Operation("a stack address that is no i64")));
                }
                let at = i64::from(slots[stack_slot.as_u32() as usize]) + i64::from(i32::from(offset));
                func.replace(inst).iadd_imm(own, Imm64::new(at));
            }
            InstructionData::UnaryGlobalValue { global_value, .. } if opcode == Opcode::TlsValue => {
                let ty = func.dfg.value_type(func.dfg.first_result(inst));
                func.replace(inst).symbol_value(ty, global_value);
            }
            InstructionData::FuncAddr { func_ref, .. } => {
                let ty = func.dfg.value_type(func.dfg.first_result(inst));
                let ir::ExternalName::User(user) = func.dfg.ext_funcs[func_ref].name else {
                    unreachable!("every ext func was checked to be a user name")
                };
                let index = user.as_u32();
                let gv = match pointers[index as usize] {
                    Some(gv) => gv,
                    None => {
                        let name = declared(func, index);
                        let gv = symbol(func, name);
                        pointers[index as usize] = Some(gv);
                        gv
                    }
                };
                func.replace(inst).symbol_value(ty, gv);
            }
            InstructionData::Call { func_ref, .. } | InstructionData::TryCall { func_ref, .. } => {
                if opcode == Opcode::ReturnCall {
                    restore(func, inst, vmctx, frame, sp_cell);
                }
                let sig_ref = func.dfg.ext_funcs[func_ref].signature;
                struct_args(func, inst, vmctx, sig_ref, 0, function)?;
                push_arg(func, inst, vmctx);
            }
            InstructionData::CallIndirect { .. } | InstructionData::TryCallIndirect { .. } => {
                let sig_ref = match func.dfg.insts[inst] {
                    InstructionData::CallIndirect { sig_ref, .. } => sig_ref,
                    InstructionData::TryCallIndirect { exception, .. } => func.dfg.exception_tables[exception].signature(),
                    _ => unreachable!("matched above"),
                };
                let sig = intern(signatures, &func.dfg.signatures[sig_ref]);
                let (base, end) = match table {
                    Some(it) => it,
                    None => {
                        let (base, end) = (declared(func, TABLE), declared(func, TABLE_SIZE));
                        let it = (symbol(func, base), symbol(func, end));
                        table = Some(it);
                        it
                    }
                };
                if opcode == Opcode::ReturnCallIndirect {
                    restore(func, inst, vmctx, frame, sp_cell);
                }
                let pointer = func.dfg.inst_args(inst)[0];
                if func.dfg.value_type(pointer) != types::I64 {
                    return Err(refuse(Why::Operation("a function pointer that is no i64")));
                }
                let code = checked(func, inst, pointer, sig, base, end);
                func.dfg.inst_args_mut(inst)[0] = code;
                struct_args(func, inst, vmctx, sig_ref, 1, function)?;
                push_arg(func, inst, vmctx);
            }
            InstructionData::MultiAry { .. } if opcode == Opcode::Return => {
                restore(func, inst, vmctx, frame, sp_cell);
            }
            _ => {}
        }
    }

    if let Some((_, own)) = frame {
        for block in handlers {
            let mut cur = FuncCursor::new(func).at_first_insertion_point(block);
            cur.ins().store(trusted, own, vmctx, sp_cell);
        }
    }
    func.sized_stack_slots.clear();

    let sig = intern(signatures, &func.signature);
    let refs = func
        .params
        .user_named_funcs()
        .values()
        .enumerate()
        .map(|(i, it)| match (it.namespace, it.index) {
            (NAMESPACE, TABLE) => Ref::Table,
            (NAMESPACE, TABLE_SIZE) => Ref::TableSize,
            (NAMESPACE, index) => Ref::Pointer(index),
            _ => Ref::Given(i as u32),
        })
        .collect(); // alloc: one per name, per function compiled
    Ok(Confined { refs, sig })
}

/// The tier's name `index`: the pointer of the code name `index`, or [`TABLE`] or
/// [`TABLE_SIZE`].
fn declared(func: &mut ir::Function, index: u32) -> ir::UserExternalNameRef {
    func.declare_imported_user_function(ir::UserExternalName::new(NAMESPACE, index))
}

/// A symbol the loader writes as an absolute address.
fn symbol(func: &mut ir::Function, name: ir::UserExternalNameRef) -> ir::GlobalValue {
    func.create_global_value(ir::GlobalValueData::Symbol {
        name: ir::ExternalName::User(name),
        offset: Imm64::new(0),
        colocated: false,
        tls: false,
    })
}

/// `flags` with the access able to trap, as a confined one may at a guard.
fn trapping(func: &mut ir::Function, flags: ir::MemFlags) -> ir::MemFlags {
    let data = func.dfg.mem_flags[flags];
    let code = data.trap_code().unwrap_or(ir::TrapCode::HEAP_OUT_OF_BOUNDS);
    func.dfg.mem_flags.insert_unchecked(data.with_trap_code(Some(code)))
}

/// `vmctx + uextend(ireduce(p + offset))`, before `inst`: the access's own chain, which
/// nothing else uses.
fn confined(
    func: &mut ir::Function,
    inst: ir::Inst,
    vmctx: ir::Value,
    p: ir::Value,
    offset: i32,
    function: &str,
) -> Result<ir::Value, Stop> {
    let ty = func.dfg.value_type(p);
    let mut cur = FuncCursor::new(func).at_inst(inst);
    let sum = if offset != 0 { cur.ins().iadd_imm(p, i64::from(offset)) } else { p };
    let low = match ty {
        types::I64 => cur.ins().ireduce(types::I32, sum),
        types::I32 => sum,
        _ => {
            // alloc: the refusal, once per crate compiled
            let refusal = Refusal { name: function.to_owned(), why: Why::Operation("an address that is no i64") };
            return Err(refusal.stop());
        }
    };
    let wide = cur.ins().uextend(types::I64, low);
    Ok(cur.ins().iadd(vmctx, wide))
}

/// The shadow stack pointer as the function found it, stored back before `inst` leaves it.
fn restore(func: &mut ir::Function, inst: ir::Inst, vmctx: ir::Value, frame: Option<(ir::Value, ir::Value)>, sp_cell: i32) {
    if let Some((sp, own)) = frame
        && sp != own
    {
        FuncCursor::new(func).at_inst(inst).ins().store(ir::MemFlagsData::trusted(), sp, vmctx, sp_cell);
    }
}

/// Each struct argument of the call `inst` (of signature `sig_ref`, its arguments from `skip`
/// on) given as the confined host address of its guest pointer, which the call copies from.
fn struct_args(
    func: &mut ir::Function,
    inst: ir::Inst,
    vmctx: ir::Value,
    sig_ref: ir::SigRef,
    skip: usize,
    function: &str,
) -> Result<(), Stop> {
    for at in 0..func.dfg.signatures[sig_ref].params.len() {
        if !matches!(func.dfg.signatures[sig_ref].params[at].purpose, ArgumentPurpose::StructArgument(_)) {
            continue;
        }
        let Some(&p) = func.dfg.inst_args(inst).get(skip + at) else { continue };
        let host = confined(func, inst, vmctx, p, 0, function)?;
        func.dfg.inst_args_mut(inst)[skip + at] = host;
    }
    Ok(())
}

/// `vmctx` as the call's last argument.
fn push_arg(func: &mut ir::Function, inst: ir::Inst, vmctx: ir::Value) {
    let ir::DataFlowGraph { insts, value_lists, .. } = &mut func.dfg;
    match &mut insts[inst] {
        InstructionData::Call { args, .. }
        | InstructionData::TryCall { args, .. }
        | InstructionData::CallIndirect { args, .. }
        | InstructionData::TryCallIndirect { args, .. } => {
            args.push(vmctx, value_lists);
        }
        _ => unreachable!("only calls take arguments here"),
    }
}

/// The code address of the table entry `pointer`, before `inst`, trapping unless it is an
/// entry ([`BAD_FN_PTR`]) of signature `sig` ([`BAD_SIGNATURE`]).
fn checked(
    func: &mut ir::Function,
    inst: ir::Inst,
    pointer: ir::Value,
    sig: Sig,
    base: ir::GlobalValue,
    end: ir::GlobalValue,
) -> ir::Value {
    let flags = ir::MemFlagsData::trusted().with_readonly();
    let mut cur = FuncCursor::new(func).at_inst(inst);
    let misaligned = cur.ins().band_imm(pointer, 15);
    cur.ins().trapnz(misaligned, BAD_FN_PTR);
    let size = cur.ins().symbol_value(types::I64, end);
    let past = cur.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, pointer, size);
    cur.ins().trapnz(past, BAD_FN_PTR);
    let table = cur.ins().symbol_value(types::I64, base);
    let entry = cur.ins().iadd(table, pointer);
    let found = cur.ins().load(types::I32, flags, entry, SIG);
    let same = cur.ins().icmp_imm(IntCC::Equal, found, i64::from(sig.0));
    cur.ins().trapz(same, BAD_SIGNATURE);
    cur.ins().load(types::I64, flags, entry, 0)
}

/// The table signature of `sig`: its parameters, returns, purposes, extensions and calling
/// convention, as Cranelift prints them.
fn intern(signatures: &mut Signatures, sig: &ir::Signature) -> Sig {
    // alloc: once per signature met, per function compiled: its interning key
    let text = sig.to_string();
    signatures.intern(text.as_bytes())
}

#[cfg(test)]
mod tests;
