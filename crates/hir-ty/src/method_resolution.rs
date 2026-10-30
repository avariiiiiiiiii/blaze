//! This module is concerned with finding methods that a given type provides.
//! For details about how this works in rustc, see the method lookup page in the
//! [rustc guide] and the corresponding code mostly in
//! [`rustc_hir_typeck/method/probe.rs`].
//!
//! [rustc guide]: https://rust-lang.github.io/rustc-guide/method-lookup.html
//! [`rustc_hir_typeck/method/probe.rs`]: https://github.com/rust-lang/rust/blob/5503df87342a73d0c29126a7e08dc9c1255c46ad/compiler/rustc_hir_typeck/src/method/probe.rs

mod confirm;
pub mod frozen;
mod probe;

use std::{hash::BuildHasher, ops::Range};

use crate::next_solver::StoredGenericArgs;
use blaze_ids::radix;
use either::Either;
use hir_expand::name::Name;
use intern::{Symbol, sym};
use salsa::{SalsaValue, plumbing::AsId};
use span::Edition;
use tracing::{debug, instrument};

use base_db::{Db, Crate};
use hir_def::{
    AssocItemId, BlockIdLt, BuiltinDeriveImplId, ConstId, DefWithBodyId, FunctionId,
    GenericParamId, HasModule, ImplId, ItemContainerId, MacroId, ModuleDefId, ModuleId, TraitId,
    attrs::AttrFlags,
    builtin_derive::BuiltinDeriveImplMethod,
    expr_store::{Body, path::GenericArgs as HirGenericArgs},
    hir::{ExprId, generics::GenericParams},
    item_scope::ItemScope,
    lang_item::LangItems,
    nameres::{DefMap, block_def_map, crate_def_map},
    resolver::Resolver,
    signatures::{ConstSignature, FunctionSignature},
    unstable_features::UnstableFeatures,
};
use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet};
use rustc_type_ir::{
    TypeFoldable, TypeVisitableExt, VisitorResult,
    fast_reject::{TreatParams, simplify_type},
    inherent::{BoundExistentialPredicates, IntoKind},
    try_visit,
};
use stdx::impl_from;
use syntax::{SyntaxKind, T, TextSize};
use triomphe::Arc;

use crate::{
    Span, all_super_traits,
    db::HirDatabase,
    infer::{InferenceContext, unify::InferenceTable},
    lower::GenericPredicates,
    next_solver::{
        AnyImplId, Binder, ClauseKind, DbInterner, FnSig, GenericArgs, ParamEnv, PredicateKind,
        SimplifiedType, SolverDefId, TraitRef, Ty, TyKind, TypingMode, Unnormalized,
        infer::{
            BoundRegionConversionTime, DbInternerInferExt, InferCtxt, InferOk,
            resolve::ReplaceInferWithError,
            select::ImplSource,
            traits::{Obligation, ObligationCause, PredicateObligations},
        },
        obligation_ctxt::ObligationCtxt,
        util::clauses_as_obligations,
    },
    traits::ParamEnvAndCrate,
};

pub use self::probe::{
    AutorefOrPtrAdjustment, Candidate, CandidateKind, CandidateStep, CandidateWithPrivate, Mode,
    Pick, PickKind,
};

pub struct MethodResolutionContext<'a, 'db> {
    pub infcx: &'a InferCtxt<'db>,
    pub resolver: &'a Resolver<'db>,
    pub param_env: ParamEnv<'db>,
    pub traits_in_scope: &'a FxHashSet<TraitId>,
    pub edition: Edition,
    pub features: &'a UnstableFeatures,
    pub call_span: Span,
    pub receiver_span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, salsa::SalsaValue)]
pub enum CandidateId {
    FunctionId(FunctionId),
    ConstId(ConstId),
}
impl_from!(FunctionId, ConstId for CandidateId);

impl CandidateId {
    fn container(self, db: &Db) -> ItemContainerId {
        match self {
            CandidateId::FunctionId(id) => id.loc(db).container,
            CandidateId::ConstId(id) => id.loc(db).container,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct MethodCallee<'db> {
    /// Impl method ID, for inherent methods, or trait method ID, otherwise.
    pub def_id: FunctionId,
    pub args: GenericArgs<'db>,

    /// Instantiated method signature, i.e., it has been
    /// instantiated, normalized, and has had late-bound
    /// lifetimes replaced with inference variables.
    pub sig: FnSig<'db>,
}

#[derive(Debug)]
pub enum MethodError<'db> {
    /// Did not find an applicable method.
    NoMatch,

    /// Multiple methods might apply.
    Ambiguity(Vec<CandidateSource>),

    /// Found an applicable method, but it is not visible.
    PrivateMatch(Pick<'db>),

    /// Found a `Self: Sized` bound where `Self` is a trait object.
    IllegalSizedBound { candidates: Vec<FunctionId>, needs_mut: bool },

    /// Error has already been emitted, no need to emit another one.
    ErrorReported,
}

// A pared down enum describing just the places from which a method
// candidate can arise. Used for error reporting only.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CandidateSource {
    Impl(AnyImplId),
    Trait(TraitId),
}

impl<'db> InferenceContext<'db> {
    /// Performs method lookup. If lookup is successful, it will return the callee
    /// and store an appropriate adjustment for the self-expr. In some cases it may
    /// report an error (e.g., invoking the `drop` method).
    #[instrument(level = "debug", skip(self))]
    pub(crate) fn lookup_method_including_private(
        &mut self,
        self_ty: Ty<'db>,
        name: Name,
        generic_args: Option<&HirGenericArgs>,
        receiver: ExprId,
        call_expr: ExprId,
    ) -> Result<(MethodCallee<'db>, bool), MethodError<'db>> {
        let (pick, is_visible) = match self.lookup_probe(call_expr, receiver, name, self_ty) {
            Ok(it) => (it, true),
            Err(MethodError::PrivateMatch(it)) => {
                // FIXME: Report error.
                (it, false)
            }
            Err(err) => return Err(err),
        };

        let result = self.confirm_method(&pick, self_ty, call_expr, generic_args);
        debug!("result = {:?}", result);

        if let Some(bound) = result.illegal_sized_bound {
            let method = result.callee.def_id;
            self.push_illegal_sized_bound(&pick, self_ty, call_expr, receiver, method, bound);
        }

        self.write_expr_adj(receiver, result.adjustments);
        self.write_method_resolution(call_expr, result.callee.def_id, result.callee.args);

        Ok((result.callee, is_visible))
    }

    #[instrument(level = "debug", skip(self))]
    pub(crate) fn lookup_probe(
        &self,
        call_expr: ExprId,
        receiver: ExprId,
        method_name: Name,
        self_ty: Ty<'db>,
    ) -> probe::PickResult<'db> {
        self.with_method_resolution(call_expr.into(), receiver.into(), |ctx| {
            let pick = ctx.probe_for_name(probe::Mode::MethodCall, method_name, self_ty)?;
            Ok(pick)
        })
    }

