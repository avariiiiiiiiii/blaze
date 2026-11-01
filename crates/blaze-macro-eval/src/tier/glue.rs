//! The compiled tier's two kinds of glue (macros-tier.md, "Running"), as Cranelift IR that
//! stencil compiles like the macro's own functions, so no assembly is written by hand but
//! the five instructions of `resume` ([`super::machine`]).
//!
//! - An **entry thunk** is how the host enters confined code of one signature:
//!   `thunk(args, rets, vmctx, code) -> u64`, in the host's C convention. It loads the
//!   arguments from 16-byte slots at `args`, `try_call`s `code` with them and `vmctx`, stores
//!   the results to 16-byte slots at `rets` and returns 0. Its handler returns
//!   [`ESCAPED`](super::machine::ESCAPED): an escape (a panic no frame catches, a fault, a
//!   stop) lands there, its frame found by the frame-pointer walk.
//! - A **stub** is the code of a bound name ([`super::bind`]): a function of the name's
//!   confined signature that stores every argument (`vmctx` last) to a 16-byte slot of its
//!   frame, calls `dispatch(id, slots, fp)` with its binding's id and its own frame pointer,
//!   and returns what the dispatcher wrote back to the first slots. A struct argument's slot
//!   holds the address of its copy; a struct return's, its guest offset.

use cranelift_codegen::{
    cursor::{Cursor as _, FuncCursor},
    ir::{self, InstBuilder as _, types},
    isa::CallConv,
};

use super::machine::ESCAPED;

/// The namespace of the glue's one name: the dispatcher, which the loader resolves to the
/// host's `dispatch`. Not confine's: the glue is never confined.
pub(crate) const NAMESPACE: u32 = 0x676c_7565;
pub(crate) const DISPATCH: u32 = 0;

/// The size of one slot: any scalar or vector Cranelift passes in one register.
pub(crate) const SLOT: u32 = 16;

/// The values of `sig` the glue moves through slots: every parameter (`vmctx` included) and
/// every return. One wider than a slot can't be moved.
fn fits(sig: &ir::Signature) -> Result<(), String> {
    match sig.params.iter().chain(&sig.returns).find(|it| it.value_type.bytes() > SLOT) {
        // alloc: error path
        Some(it) => Err(format!("a value of type {} is wider than a slot", it.value_type)),
        None => Ok(()),
    }
}

/// The signature of a thunk: `(args, rets, vmctx, code) -> u64` in `conv`.
pub(crate) fn thunk_signature(conv: CallConv) -> ir::Signature {
    let mut sig = ir::Signature::new(conv);
    // alloc: per thunk built
    sig.params.extend([ir::AbiParam::new(types::I64); 4]);
    sig.returns.push(ir::AbiParam::new(types::I64));
    sig
}

