//! The type comparison of rustc's `compare_method_clause_entailment`
//! (`rustc_hir_analysis/src/check/compare_impl_item.rs`): the signature of a trait's method,
//! instantiated with a trait impl's arguments and normalized in the impl method's
//! environment, must be a supertype of the impl method's (E0053). Also the trait method's
//! signature as an impl's method would have it, for rustc's suggestion for `_` in an impl
//! method (`suggest_trait_fn_ty_for_impl_fn_infer`, E0121).

use hir_def::{
    CallableDefId, FunctionId, HasModule as _, ImplId, ItemContainerId, Lookup as _, TraitId,
    signatures::FunctionSignature,
};
use rustc_type_ir::{
    TypeVisitableExt as _,
    error::{ExpectedFound, TypeError},
    inherent::{GenericArgs as _, IntoKind as _},
};
use smallvec::SmallVec;

use hir_ty::{
    GenericPredicates,
    db::HirDatabase,
    next_solver::{
        Clause, Clauses, DbInterner, FnSig, GenericArgKind, GenericArgs, ParamEnv, Region,
        RegionExt as _, RegionKind, SolverDefId, Ty, TypingMode,
        infer::{BoundRegionConversionTime, DbInternerInferExt as _, traits::ObligationCause},
        obligation_ctxt::ObligationCtxt,
    },
};

/// Where the signatures of an impl's method and its trait's differ: rustc's `TypeError` of
/// relating them (`extract_spans_for_error_reporting`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureMismatchAt {
    /// The type of the parameter of this index, `self` included (`ArgumentSorts`).
    Param(u32),
    /// The mutability of the parameter of this index (`ArgumentMutability`): "types differ in
    /// mutability".
    ParamMutability(u32),
    /// The return type (`ArgumentSorts` of the last of `inputs_and_output`).
    Return,
    /// The mutability of the return type.
    ReturnMutability,
    /// Another difference, of `unsafe`, the ABI or `...`: rustc reports it at the impl
    /// method's span.
    Signature,
}

/// The first difference between a trait impl's method's signature and its trait's, as
/// rustc's E0053 reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignatureMismatch<'db> {
    pub trait_method: FunctionId,
    pub at: SignatureMismatchAt,
    /// For [`SignatureMismatchAt::Param`] and [`SignatureMismatchAt::Return`], the trait's type
    /// there (`expected`) and the impl's (`found`).
    pub types: Option<ExpectedFound<Ty<'db>>>,
    /// The trait's signature, instantiated and normalized: rustc's "expected signature".
    pub expected: FnSig<'db>,
    /// The impl's signature, normalized: rustc's "found signature". The lifetimes its
    /// `for<'a>` binds are erased.
    pub found: FnSig<'db>,
}

/// The method of the trait `impl_method` implements, with the trait and the impl, when
/// `impl_method` is a method of a trait impl that the trait has a method of its name for.
fn trait_method_of(
    db: &dyn HirDatabase,
    impl_method: FunctionId,
) -> Option<(ImplId, TraitId, FunctionId)> {
    let ItemContainerId::ImplId(impl_) = impl_method.lookup(db).container else { return None };
    let trait_ref = db.impl_trait(impl_)?;
    let trait_ = trait_ref.skip_binder().def_id.0;
    let name = &FunctionSignature::of(db, impl_method).name;
    let trait_method = trait_.trait_items(db).method_by_name(name)?;
    Some((impl_, trait_, trait_method))
}

/// The signature of the trait method `impl_method` implements, instantiated with the impl's
/// trait arguments and with the trait method's own parameters, its `for<'a>` lifetimes
/// liberated onto `impl_method`: rustc's `suggest_trait_fn_ty_for_impl_fn_infer`, the types
/// it suggests for a `_` in an impl's method. `None` when `impl_method` implements no trait
/// method.
pub fn trait_signature_in_impl<'db>(
    db: &'db dyn HirDatabase,
    impl_method: FunctionId,
) -> Option<(FunctionId, FnSig<'db>)> {
    let (impl_, trait_, trait_method) = trait_method_of(db, impl_method)?;
    let interner = DbInterner::new_no_crate(db);
    let trait_ref = db.impl_trait(impl_)?.instantiate_identity().skip_norm_wip();
    let args = GenericArgs::identity_for_item(interner, SolverDefId::FunctionId(trait_method))
        .rebase_onto(interner, SolverDefId::TraitId(trait_), trait_ref.args);
    let sig = db
        .callable_item_signature(CallableDefId::FunctionId(trait_method))
        .instantiate(interner, args)
        .skip_norm_wip();
    Some((trait_method, interner.liberate_late_bound_regions(impl_method.into(), sig)))
}