    pub(crate) fn with_method_resolution<R>(
        &self,
        call_span: Span,
        receiver_span: Span,
        f: impl FnOnce(&MethodResolutionContext<'_, 'db>) -> R,
    ) -> R {
        let traits_in_scope = self.get_traits_in_scope();
        let traits_in_scope = match &traits_in_scope {
            Either::Left(it) => std::borrow::Borrow::<FxHashSet<TraitId>>::borrow(it),
            Either::Right(it) => *it,
        };
        let ctx = MethodResolutionContext {
            infcx: &self.table.infer_ctxt,
            resolver: &self.resolver,
            param_env: self.table.param_env,
            traits_in_scope,
            edition: self.edition,
            features: self.features,
            call_span,
            receiver_span,
        };
        f(&ctx)
    }
}

/// Used by `FnCtxt::lookup_method_for_operator` with `-Znext-solver`.
///
/// With `AsRigid` we error on `impl Opaque: NotInItemBounds` while
/// `AsInfer` just treats it as ambiguous and succeeds. This is necessary
/// as we want `FnCtxt::check_expr_call` to treat not-yet-defined opaque
/// types as rigid to support `impl Deref<Target = impl FnOnce()>` and
/// `Box<impl FnOnce()>`.
///
/// We only want to treat opaque types as rigid if we need to eagerly choose
/// between multiple candidates. We otherwise treat them as ordinary inference
/// variable to avoid rejecting otherwise correct code.
#[derive(Debug)]
pub(super) enum TreatNotYetDefinedOpaques {
    AsInfer,
    AsRigid,
}

impl<'db> InferenceTable<'db> {
    /// `lookup_method_in_trait` is used for overloaded operators.
    /// It does a very narrow slice of what the normal probe/confirm path does.
    /// In particular, it doesn't really do any probing: it simply constructs
    /// an obligation for a particular trait with the given self type and checks
    /// whether that trait is implemented.
    #[instrument(level = "debug", skip(self))]
    pub(super) fn lookup_method_for_operator(
        &self,
        cause: ObligationCause,
        trait_def_id: TraitId,
        method_item: FunctionId,
        self_ty: Ty<'db>,
        opt_rhs_ty: Option<Ty<'db>>,
        treat_opaques: TreatNotYetDefinedOpaques,
    ) -> Option<InferOk<'db, MethodCallee<'db>>> {
        // Construct a trait-reference `self_ty : Trait<input_tys>`
        let args = GenericArgs::for_item(
            self.interner(),
            trait_def_id.into(),
            |param_idx, param_id, _, _| match param_id {
                GenericParamId::LifetimeParamId(_) | GenericParamId::ConstParamId(_) => {
                    unreachable!("did not expect operator trait to have lifetime/const")
                }
                GenericParamId::TypeParamId(_) => {
                    if param_idx == 0 {
                        self_ty.into()
                    } else if let Some(rhs_ty) = opt_rhs_ty {
                        assert_eq!(param_idx, 1, "did not expect >1 param on operator trait");
                        rhs_ty.into()
                    } else {
                        // FIXME: We should stop passing `None` for the failure case
                        // when probing for call exprs. I.e. `opt_rhs_ty` should always
                        // be set when it needs to be.
                        self.var_for_def(param_id, cause.span())
                    }
                }
            },
        );

        let obligation = Obligation::new(
            self.interner(),
            cause,
            self.param_env,
            TraitRef::new_from_args(self.interner(), trait_def_id.into(), args),
        );

        // Now we want to know if this can be matched
        let matches_trait = match treat_opaques {
            TreatNotYetDefinedOpaques::AsInfer => self.infer_ctxt.predicate_may_hold(&obligation),
            TreatNotYetDefinedOpaques::AsRigid => {
                self.infer_ctxt.predicate_may_hold_opaque_types_jank(&obligation)
            }
        };

        if !matches_trait {
            debug!("--> Cannot match obligation");
            // Cannot be matched, no such method resolution is possible.
            return None;
        }

        // Trait must have a method named `m_name` and it should not have
        // type parameters or early-bound regions.
        let interner = self.interner();

        let def_id = method_item;

        debug!("lookup_in_trait_adjusted: method_item={:?}", method_item);
        let mut obligations = PredicateObligations::new();

        // Instantiate late-bound regions and instantiate the trait
        // parameters into the method type to get the actual method type.
        //
        // N.B., instantiate late-bound regions before normalizing the
        // function signature so that normalization does not need to deal
        // with bound regions.
        let fn_sig = self
            .db
            .callable_item_signature(method_item.into())
            .instantiate(interner, args)
            .skip_norm_wip();
        let fn_sig = self.infer_ctxt.instantiate_binder_with_fresh_vars(
            cause.span(),
            BoundRegionConversionTime::FnCall,
            fn_sig,
        );

        // Register obligations for the parameters. This will include the
        // `Self` parameter, which in turn has a bound of the main trait,
        // so this also effectively registers `obligation` as well. (We
        // used to register `obligation` explicitly, but that resulted in
        // double error messages being reported.)
        //
        // Note that as the method comes from a trait, it should not have
        // any late-bound regions appearing in its bounds.
        let bounds = GenericPredicates::query_all(self.db, method_item.into());
        let bounds = clauses_as_obligations(
            bounds.iter_instantiated(interner, args.as_slice()).map(Unnormalized::skip_norm_wip),
            cause,
            self.param_env,
        );

        obligations.extend(bounds);

        // Also add an obligation for the method type being well-formed.
        debug!(
            "lookup_method_in_trait: matched method fn_sig={:?} obligation={:?}",
            fn_sig, obligation
        );
        for ty in fn_sig.inputs_and_output {
            obligations.push(Obligation::new(
                interner,
                obligation.cause,
                self.param_env,
                Binder::dummy(PredicateKind::Clause(ClauseKind::WellFormed(ty.into()))),
            ));
        }

        let callee = MethodCallee { def_id, args, sig: fn_sig };
        debug!("callee = {:?}", callee);

        Some(InferOk { obligations, value: callee })
    }
}

pub fn lookup_impl_const<'db>(
    infcx: &InferCtxt<'db>,
    env: ParamEnv<'db>,
    const_id: ConstId,
    subs: GenericArgs<'db>,
) -> (ConstId, GenericArgs<'db>) {
    let interner = infcx.interner;
    let db = interner.db;

    let trait_id = match const_id.loc(db).container {
        ItemContainerId::TraitId(id) => id,
        _ => return (const_id, subs),
    };
    let trait_ref = TraitRef::new_from_args(interner, trait_id.into(), subs);

    let const_signature = ConstSignature::of(db, const_id);
    let name = match const_signature.name.as_ref() {
        Some(name) => name,
        None => return (const_id, subs),
    };

    lookup_impl_assoc_item_for_trait_ref(infcx, trait_ref, env, name)
        .and_then(|assoc| {
            if let (Either::Left(AssocItemId::ConstId(id)), s) = assoc {
                Some((id, s))
            } else {
                None
            }
        })
        .unwrap_or((const_id, subs))
}

/// Checks if the self parameter of `Trait` method is the `dyn Trait` and we should
/// call the method using the vtable.
pub fn is_dyn_method<'db>(
    interner: DbInterner<'db>,
    _env: ParamEnv<'db>,
    func: FunctionId,
    fn_subst: GenericArgs<'db>,
) -> Option<usize> {
    let db = interner.db;

    let ItemContainerId::TraitId(trait_id) = func.loc(db).container else {
        return None;
    };
    let trait_params = GenericParams::of(db, trait_id.into()).len();
    let fn_params = fn_subst.len() - trait_params;
    let trait_ref = TraitRef::new_from_args(
        interner,
        trait_id.into(),
        GenericArgs::new_from_slice(&fn_subst[..trait_params]),
    );
    let self_ty = trait_ref.self_ty();
    if let TyKind::Dynamic(d, _) = self_ty.kind() {
        // rustc doesn't accept `impl Foo<2> for dyn Foo<5>`, so if the trait id is equal, no matter
        // what the generics are, we are sure that the method is come from the vtable.
        let is_my_trait_in_bounds = d
            .principal_def_id()
            .is_some_and(|trait_| all_super_traits(db, trait_.0).contains(&trait_id));
        if is_my_trait_in_bounds {
            return Some(fn_params);
        }
    }
    None
}

/// What [`lookup_impl_method_query`] returns, in a form a query can keep.
pub(crate) type StoredImplMethod =
    (Either<FunctionId, (BuiltinDeriveImplId, BuiltinDeriveImplMethod)>, StoredGenericArgs);

/// [`lookup_impl_method_query`], memoized: codegen asks the same trait method with the same
/// arguments many times per build and again on every edit, and each ask re-runs trait
/// selection in a fresh inference context.
#[salsa::tracked(cycle_result = lookup_impl_method_cycle_result, returns(clone))]
pub(crate) fn lookup_impl_method_stored(
    db: &Db,
    env: crate::traits::StoredParamEnvAndCrate,
    func: FunctionId,
    fn_subst: StoredGenericArgs,
) -> StoredImplMethod {
    let (found, args) = lookup_impl_method_query(db, env.as_ref(db), func, fn_subst.as_ref());
    (found, args.store())
}

fn lookup_impl_method_cycle_result(
    _: &Db,
    _: salsa::Id,
    _env: crate::traits::StoredParamEnvAndCrate,
    func: FunctionId,
    fn_subst: StoredGenericArgs,
) -> StoredImplMethod {
    (Either::Left(func), fn_subst)
}

/// Looks up the impl method that actually runs for the trait method `func`.
///
/// Returns `func` if it's not a method defined in a trait or the lookup failed.
pub(crate) fn lookup_impl_method_query<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    func: FunctionId,
    fn_subst: GenericArgs<'db>,
) -> (Either<FunctionId, (BuiltinDeriveImplId, BuiltinDeriveImplMethod)>, GenericArgs<'db>) {
    let interner = DbInterner::new_with(db, env.krate);
    let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);

    let ItemContainerId::TraitId(trait_id) = func.loc(db).container else {
        return (Either::Left(func), fn_subst);
    };
    let trait_params = GenericParams::of(db, trait_id.into()).len();
    let trait_ref = TraitRef::new_from_args(
        interner,
        trait_id.into(),
        GenericArgs::new_from_slice(&fn_subst[..trait_params]),
    );

    let name = &FunctionSignature::of(db, func).name;
    let Some((impl_fn, impl_subst)) =
        lookup_impl_assoc_item_for_trait_ref(&infcx, trait_ref, env.param_env, name).and_then(
            |(assoc, impl_args)| {
                let assoc = match assoc {
                    Either::Left(AssocItemId::FunctionId(id)) => Either::Left(id),
                    Either::Right(it) => Either::Right(it),
                    _ => return None,
                };
                Some((assoc, impl_args))
            },
        )
    else {
        return (Either::Left(func), fn_subst);
    };

    (
        impl_fn,
        GenericArgs::new_from_iter(impl_subst.iter().chain(fn_subst.iter().skip(trait_params))),
    )
}