/// The entry thunk of `target`, a confined signature (its last parameter `vmctx`), in the
/// host's C convention `conv`.
pub(crate) fn thunk(target: &ir::Signature, conv: CallConv) -> Result<ir::Function, String> {
    fits(target)?;
    let Some((vmctx_param, params)) = target.params.split_last() else {
        return Err("a confined signature without `vmctx`".to_owned()); // alloc: error path
    };
    if vmctx_param.purpose != ir::ArgumentPurpose::VMContext {
        return Err("a confined signature whose last parameter is not `vmctx`".to_owned()); // alloc: error path
    }
    if !target.call_conv.supports_exceptions() {
        // alloc: error path
        return Err(format!("an entry of convention {}, which can't catch an escape", target.call_conv));
    }
    let mut func = ir::Function::with_name_signature(ir::UserFuncName::default(), thunk_signature(conv));
    let entry = func.dfg.make_block();
    func.layout.append_block(entry);
    let [args, rets, vmctx, code] = [(); 4].map(|()| func.dfg.append_block_param(entry, types::I64));

    let returned = func.dfg.make_block();
    func.layout.append_block(returned);
    let results: Vec<ir::Value> =
        target.returns.iter().map(|it| func.dfg.append_block_param(returned, it.value_type)).collect(); // alloc: per thunk built
    let escaped = func.dfg.make_block();
    func.layout.append_block(escaped);
    let payload = target.call_conv.exception_payload_types(types::I64);
    for &ty in payload {
        func.dfg.append_block_param(escaped, ty);
    }

    // alloc: per thunk built
    let rets_of: Vec<ir::BlockArg> = (0..target.returns.len() as u32).map(ir::BlockArg::TryCallRet).collect();
    let normal = func.dfg.block_call(returned, &rets_of);
    // alloc: per thunk built
    let exn_of: Vec<ir::BlockArg> = (0..payload.len() as u32).map(ir::BlockArg::TryCallExn).collect();
    let landed = func.dfg.block_call(escaped, &exn_of);
    let sig_ref = func.import_signature(target.clone()); // clone: the thunk's own copy of the signature
    let table = func.dfg.exception_tables.push(ir::ExceptionTableData::new(
        sig_ref,
        normal,
        [ir::ExceptionTableItem::Default(landed)],
    ));

    let flags = ir::MemFlagsData::trusted();
    let mut pos = FuncCursor::new(&mut func).at_bottom(entry);
    let mut values: Vec<ir::Value> = params
        .iter()
        .zip(0..)
        .map(|(param, i)| pos.ins().load(param.value_type, flags, args, (i * SLOT) as i32))
        .collect(); // alloc: per thunk built
    values.push(vmctx);
    pos.ins().try_call_indirect(code, &values, table);

    pos.goto_bottom(returned);
    for (&result, i) in results.iter().zip(0..) {
        pos.ins().store(flags, result, rets, (i * SLOT) as i32);
    }
    let zero = pos.ins().iconst(types::I64, 0);
    pos.ins().return_(&[zero]);

    pos.goto_bottom(escaped);
    pos.func.layout.set_cold(escaped);
    let one = pos.ins().iconst(types::I64, ESCAPED as i64);
    pos.ins().return_(&[one]);
    Ok(func)
}

/// The signature of the dispatcher: `dispatch(id, slots, fp)` in `conv`.
pub(crate) fn dispatch_signature(conv: CallConv) -> ir::Signature {
    let mut sig = ir::Signature::new(conv);
    // alloc: per stub built
    sig.params.extend([ir::AbiParam::new(types::I64); 3]);
    sig
}

/// The stub of the binding `id`, of the confined signature `sig`, calling the dispatcher in
/// the host's C convention `conv`.
pub(crate) fn stub(id: u32, sig: &ir::Signature, conv: CallConv) -> Result<ir::Function, String> {
    fits(sig)?;
    let slots = sig.params.len().max(sig.returns.len()).max(1) as u32;
    let mut func = ir::Function::with_name_signature(ir::UserFuncName::default(), sig.clone()); // clone: the stub's own signature
    let entry = func.dfg.make_block();
    func.layout.append_block(entry);
    let params: Vec<ir::Value> =
        sig.params.iter().map(|it| func.dfg.append_block_param(entry, it.value_type)).collect(); // alloc: per stub built
    let slot = func.create_sized_stack_slot(ir::StackSlotData::new(ir::StackSlotKind::ExplicitSlot, slots * SLOT, 4));
    let name = func.declare_imported_user_function(ir::UserExternalName::new(NAMESPACE, DISPATCH));
    let dispatch_sig = func.import_signature(dispatch_signature(conv));
    let dispatch = func.import_function(ir::ExtFuncData {
        name: ir::ExternalName::User(name),
        signature: dispatch_sig,
        colocated: false,
        patchable: false,
    });

    let mut pos = FuncCursor::new(&mut func).at_bottom(entry);
    for (&param, i) in params.iter().zip(0..) {
        pos.ins().stack_store(types::I64, param, slot, (i * SLOT) as i32);
    }
    let id = pos.ins().iconst(types::I64, i64::from(id));
    let at = pos.ins().stack_addr(types::I64, slot, 0);
    let fp = pos.ins().get_frame_pointer(types::I64);
    pos.ins().call(dispatch, &[id, at, fp]);
    let results: Vec<ir::Value> = sig
        .returns
        .iter()
        .zip(0..)
        .map(|(ret, i)| pos.ins().stack_load(types::I64, ret.value_type, slot, (i * SLOT) as i32))
        .collect(); // alloc: per stub built
    pos.ins().return_(&results);
    Ok(func)
}

#[cfg(test)]
mod tests;