/// rustc's E0053: the first place the signature of `impl_method`, a method of a trait impl,
/// differs from its trait method's, both normalized in the environment of the impl and the
/// trait method's own where clauses (rustc's hybrid environment). `None` when they agree,
/// when one has an error, and when the structural checks rustc makes first
/// (`check_method_is_structurally_compatible`: `self`, the number and kinds of generic
/// parameters, the number of parameters) fail, as each is an error of its own.
pub fn compare_impl_method_signature<'db>(
    db: &'db dyn HirDatabase,
    impl_method: FunctionId,
) -> Option<SignatureMismatch<'db>> {
    let (impl_, _, trait_method) = trait_method_of(db, impl_method)?;
    let interner = DbInterner::new_with(db, impl_method.module(db).krate(db));
    let trait_ref = db.impl_trait(impl_)?.instantiate_identity().skip_norm_wip();
    if trait_ref.references_error() {
        return None;
    }

    // The own generic parameters of both, at the same places and of the same kinds, so the
    // impl method's stand for the trait method's.
    let own = |def: FunctionId, parent: SolverDefId<'db>| {
        let all = GenericArgs::identity_for_item(interner, SolverDefId::FunctionId(def));
        let parent = GenericArgs::identity_for_item(interner, parent).len();
        all.iter().skip(parent).collect::<SmallVec<[_; 4]>>()
    };
    let trait_own = own(trait_method, SolverDefId::TraitId(trait_ref.def_id.0));
    let impl_own = own(impl_method, SolverDefId::ImplId(impl_));
    let kind = |arg: GenericArgKind<'db>| match arg {
        GenericArgKind::Lifetime(_) => 0u8,
        GenericArgKind::Type(_) => 1,
        GenericArgKind::Const(_) => 2,
    };
    if trait_own.len() != impl_own.len()
        || trait_own.iter().zip(&impl_own).any(|(a, b)| kind(a.kind()) != kind(b.kind()))
    {
        return None;
    }
    let trait_to_impl_args =
        GenericArgs::identity_for_item(interner, SolverDefId::FunctionId(impl_method))
            .rebase_onto(interner, SolverDefId::ImplId(impl_), trait_ref.args);

    let impl_poly_sig = db
        .callable_item_signature(CallableDefId::FunctionId(impl_method))
        .instantiate_identity()
        .skip_norm_wip();
    let trait_sig = db
        .callable_item_signature(CallableDefId::FunctionId(trait_method))
        .instantiate(interner, trait_to_impl_args)
        .skip_norm_wip();
    let trait_sig = interner.liberate_late_bound_regions(impl_method.into(), trait_sig);
    if impl_poly_sig.skip_binder().inputs().len() != trait_sig.inputs().len()
        || impl_poly_sig.references_error()
        || trait_sig.references_error()
    {
        return None;
    }

    // The impl's where clauses, and the trait method's own instantiated for the impl's.
    let hybrid: SmallVec<[Clause<'db>; 8]> =
        GenericPredicates::query_all(db, impl_.into())
            .iter_identity()
            .map(|it| it.skip_norm_wip())
            .chain(
                GenericPredicates::query_own_explicit(db, trait_method.into())
                    .iter_instantiated(interner, trait_to_impl_args.as_slice())
                    .map(|it| it.skip_norm_wip()),
            )
            .collect();
    let param_env = ParamEnv::new(Clauses::new_from_iter(rustc_type_ir::elaborate::elaborate(
        interner,
        hybrid.into_iter(),
    )));

    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let mut ocx = ObligationCtxt::new(&infcx);
    let cause = ObligationCause::dummy();
    let impl_sig = infcx.instantiate_binder_with_fresh_vars(
        cause.span(),
        BoundRegionConversionTime::HigherRankedType,
        impl_poly_sig,
    );
    // rustc's solver normalizes eagerly: a projection that normalizes to the other's type
    // agrees with it. One that can't be normalized is compared as written.
    let impl_sig = ocx.deeply_normalize(&cause, param_env, impl_sig).unwrap_or(impl_sig);
    let trait_sig = ocx.deeply_normalize(&cause, param_env, trait_sig).unwrap_or(trait_sig);

    let error = ocx.sup(&cause, param_env, trait_sig, impl_sig).err()?;
    let output = trait_sig.inputs().len();
    let (at, types) = match error {
        TypeError::ArgumentSorts(types, i) if i == output => (SignatureMismatchAt::Return, Some(types)),
        TypeError::ArgumentSorts(types, i) => (SignatureMismatchAt::Param(i as u32), Some(types)),
        TypeError::ArgumentMutability(i) if i == output => (SignatureMismatchAt::ReturnMutability, None),
        TypeError::ArgumentMutability(i) => (SignatureMismatchAt::ParamMutability(i as u32), None),
        _ => (SignatureMismatchAt::Signature, None),
    };
    let found = rustc_type_ir::fold_regions(interner, infcx.resolve_vars_if_possible(impl_sig), |region, _| {
        match region.kind() {
            RegionKind::ReVar(_) => Region::new_erased(interner),
            _ => region,
        }
    });
    let types = types.map(|ExpectedFound { expected, found }| ExpectedFound {
        expected,
        found: rustc_type_ir::fold_regions(interner, infcx.resolve_vars_if_possible(found), |region, _| {
            match region.kind() {
                RegionKind::ReVar(_) => Region::new_erased(interner),
                _ => region,
            }
        }),
    });
    Some(SignatureMismatch { trait_method, at, types, expected: trait_sig, found })
}

#[cfg(test)]
mod tests;