fn lookup_impl_assoc_item_for_trait_ref<'db>(
    infcx: &InferCtxt<'db>,
    trait_ref: TraitRef<'db>,
    env: ParamEnv<'db>,
    name: &Name,
) -> Option<(Either<AssocItemId, (BuiltinDeriveImplId, BuiltinDeriveImplMethod)>, GenericArgs<'db>)>
{
    let (impl_id, impl_subst) = find_matching_impl(infcx, env, trait_ref)?;
    let impl_id = match impl_id {
        AnyImplId::ImplId(it) => it,
        AnyImplId::BuiltinDeriveImplId(impl_) => {
            return impl_
                .loc(infcx.interner.db)
                .trait_
                .get_method(name.symbol())
                .map(|method| (Either::Right((impl_, method)), impl_subst));
        }
    };
    let db = infcx.interner.db;
    let item_of = |impl_id: ImplId| {
        impl_value_item(db, impl_id, name.clone()).map(|it| match it {
            CandidateId::FunctionId(f) => AssocItemId::FunctionId(f),
            CandidateId::ConstId(c) => AssocItemId::ConstId(c),
        })
    };
    if let Some(item) = item_of(impl_id) {
        return Some((Either::Left(item), impl_subst));
    }
    // A specializing impl takes the items it leaves out from the impl it specializes: the
    // closest one that has the item.
    let parent =
        crate::specialization::specialized_ancestor(db, impl_id, |it| item_of(it).is_some())?;
    let parent_args = infcx.fresh_args_for_item(Span::Dummy, parent.into());
    let parent_trait_ref =
        db.impl_trait(parent)?.instantiate(infcx.interner, parent_args).skip_norm_wip();
    let mut ocx = ObligationCtxt::new(infcx);
    ocx.eq(&ObligationCause::dummy(), env, trait_ref, parent_trait_ref).ok()?;
    let parent_args = infcx.resolve_vars_if_possible(parent_args);
    if parent_args.has_non_region_infer() {
        return None;
    }
    let parent_args = parent_args.fold_with(&mut ReplaceInferWithError::new(infcx.interner));
    Some((Either::Left(item_of(parent)?), parent_args))
}

/// The function or constant of `impl_` named `name`, the first if there are more: all that a
/// lookup of `name` reads of the impl, so an item of another name added to it leaves the
/// lookup as it was.
#[salsa::tracked]
pub fn impl_value_item(db: &Db, impl_: ImplId, name: Name) -> Option<CandidateId> {
    value_item_named(&impl_.impl_items(db).items, &name)
}

/// As [`impl_value_item`], of a trait.
#[salsa::tracked]
pub fn trait_value_item(
    db: &Db,
    trait_: TraitId,
    name: Name,
) -> Option<CandidateId> {
    value_item_named(&trait_.trait_items(db).items, &name)
}

/// For every trait declared in a module of `krate` (not in a block), what [`trait_value_item`]
/// answers for each name its items have, sorted by name and then trait. Built once per crate,
/// so a probe asks one query per crate of its traits in scope instead of one per trait.
#[salsa::tracked(returns(deref))]
fn trait_value_items(db: &Db, krate: Crate) -> Box<[(Name, TraitId, CandidateId)]> {
    let mut items: Vec<(Name, TraitId, CandidateId)> = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        for def in module.scope(db).declarations() {
            let ModuleDefId::TraitId(trait_) = def else { continue };
            items.extend(trait_.trait_items(db).items.iter().filter_map(|&(name, item)| {
                match item {
                    AssocItemId::FunctionId(it) => Some((name, trait_, it.into())),
                    AssocItemId::ConstId(it) => Some((name, trait_, it.into())),
                    AssocItemId::TypeAliasId(_) => None,
                }
            }));
        }
    }
    // Stable, so of a trait's items of one name the first stays first, as `value_item_named`
    // takes it.
    items.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    items.dedup_by(|later, first| (later.0, later.1) == (first.0, first.1));
    items.into()
}

/// The run of [`trait_value_items`] named `name`, sorted by trait: an item of another name
/// added to a trait of `krate` leaves it as it was.
#[salsa::tracked(returns(deref))]
fn in_crate_trait_value_items(
    db: &Db,
    krate: Crate,
    name: Name,
) -> Box<[(TraitId, CandidateId)]> {
    let items = trait_value_items(db, krate);
    let start = items.partition_point(|&(it, _, _)| it < name);
    items[start..].iter().take_while(|&&(it, _, _)| it == name).map(|&(_, t, i)| (t, i)).collect()
}

/// [`trait_value_item`] for each trait a probe asks of, through one
/// [`in_crate_trait_value_items`] per crate and a binary search per trait. A trait in a block is
/// asked of alone.
#[derive(Debug, Default)]
pub struct TraitValueItems<'db> {
    runs: smallvec::SmallVec<[(Crate, &'db [(TraitId, CandidateId)]); 4]>,
}

impl<'db> TraitValueItems<'db> {
    pub fn get(
        &mut self,
        db: &'db Db,
        trait_: TraitId,
        name: Name,
    ) -> Option<CandidateId> {
        let module = trait_.module(db);
        if module.block(db).is_some() {
            return trait_value_item(db, trait_, name);
        }
        let krate = module.krate(db);
        let run = match self.runs.iter().find(|&&(it, _)| it == krate) {
            Some(&(_, run)) => run,
            None => {
                let run = in_crate_trait_value_items(db, krate, name);
                self.runs.push((krate, run));
                run
            }
        };
        run.binary_search_by_key(&trait_, |&(it, _)| it).ok().map(|i| run[i].1)
    }
}

fn value_item_named(items: &[(Name, AssocItemId)], name: &Name) -> Option<CandidateId> {
    items.iter().find_map(|(item_name, item)| match *item {
        AssocItemId::FunctionId(it) if item_name == name => Some(it.into()),
        AssocItemId::ConstId(it) if item_name == name => Some(it.into()),
        _ => None,
    })
}

pub(crate) fn find_matching_impl<'db>(
    infcx: &InferCtxt<'db>,
    env: ParamEnv<'db>,
    trait_ref: TraitRef<'db>,
) -> Option<(AnyImplId, GenericArgs<'db>)> {
    let trait_ref = infcx.at(&ObligationCause::dummy(), env).deeply_normalize(trait_ref).ok()?;

    let obligation = Obligation::new(infcx.interner, ObligationCause::dummy(), env, trait_ref);

    let selection = infcx.select(&obligation).ok()??;

    // Currently, we use a fulfillment context to completely resolve
    // all nested obligations. This is because they can inform the
    // inference of the impl's type parameters.
    let mut ocx = ObligationCtxt::new(infcx);
    let impl_source = selection.map(|obligation| ocx.register_obligation(obligation));

    let errors = ocx.evaluate_obligations_error_on_ambiguity();
    if !errors.is_empty() {
        return None;
    }

    let impl_source = infcx.resolve_vars_if_possible(impl_source);
    if impl_source.has_non_region_infer() {
        return None;
    }

    // Selection may leave region inference variables unresolved; replace them before they escape
    // this inference context.
    //
    // FIXME: decide whether inferred regions should be replaced with error or erased.
    match impl_source {
        ImplSource::UserDefined(impl_source) => Some((
            impl_source.impl_def_id,
            impl_source.args.fold_with(&mut ReplaceInferWithError::new(infcx.interner)),
        )),
        ImplSource::Param(_) | ImplSource::Builtin(..) => None,
    }
}

/// The crates rustc loads to compile `krate`, and so the ones whose impls it knows of: `krate`,
/// the dependencies it names and what those load in turn. A dependency that is never named
/// is not loaded, and with it go the impls that leave a type open: `assert_eq!(1.5, s.parse()
/// .unwrap())` parses an `f64` until serde_json's `impl PartialEq<Value> for f64` is seen, and
/// a test of a crate whose benches use criterion depends on serde_json.
///
/// Named is by the identifiers of `krate`'s files and of what the macros of other crates
/// expand to among its items, which is more than rustc resolves (a local of the same name
/// does) and less where such a macro in a body alone names the dependency.
#[salsa::tracked(returns(deref))]
pub fn loaded_crates(db: &Db, krate: Crate) -> Box<[Crate]> {
    let _p = tracing::info_span!("loaded_crates").entered();
    let data = krate.data(db);
    let mut unnamed: Vec<_> = data
        .dependencies
        .iter()
        .filter(|dep| {
            let data = dep.crate_id.data(db);
            !(dep.is_sysroot() || data.origin.is_lang() || data.target_kind.is_proc_macro())
        })
        .collect();
    // Few files name a dependency, and a file's text tells which may without its tree. A file
    // `include!` puts among the items is one of the crate's as well: a root that is only
    // `include!("../../app/main.rs");` names its dependencies there.
    let included = hir_def::include_macro_invoc(db, krate);
    let def_map = crate_def_map(db, krate);
    let mut files = def_map
        .modules(db)
        .filter_map(|(_, module)| module.origin.file_id())
        .chain(included.iter().map(|&(_, file)| file));
    let named_in = |unnamed: &mut Vec<&base_db::BuiltDependency>, root: &syntax::SyntaxNode| {
        let root = root.to_flat();
        let (tree, at) = root.flat().expect("a flat tree");
        // Few dependencies are left unnamed: each identifier is compared with them, and the
        // walk ends when none is left.
        let mut identifiers = (at..tree.end(at))
            .filter(|&it| tree.is_token(it) && tree.kind(it).0 == SyntaxKind::IDENT as u16);
        while !unnamed.is_empty()
            && let Some(it) = identifiers.next()
        {
            let text = tree.token_text(it);
            unnamed.retain(|dep| dep.name.symbol().as_str() != text);
        }
    };
    while !unnamed.is_empty()
        && let Some(file) = files.next()
    {
        let text = db.file_text(file.file_id(db)).text(db);
        if !unnamed.iter().any(|dep| text.contains(dep.name.symbol().as_str())) {
            continue;
        }
        named_in(&mut unnamed, &file.parse(db).syntax_node());
    }
    // rustc loads the crate a path in a macro's output names too, as in the `impl
    // dep::Trait for ..` a derive of a proc macro writes. A `macro_rules!` of `krate` writes
    // nothing its files do not, nor does a built-in macro but `include!`, whose files are
    // above.
    let mut calls = def_map
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).all_macro_calls().collect::<Vec<_>>())
        .filter(|call| {
            let def = call.loc(db).def;
            def.krate != krate
                && matches!(
                    def.kind,
                    hir_expand::MacroDefKind::ProcMacro(..)
                        | hir_expand::MacroDefKind::Declarative(..)
                )
        });
    while !unnamed.is_empty()
        && let Some(call) = calls.next()
    {
        named_in(&mut unnamed, &call.parse_macro_expansion(db).value.0.syntax_node());
    }
    let mut loaded = FxHashSet::default();
    let mut ordered = vec![krate];
    loaded.insert(krate);
    for dep in &data.dependencies {
        if unnamed.iter().any(|it| std::ptr::eq(*it, dep)) {
            continue;
        }
        // The same of the dependency: the crates of its tests and benches are among its own.
        for &dep in loaded_crates(db, dep.crate_id) {
            if loaded.insert(dep) {
                ordered.push(dep);
            }
        }
    }
    ordered.into()
}

