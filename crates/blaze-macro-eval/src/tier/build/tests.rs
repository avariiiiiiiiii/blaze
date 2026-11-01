use cranelift_codegen::ir::{AbiParam, ArgumentPurpose, Signature, UserExternalName, types};

use super::*;

fn vmctx() -> AbiParam {
    AbiParam::special(types::I64, ArgumentPurpose::VMContext)
}

/// `Closure::call` as Rust lowers it: the result's address, `&mut self`, the buffer's address,
/// then `vmctx`.
fn closure_call() -> Signature {
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::special(types::I64, ArgumentPurpose::StructReturn));
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(vmctx());
    sig
}

#[test]
fn the_bridge_is_found_by_its_purposes_not_its_order() {
    assert_eq!(bridge_abi(&closure_call()).unwrap(), BridgeAbi { ret: 0, buffer: 2 });
    let mut last = closure_call();
    let sret = last.params.remove(0);
    last.params.insert(2, sret);
    assert_eq!(bridge_abi(&last).unwrap(), BridgeAbi { ret: 2, buffer: 1 });
}

#[test]
fn a_bridge_passed_otherwise_is_refused() {
    let mut no_ret = closure_call();
    no_ret.params.remove(0);
    assert!(bridge_abi(&no_ret).unwrap_err().0.contains("`Closure::call`"));
    let mut on_stack = closure_call();
    on_stack.params[2] = AbiParam::special(types::I64, ArgumentPurpose::StructArgument(40));
    assert!(bridge_abi(&on_stack).is_err());
}

#[test]
fn a_stubs_widths_leave_out_vmctx() {
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.extend([AbiParam::new(types::I32), AbiParam::new(types::I64), AbiParam::new(types::I8), vmctx()]);
    sig.returns.push(AbiParam::new(types::I16));
    assert_eq!(widths(&sig).unwrap(), (vec![4, 8, 1], 2));
    sig.returns.push(AbiParam::new(types::I64));
    assert!(widths(&sig).is_err(), "a shim gives one value");
}

#[test]
fn stencils_libcalls_are_the_c_librarys_scalar_functions() {
    for call in [ir::LibCall::CeilF32, ir::LibCall::FloorF64, ir::LibCall::NearestF32, ir::LibCall::FmaF64] {
        let symbol = libcall_symbol(call).unwrap();
        struct None_;
        impl bind::Shims for None_ {
            fn external(&self, _: &str, _: Use) -> Option<u16> {
                None
            }
            fn claimed(&self, _: FunctionId) -> Option<u16> {
                None
            }
        }
        assert_eq!(bind::external(symbol, Use::Call, &None_), Ok(Binding::Pure), "{symbol}");
    }
    assert_eq!(libcall_symbol(ir::LibCall::Probestack), None);
}

#[test]
fn the_tiers_spec_is_static_copy_and_patch_that_counts_nothing() {
    if !machine::SUPPORTED {
        return;
    }
    let origin = Origin::BuildTime(Package::new("proc-macro"));
    for (checks, assertions) in [(true, true), (false, false), (false, true)] {
        let (policy, stencils) = policy(checks, assertions).unwrap();
        assert!(!stencils.pic);
        let tier = tier_spec(policy.choose(&origin, None).unwrap().first, "f").unwrap();
        assert_eq!(tier.backend, Backend::Stencil);
        assert!(!tier.counted && !tier.fold);
        assert_eq!(tier.toolchain.relocation, Relocation::Static);
        assert_eq!(tier.debuginfo, DebugInfo::None);
        assert_eq!(
            (tier.overflow_checks, tier.debug_assertions),
            (checks, assertions),
            "what the program observes is the macro crate's profile's"
        );
    }
}

#[test]
fn each_name_is_found_by_its_user_name() {
    let mut func = ir::Function::new();
    let a = func.declare_imported_user_function(UserExternalName::new(0, 7));
    let b = func.declare_imported_user_function(UserExternalName::new(1, 3));
    let at = name_positions(&func);
    assert_eq!(at[&(0, 7)], a.as_u32());
    assert_eq!(at[&(1, 3)], b.as_u32());
    assert_eq!(at.len(), 2);
}

#[test]
fn a_unit_planned_for_llvm_is_refused_by_name_never_compiled_as_stencils() {
    if !machine::SUPPORTED {
        return;
    }
    let (policy, _) = policy(true, true).unwrap();
    let mut spec = policy.choose(&Origin::BuildTime(Package::new("proc-macro")), None).unwrap().first;
    spec.backend = Backend::Llvm { opt: 3 };
    let error = tier_spec(spec, "core::fmt::write").unwrap_err();
    assert!(error.0.contains("`core::fmt::write`") && error.0.contains("opt-level 3"), "{error:?}");
}
