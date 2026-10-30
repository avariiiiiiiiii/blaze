use cranelift_codegen::{
    cursor::{Cursor as _, FuncCursor},
    entity::EntityRef as _,
    ir::{self, InstBuilder as _, Opcode, types},
    settings, verify_function,
};

use super::*;

/// A function of `params` and `returns` (i64s) named by `names` user names, built by `body`.
fn function(params: usize, returns: usize, names: u32, body: impl FnOnce(&mut FuncCursor<'_>, &[ir::Value])) -> ir::Function {
    let mut sig = ir::Signature::new(cranelift_codegen::isa::CallConv::SystemV);
    sig.params.extend((0..params).map(|_| ir::AbiParam::new(types::I64)));
    sig.returns.extend((0..returns).map(|_| ir::AbiParam::new(types::I64)));
    let mut func = ir::Function::with_name_signature(ir::UserFuncName::default(), sig);
    for index in 0..names {
        func.declare_imported_user_function(ir::UserExternalName::new(0, index));
    }
    let block = func.dfg.make_block();
    func.layout.append_block(block);
    let args: Vec<ir::Value> = (0..params).map(|_| func.dfg.append_block_param(block, types::I64)).collect();
    let mut cur = FuncCursor::new(&mut func).at_bottom(block);
    body(&mut cur, &args);
    func
}

fn verified(func: &ir::Function) {
    let flags = settings::Flags::new(settings::builder());
    if let Err(errors) = verify_function(func, &flags) {
        panic!("{}\n{}", errors, func.display());
    }
}

fn opcodes(func: &ir::Function) -> Vec<Opcode> {
    func.layout.blocks().flat_map(|b| func.layout.block_insts(b)).map(|i| func.dfg.insts[i].opcode()).collect()
}

fn code(_: u32, _: Use) -> Result<Name, Stop> {
    Ok(Name::Code)
}

fn vmctx(func: &ir::Function) -> ir::Value {
    let entry = func.layout.entry_block().unwrap();
    *func.dfg.block_params(entry).last().unwrap()
}

#[test]
fn every_access_goes_through_a_chain_of_its_own_at_offset_zero() {
    let mut func = function(1, 1, 0, |cur, p| {
        let v = cur.ins().load(types::I64, ir::MemFlagsData::trusted(), p[0], 8);
        cur.ins().store(ir::MemFlagsData::trusted(), v, p[0], 16);
        let w = cur.ins().atomic_load(types::I64, ir::MemFlagsData::trusted(), p[0]);
        cur.ins().return_(&[w]);
    });
    let mut signatures = Signatures::default();
    confine(&mut func, "f", &mut signatures, code).unwrap();
    verified(&func);
    let base = vmctx(&func);
    let mut seen = 0;
    for inst in func.layout.blocks().flat_map(|b| func.layout.block_insts(b)) {
        let data = func.dfg.insts[inst];
        let (at, offset) = match data {
            ir::InstructionData::Load { arg, offset, .. } => (arg, i32::from(offset)),
            ir::InstructionData::Store { args: [_, p], offset, .. } => (p, i32::from(offset)),
            ir::InstructionData::LoadNoOffset { arg, .. } => (arg, 0),
            _ => continue,
        };
        seen += 1;
        assert_eq!(offset, 0);
        assert!(func.dfg.insts[inst].memflags_trap_code(&func.dfg).is_some(), "a confined access may trap");
        let add = func.dfg.value_def(at).unwrap_inst();
        assert_eq!(func.dfg.insts[add].opcode(), Opcode::Iadd);
        assert_eq!(func.dfg.inst_args(add)[0], base);
        let wide = func.dfg.value_def(func.dfg.inst_args(add)[1]).unwrap_inst();
        assert_eq!(func.dfg.insts[wide].opcode(), Opcode::Uextend);
        let low = func.dfg.value_def(func.dfg.inst_args(wide)[0]).unwrap_inst();
        assert_eq!(func.dfg.insts[low].opcode(), Opcode::Ireduce);
    }
    assert_eq!(seen, 3);
    let ops = opcodes(&func);
    assert_eq!(ops.iter().filter(|&&it| it == Opcode::IaddImm).count(), 2, "the two offsets, one each");
    assert_eq!(func.signature.params.last().unwrap().purpose, ArgumentPurpose::VMContext);
}

#[test]
fn stack_slots_move_to_a_shadow_frame_given_back_on_return() {
    let mut func = function(0, 1, 0, |cur, _| {
        let slot = cur.func.create_sized_stack_slot(ir::StackSlotData::new(ir::StackSlotKind::ExplicitSlot, 24, 3));
        let at = cur.ins().stack_addr(types::I64, slot, 8);
        cur.ins().return_(&[at]);
    });
    confine(&mut func, "f", &mut Signatures::default(), code).unwrap();
    verified(&func);
    assert!(func.sized_stack_slots.is_empty());
    let ops = opcodes(&func);
    assert!(!ops.contains(&Opcode::StackAddr));
    assert_eq!(ops.iter().filter(|&&it| it == Opcode::Trapnz).count(), 2, "under the limit, and no wrap");
    assert_eq!(ops.iter().filter(|&&it| it == Opcode::Store).count(), 2, "the frame, then the pointer given back");
    let ret = ops.iter().rposition(|&it| it == Opcode::Return).unwrap();
    assert_eq!(ops[ret - 1], Opcode::Store);
}

#[test]
fn an_indirect_call_checks_its_pointer_and_signature() {
    let mut func = function(1, 0, 0, |cur, p| {
        let sig = cur.func.import_signature(ir::Signature::new(cranelift_codegen::isa::CallConv::SystemV));
        cur.ins().call_indirect(sig, p[0], &[]);
        cur.ins().return_(&[]);
    });
    let mut signatures = Signatures::default();
    let confined = confine(&mut func, "f", &mut signatures, code).unwrap();
    verified(&func);
    let ops = opcodes(&func);
    assert_eq!(ops.iter().filter(|&&it| it == Opcode::Trapnz).count(), 2);
    assert_eq!(ops.iter().filter(|&&it| it == Opcode::Trapz).count(), 1);
    assert!(confined.refs.contains(&Ref::Table) && confined.refs.contains(&Ref::TableSize));
    let call = func
        .layout
        .blocks()
        .flat_map(|b| func.layout.block_insts(b))
        .find(|&i| func.dfg.insts[i].opcode() == Opcode::CallIndirect)
        .unwrap();
    assert_eq!(*func.dfg.inst_args(call).last().unwrap(), vmctx(&func));
    assert_eq!(signatures.len(), 2, "the call's `(i64 vmctx)` and the function's `(i64, i64 vmctx)`");
    assert_eq!(confined.sig, signatures.intern(func.signature.to_string().as_bytes()));
}

#[test]
fn a_function_address_is_its_table_pointer() {
    let mut func = function(0, 1, 1, |cur, _| {
        let sig = cur.func.import_signature(ir::Signature::new(cranelift_codegen::isa::CallConv::SystemV));
        let callee = cur.func.import_function(ir::ExtFuncData {
            name: ir::ExternalName::User(ir::UserExternalNameRef::new(0)),
            signature: sig,
            colocated: true,
            patchable: false,
        });
        cur.ins().call(callee, &[]);
        let at = cur.ins().func_addr(types::I64, callee);
        cur.ins().return_(&[at]);
    });
    let confined = confine(&mut func, "f", &mut Signatures::default(), code).unwrap();
    verified(&func);
    assert_eq!(confined.refs, [Ref::Given(0), Ref::Pointer(0)]);
    assert!(!opcodes(&func).contains(&Opcode::FuncAddr));
    assert!(func.dfg.ext_funcs.values().all(|it| !it.colocated));
}

#[test]
fn what_reaches_the_machine_is_refused_naming_the_function() {
    let mut func = function(0, 1, 0, |cur, _| {
        let fp = cur.ins().get_frame_pointer(types::I64);
        cur.ins().return_(&[fp]);
    });
    let Stop(message) = confine(&mut func, "my_macro::f", &mut Signatures::default(), code).unwrap_err();
    assert!(message.contains("get_frame_pointer") && message.contains("my_macro::f"), "{message}");
}

/// The operand of an access as the pass leaves it: `vmctx + uextend(ireduce(p))`, giving `p`.
fn confined_operand(func: &ir::Function, at: ir::Value) -> ir::Value {
    let add = func.dfg.value_def(at).unwrap_inst();
    assert_eq!(func.dfg.insts[add].opcode(), Opcode::Iadd);
    assert_eq!(func.dfg.inst_args(add)[0], vmctx(func));
    let wide = func.dfg.value_def(func.dfg.inst_args(add)[1]).unwrap_inst();
    assert_eq!(func.dfg.insts[wide].opcode(), Opcode::Uextend);
    let low = func.dfg.value_def(func.dfg.inst_args(wide)[0]).unwrap_inst();
    assert_eq!(func.dfg.insts[low].opcode(), Opcode::Ireduce);
    func.dfg.inst_args(low)[0]
}

/// A function of one struct parameter of `size` bytes that loads its first word.
fn takes_struct(size: u32) -> ir::Function {
    let mut sig = ir::Signature::new(cranelift_codegen::isa::CallConv::SystemV);
    sig.params.push(ir::AbiParam::special(types::I64, ir::ArgumentPurpose::StructArgument(size)));
    sig.returns.push(ir::AbiParam::new(types::I64));
    let mut func = ir::Function::with_name_signature(ir::UserFuncName::default(), sig);
    let block = func.dfg.make_block();
    func.layout.append_block(block);
    let p = func.dfg.append_block_param(block, types::I64);
    let mut cur = FuncCursor::new(&mut func).at_bottom(block);
    let v = cur.ins().load(types::I64, ir::MemFlagsData::trusted(), p, 0);
    cur.ins().return_(&[v]);
    func
}

#[test]
fn a_struct_parameter_is_copied_to_the_shadow_frame_and_used_there() {
    let mut func = takes_struct(16);
    let incoming = func.dfg.block_params(func.layout.entry_block().unwrap())[0];
    confine(&mut func, "f", &mut Signatures::default(), code).unwrap();
    verified(&func);
    let insts: Vec<ir::Inst> = func.layout.blocks().flat_map(|b| func.layout.block_insts(b)).collect();
    let copies: Vec<i32> = insts
        .iter()
        .filter_map(|&inst| match func.dfg.insts[inst] {
            ir::InstructionData::Load { arg, offset, .. } if arg == incoming => Some(i32::from(offset)),
            _ => None,
        })
        .collect();
    assert_eq!(copies, [0, 8], "each word of the caller's copy is read once, unconfined");
    let returned = insts.iter().find(|&&inst| func.dfg.insts[inst].opcode() == Opcode::Return).unwrap();
    let value = func.dfg.inst_args(*returned)[0];
    let load = func.dfg.value_def(value).unwrap_inst();
    let ir::InstructionData::Load { arg, .. } = func.dfg.insts[load] else { panic!("the body's load") };
    let guest = confined_operand(&func, arg);
    assert_ne!(guest, incoming, "the body reads the frame's copy");
    let frame = func.dfg.value_def(guest).unwrap_inst();
    assert_eq!(func.dfg.insts[frame].opcode(), Opcode::IaddImm, "the copy's place in the frame");
}

#[test]
fn a_struct_argument_is_copied_from_its_confined_address() {
    let mut func = function(1, 0, 1, |cur, p| {
        let mut callee = ir::Signature::new(cranelift_codegen::isa::CallConv::SystemV);
        callee.params.push(ir::AbiParam::special(types::I64, ir::ArgumentPurpose::StructArgument(24)));
        let sig = cur.func.import_signature(callee);
        let callee = cur.func.import_function(ir::ExtFuncData {
            name: ir::ExternalName::User(ir::UserExternalNameRef::new(0)),
            signature: sig,
            colocated: false,
            patchable: false,
        });
        cur.ins().call(callee, &[p[0]]);
        cur.ins().return_(&[]);
    });
    let pointer = func.dfg.block_params(func.layout.entry_block().unwrap())[0];
    confine(&mut func, "f", &mut Signatures::default(), code).unwrap();
    verified(&func);
    let call = func
        .layout
        .blocks()
        .flat_map(|b| func.layout.block_insts(b))
        .find(|&inst| func.dfg.insts[inst].opcode() == Opcode::Call)
        .unwrap();
    let args = func.dfg.inst_args(call);
    assert_eq!(args.len(), 2, "the struct and `vmctx`");
    assert_eq!(args[1], vmctx(&func));
    assert_eq!(confined_operand(&func, args[0]), pointer);
}

#[test]
fn a_struct_the_tail_guard_cant_hold_or_a_struct_return_is_refused() {
    let mut big = takes_struct(TAIL_GUARD as u32 + 8);
    let Stop(message) = confine(&mut big, "f", &mut Signatures::default(), code).unwrap_err();
    assert!(message.contains("past 64 KiB"), "{message}");
    let mut odd = takes_struct(12);
    assert!(confine(&mut odd, "f", &mut Signatures::default(), code).is_err());
    let mut returns = function(0, 0, 0, |cur, _| {
        cur.ins().return_(&[]);
    });
    returns.signature.returns.push(ir::AbiParam::special(types::I64, ir::ArgumentPurpose::StructArgument(16)));
    let Stop(message) = confine(&mut returns, "f", &mut Signatures::default(), code).unwrap_err();
    assert!(message.contains("a struct returned"), "{message}");
}

#[test]
fn a_call_of_data_is_refused() {
    let mut func = function(0, 0, 1, |cur, _| {
        let sig = cur.func.import_signature(ir::Signature::new(cranelift_codegen::isa::CallConv::SystemV));
        let callee = cur.func.import_function(ir::ExtFuncData {
            name: ir::ExternalName::User(ir::UserExternalNameRef::new(0)),
            signature: sig,
            colocated: false,
            patchable: false,
        });
        cur.ins().call(callee, &[]);
        cur.ins().return_(&[]);
    });
    let data = |_: u32, _: Use| Ok(Name::Data);
    let Stop(message) = confine(&mut func, "f", &mut Signatures::default(), data).unwrap_err();
    assert!(message.contains("a call of data"), "{message}");
}