/// The items of `scope` whose bodies may hold impls: an impl is one of its crate wherever it
/// is written. Custom derives write theirs in a const, `const _: () = { ... };` or one with a
/// name, and those are looked into in every crate. A function can hold one as well, of a type
/// that is not its own (zerocopy's tests `impl KnownLayout for elain::Align<8>` in a test).
/// Lowering every body for this is too much, and puts every body's lowering before the first
/// trait query, so a function, static or associated constant is looked into unless lexing its
/// text proves it holds none ([`may_hold`]): bincode's `deserialize_enum` holds its
/// `EnumAccess` for `&mut Deserializer`, and a `#[derive]` on a struct in a body or a
/// procedural macro called there writes one with no `impl` in the text. A module's constants
/// are all looked into: lexing finds no initializer after `const _`.
fn impl_holders(db: &Db, krate: Crate, scope: &ItemScope) -> Vec<DefWithBodyId> {
    let mut holders: Vec<DefWithBodyId> = scope.consts().map(DefWithBodyId::from).collect();
    let declared = scope.declarations().filter_map(|def| match def {
        ModuleDefId::FunctionId(it) if may_hold(db, krate, it.loc(db).id, true) => {
            Some(DefWithBodyId::from(it))
        }
        ModuleDefId::StaticId(it) if may_hold(db, krate, it.loc(db).id, true) => {
            Some(DefWithBodyId::from(it))
        }
        _ => None,
    });
    holders.extend(declared);
    let item = |item: &AssocItemId| match *item {
        AssocItemId::FunctionId(it) if may_hold(db, krate, it.loc(db).id, true) => {
            Some(DefWithBodyId::from(it))
        }
        AssocItemId::ConstId(it) if may_hold(db, krate, it.loc(db).id, true) => {
            Some(DefWithBodyId::from(it))
        }
        _ => None,
    };
    let in_impls = scope.impls().flat_map(|it| &it.impl_items(db).items);
    holders.extend(in_impls.filter_map(|(_, it)| item(it)));
    // An impl in a trait's default method or constant is as global as any other
    // (macro_rules_attribute's `impl<T: IntoIterator> CollectVec for T {}` in `CollectVec::vec`).
    let traits = scope.declarations().filter_map(|def| match def {
        ModuleDefId::TraitId(it) => Some(it),
        _ => None,
    });
    let in_traits = traits.flat_map(|it| &it.trait_items(db).items);
    holders.extend(in_traits.filter_map(|(_, it)| item(it)));
    holders
}

/// Built-in attributes that write no item, so one in a body leaves it without an impl. Any
/// other (`derive`, `cfg_attr`, a tool's, a procedural macro's) may write one.
const INERT_ATTRIBUTES: &[&str] = &[
    "allow",
    "warn",
    "deny",
    "forbid",
    "expect",
    "doc",
    "inline",
    "cold",
    "must_use",
    "track_caller",
    "cfg",
];

blaze_phf::phf_table!(static INERT_ATTRIBUTE = INERT_ATTRIBUTES;);

fn attribute_is_inert(name: &str) -> bool {
    INERT_ATTRIBUTE.get(name).is_some()
}

/// What lexing a body found that may write an impl there.
#[derive(Debug, Clone, PartialEq, Eq, SalsaValue)]
enum BodyMark {
    /// An `impl` token.
    Impl,
    /// A macro call, by the name before its `!`.
    Call(Symbol),
    /// An attribute, `#[..]` or `#![..]`, by its first identifier (empty when it has none).
    Attribute(Symbol),
}

/// Whether the body of `id` may hold an impl (or, without `impls`, an item a macro or an
/// attribute writes): its text has an `impl`, a call of a macro not proven to write none
/// ([`macro_is_inert`]) or an attribute that is not inert. An item a macro writes has no text
/// of its own and may. The file is parsed for the item's range only when lexing it found one.
pub(crate) fn may_hold<N: syntax::AstNode>(
    db: &Db,
    krate: Crate,
    id: hir_expand::AstId<N>,
    impls: bool,
) -> bool {
    let Some(file) = id.file_id.file_id() else { return true };
    let marks = impls_in_bodies(db, file);
    if marks.is_empty() {
        return false;
    }
    let range = id.file_id.ast_id_map(db).get(id.value).text_range();
    marks.iter().any(|(at, mark)| {
        range.contains(*at)
            && match mark {
                BodyMark::Impl => impls,
                BodyMark::Call(name) => !macro_is_inert(db, krate, name),
                BodyMark::Attribute(name) => !attribute_is_inert(name.as_str()),
            }
    })
}

/// Whether a macro called `name` in a body of `krate` writes no impl: some `macro_rules!` of
/// `krate` or its dependencies has the name, none of that name may write one, and no
/// procedural macro of a dependency has it ([`macros_writing_impls`]). `macro_rules!` itself
/// defines and writes nothing; `include!` writes what its file has.
fn macro_is_inert(db: &Db, krate: Crate, name: &Symbol) -> bool {
    if *name == sym::macro_rules {
        return true;
    }
    if *name == sym::include {
        return false;
    }
    let known = macros_writing_impls(db, krate);
    matches!(known.binary_search_by(|(it, _)| it.cmp_order(name)), Ok(i) if !known[i].1)
}

/// Where `file` may write an impl in a body, found by its tokens: each `impl`, macro call and
/// attribute inside the braces that follow `fn name`, or between the `=` and the `;` of a
/// `const` or `static`. An `impl Trait` type is found as well, which costs a lowering and
/// nothing else.
#[salsa::tracked(returns(deref))]
fn impls_in_bodies(
    db: &Db,
    file: base_db::EditionedFileId,
) -> Box<[(TextSize, BodyMark)]> {
    let text = db.file_text(file.file_id(db)).text(db);
    let lexed = syntax::LexedStr::new(file.edition(db), text);
    let tokens: Vec<usize> = (0..lexed.len()).filter(|&i| !lexed.kind(i).is_trivia()).collect();
    let kind = |at: usize| tokens.get(at).map_or(SyntaxKind::EOF, |&i| lexed.kind(i));
    let start = |at: usize| TextSize::from(lexed.text_start(tokens[at]) as u32);
    // For each open brace, whether it is a function's body.
    let mut braces = Vec::new();
    let mut bodies = 0usize;
    // The parentheses and brackets open: a `;` in `-> [u8; 4]` does not end the signature.
    let mut nested = 0usize;
    let mut signature = None;
    // Every delimiter open, and the depths at which a `const` or `static` initializer is.
    let mut depth = 0usize;
    let mut constant = None;
    let mut initializers: Vec<usize> = Vec::new();
    let mut found = Vec::new();
    for at in 0..tokens.len() {
        let previous = if at == 0 { SyntaxKind::EOF } else { kind(at - 1) };
        let in_body = bodies > 0 || !initializers.is_empty();
        match kind(at) {
            SyntaxKind::IDENT if previous == T![fn] => signature = Some(nested),
            SyntaxKind::IDENT if matches!(previous, T![const] | T![static] | T![mut]) => {
                constant = Some(depth)
            }
            T![=] if constant == Some(depth) => {
                constant = None;
                initializers.push(depth);
            }
            T!['('] | T!['['] => {
                nested += 1;
                depth += 1;
            }
            T![')'] | T![']'] => {
                nested = nested.saturating_sub(1);
                depth = depth.saturating_sub(1);
                initializers.retain(|&it| it <= depth);
            }
            T![;] => {
                if signature == Some(nested) {
                    signature = None;
                }
                if constant == Some(depth) {
                    constant = None;
                }
                if initializers.last() == Some(&depth) {
                    initializers.pop();
                }
            }
            T!['{'] => {
                // A block inside brackets (`-> [u8; { N }]`) is no body. One after `<`, `,` or
                // `=` may be a const argument (`-> impl Tr<{ N }>`): counted as a body, it
                // leaves the brace after it one as well.
                let body = signature == Some(nested);
                if body && !matches!(previous, T![<] | T![,] | T![=]) {
                    signature = None;
                }
                bodies += usize::from(body);
                braces.push(body);
                depth += 1;
            }
            T!['}'] => {
                bodies -= usize::from(braces.pop().unwrap_or(false));
                depth = depth.saturating_sub(1);
                initializers.retain(|&it| it <= depth);
            }
            _ => {}
        }
        if !in_body {
            continue;
        }
        match kind(at) {
            T![impl] => found.push((start(at), BodyMark::Impl)),
            T![!] if previous == SyntaxKind::IDENT => {
                let name = Symbol::intern(lexed.text(tokens[at - 1]));
                found.push((start(at - 1), BodyMark::Call(name)));
            }
            T![#] => {
                let open = if kind(at + 1) == T![!] { at + 2 } else { at + 1 };
                if kind(open) == T!['['] {
                    let name = match kind(open + 1) {
                        SyntaxKind::IDENT => lexed.text(tokens[open + 1]),
                        _ => "",
                    };
                    found.push((start(at), BodyMark::Attribute(Symbol::intern(name))));
                }
            }
            _ => {}
        }
    }
    found.into()
}

