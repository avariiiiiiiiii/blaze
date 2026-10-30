//! What the IR builder's requests are as references: one function, static or outside symbol
//! for each, the same whether the IR builder or the walk of the MIR ([`super::refs`]) asks.
//! Equal references are one place, so both go through here.

use blaze_codegen::backend::FrontRef;
use cranelift_codegen::ir;
use hir_def::{
    FunctionId, ItemContainerId, Lookup, StaticId,
    attrs::AttrFlags,
    signatures::{FunctionSignature, StaticFlags, StaticSignature},
};
use hir_ty::{
    db::HirDatabase,
    next_solver::{DbInterner, GenericArgs},
    traits::StoredParamEnvAndCrate,
};

use super::{HirInstance, HirStatic, Kind};
use crate::{
    Instance, InstanceDef, erase_regions, fragment::FuncTarget, home::Homes, link_name,
    static_link_name,
};

pub(crate) type Ref = FrontRef<HirInstance, HirStatic>;

/// The symbol of the routine the unwinder asks about a frame with landing pads.
pub(crate) const PERSONALITY: &str = crate::unwind::PERSONALITY;

/// The reference a function request of the IR builder is.
pub(crate) fn func_ref(
    db: &dyn HirDatabase,
    homes: &mut Homes,
    target: &FuncTarget,
) -> Result<Ref, String> {
    match target {
        FuncTarget::Symbol(symbol) => Ok(FrontRef::Extern(symbol.clone())),
        FuncTarget::Path(path) => {
            Err(format!("a call of rustc's `{path}`, which a build without rustc has none of"))
        }
        FuncTarget::Instance(instance, env) => instance_ref(db, homes, instance, env),
        FuncTarget::Asm(text) => Ok(FrontRef::Function(HirInstance(Kind::Asm(text.clone())))),
    }
}

/// The reference a call of `def` with `args` in `env` is: the instance, compiled in the crate
/// every asker agrees on, or the symbol of a function that has no body of Rust.
pub(crate) fn call_ref<'db>(
    db: &'db dyn HirDatabase,
    homes: &mut Homes,
    def: InstanceDef,
    args: GenericArgs<'db>,
    env: &StoredParamEnvAndCrate,
) -> Result<Ref, String> {
    let interner = DbInterner::new_with(db, env.krate);
    let instance = Instance { def, args: erase_regions(interner, args).store(), checked: false };
    instance_ref(db, homes, &instance, env)
}

/// As [`call_ref`], of an instance as the IR builder holds it.
pub(crate) fn instance_ref(
    db: &dyn HirDatabase,
    homes: &mut Homes,
    instance: &Instance,
    env: &StoredParamEnvAndCrate,
) -> Result<Ref, String> {
    let instance = Instance { checked: false, ..instance.clone() };
    if let InstanceDef::Fn(func) = instance.def
        && !FunctionSignature::of(db, func).has_body()
    {
        return bodyless(db, func).map(FrontRef::Extern);
    }
    let env = homes.home(db, &instance, env);
    Ok(FrontRef::Function(HirInstance(Kind::Code(instance, env))))
}

/// The symbol a function without a body of Rust is: an item of an `extern` block.
fn bodyless(db: &dyn HirDatabase, func: FunctionId) -> Result<String, String> {
    let name = FunctionSignature::of(db, func).name.as_str().to_owned();
    match func.lookup(db).container {
        ItemContainerId::ExternBlockId(_) => {
            let Some(symbol) = link_name(db, func) else {
                return Err(format!("`{name}` of an `extern` block has no link name"));
            };
            if symbol.starts_with("llvm.") {
                return Err(format!("a call of LLVM's intrinsic `{symbol}` the IR builder left"));
            }
            Ok(symbol)
        }
        ItemContainerId::TraitId(_) => {
            Err(format!("a call of the trait method `{name}`, which no impl was found for"))
        }
        _ if FunctionSignature::is_intrinsic(db, func) => {
            Err(format!("a call of the intrinsic `{name}`, which has no fallback body"))
        }
        _ => Err(format!("a call of `{name}`, which has no body")),
    }
}

/// The reference a static is: an item of an `extern` block by its symbol, any other by
/// which static it is.
pub(crate) fn static_ref(db: &dyn HirDatabase, id: StaticId) -> Result<Ref, String> {
    let signature = StaticSignature::of(db, id);
    if !signature.flags.contains(StaticFlags::EXTERN) {
        return Ok(FrontRef::Static(HirStatic(id)));
    }
    let Some(symbol) = static_link_name(db, id) else {
        return Err(format!("the `extern` static `{}` has no link name", signature.name.as_str()));
    };
    if AttrFlags::linkage(db, id.into()).is_some_and(|it| it.as_str() == "extern_weak") {
        return Err(format!(
            "the `extern_weak` static `{symbol}`: a reference can't be weak yet (FrontRef::Weak)"
        ));
    }
    Ok(FrontRef::Extern(symbol))
}

/// The reference a routine Cranelift calls itself is: the C library's.
pub(crate) fn libcall_ref(libcall: ir::LibCall) -> Ref {
    FrontRef::Extern(libcall_symbol(libcall))
}

pub(crate) fn libcall_symbol(libcall: ir::LibCall) -> String {
    cranelift_module::default_libcall_names()(libcall)
}

/// The reference of the personality routine of a function with landing pads.
pub(crate) fn personality_ref() -> Ref {
    FrontRef::Extern(PERSONALITY.to_owned())
}