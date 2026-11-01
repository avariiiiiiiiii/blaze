use cranelift_codegen::{
    ir::Opcode,
    isa,
    settings::{self, Configurable},
    verify_function,
};

use super::*;

fn confined(params: &[ir::Type], returns: &[ir::Type]) -> ir::Signature {
    let mut sig = ir::Signature::new(CallConv::SystemV);
    sig.params.extend(params.iter().map(|&ty| ir::AbiParam::new(ty)));
    sig.params.push(ir::AbiParam::special(types::I64, ir::ArgumentPurpose::VMContext));
    sig.returns.extend(returns.iter().map(|&ty| ir::AbiParam::new(ty)));
    sig
}

/// Verified against the host's ISA: a stub's `get_frame_pointer` needs one, with frame
/// pointers kept.
fn verified(func: &ir::Function) {
    let mut flags = settings::builder();
    flags.enable("preserve_frame_pointers").unwrap();
    let isa = isa::lookup_by_name(std::env::consts::ARCH)
        .unwrap()
        .finish(settings::Flags::new(flags))
        .unwrap();
    if let Err(errors) = verify_function(func, &*isa) {
        panic!("{}", cranelift_codegen::print_errors::pretty_verifier_error(func, None, errors));
    }
}

fn opcodes(func: &ir::Function) -> Vec<Opcode> {
    func.layout.blocks().flat_map(|block| func.layout.block_insts(block)).map(|inst| func.dfg.insts[inst].opcode()).collect()
}

fn count(func: &ir::Function, op: Opcode) -> usize {
    opcodes(func).into_iter().filter(|&it| it == op).count()
}

#[test]
fn a_thunk_loads_every_argument_calls_and_stores_every_result() {
    let target = confined(&[types::I32, types::I64, types::I128], &[types::I64, types::I8]);
    let func = thunk(&target, CallConv::SystemV).unwrap();
    verified(&func);
    assert_eq!(func.signature, thunk_signature(CallConv::SystemV));
    assert_eq!(count(&func, Opcode::Load), 3, "vmctx is passed, not loaded");
    assert_eq!(count(&func, Opcode::TryCallIndirect), 1);
    assert_eq!(count(&func, Opcode::Store), 2);
    assert_eq!(count(&func, Opcode::Return), 2, "the normal return and the escape's");
    let table = func.dfg.exception_tables.values().next().unwrap();
    assert_eq!(func.dfg.signatures[table.signature()], target);
    assert_eq!(table.items().count(), 1, "one default handler, which every escape lands at");
}

#[test]
fn a_thunk_of_no_values_still_passes_vmctx() {
    let func = thunk(&confined(&[], &[]), CallConv::SystemV).unwrap();
    verified(&func);
    assert_eq!(count(&func, Opcode::Load), 0);
    assert_eq!(count(&func, Opcode::Store), 0);
}

#[test]
fn a_thunk_refuses_what_it_cant_enter() {
    let mut no_vmctx = ir::Signature::new(CallConv::SystemV);
    no_vmctx.params.push(ir::AbiParam::new(types::I64));
    assert!(thunk(&no_vmctx, CallConv::SystemV).unwrap_err().contains("`vmctx`"));
    let mut cold = confined(&[], &[]);
    cold.call_conv = CallConv::Fast;
    assert!(thunk(&cold, CallConv::SystemV).unwrap_err().contains("can't catch"));
    let wide = confined(&[types::I64X4], &[]);
    assert!(thunk(&wide, CallConv::SystemV).unwrap_err().contains("wider than a slot"));
}

#[test]
fn a_stub_hands_every_value_to_the_dispatcher_with_its_id_and_frame() {
    let sig = confined(&[types::I32, types::F64], &[types::I64, types::I32, types::I8, types::I16]);
    let func = stub(7, &sig, CallConv::SystemV).unwrap();
    verified(&func);
    assert_eq!(func.signature, sig);
    assert_eq!(count(&func, Opcode::Store), 3, "vmctx too");
    assert_eq!(count(&func, Opcode::Load), 4);
    assert_eq!(count(&func, Opcode::GetFramePointer), 1);
    let slot = func.sized_stack_slots.values().next().unwrap();
    assert_eq!(slot.size, 4 * SLOT, "as many slots as the wider side");
    let call = opcodes(&func).iter().position(|&it| it == Opcode::Call).unwrap();
    let inst = func.layout.block_insts(func.layout.entry_block().unwrap()).nth(call).unwrap();
    let ir::InstructionData::Call { func_ref, args, .. } = func.dfg.insts[inst] else { panic!("not a call") };
    let ext = &func.dfg.ext_funcs[func_ref];
    let ir::ExternalName::User(name) = ext.name else { panic!("not a user name") };
    assert_eq!(func.params.user_named_funcs()[name], ir::UserExternalName::new(NAMESPACE, DISPATCH));
    assert_eq!(func.dfg.signatures[ext.signature], dispatch_signature(CallConv::SystemV));
    let id = args.as_slice(&func.dfg.value_lists)[0];
    let ir::InstructionData::UnaryImm { imm, .. } = func.dfg.insts[func.dfg.value_def(id).unwrap_inst()] else {
        panic!("the id is no constant")
    };
    assert_eq!(imm.bits(), 7);
}