/// Every `macro_rules!` name of `krate` and of its dependencies, and every procedural macro
/// of a dependency, with whether a macro of that name may write an impl, sorted by name. A
/// `macro_rules!` may when its transcribers have an `impl`, an attribute that is not inert or
/// one written from a fragment (`#[$meta]`), or a call of a macro that may: one passed in
/// (`$name!`), `include!`, a procedural one, one of no known definition, or one that may in
/// turn (dtoa's `dtoa!`, called in the body of `<f64 as Sealed>::write`, calls `diyfp!`, which
/// writes `impl DiyFp` there). Known by name, so a name that may costs a lowering and nothing
/// else.
#[salsa::tracked(returns(deref))]
fn macros_writing_impls(db: &Db, krate: Crate) -> Box<[(Symbol, bool)]> {
    fn add(known: &mut FxHashMap<Symbol, bool>, name: &Symbol, writes: bool) {
        *known.entry(name.clone()).or_default() |= writes;
    }
    let mut known = FxHashMap::default();
    for dep in &krate.data(db).dependencies {
        for (name, writes) in macros_writing_impls(db, dep.crate_id) {
            add(&mut known, name, *writes);
        }
        if dep.crate_id.data(db).target_kind.is_proc_macro() {
            for (_, module) in crate_def_map(db, dep.crate_id).modules(db) {
                for (name, item) in module.scope(db).macros() {
                    if matches!(item.def, MacroId::ProcMacroId(_)) {
                        add(&mut known, name.symbol(), true);
                    }
                }
            }
        }
    }
    let mut calling = Vec::new();
    let mut files = FxHashSet::default();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        let Some(file) = module.origin.file_id() else { continue };
        if !files.insert(file) {
            continue;
        }
        for (name, writes, calls) in macro_definitions(db, file) {
            add(&mut known, name, *writes);
            if !*writes {
                calling.push((name, calls));
            }
        }
    }
    let may_write = |known: &FxHashMap<Symbol, bool>, call: &Symbol| {
        if *call == sym::macro_rules {
            false
        } else if *call == sym::include {
            true
        } else {
            known.get(call).copied().unwrap_or(true)
        }
    };
    loop {
        let before = calling.len();
        calling.retain(|&(name, calls)| {
            let writes = calls.iter().any(|it| may_write(&known, it));
            if writes {
                known.insert(name.clone(), true);
            }
            !writes
        });
        if calling.len() == before {
            break;
        }
    }
    let mut known = known.into_iter().collect::<Vec<_>>();
    known.sort_unstable_by(|a, b| a.0.cmp_order(&b.0));
    known.into()
}

/// The `macro_rules!` of `file`, found by its tokens: each one's name, whether its
/// transcribers write an impl (an `impl` after `{`, `}`, `;`, `(`, `*`, an attribute or
/// `unsafe`), an attribute that is not inert or one from a fragment, or call a macro passed in
/// as `$name!`, and the names of the other macros they call.
#[salsa::tracked(returns(deref))]
fn macro_definitions(
    db: &Db,
    file: base_db::EditionedFileId,
) -> Box<[(Symbol, bool, Box<[Symbol]>)]> {
    let text = db.file_text(file.file_id(db)).text(db);
    if !text.contains("macro_rules") {
        return Box::default();
    }
    let lexed = syntax::LexedStr::new(file.edition(db), text);
    let tokens: Vec<usize> = (0..lexed.len()).filter(|&i| !lexed.kind(i).is_trivia()).collect();
    let kind = |at: usize| tokens.get(at).map_or(SyntaxKind::EOF, |&i| lexed.kind(i));
    let mut found = Vec::new();
    let mut at = 0;
    while at < tokens.len() {
        let definition = kind(at) == SyntaxKind::IDENT
            && lexed.text(tokens[at]) == "macro_rules"
            && kind(at + 1) == T![!]
            && kind(at + 2) == SyntaxKind::IDENT
            && matches!(kind(at + 3), T!['{'] | T!['('] | T!['[']);
        if !definition {
            at += 1;
            continue;
        }
        let name = Symbol::intern(lexed.text(tokens[at + 2]));
        // The rules alternate, a matcher then a transcriber, each a group at depth 1.
        let (mut writes, mut calls, mut depth, mut groups) = (false, Vec::new(), 0usize, 0usize);
        at += 3;
        while at < tokens.len() {
            let transcriber = depth >= 2 && groups % 2 == 0;
            match kind(at) {
                T!['{'] | T!['('] | T!['['] => {
                    if depth == 1 {
                        groups += 1;
                    }
                    depth += 1;
                }
                T!['}'] | T![')'] | T![']'] => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                T![impl]
                    if transcriber
                        && matches!(
                            kind(at - 1),
                            T!['{'] | T!['}'] | T![;] | T!['('] | T![*] | T![']'] | T![unsafe]
                        ) =>
                {
                    writes = true
                }
                T![#] if transcriber => {
                    let open = if kind(at + 1) == T![!] { at + 2 } else { at + 1 };
                    let inert = kind(open + 1) == SyntaxKind::IDENT
                        && attribute_is_inert(lexed.text(tokens[open + 1]));
                    if kind(open) == T!['['] && !inert {
                        writes = true;
                    }
                }
                T![!] if transcriber && kind(at - 1) == SyntaxKind::IDENT => {
                    if kind(at - 2) == T![$] {
                        writes = true;
                    } else {
                        calls.push(Symbol::intern(lexed.text(tokens[at - 1])));
                    }
                }
                _ => {}
            }
            at += 1;
        }
        found.push((name, writes, calls.into()));
        at += 1;
    }
    found.into()
}

/// What finding the impls of `krate` asks of each impl and of each body that may hold one.
/// Each is its own query, so [`ImplWork::run`] on many threads leaves the finding little to do.
pub fn impl_work(db: &Db, krate: Crate) -> Vec<ImplWork> {
    let def_map = crate_def_map(db, krate);
    let mut work = Vec::new();
    for (_, module) in def_map.modules(db) {
        let scope = module.scope(db);
        work.extend(scope.impls().map(ImplWork::Impl));
        work.extend(impl_holders(db, krate, scope).into_iter().map(ImplWork::Holder));
    }
    work
}

#[derive(Clone, Copy)]
pub enum ImplWork {
    Impl(ImplId),
    Holder(DefWithBodyId),
}

impl ImplWork {
    pub fn run(self, db: &Db) {
        match self {
            ImplWork::Impl(id) => {
                _ = db.impl_self_ty(id);
                _ = db.impl_trait(id);
            }
            ImplWork::Holder(owner) => _ = Body::of(db, owner),
        }
    }
}

#[salsa::tracked(returns(ref))]
fn crates_containing_incoherent_inherent_impls(db: &Db, krate: Crate) -> Box<[Crate]> {
    let _p = tracing::info_span!("crates_containing_incoherent_inherent_impls").entered();
    // We assume that only sysroot crates contain `#[rustc_has_incoherent_inherent_impls]`
    // impls, since this is an internal feature and only std uses it.
    krate.transitive_deps(db).into_iter().filter(|krate| krate.data(db).origin.is_lang()).collect()
}

pub fn with_incoherent_inherent_impls<'db>(
    db: &'db Db,
    krate: Crate,
    self_ty: &SimplifiedType<'db>,
    mut callback: impl FnMut(&[ImplId]),
) {
    let has_incoherent_impls = match self_ty.def() {
        Some(def_id) => match def_id.try_into() {
            Ok(def_id) => AttrFlags::query(db, def_id)
                .contains(AttrFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS),
            Err(()) => true,
        },
        _ => true,
    };
    if !has_incoherent_impls {
        return;
    }
    let _p = tracing::info_span!("incoherent_inherent_impls").entered();
    let crates = crates_containing_incoherent_inherent_impls(db, krate);
    for &krate in crates {
        callback(InherentImpls::in_crate_for_self_ty(db, krate, SelfTyKey(*self_ty)));
    }
}

pub fn simplified_type_module(db: &Db, ty: &SimplifiedType<'_>) -> Option<ModuleId> {
    match ty.def()? {
        SolverDefId::AdtId(id) => Some(id.module(db)),
        SolverDefId::TypeAliasId(id) => Some(id.module(db)),
        SolverDefId::TraitId(id) => Some(id.module(db)),
        _ => None,
    }
}

/// A `SimplifiedType` as the argument of a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SalsaValue)]
pub struct SelfTyKey<'db>(
    // SAFETY: salsa may keep this value past `'db` only if nothing in it borrows from the
    // database. `SimplifiedType<'db>` is `fast_reject::SimplifiedType<SolverDefId<'db>>`
    // (`next_solver/ty.rs`), whose variants (`vendor/rustc_type_ir/src/fast_reject.rs`) hold
    // only plain data and `SolverDefId<'db>`, which is `SalsaValue`; no `&'db`.
    #[salsa_value(unsafe(prove(SolverDefId<'db>: SalsaValue)))] pub SimplifiedType<'db>,
);

const _: () = assert!(size_of::<ImplId>() <= 8);
const _: () = assert!(size_of::<BuiltinDeriveImplId>() <= 8);
const _: () = assert!(size_of::<TraitId>() <= 8);
const _: () = assert!(size_of::<SimplifiedType<'static>>() <= 24);

/// Runs of one flat array, as compressed sparse rows: run `i` is
/// `values[starts[i]..starts[i + 1]]`.
#[derive(Debug, PartialEq, Eq, SalsaValue)]
struct Runs<V> {
    starts: Box<[u32]>,
    values: Box<[V]>,
}

impl<V: Copy> Runs<V> {
    fn new<'a>(runs: impl IntoIterator<Item = &'a [V]>) -> Self
    where
        V: 'a,
    {
        let mut starts = vec![0];
        let mut values = Vec::new();
        for run in runs {
            values.extend_from_slice(run);
            starts.push(u32::try_from(values.len()).expect("more than 2^32 impls in a crate"));
        }
        Runs { starts: starts.into_boxed_slice(), values: values.into_boxed_slice() }
    }

    fn run(&self, i: usize) -> &[V] {
        &self.values[self.starts[i] as usize..self.starts[i + 1] as usize]
    }
}

fn hash_self_ty(self_ty: &SimplifiedType<'_>) -> u64 {
    FxBuildHasher.hash_one(self_ty)
}

/// Simplified self types as two columns: the hashes, searched, and the types, compared only
/// where a hash matches. They are sorted by hash within each range a table searches.
#[derive(Debug, PartialEq, Eq)]
struct SelfTys<'db> {
    hashes: Box<[u64]>,
    self_tys: Box<[SimplifiedType<'db>]>,
}

impl<'db> SelfTys<'db> {
    fn new(self_tys: Vec<SimplifiedType<'db>>) -> Self {
        let hashes = self_tys.iter().map(hash_self_ty).collect();
        SelfTys { hashes, self_tys: self_tys.into_boxed_slice() }
    }

    fn len(&self) -> usize {
        self.self_tys.len()
    }

    fn is_empty(&self) -> bool {
        self.self_tys.is_empty()
    }

    /// The index of `self_ty` in `range`, which is sorted by hash.
    fn find(&self, range: Range<usize>, self_ty: &SimplifiedType<'db>) -> Option<usize> {
        let hash = hash_self_ty(self_ty);
        let first = range.start + self.hashes[range.clone()].partition_point(|&it| it < hash);
        (first..range.end)
            .take_while(|&i| self.hashes[i] == hash)
            .find(|&i| self.self_tys[i] == *self_ty)
    }
}

/// Values grouped by simplified self type while impls are found, each type where it was first
/// seen.
struct SelfTyGroups<'db, T> {
    index: FxHashMap<SimplifiedType<'db>, usize>,
    groups: Vec<(SimplifiedType<'db>, T)>,
}

impl<T> Default for SelfTyGroups<'_, T> {
    fn default() -> Self {
        SelfTyGroups { index: FxHashMap::default(), groups: Vec::new() }
    }
}

impl<'db, T: Default> SelfTyGroups<'db, T> {
    fn entry(&mut self, self_ty: SimplifiedType<'db>) -> &mut T {
        let groups = &mut self.groups;
        let i = *self.index.entry(self_ty).or_insert_with(|| {
            groups.push((self_ty, T::default()));
            groups.len() - 1
        });
        &mut self.groups[i].1
    }

    /// The groups sorted by the hash of their type; types of equal hash stay in the order they
    /// were first seen.
    fn into_sorted(self) -> Vec<(SimplifiedType<'db>, T)> {
        let mut groups = self.groups;
        radix::sort_by_key(&mut groups, |(self_ty, _)| hash_self_ty(self_ty));
        groups
    }
}

/// The inherent impls of a crate or block, as columns: the self types sorted by hash, each
/// with its run of impls in the order they were found.
#[derive(Debug, PartialEq, Eq, SalsaValue)]
pub struct InherentImpls<'db> {
    // SAFETY: salsa may keep this column past `'db` only if nothing in it borrows from the
    // database. `SelfTys<'db>` holds `u64`s and `SimplifiedType<'db>`s, which are
    // `fast_reject::SimplifiedType<SolverDefId<'db>>` (`next_solver/ty.rs`), whose variants
    // (`vendor/rustc_type_ir/src/fast_reject.rs`) hold only plain data and `SolverDefId<'db>`,
    // which is `SalsaValue`; no `&'db`.
    #[salsa_value(unsafe(prove(SolverDefId<'db>: SalsaValue)))]
    self_tys: SelfTys<'db>,
    impls: Runs<ImplId>,
}

#[salsa::tracked]
impl<'db> InherentImpls<'db> {
    #[salsa::tracked(returns(ref))]
    pub fn for_crate(db: &'db Db, krate: Crate) -> InherentImpls<'db> {
        let _p = tracing::info_span!("inherent_impls_in_crate_query", ?krate).entered();
        if let Some(frozen) = frozen::inherent_impls(db, krate) {
            return frozen;
        }

        let crate_def_map = crate_def_map(db, krate);

        Self::collect_def_map(db, crate_def_map)
    }

    /// The inherent impls of `krate` for one type. An impl added for another type changes
    /// `for_crate()`, and leaves this as it was.
    #[salsa::tracked(returns(deref))]
    pub fn in_crate_for_self_ty(
        db: &'db Db,
        krate: Crate,
        self_ty: SelfTyKey<'db>,
    ) -> Box<[ImplId]> {
        Self::for_crate(db, krate).for_self_ty(&self_ty.0).into()
    }

    #[salsa::tracked(returns(ref))]
    pub fn for_block(
        db: &'db Db,
        block: BlockIdLt<'db>,
    ) -> Option<Box<InherentImpls<'db>>> {
        let _p = tracing::info_span!("inherent_impls_in_block_query").entered();

        let block_def_map = block_def_map(db, block);
        let result = Self::collect_def_map(db, block_def_map);
        if result.self_tys.is_empty() { None } else { Some(Box::new(result)) }
    }

    /// The inherent impls of `block` for one type, as [`Self::in_crate_for_self_ty`].
    #[salsa::tracked(returns(deref))]
    pub fn in_block_for_self_ty(
        db: &'db Db,
        block: BlockIdLt<'db>,
        self_ty: SelfTyKey<'db>,
    ) -> Box<[ImplId]> {
        match Self::for_block(db, block) {
            Some(impls) => impls.for_self_ty(&self_ty.0).into(),
            None => Box::default(),
        }
    }
}

impl<'db> InherentImpls<'db> {
    fn collect_def_map(db: &'db Db, def_map: &'db DefMap) -> Self {
        let mut map = SelfTyGroups::default();
        collect(db, def_map, &mut map);
        return Self::from_groups(map);

        fn collect<'db>(
            db: &'db Db,
            def_map: &DefMap,
            map: &mut SelfTyGroups<'db, Vec<ImplId>>,
        ) {
            for (_module_id, module_data) in def_map.modules(db) {
                for impl_id in module_data.scope(db).inherent_impls() {
                    let interner = DbInterner::new_no_crate(db);
                    let self_ty = db.impl_self_ty(impl_id);
                    let self_ty = self_ty.instantiate_identity().skip_norm_wip();
                    if let Some(self_ty) =
                        simplify_type(interner, self_ty, TreatParams::InstantiateWithInfer)
                    {
                        map.entry(self_ty).push(impl_id);
                    }
                }

                for owner in impl_holders(db, def_map.krate(), module_data.scope(db)) {
                    for (_, block_def_map) in Body::of(db, owner).blocks(db) {
                        collect(db, block_def_map, map);
                    }
                }
            }
        }
    }

    fn from_groups(groups: SelfTyGroups<'db, Vec<ImplId>>) -> Self {
        let groups = groups.into_sorted();
        let impls = Runs::new(groups.iter().map(|(_, impls)| &**impls));
        let self_tys = SelfTys::new(groups.into_iter().map(|(self_ty, _)| self_ty).collect());
        Self { self_tys, impls }
    }

    pub fn for_self_ty(&self, self_ty: &SimplifiedType<'db>) -> &[ImplId] {
        match self.self_tys.find(0..self.self_tys.len(), self_ty) {
            Some(i) => self.impls.run(i),
            None => &[],
        }
    }

    /// Every simplified self type with its inherent impls, in the order of their hashes.
    pub fn iter(&self) -> impl Iterator<Item = (&SimplifiedType<'db>, &[ImplId])> + '_ {
        self.self_tys.self_tys.iter().enumerate().map(|(i, self_ty)| (self_ty, self.impls.run(i)))
    }

    pub fn for_each_crate_and_block(
        db: &'db Db,
        krate: Crate,
        block: Option<BlockIdLt<'db>>,
        for_each: &mut impl FnMut(&InherentImpls<'db>),
    ) {
        let blocks = std::iter::successors(block, |block| block.module(db).block(db));
        blocks.filter_map(|block| Self::for_block(db, block).as_deref()).for_each(&mut *for_each);
        for_each(Self::for_crate(db, krate));
    }
}

#[derive(Default)]
struct OneTraitImplsBuilder<'db> {
    non_blanket_impls: SelfTyGroups<'db, (Vec<ImplId>, Vec<BuiltinDeriveImplId>)>,
    blanket_impls: Vec<ImplId>,
}

/// The trait impls of a crate or block, as columns. The traits are sorted; trait `t` has the
/// run `blanket[t]` of blanket impls and the rows `trait_rows[t]..trait_rows[t + 1]`, one per
/// self type, sorted by hash. Row `r` has the runs `impls[r]` and `derives[r]`. Every run is in
/// the order its impls were found.
#[derive(Debug, PartialEq, SalsaValue)]
pub struct TraitImpls<'db> {
    traits: Box<[TraitId]>,
    trait_rows: Box<[u32]>,
    blanket: Runs<ImplId>,
    // SAFETY: salsa may keep this column past `'db` only if nothing in it borrows from the
    // database. `SelfTys<'db>` holds `u64`s and `SimplifiedType<'db>`s, which are
    // `fast_reject::SimplifiedType<SolverDefId<'db>>` (`next_solver/ty.rs`), whose variants
    // (`vendor/rustc_type_ir/src/fast_reject.rs`) hold only plain data and `SolverDefId<'db>`,
    // which is `SalsaValue`; no `&'db`.
    #[salsa_value(unsafe(prove(SolverDefId<'db>: SalsaValue)))]
    self_tys: SelfTys<'db>,
    impls: Runs<ImplId>,
    derives: Runs<BuiltinDeriveImplId>,
}

impl<'db> TraitImpls<'db> {
    fn from_traits(traits: FxHashMap<TraitId, OneTraitImplsBuilder<'db>>) -> Self {
        let mut traits = traits.into_iter().collect::<Vec<_>>();
        radix::sort_by_key(&mut traits, |&(trait_, _)| {
            let id = trait_.as_id();
            radix::pack_32(id.index(), id.generation())
        });
        let mut trait_rows = vec![0];
        let mut blanket = Vec::with_capacity(traits.len());
        let mut rows = Vec::new();
        let traits = traits
            .into_iter()
            .map(|(trait_, one)| {
                rows.extend(one.non_blanket_impls.into_sorted());
                let end = u32::try_from(rows.len()).expect("more than 2^32 impls in a crate");
                trait_rows.push(end);
                blanket.push(one.blanket_impls);
                trait_
            })
            .collect::<Box<[_]>>();
        TraitImpls {
            traits,
            trait_rows: trait_rows.into_boxed_slice(),
            blanket: Runs::new(blanket.iter().map(|it| &**it)),
            impls: Runs::new(rows.iter().map(|(_, (impls, _))| &**impls)),
            derives: Runs::new(rows.iter().map(|(_, (_, derives))| &**derives)),
            self_tys: SelfTys::new(rows.into_iter().map(|(self_ty, _)| self_ty).collect()),
        }
    }

    fn trait_index(&self, trait_: TraitId) -> Option<usize> {
        self.traits.binary_search(&trait_).ok()
    }

    fn rows(&self, t: usize) -> Range<usize> {
        self.trait_rows[t] as usize..self.trait_rows[t + 1] as usize
    }

    fn row(&self, trait_: TraitId, self_ty: &SimplifiedType<'db>) -> Option<usize> {
        self.trait_index(trait_).and_then(|t| self.self_tys.find(self.rows(t), self_ty))
    }

    /// Each trait with its index, for [`frozen`].
    fn trait_indices(&self) -> impl Iterator<Item = (TraitId, usize)> + '_ {
        self.traits.iter().copied().zip(0..)
    }

    /// The impls of the trait at index `t` per self type, for [`frozen`].
    fn rows_at(
        &self,
        t: usize,
    ) -> impl Iterator<Item = (&SimplifiedType<'db>, (&[ImplId], &[BuiltinDeriveImplId]))> + '_
    {
        self.rows(t).map(|r| {
            (&self.self_tys.self_tys[r], (self.impls.run(r), self.derives.run(r)))
        })
    }
}

#[salsa::tracked]
impl<'db> TraitImpls<'db> {
    #[salsa::tracked(returns(ref))]
    pub fn for_crate(db: &'db Db, krate: Crate) -> Arc<TraitImpls<'db>> {
        let _p = tracing::info_span!("inherent_impls_in_crate_query", ?krate).entered();
        if let Some(frozen) = frozen::trait_impls(db, krate) {
            return Arc::new(frozen);
        }

        let crate_def_map = crate_def_map(db, krate);
        let result = Self::collect_def_map(db, crate_def_map);
        Arc::new(result)
    }

    #[salsa::tracked(returns(as_deref))]
    pub fn for_block(
        db: &'db Db,
        block: BlockIdLt<'db>,
    ) -> Option<Box<TraitImpls<'db>>> {
        let _p = tracing::info_span!("inherent_impls_in_block_query").entered();

        let block_def_map = block_def_map(db, block);
        let result = Self::collect_def_map(db, block_def_map);
        if result.traits.is_empty() { None } else { Some(Box::new(result)) }
    }

    #[salsa::tracked(returns(deref))]
    pub fn for_crate_and_deps(db: &'db Db, krate: Crate) -> Box<[Arc<Self>]> {
        loaded_crates(db, krate).iter().map(|&dep| Self::for_crate(db, dep).clone()).collect()
    }

    /// The impls of one trait for one type in `krate` and its dependencies. An impl added for
    /// another trait or type changes `for_crate()`, and leaves this as it was.
    #[salsa::tracked(returns(deref))]
    pub fn in_crate_and_deps_for_trait_and_self_ty(
        db: &'db Db,
        krate: Crate,
        trait_: TraitId,
        self_ty: SelfTyKey<'db>,
    ) -> Box<[AnyImplId]> {
        let mut found = Vec::new();
        for impls in Self::for_crate_and_deps(db, krate) {
            let (impls, builtin_derive_impls) = impls.for_trait_and_self_ty(trait_, &self_ty.0);
            found.extend(impls.iter().map(|&it| AnyImplId::from(it)));
            found.extend(builtin_derive_impls.iter().map(|&it| AnyImplId::from(it)));
        }
        found.into()
    }

    /// The blanket impls of one trait in `krate` and its dependencies.
    #[salsa::tracked(returns(deref))]
    pub fn blanket_in_crate_and_deps(
        db: &'db Db,
        krate: Crate,
        trait_: TraitId,
    ) -> Box<[ImplId]> {
        Self::for_crate_and_deps(db, krate)
            .iter()
            .flat_map(|impls| impls.blanket_impls(trait_))
            .copied()
            .collect()
    }

    /// The impls of one trait for one type in `krate` alone, without the built-in derives.
    #[salsa::tracked(returns(deref))]
    pub fn in_crate_for_trait_and_self_ty(
        db: &'db Db,
        krate: Crate,
        trait_: TraitId,
        self_ty: SelfTyKey<'db>,
    ) -> Box<[ImplId]> {
        Self::for_crate(db, krate).for_trait_and_self_ty(trait_, &self_ty.0).0.into()
    }

    /// The impls of one trait for one type in `block` alone, without the built-in derives.
    #[salsa::tracked(returns(deref))]
    pub fn in_block_for_trait_and_self_ty(
        db: &'db Db,
        block: BlockIdLt<'db>,
        trait_: TraitId,
        self_ty: SelfTyKey<'db>,
    ) -> Box<[ImplId]> {
        match Self::for_block(db, block) {
            Some(impls) => impls.for_trait_and_self_ty(trait_, &self_ty.0).0.into(),
            None => Box::default(),
        }
    }

    /// The impls of one trait in `krate` alone, without the built-in derives, in the order
    /// [`Self::for_trait`] gives them: the blanket impls first.
    #[salsa::tracked(returns(deref))]
    pub fn in_crate_for_trait(
        db: &'db Db,
        krate: Crate,
        trait_: TraitId,
    ) -> Box<[ImplId]> {
        let mut found = Vec::new();
        Self::for_crate(db, krate).for_trait(trait_, |impls| {
            if let Either::Left(impls) = impls {
                found.extend_from_slice(impls);
            }
        });
        found.into()
    }

    /// The impls of one trait in `krate` and its dependencies, each crate's as
    /// [`Self::in_crate_for_trait`]. An impl of another trait changes `for_crate()` and
    /// `in_crate_for_trait()` reruns equal, so this does not rerun.
    #[salsa::tracked(returns(deref))]
    pub fn in_crate_and_deps_for_trait(
        db: &'db Db,
        krate: Crate,
        trait_: TraitId,
    ) -> Box<[ImplId]> {
        loaded_crates(db, krate)
            .iter()
            .flat_map(|&dep| Self::in_crate_for_trait(db, dep, trait_))
            .copied()
            .collect()
    }
}

impl<'db> TraitImpls<'db> {
    fn collect_def_map(db: &'db Db, def_map: &DefMap) -> Self {
        let lang_items = hir_def::lang_item::lang_items(db, def_map.krate());
        let mut map = FxHashMap::default();
        collect(db, def_map, lang_items, &mut map);
        return Self::from_traits(map);

        fn collect<'db>(
            db: &'db Db,
            def_map: &DefMap,
            lang_items: &LangItems,
            map: &mut FxHashMap<TraitId, OneTraitImplsBuilder<'db>>,
        ) {
            for (_module_id, module_data) in def_map.modules(db) {
                for impl_id in module_data.scope(db).trait_impls() {
                    let trait_ref = match db.impl_trait(impl_id) {
                        Some(tr) => tr.instantiate_identity().skip_norm_wip(),
                        None => continue,
                    };
                    // Reservation impls should be ignored during trait resolution, so we never need
                    // them during type analysis. See rust-lang/rust#64631 for details.
                    //
                    // FIXME: Reservation impls should be considered during coherence checks. If we are
                    // (ever) to implement coherence checks, this filtering should be done by the trait
                    // solver.
                    if AttrFlags::query(db, impl_id.into())
                        .contains(AttrFlags::RUSTC_RESERVATION_IMPL)
                    {
                        continue;
                    }

                    let self_ty = trait_ref.self_ty();
                    if self_ty_has_error_constructor(self_ty) {
                        // If we see `impl Foo for NoSuchType`, just ignore it.
                        continue;
                    }

                    let interner = DbInterner::new_no_crate(db);
                    let entry = map.entry(trait_ref.def_id.0).or_default();
                    match simplify_type(interner, self_ty, TreatParams::InstantiateWithInfer) {
                        Some(self_ty) => entry.non_blanket_impls.entry(self_ty).0.push(impl_id),
                        None => entry.blanket_impls.push(impl_id),
                    }
                }

                for impl_id in module_data.scope(db).builtin_derive_impls() {
                    let loc = impl_id.loc(db);
                    let Some(trait_id) = loc.trait_.get_id(lang_items) else { continue };
                    let entry = map.entry(trait_id).or_default();
                    let entry = entry.non_blanket_impls.entry(SimplifiedType::Adt(loc.adt.into()));
                    entry.1.push(impl_id);
                }

                for owner in impl_holders(db, def_map.krate(), module_data.scope(db)) {
                    for (_, block_def_map) in Body::of(db, owner).blocks(db) {
                        collect(db, block_def_map, lang_items, map);
                    }
                }
            }
        }
    }

    pub fn blanket_impls(&self, for_trait: TraitId) -> &[ImplId] {
        match self.trait_index(for_trait) {
            Some(t) => self.blanket.run(t),
            None => &[],
        }
    }

    /// Queries whether `self_ty` has potentially applicable implementations of `trait_`.
    pub fn has_impls_for_trait_and_self_ty(
        &self,
        trait_: TraitId,
        self_ty: &SimplifiedType<'db>,
    ) -> bool {
        self.trait_index(trait_).is_some_and(|t| {
            self.self_tys.find(self.rows(t), self_ty).is_some() || !self.blanket.run(t).is_empty()
        })
    }

    pub fn for_trait_and_self_ty(
        &'db self,
        trait_: TraitId,
        self_ty: &SimplifiedType<'db>,
    ) -> (&'db [ImplId], &'db [BuiltinDeriveImplId]) {
        match self.row(trait_, self_ty) {
            Some(r) => (self.impls.run(r), self.derives.run(r)),
            None => (&[], &[]),
        }
    }

    pub fn for_trait(
        &self,
        trait_: TraitId,
        mut callback: impl FnMut(Either<&[ImplId], &[BuiltinDeriveImplId]>),
    ) {
        if let Some(t) = self.trait_index(trait_) {
            callback(Either::Left(self.blanket.run(t)));
            for r in self.rows(t) {
                callback(Either::Left(self.impls.run(r)));
                callback(Either::Right(self.derives.run(r)));
            }
        }
    }

    pub fn for_self_ty(
        &self,
        self_ty: &SimplifiedType<'db>,
        mut callback: impl FnMut(Either<&[ImplId], &[BuiltinDeriveImplId]>),
    ) {
        for t in 0..self.traits.len() {
            if let Some(r) = self.self_tys.find(self.rows(t), self_ty) {
                callback(Either::Left(self.impls.run(r)));
                callback(Either::Right(self.derives.run(r)));
            }
        }
    }

    pub fn for_each_crate_and_block<R: VisitorResult>(
        db: &'db Db,
        krate: Crate,
        block: Option<BlockIdLt<'db>>,
        for_each: &mut impl FnMut(&TraitImpls<'db>) -> R,
    ) -> R {
        try_visit!(Self::for_each_block(db, block, &mut *for_each));
        for impl_ in Self::for_crate_and_deps(db, krate) {
            try_visit!(for_each(impl_));
        }
        R::output()
    }

    /// The impls of `block` and of the blocks around it.
    pub fn for_each_block<R: VisitorResult>(
        db: &'db Db,
        block: Option<BlockIdLt<'db>>,
        for_each: &mut impl FnMut(&TraitImpls<'db>) -> R,
    ) -> R {
        let blocks = std::iter::successors(block, |block| block.module(db).block(db));
        for impl_ in blocks.filter_map(|block| Self::for_block(db, block)) {
            try_visit!(for_each(impl_));
        }
        R::output()
    }

    /// Like [`Self::for_each_crate_and_block()`], but takes in account two blocks, one for a trait and one for a self type.
    pub fn for_each_crate_and_block_trait_and_type<R: VisitorResult>(
        db: &'db Db,
        krate: Crate,
        type_block: Option<BlockIdLt<'db>>,
        trait_block: Option<BlockIdLt<'db>>,
        for_each: &mut impl FnMut(&TraitImpls<'db>) -> R,
    ) -> R {
        let in_self_and_deps = TraitImpls::for_crate_and_deps(db, krate);
        for impl_ in in_self_and_deps {
            try_visit!(for_each(impl_));
        }
        Self::for_each_block_trait_and_type(db, type_block, trait_block, for_each)
    }

    /// The impls of two blocks and of the blocks around them, one for a trait and one for a
    /// self type.
    pub fn for_each_block_trait_and_type<R: VisitorResult>(
        db: &'db Db,
        type_block: Option<BlockIdLt<'db>>,
        trait_block: Option<BlockIdLt<'db>>,
        for_each: &mut impl FnMut(&TraitImpls<'db>) -> R,
    ) -> R {
        // We must not provide duplicate impls to the solver. Therefore we work with the following strategy:
        // start from each block, and walk ancestors until you meet the other block. If they never meet,
        // that means there can't be duplicate impls; if they meet, we stop the search of the deeper block.
        // This breaks when they are equal (both will stop immediately), therefore we handle this case
        // specifically.
        let blocks_iter = |block: Option<BlockIdLt<'db>>| {
            std::iter::successors(block, |block| block.module(db).block(db))
        };
        let for_each_block = |current_block: Option<BlockIdLt<'db>>,
                              other_block: Option<BlockIdLt<'db>>| {
            blocks_iter(current_block)
                .take_while(move |&block| {
                    other_block.is_none_or(|other_block| other_block != block)
                })
                .filter_map(move |block| TraitImpls::for_block(db, block))
        };
        if trait_block == type_block {
            for impl_ in
                blocks_iter(trait_block).filter_map(|block| TraitImpls::for_block(db, block))
            {
                try_visit!(for_each(impl_));
            }
        } else {
            for impl_ in for_each_block(trait_block, type_block) {
                try_visit!(for_each(impl_));
            }
            for impl_ in for_each_block(type_block, trait_block) {
                try_visit!(for_each(impl_));
            }
        }
        R::output()
    }
}

fn self_ty_has_error_constructor<'db>(mut self_ty: Ty<'db>) -> bool {
    if !self_ty.references_non_lt_error() {
        return false;
    }

    loop {
        self_ty = match self_ty.kind() {
            TyKind::Error(_) => return true,
            TyKind::Ref(_, inner, _)
            | TyKind::RawPtr(inner, _)
            | TyKind::Array(inner, _)
            | TyKind::Slice(inner)
            | TyKind::Pat(inner, _) => inner,
            TyKind::UnsafeBinder(inner) => inner.skip_binder(),
            _ => return false,
        };
    }
}

#[cfg(test)]
mod inert_attribute_tests {
    use super::{INERT_ATTRIBUTES, attribute_is_inert};

    #[test]
    fn inert_attributes_as_a_linear_search() {
        let mut names: Vec<String> = Vec::new();
        for name in INERT_ATTRIBUTES {
            names.push((*name).to_owned());
            names.push(format!("{name}_"));
            names.push(format!("r#{name}"));
            names.push(name.to_uppercase());
            names.push(name[..name.len() - 1].to_owned());
        }
        names.extend(
            ["", "derive", "cfg_attr", "test", "macro_export", "rustfmt", "clippy", "allowed"]
                .map(str::to_owned),
        );
        for name in &names {
            let by_search = INERT_ATTRIBUTES.contains(&name.as_str());
            assert_eq!(attribute_is_inert(name), by_search, "{name}");
        }
    }
}
